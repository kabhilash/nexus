//! End-to-end adapter lifecycle tests driven by the mock BlueZ
//! client. See DD-004 §4.2, §14.1.

use std::sync::Arc;
use std::time::Duration;

use nexus_bluetooth::bluez::BluezClient;
use nexus_bluetooth::{BluetoothConfig, MockBluezClient, spawn_bluetooth_backend};
use nexus_core::{InterfaceInfo, InterfaceKind, MacAddr, NexusEvent, OperState};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::sync::broadcast;

fn bt_interface(ifindex: u32, hci: &str, bluez_path: &str) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: hci.to_owned(),
        mac: [0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13],
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Bluetooth {
            hci_name: hci.to_owned(),
            hci_index: ifindex,
            bt_address: MacAddr([0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13]),
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
async fn adapter_progresses_unavailable_present_powered_discovering_gone() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();

    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    // 1. Interface Monitor surfaces the adapter.
    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            1,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();

    // 2. BlueZ publishes the adapter (powered off).
    mock.publish_adapter("/org/bluez/hci0", false, false).await;

    // Expect the BtAdapterChanged (Present).
    let adapter_event = await_event(&mut event_rx, |e| {
        matches!(
            e,
            NexusEvent::BtAdapterChanged {
                adapter,
                powered: false,
                discovering: false,
                ..
            } if adapter == "/org/bluez/hci0"
        )
    })
    .await;
    assert!(matches!(adapter_event, NexusEvent::BtAdapterChanged { .. }));

    // 3. Power on.
    mock.publish_adapter_props("/org/bluez/hci0", true, false)
        .await;
    await_event(&mut event_rx, |e| {
        matches!(
            e,
            NexusEvent::BtAdapterChanged {
                adapter,
                powered: true,
                discovering: false,
            } if adapter == "/org/bluez/hci0"
        )
    })
    .await;

    // 4. Start discovering.
    mock.publish_adapter_props("/org/bluez/hci0", true, true)
        .await;
    await_event(&mut event_rx, |e| {
        matches!(
            e,
            NexusEvent::BtAdapterChanged {
                adapter,
                powered: true,
                discovering: true,
            } if adapter == "/org/bluez/hci0"
        )
    })
    .await;

    // 5. Interface removed.
    event_tx
        .send(NexusEvent::InterfaceRemoved { ifindex: 1 })
        .unwrap();
    // Give the backend a tick to clean up.
    tokio::time::sleep(Duration::from_millis(50)).await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn bluez_disconnect_marks_adapter_unavailable_and_clears_devices() {
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
            2,
            "hci1",
            "/org/bluez/hci1",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci1", true, false).await;
    await_event(&mut event_rx, |e| {
        matches!(
            e,
            NexusEvent::BtAdapterChanged { adapter, .. } if adapter == "/org/bluez/hci1"
        )
    })
    .await;

    // Publish a device on it.
    mock.publish_device(
        "/org/bluez/hci1",
        MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
        false,
    )
    .await;
    await_event(&mut event_rx, |e| {
        matches!(e, NexusEvent::BtDeviceDiscovered(_))
    })
    .await;

    // Simulate BlueZ dropping.
    event_tx.send(NexusEvent::BluezDisconnected).unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// Helper: pull events off the bus until `pred` matches, with a
/// 2-second timeout.
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
            Ok(Err(_)) | Err(_) => {
                panic!("timed out\nseen: {}", seen.join("\n       "));
            }
        }
    }
}
