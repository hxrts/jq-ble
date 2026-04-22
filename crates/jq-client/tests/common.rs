//! Shared test utilities for jq-client integration tests.
//!
//! Declares `#[allow(dead_code)]` at the module level because not every item
//! is used by every test binary that includes this module.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use jacquard_core::{
    ByteCount, Configuration, EndpointLocator, Environment, FactSourceClass, LinkEndpoint, NodeId,
    Observation, OriginAuthenticationClass, RatioPermille, RouteEpoch, RoutingEvidenceClass, Tick,
    TransportError, TransportIngressEvent, TransportKind,
};
use jacquard_host_support::{
    DispatchReceiver, DispatchSender, TransportIngressNotifier, transport_ingress_mailbox,
};
use jacquard_testkit::topology;
use jacquard_traits::{TransportSenderEffects, effect_handler};
use jq_client::BleBridgeIo;

const MAILBOX_CAPACITY: usize = 64;

// --- Transport test types ----------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboundFrame {
    pub endpoint: LinkEndpoint,
    pub payload: Vec<u8>,
}

#[derive(Clone)]
pub struct TestTransportSender {
    pub outbound: DispatchSender<OutboundFrame>,
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

pub struct FakeTransport {
    pub ingress_sender: jacquard_host_support::TransportIngressSender,
    pub ingress_receiver: jacquard_host_support::TransportIngressReceiver,
    pub notifier: TransportIngressNotifier,
    pub outbound: DispatchReceiver<OutboundFrame>,
    pub flushed: Arc<Mutex<Vec<OutboundFrame>>>,
}

impl FakeTransport {
    #[must_use]
    pub fn new(outbound: DispatchReceiver<OutboundFrame>) -> Self {
        // recursion-exception: constructor assembles the fake transport while retaining the conventional `new` entrypoint
        let (ingress_sender, ingress_receiver, notifier) =
            transport_ingress_mailbox(MAILBOX_CAPACITY);
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

// --- Endpoint factory --------------------------------------------------------

/// Constructs a BLE `LinkEndpoint` with a single-byte `ScopedBytes` locator.
/// `byte` uniquely identifies the remote peer within a test.
#[must_use]
pub fn ble_endpoint(byte: u8, transport_kind: TransportKind) -> LinkEndpoint {
    LinkEndpoint::new(
        transport_kind,
        EndpointLocator::ScopedBytes {
            scope: "ble".into(),
            bytes: vec![byte],
        },
        ByteCount(512),
    )
}

// --- Topology builders -------------------------------------------------------
//
// These use `jacquard_reference_client::topology` helpers (simpler, profile-free
// node constructors). Tests that need full `NodePreset`-based topologies should
// build their own builders locally.

/// Two-node topology (local=1, remote=2) with one active link; suitable for
/// send-path and route-activation tests.
#[must_use]
pub fn published_topology() -> Observation<Configuration> {
    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([
                (
                    NodeId([1; 32]),
                    topology::node(1).pathway_and_batman_bellman().build(),
                ),
                (
                    NodeId([2; 32]),
                    topology::node(2).pathway_and_batman_bellman().build(),
                ),
            ]),
            links: BTreeMap::from([
                (
                    (NodeId([1; 32]), NodeId([2; 32])),
                    topology::link(2)
                        .with_confidence(RatioPermille(900))
                        .build(),
                ),
                (
                    (NodeId([2; 32]), NodeId([1; 32])),
                    topology::link(1)
                        .with_confidence(RatioPermille(900))
                        .build(),
                ),
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

/// Single-node topology containing only the local node; suitable for tests
/// that inject link observations to drive topology discovery.
#[must_use]
pub fn local_only_topology(local_node_id: NodeId) -> Observation<Configuration> {
    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([(
                local_node_id,
                topology::node(1).pathway_and_batman_bellman().build(),
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
