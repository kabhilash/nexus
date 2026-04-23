//! Connects to a running gpsd on 127.0.0.1:2947 and prints every
//! `GnssFixChanged` (and `GnssTpvReceived` before filtering) it
//! sees. Graceful on failure — if gpsd isn't reachable, prints a
//! friendly message and exits 0.
//!
//! Run with a live gpsd:
//!     cargo run --bin nexus-gnss-demo
//!
//! Or against gpsfake replaying an NMEA log:
//!     gpsfake /path/to/nmea.log &
//!     cargo run --bin nexus-gnss-demo

use std::sync::Arc;
use std::time::Duration;

use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, OperState};
use nexus_gnss::{GnssConfig, JsonGpsdClient, spawn_gnss_backend};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::sync::broadcast;

fn synth_interface(device_path: &str, ifindex: u32) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: device_path.to_owned(),
        mac: [0; 6],
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Gnss {
            device_path: device_path.to_owned(),
            gpsd_device: device_path.to_owned(),
            vendor_model: None,
        },
        discovered_at: std::time::Instant::now(),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();

    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(256);

    let gpsd = Arc::new(JsonGpsdClient::localhost(event_tx.clone()));
    // Profile store is required by the backend's new(); the demo
    // just uses a throwaway in-memory one.
    let tmp = TempDir::new().expect("tempdir");
    let keys = InMemoryKeySource::new([0x55u8; 32]);
    let store: Arc<dyn ProfileStore> =
        Arc::new(ProfileFileStore::open(tmp.path(), &keys).expect("store"));

    let _handle = spawn_gnss_backend(gpsd, store, event_tx.clone(), GnssConfig::default());

    // Synthesize an InterfaceDiscovered for the conventional
    // device path — in production, DD-001's udev pump supplies this.
    // The demo gives operators a way to see events without needing
    // to wire up the whole stack.
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/ttyUSB0".into());
    let _ = event_tx.send(NexusEvent::InterfaceDiscovered(synth_interface(&path, 1)));

    println!("subscribed to nexus event bus; waiting for gpsd events…");
    println!("(set GNSS_DEMO_TIMEOUT_S to bound the run; default = 600)");
    let deadline = std::time::Instant::now()
        + Duration::from_secs(
            std::env::var("GNSS_DEMO_TIMEOUT_S")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(600),
        );
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Ok(NexusEvent::GnssGpsdConnected)) => println!("gpsd connected"),
            Ok(Ok(NexusEvent::GnssGpsdDisconnected)) => println!("gpsd disconnected"),
            Ok(Ok(NexusEvent::GnssFixChanged { device, fix })) => {
                println!(
                    "[fix] {device} {:?} lat={:?} lon={:?} alt={:?}m eph={:?} sats={}",
                    fix.mode,
                    fix.latitude,
                    fix.longitude,
                    fix.altitude_m,
                    fix.horizontal_error_m,
                    fix.satellites_used
                );
            }
            Ok(Ok(NexusEvent::GnssTpvReceived { device, fix })) => {
                println!(
                    "[tpv] {device} {:?} lat={:?} lon={:?} sats={}",
                    fix.mode, fix.latitude, fix.longitude, fix.satellites_used
                );
            }
            Ok(Ok(NexusEvent::GnssSatellites { device, satellites })) => {
                println!("[sky] {device} {} satellites", satellites.len());
            }
            Ok(Ok(_)) => {}
            Ok(Err(_)) | Err(_) => break,
        }
    }
    println!("demo finished");
}
