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

use futures_util::StreamExt;
use nexus_core::{
    InterfaceInfo, InterfaceKind, NexusEvent, NotificationData, NotificationValue, OperState,
};
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
        ethernet_auth_backend: "none".to_owned(),
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
        ethernet_auth_backend: "none".to_owned(),
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
            ethernet_auth_backend: "none".to_owned(),
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
            ethernet_auth_backend: "none".to_owned(),
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
            ethernet_auth_backend: "none".to_owned(),
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
            last_connected_at: None,
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
            ethernet_auth_backend: "none".to_owned(),
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

#[tokio::test]
async fn ethernet_lifecycle_emits_interface_state_changed_signal() {
    // DD-006 §6.2 + §9: ethernet has no technology-specific signals;
    // lifecycle transitions surface via
    // `fi.nexus.Interface.StateChanged(new_state, details)` with
    // `details["reason"]` on `auth_failed` and
    // `details["eap_method"]` on `authenticating`.
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let config = DbusConfig {
        bus_name: "fi.nexus1.test_eth_state_signal".into(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth: nexus_dbus::always_allow(),
        ops: nexus_dbus::NoopOps::arc(),
        rate_limits: nexus_dbus::RateLimits::default(),
        enabled_features: nexus_dbus::EnabledFeatures::default(),
        ethernet_auth_backend: "wpa_supplicant".to_owned(),
    };
    let handle = spawn_dbus_service(event_tx.subscribe(), store, config)
        .await
        .unwrap();

    event_tx
        .send(NexusEvent::InterfaceDiscovered(eth_info("eth0", 7)))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    let client = bus.connection().await;

    // Subscribe BEFORE sending the lifecycle event so we don't race.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("fi.nexus.Interface")
        .unwrap()
        .member("StateChanged")
        .unwrap()
        .path("/fi/nexus1/interface/eth0")
        .unwrap()
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &client, None)
        .await
        .expect("subscribe StateChanged");

    // Drive the backend through an authenticating → auth_failed
    // transition with a populated profile so eap_method + reason
    // surface.
    event_tx
        .send(NexusEvent::EthLifecycleStateChanged {
            ifindex: 7,
            state: "authenticating".into(),
            eap_method: Some("PEAP".into()),
            auth_failure_reason: None,
        })
        .unwrap();
    event_tx
        .send(NexusEvent::EthLifecycleStateChanged {
            ifindex: 7,
            state: "auth_failed".into(),
            eap_method: Some("PEAP".into()),
            auth_failure_reason: Some("bad_credentials".into()),
        })
        .unwrap();

    let mut authenticating: Option<HashMap<String, OwnedValue>> = None;
    let mut auth_failed: Option<HashMap<String, OwnedValue>> = None;
    let deadline = std::time::Instant::now() + Duration::from_millis(800);
    while std::time::Instant::now() < deadline
        && (authenticating.is_none() || auth_failed.is_none())
    {
        let remaining =
            deadline.saturating_duration_since(std::time::Instant::now()) + Duration::from_millis(1);
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                let body = msg.body();
                if let Ok((new_state, details)) =
                    body.deserialize::<(String, HashMap<String, OwnedValue>)>()
                {
                    match new_state.as_str() {
                        "authenticating" => authenticating = Some(details),
                        "auth_failed" => auth_failed = Some(details),
                        _ => {}
                    }
                }
            }
            _ => break,
        }
    }

    let auth_details = authenticating.expect("StateChanged(authenticating) not received");
    let eap = auth_details
        .get("eap_method")
        .expect("authenticating details must include eap_method");
    let eap_str: &str = eap.downcast_ref().expect("eap_method is a string");
    assert_eq!(eap_str, "PEAP");
    assert!(
        !auth_details.contains_key("reason"),
        "authenticating must not carry reason",
    );

    let fail_details = auth_failed.expect("StateChanged(auth_failed) not received");
    let reason = fail_details
        .get("reason")
        .expect("auth_failed details must include reason");
    let reason_str: &str = reason.downcast_ref().expect("reason is a string");
    assert_eq!(reason_str, "bad_credentials");
    assert!(
        !fail_details.contains_key("eap_method"),
        "auth_failed details follow DD-006 §9 table; eap_method only on authenticating",
    );

    handle.stop().await;
}

#[tokio::test]
async fn operator_notification_fires_manager_notification_event_signal() {
    // DD-006 §5.3 / §9: a `NexusEvent::OperatorNotification` from any
    // backend translates to `fi.nexus.Manager.NotificationEvent`
    // (audit #9). Without this bridge, the gnss/eth/etc. notifications
    // never reach operator UIs.
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let config = DbusConfig {
        bus_name: "fi.nexus1.test_notif_signal".into(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth: nexus_dbus::always_allow(),
        ops: nexus_dbus::NoopOps::arc(),
        rate_limits: nexus_dbus::RateLimits::default(),
        enabled_features: nexus_dbus::EnabledFeatures::default(),
        ethernet_auth_backend: "none".to_owned(),
    };
    let handle = spawn_dbus_service(event_tx.subscribe(), store, config)
        .await
        .unwrap();

    let client = bus.connection().await;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("fi.nexus.Manager")
        .unwrap()
        .member("NotificationEvent")
        .unwrap()
        .path("/fi/nexus1")
        .unwrap()
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &client, None)
        .await
        .expect("subscribe NotificationEvent");

    let mut data = NotificationData::default();
    data.insert("ifname", NotificationValue::String("eth0".into()));
    data.insert(
        "reason",
        NotificationValue::String("certificate_rejected".into()),
    );
    event_tx
        .send(NexusEvent::OperatorNotification {
            kind: "eth_credentials_invalid".into(),
            data,
        })
        .unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("timed out waiting for NotificationEvent")
        .expect("stream closed")
        .expect("decode signal");
    let body = msg.body();
    let (kind, payload): (String, HashMap<String, OwnedValue>) =
        body.deserialize().expect("decode args");
    assert_eq!(kind, "eth_credentials_invalid");
    let ifname: &str = payload
        .get("ifname")
        .expect("ifname key")
        .downcast_ref()
        .expect("ifname is string");
    assert_eq!(ifname, "eth0");
    let reason: &str = payload
        .get("reason")
        .expect("reason key")
        .downcast_ref()
        .expect("reason is string");
    assert_eq!(reason, "certificate_rejected");

    handle.stop().await;
}
