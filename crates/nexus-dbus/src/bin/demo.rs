//! Spawns `fi.nexus1` on the session bus and prints the
//! `GetManagedObjects` tree. Fails gracefully if no D-Bus daemon is
//! reachable.
//!
//! Run:
//!     cargo run --bin nexus-dbus-demo
//!     dbus-send --session --print-reply \
//!         --dest=fi.nexus1 /fi/nexus1 \
//!         org.freedesktop.DBus.ObjectManager.GetManagedObjects

use std::sync::Arc;
use std::time::Duration;

use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, OperState};
use nexus_dbus::{DbusConfig, spawn_dbus_service};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::sync::broadcast;

fn eth0_info() -> InterfaceInfo {
    InterfaceInfo {
        ifindex: 2,
        ifname: "eth0".into(),
        mac: [0x02, 0, 0, 0, 0, 2],
        mtu: 1500,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Ethernet,
        discovered_at: std::time::Instant::now(),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().init();

    let (event_tx, _event_rx) = broadcast::channel::<NexusEvent>(64);
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x11u8; 32]);
    let store: Arc<dyn ProfileStore> = Arc::new(ProfileFileStore::open(tmp.path(), &keys).unwrap());

    let config = DbusConfig {
        bus_name: "fi.nexus1".into(),
        use_session_bus: true,
        address: None,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        // Demo binary doesn't talk to PolicyKit; allow everything
        // so `busctl` can poke around. Production wires
        // `PolicyKitChecker::new(connection)`.
        auth: nexus_dbus::always_allow(),
        ops: nexus_dbus::NoopOps::arc(),
        rate_limits: nexus_dbus::RateLimits::default(),
        enabled_features: nexus_dbus::EnabledFeatures::default(),
        ethernet_auth_backend: "none".to_owned(),
        wifi_supplicant: "wpa_supplicant".to_owned(),
        wifi_roaming_mode: "supplicant".to_owned(),
    };
    let handle = match spawn_dbus_service(event_tx.subscribe(), store, config).await {
        Ok(h) => h,
        Err(e) => {
            eprintln!("could not register on session bus: {e}");
            return;
        }
    };
    println!("nexus-dbus demo: serving fi.nexus1 at /fi/nexus1 on the session bus");

    // Seed one interface so clients see a non-empty tree.
    let _ = event_tx.send(NexusEvent::InterfaceDiscovered(eth0_info()));
    tokio::time::sleep(Duration::from_millis(100)).await;

    println!("try: busctl --user introspect fi.nexus1 /fi/nexus1");
    println!("press Ctrl-C to exit");
    let _ = tokio::signal::ctrl_c().await;
    handle.stop().await;
}
