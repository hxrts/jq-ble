//! Contract tests verifying that the reference client topology and router trait
//! bounds match the assumptions made by the BLE bridge.

mod common;

use jacquard_core::{NodeId, RoutingTickHint, Tick};
use jacquard_reference_client::{BridgeRoundProgress, ClientBuilder, SharedInMemoryNetwork};
use jacquard_traits::{RoutingControlPlane, RoutingMiddleware};

use common::published_topology;

fn assert_uses_shared_router_contracts<T>(router: &T)
where
    T: RoutingMiddleware + RoutingControlPlane,
{
    // Compile-time bounds check; the value is intentionally unused at runtime.
    // allow-ignored-result: compile-time trait-bound assertion intentionally discards the runtime value
    let _ = router;
}

#[test]
fn published_reference_client_builder_matches_ble_bridge_assumptions() {
    let local_node_id = NodeId([1; 32]);
    let topology = published_topology();
    let network = SharedInMemoryNetwork::default();

    let mut client = ClientBuilder::mercator(local_node_id, topology, network, Tick(1))
        .build()
        .expect("build published client");
    let mut bound = client.bind();

    assert_uses_shared_router_contracts(bound.router());

    let progress = bound.advance_round().expect("advance published client");
    match progress {
        BridgeRoundProgress::Advanced(report) => {
            assert_eq!(
                report.router_outcome.next_round_hint,
                RoutingTickHint::Immediate
            );
        }
        BridgeRoundProgress::Waiting(wait) => {
            assert!(
                matches!(
                    wait.next_round_hint,
                    RoutingTickHint::Immediate
                        | RoutingTickHint::WithinTicks(_)
                        | RoutingTickHint::HostDefault
                ),
                "published host bridge should surface shared round cadence hints",
            );
        }
    }
}
