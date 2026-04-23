//! Integration tests against a real system BlueZ. Gated behind the
//! `integration-bluez` feature; the `#[ignore]` attribute keeps
//! them out of the default test run even when the feature is on,
//! so `cargo test --features integration-bluez -- --ignored` is
//! the explicit invocation.
//!
//! See DD-004 §§14.2, 14.3.

#![cfg(feature = "integration-bluez")]

use std::sync::Arc;
use std::time::Duration;

use nexus_bluetooth::bluez::BluezClient;
use nexus_bluetooth::{BluetoothConfig, DiscoveryFilter, ZbusBluezClient, spawn_bluetooth_backend};
use nexus_core::NexusEvent;
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::sync::broadcast;

async fn start_store() -> (TempDir, Arc<dyn ProfileStore>) {
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x77u8; 32]);
    let store = ProfileFileStore::open(tmp.path(), &keys).unwrap();
    (tmp, Arc::new(store))
}

/// Smoke: connect to BlueZ and verify at least one adapter is
/// visible. Requires a live BlueZ daemon.
#[tokio::test]
#[ignore]
async fn real_bluez_publishes_at_least_one_adapter() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;

    let client = Arc::new(ZbusBluezClient::new(event_tx.clone()));
    let bluez: Arc<dyn BluezClient> = client.clone();
    if let Err(e) = client.connect().await {
        panic!("BlueZ not reachable: {e}");
    }
    let handle =
        spawn_bluetooth_backend(bluez, store, event_tx.clone(), BluetoothConfig::default());

    // Wait up to 2 s for a BtAdapterChanged event.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut seen_any = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Ok(NexusEvent::BtAdapterChanged { .. })) => {
                seen_any = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(seen_any, "no adapter surfaced by BlueZ in 2 s");

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// Drive a 5 s discovery session against a real adapter and emit
/// observed peers. Non-assertive; a smoke test that confirms the
/// session lifecycle works against BlueZ.
#[tokio::test]
#[ignore]
async fn real_bluez_discovery_session_runs_and_stops() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;

    let client = Arc::new(ZbusBluezClient::new(event_tx.clone()));
    let bluez: Arc<dyn BluezClient> = client.clone();
    client.connect().await.expect("bluez connect");
    let handle =
        spawn_bluetooth_backend(bluez, store, event_tx.clone(), BluetoothConfig::default());

    // Pick the first adapter we see.
    let adapter = loop {
        match tokio::time::timeout(Duration::from_secs(2), event_rx.recv()).await {
            Ok(Ok(NexusEvent::BtAdapterChanged { adapter, .. })) => break adapter,
            Ok(Ok(_)) => continue,
            _ => panic!("no adapter seen"),
        }
    };

    // Power it on and kick discovery.
    client.set_powered(&adapter, true).await.unwrap();
    client
        .start_discovery(&adapter, DiscoveryFilter::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    client.stop_discovery(&adapter).await.unwrap();

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}
