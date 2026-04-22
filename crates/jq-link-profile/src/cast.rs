//! BLE delivery-support shaping through `jacquard-cast-support`.

use jacquard_cast_support::{
    CastDeliveryObjective, CastDeliveryPolicy, CastEvidenceMeta, UnicastDeliverySupport,
    UnicastObservation, shape_unicast_delivery_support, shape_unicast_evidence,
};
use jacquard_core::{
    DurationMs, FactSourceClass, Link, LinkBuilder, LinkEndpoint, LinkRuntimeState, NodeId,
    Observation, OrderStamp, OriginAuthenticationClass, PartitionRecoveryClass, RatioPermille,
    RepairCapability, RoutingEvidenceClass, Tick, TransportIngressEvent, TransportKind,
    TransportObservation,
};
use jacquard_mem_link_profile::SimulatedLinkProfile;

const BLE_LINK_RTT_ESTIMATE_MS: DurationMs = DurationMs(150);
const BLE_LINK_VALID_FOR_MS: DurationMs = DurationMs(30_000);
const BLE_GATT_CONFIDENCE: RatioPermille = RatioPermille(700);
const BLE_L2CAP_CONFIDENCE: RatioPermille = RatioPermille(900);

#[must_use]
fn cast_evidence_meta(observed_at_tick: Tick, order: OrderStamp) -> CastEvidenceMeta {
    CastEvidenceMeta::new(
        observed_at_tick,
        DurationMs(0),
        BLE_LINK_VALID_FOR_MS,
        order,
    )
}

#[must_use]
fn shape_ble_unicast_delivery_support(
    local_node_id: NodeId,
    remote_node_id: NodeId,
    endpoint: &LinkEndpoint,
    meta: CastEvidenceMeta,
    policy: CastDeliveryPolicy,
) -> Option<UnicastDeliverySupport> {
    let observation = unicast_observation(local_node_id, remote_node_id, endpoint, meta);
    let (evidence, _) = shape_unicast_evidence([observation], policy.evidence);
    let objective = CastDeliveryObjective::unicast(local_node_id, remote_node_id);
    let (support, _) = shape_unicast_delivery_support(evidence.iter(), &objective, policy);
    support.into_iter().next()
}

#[must_use]
pub fn link_observation_from_ble_event(
    local_node_id: NodeId,
    event: TransportIngressEvent,
    observed_at_tick: Tick,
    order: OrderStamp,
) -> TransportObservation {
    let TransportIngressEvent::LinkObserved {
        remote_node_id,
        link,
        source_class,
        evidence_class,
        origin_authentication,
    } = event
    else {
        return event.observe_at(observed_at_tick);
    };

    let meta = cast_evidence_meta(observed_at_tick, order);
    let endpoint = link.endpoint;
    let support = shape_ble_unicast_delivery_support(
        local_node_id,
        remote_node_id,
        &endpoint,
        meta,
        CastDeliveryPolicy::default(),
    )
    .expect("default BLE cast policy accepts resolved GATT and L2CAP endpoints");
    // Keep BLE cast support as ordinary topology input. Mercator consumes the
    // resulting link observation through the router rather than receiving
    // engine-private evidence from the async BLE owner.
    let link = link_from_unicast_delivery_support(endpoint, support, observed_at_tick);
    TransportObservation::LinkObserved {
        remote_node_id,
        observation: Observation {
            value: link,
            source_class,
            evidence_class,
            origin_authentication,
            observed_at_tick,
        },
    }
}

#[must_use]
pub(crate) fn raw_ble_link_observed_event(
    remote_node_id: NodeId,
    endpoint: LinkEndpoint,
) -> TransportIngressEvent {
    TransportIngressEvent::LinkObserved {
        remote_node_id,
        link: link_carrier(endpoint),
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Unauthenticated,
    }
}

#[must_use]
fn unicast_observation(
    local_node_id: NodeId,
    remote_node_id: NodeId,
    endpoint: &LinkEndpoint,
    meta: CastEvidenceMeta,
) -> UnicastObservation {
    let (directional_confidence_permille, reverse_confirmation_permille) =
        match endpoint.transport_kind {
            TransportKind::BleL2cap => (BLE_L2CAP_CONFIDENCE, Some(BLE_L2CAP_CONFIDENCE)),
            TransportKind::BleGatt => (BLE_GATT_CONFIDENCE, None),
            _ => (BLE_GATT_CONFIDENCE, None),
        };
    UnicastObservation {
        from: local_node_id,
        to: remote_node_id,
        directional_confidence_permille,
        reverse_confirmation_permille,
        payload_bytes_max: endpoint.mtu_bytes,
        meta,
    }
}

#[must_use]
fn link_from_unicast_delivery_support(
    mut endpoint: LinkEndpoint,
    support: UnicastDeliverySupport,
    observed_at_tick: Tick,
) -> Link {
    endpoint.mtu_bytes = support.payload_bytes_max;
    SimulatedLinkProfile::new(endpoint)
        .with_profile(
            BLE_LINK_RTT_ESTIMATE_MS,
            RepairCapability::TransportRetransmit,
            PartitionRecoveryClass::LocalReconnect,
        )
        .with_runtime_state(LinkRuntimeState::Active)
        .with_runtime_observation(
            BLE_LINK_RTT_ESTIMATE_MS,
            0,
            BLE_LINK_VALID_FOR_MS,
            observed_at_tick,
        )
        .with_quality(
            RatioPermille(0),
            support.confidence_permille,
            support.bidirectional_confidence_permille,
        )
        .build()
}

#[must_use]
fn link_carrier(endpoint: LinkEndpoint) -> Link {
    LinkBuilder::new(endpoint)
        .with_profile(
            BLE_LINK_RTT_ESTIMATE_MS,
            RepairCapability::TransportRetransmit,
            PartitionRecoveryClass::LocalReconnect,
        )
        .with_runtime_state(LinkRuntimeState::Active)
        .build()
}

#[cfg(test)]
mod tests {
    use jacquard_cast_support::{CastEvidenceBounds, CastEvidencePolicy};
    use jacquard_core::{ByteCount, EndpointLocator, TransportKind};

    use super::*;

    fn endpoint(kind: TransportKind, mtu: u64) -> LinkEndpoint {
        LinkEndpoint::new(
            kind,
            EndpointLocator::ScopedBytes {
                scope: "ble".into(),
                bytes: vec![1],
            },
            ByteCount(mtu),
        )
    }

    fn support_for(
        endpoint: &LinkEndpoint,
        meta: CastEvidenceMeta,
        policy: CastDeliveryPolicy,
    ) -> Option<UnicastDeliverySupport> {
        shape_ble_unicast_delivery_support(NodeId([1; 32]), NodeId([2; 32]), endpoint, meta, policy)
    }

    #[test]
    fn ble_sessions_map_to_unicast_support() {
        for (kind, mtu, confidence, bidirectional) in [
            (
                TransportKind::BleGatt,
                512,
                BLE_GATT_CONFIDENCE,
                RatioPermille(0),
            ),
            (
                TransportKind::BleL2cap,
                1472,
                BLE_L2CAP_CONFIDENCE,
                BLE_L2CAP_CONFIDENCE,
            ),
        ] {
            let endpoint = endpoint(kind, mtu);
            let support = support_for(
                &endpoint,
                cast_evidence_meta(Tick(7), OrderStamp(1)),
                CastDeliveryPolicy::default(),
            )
            .expect("support");

            assert_eq!(support.sender, NodeId([1; 32]));
            assert_eq!(support.receiver, NodeId([2; 32]));
            assert_eq!(support.payload_bytes_max, ByteCount(mtu));
            assert_eq!(support.confidence_permille, confidence);
            assert_eq!(support.bidirectional_confidence_permille, bidirectional);
        }
    }

    #[test]
    fn stale_low_confidence_and_insufficient_mtu_evidence_are_omitted() {
        let endpoint = endpoint(TransportKind::BleGatt, 32);
        let strict = CastDeliveryPolicy {
            evidence: CastEvidencePolicy {
                bounds: CastEvidenceBounds {
                    evidence_age_ms_max: DurationMs(1),
                    ..CastEvidenceBounds::default()
                },
                confidence_floor: RatioPermille(900),
                payload_bytes_required: ByteCount(512),
            },
            ..CastDeliveryPolicy::default()
        };

        let stale_meta =
            CastEvidenceMeta::new(Tick(7), DurationMs(2), DurationMs(30_000), OrderStamp(3));
        assert!(support_for(&endpoint, stale_meta, strict).is_none());

        let fresh_meta = cast_evidence_meta(Tick(7), OrderStamp(4));
        assert!(support_for(&endpoint, fresh_meta, strict).is_none());
    }
}
