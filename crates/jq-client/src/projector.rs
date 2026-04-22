//! BLE-specific topology shaping on top of Jacquard's generic projector.
//!
//! [`TopologyProjector`] owns a local BLE-updated
//! [`Observation<Configuration>`], feeds that into
//! `jacquard_host_support::TopologyProjector`, and then projects the generic
//! snapshot into this repo's richer [`MeshTopology`] API.

use std::collections::BTreeMap;

use jacquard_core::{
    Configuration, ControllerId, DestinationId, FactSourceClass, Link, LinkRuntimeState, Node,
    NodeId, Observation, OriginAuthenticationClass, RouteShapeVisibility, RouterRoundOutcome,
    RoutingEngineCapabilities, Tick, TransportKind, TransportObservation,
};
use jacquard_host_support::{
    ObservedLink, ObservedNode, ObservedRoute, ObservedRouteShape,
    TopologyProjector as HostTopologyProjector,
};
use jacquard_mem_node_profile::{NodeIdentity, NodePreset, NodePresetOptions};
use jacquard_mercator::MERCATOR_ENGINE_ID;
use jq_node_profile::{
    ActiveRoute, ActiveRouteDelivery, MeshEdge, MeshNode, MeshNodeRole, MeshTopology,
};

pub(crate) struct TopologyProjector {
    topology: Observation<Configuration>,
    edge_observations: BTreeMap<(NodeId, NodeId), Observation<Link>>,
    host_projector: HostTopologyProjector,
    snapshot: MeshTopology,
}

impl TopologyProjector {
    #[must_use]
    pub(crate) fn new(local_node_id: NodeId, initial_topology: Observation<Configuration>) -> Self {
        // recursion-exception: constructor builds the derived snapshot while keeping the conventional `new` entrypoint
        let edge_observations = initial_topology
            .value
            .links
            .iter()
            .map(|(&(from_node_id, to_node_id), link)| {
                (
                    (from_node_id, to_node_id),
                    Observation {
                        value: link.clone(),
                        source_class: initial_topology.source_class,
                        evidence_class: initial_topology.evidence_class,
                        origin_authentication: initial_topology.origin_authentication,
                        observed_at_tick: initial_topology.observed_at_tick,
                    },
                )
            })
            .collect();
        let host_projector = HostTopologyProjector::new(local_node_id, initial_topology.clone());
        let snapshot = mesh_snapshot_from(&edge_observations, host_projector.snapshot());
        Self {
            topology: initial_topology,
            edge_observations,
            host_projector,
            snapshot,
        }
    }

    #[must_use]
    pub(crate) fn snapshot(&self) -> MeshTopology {
        self.snapshot.clone()
    }

    #[must_use]
    pub(crate) fn topology_observation(&self) -> Observation<Configuration> {
        self.topology.clone()
    }

    #[must_use]
    pub(crate) fn ingest_engine_capabilities(
        &mut self,
        capabilities: RoutingEngineCapabilities,
    ) -> bool {
        // recursion-exception: same-name delegation keeps projector updates aligned with the host-support projector surface
        self.host_projector.ingest_engine_capabilities(capabilities);
        self.refresh_snapshot()
    }

    #[must_use]
    pub(crate) fn ingest_materialized_route(
        &mut self,
        route: &jacquard_core::MaterializedRoute,
    ) -> bool {
        // recursion-exception: same-name delegation keeps projector updates aligned with the host-support projector surface
        self.host_projector.ingest_materialized_route(route);
        self.refresh_snapshot()
    }

    #[must_use]
    pub(crate) fn ingest_round_outcome(&mut self, outcome: &RouterRoundOutcome) -> bool {
        // recursion-exception: same-name delegation keeps projector updates aligned with the host-support projector surface
        self.host_projector.ingest_round_outcome(outcome);
        self.refresh_snapshot()
    }

    #[must_use]
    pub(crate) fn ingest_transport_observation(
        &mut self,
        observation: &TransportObservation,
    ) -> bool {
        // recursion-exception: same-name delegation keeps projector updates aligned with the host-support projector surface
        let changed = match observation {
            // A received payload confirms the peer is still reachable so we advance the topology clock.
            TransportObservation::PayloadReceived {
                observed_at_tick, ..
            } => self.bump_topology_tick(*observed_at_tick),
            TransportObservation::LinkObserved {
                remote_node_id,
                observation,
            } => {
                let mut changed = self.bump_topology_tick(observation.observed_at_tick);
                // Auto-register the remote node the first time we observe a link to it.
                changed |= self.ensure_remote_node(
                    *remote_node_id,
                    &observation.value.endpoint,
                    observation.observed_at_tick,
                );
                let edge_key = (self.snapshot.local_node_id, *remote_node_id);
                let previous_link = self
                    .topology
                    .value
                    .links
                    .insert(edge_key, observation.value.clone());
                let previous_observation =
                    self.edge_observations.insert(edge_key, observation.clone());
                changed |= previous_link != Some(observation.value.clone())
                    || previous_observation != Some(observation.clone());
                changed
            }
        };

        if !changed {
            return false;
        }

        // Propagate the updated local topology into the generic projector before refreshing the snapshot.
        self.host_projector.ingest_topology(self.topology.clone());
        self.refresh_snapshot()
    }

    fn refresh_snapshot(&mut self) -> bool {
        let next_snapshot =
            mesh_snapshot_from(&self.edge_observations, self.host_projector.snapshot());
        // Equality check avoids spurious topology-changed notifications when nothing materially changed.
        if next_snapshot == self.snapshot {
            return false;
        }
        self.snapshot = next_snapshot;
        true
    }

    fn bump_topology_tick(&mut self, observed_at_tick: Tick) -> bool {
        if observed_at_tick > self.topology.observed_at_tick {
            self.topology.observed_at_tick = observed_at_tick;
            true
        } else {
            false
        }
    }

    fn ensure_remote_node(
        &mut self,
        remote_node_id: NodeId,
        endpoint: &jacquard_core::LinkEndpoint,
        observed_at_tick: Tick,
    ) -> bool {
        if self.topology.value.nodes.contains_key(&remote_node_id) {
            return false;
        }

        // Synthesize a minimal node profile capable of participating in Mercator routing.
        let node = NodePreset::route_capable_for_engines(
            NodePresetOptions::new(
                // ControllerId mirrors the NodeId bytes because BLE peers are their own controllers.
                NodeIdentity::new(remote_node_id, ControllerId(remote_node_id.0)),
                endpoint.clone(),
                observed_at_tick,
            ),
            &[MERCATOR_ENGINE_ID],
        )
        .build();
        self.topology.value.nodes.insert(remote_node_id, node);
        true
    }
}

fn mesh_snapshot_from(
    edge_observations: &BTreeMap<(NodeId, NodeId), Observation<Link>>,
    generic_snapshot: &jacquard_host_support::TopologySnapshot,
) -> MeshTopology {
    let nodes = generic_snapshot
        .nodes
        .iter()
        .map(|(&node_id, node)| {
            (
                node_id,
                mesh_node_from_observed(node_id, generic_snapshot.local_node_id, node),
            )
        })
        .collect();
    let edges = generic_snapshot
        .links
        .iter()
        .map(|(&(from_node_id, to_node_id), link)| {
            (
                (from_node_id, to_node_id),
                MeshEdge::new(
                    from_node_id,
                    to_node_id,
                    // Prefer the BLE-stamped observation when available so metadata like tick is accurate.
                    edge_observation_from(edge_observations, (from_node_id, to_node_id), link),
                ),
            )
        })
        .collect();
    let mut snapshot = MeshTopology {
        local_node_id: generic_snapshot.local_node_id,
        observed_at_tick: generic_snapshot.observed_at_tick,
        nodes,
        edges,
        // Routes are projected after nodes and edges are in place because project_observed_route
        // requires both endpoints to exist in the snapshot.
        active_routes: BTreeMap::new(),
    };
    snapshot.active_routes = generic_snapshot
        .active_routes
        .values()
        .filter_map(|route| project_observed_route(&snapshot, route))
        .map(|route| (route.route_id, route))
        .collect();
    snapshot
}

fn mesh_node_from_observed(
    node_id: NodeId,
    local_node_id: NodeId,
    node: &ObservedNode,
) -> MeshNode {
    let role = if node_id == local_node_id {
        MeshNodeRole::Local
    } else {
        MeshNodeRole::Remote
    };
    MeshNode {
        node_id,
        role,
        node: Node {
            controller_id: node.controller_id,
            profile: node.profile.clone(),
            state: node.state.clone(),
        },
    }
}

fn edge_observation_from(
    edge_observations: &BTreeMap<(NodeId, NodeId), Observation<Link>>,
    key: (NodeId, NodeId),
    link: &ObservedLink,
) -> Observation<Link> {
    // Fall back to reconstructing an observation from the generic snapshot when no BLE observation exists.
    match edge_observations.get(&key) {
        Some(obs) => obs.clone(),
        None => Observation {
            value: Link {
                endpoint: link.endpoint.clone(),
                profile: link.profile.clone(),
                state: link.state.clone(),
            },
            source_class: FactSourceClass::Local,
            evidence_class: jacquard_core::RoutingEvidenceClass::DirectObservation,
            origin_authentication: OriginAuthenticationClass::Unauthenticated,
            observed_at_tick: link.observed_at_tick,
        },
    }
}

fn project_observed_route(topology: &MeshTopology, route: &ObservedRoute) -> Option<ActiveRoute> {
    let terminal_node_id = match route.destination {
        DestinationId::Node(node_id) => node_id,
        // Non-node destinations (e.g. broadcast groups) are not supported in this mesh snapshot.
        _ => return None,
    };
    let owner_node_id = route.lease.owner_node_id;
    // Skip routes whose endpoint nodes are not yet in the local topology view.
    topology.node(&terminal_node_id)?;
    topology.node(&owner_node_id)?;

    let shape_visibility = route_shape_visibility(route.route_shape.clone());
    let hop_count_hint = route.hop_count_hint.value_or(1);
    let delivery =
        derive_route_delivery(topology, owner_node_id, terminal_node_id, hop_count_hint)?;
    // Only derive the transport mix from concrete topology when the route shape is authoritative.
    let protocol_mix = path_transport_mix(topology, &delivery, owner_node_id, terminal_node_id)
        .unwrap_or_else(|| route.protocol_mix.clone());

    Some(ActiveRoute {
        route_id: route.route_id,
        owner_node_id,
        destination: route.destination.clone(),
        terminal_node_id,
        engine: route.engine_id.clone(),
        shape_visibility,
        protocol_mix,
        delivery_mode: route.delivery_mode,
        multicast_group_id: None,
        multicast_receivers: Vec::new(),
        delivery,
        // Use whichever tick is more recent to capture both health checks and lifecycle transitions.
        observed_at_tick: route
            .health
            .last_validated_at_tick
            .max(route.lifecycle_updated_at_tick),
    })
}

fn route_shape_visibility(route_shape: ObservedRouteShape) -> RouteShapeVisibility {
    match route_shape {
        ObservedRouteShape::ExplicitPath => RouteShapeVisibility::ExplicitPath,
        ObservedRouteShape::CorridorEnvelope => RouteShapeVisibility::CorridorEnvelope,
        ObservedRouteShape::NextHopOnly => RouteShapeVisibility::NextHopOnly,
        ObservedRouteShape::Opaque => RouteShapeVisibility::Opaque,
    }
}

#[must_use]
pub(crate) fn derive_route_delivery(
    topology: &MeshTopology,
    owner_node_id: NodeId,
    terminal_node_id: NodeId,
    hop_count_hint: u8,
) -> Option<ActiveRouteDelivery> {
    if owner_node_id == terminal_node_id {
        return Some(ActiveRouteDelivery::Local);
    }

    let direct_edge = topology
        .edge(owner_node_id, terminal_node_id)
        .filter(|edge| edge.observation.value.state.state != LinkRuntimeState::Faulted);
    if direct_edge.is_some() && hop_count_hint <= 1 {
        return Some(ActiveRouteDelivery::Direct {
            next_hop_node_id: terminal_node_id,
        });
    }

    Some(ActiveRouteDelivery::Reachable {
        hop_count_hint: hop_count_hint.max(1),
    })
}

#[must_use]
pub(crate) fn path_transport_mix(
    topology: &MeshTopology,
    delivery: &ActiveRouteDelivery,
    owner_node_id: NodeId,
    terminal_node_id: NodeId,
) -> Option<Vec<TransportKind>> {
    let hop_node_ids = match delivery {
        ActiveRouteDelivery::Local => vec![owner_node_id],
        ActiveRouteDelivery::Direct { .. } => vec![owner_node_id, terminal_node_id],
        ActiveRouteDelivery::Reachable { .. } => return None,
    };
    if hop_node_ids.len() <= 1 {
        return Some(Vec::new());
    }

    let mut protocol_mix = Vec::new();
    for edge in hop_node_ids.windows(2) {
        let edge = topology.edge(edge[0], edge[1])?;
        let transport_kind = edge.observation.value.endpoint.transport_kind.clone();
        if !protocol_mix.contains(&transport_kind) {
            protocol_mix.push(transport_kind);
        }
    }
    Some(protocol_mix)
}
