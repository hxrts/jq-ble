//! Jacquard BLE client. Assembles the router, bridge, and BLE transport into
//! a single [`JacquardBleClient`] handle for application use.
//!
//! Provides the host bridge ([`BleHostBridge`]), runtime effects
//! ([`BleRuntimeEffects`]), and topology projection ([`link_profile`],
//! [`node_profile`]) needed to run a Jacquard mesh over BLE.

mod bridge;
mod client;
mod effects;
mod projector;

pub use bridge::{
    BleBridgeConfig, BleBridgeError, BleBridgeIo, BleBridgeProgress, BleBridgeRoundReport,
    BleBridgeRouter, BleBridgeTickSource, BleBridgeTransport, BleBridgeWaitDecision,
    BleBridgeWaitState, BleHostBridge, MonotonicBleTickSource,
};
pub use client::{
    BleClientError, JacquardBleClient, JacquardBleRouter, decode_client_payload_for_testing,
    encode_client_payload_for_testing,
};
pub use effects::BleRuntimeEffects;
pub use jq_link_profile as link_profile;
pub use jq_node_profile as node_profile;
