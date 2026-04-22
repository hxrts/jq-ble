//! Session resolution helpers for the BLE runtime task.
//!
//! Pure functions operating on [`PeerSessions`] and [`BleSession`]: device
//! lookup, remote `NodeId` and PSM reads over GATT, and endpoint construction.

use blew::central::Central;
use blew::central::backend::CentralBackend;
use blew::l2cap::types::Psm;
use blew::types::DeviceId;
use jacquard_core::{LinkEndpoint, NodeId};

use crate::gatt::{JACQUARD_NODE_ID_CHAR_UUID, JACQUARD_PSM_CHAR_UUID, parse_psm_value};
use crate::transport::{BleSession, PeerSessions};

#[must_use]
pub(crate) fn session_references_device(sessions: &PeerSessions, device_id: &DeviceId) -> bool {
    sessions.references_device(device_id)
}

pub(crate) async fn resolve_remote_node_id<CB: CentralBackend>(
    central: &Central<CB>,
    device_id: &DeviceId,
) -> Result<NodeId, blew::BlewError> {
    let node_id = central
        .read_characteristic(device_id, JACQUARD_NODE_ID_CHAR_UUID)
        .await?;
    // NodeId is always exactly 32 bytes; reject malformed peers before installing a session.
    if node_id.len() != 32 {
        return Err(blew::BlewError::Gatt {
            device_id: device_id.clone(),
            source: format!("invalid node id length: {}", node_id.len()).into(),
        });
    }

    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&node_id);
    Ok(NodeId(bytes))
}

pub(crate) async fn resolve_remote_psm<CB: CentralBackend>(
    central: &Central<CB>,
    device_id: &DeviceId,
) -> Option<Psm> {
    // Returns None if the PSM characteristic is absent (peer does not support L2CAP CoC).
    let psm_bytes = central
        .read_characteristic(device_id, JACQUARD_PSM_CHAR_UUID)
        .await
        .ok()?;
    parse_psm_value(&psm_bytes)
}

#[must_use]
pub(crate) fn device_id_from_endpoint(endpoint: &LinkEndpoint) -> Option<DeviceId> {
    // Reverses ble_locator(): recovers the DeviceId address string from the "ble"-scoped locator bytes.
    match &endpoint.locator {
        jacquard_core::EndpointLocator::ScopedBytes { scope, bytes } if scope == "ble" => {
            String::from_utf8(bytes.clone()).ok().map(DeviceId::from)
        }
        _ => None,
    }
}

#[must_use]
pub(crate) fn endpoint_for_session(session: &BleSession) -> LinkEndpoint {
    session.endpoint()
}
