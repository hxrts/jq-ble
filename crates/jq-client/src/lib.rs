//! Jacquard BLE client. Assembles the router, bridge, and BLE transport into
//! a single [`JacquardBleClient`] handle for application use.
//!
//! Provides the host bridge ([`BleHostBridge`]), runtime effects
//! ([`BleRuntimeEffects`]), and topology projection ([`link_profile`],
//! [`node_profile`]) needed to run a Jacquard mesh over BLE.

#![forbid(unsafe_code)]

mod bridge;
mod client;
mod effects;
mod framing;
mod projector;
mod routing;

pub use bridge::{
    BleBridgeConfig, BleBridgeError, BleBridgeIo, BleBridgeProgress, BleBridgeRoundReport,
    BleBridgeRouter, BleBridgeTickSource, BleBridgeTransport, BleBridgeWaitDecision,
    BleBridgeWaitState, BleHostBridge, MonotonicBleTickSource,
};
pub use client::{
    BleClientError, JacquardBleClient, JacquardBleMulticastReceipt, JacquardBleSendReceipt,
    JacquardBleSendStage,
};
pub use effects::BleRuntimeEffects;
pub use jq_link_profile as link_profile;
pub use jq_node_profile as node_profile;
pub use routing::JacquardBleRouter;

#[doc(hidden)]
pub mod test_support {
    use crate::framing::{decode_client_payload, encode_client_payload};

    #[must_use]
    pub fn encode_client_payload_for_testing(payload: &[u8]) -> Vec<u8> {
        encode_client_payload(payload)
    }

    #[must_use]
    pub fn decode_client_payload_for_testing(payload: &[u8]) -> Option<Vec<u8>> {
        decode_client_payload(payload).map(ToOwned::to_owned)
    }
}
