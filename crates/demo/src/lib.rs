//! Generic host-side integration story for Jacquard over BLE.
//!
//! This crate demonstrates how a host environment can wrap [`jq_client`] and
//! expose a higher-level service/state surface without depending on any
//! particular UI framework. Event publication is modeled as a generic sink so
//! embedders can integrate with a desktop app, CLI, mobile shell, or another host.

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{Stream, StreamExt, stream};
use jacquard_core::{NodeId, RouteError, RouteSelectionError, TransportError};
use jq_client::{BleBridgeError, BleClientError, BleConfig, JacquardBleClient};
use jq_node_profile::MeshTopology;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{Mutex, Semaphore, broadcast};

pub const JACQUARD_BLE_INVITE_SCHEME: &str = "jq-ble";
// Magic prefix distinguishes application frames from internal Jacquard routing traffic.
const APPLICATION_FRAME_MAGIC: &[u8; 8] = b"JQBLEAPP";
const INVITE_ACTION_PATH: &str = "/add-peer/";
// Jacquard routes converge asynchronously; retry briefly while the mesh catches up.
const RELAY_RETRY_INTERVAL_MS: Duration = Duration::from_millis(25);
const RELAY_RETRY_ATTEMPTS_MAX: usize = 200;
const RELAY_INFLIGHT_MAX: usize = 64;
// Capacity of the broadcast channel that fans incoming messages out to multiple subscribers.
const INCOMING_BROADCAST_CAPACITY: usize = 64;

#[derive(Clone, Copy)]
struct RelayRetryPolicy {
    interval_ms: Duration,
    max_attempts: u32,
}

impl Default for RelayRetryPolicy {
    fn default() -> Self {
        Self {
            interval_ms: RELAY_RETRY_INTERVAL_MS,
            max_attempts: RELAY_RETRY_ATTEMPTS_MAX as u32,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JacquardIncomingMessage {
    pub from_node_id: NodeId,
    pub transport_from_node_id: NodeId,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JacquardBleEvent {
    Started { local_node_id: NodeId },
    Incoming { message: JacquardIncomingMessage },
    PeerIntroduced { peer: JacquardIntroducedPeer },
    RelayDropped { relay: JacquardRelayDrop },
    Topology { snapshot: MeshTopology },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JacquardIntroducedPeer {
    pub node_id: NodeId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JacquardRelayDrop {
    pub from_node_id: NodeId,
    pub destination_node_id: NodeId,
}

#[derive(Debug, Error)]
pub enum JacquardPeerInviteError {
    #[error("empty peer invite")]
    Empty,
    #[error("invalid peer invite scheme: expected {expected:?}, got {found:?}")]
    InvalidScheme {
        expected: &'static str,
        found: String,
    },
    #[error("invalid peer invite format")]
    InvalidFormat,
    #[error("invalid node id hex length: expected 64 characters, got {found}")]
    InvalidNodeIdLength { found: u64 },
    #[error("invalid node id hex")]
    InvalidNodeIdHex(#[from] hex::FromHexError),
}

#[derive(Debug, Error)]
pub enum JacquardBleServiceError {
    #[error(transparent)]
    Client(#[from] BleClientError),
    #[error(transparent)]
    Invite(#[from] JacquardPeerInviteError),
    #[error("jacquard client not started")]
    NotStarted,
    #[error("jacquard client already started for {local_node_id:?}")]
    AlreadyStarted { local_node_id: NodeId },
}

struct JacquardBleServiceInner {
    client: Arc<JacquardBleClient>,
    incoming: broadcast::Sender<JacquardIncomingMessage>,
    introduced_peers: Mutex<BTreeSet<NodeId>>,
    introduced_peer_updates: broadcast::Sender<JacquardIntroducedPeer>,
    relay_drops: broadcast::Sender<JacquardRelayDrop>,
    pump_task: tokio::task::JoinHandle<()>,
}

impl Drop for JacquardBleServiceInner {
    fn drop(&mut self) {
        // drop-side-effects-exception: aborting the background pump is the explicit shutdown path for this owner handle
        self.pump_task.abort();
    }
}

#[derive(Clone)]
pub struct JacquardBleService {
    inner: Arc<JacquardBleServiceInner>,
}

pub trait HostEventSink: Send + Sync + 'static {
    fn publish(&self, event: &JacquardBleEvent);
}

impl<F> HostEventSink for F
where
    F: Fn(&JacquardBleEvent) + Send + Sync + 'static,
{
    fn publish(&self, event: &JacquardBleEvent) {
        self(event);
    }
}

impl JacquardBleService {
    #[must_use = "constructing the BLE service has no effect unless the returned service is used"]
    pub async fn new(local_node_id: NodeId) -> Result<Self, JacquardBleServiceError> {
        // recursion-exception: constructor delegates to the shared-client assembly path with the same semantic name
        Self::new_with_ble_config(local_node_id, BleConfig::default()).await
    }

    #[must_use = "constructing the BLE service has no effect unless the returned service is used"]
    pub async fn new_with_ble_config(
        local_node_id: NodeId,
        config: BleConfig,
    ) -> Result<Self, JacquardBleServiceError> {
        Ok(Self::from_shared_client(Arc::new(
            JacquardBleClient::new_with_config(local_node_id, config).await?,
        )))
    }

    #[doc(hidden)]
    #[must_use]
    pub fn from_client_for_testing(client: JacquardBleClient) -> Self {
        Self::from_shared_client(Arc::new(client))
    }

    #[doc(hidden)]
    #[must_use]
    pub fn from_client_with_retry_policy_for_testing(
        client: JacquardBleClient,
        relay_retry_interval_ms: Duration,
        relay_retry_attempts_max: u32,
    ) -> Self {
        Self::from_shared_client_with_retry_policy(
            Arc::new(client),
            RelayRetryPolicy {
                interval_ms: relay_retry_interval_ms,
                max_attempts: relay_retry_attempts_max,
            },
        )
    }

    fn from_shared_client(client: Arc<JacquardBleClient>) -> Self {
        Self::from_shared_client_with_retry_policy(client, RelayRetryPolicy::default())
    }

    fn from_shared_client_with_retry_policy(
        client: Arc<JacquardBleClient>,
        relay_retry_policy: RelayRetryPolicy,
    ) -> Self {
        let (incoming, _) = broadcast::channel(INCOMING_BROADCAST_CAPACITY);
        let (introduced_peer_updates, _) = broadcast::channel(INCOMING_BROADCAST_CAPACITY);
        let (relay_drops, _) = broadcast::channel(INCOMING_BROADCAST_CAPACITY);
        let relay_inflight = Arc::new(Semaphore::new(RELAY_INFLIGHT_MAX));
        // The pump task demuxes raw Jacquard payloads into application messages and relay forwards.
        let pump_task = spawn_application_pump(
            Arc::clone(&client),
            incoming.clone(),
            relay_drops.clone(),
            Arc::clone(&relay_inflight),
            relay_retry_policy,
        );
        Self {
            inner: Arc::new(JacquardBleServiceInner {
                client,
                incoming,
                introduced_peers: Mutex::new(BTreeSet::new()),
                introduced_peer_updates,
                relay_drops,
                pump_task,
            }),
        }
    }

    #[must_use]
    pub fn local_node_id(&self) -> NodeId {
        // recursion-exception: same-name forwarding into the inner client keeps the public service surface aligned
        self.inner.client.local_node_id()
    }

    #[must_use]
    pub fn peer_invite(&self) -> String {
        // recursion-exception: same-name forwarding keeps invite generation on the service surface
        encode_peer_invite(self.local_node_id())
    }

    pub async fn introduce_peer(
        &self,
        node_id: NodeId,
    ) -> Result<JacquardIntroducedPeer, JacquardBleServiceError> {
        let peer = JacquardIntroducedPeer { node_id };
        let inserted = {
            let mut introduced_peers = self.inner.introduced_peers.lock().await;
            introduced_peers.insert(node_id)
        };
        if inserted {
            // allow-ignored-result: broadcast update is best-effort when no subscriber is attached
            let _ = self.inner.introduced_peer_updates.send(peer.clone());
        }
        Ok(peer)
    }

    pub async fn introduce_peer_from_invite(
        &self,
        invite: &str,
    ) -> Result<JacquardIntroducedPeer, JacquardBleServiceError> {
        let node_id = decode_peer_invite(invite)?;
        self.introduce_peer(node_id).await
    }

    pub async fn introduced_peers(&self) -> Vec<JacquardIntroducedPeer> {
        self.inner
            .introduced_peers
            .lock()
            .await
            .iter()
            .copied()
            .map(|node_id| JacquardIntroducedPeer { node_id })
            .collect()
    }

    pub async fn send(
        &self,
        destination_node_id: NodeId,
        payload: &[u8],
    ) -> Result<(), JacquardBleServiceError> {
        let frame = encode_application_frame(self.local_node_id(), destination_node_id, payload);
        self.send_framed(destination_node_id, &frame).await?;
        Ok(())
    }

    async fn send_framed(
        &self,
        destination_node_id: NodeId,
        framed_payload: &[u8],
    ) -> Result<(), JacquardBleServiceError> {
        self.inner
            .client
            .send(destination_node_id, framed_payload)
            .await?;
        Ok(())
    }

    #[must_use]
    pub fn topology(&self) -> MeshTopology {
        // recursion-exception: same-name forwarding keeps the sanitized topology view on the service surface
        sanitize_topology(self.inner.client.topology())
    }

    #[must_use = "dropping the incoming stream discards service payload events"]
    pub fn incoming_messages(&self) -> impl Stream<Item = JacquardIncomingMessage> + Send + '_ {
        broadcast_receiver_stream(self.inner.incoming.subscribe())
    }

    #[must_use = "dropping the topology stream discards service topology updates"]
    pub fn topology_updates(&self) -> impl Stream<Item = MeshTopology> + Send + '_ {
        self.inner.client.topology_stream().map(sanitize_topology)
    }

    #[must_use = "dropping the relay-drop stream discards relay failure events"]
    pub fn relay_drops(&self) -> impl Stream<Item = JacquardRelayDrop> + Send + '_ {
        broadcast_receiver_stream(self.inner.relay_drops.subscribe())
    }

    #[must_use = "dropping the peer-update stream discards introduction events"]
    pub fn introduced_peer_updates(
        &self,
    ) -> impl Stream<Item = JacquardIntroducedPeer> + Send + '_ {
        broadcast_receiver_stream(self.inner.introduced_peer_updates.subscribe())
    }

    #[must_use = "dropping the event stream discards service lifecycle and payload events"]
    pub fn events(&self) -> Pin<Box<dyn Stream<Item = JacquardBleEvent> + Send + '_>> {
        let local_node_id = self.local_node_id();
        let started = stream::once(async move { JacquardBleEvent::Started { local_node_id } });
        let incoming = self
            .incoming_messages()
            .map(|message| JacquardBleEvent::Incoming { message });
        let introduced_peers = self
            .introduced_peer_updates()
            .map(|peer| JacquardBleEvent::PeerIntroduced { peer });
        let relay_drops = self
            .relay_drops()
            .map(|relay| JacquardBleEvent::RelayDropped { relay });
        let topology = self
            .topology_updates()
            .map(|snapshot| JacquardBleEvent::Topology { snapshot });
        Box::pin(started.chain(stream::select(
            incoming,
            stream::select(introduced_peers, stream::select(relay_drops, topology)),
        )))
    }

    pub fn forward_events_to<S>(&self, sink: S)
    where
        S: HostEventSink,
    {
        let service = self.clone();
        let sink = Arc::new(sink);
        tokio::spawn(async move {
            let mut events = service.events();
            // Loop exits when the service's event stream is exhausted on shutdown.
            while let Some(event) = events.as_mut().next().await {
                sink.publish(&event);
            }
        });
    }
}

fn encode_peer_invite(node_id: NodeId) -> String {
    format!(
        "{JACQUARD_BLE_INVITE_SCHEME}://{INVITE_ACTION_PATH}{}",
        hex::encode(node_id.0)
    )
}

fn decode_peer_invite(invite: &str) -> Result<NodeId, JacquardPeerInviteError> {
    let trimmed = invite.trim();
    if trimmed.is_empty() {
        return Err(JacquardPeerInviteError::Empty);
    }

    let node_id_hex = if let Some((scheme, remainder)) = trimmed.split_once("://") {
        if scheme != JACQUARD_BLE_INVITE_SCHEME {
            return Err(JacquardPeerInviteError::InvalidScheme {
                expected: JACQUARD_BLE_INVITE_SCHEME,
                found: scheme.to_string(),
            });
        }
        let path = if remainder.starts_with('/') {
            remainder.to_string()
        } else {
            format!("/{remainder}")
        };
        path.strip_prefix(INVITE_ACTION_PATH)
            .ok_or(JacquardPeerInviteError::InvalidFormat)?
            .trim()
            .to_string()
    } else {
        trimmed.to_string()
    };

    if node_id_hex.len() != 64 {
        return Err(JacquardPeerInviteError::InvalidNodeIdLength {
            found: u64::try_from(node_id_hex.len()).expect("node id length fits into u64"),
        });
    }

    let bytes = hex::decode(&node_id_hex)?;
    let node_id: [u8; 32] =
        bytes
            .try_into()
            .map_err(|_| JacquardPeerInviteError::InvalidNodeIdLength {
                found: u64::try_from(node_id_hex.len()).expect("node id length fits into u64"),
            })?;
    Ok(NodeId(node_id))
}

fn broadcast_receiver_stream<T>(
    receiver: broadcast::Receiver<T>,
) -> impl Stream<Item = T> + Send + 'static
where
    T: Clone + Send + 'static,
{
    stream::unfold(receiver, |mut receiver| async move {
        // Loop exits on RecvError::Closed when all senders are dropped.
        loop {
            match receiver.recv().await {
                Ok(item) => return Some((item, receiver)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
}

struct ApplicationFrame {
    from_node_id: NodeId,
    destination_node_id: NodeId,
    payload: Vec<u8>,
}

// Frame layout: [magic 8B][from_node_id 32B][destination_node_id 32B][payload]
fn encode_application_frame(
    from_node_id: NodeId,
    destination_node_id: NodeId,
    payload: &[u8],
) -> Vec<u8> {
    let mut framed = Vec::with_capacity(
        APPLICATION_FRAME_MAGIC.len()
            + from_node_id.0.len()
            + destination_node_id.0.len()
            + payload.len(),
    );
    framed.extend_from_slice(APPLICATION_FRAME_MAGIC);
    framed.extend_from_slice(&from_node_id.0);
    framed.extend_from_slice(&destination_node_id.0);
    framed.extend_from_slice(payload);
    framed
}

fn decode_application_frame(payload: &[u8]) -> Option<ApplicationFrame> {
    let header_len = APPLICATION_FRAME_MAGIC.len() + 32 + 32;
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

    Some(ApplicationFrame {
        from_node_id: NodeId(from_node_id),
        destination_node_id: NodeId(destination_node_id),
        payload: payload[header_len..].to_vec(),
    })
}

fn spawn_application_pump(
    client: Arc<JacquardBleClient>,
    incoming: broadcast::Sender<JacquardIncomingMessage>,
    relay_drops: broadcast::Sender<JacquardRelayDrop>,
    relay_inflight: Arc<Semaphore>,
    relay_retry_policy: RelayRetryPolicy,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let local_node_id = client.local_node_id();
        let mut transport_incoming = Box::pin(client.incoming());
        // Loop exits when the client's incoming stream is exhausted on shutdown.
        while let Some((transport_from_node_id, payload)) = transport_incoming.as_mut().next().await
        {
            let Some(frame) = decode_application_frame(&payload) else {
                // Drop anything that isn't a recognized application frame (e.g. internal Jacquard traffic).
                continue;
            };
            if frame.destination_node_id == local_node_id {
                // Deliver to the local subscriber; the transport already handled routing.
                // Broadcast drops when no subscribers are active; that is expected at startup.
                // allow-ignored-result: incoming fanout is best-effort when no application listener is attached
                let _ = incoming.send(JacquardIncomingMessage {
                    from_node_id: frame.from_node_id,
                    transport_from_node_id,
                    payload: frame.payload,
                });
                continue;
            }
            // This node is an intermediate hop; forward the frame toward the true destination.
            let relay_client = Arc::clone(&client);
            let relay_drops = relay_drops.clone();
            let relay = JacquardRelayDrop {
                from_node_id: frame.from_node_id,
                destination_node_id: frame.destination_node_id,
            };
            let Ok(relay_permit) = Arc::clone(&relay_inflight).acquire_owned().await else {
                break;
            };
            tokio::spawn(async move {
                let _relay_permit = relay_permit;
                if !relay_framed_payload(
                    &relay_client,
                    frame.destination_node_id,
                    &payload,
                    relay_retry_policy,
                )
                .await
                {
                    // allow-ignored-result: relay-drop fanout is best-effort when no observer is attached
                    let _ = relay_drops.send(relay);
                }
            });
        }
    })
}

async fn relay_framed_payload(
    client: &JacquardBleClient,
    destination_node_id: NodeId,
    payload: &[u8],
    relay_retry_policy: RelayRetryPolicy,
) -> bool {
    for attempt in 0..relay_retry_policy.max_attempts {
        match client.send(destination_node_id, payload).await {
            Ok(_) => return true,
            Err(error)
                if relay_retryable(&error) && attempt + 1 < relay_retry_policy.max_attempts =>
            {
                tokio::time::sleep(relay_retry_policy.interval_ms).await;
            }
            Err(_) => return false,
        }
    }
    false
}

fn relay_retryable(error: &BleClientError) -> bool {
    matches!(
        error,
        BleClientError::Route(
            RouteError::Selection(RouteSelectionError::NoCandidate)
                | RouteError::Transport(TransportError::Unavailable | TransportError::TimedOut)
        ) | BleClientError::Transport(TransportError::Unavailable | TransportError::TimedOut)
            | BleClientError::Bridge(
                BleBridgeError::Route(RouteError::Transport(
                    TransportError::Unavailable | TransportError::TimedOut
                )) | BleBridgeError::Transport(
                    TransportError::Unavailable | TransportError::TimedOut
                )
            )
    )
}

// Strip topology details the local node cannot directly verify: non-adjacent nodes, relay paths.
fn sanitize_topology(snapshot: MeshTopology) -> MeshTopology {
    let local_node_id = snapshot.local_node_id;
    let mut visible_node_ids = BTreeSet::from([local_node_id]);
    let edges = snapshot
        .edges
        .into_iter()
        .filter_map(|((from_node_id, to_node_id), edge)| {
            (from_node_id == local_node_id).then(|| {
                visible_node_ids.insert(to_node_id);
                ((from_node_id, to_node_id), edge)
            })
        })
        .collect();
    let nodes = snapshot
        .nodes
        .into_iter()
        .filter(|(node_id, _)| visible_node_ids.contains(node_id))
        .collect();
    let active_routes = snapshot
        .active_routes
        .into_iter()
        .filter(|(_, route)| {
            visible_node_ids.contains(&route.owner_node_id)
                && visible_node_ids.contains(&route.terminal_node_id)
        })
        .collect();

    MeshTopology {
        local_node_id,
        observed_at_tick: snapshot.observed_at_tick,
        nodes,
        edges,
        active_routes,
    }
}

#[derive(Default)]
pub struct JacquardBleHostState {
    service: Mutex<Option<Arc<JacquardBleService>>>,
}

impl JacquardBleHostState {
    pub async fn start(
        &self,
        local_node_id: NodeId,
    ) -> Result<Arc<JacquardBleService>, JacquardBleServiceError> {
        {
            let guard = self.service.lock().await;
            if let Some(service) = guard.as_ref() {
                return Err(JacquardBleServiceError::AlreadyStarted {
                    local_node_id: service.local_node_id(),
                });
            }
        }

        let service = Arc::new(JacquardBleService::new(local_node_id).await?);
        let mut guard = self.service.lock().await;
        if let Some(existing) = guard.as_ref() {
            return Err(JacquardBleServiceError::AlreadyStarted {
                local_node_id: existing.local_node_id(),
            });
        }
        *guard = Some(Arc::clone(&service));
        Ok(service)
    }

    pub async fn start_with_event_sink<S>(
        &self,
        local_node_id: NodeId,
        sink: S,
    ) -> Result<Arc<JacquardBleService>, JacquardBleServiceError>
    where
        S: HostEventSink,
    {
        let service = self.start(local_node_id).await?;
        service.forward_events_to(sink);
        Ok(service)
    }

    pub async fn service(&self) -> Result<Arc<JacquardBleService>, JacquardBleServiceError> {
        self.service
            .lock()
            .await
            .as_ref()
            .cloned()
            .ok_or(JacquardBleServiceError::NotStarted)
    }

    pub async fn send(
        &self,
        destination_node_id: NodeId,
        payload: &[u8],
    ) -> Result<(), JacquardBleServiceError> {
        // recursion-exception: same-name forwarding keeps the host-state facade thin over the service API
        self.service()
            .await?
            .send(destination_node_id, payload)
            .await
    }

    pub async fn topology(&self) -> Result<MeshTopology, JacquardBleServiceError> {
        // recursion-exception: same-name forwarding keeps topology access on the host-state facade
        Ok(self.service().await?.topology())
    }

    pub async fn peer_invite(&self) -> Result<String, JacquardBleServiceError> {
        // recursion-exception: same-name forwarding keeps invite access on the host-state facade
        Ok(self.service().await?.peer_invite())
    }

    pub async fn introduce_peer(
        &self,
        node_id: NodeId,
    ) -> Result<JacquardIntroducedPeer, JacquardBleServiceError> {
        // recursion-exception: same-name forwarding keeps peer introduction on the host-state facade
        self.service().await?.introduce_peer(node_id).await
    }

    pub async fn introduce_peer_from_invite(
        &self,
        invite: &str,
    ) -> Result<JacquardIntroducedPeer, JacquardBleServiceError> {
        // recursion-exception: same-name forwarding keeps invite parsing on the host-state facade
        self.service()
            .await?
            .introduce_peer_from_invite(invite)
            .await
    }

    pub async fn introduced_peers(
        &self,
    ) -> Result<Vec<JacquardIntroducedPeer>, JacquardBleServiceError> {
        // recursion-exception: same-name forwarding keeps peer snapshot access on the host-state facade
        Ok(self.service().await?.introduced_peers().await)
    }

    #[doc(hidden)]
    pub async fn install_for_testing(
        &self,
        service: JacquardBleService,
    ) -> Result<Arc<JacquardBleService>, JacquardBleServiceError> {
        let mut guard = self.service.lock().await;
        if let Some(existing) = guard.as_ref() {
            return Err(JacquardBleServiceError::AlreadyStarted {
                local_node_id: existing.local_node_id(),
            });
        }
        let service = Arc::new(service);
        *guard = Some(Arc::clone(&service));
        Ok(service)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        JacquardPeerInviteError, decode_application_frame, decode_peer_invite,
        encode_application_frame, encode_peer_invite,
    };
    use jacquard_core::NodeId;

    fn node_id(byte: u8) -> NodeId {
        NodeId([byte; 32])
    }

    #[test]
    fn peer_invite_round_trips_through_deep_link() {
        let node_id = node_id(7);
        let invite = encode_peer_invite(node_id);
        assert_eq!(decode_peer_invite(&invite).expect("decode invite"), node_id);
    }

    #[test]
    fn peer_invite_accepts_plain_hex_node_ids() {
        let node_id = node_id(9);
        let plain = hex::encode(node_id.0);
        assert_eq!(decode_peer_invite(&plain).expect("decode plain"), node_id);
    }

    #[test]
    fn peer_invite_rejects_wrong_scheme() {
        let err = decode_peer_invite("wrong-scheme:///add-peer/deadbeef")
            .expect_err("wrong scheme should fail");
        assert!(matches!(err, JacquardPeerInviteError::InvalidScheme { .. }));
    }

    #[test]
    fn application_frame_preserves_claimed_source_for_relay() {
        let claimed_source = node_id(1);
        let destination = node_id(3);
        let payload = encode_application_frame(claimed_source, destination, b"relay");
        let frame = decode_application_frame(&payload).expect("decode application frame");

        assert_eq!(frame.from_node_id, claimed_source);
        assert_eq!(frame.destination_node_id, destination);
        assert_eq!(frame.payload, b"relay");
    }
}
