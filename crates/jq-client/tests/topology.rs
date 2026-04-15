//! Tests for topology projection via the client API.
//!
//! Covers node and edge discovery, GATT to L2CAP transport upgrades,
//! disconnect-like state changes, and active route surfacing after a send.

mod common;

use std::collections::BTreeMap;
use std::time::Duration;

use futures_util::StreamExt;
use jacquard_adapter::{TransportIngressClass, dispatch_mailbox};
use jacquard_batman_bellman::BATMAN_BELLMAN_ENGINE_ID;
use jacquard_core::{
    Configuration, ControllerId, Environment, FactSourceClass, LinkBuilder, LinkEndpoint,
    LinkRuntimeState, NodeId, Observation, OriginAuthenticationClass, PartitionRecoveryClass,
    RatioPermille, RepairCapability, RouteEpoch, RoutingEvidenceClass, Tick, TransportIngressEvent,
    TransportKind,
};
use jacquard_mem_node_profile::{NodeIdentity, NodePreset, NodePresetOptions};
use jacquard_pathway::PATHWAY_ENGINE_ID;
use jq_client::JacquardBleClient;

use common::{FakeTransport, TestTransportSender, ble_endpoint};

// --- Topology builders (NodePreset-based) ------------------------------------
//
// These use the full NodePreset machinery so the projector can resolve node
// profiles. They differ from the reference-client topology helpers in common.

fn local_only_topology() -> Observation<Configuration> {
    let local_node_id = NodeId([1; 32]);
    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([(
                local_node_id,
                NodePreset::route_capable_for_engines(
                    NodePresetOptions::new(
                        NodeIdentity::new(local_node_id, ControllerId(local_node_id.0)),
                        ble_endpoint(1, TransportKind::BleGatt),
                        Tick(1),
                    ),
                    &[PATHWAY_ENGINE_ID, BATMAN_BELLMAN_ENGINE_ID],
                )
                .build(),
            )]),
            links: BTreeMap::new(),
            environment: Environment {
                reachable_neighbor_count: 0,
                churn_permille: RatioPermille(0),
                contention_permille: RatioPermille(0),
            },
        },
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Controlled,
        observed_at_tick: Tick(1),
    }
}

fn published_topology() -> Observation<Configuration> {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([2; 32]);
    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([
                (
                    local_node_id,
                    NodePreset::route_capable_for_engines(
                        NodePresetOptions::new(
                            NodeIdentity::new(local_node_id, ControllerId(local_node_id.0)),
                            ble_endpoint(1, TransportKind::BleGatt),
                            Tick(1),
                        ),
                        &[PATHWAY_ENGINE_ID, BATMAN_BELLMAN_ENGINE_ID],
                    )
                    .build(),
                ),
                (
                    remote_node_id,
                    NodePreset::route_capable_for_engines(
                        NodePresetOptions::new(
                            NodeIdentity::new(remote_node_id, ControllerId(remote_node_id.0)),
                            ble_endpoint(2, TransportKind::BleGatt),
                            Tick(1),
                        ),
                        &[PATHWAY_ENGINE_ID, BATMAN_BELLMAN_ENGINE_ID],
                    )
                    .build(),
                ),
            ]),
            links: BTreeMap::from([
                ((local_node_id, remote_node_id), {
                    let mut link = LinkBuilder::new(ble_endpoint(2, TransportKind::BleGatt))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build();
                    link.state.delivery_confidence_permille =
                        jacquard_core::Belief::certain(RatioPermille(950), Tick(1));
                    link
                }),
                ((remote_node_id, local_node_id), {
                    let mut link = LinkBuilder::new(ble_endpoint(1, TransportKind::BleGatt))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build();
                    link.state.delivery_confidence_permille =
                        jacquard_core::Belief::certain(RatioPermille(950), Tick(1));
                    link
                }),
            ]),
            environment: Environment {
                reachable_neighbor_count: 1,
                churn_permille: RatioPermille(0),
                contention_permille: RatioPermille(0),
            },
        },
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Controlled,
        observed_at_tick: Tick(1),
    }
}

fn ambiguous_multi_path_topology() -> Observation<Configuration> {
    let local_node_id = NodeId([1; 32]);
    let relay_left = NodeId([2; 32]);
    let relay_right = NodeId([3; 32]);
    let remote_node_id = NodeId([4; 32]);
    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([
                (
                    local_node_id,
                    NodePreset::route_capable_for_engines(
                        NodePresetOptions::new(
                            NodeIdentity::new(local_node_id, ControllerId(local_node_id.0)),
                            ble_endpoint(1, TransportKind::BleGatt),
                            Tick(1),
                        ),
                        &[PATHWAY_ENGINE_ID, BATMAN_BELLMAN_ENGINE_ID],
                    )
                    .build(),
                ),
                (
                    relay_left,
                    NodePreset::route_capable_for_engines(
                        NodePresetOptions::new(
                            NodeIdentity::new(relay_left, ControllerId(relay_left.0)),
                            ble_endpoint(2, TransportKind::BleGatt),
                            Tick(1),
                        ),
                        &[PATHWAY_ENGINE_ID, BATMAN_BELLMAN_ENGINE_ID],
                    )
                    .build(),
                ),
                (
                    relay_right,
                    NodePreset::route_capable_for_engines(
                        NodePresetOptions::new(
                            NodeIdentity::new(relay_right, ControllerId(relay_right.0)),
                            ble_endpoint(3, TransportKind::BleL2cap),
                            Tick(1),
                        ),
                        &[PATHWAY_ENGINE_ID, BATMAN_BELLMAN_ENGINE_ID],
                    )
                    .build(),
                ),
                (
                    remote_node_id,
                    NodePreset::route_capable_for_engines(
                        NodePresetOptions::new(
                            NodeIdentity::new(remote_node_id, ControllerId(remote_node_id.0)),
                            ble_endpoint(4, TransportKind::BleGatt),
                            Tick(1),
                        ),
                        &[PATHWAY_ENGINE_ID, BATMAN_BELLMAN_ENGINE_ID],
                    )
                    .build(),
                ),
            ]),
            links: BTreeMap::from([
                (
                    (local_node_id, relay_left),
                    LinkBuilder::new(ble_endpoint(2, TransportKind::BleGatt))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build(),
                ),
                (
                    (relay_left, local_node_id),
                    LinkBuilder::new(ble_endpoint(1, TransportKind::BleGatt))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build(),
                ),
                (
                    (relay_left, remote_node_id),
                    LinkBuilder::new(ble_endpoint(4, TransportKind::BleGatt))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build(),
                ),
                (
                    (remote_node_id, relay_left),
                    LinkBuilder::new(ble_endpoint(2, TransportKind::BleGatt))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build(),
                ),
                (
                    (local_node_id, relay_right),
                    LinkBuilder::new(ble_endpoint(3, TransportKind::BleL2cap))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build(),
                ),
                (
                    (relay_right, local_node_id),
                    LinkBuilder::new(ble_endpoint(1, TransportKind::BleL2cap))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build(),
                ),
                (
                    (relay_right, remote_node_id),
                    LinkBuilder::new(ble_endpoint(4, TransportKind::BleL2cap))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build(),
                ),
                (
                    (remote_node_id, relay_right),
                    LinkBuilder::new(ble_endpoint(3, TransportKind::BleL2cap))
                        .with_profile(
                            jacquard_core::DurationMs(25),
                            RepairCapability::TransportRetransmit,
                            PartitionRecoveryClass::LocalReconnect,
                        )
                        .with_runtime_state(LinkRuntimeState::Active)
                        .build(),
                ),
            ]),
            environment: Environment {
                reachable_neighbor_count: 2,
                churn_permille: RatioPermille(0),
                contention_permille: RatioPermille(0),
            },
        },
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Controlled,
        observed_at_tick: Tick(1),
    }
}

fn link_observed(
    remote_node_id: NodeId,
    endpoint: LinkEndpoint,
    runtime_state: LinkRuntimeState,
) -> TransportIngressEvent {
    TransportIngressEvent::LinkObserved {
        remote_node_id,
        link: LinkBuilder::new(endpoint)
            .with_profile(
                jacquard_core::DurationMs(25),
                RepairCapability::TransportRetransmit,
                PartitionRecoveryClass::LocalReconnect,
            )
            .with_runtime_state(runtime_state)
            .build(),
        source_class: FactSourceClass::Local,
        evidence_class: RoutingEvidenceClass::DirectObservation,
        origin_authentication: OriginAuthenticationClass::Unauthenticated,
    }
}

#[tokio::test]
async fn topology_api_tracks_discovery_upgrade_and_disconnect_like_events() {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([9; 32]);
    let (outbound_tx, outbound_rx) = dispatch_mailbox(16);
    let transport_sender = TestTransportSender {
        outbound: outbound_tx,
    };
    let transport = FakeTransport::new(outbound_rx);
    let ingress_sender = transport.ingress_sender.clone();
    let client = JacquardBleClient::new_with_transport_for_testing(
        local_node_id,
        local_only_topology(),
        transport,
        transport_sender,
    );

    assert_eq!(client.topology().nodes.len(), 1);

    let mut stream = Box::pin(client.topology_stream());
    let initial = tokio::time::timeout(Duration::from_millis(200), stream.next())
        .await
        .expect("initial topology tick")
        .expect("initial topology");
    assert_eq!(initial.nodes.len(), 1);

    ingress_sender
        .emit(
            TransportIngressClass::Control,
            link_observed(
                remote_node_id,
                ble_endpoint(9, TransportKind::BleGatt),
                LinkRuntimeState::Active,
            ),
        )
        .expect("emit gatt link");

    let gatt_snapshot = tokio::time::timeout(Duration::from_millis(400), stream.next())
        .await
        .expect("gatt topology update")
        .expect("gatt snapshot");
    assert!(gatt_snapshot.node(&remote_node_id).is_some());
    assert_eq!(
        gatt_snapshot
            .edge(local_node_id, remote_node_id)
            .expect("gatt edge")
            .transport_kind(),
        &TransportKind::BleGatt
    );
    assert_eq!(
        client
            .topology()
            .edge(local_node_id, remote_node_id)
            .expect("gatt edge")
            .observation
            .value
            .profile
            .latency_floor_ms,
        jacquard_core::DurationMs(25)
    );

    ingress_sender
        .emit(
            TransportIngressClass::Control,
            link_observed(
                remote_node_id,
                ble_endpoint(9, TransportKind::BleL2cap),
                LinkRuntimeState::Active,
            ),
        )
        .expect("emit l2cap link");
    let l2cap_snapshot = tokio::time::timeout(Duration::from_millis(400), stream.next())
        .await
        .expect("l2cap topology update")
        .expect("l2cap snapshot");
    assert_eq!(
        l2cap_snapshot
            .edge(local_node_id, remote_node_id)
            .expect("l2cap edge")
            .transport_kind(),
        &TransportKind::BleL2cap
    );

    ingress_sender
        .emit(
            TransportIngressClass::Control,
            link_observed(
                remote_node_id,
                ble_endpoint(9, TransportKind::BleGatt),
                LinkRuntimeState::Degraded,
            ),
        )
        .expect("emit downgraded link");
    let downgraded_snapshot = tokio::time::timeout(Duration::from_millis(400), stream.next())
        .await
        .expect("downgraded topology update")
        .expect("downgraded snapshot");
    let downgraded_edge = downgraded_snapshot
        .edge(local_node_id, remote_node_id)
        .expect("downgraded edge");
    assert_eq!(downgraded_edge.transport_kind(), &TransportKind::BleGatt);
    assert_eq!(
        downgraded_edge.observation.value.state.state,
        LinkRuntimeState::Degraded
    );
}

#[tokio::test]
async fn topology_snapshot_surfaces_active_route_after_send() {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([2; 32]);
    let (outbound_tx, outbound_rx) = dispatch_mailbox(16);
    let transport_sender = TestTransportSender {
        outbound: outbound_tx,
    };
    let transport = FakeTransport::new(outbound_rx);
    let client = JacquardBleClient::new_with_transport_for_testing(
        local_node_id,
        published_topology(),
        transport,
        transport_sender,
    );
    let mut stream = Box::pin(client.topology_stream());
    let _initial = stream.next().await.expect("initial snapshot");

    client
        .send(remote_node_id, b"route me")
        .await
        .expect("send");

    let updated = tokio::time::timeout(Duration::from_millis(400), stream.next())
        .await
        .expect("active route update")
        .expect("updated snapshot");
    let active_route = updated
        .active_routes
        .values()
        .next()
        .expect("active route present");
    assert_eq!(active_route.terminal_node_id, remote_node_id);
    assert_eq!(active_route.protocol_mix, vec![TransportKind::BleGatt]);
    assert!(matches!(
        active_route.delivery,
        jq_node_profile::ActiveRouteDelivery::Direct { next_hop_node_id }
            if next_hop_node_id == remote_node_id
    ));
}

#[tokio::test]
async fn topology_snapshot_does_not_overclaim_an_ambiguous_multi_hop_route() {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([4; 32]);
    let (outbound_tx, outbound_rx) = dispatch_mailbox(16);
    let transport_sender = TestTransportSender {
        outbound: outbound_tx,
    };
    let transport = FakeTransport::new(outbound_rx);
    let client = JacquardBleClient::new_with_transport_for_testing(
        local_node_id,
        ambiguous_multi_path_topology(),
        transport,
        transport_sender,
    );
    let mut stream = Box::pin(client.topology_stream());
    let _initial = stream.next().await.expect("initial snapshot");

    client
        .send(remote_node_id, b"route me carefully")
        .await
        .expect("send over ambiguous graph");

    let updated = tokio::time::timeout(Duration::from_millis(400), stream.next())
        .await
        .expect("active route update")
        .expect("updated snapshot");
    let active_route = updated
        .active_routes
        .values()
        .next()
        .expect("active route present");
    assert_eq!(active_route.terminal_node_id, remote_node_id);
    assert!(matches!(
        active_route.delivery,
        jq_node_profile::ActiveRouteDelivery::Reachable { hop_count_hint }
            if hop_count_hint >= 2
    ));
}
