//! GATT service shape, UUID constants, and BLE locator helpers.
//!
//! Defines the Jacquard BLE service layout (`6a6171xx` namespace):
//! `NODE_ID_CHAR_UUID` (required full identity), `C2P`/`P2C` (GATT payload
//! path), `PSM_CHAR_UUID` (L2CAP upgrade). Discovery hints use a separate
//! `6a6172xx` advertisement UUID encoding the first 6 bytes of the `NodeId`.

use blew::gatt::props::{AttributePermissions, CharacteristicProperties};
use blew::gatt::service::{GattCharacteristic, GattService};
use blew::l2cap::types::Psm;
use blew::types::DeviceId;
use jacquard_core::{ByteCount, EndpointLocator, LinkEndpoint, NodeId, TransportKind};
use uuid::Uuid;

// Primary service UUID for Jacquard identity and transport; the byte prefix 'ja\x01' identifies the namespace.
pub const JACQUARD_IDENTITY_SERVICE_UUID: Uuid =
    Uuid::from_u128(0x6a617100_8e45_4c2c_b3a5_331f3098b5c2);
// Readable characteristic that carries the full 32-byte NodeId so the central can verify the advertised hint.
pub const JACQUARD_NODE_ID_CHAR_UUID: Uuid =
    Uuid::from_u128(0x6a617101_8e45_4c2c_b3a5_331f3098b5c2);
// Central-to-peripheral data path: central writes payloads here.
pub const JACQUARD_C2P_CHAR_UUID: Uuid = Uuid::from_u128(0x6a617102_8e45_4c2c_b3a5_331f3098b5c2);
// Peripheral-to-central data path: peripheral notifies the central with payloads.
pub const JACQUARD_P2C_CHAR_UUID: Uuid = Uuid::from_u128(0x6a617103_8e45_4c2c_b3a5_331f3098b5c2);
// Optional 2-byte LE PSM value; presence indicates the peripheral supports L2CAP CoC upgrade.
pub const JACQUARD_PSM_CHAR_UUID: Uuid = Uuid::from_u128(0x6a617104_8e45_4c2c_b3a5_331f3098b5c2);

// Advertisement UUID namespace prefix ('jar\x00') distinct from the service UUIDs to avoid service-scan confusion.
const DISCOVERY_HINT_NAMESPACE_PREFIX: u128 = 0x6a617200_0000_0000_0000_000000000000;

pub const IROH_BLE_DEVICE_ID_STABILITY_POLICY: &str = "BLE DeviceId values are treated as short-lived observational locators. \
     Platform privacy rotation and runtime-specific identifiers mean this crate \
     does not assume long-term locator stability across rediscovery.";

#[must_use]
pub fn discovery_service_uuid(node_id: &NodeId) -> Uuid {
    let prefix = node_id_prefix(node_id);
    // Encode the 6-byte prefix into bytes 4-9 of the UUID so scanners can extract it without connecting.
    let encoded = u128::from_be_bytes([
        b'j', b'a', b'r', 0, prefix[0], prefix[1], prefix[2], prefix[3], prefix[4], prefix[5], 0,
        0, 0, 0, 0, 0,
    ]);
    Uuid::from_u128(DISCOVERY_HINT_NAMESPACE_PREFIX | encoded)
}

#[must_use]
pub fn parse_discovery_hint(services: &[Uuid]) -> Option<[u8; 6]> {
    services.iter().find_map(|uuid| {
        let bytes = uuid.as_u128().to_be_bytes();
        if bytes[..4] != [b'j', b'a', b'r', 0] {
            return None;
        }

        Some([bytes[4], bytes[5], bytes[6], bytes[7], bytes[8], bytes[9]])
    })
}

#[must_use]
pub fn advertised_hint_service(node_id: &NodeId) -> GattService {
    GattService {
        uuid: discovery_service_uuid(node_id),
        // Non-primary so it is included in advertisement packets without polluting the primary service list.
        primary: false,
        characteristics: Vec::new(),
    }
}

#[must_use]
pub fn identity_gatt_service(node_id: &NodeId) -> GattService {
    GattService {
        uuid: JACQUARD_IDENTITY_SERVICE_UUID,
        primary: true,
        characteristics: vec![GattCharacteristic {
            uuid: JACQUARD_NODE_ID_CHAR_UUID,
            properties: CharacteristicProperties::READ,
            permissions: AttributePermissions::READ,
            value: node_id.0.to_vec(),
            descriptors: Vec::new(),
        }],
    }
}

#[must_use]
pub fn gatt_fallback_service(node_id: &NodeId) -> GattService {
    transport_gatt_service(node_id, None)
}

#[must_use]
pub fn gatt_l2cap_service(node_id: &NodeId, psm: Psm) -> GattService {
    transport_gatt_service(node_id, Some(psm))
}

fn transport_gatt_service(node_id: &NodeId, psm: Option<Psm>) -> GattService {
    let mut characteristics = vec![
        GattCharacteristic {
            uuid: JACQUARD_NODE_ID_CHAR_UUID,
            properties: CharacteristicProperties::READ,
            permissions: AttributePermissions::READ,
            // Static value is safe here because NodeId does not change; no WriteRequest property so no SIGABRT.
            value: node_id.0.to_vec(),
            descriptors: Vec::new(),
        },
        GattCharacteristic {
            uuid: JACQUARD_C2P_CHAR_UUID,
            // Both WRITE and WRITE_WITHOUT_RESPONSE so Android (requires WRITE) and Apple (prefers WO_RESP) both work.
            properties: CharacteristicProperties::WRITE
                | CharacteristicProperties::WRITE_WITHOUT_RESPONSE,
            permissions: AttributePermissions::WRITE,
            // Empty value required for writable characteristics to avoid CoreBluetooth SIGABRT.
            value: Vec::new(),
            descriptors: Vec::new(),
        },
        GattCharacteristic {
            uuid: JACQUARD_P2C_CHAR_UUID,
            properties: CharacteristicProperties::NOTIFY,
            // READ permission satisfies the CCCD subscription requirement on most platforms.
            permissions: AttributePermissions::READ,
            value: Vec::new(),
            descriptors: Vec::new(),
        },
    ];
    if let Some(psm) = psm {
        characteristics.push(GattCharacteristic {
            uuid: JACQUARD_PSM_CHAR_UUID,
            properties: CharacteristicProperties::READ,
            permissions: AttributePermissions::READ,
            // PSM stored as little-endian u16 matching the parse_psm_value convention.
            value: psm.value().to_le_bytes().to_vec(),
            descriptors: Vec::new(),
        });
    }

    GattService {
        uuid: JACQUARD_IDENTITY_SERVICE_UUID,
        primary: true,
        characteristics,
    }
}

#[must_use]
pub fn ble_locator(device_id: &DeviceId) -> EndpointLocator {
    // The "ble" scope prefix allows session.rs to recover the DeviceId from a LinkEndpoint at dispatch time.
    EndpointLocator::ScopedBytes {
        scope: "ble".into(),
        bytes: device_id.as_str().as_bytes().to_vec(),
    }
}

#[must_use]
pub fn gatt_endpoint(device_id: &DeviceId) -> LinkEndpoint {
    // 512-byte MTU matches the GATT write MTU negotiated by Android and used as the ceiling on Apple.
    LinkEndpoint::new(
        TransportKind::BleGatt,
        ble_locator(device_id),
        ByteCount(512),
    )
}

#[must_use]
pub fn l2cap_endpoint(device_id: &DeviceId) -> LinkEndpoint {
    // L2CAP CoC supports larger frames; 1472 aligns with the reliable.rs MAX_DATAGRAM_SIZE.
    LinkEndpoint::new(
        TransportKind::BleL2cap,
        ble_locator(device_id),
        ByteCount(1472),
    )
}

#[must_use]
pub fn node_id_prefix(node_id: &NodeId) -> [u8; 6] {
    let bytes = node_id.0;
    [bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5]]
}

#[must_use]
pub fn hint_matches_node_id(hint: [u8; 6], node_id: &NodeId) -> bool {
    hint == node_id_prefix(node_id)
}

#[must_use]
pub fn parse_psm_value(bytes: &[u8]) -> Option<Psm> {
    if bytes.len() < 2 {
        return None;
    }

    let psm = u16::from_le_bytes([bytes[0], bytes[1]]);
    if psm == 0 { None } else { Some(Psm(psm)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_uuid_round_trips_hint_prefix() {
        let node_id = NodeId([7; 32]);
        let services = vec![discovery_service_uuid(&node_id)];

        let parsed = parse_discovery_hint(&services);

        assert_eq!(parsed, Some(node_id_prefix(&node_id)));
    }

    #[test]
    fn identity_service_contains_raw_node_id() {
        let node_id = NodeId([9; 32]);
        let service = identity_gatt_service(&node_id);

        assert_eq!(service.uuid, JACQUARD_IDENTITY_SERVICE_UUID);
        assert_eq!(service.characteristics.len(), 1);
        assert_eq!(service.characteristics[0].uuid, JACQUARD_NODE_ID_CHAR_UUID);
        assert_eq!(service.characteristics[0].value, node_id.0.to_vec());
    }

    #[test]
    fn gatt_fallback_service_exposes_identity_and_transport_characteristics() {
        let node_id = NodeId([4; 32]);
        let service = gatt_fallback_service(&node_id);

        assert_eq!(service.uuid, JACQUARD_IDENTITY_SERVICE_UUID);
        assert_eq!(service.characteristics.len(), 3);
        assert_eq!(service.characteristics[0].uuid, JACQUARD_NODE_ID_CHAR_UUID);
        assert_eq!(service.characteristics[0].value, node_id.0.to_vec());
        assert_eq!(service.characteristics[1].uuid, JACQUARD_C2P_CHAR_UUID);
        assert_eq!(service.characteristics[2].uuid, JACQUARD_P2C_CHAR_UUID);
    }

    #[test]
    fn l2cap_service_exposes_psm_characteristic() {
        let node_id = NodeId([4; 32]);
        let service = gatt_l2cap_service(&node_id, Psm(0x1001));

        assert_eq!(service.characteristics.len(), 4);
        assert_eq!(service.characteristics[3].uuid, JACQUARD_PSM_CHAR_UUID);
        assert_eq!(service.characteristics[3].value, 0x1001_u16.to_le_bytes());
    }

    #[test]
    fn parse_psm_value_reads_little_endian_psm() {
        assert_eq!(parse_psm_value(&[0x01, 0x10]), Some(Psm(0x1001)));
        assert_eq!(parse_psm_value(&[0x00, 0x00]), None);
        assert_eq!(parse_psm_value(&[0x01]), None);
    }
}
