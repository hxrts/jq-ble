//! BLE runtime task. Single async owner of all `blew` runtime state.
//!
//! [`BleRuntimeTask`] owns `Central`, `Peripheral`, all event streams, L2CAP
//! channels, discovery hints, resolved peers, and session state. Drives
//! handshake progression internally and emits tick-free ingress events into
//! the [`TransportIngressMailbox`] for the host bridge to stamp and ingest.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use blew::central::backend::CentralBackend;
use blew::central::{Central, CentralEvent, ScanFilter, WriteType};
use blew::l2cap::{L2capChannel, types::Psm};
use blew::peripheral::backend::PeripheralBackend;
use blew::peripheral::{Peripheral, PeripheralEvent};
use blew::types::DeviceId;
use jacquard_adapter::{
    PeerDirectory, PendingClaims, TransportIngressClass, TransportIngressSender, dispatch_mailbox,
    transport_ingress_mailbox,
};
use jacquard_core::{
    DurationMs, LinkBuilder, LinkEndpoint, LinkRuntimeState, NodeId, OriginAuthenticationClass,
    PartitionRecoveryClass, RepairCapability, RoutingEvidenceClass, TransportIngressEvent,
};
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use blew::util::event_stream::EventStream;

use crate::gatt::{
    JACQUARD_C2P_CHAR_UUID, JACQUARD_NODE_ID_CHAR_UUID, JACQUARD_P2C_CHAR_UUID,
    JACQUARD_PSM_CHAR_UUID, gatt_endpoint, gatt_fallback_service, gatt_l2cap_service,
    hint_matches_node_id, l2cap_endpoint, parse_discovery_hint,
};
use crate::l2cap::{
    ActiveL2capChannel, L2capAcceptStream, L2capRuntimeEvent, read_l2cap_identity,
    spawn_l2cap_channel_tasks, write_l2cap_identity,
};
use crate::session::{
    device_id_from_endpoint, endpoint_for_session, first_gatt_fallback, resolve_remote_node_id,
    resolve_remote_psm, session_device_id, session_references_device,
};
use crate::transport::{
    BleChannelId, BleConfig, BleDriverCommand, BleDriverControl, BleLinkError, BleOutboundCommand,
    BlePeerKey, BleRuntimeParts, BleSession, BleTransportComponents, BleTransportDriver,
    BleTransportSender, DiscoveredPeerHint, PeerSessions,
};

// Short sleep between deferred control ingress retries; keeps the select loop responsive without busy-looping.
const DEFERRED_CONTROL_RETRY_INTERVAL: Duration = Duration::from_millis(5);

type CentralEvents<CB> = EventStream<CentralEvent, <CB as CentralBackend>::EventStream>;
type PeripheralEvents<PB> = EventStream<PeripheralEvent, <PB as PeripheralBackend>::EventStream>;

pub struct BleRuntimeTask<CB: CentralBackend, PB: PeripheralBackend> {
    central: Central<CB>,
    peripheral: Peripheral<PB>,
    central_events: CentralEvents<CB>,
    peripheral_events: PeripheralEvents<PB>,
    l2cap_accept_stream: Option<L2capAcceptStream>,
    l2cap_events_tx: mpsc::Sender<L2capRuntimeEvent>,
    l2cap_events_rx: mpsc::Receiver<L2capRuntimeEvent>,
    l2cap_channels: BTreeMap<BleChannelId, ActiveL2capChannel>,
    next_channel_id: u64,
    peers: PeerDirectory<BlePeerKey, DiscoveredPeerHint>,
    sessions: BTreeMap<NodeId, PeerSessions>,
    pending_claims: PendingClaims<BlePeerKey>,
    ingress_tx: TransportIngressSender,
    command_rx: mpsc::Receiver<BleDriverCommand>,
    local_node_id: NodeId,
    local_psm: Option<Psm>,
    _config: BleConfig,
    deferred_control_ingress: VecDeque<TransportIngressEvent>,
}

impl<CB, PB> BleRuntimeTask<CB, PB>
where
    CB: CentralBackend + Send + 'static,
    PB: PeripheralBackend + Send + 'static,
{
    pub fn new(
        local_node_id: NodeId,
        central: Central<CB>,
        peripheral: Peripheral<PB>,
        config: BleConfig,
    ) -> Result<(Self, BleRuntimeParts), BleLinkError> {
        if config.ingress_capacity == 0 {
            return Err(BleLinkError::ZeroIngressCapacity);
        }
        if config.command_capacity == 0 {
            return Err(BleLinkError::ZeroCommandCapacity);
        }

        let central_events = central.events();
        let peripheral_events = peripheral.events();
        let (ingress_tx, ingress_rx, notifier) = transport_ingress_mailbox(config.ingress_capacity);
        let (command_tx, command_rx) = mpsc::channel(config.command_capacity);
        let (l2cap_events_tx, l2cap_events_rx) = mpsc::channel(config.command_capacity);
        let (outbound_tx, outbound_rx) = dispatch_mailbox(config.command_capacity);
        let control = BleDriverControl::new(command_tx);
        let driver = BleTransportDriver::new(ingress_rx, control.clone());
        let sender = BleTransportSender::new(outbound_tx);

        let task = Self {
            central,
            peripheral,
            central_events,
            peripheral_events,
            l2cap_accept_stream: None,
            l2cap_events_tx,
            l2cap_events_rx,
            l2cap_channels: BTreeMap::new(),
            next_channel_id: 1,
            peers: PeerDirectory::new(),
            sessions: BTreeMap::new(),
            pending_claims: PendingClaims::new(),
            ingress_tx,
            command_rx,
            local_node_id,
            local_psm: None,
            _config: config,
            deferred_control_ingress: VecDeque::new(),
        };

        Ok((task, (driver, sender, outbound_rx, control, notifier)))
    }

    pub fn spawn(
        local_node_id: NodeId,
        central: Central<CB>,
        peripheral: Peripheral<PB>,
        config: BleConfig,
    ) -> Result<BleTransportComponents, BleLinkError> {
        let (task, (driver, sender, outbound, control, notifier)) =
            Self::new(local_node_id, central, peripheral, config)?;
        let runtime_task = tokio::spawn(task.run());
        Ok(BleTransportComponents {
            driver,
            sender,
            outbound,
            control,
            notifier,
            runtime_task,
        })
    }

    #[doc(hidden)]
    pub fn testing_seed_resolved_peer(&mut self, device_id: DeviceId, node_id: NodeId) {
        self.peers.resolve(BlePeerKey::from(&device_id), node_id);
    }

    #[doc(hidden)]
    pub fn testing_seed_sessions(&mut self, node_id: NodeId, sessions: PeerSessions) {
        self.sessions.insert(node_id, sessions);
    }

    #[doc(hidden)]
    pub fn testing_identity_resolved_for_device(&self, device_id: &DeviceId) -> bool {
        self.resolved_node_id_for_device(device_id).is_some()
    }

    #[doc(hidden)]
    pub fn testing_session_count(&self) -> usize {
        self.sessions.len()
    }

    #[doc(hidden)]
    pub fn testing_has_egress_session_for_device(&self, device_id: &DeviceId) -> bool {
        self.egress_session_for_device(device_id).is_some()
    }

    #[doc(hidden)]
    pub async fn testing_handle_central_event(&mut self, event: CentralEvent) {
        self.handle_central_event(event).await;
    }

    #[doc(hidden)]
    pub async fn testing_run(self) {
        self.run().await;
    }

    async fn run(mut self) {
        // Attempt L2CAP listener setup first so the PSM can be embedded in the advertised GATT service.
        if let Ok((psm, stream)) = self.peripheral.l2cap_listener().await {
            self.local_psm = Some(psm);
            self.l2cap_accept_stream = Some(Box::pin(stream));
        }

        // Advertise either the full L2CAP service or the GATT-only fallback depending on platform support.
        let service = self.local_psm.map_or_else(
            || gatt_fallback_service(&self.local_node_id),
            |psm| gatt_l2cap_service(&self.local_node_id, psm),
        );
        // Hardware errors here are non-fatal; the event loop continues and peers can still connect.
        let _ = self.peripheral.add_service(&service).await;
        // Scan errors are non-fatal; discovery resumes on the next topology refresh.
        let _ = self.central.start_scan(ScanFilter::default()).await;

        loop {
            self.flush_deferred_control_ingress();
            // When deferred control events are queued, wake again after 5ms instead of blocking forever.
            let retry_deferred_control = async {
                if self.deferred_control_ingress.is_empty() {
                    std::future::pending::<()>().await;
                } else {
                    tokio::time::sleep(DEFERRED_CONTROL_RETRY_INTERVAL).await;
                }
            };
            tokio::select! {
                Some(command) = self.command_rx.recv() => {
                    if self.handle_command(command).await {
                        break;
                    }
                }
                Some(event) = self.central_events.next() => {
                    self.handle_central_event(event).await;
                }
                Some(event) = self.peripheral_events.next() => {
                    self.handle_peripheral_event(event);
                }
                // Use pending() when L2CAP is not supported so the select arm stays dormant without an error.
                accept_result = async {
                    match self.l2cap_accept_stream.as_mut() {
                        Some(stream) => stream.next().await,
                        None => std::future::pending::<Option<blew::BlewResult<L2capChannel>>>().await,
                    }
                } => {
                    if let Some(accept_result) = accept_result {
                        self.handle_l2cap_accept(accept_result).await;
                    }
                }
                Some(l2cap_event) = self.l2cap_events_rx.recv() => {
                    self.handle_l2cap_runtime_event(l2cap_event);
                }
                _ = retry_deferred_control => {}
                else => break,
            }
        }
    }

    async fn handle_command(&mut self, command: BleDriverCommand) -> bool {
        match command {
            BleDriverCommand::Dispatch(batch) => {
                self.dispatch_outbound_batch(batch).await;
                false
            }
            BleDriverCommand::Shutdown => true,
        }
    }

    async fn handle_central_event(&mut self, event: CentralEvent) {
        match event {
            CentralEvent::AdapterStateChanged { .. } => {}
            CentralEvent::DeviceDiscovered(device) => {
                self.handle_device_discovered(device).await;
            }
            CentralEvent::DeviceConnected { device_id } => {
                self.handle_device_connected(device_id).await;
            }
            CentralEvent::DeviceDisconnected { device_id } => {
                let key = BlePeerKey::from(&device_id);
                let removed = self.peers.remove(&key);
                if let Some(jacquard_adapter::PeerIdentityState::Resolved(node_id)) = removed {
                    self.drop_l2cap_channels_for_node(node_id);
                }
                self.sessions
                    .retain(|_, sessions| !session_references_device(sessions, &device_id));
            }
            CentralEvent::CharacteristicNotification {
                device_id,
                char_uuid,
                value,
            } => self.handle_characteristic_notification(device_id, char_uuid, value.as_ref()),
        }
    }

    async fn handle_device_discovered(&mut self, device: blew::types::BleDevice) {
        // Skip devices that don't carry a Jacquard discovery UUID; they are not mesh peers.
        let Some(prefix) = parse_discovery_hint(&device.services) else {
            return;
        };

        let key = BlePeerKey::from(&device.id);
        let hint = DiscoveredPeerHint {
            device_id: device.id.clone(),
            node_id_prefix: prefix,
            advertised_gatt: true,
            advertised_l2cap: false,
        };
        self.peers.upsert_hint(key.clone(), hint);

        // Claim prevents duplicate concurrent connections to the same device.
        if self.pending_claims.contains(&key) {
            return;
        }

        let Ok(_claim_guard) = self.pending_claims.try_claim(key) else {
            return;
        };

        // Connection outcome arrives asynchronously via CentralEvent::DeviceConnected/Disconnected.
        let _ = self.central.connect(&device.id).await;
    }

    async fn handle_device_connected(&mut self, device_id: DeviceId) {
        let Ok(node_id) = resolve_remote_node_id(&self.central, &device_id).await else {
            return;
        };

        let key = BlePeerKey::from(&device_id);
        let Some(identity_state) = self.peers.identity_state(&key).cloned() else {
            return;
        };

        // Only proceed if we have a prior hint for this device; avoids accepting unsolicited connections.
        let jacquard_adapter::PeerIdentityState::Hint(hint) = identity_state else {
            return;
        };

        // Cross-check the hint prefix against the full NodeId to detect address-reuse spoofing.
        if !hint_matches_node_id(hint.node_id_prefix, &node_id) {
            // Best-effort cleanup; this peer is rejected regardless of whether disconnect succeeds.
            let _ = self.central.disconnect(&device_id).await;
            let _ = self.peers.remove(&key);
            return;
        }

        self.peers.resolve(key, node_id);
        // Install GATT session immediately so the peer is reachable before we attempt the L2CAP upgrade.
        self.install_gatt_session(node_id, device_id.clone()).await;
        self.emit_link_observed(node_id, gatt_endpoint(&device_id));

        // Try to upgrade to L2CAP CoC; proceed with GATT-only if any step fails.
        if let Some(psm) = resolve_remote_psm(&self.central, &device_id).await
            && let Ok(mut channel) = self.central.open_l2cap_channel(&device_id, psm).await
            && write_l2cap_identity(&mut channel, &self.local_node_id)
                .await
                .is_ok()
        {
            self.install_l2cap_session(node_id, device_id, channel)
                .await;
        }
    }

    fn handle_peripheral_event(&mut self, event: PeripheralEvent) {
        match event {
            PeripheralEvent::ReadRequest {
                char_uuid,
                responder,
                ..
            } if char_uuid == JACQUARD_NODE_ID_CHAR_UUID => {
                responder.respond(self.local_node_id.0.to_vec());
            }
            PeripheralEvent::ReadRequest {
                char_uuid,
                responder,
                ..
            } if char_uuid == JACQUARD_PSM_CHAR_UUID => {
                if let Some(psm) = self.local_psm {
                    responder.respond(psm.value().to_le_bytes().to_vec());
                } else {
                    responder.error();
                }
            }
            PeripheralEvent::ReadRequest { responder, .. } => {
                responder.error();
            }
            PeripheralEvent::WriteRequest {
                client_id,
                char_uuid,
                value,
                responder,
                ..
            } if char_uuid == JACQUARD_C2P_CHAR_UUID => {
                self.handle_c2p_write(client_id, value, responder);
            }
            PeripheralEvent::WriteRequest { responder, .. } => {
                if let Some(responder) = responder {
                    responder.error();
                }
            }
            PeripheralEvent::SubscriptionChanged {
                client_id,
                char_uuid,
                subscribed,
            } if char_uuid == JACQUARD_P2C_CHAR_UUID && subscribed => {
                self.handle_p2c_subscription(client_id);
            }
            PeripheralEvent::SubscriptionChanged { .. }
            | PeripheralEvent::AdapterStateChanged { .. } => {}
        }
    }

    fn handle_c2p_write(
        &mut self,
        client_id: DeviceId,
        value: Vec<u8>,
        responder: Option<blew::peripheral::WriteResponder>,
    ) {
        if let Some(node_id) = self.resolved_node_id_for_device(&client_id) {
            // Payload class drops on a full mailbox; loss is acceptable under back-pressure.
            let _ = self.ingress_tx.emit(
                TransportIngressClass::Payload,
                TransportIngressEvent::PayloadReceived {
                    from_node_id: node_id,
                    endpoint: gatt_endpoint(&client_id),
                    payload: value,
                },
            );
            // Respond success to satisfy Write Request semantics on platforms that require it.
            if let Some(responder) = responder {
                responder.success();
            }
        } else if let Some(responder) = responder {
            // Reject writes from unrecognized devices that haven't completed identity exchange.
            responder.error();
        }
    }

    fn handle_p2c_subscription(&mut self, client_id: DeviceId) {
        if let Some(node_id) = self.resolved_node_id_for_device(&client_id) {
            let session = BleSession::GattPeripheralSubscribed {
                device_id: client_id.clone(),
            };
            self.sessions
                .entry(node_id)
                .and_modify(|sessions| {
                    // Don't demote an active L2CAP egress session; store GATT as the fallback instead.
                    if !matches!(sessions.preferred_egress, Some(BleSession::L2cap { .. })) {
                        sessions.preferred_ingress = Some(session.clone());
                        sessions.preferred_egress = Some(session.clone());
                    } else {
                        sessions.fallback_egress = Some(session.clone());
                    }
                })
                .or_insert_with(|| PeerSessions {
                    preferred_ingress: Some(session.clone()),
                    preferred_egress: Some(session),
                    fallback_egress: None,
                });
        }
    }

    async fn handle_l2cap_accept(&mut self, accept_result: blew::BlewResult<L2capChannel>) {
        let Ok(mut channel) = accept_result else {
            return;
        };

        let Ok(remote_node_id) = read_l2cap_identity(&mut channel).await else {
            return;
        };
        // The identity exchange in write_l2cap_identity (central side) is necessary because the
        // peripheral accept API does not provide the remote DeviceId.
        let Some(device_id) = self.device_id_for_resolved_node(&remote_node_id) else {
            return;
        };

        self.install_l2cap_session(remote_node_id, device_id, channel)
            .await;
    }

    fn handle_l2cap_runtime_event(&mut self, event: L2capRuntimeEvent) {
        match event {
            L2capRuntimeEvent::FrameReceived {
                channel_id,
                payload,
            } => {
                let Some(channel) = self.l2cap_channels.get(&channel_id) else {
                    return;
                };

                // Payload class drops on a full mailbox; loss is acceptable under back-pressure.
                let _ = self.ingress_tx.emit(
                    TransportIngressClass::Payload,
                    TransportIngressEvent::PayloadReceived {
                        from_node_id: channel.node_id,
                        endpoint: l2cap_endpoint(&channel.device_id),
                        payload,
                    },
                );
            }
            L2capRuntimeEvent::ChannelClosed { channel_id } => {
                self.handle_l2cap_channel_closed(channel_id);
            }
        }
    }

    async fn dispatch_outbound_batch(&mut self, batch: Vec<BleOutboundCommand>) {
        for command in batch {
            self.dispatch_outbound_command(command).await;
        }
    }

    async fn dispatch_outbound_command(&mut self, command: BleOutboundCommand) {
        let Some(device_id) = device_id_from_endpoint(&command.endpoint) else {
            return;
        };

        // Loop at most twice: once via the preferred session, once via GATT fallback after an L2CAP downgrade.
        let mut downgraded_l2cap = None;
        loop {
            let Some(session) = self.egress_session_for_device(&device_id) else {
                return;
            };
            match self.send_via_session(session, &command.payload).await {
                None => return,
                Some(channel_id) => {
                    // Guard against an infinite loop if we already downgraded this channel once.
                    if downgraded_l2cap == Some(channel_id) {
                        return;
                    }
                    // L2CAP channel is broken; close it and retry with the GATT fallback.
                    downgraded_l2cap = Some(channel_id);
                    self.handle_l2cap_channel_closed(channel_id);
                }
            }
        }
    }

    /// Attempts to send `payload` via `session`. Returns `Some(channel_id)` if the L2CAP channel
    /// failed and should be closed; `None` indicates the send was dispatched (fire-and-forget).
    async fn send_via_session(
        &mut self,
        session: BleSession,
        payload: &[u8],
    ) -> Option<BleChannelId> {
        match session {
            BleSession::GattCentral { device_id } => {
                // Fire-and-forget; BLE write errors surface via link health, not per-send failures.
                let _ = self
                    .central
                    .write_characteristic(
                        &device_id,
                        JACQUARD_C2P_CHAR_UUID,
                        payload.to_vec(),
                        WriteType::WithoutResponse,
                    )
                    .await;
                None
            }
            BleSession::GattPeripheralSubscribed { .. } => {
                // Fire-and-forget; notify errors are transient and handled by the reliable layer.
                let _ = self
                    .peripheral
                    .notify_characteristic(JACQUARD_P2C_CHAR_UUID, payload.to_vec())
                    .await;
                None
            }
            BleSession::L2cap { channel_id, .. } => {
                let send_failed = match self.l2cap_channels.get(&channel_id) {
                    Some(channel) => channel.outbound_tx.send(payload.to_vec()).await.is_err(),
                    None => true,
                };
                if send_failed { Some(channel_id) } else { None }
            }
        }
    }

    fn handle_characteristic_notification(
        &mut self,
        device_id: DeviceId,
        char_uuid: uuid::Uuid,
        value: &[u8],
    ) {
        if char_uuid != JACQUARD_P2C_CHAR_UUID {
            return;
        }

        let Some(node_id) = self.resolved_node_id_for_device(&device_id) else {
            return;
        };

        // Payload class drops on a full mailbox; loss is acceptable under back-pressure.
        let _ = self.ingress_tx.emit(
            TransportIngressClass::Payload,
            TransportIngressEvent::PayloadReceived {
                from_node_id: node_id,
                endpoint: gatt_endpoint(&device_id),
                payload: value.to_vec(),
            },
        );
    }

    fn resolved_node_id_for_device(&self, device_id: &DeviceId) -> Option<NodeId> {
        match self
            .peers
            .identity_state(&crate::transport::BlePeerKey::from(device_id))
        {
            Some(jacquard_adapter::PeerIdentityState::Resolved(node_id)) => Some(*node_id),
            _ => None,
        }
    }

    fn device_id_for_resolved_node(&self, node_id: &NodeId) -> Option<DeviceId> {
        self.peers
            .address_for_resolved(node_id)
            .map(|key| DeviceId::from(key.0.clone()))
    }

    fn egress_session_for_device(&self, device_id: &DeviceId) -> Option<BleSession> {
        self.sessions.values().find_map(|sessions| {
            sessions
                .preferred_egress
                .as_ref()
                .or(sessions.fallback_egress.as_ref())
                .filter(|session| session_device_id(session) == device_id)
                .cloned()
        })
    }

    async fn install_gatt_session(&mut self, node_id: NodeId, device_id: DeviceId) {
        // Subscribe to P2C notifications so the peripheral can push payloads to us as central.
        // Subscription failure is recoverable; the session is still usable for central writes.
        let _ = self
            .central
            .subscribe_characteristic(&device_id, JACQUARD_P2C_CHAR_UUID)
            .await;

        let session = BleSession::GattCentral {
            device_id: device_id.clone(),
        };
        self.sessions
            .entry(node_id)
            .and_modify(|sessions| {
                // Preserve an existing L2CAP egress session; only update the GATT fallback slot.
                if !matches!(sessions.preferred_egress, Some(BleSession::L2cap { .. })) {
                    sessions.preferred_ingress = Some(session.clone());
                    sessions.preferred_egress = Some(session.clone());
                }
                if sessions.fallback_egress.is_none() {
                    sessions.fallback_egress = Some(session.clone());
                }
            })
            .or_insert_with(|| PeerSessions {
                preferred_ingress: Some(session.clone()),
                preferred_egress: Some(session.clone()),
                fallback_egress: Some(session),
            });
    }

    async fn install_l2cap_session(
        &mut self,
        node_id: NodeId,
        device_id: DeviceId,
        channel: L2capChannel,
    ) {
        let channel_id = self.register_l2cap_channel(node_id, device_id.clone(), channel);
        let l2cap_session = BleSession::L2cap {
            device_id: device_id.clone(),
            channel_id,
        };
        self.sessions
            .entry(node_id)
            .and_modify(|sessions| {
                // Capture the existing GATT session as fallback before promoting L2CAP to preferred.
                if sessions.fallback_egress.is_none() {
                    sessions.fallback_egress = first_gatt_fallback(sessions);
                }
                sessions.preferred_ingress = Some(l2cap_session.clone());
                sessions.preferred_egress = Some(l2cap_session.clone());
            })
            .or_insert_with(|| PeerSessions {
                preferred_ingress: Some(l2cap_session.clone()),
                preferred_egress: Some(l2cap_session.clone()),
                fallback_egress: None,
            });
        // Emit a link observation so the router updates its endpoint record to L2CAP.
        self.emit_link_observed(node_id, l2cap_endpoint(&device_id));
    }

    /// Allocates a channel ID, spawns I/O tasks, and inserts the channel into the active map.
    fn register_l2cap_channel(
        &mut self,
        node_id: NodeId,
        device_id: DeviceId,
        channel: L2capChannel,
    ) -> BleChannelId {
        let channel_id = self.next_channel_id();
        let outbound_tx =
            spawn_l2cap_channel_tasks(channel_id, channel, self.l2cap_events_tx.clone());
        self.l2cap_channels.insert(
            channel_id,
            ActiveL2capChannel {
                node_id,
                device_id,
                outbound_tx,
            },
        );
        channel_id
    }

    fn handle_l2cap_channel_closed(&mut self, channel_id: BleChannelId) {
        let Some(channel) = self.l2cap_channels.remove(&channel_id) else {
            return;
        };
        let Some(sessions) = self.sessions.get_mut(&channel.node_id) else {
            return;
        };

        // Demote preferred sessions back to the GATT fallback when they referenced this channel.
        let fallback = sessions.fallback_egress.clone();
        if matches!(
            sessions.preferred_ingress,
            Some(BleSession::L2cap {
                channel_id: current,
                ..
            }) if current == channel_id
        ) {
            sessions.preferred_ingress = fallback.clone();
        }
        if matches!(
            sessions.preferred_egress,
            Some(BleSession::L2cap {
                channel_id: current,
                ..
            }) if current == channel_id
        ) {
            sessions.preferred_egress = fallback.clone();
        }

        // Notify the router that the effective endpoint has changed so it updates its link record.
        if let Some(fallback) = sessions.preferred_egress.clone() {
            self.emit_link_observed(channel.node_id, endpoint_for_session(&fallback));
        }
    }

    fn drop_l2cap_channels_for_node(&mut self, node_id: NodeId) {
        self.l2cap_channels
            .retain(|_, channel| channel.node_id != node_id);
    }

    fn next_channel_id(&mut self) -> BleChannelId {
        let channel_id = BleChannelId(self.next_channel_id);
        self.next_channel_id = self.next_channel_id.saturating_add(1);
        channel_id
    }

    fn flush_deferred_control_ingress(&mut self) {
        while let Some(event) = self.deferred_control_ingress.pop_front() {
            if self
                .ingress_tx
                .emit(TransportIngressClass::Control, event.clone())
                .is_err()
            {
                // Mailbox is full; put the event back and retry on the next loop iteration.
                self.deferred_control_ingress.push_front(event);
                break;
            }
        }
    }

    fn emit_control_event(&mut self, event: TransportIngressEvent) {
        if self
            .ingress_tx
            .emit(TransportIngressClass::Control, event.clone())
            .is_err()
        {
            // Defer rather than drop so link observations are not lost under back-pressure.
            self.deferred_control_ingress.push_back(event);
        }
    }

    fn emit_link_observed(&mut self, remote_node_id: NodeId, endpoint: LinkEndpoint) {
        let link = LinkBuilder::new(endpoint)
            .with_profile(
                // 150ms RTT estimate reflects typical BLE connection interval plus stack latency.
                DurationMs(150),
                RepairCapability::TransportRetransmit,
                PartitionRecoveryClass::LocalReconnect,
            )
            .with_runtime_state(LinkRuntimeState::Active)
            .build();

        self.emit_control_event(TransportIngressEvent::LinkObserved {
            remote_node_id,
            link,
            source_class: jacquard_core::FactSourceClass::Local,
            evidence_class: RoutingEvidenceClass::DirectObservation,
            origin_authentication: OriginAuthenticationClass::Unauthenticated,
        });
    }
}
