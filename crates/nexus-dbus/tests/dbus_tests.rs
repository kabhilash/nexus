//! In-process tests against the D-Bus surface.
//!
//! Each test launches its own D-Bus daemon via `dbus-run-session`
//! (a wrapper that starts a fresh session bus, runs a command
//! inside it, then tears it down) — except we drive it from code.
//! The session bus is reached by setting `DBUS_SESSION_BUS_ADDRESS`
//! in the test process. `tempfile` plus a random socket path is
//! the typical pattern; the simplest thing that actually works is
//! launching `dbus-daemon --session --print-address --nofork` and
//! consuming its stdout.
//!
//! Every test uses a *unique* bus name suffix to avoid owner
//! conflicts when tests run in parallel.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, OperState};
use nexus_dbus::{DbusConfig, spawn_dbus_service};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::broadcast;
use zbus::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

// ---------------------------------------------------------------------------
// Session-bus harness
// ---------------------------------------------------------------------------

struct Bus {
    addr: String,
    _process: tokio::process::Child,
}

impl Bus {
    async fn spawn() -> Self {
        let mut child = Command::new("dbus-daemon")
            .arg("--session")
            .arg("--print-address")
            .arg("--nofork")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn dbus-daemon; is it installed?");

        let stdout = child.stdout.take().expect("dbus-daemon stdout");
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line = String::new();
        use tokio::io::AsyncBufReadExt;
        reader
            .read_line(&mut line)
            .await
            .expect("dbus-daemon produced no address");
        let addr = line.trim().to_owned();
        Bus {
            addr,
            _process: child,
        }
    }

    async fn connection(&self) -> Connection {
        zbus::connection::Builder::address(self.addr.as_str())
            .unwrap()
            .build()
            .await
            .expect("connect to ephemeral bus")
    }
}

async fn start_store() -> (TempDir, Arc<dyn ProfileStore>) {
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x11u8; 32]);
    let store = ProfileFileStore::open(tmp.path(), &keys).unwrap();
    (tmp, Arc::new(store))
}

fn eth_info(ifname: &str, ifindex: u32) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: ifname.into(),
        mac: [0x02, 0, 0, 0, 0, ifindex as u8],
        mtu: 1500,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Ethernet,
        discovered_at: std::time::Instant::now(),
    }
}

async fn spawn_service(bus: &Bus, bus_name: &str) -> nexus_dbus::DbusServiceHandle {
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let config = DbusConfig {
        bus_name: bus_name.to_owned(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth: nexus_dbus::always_allow(),
        ops: nexus_dbus::NoopOps::arc(),
        rate_limits: nexus_dbus::RateLimits::default(),
        enabled_features: nexus_dbus::EnabledFeatures::default(),
    };
    spawn_dbus_service(event_tx.subscribe(), store, config)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_managed_objects_returns_manager_root() {
    let bus = Bus::spawn().await;
    // Give dbus-daemon a moment to be ready for connections.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let handle = spawn_service(&bus, "fi.nexus1.test_root").await;
    let client = bus.connection().await;

    let reply = client
        .call_method(
            Some("fi.nexus1.test_root"),
            "/fi/nexus1",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await
        .expect("GetManagedObjects");

    let body = reply.body();
    let objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
        body.deserialize().expect("decode GetManagedObjects reply");

    // Manager root should be present.
    let root = zbus::zvariant::OwnedObjectPath::try_from("/fi/nexus1").unwrap();
    let mgr_ifaces = objects.get(&root).expect("Manager root object present");
    assert!(mgr_ifaces.contains_key("fi.nexus.Manager"));

    handle.stop().await;
}

#[tokio::test]
async fn interface_appears_in_managed_objects_after_event() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Build the service + get a handle on the event_tx so the test
    // can publish InterfaceDiscovered.
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let config = DbusConfig {
        bus_name: "fi.nexus1.test_iface".into(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth: nexus_dbus::always_allow(),
        ops: nexus_dbus::NoopOps::arc(),
        rate_limits: nexus_dbus::RateLimits::default(),
        enabled_features: nexus_dbus::EnabledFeatures::default(),
    };
    let handle = spawn_dbus_service(event_tx.subscribe(), store, config)
        .await
        .unwrap();

    // Announce an Ethernet interface.
    event_tx
        .send(NexusEvent::InterfaceDiscovered(eth_info("eth0", 2)))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some("fi.nexus1.test_iface"),
            "/fi/nexus1",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await
        .expect("GetManagedObjects");
    let body = reply.body();
    let objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
        body.deserialize().expect("decode GetManagedObjects reply");
    let iface_path = OwnedObjectPath::try_from("/fi/nexus1/interface/eth0").unwrap();
    let iface_ifaces = objects.get(&iface_path).expect("eth0 present");
    assert!(iface_ifaces.contains_key("fi.nexus.Interface"));
    assert!(iface_ifaces.contains_key("fi.nexus.Ethernet"));

    handle.stop().await;
}

#[tokio::test]
async fn manager_properties_are_readable() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let handle = spawn_service(&bus, "fi.nexus1.test_props").await;
    let client = bus.connection().await;

    let props = client
        .call_method(
            Some("fi.nexus1.test_props"),
            "/fi/nexus1",
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("fi.nexus.Manager",),
        )
        .await
        .expect("GetAll Manager");
    let dict: HashMap<String, OwnedValue> = props.body().deserialize().unwrap();
    assert!(dict.contains_key("Version"));
    assert!(dict.contains_key("PowerState"));
    assert!(dict.contains_key("Interfaces"));
    assert!(dict.contains_key("WifiProfiles"));

    handle.stop().await;
}

#[tokio::test]
async fn get_manager_status_returns_dict() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let handle = spawn_service(&bus, "fi.nexus1.test_status").await;
    let client = bus.connection().await;

    let reply = client
        .call_method(
            Some("fi.nexus1.test_status"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "GetManagerStatus",
            &(),
        )
        .await
        .unwrap();
    let body = reply.body();
    let dict: HashMap<String, OwnedValue> = body.deserialize().unwrap();
    assert!(dict.contains_key("Version"));
    assert!(dict.contains_key("PowerState"));
    assert!(dict.contains_key("ApiCapabilities"));

    handle.stop().await;
}

#[tokio::test]
async fn get_interface_returns_notfound_when_unknown() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let handle = spawn_service(&bus, "fi.nexus1.test_getifnf").await;
    let client = bus.connection().await;

    let err = client
        .call_method(
            Some("fi.nexus1.test_getifnf"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "GetInterface",
            &("nope0",),
        )
        .await
        .expect_err("unknown interface should error");
    let s = format!("{err:?}");
    assert!(
        s.contains("fi.nexus.Error.NotFound") || s.contains("NotFound"),
        "expected NotFound, got {s}"
    );

    handle.stop().await;
}

#[tokio::test]
async fn wifi_interface_properties_readable() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let handle = spawn_dbus_service(
        event_tx.subscribe(),
        store,
        DbusConfig {
            bus_name: "fi.nexus1.test_wifi".into(),
            use_session_bus: false,
            address: Some(bus.addr.clone()),
            version: "0.1.0-test".into(),
            auth: nexus_dbus::always_allow(),
            ops: nexus_dbus::NoopOps::arc(),
            rate_limits: nexus_dbus::RateLimits::default(),
            enabled_features: nexus_dbus::EnabledFeatures::default(),
        },
    )
    .await
    .unwrap();

    // Announce a wireless interface — minimum required shape.
    let info = InterfaceInfo {
        ifindex: 3,
        ifname: "wlp2s0".into(),
        mac: [0; 6],
        mtu: 0,
        operstate: OperState::Dormant,
        carrier: false,
        kind: InterfaceKind::Wireless {
            wiphy: 0,
            wiphy_name: "phy0".into(),
            wdev: 1,
            iftype: nexus_core::Nl80211IfType(2),
            capabilities: Arc::new(nexus_core::PhyCapabilities::default()),
        },
        discovered_at: std::time::Instant::now(),
    };
    event_tx
        .send(NexusEvent::InterfaceDiscovered(info))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some("fi.nexus1.test_wifi"),
            "/fi/nexus1/interface/wlp2s0",
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("fi.nexus.Wifi",),
        )
        .await
        .expect("GetAll fi.nexus.Wifi");
    let props: HashMap<String, OwnedValue> = reply.body().deserialize().unwrap();
    assert!(props.contains_key("State"));
    assert!(props.contains_key("SignalDbm"));
    assert!(props.contains_key("Frequency"));
    assert!(props.contains_key("ConnectedBss"));

    handle.stop().await;
}

#[tokio::test]
async fn bluetooth_interface_properties_readable() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let handle = spawn_dbus_service(
        event_tx.subscribe(),
        store,
        DbusConfig {
            bus_name: "fi.nexus1.test_bt".into(),
            use_session_bus: false,
            address: Some(bus.addr.clone()),
            version: "0.1.0-test".into(),
            auth: nexus_dbus::always_allow(),
            ops: nexus_dbus::NoopOps::arc(),
            rate_limits: nexus_dbus::RateLimits::default(),
            enabled_features: nexus_dbus::EnabledFeatures::default(),
        },
    )
    .await
    .unwrap();

    let info = InterfaceInfo {
        ifindex: 4,
        ifname: "hci0".into(),
        mac: [0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13],
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Bluetooth {
            hci_name: "hci0".into(),
            hci_index: 0,
            bt_address: nexus_core::MacAddr([0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13]),
            bluez_path: "/org/bluez/hci0".into(),
        },
        discovered_at: std::time::Instant::now(),
    };
    event_tx
        .send(NexusEvent::InterfaceDiscovered(info))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some("fi.nexus1.test_bt"),
            "/fi/nexus1/interface/hci0",
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("fi.nexus.Bluetooth",),
        )
        .await
        .expect("GetAll fi.nexus.Bluetooth");
    let props: HashMap<String, OwnedValue> = reply.body().deserialize().unwrap();
    for key in [
        "Powered",
        "Discoverable",
        "Pairable",
        "Discovering",
        "NexusDiscovering",
        "State",
    ] {
        assert!(props.contains_key(key), "missing {key}");
    }

    handle.stop().await;
}

#[tokio::test]
async fn gnss_interface_properties_readable() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let handle = spawn_dbus_service(
        event_tx.subscribe(),
        store,
        DbusConfig {
            bus_name: "fi.nexus1.test_gnss".into(),
            use_session_bus: false,
            address: Some(bus.addr.clone()),
            version: "0.1.0-test".into(),
            auth: nexus_dbus::always_allow(),
            ops: nexus_dbus::NoopOps::arc(),
            rate_limits: nexus_dbus::RateLimits::default(),
            enabled_features: nexus_dbus::EnabledFeatures::default(),
        },
    )
    .await
    .unwrap();

    let info = InterfaceInfo {
        ifindex: 5,
        ifname: "/dev/ttyUSB0".into(),
        mac: [0; 6],
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Gnss {
            device_path: "/dev/ttyUSB0".into(),
            gpsd_device: "/dev/ttyUSB0".into(),
            vendor_model: Some("u-blox F9P".into()),
        },
        discovered_at: std::time::Instant::now(),
    };
    event_tx
        .send(NexusEvent::InterfaceDiscovered(info))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = bus.connection().await;
    // The path uses escaping — /dev/ttyUSB0 → _2fdev_2fttyUSB0.
    let path = nexus_dbus::interface_path("/dev/ttyUSB0");
    let reply = client
        .call_method(
            Some("fi.nexus1.test_gnss"),
            path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("fi.nexus.Gnss",),
        )
        .await
        .expect("GetAll fi.nexus.Gnss");
    let props: HashMap<String, OwnedValue> = reply.body().deserialize().unwrap();
    for key in [
        "State",
        "DevicePath",
        "VendorModel",
        "LastFix",
        "SatellitesInView",
        "GpsdConnected",
    ] {
        assert!(props.contains_key(key), "missing {key}");
    }

    handle.stop().await;
}

#[tokio::test]
async fn wifi_profile_with_credentials_exposes_has_credentials() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    unsafe {
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &bus.addr);
    }
    // Pre-seed a Wi-Fi profile so we know its ULID.
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x44u8; 32]);
    let store = Arc::new(ProfileFileStore::open(tmp.path(), &keys).unwrap());
    use nexus_profile_store::{
        ProfileMetadata, SecretString, SecurityConfig, WifiNetworkSettings, WifiProfile, WpaPsk,
    };
    let profile = WifiProfile {
        id: ulid::Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        network: WifiNetworkSettings {
            ssid: nexus_core::Ssid::new(b"corp".to_vec()).unwrap(),
            hidden: false,
            priority: 10,
            auto_connect: true,
            fast_transition: false,
            security: SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(SecretString::from("hunter2")),
            },
            bssid_preferred: None,
            bssid_blacklist: Vec::new(),
            scan_freqs: Vec::new(),
            credentials_invalid: false,
        },
    };
    store.put_wifi(&profile).await.unwrap();
    let id = profile.id;

    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let store_arc: Arc<dyn ProfileStore> = store;
    let handle = spawn_dbus_service(
        event_tx.subscribe(),
        store_arc,
        DbusConfig {
            bus_name: "fi.nexus1.test_prof".into(),
            use_session_bus: false,
            address: Some(bus.addr.clone()),
            version: "0.1.0-test".into(),
            auth: nexus_dbus::always_allow(),
            ops: nexus_dbus::NoopOps::arc(),
            rate_limits: nexus_dbus::RateLimits::default(),
            enabled_features: nexus_dbus::EnabledFeatures::default(),
        },
    )
    .await
    .unwrap();

    let client = bus.connection().await;
    let path = nexus_dbus::wifi_profile_path(&id);
    // Use `Properties.GetAll` — decoding a single `Variant` of
    // `a{sb}` from `Properties.Get` in zbus 5 is awkward because
    // the outer variant wraps the dict; `GetAll` delivers the
    // dict as an OwnedValue directly.
    let reply = client
        .call_method(
            Some("fi.nexus1.test_prof"),
            path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("fi.nexus.Profile.Wifi",),
        )
        .await
        .expect("GetAll wifi profile");
    let dict: HashMap<String, OwnedValue> = reply.body().deserialize().unwrap();
    assert!(dict.contains_key("Ssid"));
    assert!(dict.contains_key("HasCredentials"));

    // Silence unused-binding warning on the seed id.
    let _ = id;
    handle.stop().await;
}
