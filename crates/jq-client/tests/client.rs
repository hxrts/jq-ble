//! Integration tests for [`JacquardBleClient`].
//!
//! Verifies that a send call flushes a payload through the full client
//! boundary using an in-memory transport stub.

mod common;

use std::time::{Duration, Instant};

use jacquard_adapter::{dispatch_mailbox, TransportIngressClass};
use jacquard_core::{
    LinkBuilder, LinkEndpoint, LinkRuntimeState, NodeId, PartitionRecoveryClass, RepairCapability,
    TransportIngressEvent, TransportKind,
};
use jq_client::{
    decode_client_payload_for_testing, encode_client_payload_for_testing, JacquardBleClient,
};
use tokio::task::LocalSet;
use tokio_stream::StreamExt;

use common::{
    ble_endpoint, local_only_topology, published_topology, FakeTransport, TestTransportSender,
};

fn payload_received(
    remote_node_id: NodeId,
    endpoint: LinkEndpoint,
    payload: &[u8],
) -> TransportIngressEvent {
    TransportIngressEvent::PayloadReceived {
        from_node_id: remote_node_id,
        endpoint,
        payload: payload.to_vec(),
    }
}

fn link_observed(remote_node_id: NodeId, endpoint: LinkEndpoint) -> TransportIngressEvent {
    TransportIngressEvent::LinkObserved {
        remote_node_id,
        link: LinkBuilder::new(endpoint)
            .with_profile(
                jacquard_core::DurationMs(25),
                RepairCapability::TransportRetransmit,
                PartitionRecoveryClass::LocalReconnect,
            )
            .with_runtime_state(LinkRuntimeState::Active)
            .build(),
        source_class: jacquard_core::FactSourceClass::Local,
        evidence_class: jacquard_core::RoutingEvidenceClass::DirectObservation,
        origin_authentication: jacquard_core::OriginAuthenticationClass::Unauthenticated,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn send_flushes_a_payload_through_the_client_boundary() {
    LocalSet::new()
        .run_until(async {
            let local_node_id = NodeId([1; 32]);
            let remote_node_id = NodeId([2; 32]);
            let (outbound_tx, outbound_rx) = dispatch_mailbox(64);
            let transport_sender = TestTransportSender {
                outbound: outbound_tx,
            };
            let transport = FakeTransport::new(outbound_rx);
            let flushed = transport.flushed.clone();
            let client = JacquardBleClient::new_with_transport_for_testing(
                local_node_id,
                published_topology(),
                transport,
                transport_sender,
            );
            let started_at = Instant::now();

            client
                .send(remote_node_id, b"hello over jacquard ble")
                .await
                .expect("send through client");
            assert!(
                started_at.elapsed() < Duration::from_millis(150),
                "send should preempt the idle wait path",
            );

            let flushed = flushed.lock().expect("flushed frames");
            assert!(flushed
                .iter()
                .filter_map(|frame| decode_client_payload_for_testing(&frame.payload))
                .any(|payload| payload == b"hello over jacquard ble"));
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn user_payloads_starting_with_jqbatman_are_delivered_while_unframed_control_bytes_stay_hidden(
) {
    LocalSet::new()
        .run_until(async {
            let local_node_id = NodeId([1; 32]);
            let remote_node_id = NodeId([2; 32]);
            let (outbound_tx, outbound_rx) = dispatch_mailbox(64);
            let transport_sender = TestTransportSender {
                outbound: outbound_tx,
            };
            let transport = FakeTransport::new(outbound_rx);
            let ingress_sender = transport.ingress_sender.clone();
            let client = JacquardBleClient::new_with_transport_for_testing(
                local_node_id,
                local_only_topology(local_node_id),
                transport,
                transport_sender,
            );
            let mut incoming = Box::pin(client.incoming());

            ingress_sender
                .emit(
                    TransportIngressClass::Payload,
                    payload_received(
                        remote_node_id,
                        ble_endpoint(2, TransportKind::BleGatt),
                        &encode_client_payload_for_testing(b"JQBATMAN user payload"),
                    ),
                )
                .expect("emit framed user payload");

            let delivered = tokio::time::timeout(Duration::from_millis(200), incoming.next())
                .await
                .expect("framed user payload should be delivered")
                .expect("incoming payload");
            assert_eq!(delivered.0, remote_node_id);
            assert_eq!(delivered.1.as_ref(), b"JQBATMAN user payload");

            ingress_sender
                .emit(
                    TransportIngressClass::Payload,
                    payload_received(
                        remote_node_id,
                        ble_endpoint(2, TransportKind::BleGatt),
                        b"JQBATMAN control plane",
                    ),
                )
                .expect("emit unframed control payload");

            assert!(
                tokio::time::timeout(Duration::from_millis(100), incoming.next())
                    .await
                    .is_err(),
                "unframed control bytes must stay hidden from the application stream",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn notifier_wakes_the_client_without_waiting_for_the_full_tick_interval() {
    LocalSet::new()
        .run_until(async {
            let local_node_id = NodeId([1; 32]);
            let remote_node_id = NodeId([9; 32]);
            let (outbound_tx, outbound_rx) = dispatch_mailbox(64);
            let transport_sender = TestTransportSender {
                outbound: outbound_tx,
            };
            let transport = FakeTransport::new(outbound_rx);
            let ingress_sender = transport.ingress_sender.clone();
            let client = JacquardBleClient::new_with_transport_and_round_interval_for_testing(
                local_node_id,
                local_only_topology(local_node_id),
                transport,
                transport_sender,
                Duration::from_millis(250),
            );

            let mut topology_stream = Box::pin(client.topology_stream());
            let _initial = topology_stream.next().await.expect("initial topology");

            let started_at = Instant::now();
            ingress_sender
                .emit(
                    TransportIngressClass::Control,
                    link_observed(remote_node_id, ble_endpoint(9, TransportKind::BleGatt)),
                )
                .expect("emit link observation");

            let updated = tokio::time::timeout(Duration::from_millis(125), topology_stream.next())
                .await
                .expect("notifier should wake before the fallback tick interval")
                .expect("topology update");
            assert!(updated.nodes.contains_key(&remote_node_id));
            assert!(
                started_at.elapsed() < Duration::from_millis(200),
                "topology update should be driven by the notifier rather than a 250ms poll",
            );
        })
        .await;
}
