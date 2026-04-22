//! Jacquard BLE link profile. BLE transport profile for the Jacquard mesh
//! router.
//!
//! Exposes [`BleTransportComponents`] as the entry point. Modules: [`gatt`]
//! (service shape, UUIDs, locators), [`transport`] (sender, driver control,
//! session and config types), [`session`] (resolution helpers), [`l2cap`]
//! (channel tasks and identity exchange), [`task`] (single async runtime
//! owner).

#![forbid(unsafe_code)]

mod cast;
mod gatt;
mod l2cap;
mod session;
mod task;
mod transport;

pub use cast::{gatt_notify_multicast_support, link_observation_from_ble_event};
pub use gatt::{
    IROH_BLE_DEVICE_ID_STABILITY_POLICY, JACQUARD_C2P_CHAR_UUID, JACQUARD_IDENTITY_SERVICE_UUID,
    JACQUARD_NODE_ID_CHAR_UUID, JACQUARD_P2C_CHAR_UUID, JACQUARD_PSM_CHAR_UUID,
    advertised_hint_service, ble_locator, discovery_service_uuid, gatt_endpoint,
    gatt_fallback_service, gatt_l2cap_service, identity_gatt_service, l2cap_endpoint,
    parse_discovery_hint, parse_psm_value,
};
pub use task::BleRuntimeTask;
pub use transport::{
    BleConfig, BleDriverCommand, BleDriverControl, BleLinkError, BleNotifySubscriber,
    BleOutboundCommand, BleSession, BleTransportComponents, BleTransportDriver, BleTransportSender,
    DiscoveredPeerHint, PeerSessions,
};
