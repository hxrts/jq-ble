//! Builder helpers for constructing [`MeshTopology`] and [`ActiveRoute`]
//! values in tests and application code.
//!
//! [`topology`] returns a [`MeshTopologyBuilder`] and [`active_route`] returns
//! an [`ActiveRouteBuilder`]. Both validate structural invariants (no dangling
//! edges, no collapsed routes) on [`build`](MeshTopologyBuilder::build).

use std::collections::BTreeMap;

use jacquard_core::{
    DestinationId, NodeId, RouteId, RouteShapeVisibility, RoutingEngineId, Tick, TransportKind,
};

use crate::topology::{
    ActiveRoute, ActiveRouteDelivery, MeshEdge, MeshNode, MeshTopology, MeshTopologyBuildError,
};

#[must_use]
pub fn topology(local_node_id: NodeId, observed_at_tick: Tick) -> MeshTopologyBuilder {
    MeshTopologyBuilder::new(local_node_id, observed_at_tick)
}

#[must_use]
pub fn active_route(
    route_id: RouteId,
    owner_node_id: NodeId,
    destination: DestinationId,
    terminal_node_id: NodeId,
    engine: RoutingEngineId,
    observed_at_tick: Tick,
) -> ActiveRouteBuilder {
    ActiveRouteBuilder::new(
        route_id,
        owner_node_id,
        destination,
        terminal_node_id,
        engine,
        observed_at_tick,
    )
}

#[derive(Clone, Debug)]
pub struct ActiveRouteBuilder {
    route_id: RouteId,
    owner_node_id: NodeId,
    destination: DestinationId,
    terminal_node_id: NodeId,
    engine: RoutingEngineId,
    shape_visibility: RouteShapeVisibility,
    protocol_mix: Vec<TransportKind>,
    delivery: ActiveRouteDelivery,
    observed_at_tick: Tick,
}

impl ActiveRouteBuilder {
    #[must_use]
    pub fn new(
        route_id: RouteId,
        owner_node_id: NodeId,
        destination: DestinationId,
        terminal_node_id: NodeId,
        engine: RoutingEngineId,
        observed_at_tick: Tick,
    ) -> Self {
        // Infer the most specific delivery type possible from the owner/terminal relationship.
        let delivery = if owner_node_id == terminal_node_id {
            ActiveRouteDelivery::Local
        } else {
            ActiveRouteDelivery::Direct {
                next_hop_node_id: terminal_node_id,
            }
        };
        Self {
            route_id,
            owner_node_id,
            destination,
            terminal_node_id,
            engine,
            // ExplicitPath is the default so validation checks edges and protocol_mix by default.
            shape_visibility: RouteShapeVisibility::ExplicitPath,
            protocol_mix: Vec::new(),
            delivery,
            observed_at_tick,
        }
    }

    #[must_use]
    pub fn local(mut self) -> Self {
        self.delivery = ActiveRouteDelivery::Local;
        // Local delivery uses no transport hops so the protocol_mix must be empty for validation.
        self.protocol_mix.clear();
        self
    }

    #[must_use]
    pub fn direct(mut self, next_hop_node_id: NodeId) -> Self {
        self.delivery = ActiveRouteDelivery::Direct { next_hop_node_id };
        self
    }

    #[must_use]
    pub fn reachable(mut self, hop_count_hint: u8) -> Self {
        self.delivery = ActiveRouteDelivery::Reachable {
            hop_count_hint: hop_count_hint.max(1),
        };
        self
    }

    #[must_use]
    pub fn with_protocol_mix(mut self, protocol_mix: Vec<TransportKind>) -> Self {
        self.protocol_mix = protocol_mix;
        self
    }

    #[must_use]
    pub fn with_shape_visibility(mut self, shape_visibility: RouteShapeVisibility) -> Self {
        self.shape_visibility = shape_visibility;
        self
    }

    #[must_use]
    pub fn build(self) -> ActiveRoute {
        ActiveRoute {
            route_id: self.route_id,
            owner_node_id: self.owner_node_id,
            destination: self.destination,
            terminal_node_id: self.terminal_node_id,
            engine: self.engine,
            shape_visibility: self.shape_visibility,
            protocol_mix: self.protocol_mix,
            delivery: self.delivery,
            observed_at_tick: self.observed_at_tick,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MeshTopologyBuilder {
    local_node_id: NodeId,
    observed_at_tick: Tick,
    nodes: Vec<MeshNode>,
    edges: Vec<MeshEdge>,
    active_routes: Vec<ActiveRoute>,
}

impl MeshTopologyBuilder {
    #[must_use]
    pub fn new(local_node_id: NodeId, observed_at_tick: Tick) -> Self {
        Self {
            local_node_id,
            observed_at_tick,
            nodes: Vec::new(),
            edges: Vec::new(),
            active_routes: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_node(mut self, node: MeshNode) -> Self {
        self.nodes.push(node);
        self
    }

    #[must_use]
    pub fn with_edge(mut self, edge: MeshEdge) -> Self {
        self.edges.push(edge);
        self
    }

    #[must_use]
    pub fn with_active_route(mut self, active_route: ActiveRoute) -> Self {
        self.active_routes.push(active_route);
        self
    }

    pub fn build(self) -> Result<MeshTopology, MeshTopologyBuildError> {
        let nodes = collect_nodes(self.nodes, self.local_node_id)?;
        let edges = collect_edges(self.edges, &nodes)?;
        let active_routes = collect_routes(self.active_routes, &nodes, &edges)?;
        Ok(MeshTopology {
            local_node_id: self.local_node_id,
            observed_at_tick: self.observed_at_tick,
            nodes,
            edges,
            active_routes,
        })
    }
}

fn collect_nodes(
    nodes: Vec<MeshNode>,
    local_node_id: NodeId,
) -> Result<BTreeMap<NodeId, MeshNode>, MeshTopologyBuildError> {
    let mut map = BTreeMap::new();
    for node in nodes {
        let node_id = node.node_id;
        if map.insert(node_id, node).is_some() {
            return Err(MeshTopologyBuildError::DuplicateNode { node_id });
        }
    }
    // The local node must exist and have the Local role for the topology to be usable.
    let Some(local_node) = map.get(&local_node_id) else {
        return Err(MeshTopologyBuildError::MissingLocalNode {
            node_id: local_node_id,
        });
    };
    if local_node.role != crate::topology::MeshNodeRole::Local {
        return Err(MeshTopologyBuildError::MissingLocalNode {
            node_id: local_node_id,
        });
    }
    Ok(map)
}

fn collect_edges(
    edges: Vec<MeshEdge>,
    nodes: &BTreeMap<NodeId, MeshNode>,
) -> Result<BTreeMap<(NodeId, NodeId), MeshEdge>, MeshTopologyBuildError> {
    let mut map = BTreeMap::new();
    for edge in edges {
        // Validate both endpoints before inserting to catch dangling references early.
        for missing_node_id in [edge.from_node_id, edge.to_node_id] {
            if !nodes.contains_key(&missing_node_id) {
                return Err(MeshTopologyBuildError::EdgeReferencesUnknownNode {
                    from_node_id: edge.from_node_id,
                    to_node_id: edge.to_node_id,
                    missing_node_id,
                });
            }
        }
        let key = (edge.from_node_id, edge.to_node_id);
        if map.insert(key, edge).is_some() {
            return Err(MeshTopologyBuildError::DuplicateEdge {
                from_node_id: key.0,
                to_node_id: key.1,
            });
        }
    }
    Ok(map)
}

fn collect_routes(
    routes: Vec<ActiveRoute>,
    nodes: &BTreeMap<NodeId, MeshNode>,
    edges: &BTreeMap<(NodeId, NodeId), MeshEdge>,
) -> Result<BTreeMap<RouteId, ActiveRoute>, MeshTopologyBuildError> {
    let mut map = BTreeMap::new();
    for active_route in routes {
        let route_id = active_route.route_id;
        // Full structural validation (hop nodes, edges, protocol_mix) runs here before insertion.
        validate_route(&active_route, nodes, edges)?;
        if map.insert(route_id, active_route).is_some() {
            return Err(MeshTopologyBuildError::DuplicateRoute { route_id });
        }
    }
    Ok(map)
}

fn validate_route(
    active_route: &ActiveRoute,
    nodes: &BTreeMap<NodeId, MeshNode>,
    edges: &BTreeMap<(NodeId, NodeId), MeshEdge>,
) -> Result<(), MeshTopologyBuildError> {
    if !nodes.contains_key(&active_route.owner_node_id) {
        return Err(MeshTopologyBuildError::RouteOwnerMissing {
            route_id: active_route.route_id,
            owner_node_id: active_route.owner_node_id,
        });
    }
    if !nodes.contains_key(&active_route.terminal_node_id) {
        return Err(MeshTopologyBuildError::RouteTerminalMissing {
            route_id: active_route.route_id,
            terminal_node_id: active_route.terminal_node_id,
        });
    }

    match &active_route.delivery {
        ActiveRouteDelivery::Local => {
            // Local delivery requires owner == terminal so no actual hop occurs.
            if active_route.owner_node_id != active_route.terminal_node_id {
                return Err(MeshTopologyBuildError::LocalDeliveryTargetsRemote {
                    route_id: active_route.route_id,
                    owner_node_id: active_route.owner_node_id,
                    terminal_node_id: active_route.terminal_node_id,
                });
            }
            if !active_route.protocol_mix.is_empty() {
                return Err(MeshTopologyBuildError::LocalDeliveryHasProtocolMix {
                    route_id: active_route.route_id,
                });
            }
            Ok(())
        }
        ActiveRouteDelivery::Direct { next_hop_node_id } => {
            // Direct delivery must name the terminal node as the next hop; anything else is a Routed route.
            if *next_hop_node_id != active_route.terminal_node_id {
                return Err(MeshTopologyBuildError::DirectDeliveryNextHopMismatch {
                    route_id: active_route.route_id,
                    next_hop_node_id: *next_hop_node_id,
                    terminal_node_id: active_route.terminal_node_id,
                });
            }
            validate_path(active_route, &active_route.hop_node_ids(), nodes, edges)
        }
        ActiveRouteDelivery::Reachable { hop_count_hint } => {
            if *hop_count_hint <= 1 {
                return Err(MeshTopologyBuildError::ReachableDeliveryRequiresMultiHop {
                    route_id: active_route.route_id,
                });
            }
            if active_route.owner_node_id == active_route.terminal_node_id {
                return Err(MeshTopologyBuildError::ReachableDeliveryCollapsesToDirect {
                    route_id: active_route.route_id,
                });
            }
            Ok(())
        }
    }
}

fn validate_path(
    active_route: &ActiveRoute,
    hop_node_ids: &[NodeId],
    nodes: &BTreeMap<NodeId, MeshNode>,
    edges: &BTreeMap<(NodeId, NodeId), MeshEdge>,
) -> Result<(), MeshTopologyBuildError> {
    for node_id in hop_node_ids {
        if !nodes.contains_key(node_id) {
            return Err(MeshTopologyBuildError::RouteHopNodeMissing {
                route_id: active_route.route_id,
                node_id: *node_id,
            });
        }
    }

    for pair in hop_node_ids.windows(2) {
        let from_node_id = pair[0];
        let to_node_id = pair[1];
        // Every consecutive hop pair must have a corresponding edge in the topology.
        let Some(edge) = edges.get(&(from_node_id, to_node_id)) else {
            return Err(MeshTopologyBuildError::RouteMissingEdge {
                route_id: active_route.route_id,
                from_node_id,
                to_node_id,
            });
        };

        // Each hop's transport kind must appear in the route's protocol_mix declaration.
        let transport_kind = edge.transport_kind();
        if !active_route.protocol_mix.contains(transport_kind) {
            return Err(MeshTopologyBuildError::RouteProtocolMixMissingTransport {
                route_id: active_route.route_id,
                transport_kind: transport_kind.clone(),
            });
        }
    }

    Ok(())
}
