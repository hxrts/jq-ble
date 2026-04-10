//! Host bridge between the BLE transport layer and the Jacquard router.
//!
//! [`BleHostBridge`] drains raw tick-free ingress from [`BleBridgeIo`],
//! stamps observations with Jacquard [`Tick`] via [`BleBridgeTickSource`],
//! ingests them into the router, advances one synchronous round, then
//! flushes the queued outbound commands back to the BLE runtime task.
//!
//! The bridge waits on ingress readiness and router scheduling hints rather
//! than a fixed poll interval.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

use jacquard_adapter::{DispatchReceiver, TransportIngressNotifier};
use jacquard_core::{
    RouteError, RouterRoundOutcome, RoutingTickChange, RoutingTickHint, Tick, TransportError,
    TransportIngressEvent, TransportObservation,
};
use jacquard_traits::{RoutingControlPlane, RoutingMiddleware, TransportDriver};
use jq_link_profile::{
    BleDriverControl, BleOutboundCommand, BleTransportComponents, BleTransportDriver,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BleBridgeError {
    #[error(transparent)]
    Route(#[from] RouteError),
    #[error(transparent)]
    Transport(#[from] TransportError),
}

pub trait BleBridgeRouter {
    fn ingest_transport_observation(
        &mut self,
        observation: &TransportObservation,
    ) -> Result<(), RouteError>;

    fn advance_round(&mut self) -> Result<RouterRoundOutcome, RouteError>;
}

// Blanket implementation so any router that satisfies both traits can be used as a bridge router.
impl<T> BleBridgeRouter for T
where
    T: RoutingMiddleware + RoutingControlPlane,
{
    fn ingest_transport_observation(
        &mut self,
        observation: &TransportObservation,
    ) -> Result<(), RouteError> {
        // recursion-exception: same-name delegation binds the bridge trait to the router middleware trait
        RoutingMiddleware::ingest_transport_observation(self, observation)
    }

    fn advance_round(&mut self) -> Result<RouterRoundOutcome, RouteError> {
        // recursion-exception: same-name delegation binds the bridge trait to the router control-plane trait
        RoutingControlPlane::advance_round(self)
    }
}

pub trait BleBridgeIo {
    fn drain_raw_ingress(&mut self) -> Result<Vec<TransportIngressEvent>, TransportError>;
    fn flush_outbound(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<usize, TransportError>> + Send + '_>>;
    fn pending_outbound(&self) -> usize;
    fn notifier(&self) -> &TransportIngressNotifier;
}

pub trait BleBridgeTickSource {
    fn advance_tick(&mut self) -> Tick;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonotonicBleTickSource {
    next_tick: Tick,
}

impl MonotonicBleTickSource {
    #[must_use]
    pub fn new(initial_tick: Tick) -> Self {
        Self {
            next_tick: initial_tick,
        }
    }
}

impl BleBridgeTickSource for MonotonicBleTickSource {
    fn advance_tick(&mut self) -> Tick {
        let tick = self.next_tick;
        // Saturating prevents wrapping so the clock stalls rather than going backwards.
        self.next_tick = Tick(self.next_tick.0.saturating_add(1));
        tick
    }
}

pub struct BleBridgeTransport {
    driver: BleTransportDriver,
    outbound: DispatchReceiver<BleOutboundCommand>,
    control: BleDriverControl,
    notifier: TransportIngressNotifier,
    runtime_task: tokio::task::JoinHandle<()>,
}

impl BleBridgeTransport {
    #[must_use]
    pub fn from_components(components: BleTransportComponents) -> Self {
        // _sender is intentionally dropped here: BleBridgeTransport drives outbound via the
        // outbound_rx drain path, not via BleTransportSender.
        let (driver, _sender, outbound, control, notifier, runtime_task) = components.into_parts();
        Self::from_parts(driver, outbound, control, notifier, runtime_task)
    }

    #[must_use]
    pub fn from_parts(
        driver: BleTransportDriver,
        outbound: DispatchReceiver<BleOutboundCommand>,
        control: BleDriverControl,
        notifier: TransportIngressNotifier,
        runtime_task: tokio::task::JoinHandle<()>,
    ) -> Self {
        Self {
            driver,
            outbound,
            control,
            notifier,
            runtime_task,
        }
    }

    #[must_use]
    pub fn runtime_task(&self) -> &tokio::task::JoinHandle<()> {
        &self.runtime_task
    }
}

impl BleBridgeIo for BleBridgeTransport {
    fn drain_raw_ingress(&mut self) -> Result<Vec<TransportIngressEvent>, TransportError> {
        self.driver.drain_transport_ingress()
    }

    fn flush_outbound(
        &mut self,
    ) -> Pin<Box<dyn Future<Output = Result<usize, TransportError>> + Send + '_>> {
        Box::pin(async move {
            // Drain everything queued by the router this round and send as a single batch.
            let batch = self.outbound.drain();
            let len = batch.len();
            if len > 0 {
                self.control.dispatch(batch).await?;
            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BleBridgeConfig {
    pub inbound_capacity: u32,
}

// Upper bound on queued inbound frames before back-pressure stalls the BLE runtime task.
const INBOUND_CAPACITY_MAX: u32 = 1024;

impl Default for BleBridgeConfig {
    fn default() -> Self {
        Self {
            inbound_capacity: INBOUND_CAPACITY_MAX,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleBridgeRoundReport {
    pub router_outcome: RouterRoundOutcome,
    pub ingested_transport_observations: Vec<TransportObservation>,
    pub flushed_transport_commands: u32,
    pub dropped_transport_observations: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleBridgeWaitState {
    pub next_round_hint: RoutingTickHint,
    pub pending_transport_observations: u32,
    pub pending_transport_commands: u32,
    pub dropped_transport_observations: u32,
    pub notifier_snapshot: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BleBridgeProgress {
    Advanced(Box<BleBridgeRoundReport>),
    Waiting(BleBridgeWaitState),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BleBridgeWaitDecision {
    AdvanceImmediately,
    WaitUntilTick(Tick),
    BlockOnNotifier { snapshot: u64 },
}

impl BleBridgeWaitDecision {
    pub fn wait(self, notifier: &TransportIngressNotifier) {
        // Only park the thread when the notifier confirms nothing new arrived since the snapshot was taken.
        if let Self::BlockOnNotifier { snapshot } = self
            && !notifier.has_changed_since(snapshot)
        {
            notifier.wait_for_change(snapshot);
        }
    }
}

pub struct BleHostBridge<Router, Transport = BleBridgeTransport, Clock = MonotonicBleTickSource> {
    router: Router,
    transport: Transport,
    clock: Clock,
    pending_transport_observations: VecDeque<TransportObservation>,
    inbound_capacity: usize,
    dropped_transport_observations: usize,
}

impl<Router, Transport> BleHostBridge<Router, Transport>
where
    Router: BleBridgeRouter,
    Transport: BleBridgeIo,
{
    #[must_use]
    pub fn new(
        router: Router,
        transport: Transport,
        initial_tick: Tick,
        config: BleBridgeConfig,
    ) -> Self {
        // recursion-exception: constructor delegates to the clock-injecting constructor with the same semantic name
        Self::with_clock(
            router,
            transport,
            MonotonicBleTickSource::new(initial_tick),
            config,
        )
    }
}

impl<Router, Transport, Clock> BleHostBridge<Router, Transport, Clock>
where
    Router: BleBridgeRouter,
    Transport: BleBridgeIo,
    Clock: BleBridgeTickSource,
{
    #[must_use]
    pub fn with_clock(
        router: Router,
        transport: Transport,
        clock: Clock,
        config: BleBridgeConfig,
    ) -> Self {
        Self {
            router,
            transport,
            clock,
            pending_transport_observations: VecDeque::new(),
            inbound_capacity: config.inbound_capacity as usize,
            dropped_transport_observations: 0,
        }
    }

    #[must_use]
    pub fn router(&self) -> &Router {
        &self.router
    }

    #[must_use]
    pub fn router_mut(&mut self) -> &mut Router {
        &mut self.router
    }

    #[must_use]
    pub fn transport(&self) -> &Transport {
        &self.transport
    }

    #[must_use]
    pub fn notifier(&self) -> &TransportIngressNotifier {
        // recursion-exception: same-name forwarding keeps notifier access on the bridge surface
        self.transport.notifier()
    }

    pub async fn advance_round(&mut self) -> Result<BleBridgeProgress, BleBridgeError> {
        // recursion-exception: same-name forwarding keeps round advancement on the bridge surface
        // Stamp all buffered ingress events with the current logical tick before handing them to the router.
        let observed_at_tick = self.clock.advance_tick();
        self.stage_transport_ingress(observed_at_tick)?;
        let ingested = self
            .pending_transport_observations
            .drain(..)
            .collect::<Vec<_>>();
        for observation in &ingested {
            self.router.ingest_transport_observation(observation)?;
        }

        let router_outcome = self.router.advance_round()?;
        let flushed_transport_commands = self.transport.flush_outbound().await?;
        let dropped_transport_observations =
            std::mem::take(&mut self.dropped_transport_observations);

        // If nothing changed this round, return Waiting so the caller can block instead of spinning.
        if ingested.is_empty()
            && flushed_transport_commands == 0
            && dropped_transport_observations == 0
            && router_outcome.engine_change == RoutingTickChange::NoChange
        {
            return Ok(BleBridgeProgress::Waiting(BleBridgeWaitState {
                next_round_hint: router_outcome.next_round_hint,
                pending_transport_observations: narrow_count(
                    self.pending_transport_observations.len(),
                ),
                pending_transport_commands: narrow_count(self.transport.pending_outbound()),
                dropped_transport_observations: narrow_count(dropped_transport_observations),
                // Snapshot taken here so BlockOnNotifier can detect races between this check and parking.
                notifier_snapshot: self.transport.notifier().snapshot(),
            }));
        }

        Ok(BleBridgeProgress::Advanced(Box::new(
            BleBridgeRoundReport {
                router_outcome,
                ingested_transport_observations: ingested,
                flushed_transport_commands: narrow_count(flushed_transport_commands),
                dropped_transport_observations: narrow_count(dropped_transport_observations),
            },
        )))
    }

    #[must_use]
    pub fn wait_decision(&self, wait_state: &BleBridgeWaitState) -> BleBridgeWaitDecision {
        // Pending work always takes priority over any hint from the router scheduler.
        if wait_state.pending_transport_observations > 0
            || wait_state.pending_transport_commands > 0
        {
            return BleBridgeWaitDecision::AdvanceImmediately;
        }

        match wait_state.next_round_hint {
            RoutingTickHint::Immediate => BleBridgeWaitDecision::AdvanceImmediately,
            // Router has time-sensitive work scheduled at a future tick so we use a timed sleep.
            RoutingTickHint::WithinTicks(tick) => BleBridgeWaitDecision::WaitUntilTick(tick),
            // No scheduled work: block the thread until the BLE layer signals new ingress.
            RoutingTickHint::HostDefault => BleBridgeWaitDecision::BlockOnNotifier {
                snapshot: wait_state.notifier_snapshot,
            },
        }
    }

    fn stage_transport_ingress(&mut self, observed_at_tick: Tick) -> Result<(), TransportError> {
        let raw_events = self.transport.drain_raw_ingress()?;
        if raw_events.is_empty() {
            return Ok(());
        }

        for event in raw_events {
            // Back-pressure: drop excess events rather than growing the queue unboundedly.
            if self.pending_transport_observations.len() >= self.inbound_capacity {
                self.dropped_transport_observations =
                    self.dropped_transport_observations.saturating_add(1);
                continue;
            }
            // Promote raw BLE event to a Jacquard observation by attaching the current tick.
            self.pending_transport_observations
                .push_back(event.observe_at(observed_at_tick));
        }
        Ok(())
    }
}

fn narrow_count(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}
