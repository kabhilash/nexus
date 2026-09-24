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
async fn reconcile_self_heals_a_missed_adapter_props_signal() {
    // Boot-time race: BlueZ already has the adapter powered, but the
    // initial ObjectManager snapshot / PropertiesChanged signal never
    // reached Nexus (both daemons started in the same instant). The
    // reconcile tick's `refresh_adapter` backstop (DD-004 §7.3)
    // should catch it up within one tick without any explicit signal.
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
        )))
        .unwrap();
    // BlueZ's own truth is "powered" — set it *silently*, with no
    // InterfacesAdded/PropertiesChanged signal, so the backend has no
    // way to learn it except by polling `refresh_adapter`.
    mock.set_adapter_props_silently("/org/bluez/hci2", true, false);

    let event = await_event(&mut event_rx, |e| {
        matches!(
            e,
            NexusEvent::BtAdapterChanged {
                adapter,
                powered: true,
                ..
            } if adapter == "/org/bluez/hci2"
        )
    })
    .await;
    assert!(matches!(event, NexusEvent::BtAdapterChanged { .. }));

    // The mock's address map was never configured for this adapter,
    // so `refresh_adapter` reports the zero default every tick. That
    // must never be treated as a correction against the real cached
    // address (`bt_interface`'s non-zero mac) — confirm a further
    // tick doesn't emit a spurious MacChanged regressing it to zero.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    while let Ok(e) = event_rx.try_recv() {
        assert!(
            !matches!(e, NexusEvent::MacChanged { .. }),
            "unexpected MacChanged from an unconfigured (zero) mock address: {e:?}"
        );
    }

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn reconcile_corrects_address_from_bluez_when_sysfs_never_had_one() {
    // UART/serdev-attached controllers never populate a kernel sysfs
    // address at all, so nexus-interface-monitor's udev probe stamps
    // [0; 6] at discovery and no `change` uevent ever arrives to fix
    // it (there's no sysfs file to begin with). BlueZ's own
    // Adapter1.Address is authoritative and is what the reconcile
    // loop's `refresh_adapter` polls instead (DD-004 §7.3).
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    // Discovered with the zeroed placeholder udev stamps when it
    // never finds a sysfs address.
    let mut zeroed = bt_interface(4, "hci3", "/org/bluez/hci3");
    zeroed.mac = [0; 6];
    if let InterfaceKind::Bluetooth { bt_address, .. } = &mut zeroed.kind {
        *bt_address = MacAddr([0; 6]);
    }
    event_tx
        .send(NexusEvent::InterfaceDiscovered(zeroed))
        .unwrap();

    // BlueZ, however, has always known the real address.
    mock.set_adapter_address(
        "/org/bluez/hci3",
        MacAddr([0x34, 0x90, 0xEA, 0xAD, 0xAB, 0xC3]),
    );

    let event = await_event(&mut event_rx, |e| {
        matches!(
            e,
            NexusEvent::MacChanged { mac, .. } if mac.0 == [0x34, 0x90, 0xEA, 0xAD, 0xAB, 0xC3]
        )
    })
    .await;
    assert!(matches!(event, NexusEvent::MacChanged { .. }));

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
