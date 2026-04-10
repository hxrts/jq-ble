//! Tests for [`MeshTopology`] structural correctness via the authoring API.
//!
//! Covers local, direct, and multi-hop reachable delivery shapes, and verifies
//! that protocol mix must account for every authoritative transport kind.

use jacquard_core::{
    DestinationId, FactSourceClass, NodeId, Observation, OriginAuthenticationClass, RatioPermille,
    RouteId, RouteShapeVisibility, RoutingEngineId, RoutingEvidenceClass, Tick, TransportKind,
};
use jacquard_reference_client::topology as reference_topology;
use jq_node_profile::{MeshEdge, MeshNode, MeshTopologyBuildError, active_route, topology};

fn node(byte: u8) -> jacquard_core::Node {
    // recursion-exception: helper keeps the short `node` name while delegating to the reference topology builder
    reference_topology::node(byte).pathway().build()
}

fn edge(
    from_byte: u8,
    to_byte: u8,
    transport_kind: TransportKind,
    observed_at_tick: Tick,
) -> MeshEdge {
    MeshEdge::new(
        NodeId([from_byte; 32]),
        NodeId([to_byte; 32]),
        Observation {
            value: {
                let mut link = reference_topology::link(to_byte)
                    .with_confidence(RatioPermille(950))
                    .build();
                match transport_kind {
                    TransportKind::BleGatt | TransportKind::BleL2cap => {
                        link.endpoint.transport_kind = transport_kind;
                        link
                    }
                    other => panic!("unsupported test transport kind: {other:?}"),
                }
            },
            source_class: FactSourceClass::Local,
            evidence_class: RoutingEvidenceClass::DirectObservation,
            origin_authentication: OriginAuthenticationClass::Unauthenticated,
            observed_at_tick,
        },
    )
}

fn engine() -> RoutingEngineId {
    RoutingEngineId::from_contract_bytes(*b"jq-ble-demo---01")
}

#[test]
fn local_delivery_is_represented_honestly() {
    let local_node_id = NodeId([1; 32]);
    let route_id = RouteId([1; 16]);
    let topology = topology(local_node_id, Tick(7))
        .with_node(MeshNode::local(local_node_id, node(1)))
        .with_active_route(
            active_route(
                route_id,
                local_node_id,
                DestinationId::Node(local_node_id),
                local_node_id,
                engine(),
                Tick(7),
            )
            .local()
            .build(),
        )
        .build()
        .expect("build local topology");

    let active_route = topology
        .active_route(&route_id)
        .expect("local route present");
    assert!(matches!(
        active_route.delivery,
        jq_node_profile::ActiveRouteDelivery::Local
    ));
    assert!(active_route.protocol_mix.is_empty());
    assert_eq!(active_route.hop_node_ids(), vec![local_node_id]);
}

#[test]
fn direct_delivery_stays_coherent_with_ble_gatt_edge() {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([2; 32]);
    let route_id = RouteId([2; 16]);
    let topology = topology(local_node_id, Tick(9))
        .with_node(MeshNode::local(local_node_id, node(1)))
        .with_node(MeshNode::remote(remote_node_id, node(2)))
        .with_edge(edge(1, 2, TransportKind::BleGatt, Tick(9)))
        .with_active_route(
            active_route(
                route_id,
                local_node_id,
                DestinationId::Node(remote_node_id),
                remote_node_id,
                engine(),
                Tick(9),
            )
            .direct(remote_node_id)
            .with_protocol_mix(vec![TransportKind::BleGatt])
            .build(),
        )
        .build()
        .expect("build direct topology");

    let active_route = topology
        .active_route(&route_id)
        .expect("direct route present");
    assert_eq!(
        active_route.hop_node_ids(),
        vec![local_node_id, remote_node_id]
    );
    assert_eq!(active_route.protocol_mix, vec![TransportKind::BleGatt]);
    assert_eq!(
        topology
            .edge(local_node_id, remote_node_id)
            .expect("edge present")
            .transport_kind(),
        &TransportKind::BleGatt
    );
}

#[test]
fn reachable_delivery_does_not_overclaim_multi_hop_path_structure() {
    let local_node_id = NodeId([1; 32]);
    let relay_node_id = NodeId([2; 32]);
    let remote_node_id = NodeId([3; 32]);
    let route_id = RouteId([3; 16]);
    let topology = topology(local_node_id, Tick(11))
        .with_node(MeshNode::local(local_node_id, node(1)))
        .with_node(MeshNode::remote(relay_node_id, node(2)))
        .with_node(MeshNode::remote(remote_node_id, node(3)))
        .with_edge(edge(1, 2, TransportKind::BleGatt, Tick(11)))
        .with_edge(edge(2, 3, TransportKind::BleL2cap, Tick(11)))
        .with_active_route(
            active_route(
                route_id,
                local_node_id,
                DestinationId::Node(remote_node_id),
                remote_node_id,
                engine(),
                Tick(11),
            )
            .reachable(2)
            .with_shape_visibility(RouteShapeVisibility::NextHopOnly)
            .with_protocol_mix(vec![TransportKind::BleGatt, TransportKind::BleL2cap])
            .build(),
        )
        .build()
        .expect("build reachable topology");

    let active_route = topology
        .active_route(&route_id)
        .expect("reachable route present");
    assert_eq!(
        active_route.hop_node_ids(),
        vec![local_node_id, remote_node_id]
    );
    assert_eq!(
        active_route.protocol_mix,
        vec![TransportKind::BleGatt, TransportKind::BleL2cap]
    );
    assert!(matches!(
        active_route.delivery,
        jq_node_profile::ActiveRouteDelivery::Reachable { hop_count_hint: 2 }
    ));
}

#[test]
fn reachable_delivery_accepts_non_authoritative_protocol_mix_without_path_validation() {
    let local_node_id = NodeId([1; 32]);
    let relay_node_id = NodeId([2; 32]);
    let remote_node_id = NodeId([3; 32]);
    let route_id = RouteId([4; 16]);
    let topology = topology(local_node_id, Tick(13))
        .with_node(MeshNode::local(local_node_id, node(1)))
        .with_node(MeshNode::remote(relay_node_id, node(2)))
        .with_node(MeshNode::remote(remote_node_id, node(3)))
        .with_edge(edge(1, 2, TransportKind::BleGatt, Tick(13)))
        .with_edge(edge(2, 3, TransportKind::BleL2cap, Tick(13)))
        .with_active_route(
            active_route(
                route_id,
                local_node_id,
                DestinationId::Node(remote_node_id),
                remote_node_id,
                engine(),
                Tick(13),
            )
            .reachable(3)
            .with_protocol_mix(vec![TransportKind::BleGatt])
            .build(),
        )
        .build()
        .expect("reachable delivery should not validate an inferred path");

    let active_route = topology.active_route(&route_id).expect("reachable route");
    assert_eq!(active_route.protocol_mix, vec![TransportKind::BleGatt]);
}

#[test]
fn reachable_delivery_requires_a_multi_hop_hint() {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([2; 32]);
    let route_id = RouteId([5; 16]);
    let error = topology(local_node_id, Tick(15))
        .with_node(MeshNode::local(local_node_id, node(1)))
        .with_node(MeshNode::remote(remote_node_id, node(2)))
        .with_active_route(
            active_route(
                route_id,
                local_node_id,
                DestinationId::Node(remote_node_id),
                remote_node_id,
                engine(),
                Tick(15),
            )
            .reachable(1)
            .build(),
        )
        .build()
        .expect_err("reachable delivery should require more than one hop");

    assert_eq!(
        error,
        MeshTopologyBuildError::ReachableDeliveryRequiresMultiHop { route_id }
    );
}
