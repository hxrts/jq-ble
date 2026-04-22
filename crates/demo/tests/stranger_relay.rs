//! In Jacquard, the host can keep relay routing behind a service boundary even
//! when intermediate hops are not exposed to endpoint applications.
//!
//! In this e2e test the Mercator router plans over an explicit connected
//! corridor while the service surface still shows only locally verified peers.
//!
//! Tests run against `JacquardBleHostState` and `JacquardBleService` through a
//! fully injected fake transport. The harness models a BLE adjacency graph and
//! pumps frames between neighbors each round. Intermediate relay nodes receive
//! transit frames but must not surface them to the application layer.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use demo::{JacquardBleEvent, JacquardBleHostState, JacquardBleService, JacquardBleServiceError};
use futures_util::{FutureExt, Stream, StreamExt};
use jacquard_core::{
    ByteCount, Configuration, ControllerId, Environment, FactSourceClass, LinkBuilder,
    LinkEndpoint, LinkRuntimeState, NodeId, Observation, OriginAuthenticationClass,
    PartitionRecoveryClass, RatioPermille, RepairCapability, RouteEpoch, RouteError,
    RouteSelectionError, RoutingEvidenceClass, Tick, TransportError, TransportIngressEvent,
    TransportKind,
};
use jacquard_host_support::{
    DispatchReceiver, DispatchSender, TransportIngressClass, TransportIngressNotifier,
    dispatch_mailbox, transport_ingress_mailbox,
};
use jacquard_mem_node_profile::{NodeIdentity, NodePreset, NodePresetOptions};
use jacquard_mercator::MERCATOR_ENGINE_ID;
use jacquard_traits::{TransportSenderEffects, effect_handler};
use jq_client::{
    BleBridgeIo, BleClientError, JacquardBleClient, decode_client_payload_for_testing,
    encode_client_payload_for_testing,
};
use jq_node_profile::MeshTopology;

// Magic prefix that marks a harness application frame. The service layer uses
// this to distinguish addressed app messages from raw router protocol traffic.
const APPLICATION_FRAME_MAGIC: &[u8; 8] = b"JQBLEAPP";

const HARNESS_MAILBOX_CAPACITY: usize = 1024;
const BRIDGE_ROUND_FOR_TESTS_MS: Duration = Duration::ZERO;
const RELAY_RETRY_FOR_TESTS_MS: Duration = Duration::ZERO;
const RELAY_RETRY_ATTEMPTS_FOR_TESTS: u32 = 64;
const DISCOVERY_ROUNDS: usize = 128;
const CONVERGENCE_ROUNDS: usize = 256;
const DELIVERY_ROUNDS: usize = 256;
const QUIET_ROUNDS: usize = 32;

// --- Fake transport plumbing -------------------------------------------------
//
// TestTransportSender satisfies TransportSenderEffects by pushing frames into a
// dispatch mailbox rather than performing real BLE I/O. FakeTransport pairs this
// mailbox with a transport ingress mailbox so the harness can inject inbound
// events and inspect everything the node tried to send.

#[derive(Clone, Debug, PartialEq, Eq)]
struct OutboundFrame {
    endpoint: LinkEndpoint,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DeliveredFrame {
    from_node_id: NodeId,
    to_node_id: NodeId,
    transport_kind: TransportKind,
    payload: Vec<u8>,
}

#[derive(Clone)]
struct TestTransportSender {
    outbound: DispatchSender<OutboundFrame>,
}

#[effect_handler]
impl TransportSenderEffects for TestTransportSender {
    fn send_transport(
        &mut self,
        endpoint: &LinkEndpoint,
        payload: &[u8],
    ) -> Result<(), TransportError> {
        self.outbound
            .send(OutboundFrame {
                endpoint: endpoint.clone(),
                payload: payload.to_vec(),
            })
            .map(|_| ())
            .map_err(|_| TransportError::Unavailable)
    }
}

struct FakeTransport {
    ingress_sender: jacquard_host_support::TransportIngressSender,
    ingress_receiver: jacquard_host_support::TransportIngressReceiver,
    notifier: TransportIngressNotifier,
    outbound: DispatchReceiver<OutboundFrame>,
    flushed: Arc<Mutex<Vec<OutboundFrame>>>,
}

impl FakeTransport {
    fn new(outbound: DispatchReceiver<OutboundFrame>) -> Self {
        // recursion-exception: constructor assembles the fake transport while retaining the conventional `new` entrypoint
        let (ingress_sender, ingress_receiver, notifier) =
            transport_ingress_mailbox(HARNESS_MAILBOX_CAPACITY);
        Self {
            ingress_sender,
            ingress_receiver,
            notifier,
            outbound,
            flushed: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl BleBridgeIo for FakeTransport {
    fn drain_raw_ingress(&mut self) -> Result<Vec<TransportIngressEvent>, TransportError> {
        Ok(self.ingress_receiver.drain().events)
    }

    fn flush_outbound(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<usize, TransportError>> + Send + '_>> {
        Box::pin(async move {
            let drained = self.outbound.drain();
            let len = drained.len();
            self.flushed.lock().expect("flushed frames").extend(drained);
            Ok(len)
        })
    }

    fn pending_outbound(&self) -> usize {
        self.outbound.pending_len()
    }

    fn notifier(&self) -> &TransportIngressNotifier {
        &self.notifier
    }
}

// --- Harness graph model ----------------------------------------------------
//
// SimulatedLink holds the transport kind and runtime state for a directed edge.
// HarnessNode bundles one node's service and the handles needed to inject events
// or inspect flushed frames. RelayHarness owns the full graph, drives pump
// rounds, and records deliveries.

#[derive(Clone, Debug)]
struct SimulatedLink {
    transport_kind: TransportKind,
    runtime_state: LinkRuntimeState,
}

impl SimulatedLink {
    fn delivers(&self) -> bool {
        matches!(
            self.runtime_state,
            LinkRuntimeState::Active | LinkRuntimeState::Degraded
        )
    }
}

struct HarnessNode {
    node_id: NodeId,
    state: JacquardBleHostState,
    service: Arc<JacquardBleService>,
    ingress_sender: jacquard_host_support::TransportIngressSender,
    flushed: Arc<Mutex<Vec<OutboundFrame>>>,
}

impl HarnessNode {
    fn topology(&self) -> MeshTopology {
        // recursion-exception: same-name forwarding keeps topology access on the harness node surface
        self.service.topology()
    }

    fn drain_flushed(&self) -> Vec<OutboundFrame> {
        let mut guard = self.flushed.lock().expect("flushed frames");
        std::mem::take(&mut *guard)
    }
}

struct RelayHarness {
    nodes: BTreeMap<NodeId, HarnessNode>,
    links: BTreeMap<(NodeId, NodeId), SimulatedLink>,
    deliveries: Arc<Mutex<Vec<DeliveredFrame>>>,
}

impl RelayHarness {
    // Builds a five-node line graph with alternating GATT and L2CAP edges:
    // 1 --GATT-- 2 --L2CAP-- 3 --GATT-- 4 --L2CAP-- 5
    async fn five_node_line() -> Self {
        let node_ids = [node_id(1), node_id(2), node_id(3), node_id(4), node_id(5)];
        let mut nodes = BTreeMap::new();
        for node_id in node_ids {
            let node = build_harness_node(
                node_id,
                five_node_line_topology(node_id),
                RELAY_RETRY_FOR_TESTS_MS,
                RELAY_RETRY_ATTEMPTS_FOR_TESTS,
            )
            .await;
            nodes.insert(node_id, node);
        }
        let mut harness = Self {
            nodes,
            links: BTreeMap::new(),
            deliveries: Arc::new(Mutex::new(Vec::new())),
        };

        harness
            .set_link_state(
                node_id(1),
                node_id(2),
                TransportKind::BleGatt,
                LinkRuntimeState::Active,
            )
            .await;
        harness
            .set_link_state(
                node_id(2),
                node_id(3),
                TransportKind::BleL2cap,
                LinkRuntimeState::Active,
            )
            .await;
        harness
            .set_link_state(
                node_id(3),
                node_id(4),
                TransportKind::BleGatt,
                LinkRuntimeState::Active,
            )
            .await;
        harness
            .set_link_state(
                node_id(4),
                node_id(5),
                TransportKind::BleL2cap,
                LinkRuntimeState::Active,
            )
            .await;

        harness
    }

    fn node(&self, node_id: NodeId) -> &HarnessNode {
        self.nodes.get(&node_id).expect("node present in harness")
    }

    fn clear_deliveries(&self) {
        self.deliveries.lock().expect("deliveries").clear();
    }

    fn deliveries(&self) -> Vec<DeliveredFrame> {
        self.deliveries.lock().expect("deliveries").clone()
    }

    // Updates the harness link table and injects a LinkObserved event on both
    // ends so each node's router sees the new link state on its next round.
    async fn set_link_state(
        &mut self,
        left: NodeId,
        right: NodeId,
        transport_kind: TransportKind,
        runtime_state: LinkRuntimeState,
    ) {
        self.links.insert(
            pair_key(left, right),
            SimulatedLink {
                transport_kind: transport_kind.clone(),
                runtime_state,
            },
        );

        self.node(left)
            .ingress_sender
            .emit(
                TransportIngressClass::Control,
                link_observed(
                    right,
                    ble_endpoint(node_byte(right), transport_kind.clone()),
                    runtime_state,
                ),
            )
            .expect("left link update");
        self.node(right)
            .ingress_sender
            .emit(
                TransportIngressClass::Control,
                link_observed(
                    left,
                    ble_endpoint(node_byte(left), transport_kind),
                    runtime_state,
                ),
            )
            .expect("right link update");
    }

    // Drains one round of outbound frames from every node and delivers each
    // frame to its target neighbor if the harness link allows it.
    fn pump_once(&self) {
        for node in self.nodes.values() {
            let flushed = node.drain_flushed();
            for frame in flushed {
                let Some(target_node_id) = endpoint_node_id(&frame.endpoint) else {
                    continue;
                };
                let Some(link) = self.links.get(&pair_key(node.node_id, target_node_id)) else {
                    continue;
                };
                if !link.delivers() {
                    continue;
                }

                let delivered = DeliveredFrame {
                    from_node_id: node.node_id,
                    to_node_id: target_node_id,
                    transport_kind: link.transport_kind.clone(),
                    payload: frame.payload.clone(),
                };
                self.deliveries
                    .lock()
                    .expect("deliveries")
                    .push(delivered.clone());

                self.node(target_node_id)
                    .ingress_sender
                    .emit(
                        TransportIngressClass::Payload,
                        payload_received(
                            node.node_id,
                            ble_endpoint(node_byte(node.node_id), link.transport_kind.clone()),
                            &frame.payload,
                        ),
                    )
                    .expect("deliver payload");
            }
        }
    }

    async fn pump_rounds(&self, rounds: usize) {
        for _ in 0..rounds {
            self.pump_once();
            tokio::task::yield_now().await;
        }
    }

    async fn wait_until_rounds<F>(&self, rounds: usize, mut predicate: F) -> bool
    where
        F: FnMut(&Self) -> bool,
    {
        for _ in 0..rounds {
            if predicate(self) {
                return true;
            }
            self.pump_once();
            tokio::task::yield_now().await;
        }
        predicate(self)
    }

    async fn wait_for_stream_item<S, T>(&self, stream: &mut Pin<Box<S>>, rounds: usize) -> Option<T>
    where
        S: Stream<Item = T>,
    {
        for _ in 0..rounds {
            if let Some(item) = stream.as_mut().next().now_or_never().flatten() {
                return Some(item);
            }
            self.pump_once();
            tokio::task::yield_now().await;
        }
        stream.as_mut().next().now_or_never().flatten()
    }
}

// --- Helper constructors ----------------------------------------------------

fn node_id(byte: u8) -> NodeId {
    NodeId([byte; 32])
}

fn node_byte(node_id: NodeId) -> u8 {
    node_id.0[0]
}

// Canonical unordered key for a bidirectional link so both directions share the
// same entry in the harness link table.
fn pair_key(left: NodeId, right: NodeId) -> (NodeId, NodeId) {
    if left <= right {
        (left, right)
    } else {
        (right, left)
    }
}

// Recovers a NodeId from a BLE scoped-bytes locator by reading the first byte.
fn endpoint_node_id(endpoint: &LinkEndpoint) -> Option<NodeId> {
    match &endpoint.locator {
        jacquard_core::EndpointLocator::ScopedBytes { scope, bytes } if scope == "ble" => {
            bytes.first().copied().map(node_id)
        }
        _ => None,
    }
}

fn ble_endpoint(byte: u8, transport_kind: TransportKind) -> LinkEndpoint {
    LinkEndpoint::new(
        transport_kind,
        jacquard_core::EndpointLocator::ScopedBytes {
            scope: "ble".into(),
            bytes: vec![byte],
        },
        ByteCount(512),
    )
}

// Topology builders ----------------------------------------------------------
//
// local_only_topology seeds a single node with no known neighbors.
// seeded_node_roster seeds a set of nodes with identities only and no links.
// direct_peer_topology seeds exactly two nodes with one GATT edge between them.

fn local_only_topology(local_node_id: NodeId) -> Observation<Configuration> {
    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([(
                local_node_id,
                NodePreset::route_capable_for_engines(
                    NodePresetOptions::new(
                        NodeIdentity::new(local_node_id, ControllerId(local_node_id.0)),
                        ble_endpoint(node_byte(local_node_id), TransportKind::BleGatt),
                        Tick(1),
                    ),
                    &[MERCATOR_ENGINE_ID],
                )
                .build(),
            )]),
            links: BTreeMap::new(),
            environment: Environment {
                reachable_neighbor_count: 0,
                churn_permille: RatioPermille(0),
                contention_permille: RatioPermille(0),
            },
        },
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Controlled,
        observed_at_tick: Tick(1),
    }
}

fn five_node_line_topology(local_node_id: NodeId) -> Observation<Configuration> {
    let roster = [node_id(1), node_id(2), node_id(3), node_id(4), node_id(5)];
    let line_links = [
        (node_id(1), node_id(2), TransportKind::BleGatt),
        (node_id(2), node_id(3), TransportKind::BleL2cap),
        (node_id(3), node_id(4), TransportKind::BleGatt),
        (node_id(4), node_id(5), TransportKind::BleL2cap),
    ];

    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: roster
                .into_iter()
                .map(|id| (id, route_capable_node(id)))
                .collect(),
            links: line_links
                .into_iter()
                .flat_map(|(left, right, transport_kind)| {
                    [
                        ((left, right), active_link(right, transport_kind.clone())),
                        ((right, left), active_link(left, transport_kind)),
                    ]
                })
                .collect(),
            environment: Environment {
                reachable_neighbor_count: direct_neighbor_count(local_node_id),
                churn_permille: RatioPermille(0),
                contention_permille: RatioPermille(0),
            },
        },
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Controlled,
        observed_at_tick: Tick(1),
    }
}

fn direct_neighbor_count(local_node_id: NodeId) -> u32 {
    match node_byte(local_node_id) {
        1 | 5 => 1,
        2..=4 => 2,
        _ => 0,
    }
}

fn route_capable_node(id: NodeId) -> jacquard_core::Node {
    NodePreset::route_capable_for_engines(
        NodePresetOptions::new(
            NodeIdentity::new(id, ControllerId(id.0)),
            ble_endpoint(node_byte(id), TransportKind::BleGatt),
            Tick(1),
        ),
        &[MERCATOR_ENGINE_ID],
    )
    .build()
}

fn active_link(neighbor_node_id: NodeId, transport_kind: TransportKind) -> jacquard_core::Link {
    let mut link = LinkBuilder::new(ble_endpoint(node_byte(neighbor_node_id), transport_kind))
        .with_profile(
            jacquard_core::DurationMs(25),
            RepairCapability::TransportRetransmit,
            PartitionRecoveryClass::LocalReconnect,
        )
        .with_runtime_state(LinkRuntimeState::Active)
        .build();
    link.state.delivery_confidence_permille =
        jacquard_core::Belief::certain(RatioPermille(950), Tick(1));
    link
}

fn direct_peer_topology(
    local_node_id: NodeId,
    remote_node_id: NodeId,
) -> Observation<Configuration> {
    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([
                (
                    local_node_id,
                    NodePreset::route_capable_for_engines(
                        NodePresetOptions::new(
                            NodeIdentity::new(local_node_id, ControllerId(local_node_id.0)),
                            ble_endpoint(node_byte(local_node_id), TransportKind::BleGatt),
                            Tick(1),
                        ),
                        &[MERCATOR_ENGINE_ID],
                    )
                    .build(),
                ),
                (
                    remote_node_id,
                    NodePreset::route_capable_for_engines(
                        NodePresetOptions::new(
                            NodeIdentity::new(remote_node_id, ControllerId(remote_node_id.0)),
                            ble_endpoint(node_byte(remote_node_id), TransportKind::BleGatt),
                            Tick(1),
                        ),
                        &[MERCATOR_ENGINE_ID],
                    )
                    .build(),
                ),
            ]),
            links: BTreeMap::from([
                ((local_node_id, remote_node_id), {
                    let mut link = LinkBuilder::new(ble_endpoint(
                        node_byte(remote_node_id),
                        TransportKind::BleGatt,
                    ))
                    .with_profile(
                        jacquard_core::DurationMs(25),
                        RepairCapability::TransportRetransmit,
                        PartitionRecoveryClass::LocalReconnect,
                    )
                    .with_runtime_state(LinkRuntimeState::Active)
                    .build();
                    link.state.delivery_confidence_permille =
                        jacquard_core::Belief::certain(RatioPermille(950), Tick(1));
                    link
                }),
                ((remote_node_id, local_node_id), {
                    let mut link = LinkBuilder::new(ble_endpoint(
                        node_byte(local_node_id),
                        TransportKind::BleGatt,
                    ))
                    .with_profile(
                        jacquard_core::DurationMs(25),
                        RepairCapability::TransportRetransmit,
                        PartitionRecoveryClass::LocalReconnect,
                    )
                    .with_runtime_state(LinkRuntimeState::Active)
                    .build();
                    link.state.delivery_confidence_permille =
                        jacquard_core::Belief::certain(RatioPermille(950), Tick(1));
                    link
                }),
            ]),
            environment: Environment {
                reachable_neighbor_count: 1,
                churn_permille: RatioPermille(0),
                contention_permille: RatioPermille(0),
            },
        },
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Controlled,
        observed_at_tick: Tick(1),
    }
}

fn link_observed(
    remote_node_id: NodeId,
    endpoint: LinkEndpoint,
    runtime_state: LinkRuntimeState,
) -> TransportIngressEvent {
    TransportIngressEvent::LinkObserved {
        remote_node_id,
        link: LinkBuilder::new(endpoint)
            .with_profile(
                jacquard_core::DurationMs(25),
                RepairCapability::TransportRetransmit,
                PartitionRecoveryClass::LocalReconnect,
            )
            .with_runtime_state(runtime_state)
            .build(),
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Unauthenticated,
    }
}

fn payload_received(
    remote_node_id: NodeId,
    endpoint: LinkEndpoint,
    payload: &[u8],
) -> TransportIngressEvent {
    TransportIngressEvent::PayloadReceived {
        from_node_id: remote_node_id,
        endpoint,
        payload: payload.to_vec(),
    }
}

// Application frame encoding: magic(8) + from_node_id(32) + destination(32) + payload
fn encode_application_frame(
    from_node_id: NodeId,
    destination_node_id: NodeId,
    payload: &[u8],
) -> Vec<u8> {
    let mut framed = Vec::with_capacity(APPLICATION_FRAME_MAGIC.len() + 64 + payload.len());
    framed.extend_from_slice(APPLICATION_FRAME_MAGIC);
    framed.extend_from_slice(&from_node_id.0);
    framed.extend_from_slice(&destination_node_id.0);
    framed.extend_from_slice(payload);
    framed
}

fn encode_client_application_frame(
    from_node_id: NodeId,
    destination_node_id: NodeId,
    payload: &[u8],
) -> Vec<u8> {
    encode_client_payload_for_testing(&encode_application_frame(
        from_node_id,
        destination_node_id,
        payload,
    ))
}

fn decode_application_frame(payload: &[u8]) -> Option<(NodeId, NodeId, Vec<u8>)> {
    let header_len = APPLICATION_FRAME_MAGIC.len() + 64;
    if payload.len() < header_len || !payload.starts_with(APPLICATION_FRAME_MAGIC) {
        return None;
    }

    let mut from_node_id = [0_u8; 32];
    from_node_id.copy_from_slice(
        &payload[APPLICATION_FRAME_MAGIC.len()..APPLICATION_FRAME_MAGIC.len() + 32],
    );

    let destination_offset = APPLICATION_FRAME_MAGIC.len() + 32;
    let mut destination_node_id = [0_u8; 32];
    destination_node_id.copy_from_slice(&payload[destination_offset..destination_offset + 32]);

    Some((
        NodeId(from_node_id),
        NodeId(destination_node_id),
        payload[header_len..].to_vec(),
    ))
}

// Constructs a HarnessNode: wires a FakeTransport into a JacquardBleClient,
// wraps it in a JacquardBleService, and installs it into a fresh host state.
async fn build_harness_node(
    node_id: NodeId,
    initial_topology: Observation<Configuration>,
    relay_retry_interval_ms: Duration,
    relay_retry_attempts_max: u32,
) -> HarnessNode {
    let (outbound_tx, outbound_rx) = dispatch_mailbox(HARNESS_MAILBOX_CAPACITY);
    let transport_sender = TestTransportSender {
        outbound: outbound_tx,
    };
    let transport = FakeTransport::new(outbound_rx);
    let ingress_sender = transport.ingress_sender.clone();
    let flushed = Arc::clone(&transport.flushed);
    let client = JacquardBleClient::new_with_transport_and_round_interval_ms_for_testing(
        node_id,
        initial_topology,
        transport,
        transport_sender,
        BRIDGE_ROUND_FOR_TESTS_MS,
    );
    let state = JacquardBleHostState::default();
    let service = state
        .install_for_testing(
            JacquardBleService::from_client_with_retry_policy_for_testing(
                client,
                relay_retry_interval_ms,
                relay_retry_attempts_max,
            ),
        )
        .await
        .expect("install harness service");
    HarnessNode {
        node_id,
        state,
        service,
        ingress_sender,
        flushed,
    }
}

async fn next_stream_item<S, T>(stream: &mut Pin<Box<S>>, rounds: usize) -> Option<T>
where
    S: Stream<Item = T>,
{
    for _ in 0..rounds {
        if let Some(item) = stream.as_mut().next().now_or_never().flatten() {
            return Some(item);
        }
        tokio::task::yield_now().await;
    }
    stream.as_mut().next().now_or_never().flatten()
}

async fn wait_for_event<S, T, F>(
    stream: &mut Pin<Box<S>>,
    timeout_s: u64,
    mut predicate: F,
) -> Option<T>
where
    S: Stream<Item = T>,
    F: FnMut(&T) -> bool,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_s);
    loop {
        let remaining = deadline.checked_duration_since(tokio::time::Instant::now())?;
        let item = tokio::time::timeout(remaining, stream.as_mut().next())
            .await
            .ok()
            .flatten()?;
        if predicate(&item) {
            return Some(item);
        }
    }
}

async fn deliver_with_retries(
    harness: &RelayHarness,
    sender: NodeId,
    destination: NodeId,
    payload: &[u8],
    incoming: &mut Pin<Box<impl Stream<Item = demo::JacquardIncomingMessage>>>,
    rounds: usize,
) -> demo::JacquardIncomingMessage {
    for _ in 0..rounds {
        match harness.node(sender).state.send(destination, payload).await {
            Ok(()) => {
                if let Some(message) =
                    wait_for_message(harness, incoming, sender, payload, DELIVERY_ROUNDS).await
                {
                    return message;
                }
            }
            Err(JacquardBleServiceError::Client(BleClientError::Route(RouteError::Selection(
                RouteSelectionError::NoCandidate,
            )))) => {}
            Err(error) => panic!("unexpected send failure: {error:?}"),
        }
        harness.pump_rounds(1).await;
    }
    panic!("payload should become deliverable within the allotted convergence rounds");
}

async fn wait_for_message<S>(
    harness: &RelayHarness,
    stream: &mut Pin<Box<S>>,
    from_node_id: NodeId,
    payload: &[u8],
    rounds: usize,
) -> Option<demo::JacquardIncomingMessage>
where
    S: Stream<Item = demo::JacquardIncomingMessage>,
{
    for _ in 0..rounds {
        if let Some(message) = stream.as_mut().next().now_or_never().flatten()
            && message.from_node_id == from_node_id
            && message.payload == payload
        {
            return Some(message);
        }
        harness.pump_once();
        tokio::task::yield_now().await;
    }
    None
}

// --- Tests ------------------------------------------------------------------

// Verifies that the event stream emits Started, an initial Topology snapshot,
// a Topology update when a peer is discovered, and an Incoming message when a
// payload addressed to this node arrives.
#[tokio::test(flavor = "current_thread")]
async fn service_events_surface_start_topology_discovery_and_incoming_payloads() {
    let local_node_id = node_id(1);
    let remote_node_id = node_id(9);
    let (outbound_tx, outbound_rx) = dispatch_mailbox(HARNESS_MAILBOX_CAPACITY);
    let transport_sender = TestTransportSender {
        outbound: outbound_tx,
    };
    let transport = FakeTransport::new(outbound_rx);
    let ingress_sender = transport.ingress_sender.clone();
    let client = JacquardBleClient::new_with_transport_and_round_interval_ms_for_testing(
        local_node_id,
        local_only_topology(local_node_id),
        transport,
        transport_sender,
        BRIDGE_ROUND_FOR_TESTS_MS,
    );
    let service = JacquardBleService::from_client_for_testing(client);
    let mut events = Box::pin(service.events());

    // The first event must be Started carrying the local node identity.
    let started = next_stream_item(&mut events, QUIET_ROUNDS)
        .await
        .expect("started payload");
    assert_eq!(started, JacquardBleEvent::Started { local_node_id });

    // The second event must be the initial topology containing only this node.
    let initial = next_stream_item(&mut events, QUIET_ROUNDS)
        .await
        .expect("initial topology payload");
    match initial {
        JacquardBleEvent::Topology { snapshot } => {
            assert_eq!(snapshot.local_node_id, local_node_id);
            assert_eq!(snapshot.nodes.len(), 1);
        }
        other => panic!("unexpected initial event: {other:?}"),
    }

    // Inject a link observation to simulate the peer coming into radio range.
    ingress_sender
        .emit(
            TransportIngressClass::Control,
            link_observed(
                remote_node_id,
                ble_endpoint(node_byte(remote_node_id), TransportKind::BleGatt),
                LinkRuntimeState::Active,
            ),
        )
        .expect("emit discovery");

    // The topology update must include the new peer and the edge to it.
    let discovered = wait_for_event(&mut events, 1, |event| {
        matches!(
            event,
            JacquardBleEvent::Topology { snapshot }
                if snapshot.nodes.contains_key(&remote_node_id)
                    && snapshot.edge(local_node_id, remote_node_id).is_some()
        )
    })
    .await
    .expect("discovery payload");
    match discovered {
        JacquardBleEvent::Topology { snapshot } => {
            assert!(snapshot.nodes.contains_key(&remote_node_id));
            assert!(snapshot.edge(local_node_id, remote_node_id).is_some());
        }
        other => panic!("unexpected discovery event: {other:?}"),
    }

    // Inject a payload addressed to this node from the peer.
    ingress_sender
        .emit(
            TransportIngressClass::Payload,
            payload_received(
                remote_node_id,
                ble_endpoint(node_byte(remote_node_id), TransportKind::BleGatt),
                &encode_client_application_frame(
                    remote_node_id,
                    local_node_id,
                    b"hello from jacquard",
                ),
            ),
        )
        .expect("emit payload");

    // Drain events until the Incoming message surfaces, skipping any interleaved
    // topology updates produced by the bridge round.
    for _ in 0..DELIVERY_ROUNDS {
        let Some(event) = next_stream_item(&mut events, 1).await else {
            continue;
        };
        match event {
            JacquardBleEvent::Incoming { message } => {
                assert_eq!(message.from_node_id, remote_node_id);
                assert_eq!(message.payload, b"hello from jacquard");
                return;
            }
            JacquardBleEvent::Topology { .. } => continue,
            other => panic!("unexpected incoming event: {other:?}"),
        }
    }
    panic!("incoming payload should surface through the service event stream");
}

// Verifies that JacquardBleHostState routes a send through the host service
// boundary and wraps it correctly, and that a second install attempt on an
// already-started state is rejected.
#[tokio::test(flavor = "current_thread")]
async fn host_state_routes_payloads_through_the_jacquard_host_boundary() {
    let local_node_id = node_id(1);
    let remote_node_id = node_id(2);
    let (outbound_tx, outbound_rx) = dispatch_mailbox(HARNESS_MAILBOX_CAPACITY);
    let transport_sender = TestTransportSender {
        outbound: outbound_tx,
    };
    let transport = FakeTransport::new(outbound_rx);
    let flushed = Arc::clone(&transport.flushed);
    let client = JacquardBleClient::new_with_transport_and_round_interval_ms_for_testing(
        local_node_id,
        direct_peer_topology(local_node_id, remote_node_id),
        transport,
        transport_sender,
        BRIDGE_ROUND_FOR_TESTS_MS,
    );
    let state = JacquardBleHostState::default();
    state
        .install_for_testing(JacquardBleService::from_client_for_testing(client))
        .await
        .expect("install service");

    state
        .send(remote_node_id, b"phase-13 send")
        .await
        .expect("send through host state");

    let topology = state.topology().await.expect("read topology");
    assert_eq!(topology.local_node_id, local_node_id);
    assert!(topology.nodes.contains_key(&remote_node_id));

    // The flushed frame must contain exactly one application-framed payload with
    // correct source and destination addresses.
    {
        let flushed = flushed.lock().expect("flushed frames");
        let application_frames = flushed
            .iter()
            .filter_map(|frame| {
                decode_client_payload_for_testing(&frame.payload)
                    .and_then(|payload| decode_application_frame(&payload))
            })
            .collect::<Vec<_>>();
        assert_eq!(application_frames.len(), 1);
        let (from_node_id, destination_node_id, payload) = application_frames[0].clone();
        assert_eq!(from_node_id, local_node_id);
        assert_eq!(destination_node_id, remote_node_id);
        assert_eq!(payload, b"phase-13 send");
    }

    // A second install on the same host state must fail with AlreadyStarted.
    let (duplicate_tx, duplicate_rx) = dispatch_mailbox(HARNESS_MAILBOX_CAPACITY);
    let duplicate_client = JacquardBleClient::new_with_transport_and_round_interval_ms_for_testing(
        local_node_id,
        direct_peer_topology(local_node_id, remote_node_id),
        FakeTransport::new(duplicate_rx),
        TestTransportSender {
            outbound: duplicate_tx,
        },
        BRIDGE_ROUND_FOR_TESTS_MS,
    );
    let duplicate = state
        .install_for_testing(JacquardBleService::from_client_for_testing(
            duplicate_client,
        ))
        .await;
    assert!(matches!(
        duplicate,
        Err(JacquardBleServiceError::AlreadyStarted { .. })
    ));
}

// Verifies the core relay property: A sends to E through B, C, D as unknown
// intermediaries. Each of B, C, D must relay the frame without exposing it to
// their application layer. The return path E to A must also succeed.
#[tokio::test(flavor = "current_thread")]
async fn five_node_line_relays_end_to_end_without_exposing_intermediaries_to_endpoints() {
    let harness = RelayHarness::five_node_line().await;
    let node_a = node_id(1);
    let node_b = node_id(2);
    let node_c = node_id(3);
    let node_d = node_id(4);
    let node_e = node_id(5);

    // Wait for each endpoint to discover its direct neighbor before sending.
    assert!(
        harness
            .wait_until_rounds(DISCOVERY_ROUNDS, |h| {
                let topology_a = h.node(node_a).topology();
                let topology_e = h.node(node_e).topology();
                topology_a.nodes.contains_key(&node_b) && topology_e.nodes.contains_key(&node_d)
            })
            .await,
        "line graph should discover direct neighbors"
    );

    // A and E must each see only their immediate neighbor, not the full line.
    let topology_a = harness.node(node_a).topology();
    let topology_e = harness.node(node_e).topology();
    assert_eq!(
        topology_a.nodes.keys().copied().collect::<Vec<_>>(),
        vec![node_a, node_b]
    );
    assert_eq!(
        topology_e.nodes.keys().copied().collect::<Vec<_>>(),
        vec![node_d, node_e]
    );

    let service_a = Arc::clone(&harness.node(node_a).service);
    let service_b = Arc::clone(&harness.node(node_b).service);
    let service_c = Arc::clone(&harness.node(node_c).service);
    let service_d = Arc::clone(&harness.node(node_d).service);
    let service_e = Arc::clone(&harness.node(node_e).service);
    let mut incoming_a = Box::pin(service_a.incoming_messages());
    let mut incoming_b = Box::pin(service_b.incoming_messages());
    let mut incoming_c = Box::pin(service_c.incoming_messages());
    let mut incoming_d = Box::pin(service_d.incoming_messages());
    let mut incoming_e = Box::pin(service_e.incoming_messages());

    harness.pump_rounds(CONVERGENCE_ROUNDS).await;
    harness.clear_deliveries();

    let received_by_e = deliver_with_retries(
        &harness,
        node_a,
        node_e,
        b"unknown-intermediary",
        &mut incoming_e,
        CONVERGENCE_ROUNDS,
    )
    .await;
    assert_eq!(received_by_e.from_node_id, node_a);
    assert_eq!(received_by_e.payload, b"unknown-intermediary");

    // B, C, and D relay the frame but must not surface it as an incoming message.
    assert!(
        harness
            .wait_for_stream_item(&mut incoming_b, QUIET_ROUNDS)
            .await
            .is_none(),
        "B must not see transit application payloads"
    );
    assert!(
        harness
            .wait_for_stream_item(&mut incoming_c, QUIET_ROUNDS)
            .await
            .is_none(),
        "C must not see transit application payloads"
    );
    assert!(
        harness
            .wait_for_stream_item(&mut incoming_d, QUIET_ROUNDS)
            .await
            .is_none(),
        "D must not see transit application payloads"
    );

    // The path traverses both GATT and L2CAP edges, so both transport kinds
    // must appear in the recorded deliveries for this payload.
    let kinds_seen = harness
        .deliveries()
        .into_iter()
        .filter_map(|delivery| {
            decode_client_payload_for_testing(&delivery.payload)
                .and_then(|payload| decode_application_frame(&payload))
                .and_then(|(from_node_id, destination_node_id, payload)| {
                    (from_node_id == node_a
                        && destination_node_id == node_e
                        && payload == b"unknown-intermediary")
                        .then_some(delivery.transport_kind)
                })
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert!(kinds_seen.contains(&TransportKind::BleGatt));
    assert!(kinds_seen.contains(&TransportKind::BleL2cap));

    // Verify the return path independently.
    harness.pump_rounds(QUIET_ROUNDS).await;
    harness.clear_deliveries();

    let received_by_a = deliver_with_retries(
        &harness,
        node_e,
        node_a,
        b"return-path",
        &mut incoming_a,
        CONVERGENCE_ROUNDS,
    )
    .await;
    assert_eq!(received_by_a.from_node_id, node_e);
    assert_eq!(received_by_a.payload, b"return-path");
}

// Verifies that a faulted link silences end-to-end delivery and that delivery
// resumes correctly once the link is restored.
#[tokio::test(flavor = "current_thread")]
async fn transient_disconnect_drops_delivery_until_link_is_repaired() {
    let mut harness = RelayHarness::five_node_line().await;
    let node_a = node_id(1);
    let node_c = node_id(3);
    let node_d = node_id(4);
    let node_e = node_id(5);

    let service_e = Arc::clone(&harness.node(node_e).service);
    let mut incoming_e = Box::pin(service_e.incoming_messages());

    harness.pump_rounds(CONVERGENCE_ROUNDS).await;

    let baseline = deliver_with_retries(
        &harness,
        node_a,
        node_e,
        b"before-fault",
        &mut incoming_e,
        CONVERGENCE_ROUNDS,
    )
    .await;
    assert_eq!(baseline.from_node_id, node_a);
    assert_eq!(baseline.payload, b"before-fault");

    // Fault the C--D link to break the only path from A to E.
    harness
        .set_link_state(
            node_c,
            node_d,
            TransportKind::BleGatt,
            LinkRuntimeState::Faulted,
        )
        .await;
    harness.pump_rounds(CONVERGENCE_ROUNDS).await;

    let faulted_send = harness
        .node(node_a)
        .state
        .send(node_e, b"during-fault")
        .await;
    assert!(matches!(
        faulted_send,
        Ok(())
            | Err(JacquardBleServiceError::Client(BleClientError::Route(
                RouteError::Selection(RouteSelectionError::NoCandidate)
            )))
    ));
    assert!(
        wait_for_message(
            &harness,
            &mut incoming_e,
            node_a,
            b"during-fault",
            QUIET_ROUNDS
        )
        .await
        .is_none(),
        "no end-to-end delivery should occur while the middle link is faulted"
    );

    // Restore the link and confirm delivery resumes.
    harness
        .set_link_state(
            node_c,
            node_d,
            TransportKind::BleGatt,
            LinkRuntimeState::Active,
        )
        .await;
    harness.pump_rounds(CONVERGENCE_ROUNDS).await;

    let repaired = deliver_with_retries(
        &harness,
        node_a,
        node_e,
        b"after-repair",
        &mut incoming_e,
        CONVERGENCE_ROUNDS,
    )
    .await;
    assert_eq!(repaired.from_node_id, node_a);
    assert_eq!(repaired.payload, b"after-repair");
}

#[tokio::test(flavor = "current_thread")]
async fn stalled_relay_does_not_block_local_delivery_or_leak_transit_payloads() {
    let harness = RelayHarness::five_node_line().await;
    let node_a = node_id(1);
    let node_b = node_id(2);
    let unreachable_node = node_id(9);

    harness.pump_rounds(CONVERGENCE_ROUNDS).await;

    let service_b = Arc::clone(&harness.node(node_b).service);
    let mut incoming_b = Box::pin(service_b.incoming_messages());

    harness
        .node(node_b)
        .ingress_sender
        .emit(
            TransportIngressClass::Payload,
            payload_received(
                node_a,
                ble_endpoint(node_byte(node_a), TransportKind::BleGatt),
                &encode_client_application_frame(node_a, unreachable_node, b"transit-should-fail"),
            ),
        )
        .expect("inject failing transit frame");

    harness
        .node(node_b)
        .ingress_sender
        .emit(
            TransportIngressClass::Payload,
            payload_received(
                node_a,
                ble_endpoint(node_byte(node_a), TransportKind::BleGatt),
                &encode_client_application_frame(node_a, node_b, b"deliver-local"),
            ),
        )
        .expect("inject local frame");

    let delivered = wait_for_message(
        &harness,
        &mut incoming_b,
        node_a,
        b"deliver-local",
        CONVERGENCE_ROUNDS * 16,
    )
    .await
    .expect("local delivery should not be blocked by relay retries");
    assert_eq!(delivered.from_node_id, node_a);
    assert_eq!(delivered.payload, b"deliver-local");
    assert!(
        wait_for_message(
            &harness,
            &mut incoming_b,
            node_a,
            b"transit-should-fail",
            QUIET_ROUNDS,
        )
        .await
        .is_none(),
        "transit payloads must not leak into the relay node application stream"
    );
}
