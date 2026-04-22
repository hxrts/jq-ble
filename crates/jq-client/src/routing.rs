//! Router assembly and BLE routing policy defaults.
//!
//! Wires the Mercator routing engine with a FixedPolicyEngine and provides
//! the initial topology, policy inputs, and default routing objective used by
//! the BLE client.

use std::collections::BTreeMap;

use jacquard_core::{
    ByteCount, Configuration, ConnectivityPosture, ControllerId, DestinationId, DurationMs,
    Environment, FactSourceClass, HealthScore, HoldFallbackPolicy, IdentityAssuranceClass, Limit,
    NodeId, Observation, OriginAuthenticationClass, PriorityPoints, RatioPermille, RouteEpoch,
    RouteError, RoutePartitionClass, RouteProtectionClass, RouteRepairClass, RouteServiceKind,
    RoutingEvidenceClass, RoutingObjective, RoutingPolicyInputs, SelectedRoutingParameters, Tick,
    TransportKind,
};
use jacquard_host_support::opaque_endpoint;
use jacquard_mem_node_profile::{NodeIdentity, NodePreset, NodePresetOptions};
use jacquard_mercator::{MERCATOR_ENGINE_ID, MercatorEngine};
use jacquard_router::{FixedPolicyEngine, MultiEngineRouter};

use crate::BleRuntimeEffects;

pub type JacquardBleRouter = MultiEngineRouter<FixedPolicyEngine, BleRuntimeEffects>;

pub(crate) fn build_router(
    local_node_id: NodeId,
    topology: Observation<Configuration>,
    effects: BleRuntimeEffects,
) -> Result<JacquardBleRouter, RouteError> {
    // Mercator is the routing engine; FixedPolicyEngine supplies the shared routing-policy profile to every engine.
    let mut router = MultiEngineRouter::new(
        local_node_id,
        FixedPolicyEngine::new(default_profile()),
        effects.clone(),
        topology.clone(),
        policy_inputs_for(&topology, local_node_id),
    );
    router.register_engine(Box::new(MercatorEngine::new(local_node_id)))?;
    Ok(router)
}

pub(crate) fn initial_topology(
    local_node_id: NodeId,
    observed_at_tick: Tick,
) -> Observation<Configuration> {
    let endpoint = opaque_endpoint(
        TransportKind::BleGatt,
        local_node_id.0.to_vec(),
        ByteCount(512),
    );
    let local_node = NodePreset::route_capable_for_engines(
        NodePresetOptions::new(
            NodeIdentity::new(local_node_id, ControllerId(local_node_id.0)),
            endpoint,
            observed_at_tick,
        ),
        &[MERCATOR_ENGINE_ID],
    )
    .build();

    Observation {
        value: Configuration {
            epoch: RouteEpoch(1),
            nodes: BTreeMap::from([(local_node_id, local_node)]),
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
        observed_at_tick,
    }
}

pub(crate) fn policy_inputs_for(
    topology: &Observation<Configuration>,
    local_node_id: NodeId,
) -> RoutingPolicyInputs {
    RoutingPolicyInputs {
        local_node: Observation {
            value: topology.value.nodes[&local_node_id].clone(),
            source_class: topology.source_class,
            evidence_class: topology.evidence_class,
            origin_authentication: topology.origin_authentication,
            observed_at_tick: topology.observed_at_tick,
        },
        local_environment: Observation {
            value: topology.value.environment.clone(),
            source_class: topology.source_class,
            evidence_class: topology.evidence_class,
            origin_authentication: topology.origin_authentication,
            observed_at_tick: topology.observed_at_tick,
        },
        routing_engine_count: 1,
        median_rtt_ms: DurationMs(40),
        loss_permille: RatioPermille(50),
        partition_risk_permille: RatioPermille(150),
        adversary_pressure_permille: RatioPermille(25),
        identity_assurance: IdentityAssuranceClass::ControllerBound,
        direct_reachability_score: HealthScore(900),
    }
}

fn default_profile() -> SelectedRoutingParameters {
    SelectedRoutingParameters {
        selected_protection: RouteProtectionClass::LinkProtected,
        selected_connectivity: ConnectivityPosture {
            repair: RouteRepairClass::Repairable,
            partition: RoutePartitionClass::ConnectedOnly,
        },
        deployment_profile: jacquard_core::OperatingMode::DenseInteractive,
        diversity_floor: jacquard_core::DiversityFloor(1),
        routing_engine_fallback_policy: jacquard_core::RoutingEngineFallbackPolicy::Forbidden,
        route_replacement_policy: jacquard_core::RouteReplacementPolicy::Allowed,
    }
}

pub(crate) fn default_objective(destination: NodeId) -> RoutingObjective {
    RoutingObjective {
        destination: DestinationId::Node(destination),
        service_kind: RouteServiceKind::Move,
        target_protection: RouteProtectionClass::LinkProtected,
        protection_floor: RouteProtectionClass::LinkProtected,
        target_connectivity: ConnectivityPosture {
            repair: RouteRepairClass::Repairable,
            partition: RoutePartitionClass::ConnectedOnly,
        },
        hold_fallback_policy: HoldFallbackPolicy::Forbidden,
        latency_budget_ms: Limit::Bounded(DurationMs(250)),
        protection_priority: PriorityPoints(10),
        connectivity_priority: PriorityPoints(20),
    }
}
