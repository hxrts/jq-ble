//! Shared transport surface types for the BLE link profile.
//!
//! Defines [`BleConfig`], [`BleTransportSender`] (sync command queue),
//! [`BleDriverControl`] (async post-round dispatch), [`BleOutboundCommand`],
//! [`BleDriverCommand`], [`BleSession`], [`PeerSessions`],
//! [`DiscoveredPeerHint`], and [`BleLinkError`].

use blew::central::Central;
use blew::peripheral::Peripheral;
use blew::types::DeviceId;
use jacquard_core::{LinkEndpoint, NodeId, TransportError};
use jacquard_host_support::{
    DispatchReceiver, DispatchSender, TransportIngressNotifier, TransportIngressReceiver,
};
use jacquard_traits::{TransportDriver, TransportSenderEffects, effect_handler};
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::task::BleRuntimeTask;

pub(crate) const DEFAULT_INGRESS_CAPACITY: u32 = 1024;
pub(crate) const DEFAULT_COMMAND_CAPACITY: u32 = 64;

pub(crate) type BleRuntimeParts = (
    BleTransportDriver,
    BleTransportSender,
    DispatchReceiver<BleOutboundCommand>,
    BleDriverControl,
    TransportIngressNotifier,
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleConfig {
    pub ingress_capacity: u32,
    pub command_capacity: u32,
}

impl Default for BleConfig {
    fn default() -> Self {
        Self {
            ingress_capacity: DEFAULT_INGRESS_CAPACITY,
            command_capacity: DEFAULT_COMMAND_CAPACITY,
        }
    }
}

#[derive(Debug, Error)]
pub enum BleLinkError {
    #[error(transparent)]
    Blew(#[from] blew::BlewError),
    #[error("ble ingress capacity must be non-zero")]
    ZeroIngressCapacity,
    #[error("ble command capacity must be non-zero")]
    ZeroCommandCapacity,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredPeerHint {
    pub device_id: DeviceId,
    // First 6 bytes of the NodeId extracted from the discovery UUID before a full connection is made.
    pub node_id_prefix: [u8; 6],
    pub advertised_gatt: bool,
    pub advertised_l2cap: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct BlePeerKey(pub(crate) String);

impl From<&DeviceId> for BlePeerKey {
    fn from(device_id: &DeviceId) -> Self {
        Self(device_id.as_str().to_owned())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BleChannelId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BleSession {
    // Central role: we connected to the peer and write to their C2P characteristic.
    GattCentral {
        device_id: DeviceId,
    },
    // L2CAP CoC channel is the preferred path when both sides support it.
    L2cap {
        device_id: DeviceId,
        channel_id: BleChannelId,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleNotifySubscriber {
    pub device_id: DeviceId,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerSessions {
    state: PeerSessionState,
    notify_subscribers: Vec<BleNotifySubscriber>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum PeerSessionState {
    #[default]
    Disconnected,
    GattOnly {
        ingress: BleSession,
        egress: BleSession,
    },
    L2capPreferred {
        session: BleSession,
        fallback: Option<BleSession>,
    },
}

impl PeerSessions {
    #[must_use]
    pub fn disconnected() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn gatt_only(session: BleSession) -> Self {
        Self {
            state: PeerSessionState::GattOnly {
                ingress: session.clone(),
                egress: session,
            },
            notify_subscribers: Vec::new(),
        }
    }

    #[must_use]
    pub fn gatt_with_fallback(session: BleSession) -> Self {
        Self::gatt_only(session)
    }

    #[must_use]
    pub fn gatt_notify_fanout(subscriber: BleNotifySubscriber) -> Self {
        Self {
            state: PeerSessionState::Disconnected,
            notify_subscribers: vec![subscriber],
        }
    }

    #[must_use]
    pub fn l2cap_preferred(session: BleSession, fallback: Option<BleSession>) -> Self {
        Self {
            state: PeerSessionState::L2capPreferred { session, fallback },
            notify_subscribers: Vec::new(),
        }
    }

    pub(crate) fn install_gatt_subscription(&mut self, subscriber: BleNotifySubscriber) {
        if let Some(existing) = self
            .notify_subscribers
            .iter_mut()
            .find(|existing| existing.device_id == subscriber.device_id)
        {
            *existing = subscriber;
            return;
        }
        self.notify_subscribers.push(subscriber);
    }

    pub(crate) fn install_gatt_central(&mut self, session: BleSession) {
        match &mut self.state {
            PeerSessionState::Disconnected | PeerSessionState::GattOnly { .. } => {
                self.state = PeerSessionState::GattOnly {
                    ingress: session.clone(),
                    egress: session,
                };
            }
            PeerSessionState::L2capPreferred { fallback, .. } => {
                if fallback.is_none() {
                    *fallback = Some(session);
                }
            }
        }
    }

    pub(crate) fn promote_l2cap(&mut self, session: BleSession) {
        let fallback = self.first_gatt_fallback();
        self.state = PeerSessionState::L2capPreferred { session, fallback };
    }

    pub(crate) fn downgrade_l2cap(&mut self, channel_id: BleChannelId) -> Option<BleSession> {
        let fallback = match &self.state {
            PeerSessionState::L2capPreferred {
                session:
                    BleSession::L2cap {
                        channel_id: current,
                        ..
                    },
                fallback,
            } if *current == channel_id => fallback.clone(),
            _ => return self.preferred_egress(),
        };
        if let Some(fallback) = fallback {
            self.state = PeerSessionState::GattOnly {
                ingress: fallback.clone(),
                egress: fallback.clone(),
            };
            return Some(fallback);
        }
        self.state = PeerSessionState::Disconnected;
        None
    }

    #[must_use]
    pub(crate) fn egress_for_device(&self, device_id: &DeviceId) -> Option<BleSession> {
        self.preferred_egress()
            .filter(|session| session.device_id() == device_id)
    }

    #[must_use]
    pub(crate) fn endpoints_for_device(&self, device_id: &DeviceId) -> Vec<LinkEndpoint> {
        let mut endpoints = Vec::new();
        for session in self.iter_sessions() {
            if session.device_id() != device_id {
                continue;
            }
            push_unique_endpoint(&mut endpoints, session.endpoint());
        }
        for subscriber in &self.notify_subscribers {
            if &subscriber.device_id == device_id {
                push_unique_endpoint(&mut endpoints, crate::gatt::gatt_endpoint(device_id));
            }
        }
        endpoints
    }

    #[must_use]
    pub(crate) fn references_device(&self, device_id: &DeviceId) -> bool {
        self.iter_sessions()
            .any(|session| session.device_id() == device_id)
            || self
                .notify_subscribers
                .iter()
                .any(|subscriber| &subscriber.device_id == device_id)
    }

    fn preferred_egress(&self) -> Option<BleSession> {
        match &self.state {
            PeerSessionState::Disconnected => None,
            PeerSessionState::GattOnly { egress, .. } => Some(egress.clone()),
            PeerSessionState::L2capPreferred { session, .. } => Some(session.clone()),
        }
    }

    fn iter_sessions(&self) -> impl Iterator<Item = &BleSession> {
        let mut sessions = Vec::new();
        match &self.state {
            PeerSessionState::Disconnected => {}
            PeerSessionState::GattOnly { ingress, egress } => {
                sessions.push(ingress);
                sessions.push(egress);
            }
            PeerSessionState::L2capPreferred { session, fallback } => {
                sessions.push(session);
                if let Some(fallback) = fallback {
                    sessions.push(fallback);
                }
            }
        }
        sessions.into_iter()
    }

    fn first_gatt_fallback(&self) -> Option<BleSession> {
        self.iter_sessions()
            .find(|session| !matches!(session, BleSession::L2cap { .. }))
            .cloned()
    }
}

impl BleSession {
    #[must_use]
    pub fn device_id(&self) -> &DeviceId {
        match self {
            Self::GattCentral { device_id } | Self::L2cap { device_id, .. } => device_id,
        }
    }

    #[must_use]
    pub fn endpoint(&self) -> LinkEndpoint {
        match self {
            Self::GattCentral { device_id } => crate::gatt::gatt_endpoint(device_id),
            Self::L2cap { device_id, .. } => crate::gatt::l2cap_endpoint(device_id),
        }
    }
}

fn push_unique_endpoint(endpoints: &mut Vec<LinkEndpoint>, endpoint: LinkEndpoint) {
    if !endpoints.contains(&endpoint) {
        endpoints.push(endpoint);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BleOutboundCommand {
    pub endpoint: LinkEndpoint,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BleDriverCommand {
    Dispatch(Vec<BleOutboundCommand>),
    Shutdown,
}

#[derive(Clone)]
pub struct BleTransportSender {
    outbound: DispatchSender<BleOutboundCommand>,
}

impl BleTransportSender {
    #[must_use]
    pub(crate) fn new(outbound: DispatchSender<BleOutboundCommand>) -> Self {
        Self { outbound }
    }
}

#[effect_handler]
impl TransportSenderEffects for BleTransportSender {
    fn send_transport(
        &mut self,
        endpoint: &LinkEndpoint,
        payload: &[u8],
    ) -> Result<(), TransportError> {
        // Enqueue to the dispatch mailbox; the runtime task drains this after each router round.
        self.outbound
            .send(BleOutboundCommand {
                endpoint: endpoint.clone(),
                payload: payload.to_vec(),
            })
            .map(|_| ())
            .map_err(|_| TransportError::Unavailable)
    }
}

#[derive(Clone)]
pub struct BleDriverControl {
    pub(crate) tx: mpsc::Sender<BleDriverCommand>,
}

impl BleDriverControl {
    #[must_use]
    pub(crate) fn new(tx: mpsc::Sender<BleDriverCommand>) -> Self {
        Self { tx }
    }

    pub async fn dispatch(&self, batch: Vec<BleOutboundCommand>) -> Result<(), TransportError> {
        self.tx
            .send(BleDriverCommand::Dispatch(batch))
            .await
            .map_err(|_| TransportError::Unavailable)
    }

    pub async fn shutdown(&self) -> Result<(), TransportError> {
        self.tx
            .send(BleDriverCommand::Shutdown)
            .await
            .map_err(|_| TransportError::Unavailable)
    }
}

pub struct BleTransportDriver {
    ingress: TransportIngressReceiver,
    control: BleDriverControl,
}

impl BleTransportDriver {
    #[must_use]
    pub(crate) fn new(ingress: TransportIngressReceiver, control: BleDriverControl) -> Self {
        Self { ingress, control }
    }
}

impl TransportDriver for BleTransportDriver {
    fn drain_transport_ingress(
        &mut self,
    ) -> Result<Vec<jacquard_core::TransportIngressEvent>, TransportError> {
        // Drains everything accumulated since the last round in one batch for the bridge to stamp.
        Ok(self.ingress.drain().events)
    }

    fn shutdown_transport_driver(&mut self) -> Result<(), TransportError> {
        // try_send avoids blocking the synchronous caller; failure means the runtime is already gone.
        self.control
            .tx
            .try_send(BleDriverCommand::Shutdown)
            .map_err(|_| TransportError::Unavailable)
    }
}

pub struct BleTransportComponents {
    pub driver: BleTransportDriver,
    pub sender: BleTransportSender,
    pub(crate) outbound: DispatchReceiver<BleOutboundCommand>,
    pub control: BleDriverControl,
    pub notifier: TransportIngressNotifier,
    pub(crate) runtime_task: JoinHandle<()>,
}

impl BleTransportComponents {
    #[must_use = "constructing the BLE transport components has no effect unless the returned components are used"]
    pub async fn new(local_node_id: NodeId, config: BleConfig) -> Result<Self, BleLinkError> {
        // recursion-exception: constructor performs backend setup while retaining the conventional `new` entrypoint
        let central: Central = Central::new().await?;
        let peripheral: Peripheral = Peripheral::new().await?;
        BleRuntimeTask::spawn(local_node_id, central, peripheral, config)
    }

    #[must_use]
    pub fn runtime_task(&self) -> &JoinHandle<()> {
        &self.runtime_task
    }

    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        BleTransportDriver,
        BleTransportSender,
        DispatchReceiver<BleOutboundCommand>,
        BleDriverControl,
        TransportIngressNotifier,
        JoinHandle<()>,
    ) {
        (
            self.driver,
            self.sender,
            self.outbound,
            self.control,
            self.notifier,
            self.runtime_task,
        )
    }
}
