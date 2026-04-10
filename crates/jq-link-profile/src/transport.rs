//! Shared transport surface types for the BLE link profile.
//!
//! Defines [`BleConfig`], [`BleTransportSender`] (sync command queue),
//! [`BleDriverControl`] (async post-round dispatch), [`BleOutboundCommand`],
//! [`BleDriverCommand`], [`BleSession`], [`PeerSessions`],
//! [`DiscoveredPeerHint`], and [`BleLinkError`].

use blew::central::Central;
use blew::peripheral::Peripheral;
use blew::types::DeviceId;
use jacquard_adapter::{
    DispatchReceiver, DispatchSender, TransportIngressNotifier, TransportIngressReceiver,
};
use jacquard_core::{LinkEndpoint, NodeId, TransportError};
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
    // Peripheral role: the peer subscribed to our P2C characteristic so we notify them.
    GattPeripheralSubscribed {
        device_id: DeviceId,
    },
    // L2CAP CoC channel is the preferred path when both sides support it.
    L2cap {
        device_id: DeviceId,
        channel_id: BleChannelId,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeerSessions {
    // The session used to receive payloads from this peer (GATT or L2CAP).
    pub preferred_ingress: Option<BleSession>,
    // The session used to send payloads to this peer; upgraded to L2CAP when available.
    pub preferred_egress: Option<BleSession>,
    // GATT session retained so egress can downgrade if the L2CAP channel closes unexpectedly.
    pub fallback_egress: Option<BleSession>,
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
