//! Top-level BLE client handle.
//!
//! [`JacquardBleClient`] wires together the BLE transport, Jacquard router
//! (Mercator), host bridge, and topology projector. It spawns a
//! dedicated single-threaded runtime and exposes `send`, `send_multicast`,
//! `incoming`, and `topology_stream` to the application.
//!
//! Router and bridge construction helpers (`build_router`, `initial_topology`,
//! `policy_inputs_for`, `default_profile`, `default_objective`) are kept
//! module-private. Use [`JacquardBleClient::new`] as the entry point.

use std::collections::BTreeMap;
use std::io;
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use jacquard_core::{
    Configuration, DeliveryCompatibilityPolicy, LinkEndpoint, MaterializedRoute, MulticastGroupId,
    NodeId, Observation, RouteAdmissionRejection, RouteDeliveryObjective, RouteError, RouteId,
    RouteSelectionError, RoutingTickHint, Tick, TransportDeliveryIntent, TransportDeliveryMode,
    TransportError, TransportObservation,
};
use jacquard_mercator::selected_neighbor_from_backend_route_id;
use jacquard_router::admitted_delivery_intent;
use jacquard_traits::{Router, RoutingDataPlane, TransportSenderEffects};
use jq_link_profile::{BleConfig, BleLinkError, BleTransportComponents};
use jq_node_profile::MeshTopology;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_stream::Stream;
use tokio_stream::wrappers::BroadcastStream;

use crate::framing::{decode_client_payload, encode_client_payload};
use crate::projector::TopologyProjector;
use crate::routing::{
    JacquardBleRouter, build_router, default_objective, initial_topology, policy_inputs_for,
};
use crate::{
    BleBridgeConfig, BleBridgeError, BleBridgeIo, BleBridgeProgress, BleBridgeRoundReport,
    BleBridgeTransport, BleHostBridge, BleRuntimeEffects,
};

const DEFAULT_CLIENT_COMMAND_CAPACITY: usize = 64;
const CLIENT_COMMAND_DRAIN_MAX: usize = DEFAULT_CLIENT_COMMAND_CAPACITY;
// Wall-clock duration of one logical Jacquard tick; translates router "within N ticks" hints into a real sleep.
const DEFAULT_CLIENT_TICK_DURATION_MS: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JacquardBleSendStage {
    QueuedForTransport,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JacquardBleSendReceipt {
    pub destination_node_id: NodeId,
    pub route_id: RouteId,
    pub next_hop_node_id: NodeId,
    pub delivery_mode: TransportDeliveryMode,
    pub endpoint: LinkEndpoint,
    pub stage: JacquardBleSendStage,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JacquardBleMulticastReceipt {
    pub group_id: MulticastGroupId,
    pub receivers: Vec<NodeId>,
    pub delivery_mode: TransportDeliveryMode,
    pub endpoint: LinkEndpoint,
    pub stage: JacquardBleSendStage,
}

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
    #[error(transparent)]
    DeliveryAdmission(#[from] RouteAdmissionRejection),
    #[error("failed to start jacquard ble client runtime")]
    RuntimeStart(#[from] io::Error),
    #[error("jacquard ble client runtime is no longer running")]
    RuntimeStopped,
    #[error("jacquard ble client runtime panicked")]
    RuntimePanicked,
}

enum ClientCommand {
    Send {
        to: NodeId,
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<JacquardBleSendReceipt, BleClientError>>,
    },
    SendMulticast {
        group_id: MulticastGroupId,
        receivers: Vec<NodeId>,
        payload: Vec<u8>,
        reply: oneshot::Sender<Result<JacquardBleMulticastReceipt, BleClientError>>,
    },
}

struct QueuedTransportCommand {
    next_hop_node_id: NodeId,
    intent: TransportDeliveryIntent,
}

struct ClientTask<Transport, Sender> {
    local_node_id: NodeId,
    // BleRuntimeEffects is the router's effect handler AND the bridge's clock so tick/order stamps stay consistent.
    bridge: BleHostBridge<JacquardBleRouter, Transport, BleRuntimeEffects>,
    transport_sender: Sender,
    commands: mpsc::Receiver<ClientCommand>,
    incoming: broadcast::Sender<(NodeId, Bytes)>,
    topology: Arc<Mutex<MeshTopology>>,
    topology_updates: broadcast::Sender<MeshTopology>,
    projector: TopologyProjector,
    route_cache: BTreeMap<NodeId, jacquard_core::RouteId>,
    tick_duration_ms: Duration,
    shutdown: watch::Receiver<bool>,
    startup: Option<std_mpsc::Sender<Result<(), BleClientError>>>,
}

impl<Transport, Sender> ClientTask<Transport, Sender>
where
    Transport: BleBridgeIo + Send + 'static,
    Sender: TransportSenderEffects + Send + 'static,
{
    async fn run(mut self) {
        if let Some(startup) = self.startup.take() {
            // allow-ignored-result: startup receiver may have gone away if construction was abandoned
            let _ = startup.send(Ok(()));
        }
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
        for _ in 0..CLIENT_COMMAND_DRAIN_MAX {
            match self.commands.try_recv() {
                Ok(command) => self.handle_command(command).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return true,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return false,
            }
        }
        true
    }

    async fn handle_command(&mut self, command: ClientCommand) {
        match command {
            ClientCommand::Send { to, payload, reply } => {
                let result = self.handle_send(to, payload).await;
                // Caller may have been dropped (fire-and-forget); a closed oneshot is not an error.
                // allow-ignored-result: reply delivery is best-effort when the caller dropped the oneshot receiver
                let _ = reply.send(result);
            }
            ClientCommand::SendMulticast {
                group_id,
                receivers,
                payload,
                reply,
            } => {
                let result = self
                    .handle_send_multicast(group_id, receivers, &payload)
                    .await;
                // Caller may have been dropped (fire-and-forget); a closed oneshot is not an error.
                // allow-ignored-result: reply delivery is best-effort when the caller dropped the oneshot receiver
                let _ = reply.send(result);
            }
        }
    }

    async fn handle_send(
        &mut self,
        to: NodeId,
        payload: Vec<u8>,
    ) -> Result<JacquardBleSendReceipt, BleClientError> {
        if let Some(result) = self.try_cached_forward(to, &payload).await {
            return result;
        }
        self.activate_and_forward(to, &payload).await
    }

    async fn handle_send_multicast(
        &mut self,
        group_id: MulticastGroupId,
        receivers: Vec<NodeId>,
        payload: &[u8],
    ) -> Result<JacquardBleMulticastReceipt, BleClientError> {
        let objective = RouteDeliveryObjective::MulticastGroup {
            group_id,
            receivers: receivers.clone(),
        };
        let support = jq_link_profile::gatt_notify_multicast_support(
            self.local_node_id,
            group_id,
            receivers.clone(),
            Tick(0),
            jacquard_core::OrderStamp(0),
        )
        .ok_or(RouteAdmissionRejection::DeliveryAssumptionUnsupported)?;
        let intent = admitted_delivery_intent(
            &objective,
            &support,
            DeliveryCompatibilityPolicy::ExactDeliveryOnly,
        )?;
        self.transport_sender
            .send_transport_to(&intent, payload)
            .map_err(BleClientError::Transport)?;
        self.drive_bridge_round().await?;
        Ok(multicast_receipt(group_id, receivers, intent))
    }

    // Fast path: reuse the cached route if it is still valid.
    // Returns Some(result) if the cache was hit (success or drive_bridge_round error),
    // or None if the route was missing or stale (caller should activate a new one).
    async fn try_cached_forward(
        &mut self,
        to: NodeId,
        payload: &[u8],
    ) -> Option<Result<JacquardBleSendReceipt, BleClientError>> {
        let route_id = self.route_cache.get(&to).copied()?;
        let route = self.bridge.router_mut().active_route(&route_id).cloned()?;
        if self
            .bridge
            .router_mut()
            .forward_payload(&route_id, payload)
            .is_ok()
        {
            let queued = match self.queue_payload_on_route(&route, payload) {
                Ok(queued) => queued,
                Err(error) => return Some(Err(error)),
            };
            return Some(
                self.drive_bridge_round()
                    .await
                    .map(|_| send_receipt(to, route_id, queued)),
            );
        }
        // Route is stale; evict it and signal that the slow path is needed.
        self.route_cache.remove(&to);
        None
    }

    async fn activate_and_forward(
        &mut self,
        to: NodeId,
        payload: &[u8],
    ) -> Result<JacquardBleSendReceipt, BleClientError> {
        // Slow path: materialize a new route then forward the payload in one step.
        let route = Router::activate_route(self.bridge.router_mut(), default_objective(to))?;
        let route_id = route.identity.stamp.route_id;
        let topology_changed = self.projector.ingest_materialized_route(&route);
        self.bridge
            .router_mut()
            .forward_payload(&route_id, payload)?;
        let queued = self.queue_payload_on_route(&route, payload)?;
        self.route_cache.insert(to, route_id);
        self.drive_bridge_round().await?;
        if topology_changed {
            self.publish_topology();
        }
        Ok(send_receipt(to, route_id, queued))
    }

    fn queue_payload_on_route(
        &mut self,
        route: &MaterializedRoute,
        payload: &[u8],
    ) -> Result<QueuedTransportCommand, BleClientError> {
        let next_hop_node_id = selected_neighbor_from_backend_route_id(
            &route.identity.admission.backend_ref.backend_route_id,
        )
        .ok_or_else(no_route_candidate)?;
        let endpoint = self
            .projector
            .snapshot()
            .edge(self.local_node_id, next_hop_node_id)
            .map(|edge| edge.observation.value.endpoint.clone())
            .ok_or_else(no_route_candidate)?;
        let intent = TransportDeliveryIntent::unicast(endpoint);
        self.transport_sender
            .send_transport_to(&intent, payload)
            .map_err(BleClientError::Transport)?;
        Ok(QueuedTransportCommand {
            next_hop_node_id,
            intent,
        })
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
                self.wait_for_tick_or_work(ticks, wait_state.notifier_snapshot)
                    .await
            }
            crate::BleBridgeWaitDecision::BlockOnNotifier { snapshot } => {
                self.wait_for_notifier_or_work(snapshot).await
            }
        }
    }

    async fn wait_for_tick_or_work(&mut self, ticks: Tick, notifier_snapshot: u64) -> bool {
        let sleep = tokio::time::sleep(duration_for_tick_hint(self.tick_duration_ms, ticks));
        let notifier = self.bridge.notifier().clone();
        if notifier.has_changed_since(notifier_snapshot) {
            return true;
        }
        tokio::pin!(sleep);
        tokio::select! {
            biased;
            changed = self.shutdown.changed() => changed.is_err() || !*self.shutdown.borrow(),
            command = self.commands.recv() => self.handle_wait_command(command).await,
            _ = notifier.changed(notifier_snapshot) => true,
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
    #[must_use = "constructing the BLE client has no effect unless the returned client is used"]
    pub async fn new(local_node_id: NodeId) -> Result<Self, BleClientError> {
        // recursion-exception: constructor delegates to transport assembly with the same semantic name
        let components = BleTransportComponents::new(local_node_id, BleConfig::default()).await?;
        let (driver, sender, outbound, control, notifier, runtime_task) = components.into_parts();
        let transport =
            BleBridgeTransport::from_parts(driver, outbound, control, notifier, runtime_task);
        Self::spawn_with_transport(
            local_node_id,
            initial_topology(local_node_id, Tick(1)),
            transport,
            sender,
            DEFAULT_CLIENT_TICK_DURATION_MS,
        )
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
        .expect("spawn test jq-client")
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
        .expect("spawn test jq-client")
    }

    pub async fn send(
        &self,
        to: NodeId,
        payload: &[u8],
    ) -> Result<JacquardBleSendReceipt, BleClientError> {
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

    /// Sends one application payload through explicit BLE notification fanout.
    ///
    /// This is multicast, not targeted unicast: the admitted receiver set is
    /// encoded in the Jacquard delivery intent and the BLE runtime only maps it
    /// to GATT notify when matching subscriber fanout state is known.
    pub async fn send_multicast(
        &self,
        group_id: MulticastGroupId,
        receivers: impl IntoIterator<Item = NodeId>,
        payload: &[u8],
    ) -> Result<JacquardBleMulticastReceipt, BleClientError> {
        let (reply_tx, reply_rx) = oneshot::channel();
        let payload = encode_client_payload(payload);
        self.commands
            .send(ClientCommand::SendMulticast {
                group_id,
                receivers: receivers.into_iter().collect(),
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

    pub fn shutdown(mut self) -> Result<(), BleClientError> {
        self.request_runtime_shutdown();
        self.join_runtime_thread()
    }

    fn spawn_with_transport<Transport, Sender>(
        local_node_id: NodeId,
        topology: Observation<Configuration>,
        transport: Transport,
        transport_sender: Sender,
        tick_duration_ms: Duration,
    ) -> Result<Self, BleClientError>
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
        let (startup_tx, startup_rx) = std_mpsc::channel();
        // Dedicated OS thread with a single-threaded tokio runtime isolates the client event loop
        // from the caller's runtime and avoids Send requirements on the LocalSet-pinned futures.
        let runtime_thread = std::thread::Builder::new()
            .name("jq-client-runtime".into())
            .spawn(move || {
                let effects = BleRuntimeEffects::new(topology.observed_at_tick);
                let router = match build_router(local_node_id, topology.clone(), effects.clone()) {
                    Ok(router) => router,
                    Err(error) => {
                        // allow-ignored-result: startup receiver may have gone away if construction was abandoned
                        let _ = startup_tx.send(Err(BleClientError::Route(error)));
                        return;
                    }
                };
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
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        // allow-ignored-result: startup receiver may have gone away if construction was abandoned
                        let _ = startup_tx.send(Err(BleClientError::RuntimeStart(error)));
                        return;
                    }
                };
                let bridge = BleHostBridge::with_clock(
                    local_node_id,
                    router,
                    transport,
                    effects,
                    BleBridgeConfig::default(),
                );
                let task = ClientTask {
                    local_node_id,
                    bridge,
                    transport_sender,
                    commands: commands_rx,
                    incoming: incoming_task,
                    topology: topology_task_state,
                    topology_updates: topology_updates_task,
                    projector,
                    route_cache: BTreeMap::new(),
                    tick_duration_ms,
                    shutdown: shutdown_rx,
                    startup: Some(startup_tx),
                };
                let local = tokio::task::LocalSet::new();
                local.block_on(&runtime, task.run());
            })?;
        startup_rx
            .recv()
            .map_err(|_| BleClientError::RuntimeStopped)??;

        Ok(Self {
            local_node_id,
            commands: commands_tx,
            incoming: incoming_tx,
            topology: topology_state,
            topology_updates,
            shutdown: shutdown_tx,
            runtime_thread: Some(runtime_thread),
        })
    }

    fn request_runtime_shutdown(&mut self) {
        // allow-ignored-result: shutdown notification is best-effort during teardown when the runtime already exited
        let _ = self.shutdown.send(true);
        let (replacement, _) = mpsc::channel(1);
        let commands = std::mem::replace(&mut self.commands, replacement);
        drop(commands);
    }

    fn join_runtime_thread(&mut self) -> Result<(), BleClientError> {
        if let Some(runtime_thread) = self.runtime_thread.take() {
            runtime_thread
                .join()
                .map_err(|_| BleClientError::RuntimePanicked)?;
        }
        Ok(())
    }
}

impl Drop for JacquardBleClient {
    fn drop(&mut self) {
        // recursion-exception: drop uses same-name shutdown steps on owned channels and thread handles
        // drop-side-effects-exception: dropping the client is the explicit teardown point for the runtime thread
        self.request_runtime_shutdown();
        // Dropping JoinHandle detaches the OS thread; explicit shutdown() is available for callers
        // that need deterministic teardown and panic reporting.
        let _detached_runtime = self.runtime_thread.take();
    }
}

fn duration_for_tick_hint(round_interval_ms: Duration, ticks: Tick) -> Duration {
    round_interval_ms
        .checked_mul(u32::try_from(ticks.0).unwrap_or(u32::MAX))
        .unwrap_or(Duration::MAX)
}

fn no_route_candidate() -> BleClientError {
    BleClientError::Route(RouteError::Selection(RouteSelectionError::NoCandidate))
}

fn send_receipt(
    destination_node_id: NodeId,
    route_id: RouteId,
    queued: QueuedTransportCommand,
) -> JacquardBleSendReceipt {
    JacquardBleSendReceipt {
        destination_node_id,
        route_id,
        next_hop_node_id: queued.next_hop_node_id,
        delivery_mode: queued.intent.mode(),
        endpoint: queued.intent.endpoint().clone(),
        stage: JacquardBleSendStage::QueuedForTransport,
    }
}

fn multicast_receipt(
    group_id: MulticastGroupId,
    receivers: Vec<NodeId>,
    intent: TransportDeliveryIntent,
) -> JacquardBleMulticastReceipt {
    JacquardBleMulticastReceipt {
        group_id,
        receivers,
        delivery_mode: intent.mode(),
        endpoint: intent.endpoint().clone(),
        stage: JacquardBleSendStage::QueuedForTransport,
    }
}
