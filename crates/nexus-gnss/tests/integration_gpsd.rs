//! Integration tests against a real gpsd on 127.0.0.1:2947. Gated
//! behind the `integration-gpsd` feature; `#[ignore]` so they only
//! run when explicitly requested (`cargo test --features
//! integration-gpsd -- --ignored`).
//!
//! On a machine with gpsd running (either against a real receiver
//! or via `gpsfake`), these tests verify:
//!
//! 1. VERSION handshake succeeds;
//! 2. the backend emits `GnssGpsdConnected` shortly after spawn;
//! 3. if the gpsd instance has at least one active device, at
//!    least one `GnssTpvReceived` arrives within 60 s.

#![cfg(feature = "integration-gpsd")]

use std::sync::Arc;
use std::time::Duration;

use nexus_core::NexusEvent;
use nexus_gnss::{GnssConfig, JsonGpsdClient, spawn_gnss_backend};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::sync::broadcast;

async fn start_store() -> (TempDir, Arc<dyn ProfileStore>) {
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x99u8; 32]);
    let store = ProfileFileStore::open(tmp.path(), &keys).unwrap();
    (tmp, Arc::new(store))
}

#[tokio::test]
#[ignore]
async fn real_gpsd_version_handshake_and_connected_event() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let client = Arc::new(JsonGpsdClient::localhost(event_tx.clone()));
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    // Expect GnssGpsdConnected within 2 s.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Ok(NexusEvent::GnssGpsdConnected)) =
            tokio::time::timeout(remaining, event_rx.recv()).await
        {
            saw = true;
            break;
        }
    }
    assert!(saw, "no GnssGpsdConnected observed — is gpsd running?");
    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
#[ignore]
async fn real_gpsd_streams_at_least_one_tpv_in_60s() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(256);
    let (_tmp, store) = start_store().await;
    let client = Arc::new(JsonGpsdClient::localhost(event_tx.clone()));
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut saw_tpv = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if let Ok(Ok(event)) = tokio::time::timeout(remaining, event_rx.recv()).await {
            if matches!(event, NexusEvent::GnssTpvReceived { .. }) {
                saw_tpv = true;
                break;
            }
        } else {
            break;
        }
    }
    assert!(saw_tpv, "no TPV in 60 s — gpsd/receiver configured?");
    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}
