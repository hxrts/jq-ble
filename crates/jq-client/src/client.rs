//! Top-level BLE client handle.
//!
//! [`JacquardBleClient`] wires together the BLE transport, Jacquard router
//! (Batman + Pathway), host bridge, and topology projector. It spawns a
//! dedicated single-threaded runtime and exposes `send`, `incoming`, and
//! `topology_stream` to the application.
//!
//! Router and bridge construction helpers (`build_router`, `initial_topology`,
//! `policy_inputs_for`, `default_profile`, `default_objective`) are kept
//! module-private. Use [`JacquardBleClient::new`] as the entry point.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use jacquard_adapter::opaque_endpoint;
use jacquard_batman::{BATMAN_ENGINE_ID, BatmanEngine};
use jacquard_core::{
    ByteCount, Configuration, ConnectivityPosture, ControllerId, DestinationId, DurationMs,
    Environment, FactSourceClass, HealthScore, HoldFallbackPolicy, IdentityAssuranceClass, Limit,
    NodeId, Observation, OriginAuthenticationClass, PriorityPoints, RatioPermille, RouteEpoch,
    RouteError, RoutePartitionClass, RouteProtectionClass, RouteRepairClass, RouteServiceKind,
    RoutingEvidenceClass, RoutingObjective, RoutingPolicyInputs, RoutingTickHint,
    SelectedRoutingParameters, Tick, TransportError, TransportKind, TransportObservation,
};
use jacquard_mem_link_profile::InMemoryRetentionStore;
use jacquard_mem_node_profile::{NodeIdentity, NodePreset, NodePresetOptions};
use jacquard_pathway::{DeterministicPathwayTopologyModel, PATHWAY_ENGINE_ID, PathwayEngine};
use jacquard_router::{FixedPolicyEngine, MultiEngineRouter};
use jacquard_traits::{Blake3Hashing, Router, RoutingDataPlane, TransportSenderEffects};
use jq_link_profile::{BleConfig, BleLinkError, BleTransportComponents};
use jq_node_profile::MeshTopology;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;

use crate::projector::TopologyProjector;
use crate::{
    BleBridgeConfig, BleBridgeError, BleBridgeIo, BleBridgeProgress, BleBridgeRoundReport,
    BleBridgeTransport, BleHostBridge, BleRuntimeEffects,
};

const DEFAULT_CLIENT_COMMAND_CAPACITY: usize = 64;
const DEFAULT_CLIENT_TICK_DURATION_MS: Duration = Duration::from_millis(25);
const CLIENT_PAYLOAD_MAGIC: &[u8; 8] = b"JQCLIENT";

pub type JacquardBleRouter = MultiEngineRouter<FixedPolicyEngine, BleRuntimeEffects>;

#[derive(Debug, Error)]
pub enum BleClientError {
    #[error(transparent)]
    Link(#[from] BleLinkError),
    #[error(transparent)]
    Route(#[from] RouteError),
    #[error(transparent)]
    Bridge(#[from] BleBridgeError),
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("jacquard ble client runtime is no longer running")]
    RuntimeStopped,
}

enum ClientCommand {
    Send {
        to: NodeId,
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<(), BleClientError>>,
    },
}

fn encode_client_payload(payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(CLIENT_PAYLOAD_MAGIC.len() + payload.len());
    framed.extend_from_slice(CLIENT_PAYLOAD_MAGIC);
    framed.extend_from_slice(payload);
    framed
}

fn decode_client_payload(payload: &[u8]) -> Option<&[u8]> {
    payload
        .starts_with(CLIENT_PAYLOAD_MAGIC)
        .then_some(&payload[CLIENT_PAYLOAD_MAGIC.len()..])
}

struct ClientTask<Transport> {
    local_node_id: NodeId,
    bridge: BleHostBridge<JacquardBleRouter, Transport, BleRuntimeEffects>,
    commands: mpsc::Receiver<ClientCommand>,
    incoming: broadcast::Sender<(NodeId, Bytes)>,
    topology: Arc<Mutex<MeshTopology>>,
    topology_updates: broadcast::Sender<MeshTopology>,
    projector: TopologyProjector,
    route_cache: BTreeMap<NodeId, jacquard_core::RouteId>,
    tick_duration_ms: Duration,
    shutdown: watch::Receiver<bool>,
}

impl<Transport> ClientTask<Transport>
where
    Transport: BleBridgeIo + Send + 'static,
{
    async fn run(mut self) {
        loop {
            if *self.shutdown.borrow() {
                break;
            }
            if !self.drain_ready_commands().await {
                break;
            }

            match self.drive_bridge_round().await {
                Ok(BleBridgeProgress::Advanced(_)) => continue,
                Ok(BleBridgeProgress::Waiting(wait_state)) => {
                    if !self.wait_for_work(wait_state).await {
                        break;
                    }
                }
                Err(_) => {
                    if !self.wait_after_bridge_error().await {
                        break;
                    }
                }
            }
        }
    }

    async fn drain_ready_commands(&mut self) -> bool {
        loop {
            match self.commands.try_recv() {
                Ok(command) => self.handle_command(command).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return true,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return false,
            }
        }
    }

    async fn handle_command(&mut self, command: ClientCommand) {
        match command {
            ClientCommand::Send { to, payload, reply } => {
                let result = self.handle_send(to, payload).await;
                // Caller may have been dropped (fire-and-forget); a closed oneshot is not an error.
                // allow-ignored-result: reply delivery is best-effort when the caller dropped the oneshot receiver
                let _ = reply.send(result);
            }
        }
    }

    async fn handle_send(&mut self, to: NodeId, payload: Vec<u8>) -> Result<(), BleClientError> {
        if let Some(result) = self.try_cached_forward(to, &payload).await {
            return result;
        }
        self.activate_and_forward(to, &payload).await
    }

    // Fast path: reuse the cached route if it is still valid.
    // Returns Some(result) if the cache was hit (success or drive_bridge_round error),
    // or None if the route was missing or stale (caller should activate a new one).
    async fn try_cached_forward(
        &mut self,
        to: NodeId,
        payload: &[u8],
    ) -> Option<Result<(), BleClientError>> {
        let route_id = self.route_cache.get(&to).copied()?;
        if self
            .bridge
            .router_mut()
            .forward_payload(&route_id, payload)
            .is_ok()
        {
            return Some(self.drive_bridge_round().await.map(|_| ()));
        }
        // Route is stale; evict it and signal that the slow path is needed.
        self.route_cache.remove(&to);
        None
    }

    async fn activate_and_forward(
        &mut self,
        to: NodeId,
        payload: &[u8],
    ) -> Result<(), BleClientError> {
        // Slow path: materialize a new route then forward the payload in one step.
        let route = Router::activate_route(self.bridge.router_mut(), default_objective(to))?;
        let route_id = route.identity.stamp.route_id;
        let topology_changed = self.projector.ingest_materialized_route(&route);
        self.bridge
            .router_mut()
            .forward_payload(&route_id, payload)?;
        self.route_cache.insert(to, route_id);
        self.drive_bridge_round().await?;
        if topology_changed {
            self.publish_topology();
        }
        Ok(())
    }

    async fn drive_bridge_round(&mut self) -> Result<BleBridgeProgress, BleClientError> {
        let progress = self.bridge.advance_round().await?;
        let mut changed = self.apply_bridge_progress(&progress);
        // When the router requests an immediate next round (e.g. it queued internal work), run one
        // extra round in the same call instead of parking the client loop.
        let run_follow_up = matches!(
            &progress,
            BleBridgeProgress::Advanced(report)
                if matches!(
                    report.router_outcome.next_round_hint,
                    RoutingTickHint::Immediate
                )
        );
        let final_progress = if run_follow_up {
            let follow_up = self.bridge.advance_round().await?;
            changed |= self.apply_bridge_progress(&follow_up);
            follow_up
        } else {
            progress
        };
        if changed {
            self.publish_topology();
        }
        Ok(final_progress)
    }

    fn apply_bridge_progress(&mut self, progress: &BleBridgeProgress) -> bool {
        match progress {
            BleBridgeProgress::Advanced(report) => self.apply_bridge_report(report),
            BleBridgeProgress::Waiting(_) => false,
        }
    }

    fn apply_bridge_report(&mut self, report: &BleBridgeRoundReport) -> bool {
        let mut changed = false;
        for observation in &report.ingested_transport_observations {
            changed |= self.projector.ingest_transport_observation(observation);
        }
        self.publish_incoming(&report.ingested_transport_observations);
        if changed {
            // Feed the updated topology back into the router so it can recalculate routing policy.
            let topology = self.projector.topology_observation();
            self.bridge
                .router_mut()
                .ingest_topology_observation(topology.clone());
            self.bridge
                .router_mut()
                .ingest_policy_inputs(policy_inputs_for(&topology, self.local_node_id));
        }
        changed | self.projector.ingest_round_outcome(&report.router_outcome)
    }

    fn publish_incoming(&self, observations: &[TransportObservation]) {
        for observation in observations {
            if let TransportObservation::PayloadReceived {
                from_node_id,
                payload,
                ..
            } = observation
            {
                let Some(payload) = decode_client_payload(payload) else {
                    continue;
                };
                // Broadcast drops when no subscribers are active; that is not an error.
                // allow-ignored-result: incoming fanout is best-effort when no subscriber is attached
                let _ = self
                    .incoming
                    .send((*from_node_id, Bytes::copy_from_slice(payload)));
            }
        }
    }

    fn publish_topology(&self) {
        let snapshot = self.projector.snapshot();
        *self.topology.lock().expect("topology state lock") = snapshot.clone();
        // Broadcast drops when no subscribers are active; that is not an error.
        // allow-ignored-result: topology fanout is best-effort when no subscriber is attached
        let _ = self.topology_updates.send(snapshot);
    }

    async fn wait_for_work(&mut self, wait_state: crate::BleBridgeWaitState) -> bool {
        match self.bridge.wait_decision(&wait_state) {
            crate::BleBridgeWaitDecision::AdvanceImmediately => true,
            crate::BleBridgeWaitDecision::WaitUntilTick(ticks) => {
                self.wait_for_tick_or_work(ticks).await
            }
            crate::BleBridgeWaitDecision::BlockOnNotifier { snapshot } => {
                self.wait_for_notifier_or_work(snapshot).await
            }
        }
    }

    async fn wait_for_tick_or_work(&mut self, ticks: Tick) -> bool {
        let sleep = tokio::time::sleep(duration_for_tick_hint(self.tick_duration_ms, ticks));
        let notifier = self.bridge.notifier().clone();
        let snapshot = notifier.snapshot();
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            changed = self.shutdown.changed() => changed.is_err() || !*self.shutdown.borrow(),
            command = self.commands.recv() => self.handle_wait_command(command).await,
            _ = notifier.changed(snapshot) => true,
            _ = &mut sleep => true,
        }
    }

    async fn wait_for_notifier_or_work(&mut self, snapshot: u64) -> bool {
        let notifier = self.bridge.notifier().clone();
        if notifier.has_changed_since(snapshot) {
            return true;
        }
        tokio::select! {
            biased;
            changed = self.shutdown.changed() => changed.is_err() || !*self.shutdown.borrow(),
            command = self.commands.recv() => self.handle_wait_command(command).await,
            _ = notifier.changed(snapshot) => true,
        }
    }

    async fn wait_after_bridge_error(&mut self) -> bool {
        let notifier = self.bridge.notifier().clone();
        let snapshot = notifier.snapshot();
        let sleep = tokio::time::sleep(self.tick_duration_ms);
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            changed = self.shutdown.changed() => changed.is_err() || !*self.shutdown.borrow(),
            command = self.commands.recv() => self.handle_wait_command(command).await,
            _ = notifier.changed(snapshot) => true,
            _ = &mut sleep => true,
        }
    }

    async fn handle_wait_command(&mut self, command: Option<ClientCommand>) -> bool {
        let Some(command) = command else {
            return false;
        };
        self.handle_command(command).await;
        true
    }
}

pub struct JacquardBleClient {
    local_node_id: NodeId,
    commands: mpsc::Sender<ClientCommand>,
    incoming: broadcast::Sender<(NodeId, Bytes)>,
    topology: Arc<Mutex<MeshTopology>>,
    topology_updates: broadcast::Sender<MeshTopology>,
    shutdown: watch::Sender<bool>,
    runtime_thread: Option<std::thread::JoinHandle<()>>,
}

impl JacquardBleClient {
    pub async fn new(local_node_id: NodeId) -> Result<Self, BleClientError> {
        // recursion-exception: constructor delegates to transport assembly with the same semantic name
        let components = BleTransportComponents::new(local_node_id, BleConfig::default()).await?;
        let (driver, sender, outbound, control, notifier, runtime_task) = components.into_parts();
        let transport =
            BleBridgeTransport::from_parts(driver, outbound, control, notifier, runtime_task);
        Ok(Self::spawn_with_transport(
            local_node_id,
            initial_topology(local_node_id, Tick(1)),
            transport,
            sender,
            DEFAULT_CLIENT_TICK_DURATION_MS,
        ))
    }

    #[doc(hidden)]
    #[must_use]
    pub fn new_with_transport_for_testing<Transport, Sender>(
        local_node_id: NodeId,
        topology: Observation<Configuration>,
        transport: Transport,
        transport_sender: Sender,
    ) -> Self
    where
        Transport: BleBridgeIo + Send + 'static,
        Sender: TransportSenderEffects + Clone + Send + 'static,
    {
        Self::spawn_with_transport(
            local_node_id,
            topology,
            transport,
            transport_sender,
            DEFAULT_CLIENT_TICK_DURATION_MS,
        )
    }

    #[doc(hidden)]
    #[must_use]
    pub fn new_with_transport_and_round_interval_ms_for_testing<Transport, Sender>(
        local_node_id: NodeId,
        topology: Observation<Configuration>,
        transport: Transport,
        transport_sender: Sender,
        tick_duration_ms: Duration,
    ) -> Self
    where
        Transport: BleBridgeIo + Send + 'static,
        Sender: TransportSenderEffects + Clone + Send + 'static,
    {
        Self::spawn_with_transport(
            local_node_id,
            topology,
            transport,
            transport_sender,
            tick_duration_ms,
        )
    }

    pub async fn send(&self, to: NodeId, payload: &[u8]) -> Result<(), BleClientError> {
        // recursion-exception: same-name forwarding keeps send on the top-level client handle
        let (reply_tx, reply_rx) = oneshot::channel();
        let payload = encode_client_payload(payload);
        self.commands
            .send(ClientCommand::Send {
                to,
                payload,
                reply: reply_tx,
            })
            .await
            .map_err(|_| BleClientError::RuntimeStopped)?;
        reply_rx.await.map_err(|_| BleClientError::RuntimeStopped)?
    }

    #[must_use = "dropping the incoming stream discards client payload events"]
    pub fn incoming(&self) -> impl Stream<Item = (NodeId, Bytes)> {
        BroadcastStream::new(self.incoming.subscribe())
            .filter_map(|result| async move { result.ok() })
    }

    #[must_use]
    pub fn topology(&self) -> MeshTopology {
        self.topology.lock().expect("topology state lock").clone()
    }

    #[must_use = "dropping the topology stream discards topology updates"]
    pub fn topology_stream(&self) -> impl Stream<Item = MeshTopology> {
        let initial = self.topology();
        // Prepend the current snapshot so the caller always gets an immediate first item.
        stream::once(async move { initial }).chain(
            BroadcastStream::new(self.topology_updates.subscribe())
                .filter_map(|result| async move { result.ok() }),
        )
    }

    #[must_use]
    pub fn local_node_id(&self) -> NodeId {
        self.local_node_id
    }

    fn spawn_with_transport<Transport, Sender>(
        local_node_id: NodeId,
        topology: Observation<Configuration>,
        transport: Transport,
        transport_sender: Sender,
        tick_duration_ms: Duration,
    ) -> Self
    where
        Transport: BleBridgeIo + Send + 'static,
        Sender: TransportSenderEffects + Clone + Send + 'static,
    {
        let initial_snapshot = TopologyProjector::new(local_node_id, topology.clone()).snapshot();
        let topology_state = Arc::new(Mutex::new(initial_snapshot.clone()));
        let topology_task_state = Arc::clone(&topology_state);
        let (commands_tx, commands_rx) = mpsc::channel(DEFAULT_CLIENT_COMMAND_CAPACITY);
        let (incoming_tx, _) = broadcast::channel(DEFAULT_CLIENT_COMMAND_CAPACITY);
        let incoming_task = incoming_tx.clone();
        let (topology_updates, _) = broadcast::channel(DEFAULT_CLIENT_COMMAND_CAPACITY);
        let topology_updates_task = topology_updates.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        // Dedicated OS thread with a single-threaded tokio runtime isolates the client event loop
        // from the caller's runtime and avoids Send requirements on the LocalSet-pinned futures.
        let runtime_thread = std::thread::spawn(move || {
            let effects = BleRuntimeEffects::new(topology.observed_at_tick);
            let router = build_router(
                local_node_id,
                topology.clone(),
                transport_sender,
                effects.clone(),
            )
            .expect("build router");
            let mut projector = TopologyProjector::new(local_node_id, topology);
            // Seed the projector with capabilities advertised by each engine so route shapes are
            // classified correctly from the first round.
            for engine_id in router.registered_engine_ids() {
                if let Some(capabilities) = router.registered_engine_capabilities(&engine_id) {
                    // false means no snapshot change; the projector is still seeded correctly.
                    // allow-ignored-result: capability seeding is best-effort because the projector already owns the initial topology
                    let _ = projector.ingest_engine_capabilities(capabilities);
                }
            }
            let bridge =
                BleHostBridge::with_clock(router, transport, effects, BleBridgeConfig::default());
            let task = ClientTask {
                local_node_id,
                bridge,
                commands: commands_rx,
                incoming: incoming_task,
                topology: topology_task_state,
                topology_updates: topology_updates_task,
                projector,
                route_cache: BTreeMap::new(),
                tick_duration_ms,
                shutdown: shutdown_rx,
            };
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build jq-client runtime");
            let local = tokio::task::LocalSet::new();
            local.block_on(&runtime, task.run());
        });

        Self {
            local_node_id,
            commands: commands_tx,
            incoming: incoming_tx,
            topology: topology_state,
            topology_updates,
            shutdown: shutdown_tx,
            runtime_thread: Some(runtime_thread),
        }
    }
}

impl Drop for JacquardBleClient {
    fn drop(&mut self) {
        // recursion-exception: drop uses same-name shutdown steps on owned channels and thread handles
        // drop-side-effects-exception: dropping the client is the explicit teardown point for the runtime thread
        // allow-ignored-result: shutdown notification is best-effort during teardown when the runtime already exited
        let _ = self.shutdown.send(true);
        let (replacement, _) = mpsc::channel(1);
        let commands = std::mem::replace(&mut self.commands, replacement);
        drop(commands);
        if let Some(runtime_thread) = self.runtime_thread.take() {
            // allow-ignored-result: thread join result is ignored during teardown because the runtime may already be unwinding
            let _ = runtime_thread.join();
        }
    }
}

#[doc(hidden)]
#[must_use]
pub fn encode_client_payload_for_testing(payload: &[u8]) -> Vec<u8> {
    encode_client_payload(payload)
}

#[doc(hidden)]
#[must_use]
pub fn decode_client_payload_for_testing(payload: &[u8]) -> Option<Vec<u8>> {
    decode_client_payload(payload).map(ToOwned::to_owned)
}

fn duration_for_tick_hint(round_interval_ms: Duration, ticks: Tick) -> Duration {
    round_interval_ms
        .checked_mul(u32::try_from(ticks.0).unwrap_or(u32::MAX))
        .unwrap_or(Duration::MAX)
}

fn build_router<Sender>(
    local_node_id: NodeId,
    topology: Observation<Configuration>,
    transport: Sender,
    effects: BleRuntimeEffects,
) -> Result<JacquardBleRouter, RouteError>
where
    Sender: TransportSenderEffects + Clone + Send + 'static,
{
    // Pathway handles structured multi-hop routing with deterministic topology modelling.
    let pathway_engine = PathwayEngine::without_committee_selector(
        local_node_id,
        DeterministicPathwayTopologyModel::new(),
        transport.clone(),
        InMemoryRetentionStore::default(),
        effects.clone(),
        Blake3Hashing,
    );

    let mut router = MultiEngineRouter::new(
        local_node_id,
        FixedPolicyEngine::new(default_profile()),
        effects.clone(),
        topology.clone(),
        policy_inputs_for(&topology, local_node_id),
    );
    router.register_engine(Box::new(pathway_engine))?;
    // Batman provides opportunistic flooding-based fallback when Pathway has no explicit path.
    router.register_engine(Box::new(BatmanEngine::new(
        local_node_id,
        transport,
        effects,
    )))?;
    Ok(router)
}

fn initial_topology(local_node_id: NodeId, observed_at_tick: Tick) -> Observation<Configuration> {
    // Bootstrap a single-node topology so the router has a valid configuration before any peers are discovered.
    let endpoint = opaque_endpoint(
        TransportKind::BleGatt,
        local_node_id.0.to_vec(),
        ByteCount(512),
    );
    let local_node = NodePreset::route_capable_for_engines(
        NodePresetOptions::new(
            NodeIdentity::new(local_node_id, ControllerId(local_node_id.0)),
            endpoint,
            observed_at_tick,
        ),
        &[PATHWAY_ENGINE_ID, BATMAN_ENGINE_ID],
    )
    .build();

    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([(local_node_id, local_node)]),
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
        observed_at_tick,
    }
}

fn policy_inputs_for(
    topology: &Observation<Configuration>,
    local_node_id: NodeId,
) -> RoutingPolicyInputs {
    RoutingPolicyInputs {
        local_node: Observation {
            value: topology.value.nodes[&local_node_id].clone(),
            source_class: topology.source_class,
            evidence_class: topology.evidence_class,
            origin_authentication: topology.origin_authentication,
            observed_at_tick: topology.observed_at_tick,
        },
        local_environment: Observation {
            value: topology.value.environment.clone(),
            source_class: topology.source_class,
            evidence_class: topology.evidence_class,
            origin_authentication: topology.origin_authentication,
            observed_at_tick: topology.observed_at_tick,
        },
        // Two engines registered: Pathway + Batman.
        routing_engine_count: 2,
        // Conservative BLE estimates that bias the policy engine toward repair-tolerant routes.
        median_rtt_ms: DurationMs(40),
        loss_permille: RatioPermille(50),
        partition_risk_permille: RatioPermille(150),
        adversary_pressure_permille: RatioPermille(25),
        identity_assurance: IdentityAssuranceClass::ControllerBound,
        direct_reachability_score: HealthScore(900),
    }
}

fn default_profile() -> SelectedRoutingParameters {
    SelectedRoutingParameters {
        selected_protection: RouteProtectionClass::LinkProtected,
        selected_connectivity: ConnectivityPosture {
            repair: RouteRepairClass::Repairable,
            partition: RoutePartitionClass::ConnectedOnly,
        },
        deployment_profile: jacquard_core::OperatingMode::DenseInteractive,
        diversity_floor: jacquard_core::DiversityFloor(1),
        routing_engine_fallback_policy: jacquard_core::RoutingEngineFallbackPolicy::Allowed,
        route_replacement_policy: jacquard_core::RouteReplacementPolicy::Allowed,
    }
}

fn default_objective(destination: NodeId) -> RoutingObjective {
    RoutingObjective {
        destination: DestinationId::Node(destination),
        // Move semantics: payload is consumed by the destination, not fanned out.
        service_kind: RouteServiceKind::Move,
        target_protection: RouteProtectionClass::LinkProtected,
        protection_floor: RouteProtectionClass::LinkProtected,
        target_connectivity: ConnectivityPosture {
            repair: RouteRepairClass::Repairable,
            // PartitionTolerant allows the router to queue messages while waiting for reconnect.
            partition: RoutePartitionClass::PartitionTolerant,
        },
        hold_fallback_policy: HoldFallbackPolicy::Allowed,
        latency_budget_ms: Limit::Bounded(DurationMs(250)),
        protection_priority: PriorityPoints(10),
        connectivity_priority: PriorityPoints(20),
    }
}
