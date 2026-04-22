//! Integration tests for [`JacquardBleClient`].
//!
//! Verifies that a send call flushes a payload through the full client
//! boundary using an in-memory transport stub.

mod common;

use std::time::{Duration, Instant};

use jacquard_core::{
    LinkBuilder, LinkEndpoint, LinkRuntimeState, MulticastGroupId, NodeId, PartitionRecoveryClass,
    RepairCapability, TransportDeliveryIntent, TransportDeliveryMode, TransportIngressEvent,
    TransportKind,
};
use jacquard_host_support::{TransportIngressClass, dispatch_mailbox};
use jq_client::test_support::{
    decode_client_payload_for_testing, encode_client_payload_for_testing,
};
use jq_client::{JacquardBleClient, JacquardBleSendStage};
use tokio::task::LocalSet;
use tokio_stream::StreamExt;

use common::{
    FakeTransport, TestTransportSender, ble_endpoint, local_only_topology, published_topology,
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

            let receipt = client
                .send(remote_node_id, b"hello over jacquard ble")
                .await
                .expect("send through client");
            assert_eq!(receipt.destination_node_id, remote_node_id);
            assert_eq!(receipt.next_hop_node_id, remote_node_id);
            assert_eq!(receipt.delivery_mode, TransportDeliveryMode::Unicast);
            assert_eq!(receipt.stage, JacquardBleSendStage::QueuedForTransport);
            assert!(
                started_at.elapsed() < Duration::from_millis(150),
                "send should preempt the idle wait path",
            );

            let flushed = flushed.lock().expect("flushed frames");
            assert!(
                flushed
                    .iter()
                    .filter_map(|frame| decode_client_payload_for_testing(&frame.payload))
                    .any(|payload| payload == b"hello over jacquard ble")
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn send_multicast_flushes_an_admitted_multicast_intent() {
    LocalSet::new()
        .run_until(async {
            let local_node_id = NodeId([1; 32]);
            let receivers = vec![NodeId([2; 32]), NodeId([3; 32])];
            let group_id = MulticastGroupId([9; 16]);
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

            let receipt = client
                .send_multicast(group_id, receivers.clone(), b"hello multicast")
                .await
                .expect("send multicast through client");

            assert_eq!(receipt.group_id, group_id);
            assert_eq!(receipt.receivers, receivers);
            assert_eq!(receipt.delivery_mode, TransportDeliveryMode::Multicast);
            assert_eq!(receipt.stage, JacquardBleSendStage::QueuedForTransport);

            let flushed = flushed.lock().expect("flushed frames");
            let frame = flushed
                .iter()
                .find(|frame| {
                    decode_client_payload_for_testing(&frame.payload).as_deref()
                        == Some(b"hello multicast".as_slice())
                })
                .expect("multicast frame flushed");
            match &frame.intent {
                TransportDeliveryIntent::Multicast {
                    group_id: frame_group,
                    receivers: frame_receivers,
                    ..
                } => {
                    assert_eq!(*frame_group, group_id);
                    assert_eq!(*frame_receivers, receipt.receivers);
                }
                other => panic!("expected multicast intent, got {other:?}"),
            }
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn user_payloads_starting_with_jqclient_are_delivered_while_unframed_control_bytes_stay_hidden()
 {
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
                        &encode_client_payload_for_testing(b"JQCLIENT user payload"),
                    ),
                )
                .expect("emit framed user payload");

            let delivered = tokio::time::timeout(Duration::from_millis(200), incoming.next())
                .await
                .expect("framed user payload should be delivered")
                .expect("incoming payload");
            assert_eq!(delivered.0, remote_node_id);
            assert_eq!(delivered.1.as_ref(), b"JQCLIENT user payload");

            ingress_sender
                .emit(
                    TransportIngressClass::Payload,
                    payload_received(
                        remote_node_id,
                        ble_endpoint(2, TransportKind::BleGatt),
                        b"JQCONTROL plane",
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
            let bridge_round_interval_ms = Duration::from_millis(1_000);
            let client = JacquardBleClient::new_with_transport_and_round_interval_ms_for_testing(
                local_node_id,
                local_only_topology(local_node_id),
                transport,
                transport_sender,
                bridge_round_interval_ms,
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

            let updated = tokio::time::timeout(Duration::from_millis(500), topology_stream.next())
                .await
                .expect("notifier should wake before the fallback tick interval")
                .expect("topology update");
            assert!(updated.nodes.contains_key(&remote_node_id));
            assert!(
                started_at.elapsed() < Duration::from_millis(750),
                "topology update should be driven by the notifier rather than a 1s poll",
            );
        })
        .await;
}

#[tokio::test(flavor = "current_thread")]
async fn explicit_shutdown_joins_the_client_runtime() {
    LocalSet::new()
        .run_until(async {
            let local_node_id = NodeId([1; 32]);
            let (outbound_tx, outbound_rx) = dispatch_mailbox(64);
            let transport_sender = TestTransportSender {
                outbound: outbound_tx,
            };
            let transport = FakeTransport::new(outbound_rx);
            let client = JacquardBleClient::new_with_transport_for_testing(
                local_node_id,
                local_only_topology(local_node_id),
                transport,
                transport_sender,
            );

            client.shutdown().expect("shutdown client runtime");
        })
        .await;
}
