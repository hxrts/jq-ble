//! Integration tests for [`BleRuntimeTask`] using the `blew` mock backend.
//!
//! Covers the single-owner invariant, identity resolution before
//! `LinkObserved`, hint mismatch fail-closed behaviour, sender queue
//! isolation from BLE I/O, and peripheral-side node ID reads.

use blew::central::{Central, CentralEvent, WriteType};
use blew::gatt::service::GattService;
use blew::peripheral::{AdvertisingConfig, Peripheral, PeripheralEvent};
use blew::testing::MockLink;
use blew::types::DeviceId;
use bytes::Bytes;
use futures_util::SinkExt;
use jacquard_core::{
    ByteCount, EndpointLocator, LinkEndpoint, NodeId, TransportError, TransportIngressEvent,
    TransportKind,
};
use jacquard_host_support::DispatchReceiver;
use jacquard_traits::{TransportDriver, TransportSenderEffects};
use jq_link_profile::{
    BleConfig, BleOutboundCommand, BleRuntimeTask, BleSession, BleTransportDriver,
    JACQUARD_C2P_CHAR_UUID, JACQUARD_NODE_ID_CHAR_UUID, JACQUARD_P2C_CHAR_UUID,
    JACQUARD_PSM_CHAR_UUID, PeerSessions, advertised_hint_service, gatt_endpoint,
    gatt_fallback_service, gatt_l2cap_service, identity_gatt_service, l2cap_endpoint,
    parse_psm_value,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep, timeout};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite, LengthDelimitedCodec};

const WAIT_MAX_RETRIES: usize = 40;
const WAIT_RETRY_INTERVAL_MS: Duration = Duration::from_millis(10);

fn endpoint(byte: u8) -> LinkEndpoint {
    LinkEndpoint::new(
        TransportKind::BleGatt,
        EndpointLocator::ScopedBytes {
            scope: "ble".into(),
            bytes: vec![byte],
        },
        ByteCount(128),
    )
}

fn l2cap_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .length_field_length(2)
        .little_endian()
        .max_frame_length(1472)
        .new_codec()
}

async fn shutdown_runtime(
    control: jq_link_profile::BleDriverControl,
    runtime_task: JoinHandle<()>,
) {
    control.shutdown().await.expect("shutdown runtime");
    timeout(Duration::from_secs(1), runtime_task)
        .await
        .expect("runtime exits")
        .expect("join runtime");
}

async fn spawn_mock_runtime(
    local_node_id: NodeId,
    remote_services: impl IntoIterator<Item = GattService>,
) -> (
    BleTransportDriver,
    jq_link_profile::BleTransportSender,
    DispatchReceiver<BleOutboundCommand>,
    jq_link_profile::BleDriverControl,
    JoinHandle<()>,
) {
    let (central_link, remote_peripheral_link) = MockLink::pair();
    let central: Central<_> = Central::from_backend(central_link.central);
    let remote_peripheral: Peripheral<_> =
        Peripheral::from_backend(remote_peripheral_link.peripheral);

    for service in remote_services {
        remote_peripheral
            .add_service(&service)
            .await
            .expect("add remote service");
    }

    remote_peripheral
        .start_advertising(&AdvertisingConfig {
            local_name: "jacquard-peer".into(),
            service_uuids: Vec::new(),
        })
        .await
        .expect("start advertising");

    let (_remote_central_link, peripheral_link) = MockLink::pair();
    let peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let components =
        BleRuntimeTask::spawn(local_node_id, central, peripheral, BleConfig::default())
            .expect("spawn mock runtime");
    let (driver, sender, outbound, control, _notifier, runtime_task) = components.into_parts();
    (driver, sender, outbound, control, runtime_task)
}

async fn wait_for_ingress(
    driver: &mut BleTransportDriver,
    minimum_events: usize,
) -> Vec<TransportIngressEvent> {
    for _ in 0..WAIT_MAX_RETRIES {
        let ingress = driver
            .drain_transport_ingress()
            .expect("drain transport ingress");
        if ingress.len() >= minimum_events {
            return ingress;
        }
        sleep(WAIT_RETRY_INTERVAL_MS).await;
    }

    panic!("timed out waiting for ingress events");
}

async fn wait_for_link_kinds(
    driver: &mut BleTransportDriver,
    minimum_events: usize,
) -> Vec<TransportKind> {
    let ingress = wait_for_ingress(driver, minimum_events).await;
    ingress
        .into_iter()
        .filter_map(|event| match event {
            TransportIngressEvent::LinkObserved { link, .. } => Some(link.endpoint.transport_kind),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn runtime_task_owns_ble_roles_while_components_expose_bridge_surfaces() {
    let (central_link, peripheral_link) = MockLink::pair();
    let central: Central<_> = Central::from_backend(central_link.central);
    let peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let components =
        BleRuntimeTask::spawn(NodeId([7; 32]), central, peripheral, BleConfig::default())
            .expect("spawn ble runtime");
    let (_driver, _sender, _outbound, control, _notifier, runtime_task) = components.into_parts();

    assert!(!runtime_task.is_finished());

    control
        .dispatch(vec![BleOutboundCommand {
            endpoint: endpoint(7),
            payload: b"queued".to_vec(),
        }])
        .await
        .expect("dispatch outbound batch");

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn unresolved_advertisement_hints_do_not_emit_link_observed() {
    let remote_node_id = NodeId([3; 32]);
    let (mut driver, _sender, _outbound, control, runtime_task) =
        spawn_mock_runtime(NodeId([1; 32]), [advertised_hint_service(&remote_node_id)]).await;

    sleep(Duration::from_millis(50)).await;
    let ingress = driver.drain_transport_ingress().expect("drain ingress");
    assert!(ingress.is_empty());

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn gatt_only_peers_resolve_identity_before_link_observed() {
    let remote_node_id = NodeId([5; 32]);
    let (mut driver, _sender, _outbound, control, runtime_task) = spawn_mock_runtime(
        NodeId([1; 32]),
        [
            advertised_hint_service(&remote_node_id),
            identity_gatt_service(&remote_node_id),
        ],
    )
    .await;

    let ingress = wait_for_ingress(&mut driver, 1).await;
    assert_eq!(ingress.len(), 1);
    match &ingress[0] {
        TransportIngressEvent::LinkObserved {
            remote_node_id: observed,
            ..
        } => assert_eq!(observed, &remote_node_id),
        other => panic!("unexpected ingress event: {other:?}"),
    }

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn hint_mismatch_fails_closed() {
    let hinted_node_id = NodeId([7; 32]);
    let resolved_node_id = NodeId([8; 32]);
    let (mut driver, _sender, _outbound, control, runtime_task) = spawn_mock_runtime(
        NodeId([1; 32]),
        [
            advertised_hint_service(&hinted_node_id),
            identity_gatt_service(&resolved_node_id),
        ],
    )
    .await;

    sleep(Duration::from_millis(50)).await;
    let ingress = driver.drain_transport_ingress().expect("drain ingress");
    assert!(ingress.is_empty());

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn sender_queue_does_not_perform_ble_io_until_control_dispatch() {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([6; 32]);

    let (central_link, remote_peripheral_link) = MockLink::pair();
    let runtime_central: Central<_> = Central::from_backend(central_link.central);
    let remote_peripheral: Peripheral<_> =
        Peripheral::from_backend(remote_peripheral_link.peripheral);
    remote_peripheral
        .add_service(&advertised_hint_service(&remote_node_id))
        .await
        .expect("add hint service");
    remote_peripheral
        .add_service(&gatt_fallback_service(&remote_node_id))
        .await
        .expect("add remote fallback service");
    remote_peripheral
        .start_advertising(&AdvertisingConfig {
            local_name: "remote-peer".into(),
            service_uuids: Vec::new(),
        })
        .await
        .expect("start advertising");

    let (_remote_central_link, peripheral_link) = MockLink::pair();
    let runtime_peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let components = BleRuntimeTask::spawn(
        local_node_id,
        runtime_central,
        runtime_peripheral,
        BleConfig::default(),
    )
    .expect("spawn runtime");
    let (mut driver, mut sender, mut outbound, control, _notifier, runtime_task) =
        components.into_parts();
    // allow-ignored-result: this wait only ensures ingress drained before the subsequent assertion path
    let _ = wait_for_ingress(&mut driver, 1).await;

    let mut remote_events = remote_peripheral.events();
    let setup_event = timeout(Duration::from_millis(100), remote_events.next())
        .await
        .expect("wait for subscription setup event")
        .expect("remote events should stay open");
    assert!(matches!(
        setup_event,
        PeripheralEvent::SubscriptionChanged {
            char_uuid: JACQUARD_P2C_CHAR_UUID,
            subscribed: true,
            ..
        }
    ));

    sender
        .send_transport(
            &gatt_endpoint(&DeviceId::from("mock-peripheral")),
            b"queued",
        )
        .expect("queue send");

    assert!(
        timeout(Duration::from_millis(50), remote_events.next())
            .await
            .is_err(),
        "sender should only queue work"
    );

    let batch = outbound.drain();
    assert_eq!(batch.len(), 1);
    control
        .dispatch(batch)
        .await
        .expect("dispatch queued batch");

    let event = timeout(Duration::from_millis(100), remote_events.next())
        .await
        .expect("wait for remote write")
        .expect("remote events should stay open");
    match event {
        PeripheralEvent::WriteRequest {
            client_id,
            char_uuid,
            value,
            responder,
            ..
        } => {
            assert_eq!(client_id, DeviceId::from("mock-central"));
            assert_eq!(char_uuid, JACQUARD_C2P_CHAR_UUID);
            assert_eq!(value, b"queued");
            assert!(responder.is_none());
        }
        other => panic!("unexpected remote event: {other:?}"),
    }

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn sender_queue_fails_closed_when_dispatch_mailbox_is_full() {
    let (central_link, peripheral_link) = MockLink::pair();
    let central: Central<_> = Central::from_backend(central_link.central);
    let peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let components = BleRuntimeTask::spawn(
        NodeId([1; 32]),
        central,
        peripheral,
        BleConfig {
            command_capacity: 1,
            ..BleConfig::default()
        },
    )
    .expect("spawn runtime");
    let (_driver, mut sender, _outbound, control, _notifier, runtime_task) =
        components.into_parts();

    sender
        .send_transport(&endpoint(1), b"first")
        .expect("first send should enqueue");
    let error = sender
        .send_transport(&endpoint(1), b"second")
        .expect_err("second send should fail closed");
    assert_eq!(error, TransportError::Unavailable);

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn peripheral_side_node_id_reads_respond_with_local_identity() {
    let local_node_id = NodeId([4; 32]);

    let (central_link, _remote_peripheral_link) = MockLink::pair();
    let runtime_central: Central<_> = Central::from_backend(central_link.central);
    let (remote_central_link, peripheral_link) = MockLink::pair();
    let remote_central: Central<_> = Central::from_backend(remote_central_link.central);
    let runtime_peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let components = BleRuntimeTask::spawn(
        local_node_id,
        runtime_central,
        runtime_peripheral,
        BleConfig::default(),
    )
    .expect("spawn runtime");
    let (_driver, _sender, _outbound, control, _notifier, runtime_task) = components.into_parts();

    sleep(Duration::from_millis(20)).await;
    let value = remote_central
        .read_characteristic(
            &DeviceId::from("mock-peripheral"),
            JACQUARD_NODE_ID_CHAR_UUID,
        )
        .await
        .expect("read local node id");
    assert_eq!(value, local_node_id.0.to_vec());

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn unresolved_peripheral_writes_fail_closed() {
    let local_node_id = NodeId([4; 32]);

    let (central_link, _remote_peripheral_link) = MockLink::pair();
    let runtime_central: Central<_> = Central::from_backend(central_link.central);
    let (remote_central_link, peripheral_link) = MockLink::pair();
    let remote_central: Central<_> = Central::from_backend(remote_central_link.central);
    let runtime_peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let components = BleRuntimeTask::spawn(
        local_node_id,
        runtime_central,
        runtime_peripheral,
        BleConfig::default(),
    )
    .expect("spawn runtime");
    let (_driver, _sender, _outbound, control, _notifier, runtime_task) = components.into_parts();

    sleep(Duration::from_millis(20)).await;
    let result = remote_central
        .write_characteristic(
            &DeviceId::from("mock-peripheral"),
            JACQUARD_C2P_CHAR_UUID,
            b"should-fail".to_vec(),
            WriteType::WithResponse,
        )
        .await;
    assert!(result.is_err(), "unresolved client writes must be rejected");

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn resolved_peripheral_writes_emit_payload_and_ack() {
    let local_node_id = NodeId([4; 32]);
    let remote_node_id = NodeId([9; 32]);

    let (central_link, _remote_peripheral_link) = MockLink::pair();
    let runtime_central: Central<_> = Central::from_backend(central_link.central);
    let (remote_central_link, peripheral_link) = MockLink::pair();
    let remote_central: Central<_> = Central::from_backend(remote_central_link.central);
    let runtime_peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let (mut task, (mut driver, _sender, _outbound, control, _notifier)) = BleRuntimeTask::new(
        local_node_id,
        runtime_central,
        runtime_peripheral,
        BleConfig::default(),
    )
    .expect("build runtime");
    task.testing_seed_resolved_peer(DeviceId::from("mock-central"), remote_node_id);
    let runtime_task = tokio::spawn(task.testing_run());

    sleep(Duration::from_millis(20)).await;
    remote_central
        .write_characteristic(
            &DeviceId::from("mock-peripheral"),
            JACQUARD_C2P_CHAR_UUID,
            b"inbound".to_vec(),
            WriteType::WithResponse,
        )
        .await
        .expect("resolved client write should succeed");

    let ingress = wait_for_ingress(&mut driver, 1).await;
    assert_eq!(ingress.len(), 1);
    match &ingress[0] {
        TransportIngressEvent::PayloadReceived {
            from_node_id,
            endpoint,
            payload,
        } => {
            assert_eq!(from_node_id, &remote_node_id);
            assert_eq!(endpoint, &gatt_endpoint(&DeviceId::from("mock-central")));
            assert_eq!(payload, b"inbound");
        }
        other => panic!("unexpected ingress event: {other:?}"),
    }

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn central_opened_l2cap_upgrade_emits_updates_and_carries_framed_payloads() {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([6; 32]);

    let (central_link, remote_peripheral_link) = MockLink::pair();
    let runtime_central: Central<_> = Central::from_backend(central_link.central);
    let remote_peripheral: Peripheral<_> =
        Peripheral::from_backend(remote_peripheral_link.peripheral);
    let (remote_psm, mut remote_accept_stream) = remote_peripheral
        .l2cap_listener()
        .await
        .expect("start remote l2cap listener");
    remote_peripheral
        .add_service(&advertised_hint_service(&remote_node_id))
        .await
        .expect("add hint service");
    remote_peripheral
        .add_service(&gatt_l2cap_service(&remote_node_id, remote_psm))
        .await
        .expect("add remote l2cap service");
    remote_peripheral
        .start_advertising(&AdvertisingConfig {
            local_name: "remote-peer".into(),
            service_uuids: Vec::new(),
        })
        .await
        .expect("start advertising");

    let (_remote_central_link, peripheral_link) = MockLink::pair();
    let runtime_peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let components = BleRuntimeTask::spawn(
        local_node_id,
        runtime_central,
        runtime_peripheral,
        BleConfig::default(),
    )
    .expect("spawn runtime");
    let (mut driver, mut sender, mut outbound, control, _notifier, runtime_task) =
        components.into_parts();

    let accepted_channel = timeout(Duration::from_millis(250), remote_accept_stream.next())
        .await
        .expect("wait for accepted l2cap channel")
        .expect("accept stream should stay open")
        .expect("accept channel");
    let mut accepted_channel = accepted_channel;

    let mut remote_identity = [0_u8; 32];
    accepted_channel
        .read_exact(&mut remote_identity)
        .await
        .expect("read central identity");
    assert_eq!(remote_identity, local_node_id.0);

    let link_kinds = wait_for_link_kinds(&mut driver, 2).await;
    assert_eq!(
        link_kinds,
        vec![TransportKind::BleGatt, TransportKind::BleL2cap]
    );

    let (remote_reader, remote_writer) = tokio::io::split(accepted_channel);
    let mut remote_reader = FramedRead::new(remote_reader, l2cap_codec());
    let mut remote_writer = FramedWrite::new(remote_writer, l2cap_codec());

    sender
        .send_transport(
            &l2cap_endpoint(&DeviceId::from("mock-peripheral")),
            b"upgraded-outbound",
        )
        .expect("queue outbound");
    let batch = outbound.drain();
    control.dispatch(batch).await.expect("dispatch outbound");

    let outbound = timeout(Duration::from_millis(100), remote_reader.next())
        .await
        .expect("wait for framed outbound payload")
        .expect("reader stays open")
        .expect("framed payload");
    assert_eq!(outbound.as_ref(), b"upgraded-outbound");

    remote_writer
        .send(Bytes::from_static(b"from-remote"))
        .await
        .expect("send framed inbound payload");

    let ingress = wait_for_ingress(&mut driver, 1).await;
    assert_eq!(ingress.len(), 1);
    match &ingress[0] {
        TransportIngressEvent::PayloadReceived {
            from_node_id,
            endpoint,
            payload,
        } => {
            assert_eq!(from_node_id, &remote_node_id);
            assert_eq!(
                endpoint,
                &l2cap_endpoint(&DeviceId::from("mock-peripheral"))
            );
            assert_eq!(payload, b"from-remote");
        }
        other => panic!("unexpected ingress event: {other:?}"),
    }

    drop(remote_writer);
    drop(remote_reader);

    let downgrade = wait_for_link_kinds(&mut driver, 1).await;
    assert_eq!(downgrade, vec![TransportKind::BleGatt]);

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn accepted_l2cap_channels_register_and_downgrade_to_gatt_on_close() {
    let local_node_id = NodeId([4; 32]);
    let remote_node_id = NodeId([7; 32]);

    let (central_link, _remote_peripheral_link) = MockLink::pair();
    let runtime_central: Central<_> = Central::from_backend(central_link.central);
    let (remote_central_link, peripheral_link) = MockLink::pair();
    let remote_central: Central<_> = Central::from_backend(remote_central_link.central);
    let runtime_peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let (mut task, (mut driver, _sender, _outbound, control, _notifier)) = BleRuntimeTask::new(
        local_node_id,
        runtime_central,
        runtime_peripheral,
        BleConfig::default(),
    )
    .expect("build runtime");
    task.testing_seed_resolved_peer(DeviceId::from("mock-central"), remote_node_id);
    task.testing_seed_sessions(
        remote_node_id,
        PeerSessions {
            preferred_ingress: Some(BleSession::GattPeripheralSubscribed {
                device_id: DeviceId::from("mock-central"),
            }),
            preferred_egress: Some(BleSession::GattPeripheralSubscribed {
                device_id: DeviceId::from("mock-central"),
            }),
            fallback_egress: None,
        },
    );
    let runtime_task = tokio::spawn(task.testing_run());

    sleep(Duration::from_millis(20)).await;
    let psm_bytes = remote_central
        .read_characteristic(&DeviceId::from("mock-peripheral"), JACQUARD_PSM_CHAR_UUID)
        .await
        .expect("read advertised psm");
    let psm = parse_psm_value(&psm_bytes).expect("psm should parse");
    let mut channel = remote_central
        .open_l2cap_channel(&DeviceId::from("mock-peripheral"), psm)
        .await
        .expect("open l2cap channel");
    channel
        .write_all(&remote_node_id.0)
        .await
        .expect("write remote identity");
    channel.flush().await.expect("flush identity");

    let upgrade = wait_for_link_kinds(&mut driver, 1).await;
    assert_eq!(upgrade, vec![TransportKind::BleL2cap]);

    let (remote_reader, remote_writer) = tokio::io::split(channel);
    let remote_reader = FramedRead::new(remote_reader, l2cap_codec());
    let mut remote_writer = FramedWrite::new(remote_writer, l2cap_codec());

    remote_writer
        .send(Bytes::from_static(b"accepted-frame"))
        .await
        .expect("send framed inbound payload");

    let ingress = wait_for_ingress(&mut driver, 1).await;
    assert_eq!(ingress.len(), 1);
    match &ingress[0] {
        TransportIngressEvent::PayloadReceived {
            from_node_id,
            endpoint,
            payload,
        } => {
            assert_eq!(from_node_id, &remote_node_id);
            assert_eq!(endpoint, &l2cap_endpoint(&DeviceId::from("mock-central")));
            assert_eq!(payload, b"accepted-frame");
        }
        other => panic!("unexpected ingress event: {other:?}"),
    }

    drop(remote_writer);
    drop(remote_reader);

    let downgrade = wait_for_link_kinds(&mut driver, 1).await;
    assert_eq!(downgrade, vec![TransportKind::BleGatt]);

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn control_path_events_are_retried_after_ingress_overflow() {
    let local_node_id = NodeId([1; 32]);
    let remote_node_id = NodeId([6; 32]);

    let (central_link, remote_peripheral_link) = MockLink::pair();
    let runtime_central: Central<_> = Central::from_backend(central_link.central);
    let remote_peripheral: Peripheral<_> =
        Peripheral::from_backend(remote_peripheral_link.peripheral);
    let (remote_psm, mut remote_accept_stream) = remote_peripheral
        .l2cap_listener()
        .await
        .expect("start remote l2cap listener");
    remote_peripheral
        .add_service(&advertised_hint_service(&remote_node_id))
        .await
        .expect("add hint service");
    remote_peripheral
        .add_service(&gatt_l2cap_service(&remote_node_id, remote_psm))
        .await
        .expect("add remote l2cap service");
    remote_peripheral
        .start_advertising(&AdvertisingConfig {
            local_name: "remote-peer".into(),
            service_uuids: Vec::new(),
        })
        .await
        .expect("start advertising");

    let (_remote_central_link, peripheral_link) = MockLink::pair();
    let runtime_peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let components = BleRuntimeTask::spawn(
        local_node_id,
        runtime_central,
        runtime_peripheral,
        BleConfig {
            ingress_capacity: 1,
            ..BleConfig::default()
        },
    )
    .expect("spawn runtime");
    let (mut driver, _sender, _outbound, control, _notifier, runtime_task) =
        components.into_parts();

    let accepted_channel = timeout(Duration::from_millis(250), remote_accept_stream.next())
        .await
        .expect("wait for accepted l2cap channel")
        .expect("accept stream should stay open")
        .expect("accept channel");
    let mut accepted_channel = accepted_channel;

    let mut remote_identity = [0_u8; 32];
    accepted_channel
        .read_exact(&mut remote_identity)
        .await
        .expect("read central identity");
    assert_eq!(remote_identity, local_node_id.0);

    let link_kinds = wait_for_link_kinds(&mut driver, 1).await;
    assert_eq!(link_kinds, vec![TransportKind::BleGatt]);
    let link_kinds = wait_for_link_kinds(&mut driver, 1).await;
    assert_eq!(link_kinds, vec![TransportKind::BleL2cap]);

    let (remote_reader, remote_writer) = tokio::io::split(accepted_channel);
    let remote_reader = FramedRead::new(remote_reader, l2cap_codec());
    let mut remote_writer = FramedWrite::new(remote_writer, l2cap_codec());

    remote_writer
        .send(Bytes::from_static(b"payload-before-close"))
        .await
        .expect("send framed inbound payload");
    drop(remote_writer);
    drop(remote_reader);

    let ingress = wait_for_ingress(&mut driver, 1).await;
    match &ingress[0] {
        TransportIngressEvent::PayloadReceived { payload, .. } => {
            assert_eq!(payload, b"payload-before-close");
        }
        other => panic!("unexpected ingress event: {other:?}"),
    }

    let downgrade = wait_for_link_kinds(&mut driver, 1).await;
    assert_eq!(downgrade, vec![TransportKind::BleGatt]);

    shutdown_runtime(control, runtime_task).await;
}

#[tokio::test]
async fn disconnect_clears_gatt_session_state_per_device_id_policy() {
    let (central_link, _remote_peripheral_link) = MockLink::pair();
    let runtime_central: Central<_> = Central::from_backend(central_link.central);
    let (_remote_central_link, peripheral_link) = MockLink::pair();
    let runtime_peripheral: Peripheral<_> = Peripheral::from_backend(peripheral_link.peripheral);

    let (mut task, (_driver, _sender, _outbound, _control, _notifier)) = BleRuntimeTask::new(
        NodeId([1; 32]),
        runtime_central,
        runtime_peripheral,
        BleConfig::default(),
    )
    .expect("build runtime");
    let device_id = DeviceId::from("mock-peripheral");
    let node_id = NodeId([2; 32]);
    task.testing_seed_resolved_peer(device_id.clone(), node_id);
    task.testing_seed_sessions(
        node_id,
        PeerSessions {
            preferred_ingress: Some(BleSession::GattCentral {
                device_id: device_id.clone(),
            }),
            preferred_egress: Some(BleSession::GattCentral {
                device_id: device_id.clone(),
            }),
            fallback_egress: None,
        },
    );

    task.testing_handle_central_event(CentralEvent::DeviceDisconnected {
        device_id: device_id.clone(),
    })
    .await;

    assert!(!task.testing_identity_resolved_for_device(&device_id));
    assert_eq!(task.testing_session_count(), 0);
    assert!(!task.testing_has_egress_session_for_device(&device_id));
}
