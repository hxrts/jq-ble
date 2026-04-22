//! Client payload framing helpers.
//!
//! Wraps outbound client payloads with an 8-byte magic prefix so the receiver
//! can distinguish framed client traffic from raw transport observations.

const CLIENT_PAYLOAD_MAGIC: &[u8; 8] = b"JQCLIENT";

pub(crate) fn encode_client_payload(payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(CLIENT_PAYLOAD_MAGIC.len() + payload.len());
    framed.extend_from_slice(CLIENT_PAYLOAD_MAGIC);
    framed.extend_from_slice(payload);
    framed
}

pub(crate) fn decode_client_payload(payload: &[u8]) -> Option<&[u8]> {
    payload
        .starts_with(CLIENT_PAYLOAD_MAGIC)
        .then_some(&payload[CLIENT_PAYLOAD_MAGIC.len()..])
}
