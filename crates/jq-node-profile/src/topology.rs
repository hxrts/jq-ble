//! Stable snapshot types for the live mesh topology.
//!
//! [`MeshTopology`] holds the current view of nodes, directed edges, and
//! active routes. [`ActiveRouteDelivery`] distinguishes authoritative local
//! and direct delivery from multi-hop reachability that does not expose a
//! selected path. [`MeshTopologyBuildError`] covers all structural constraint
//! violations caught at build time.

use std::collections::BTreeMap;

use jacquard_core::{
    DestinationId, Link, Node, NodeId, Observation, RouteId, RouteShapeVisibility, RoutingEngineId,
    Tick, TransportKind,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MeshNodeRole {
    Local,
    Remote,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshNode {
    pub node_id: NodeId,
    pub role: MeshNodeRole,
    pub node: Node,
}

impl MeshNode {
    #[must_use]
    pub fn local(node_id: NodeId, node: Node) -> Self {
        Self {
            node_id,
            role: MeshNodeRole::Local,
            node,
        }
    }

    #[must_use]
    pub fn remote(node_id: NodeId, node: Node) -> Self {
        Self {
            node_id,
            role: MeshNodeRole::Remote,
            node,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshEdge {
    pub from_node_id: NodeId,
    pub to_node_id: NodeId,
    pub observation: Observation<Link>,
}

impl MeshEdge {
    #[must_use]
    pub fn new(from_node_id: NodeId, to_node_id: NodeId, observation: Observation<Link>) -> Self {
        Self {
            from_node_id,
            to_node_id,
            observation,
        }
    }

    #[must_use]
    pub fn transport_kind(&self) -> &TransportKind {
        &self.observation.value.endpoint.transport_kind
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActiveRouteDelivery {
    Local,
    Direct { next_hop_node_id: NodeId },
    Reachable { hop_count_hint: u8 },
}

impl ActiveRouteDelivery {
    // Returns the authoritative ordered hops when the delivery structure is known.
    fn hop_node_ids(&self, owner_node_id: NodeId, terminal_node_id: NodeId) -> Vec<NodeId> {
        match self {
            Self::Local => vec![owner_node_id],
            Self::Direct { .. } => vec![owner_node_id, terminal_node_id],
            Self::Reachable { .. } => vec![owner_node_id, terminal_node_id],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActiveRoute {
    pub route_id: RouteId,
    // The node that owns and operates this route (typically the local node).
    pub owner_node_id: NodeId,
    pub destination: DestinationId,
    // The final destination node; equals owner_node_id only for Local delivery.
    pub terminal_node_id: NodeId,
    pub engine: RoutingEngineId,
    // How much of the path shape is visible in this snapshot (ExplicitPath, NextHopOnly, etc).
    pub shape_visibility: RouteShapeVisibility,
    // Deduplicated list of transport kinds used across all hops of the route.
    pub protocol_mix: Vec<TransportKind>,
    pub delivery: ActiveRouteDelivery,
    pub observed_at_tick: Tick,
}

impl ActiveRoute {
    #[must_use]
    pub fn hop_node_ids(&self) -> Vec<NodeId> {
        self.delivery
            .hop_node_ids(self.owner_node_id, self.terminal_node_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeshTopology {
    pub local_node_id: NodeId,
    pub observed_at_tick: Tick,
    pub nodes: BTreeMap<NodeId, MeshNode>,
    pub edges: BTreeMap<(NodeId, NodeId), MeshEdge>,
    pub active_routes: BTreeMap<RouteId, ActiveRoute>,
}

impl MeshTopology {
    #[must_use]
    pub fn node(&self, node_id: &NodeId) -> Option<&MeshNode> {
        self.nodes.get(node_id)
    }

    #[must_use]
    pub fn edge(&self, from_node_id: NodeId, to_node_id: NodeId) -> Option<&MeshEdge> {
        self.edges.get(&(from_node_id, to_node_id))
    }

    #[must_use]
    pub fn active_route(&self, route_id: &RouteId) -> Option<&ActiveRoute> {
        self.active_routes.get(route_id)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error, Serialize, Deserialize)]
pub enum MeshTopologyBuildError {
    #[error("local node {node_id:?} is missing from the topology")]
    MissingLocalNode { node_id: NodeId },
    #[error("node {node_id:?} appears more than once in the topology")]
    DuplicateNode { node_id: NodeId },
    #[error("edge {from_node_id:?}->{to_node_id:?} appears more than once in the topology")]
    DuplicateEdge {
        from_node_id: NodeId,
        to_node_id: NodeId,
    },
    #[error("edge {from_node_id:?}->{to_node_id:?} references unknown node {missing_node_id:?}")]
    EdgeReferencesUnknownNode {
        from_node_id: NodeId,
        to_node_id: NodeId,
        missing_node_id: NodeId,
    },
    #[error("route {route_id:?} appears more than once in the topology")]
    DuplicateRoute { route_id: RouteId },
    #[error("route {route_id:?} owner node {owner_node_id:?} is missing")]
    RouteOwnerMissing {
        route_id: RouteId,
        owner_node_id: NodeId,
    },
    #[error("route {route_id:?} terminal node {terminal_node_id:?} is missing")]
    RouteTerminalMissing {
        route_id: RouteId,
        terminal_node_id: NodeId,
    },
    #[error("route {route_id:?} hop node {node_id:?} is missing")]
    RouteHopNodeMissing { route_id: RouteId, node_id: NodeId },
    #[error("route {route_id:?} local delivery must terminate at the owner node")]
    LocalDeliveryTargetsRemote {
        route_id: RouteId,
        owner_node_id: NodeId,
        terminal_node_id: NodeId,
    },
    #[error("route {route_id:?} local delivery must not advertise transport protocols")]
    LocalDeliveryHasProtocolMix { route_id: RouteId },
    #[error("route {route_id:?} direct delivery must target the terminal node as next hop")]
    DirectDeliveryNextHopMismatch {
        route_id: RouteId,
        next_hop_node_id: NodeId,
        terminal_node_id: NodeId,
    },
    #[error("route {route_id:?} reachable delivery collapses to a direct route")]
    ReachableDeliveryCollapsesToDirect { route_id: RouteId },
    #[error("route {route_id:?} requires edge {from_node_id:?}->{to_node_id:?}")]
    RouteMissingEdge {
        route_id: RouteId,
        from_node_id: NodeId,
        to_node_id: NodeId,
    },
    #[error("route {route_id:?} omits transport {transport_kind:?} from its protocol mix")]
    RouteProtocolMixMissingTransport {
        route_id: RouteId,
        transport_kind: TransportKind,
    },
    #[error("route {route_id:?} reachable delivery requires a multi-hop hint")]
    ReachableDeliveryRequiresMultiHop { route_id: RouteId },
}
