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

#[tokio::test]
async fn gc_emits_bt_device_removed_for_expired_unpaired_device() {
    // An unpaired, unbonded, disconnected device past its discovery
    // TTL must be evicted *and* announced on the bus — otherwise the
    // D-Bus layer's KnownDevices/per-device object never learns it's
    // gone (DD-004 §13.2's `discovery_device_ttl_s`).
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let config = BluetoothConfig {
        discovery_device_ttl_s: 1,
        ..BluetoothConfig::default()
    };
    let handle = spawn_bluetooth_backend(client, store, event_tx.clone(), config);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            6,
            "hci5",
            "/org/bluez/hci5",
            MacAddr([0; 6]),
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci5", true, false).await;

    let addr = MacAddr([0x99, 0x88, 0x77, 0x66, 0x55, 0x44]);
    mock.publish_device("/org/bluez/hci5", addr, false).await;
    await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtDeviceDiscovered(info) if info.address == addr),
    )
    .await;

    // The reconcile tick (and therefore GC) only runs once per
    // second; with `discovery_device_ttl_s: 1`, worst case (discovery
    // landing just after a tick fires) eviction needs up to ~2 ticks.
    // `await_event`'s fixed 2s window is too tight for that margin, so
    // this waits longer explicitly rather than risk flaking on
    // `await_event`'s deadline.
    let event = await_event_timeout(
        &mut event_rx,
        |e| {
            matches!(
                e,
                NexusEvent::BtDeviceRemoved { adapter, address }
                    if adapter == "/org/bluez/hci5" && *address == addr
            )
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(matches!(event, NexusEvent::BtDeviceRemoved { .. }));

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn gc_keeps_paired_devices_past_ttl() {
    // DD-004 §13.2: paired devices are kept indefinitely regardless
    // of the discovery TTL — only Forget removes them.
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let config = BluetoothConfig {
        discovery_device_ttl_s: 1,
        ..BluetoothConfig::default()
    };
    let handle = spawn_bluetooth_backend(client, store, event_tx.clone(), config);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            7,
            "hci6",
            "/org/bluez/hci6",
            MacAddr([0; 6]),
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci6", true, false).await;

    let addr = MacAddr([0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc]);
    // `paired = true` — this is the branch that must survive GC.
    mock.publish_device("/org/bluez/hci6", addr, true).await;
    await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtDeviceDiscovered(info) if info.address == addr),
    )
    .await;

    // Give the reconcile tick well past the 1s TTL a chance to run
    // (up to ~2 ticks worst case, since GC only checks once/second),
    // then confirm no BtDeviceRemoved ever showed up for this device.
    tokio::time::sleep(Duration::from_millis(3000)).await;
    let mut removed = false;
    while let Ok(e) = event_rx.try_recv() {
        if matches!(&e, NexusEvent::BtDeviceRemoved { address, .. } if *address == addr) {
            removed = true;
        }
    }
    assert!(!removed, "paired device must not be GC'd by TTL");

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

async fn await_event<F: Fn(&NexusEvent) -> bool>(
    rx: &mut broadcast::Receiver<NexusEvent>,
    pred: F,
) -> NexusEvent {
    await_event_timeout(rx, pred, Duration::from_secs(2)).await
}

/// Like [`await_event`], but with an explicit deadline instead of
/// the default 2s — for waits with a known-longer worst case (e.g. a
/// reconcile-tick-driven event, where a 1Hz tick plus a short TTL can
/// legitimately take close to two tick intervals).
async fn await_event_timeout<F: Fn(&NexusEvent) -> bool>(
    rx: &mut broadcast::Receiver<NexusEvent>,
    pred: F,
    timeout: Duration,
) -> NexusEvent {
    let deadline = tokio::time::Instant::now() + timeout;
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
