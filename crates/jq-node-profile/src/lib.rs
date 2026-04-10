//! Node and topology snapshot models for Jacquard BLE clients.
//!
//! Most callers should start with [`topology`] and [`active_route`]. The lower-
//! level snapshot types remain available when a client projection already owns
//! concrete Jacquard `Node`, `Link`, and route identity values.
//!
//! Module map:
//! - [`authoring`]: builder-style topology authoring helpers
//! - [`topology`]: stable snapshot structs for nodes, edges, and active routes
//!
//! Ownership:
//! - owns node/topology-facing snapshot models and authoring only
//! - never owns BLE runtime handles, bridge cadence, or router advancement

#![forbid(unsafe_code)]

pub mod authoring;
pub mod topology;

pub use authoring::{ActiveRouteBuilder, MeshTopologyBuilder, active_route, topology};
pub use topology::{
    ActiveRoute, ActiveRouteDelivery, MeshEdge, MeshNode, MeshNodeRole, MeshTopology,
    MeshTopologyBuildError,
};
