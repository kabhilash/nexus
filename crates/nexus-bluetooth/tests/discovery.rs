//! Discovery session tests: sole-owner start/stop, concurrent
//! external sessions, and auto-stop timeout. See DD-004 §9.1.

use std::sync::Arc;
use std::time::Duration;

use nexus_bluetooth::bluez::BluezClient;
use nexus_bluetooth::bluez::mock::MockCall;
use nexus_bluetooth::{
    BluetoothConfig, BtCommand, DiscoveryFilter, MockBluezClient, spawn_bluetooth_backend,
};
use nexus_core::{InterfaceInfo, InterfaceKind, MacAddr, NexusEvent, OperState};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::sync::{broadcast, oneshot};

fn bt_interface(ifindex: u32, hci: &str, bluez_path: &str) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: hci.to_owned(),
        mac: [0; 6],
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Bluetooth {
            hci_name: hci.to_owned(),
            hci_index: ifindex,
            bt_address: MacAddr([0; 6]),
            bluez_path: bluez_path.to_owned(),
        },
        discovered_at: std::time::Instant::now(),
    }
}

async fn start_store() -> (TempDir, Arc<dyn ProfileStore>) {
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x88u8; 32]);
    let store = ProfileFileStore::open(tmp.path(), &keys).unwrap();
    (tmp, Arc::new(store))
}

#[tokio::test]
async fn start_and_stop_discovery_records_calls() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(32);
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            1,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    tokio::time::sleep(Duration::from_millis(20)).await;

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::StartDiscovery {
            adapter: "/org/bluez/hci0".into(),
            filter: DiscoveryFilter::default(),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::StopDiscovery {
            adapter: "/org/bluez/hci0".into(),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    let calls = mock.state().calls;
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, MockCall::StartDiscovery(a, _) if a == "/org/bluez/hci0"))
    );
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, MockCall::StopDiscovery(a) if a == "/org/bluez/hci0"))
    );

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn external_client_discovery_transitions_state_without_nexus_session() {
    // Another BlueZ client (e.g., bluetoothctl) started its own
    // discovery — BlueZ emits Discovering=true. Nexus observes this
    // and transitions the adapter state, but its own
    // `nexus_has_discovery_session` flag stays false.
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(32);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            1,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    await_event(&mut event_rx, |e| {
        matches!(e, NexusEvent::BtAdapterChanged { .. })
    })
    .await;

    // No `StartDiscovery` command — just a property flip, as though
    // a concurrent client started a session.
    mock.publish_adapter_props("/org/bluez/hci0", true, true)
        .await;
    await_event(&mut event_rx, |e| {
        matches!(
            e,
            NexusEvent::BtAdapterChanged {
                discovering: true,
                ..
            }
        )
    })
    .await;

    // The call list should not contain a StartDiscovery — Nexus
    // didn't initiate anything.
    let calls = mock.state().calls;
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, MockCall::StartDiscovery(_, _)))
    );

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn discovery_session_auto_stops_after_timeout() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(32);
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();

    let config = BluetoothConfig {
        discovery_timeout_s: 1, // short for test
        ..BluetoothConfig::default()
    };

    let handle = spawn_bluetooth_backend(client, store, event_tx.clone(), config);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            1,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    // Commands are biased over events on the backend's select! — give
    // the event loop a chance to observe the InterfaceDiscovered
    // before the StartDiscovery command lands, otherwise the
    // `nexus_has_discovery_session` flag wouldn't have an adapter
    // entry to attach to.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::StartDiscovery {
            adapter: "/org/bluez/hci0".into(),
            filter: DiscoveryFilter::default(),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    // Wait past the timeout + two reconcile ticks.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    let calls = mock.state().calls;
    let stops = calls
        .iter()
        .filter(|c| matches!(c, MockCall::StopDiscovery(_)))
        .count();
    assert!(
        stops >= 1,
        "expected the reconcile tick to auto-stop discovery; calls = {calls:?}"
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
