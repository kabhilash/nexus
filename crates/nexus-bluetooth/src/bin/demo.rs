//! Connects to the real system BlueZ, lists adapters that appear
//! through ObjectManager, and runs a 10-second discovery session on
//! the first powered adapter it sees. See DD-004 §15 phase 4 exit
//! criterion.
//!
//! Run with a live BlueZ:
//!     cargo run --bin nexus-bluetooth-demo
//!
//! Graceful on failure — if the system bus or BlueZ isn't
//! reachable, prints a friendly message and exits 0. The demo is
//! advisory; automated CI doesn't depend on it.

use std::sync::Arc;
use std::time::Duration;

use nexus_bluetooth::{BluetoothConfig, BluezClient, DiscoveryFilter, ZbusBluezClient};
use nexus_core::NexusEvent;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(64);
    let client = Arc::new(ZbusBluezClient::new(event_tx.clone()));

    println!("connecting to BlueZ on system bus…");
    if let Err(e) = client.connect().await {
        eprintln!("could not reach BlueZ — is the daemon running? ({e})");
        return;
    }
    println!("connected to bluez");

    // Collect the initial ObjectManager republish for 500 ms.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
    let mut adapters: Vec<String> = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Ok(NexusEvent::BtAdapterChanged { adapter, .. })) => {
                if !adapters.contains(&adapter) {
                    adapters.push(adapter);
                }
            }
            Ok(Ok(_)) | Ok(Err(_)) | Err(_) => break,
        }
    }
    if adapters.is_empty() {
        println!("no adapters visible under /org/bluez");
        return;
    }
    for adapter in &adapters {
        println!("adapter: {adapter}");
    }

    // Run a 10-second discovery session on the first one.
    let target = adapters[0].clone();
    let _ = client.set_powered(&target, true).await;
    if let Err(e) = client
        .start_discovery(&target, DiscoveryFilter::default())
        .await
    {
        eprintln!("start_discovery({target}) failed: {e}");
        return;
    }
    println!("discovering on {target} for 10s…");
    let end = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = end.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Ok(NexusEvent::BtDeviceDiscovered(info))) => {
                println!(
                    "device: {} ({:?}) rssi={:?}",
                    info.address_str(),
                    info.transport,
                    info.rssi,
                );
            }
            Ok(Ok(NexusEvent::BtDeviceConnected { adapter, address })) => {
                println!("connected: {} on {adapter}", address_str(&address));
            }
            Ok(Ok(NexusEvent::BtDeviceDisconnected { adapter, address })) => {
                println!("disconnected: {} on {adapter}", address_str(&address));
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }
    let _ = client.stop_discovery(&target).await;
    let _ = BluetoothConfig::default(); // silence unused-import warning
    println!("done");
}

fn address_str(a: &nexus_core::MacAddr) -> String {
    <nexus_core::MacAddr as nexus_core::BluetoothAddrExt>::to_bluez(a)
}

// Convenience trait for the demo only — BtDeviceInfo doesn't carry
// a to_bluez helper.
trait BtDeviceInfoExt {
    fn address_str(&self) -> String;
}

impl BtDeviceInfoExt for nexus_core::BtDeviceInfo {
    fn address_str(&self) -> String {
        address_str(&self.address)
    }
}
