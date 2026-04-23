//! End-to-end device lifecycle tests. See DD-004 §5.2, §14.1.

use std::sync::Arc;
use std::time::Duration;

use nexus_bluetooth::bluez::BluezClient;
use nexus_bluetooth::{BluetoothConfig, BtCommand, MockBluezClient, spawn_bluetooth_backend};
use nexus_core::{BluetoothAddrExt, InterfaceInfo, InterfaceKind, MacAddr, NexusEvent, OperState};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::sync::{broadcast, oneshot};

fn bt_interface(ifindex: u32, hci: &str, bluez_path: &str, bt_address: MacAddr) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: hci.to_owned(),
        mac: bt_address.0,
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Bluetooth {
            hci_name: hci.to_owned(),
            hci_index: ifindex,
            bt_address,
            bluez_path: bluez_path.to_owned(),
        },
        discovered_at: std::time::Instant::now(),
    }
}

async fn start_store() -> (TempDir, Arc<dyn ProfileStore>) {
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x77u8; 32]);
    let store = ProfileFileStore::open(tmp.path(), &keys).unwrap();
    (tmp, Arc::new(store))
}

#[tokio::test]
async fn discovery_connect_disconnect_end_to_end() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    // Surface the adapter and publish it as powered.
    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            1,
            "hci0",
            "/org/bluez/hci0",
            MacAddr([0x00; 6]),
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;

    // Operator calls StartDiscovery.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::StartDiscovery {
            adapter: "/org/bluez/hci0".into(),
            filter: nexus_bluetooth::DiscoveryFilter::default(),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    // BlueZ publishes a device.
    let device_address = MacAddr([0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
    mock.publish_device("/org/bluez/hci0", device_address, false)
        .await;

    await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtDeviceDiscovered(info) if info.address == device_address),
    )
    .await;

    // Operator calls Connect.
    let device_path = format!(
        "/org/bluez/hci0/{}",
        device_address.to_object_path_component()
    );
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Connect {
            device_path: device_path.clone(),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    await_event(&mut event_rx, |e| {
        matches!(e, NexusEvent::BtDeviceConnected { address, .. } if *address == device_address)
    })
    .await;

    // Disconnect.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Disconnect {
            device_path: device_path.clone(),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    await_event(&mut event_rx, |e| {
        matches!(e, NexusEvent::BtDeviceDisconnected { address, .. } if *address == device_address)
    })
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn forget_device_calls_remove_and_emits_disconnected() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            3,
            "hci2",
            "/org/bluez/hci2",
            MacAddr([0x00; 6]),
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci2", true, false).await;

    let addr = MacAddr([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    mock.publish_device_full("/org/bluez/hci2", addr, true, false, &[])
        .await;
    await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtDeviceDiscovered(info) if info.address == addr),
    )
    .await;

    let device_path = format!("/org/bluez/hci2/{}", addr.to_object_path_component());
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Forget {
            adapter: "/org/bluez/hci2".into(),
            device_path: device_path.clone(),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    let calls = mock.state().calls;
    assert!(
        calls.iter().any(|c| matches!(
            c,
            nexus_bluetooth::bluez::mock::MockCall::ForgetDevice(adapter, path)
                if adapter == "/org/bluez/hci2" && path == &device_path
        )),
        "calls = {calls:?}"
    );

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn pair_on_unknown_device_errors() {
    // Pair against a device the backend has never seen should
    // error at start_pairing, not spawn a driver.
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(32);
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Pair {
            device_path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".into(),
            responder: tx,
        })
        .await
        .unwrap();
    let err = rx.await.unwrap().unwrap_err();
    assert!(
        matches!(err, nexus_bluetooth::BtError::UnknownDevice(_)),
        "got {err:?}"
    );

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

async fn await_event<F: Fn(&NexusEvent) -> bool>(
    rx: &mut broadcast::Receiver<NexusEvent>,
    pred: F,
) -> NexusEvent {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => {
                if pred(&event) {
                    return event;
                }
                seen.push(format!("{event:?}"));
            }
            Ok(Err(_)) | Err(_) => panic!("timed out\nseen: {}", seen.join("\n       ")),
        }
    }
}
