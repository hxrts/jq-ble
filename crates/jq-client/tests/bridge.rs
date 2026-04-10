//! Tests for [`BleHostBridge`] round progression.
//!
//! Covers tick stamping in the bridge only, wait decision logic against router
//! hints and the ingress notifier, and FIFO ordering of flushed outbound
//! command batches.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jacquard_adapter::{
    DispatchReceiver, DispatchSender, TransportIngressClass, TransportIngressNotifier,
    dispatch_mailbox, transport_ingress_mailbox,
};
use jacquard_core::{
    ByteCount, EndpointLocator, FactSourceClass, LinkBuilder, LinkRuntimeState, NodeId,
    OriginAuthenticationClass, RepairCapability, RouteEpoch, RouterCanonicalMutation,
    RoutingEvidenceClass, RoutingTickChange, RoutingTickHint, Tick, TransportError,
    TransportIngressEvent, TransportKind, TransportObservation,
};
use jq_client::{
    BleBridgeConfig, BleBridgeIo, BleBridgeProgress, BleBridgeRouter, BleBridgeWaitDecision,
    BleBridgeWaitState, BleHostBridge,
};

fn endpoint(byte: u8) -> jacquard_core::LinkEndpoint {
    jacquard_core::LinkEndpoint::new(
        TransportKind::BleGatt,
        EndpointLocator::ScopedBytes {
            scope: "ble".into(),
            bytes: vec![byte],
        },
        ByteCount(128),
    )
}

fn link_observed(byte: u8) -> TransportIngressEvent {
    TransportIngressEvent::LinkObserved {
        remote_node_id: NodeId([byte; 32]),
        link: LinkBuilder::new(endpoint(byte))
            .with_profile(
                jacquard_core::DurationMs(150),
                RepairCapability::TransportRetransmit,
                jacquard_core::PartitionRecoveryClass::LocalReconnect,
            )
            .with_runtime_state(LinkRuntimeState::Active)
            .build(),
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Unauthenticated,
    }
}

#[derive(Clone)]
struct TestRouter {
    ingested: Arc<Mutex<Vec<TransportObservation>>>,
    outbound_queue: Option<DispatchSender<Vec<u8>>>,
    next_hint: RoutingTickHint,
    change: RoutingTickChange,
}

impl TestRouter {
    fn new(next_hint: RoutingTickHint, change: RoutingTickChange) -> Self {
        Self {
            ingested: Arc::new(Mutex::new(Vec::new())),
            outbound_queue: None,
            next_hint,
            change,
        }
    }

    fn with_outbound_queue(
        next_hint: RoutingTickHint,
        change: RoutingTickChange,
        outbound_queue: DispatchSender<Vec<u8>>,
    ) -> Self {
        Self {
            ingested: Arc::new(Mutex::new(Vec::new())),
            outbound_queue: Some(outbound_queue),
            next_hint,
            change,
        }
    }
}

impl BleBridgeRouter for TestRouter {
    fn ingest_transport_observation(
        &mut self,
        observation: &TransportObservation,
    ) -> Result<(), jacquard_core::RouteError> {
        self.ingested
            .lock()
            .expect("ingested log")
            .push(observation.clone());
        Ok(())
    }

    fn advance_round(
        &mut self,
    ) -> Result<jacquard_core::RouterRoundOutcome, jacquard_core::RouteError> {
        if let Some(queue) = &self.outbound_queue {
            queue.send(b"first".to_vec()).expect("enqueue first");
            queue.send(b"second".to_vec()).expect("enqueue second");
        }

        Ok(jacquard_core::RouterRoundOutcome {
            topology_epoch: RouteEpoch(1),
            engine_change: self.change,
            next_round_hint: self.next_hint,
            canonical_mutation: RouterCanonicalMutation::None,
        })
    }
}

struct FakeTransport {
    ingress_sender: jacquard_adapter::TransportIngressSender,
    ingress_receiver: jacquard_adapter::TransportIngressReceiver,
    notifier: TransportIngressNotifier,
    pending_outbound_tx: DispatchSender<Vec<u8>>,
    pending_outbound_rx: DispatchReceiver<Vec<u8>>,
    flushed_outbound: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl FakeTransport {
    fn new() -> Self {
        let (ingress_sender, ingress_receiver, notifier) = transport_ingress_mailbox(16);
        let (pending_outbound_tx, pending_outbound_rx) = dispatch_mailbox(16);
        Self {
            ingress_sender,
            ingress_receiver,
            notifier,
            pending_outbound_tx,
            pending_outbound_rx,
            flushed_outbound: Arc::new(Mutex::new(Vec::new())),
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
            let flushed = self.pending_outbound_rx.drain();
            let len = flushed.len();
            self.flushed_outbound
                .lock()
                .expect("flushed outbound")
                .extend(flushed);
            Ok(len)
        })
    }

    fn pending_outbound(&self) -> usize {
        self.pending_outbound_rx.pending_len()
    }

    fn notifier(&self) -> &TransportIngressNotifier {
        &self.notifier
    }
}

#[tokio::test]
async fn ingress_is_stamped_in_bridge_only() {
    let transport = FakeTransport::new();
    let ingress_sender = transport.ingress_sender.clone();
    let router = TestRouter::new(RoutingTickHint::HostDefault, RoutingTickChange::NoChange);
    let ingested = router.ingested.clone();
    let mut bridge = BleHostBridge::new(router, transport, Tick(9), BleBridgeConfig::default());

    ingress_sender
        .emit(TransportIngressClass::Control, link_observed(7))
        .expect("emit raw ingress");

    let progress = bridge.advance_round().await.expect("advance round");
    let BleBridgeProgress::Advanced(report) = progress else {
        panic!("expected advanced progress");
    };
    assert_eq!(report.ingested_transport_observations.len(), 1);
    match &report.ingested_transport_observations[0] {
        TransportObservation::LinkObserved { observation, .. } => {
            assert_eq!(observation.observed_at_tick, Tick(9));
        }
        other => panic!("unexpected observation: {other:?}"),
    }
    assert_eq!(ingested.lock().expect("ingested log").len(), 1);
}

#[tokio::test]
async fn wait_decision_uses_notifier_and_router_hint() {
    let transport = FakeTransport::new();
    let ingress_sender = transport.ingress_sender.clone();
    let mut bridge = BleHostBridge::new(
        TestRouter::new(RoutingTickHint::HostDefault, RoutingTickChange::NoChange),
        transport,
        Tick(1),
        BleBridgeConfig::default(),
    );

    let waiting = bridge.advance_round().await.expect("advance round");
    let BleBridgeProgress::Waiting(wait_state) = waiting else {
        panic!("expected waiting progress");
    };
    let decision = bridge.wait_decision(&wait_state);
    assert!(matches!(
        decision,
        BleBridgeWaitDecision::BlockOnNotifier { .. }
    ));

    let notifier = bridge.notifier().clone();
    let waiter = std::thread::spawn(move || {
        decision.wait(&notifier);
    });
    std::thread::sleep(Duration::from_millis(20));
    ingress_sender
        .emit(TransportIngressClass::Control, link_observed(3))
        .expect("emit wake ingress");
    waiter.join().expect("waiter thread");

    let immediate = bridge.wait_decision(&BleBridgeWaitState {
        next_round_hint: RoutingTickHint::Immediate,
        pending_transport_observations: 0,
        pending_transport_commands: 0,
        dropped_transport_observations: 0,
        notifier_snapshot: bridge.notifier().snapshot(),
    });
    assert_eq!(immediate, BleBridgeWaitDecision::AdvanceImmediately);

    let hinted = bridge.wait_decision(&BleBridgeWaitState {
        next_round_hint: RoutingTickHint::WithinTicks(Tick(4)),
        pending_transport_observations: 0,
        pending_transport_commands: 0,
        dropped_transport_observations: 0,
        notifier_snapshot: bridge.notifier().snapshot(),
    });
    assert_eq!(hinted, BleBridgeWaitDecision::WaitUntilTick(Tick(4)));
}

#[tokio::test]
async fn outbound_batches_are_flushed_after_round_in_fifo_order() {
    let transport = FakeTransport::new();
    let outbound_queue = transport.pending_outbound_tx.clone();
    let flushed = transport.flushed_outbound.clone();
    let mut bridge = BleHostBridge::new(
        TestRouter::with_outbound_queue(
            RoutingTickHint::HostDefault,
            RoutingTickChange::PrivateStateUpdated,
            outbound_queue,
        ),
        transport,
        Tick(1),
        BleBridgeConfig::default(),
    );

    let progress = bridge.advance_round().await.expect("advance round");
    let BleBridgeProgress::Advanced(report) = progress else {
        panic!("expected advanced progress");
    };
    assert_eq!(report.flushed_transport_commands, 2);

    let flushed = flushed.lock().expect("flushed outbound");
    assert_eq!(flushed.len(), 2);
    assert_eq!(flushed[0], b"first");
    assert_eq!(flushed[1], b"second");
}
