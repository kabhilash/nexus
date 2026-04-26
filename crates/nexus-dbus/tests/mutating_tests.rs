//! Auth + dispatch coverage for the DD-006 phase 4-6 mutating
//! methods. Each method has a "denied" path (PolicyKit returns
//! Denied → caller sees `fi.nexus.Error.AuthFailed`) and an
//! "accepted" path (PolicyKit returns Authorized → backend op
//! is invoked).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use nexus_core::{
    BssCapabilities, BssInfo, DisconnectReason, InterfaceInfo, InterfaceKind, MacAddr,
    NexusEvent, OperState, SecurityMode, Ssid, WifiState,
};
use nexus_dbus::backend_ops::BackendOps;
use nexus_dbus::{
    DbusConfig, DbusError, NoopOps, PolicyMapChecker, RecordedCall, RecordingOps, RoamingMode,
    ScanParams, actions, always_allow, always_deny, spawn_dbus_service,
};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::broadcast;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

// ---------------------------------------------------------------------------
// Bus harness — same pattern as dbus_tests.rs.
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
            .expect("spawn dbus-daemon");
        let stdout = child.stdout.take().expect("dbus-daemon stdout");
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line = String::new();
        use tokio::io::AsyncBufReadExt;
        reader.read_line(&mut line).await.expect("addr");
        Bus {
            addr: line.trim().to_owned(),
            _process: child,
        }
    }

    async fn connection(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.addr.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }
}

async fn store() -> (TempDir, Arc<dyn ProfileStore>) {
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x33u8; 32]);
    let s = ProfileFileStore::open(tmp.path(), &keys).unwrap();
    (tmp, Arc::new(s))
}

fn wlan_info() -> InterfaceInfo {
    InterfaceInfo {
        ifindex: 3,
        ifname: "wlan0".into(),
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
    }
}

fn hci_info() -> InterfaceInfo {
    InterfaceInfo {
        ifindex: 0x8000_0000,
        ifname: "hci0".into(),
        mac: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01],
        mtu: 0,
        operstate: OperState::Up,
        carrier: false,
        kind: InterfaceKind::Bluetooth {
            hci_name: "hci0".into(),
            hci_index: 0,
            bt_address: MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01]),
            bluez_path: "/org/bluez/hci0".into(),
        },
        discovered_at: std::time::Instant::now(),
    }
}

async fn spawn(
    bus: &Bus,
    bus_name: &str,
    auth: Arc<dyn nexus_dbus::AuthChecker>,
    ops: Arc<dyn BackendOps>,
) -> (nexus_dbus::DbusServiceHandle, broadcast::Sender<NexusEvent>) {
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, st) = store().await;
    let cfg = DbusConfig {
        bus_name: bus_name.to_owned(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth,
        ops,
        rate_limits: nexus_dbus::RateLimits::default(),
        enabled_features: nexus_dbus::EnabledFeatures::default(),
        ethernet_auth_backend: "none".to_owned(),
        wifi_supplicant: "wpa_supplicant".to_owned(),
        wifi_roaming_mode: "supplicant".to_owned(),
    };
    let h = spawn_dbus_service(event_tx.subscribe(), st, cfg)
        .await
        .unwrap();
    (h, event_tx)
}

fn ssid_dict_value(s: &[u8]) -> OwnedValue {
    OwnedValue::try_from(Value::new(s.to_vec())).unwrap()
}

fn str_value(s: &str) -> OwnedValue {
    OwnedValue::try_from(Value::new(s.to_owned())).unwrap()
}

fn bool_value(b: bool) -> OwnedValue {
    OwnedValue::try_from(Value::new(b)).unwrap()
}

fn build_wifi_settings_dict() -> HashMap<String, OwnedValue> {
    // Build the inner security dict as a HashMap<String, Value>
    // (NOT OwnedValue), wrap it as a Value, then take an OwnedValue.
    // HashMap<K, V: Type> implements DynamicType; Dict<…> needs an
    // explicit signature in zbus 5, which is fiddly here.
    let mut sec: HashMap<String, Value<'_>> = HashMap::new();
    sec.insert("type".into(), Value::new("wpa2_personal".to_owned()));
    sec.insert("passphrase".into(), Value::new("hunter2hunter2".to_owned()));
    let security_value = OwnedValue::try_from(Value::new(sec)).unwrap();
    let mut s: HashMap<String, OwnedValue> = HashMap::new();
    s.insert("ssid".into(), ssid_dict_value(b"corp"));
    s.insert(
        "priority".into(),
        OwnedValue::try_from(Value::new(10i32)).unwrap(),
    );
    s.insert("auto_connect".into(), bool_value(true));
    s.insert("security".into(), security_value);
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_wifi_profile_denied_returns_auth_failed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_addwifi_deny",
        always_deny(),
        NoopOps::arc(),
    )
    .await;

    let client = bus.connection().await;
    let settings = build_wifi_settings_dict();
    let err = client
        .call_method(
            Some("fi.nexus1.test_addwifi_deny"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(settings,),
        )
        .await
        .expect_err("denied");
    let s = format!("{err:?}");
    assert!(s.contains("AuthFailed"), "expected AuthFailed; got {s}");
    handle.stop().await;
}

#[tokio::test]
async fn add_wifi_profile_accepted_persists_and_reads_back() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_addwifi_ok",
        always_allow(),
        NoopOps::arc(),
    )
    .await;

    let client = bus.connection().await;
    let settings = build_wifi_settings_dict();
    let reply = client
        .call_method(
            Some("fi.nexus1.test_addwifi_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(settings,),
        )
        .await
        .expect("AddWifiProfile");
    let path: OwnedObjectPath = reply.body().deserialize().unwrap();
    assert!(path.as_str().starts_with("/fi/nexus1/profile/wifi/"));

    // Confirm the profile shows up via FindWifiProfile.
    let lookup = client
        .call_method(
            Some("fi.nexus1.test_addwifi_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "FindWifiProfile",
            &(b"corp".to_vec(),),
        )
        .await
        .expect("FindWifiProfile");
    let found: OwnedObjectPath = lookup.body().deserialize().unwrap();
    assert_eq!(found, path);

    handle.stop().await;
}

#[tokio::test]
async fn add_wifi_profile_duplicate_returns_already_exists() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_addwifi_dup",
        always_allow(),
        NoopOps::arc(),
    )
    .await;
    let client = bus.connection().await;
    let settings = build_wifi_settings_dict();
    client
        .call_method(
            Some("fi.nexus1.test_addwifi_dup"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(settings.clone(),),
        )
        .await
        .expect("first add");
    let err = client
        .call_method(
            Some("fi.nexus1.test_addwifi_dup"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(settings,),
        )
        .await
        .expect_err("dup");
    let s = format!("{err:?}");
    assert!(s.contains("AlreadyExists"), "got {s}");
    handle.stop().await;
}

#[tokio::test]
async fn add_ethernet_profile_denied_returns_auth_failed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_addeth_deny",
        always_deny(),
        NoopOps::arc(),
    )
    .await;
    let client = bus.connection().await;
    let mut s: HashMap<String, OwnedValue> = HashMap::new();
    s.insert("ifname".into(), str_value("eth0"));
    s.insert("auto_connect".into(), bool_value(true));
    let err = client
        .call_method(
            Some("fi.nexus1.test_addeth_deny"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddEthernetProfile",
            &(s,),
        )
        .await
        .expect_err("denied");
    let s = format!("{err:?}");
    assert!(s.contains("AuthFailed"), "got {s}");
    handle.stop().await;
}

#[tokio::test]
async fn add_ethernet_profile_accepted_creates_object() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_addeth_ok",
        always_allow(),
        NoopOps::arc(),
    )
    .await;
    let client = bus.connection().await;
    let mut s: HashMap<String, OwnedValue> = HashMap::new();
    s.insert("ifname".into(), str_value("eth0"));
    s.insert("auto_connect".into(), bool_value(true));
    let reply = client
        .call_method(
            Some("fi.nexus1.test_addeth_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddEthernetProfile",
            &(s,),
        )
        .await
        .expect("AddEthernetProfile");
    let path: OwnedObjectPath = reply.body().deserialize().unwrap();
    assert!(path.as_str().starts_with("/fi/nexus1/profile/ethernet/"));
    handle.stop().await;
}

#[tokio::test]
async fn set_power_state_denied_returns_auth_failed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_power_deny",
        always_deny(),
        ops.clone(),
    )
    .await;
    let client = bus.connection().await;
    let err = client
        .call_method(
            Some("fi.nexus1.test_power_deny"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "SetPowerState",
            &("sleep",),
        )
        .await
        .expect_err("denied");
    let s = format!("{err:?}");
    assert!(s.contains("AuthFailed"), "got {s}");
    assert!(
        ops.calls().is_empty(),
        "ops should not be invoked when denied"
    );
    handle.stop().await;
}

#[tokio::test]
async fn set_power_state_accepted_propagates_to_ops() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, _tx) = spawn(&bus, "fi.nexus1.test_power_ok", always_allow(), ops.clone()).await;
    let client = bus.connection().await;
    client
        .call_method(
            Some("fi.nexus1.test_power_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "SetPowerState",
            &("background",),
        )
        .await
        .expect("SetPowerState");
    let calls = ops.calls();
    assert!(matches!(
        calls.first(),
        Some(RecordedCall::SetPowerState(
            nexus_dbus::PowerState::Background
        ))
    ));
    handle.stop().await;
}

#[tokio::test]
async fn wifi_scan_denied_does_not_call_backend() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_scan_deny", always_deny(), ops.clone()).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let err = client
        .call_method(
            Some("fi.nexus1.test_scan_deny"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Scan",
            &(empty,),
        )
        .await
        .expect_err("denied");
    let s = format!("{err:?}");
    assert!(s.contains("AuthFailed"), "got {s}");
    assert!(ops.calls().is_empty());
    handle.stop().await;
}

#[tokio::test]
async fn wifi_scan_accepted_dispatches_to_ops() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_scan_ok", always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut params: HashMap<String, OwnedValue> = HashMap::new();
    params.insert("active".into(), bool_value(true));
    client
        .call_method(
            Some("fi.nexus1.test_scan_ok"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Scan",
            &(params,),
        )
        .await
        .expect("Scan");
    let calls = ops.calls();
    assert!(
        matches!(
            calls.first(),
            Some(RecordedCall::WifiScan { ifname, params: ScanParams { active: true, .. } })
                if ifname == "wlan0"
        ),
        "got {calls:?}"
    );
    handle.stop().await;
}

#[tokio::test]
async fn wifi_disconnect_denied_returns_auth_failed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_disc_deny", always_deny(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = bus.connection().await;
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let err = client
        .call_method(
            Some("fi.nexus1.test_disc_deny"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Disconnect",
            &(empty,),
        )
        .await
        .expect_err("denied");
    let s = format!("{err:?}");
    assert!(s.contains("AuthFailed"), "got {s}");
    handle.stop().await;
}

#[tokio::test]
async fn wifi_disconnect_accepted_dispatches() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_disc_ok", always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = bus.connection().await;
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    client
        .call_method(
            Some("fi.nexus1.test_disc_ok"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Disconnect",
            &(empty,),
        )
        .await
        .expect("Disconnect");
    let calls = ops.calls();
    assert!(matches!(
        calls.first(),
        Some(RecordedCall::WifiDisconnect { ifname, pause_auto_connect: false }) if ifname == "wlan0"
    ));
    handle.stop().await;
}

#[tokio::test]
async fn wifi_disconnect_with_pause_flag_propagates() {
    // DD-006 §6.3 Wifi.Disconnect(params) — `pause_auto_connect`
    // forwards through to the BackendOps layer.
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_disc_pause", always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = bus.connection().await;
    let mut params: HashMap<String, OwnedValue> = HashMap::new();
    params.insert(
        "pause_auto_connect".to_owned(),
        OwnedValue::try_from(zbus::zvariant::Value::new(true)).unwrap(),
    );
    client
        .call_method(
            Some("fi.nexus1.test_disc_pause"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Disconnect",
            &(params,),
        )
        .await
        .expect("Disconnect");
    let calls = ops.calls();
    assert!(matches!(
        calls.first(),
        Some(RecordedCall::WifiDisconnect {
            ifname,
            pause_auto_connect: true,
        }) if ifname == "wlan0"
    ));
    handle.stop().await;
}

#[tokio::test]
async fn wifi_roam_with_bad_bssid_length_errors() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_roam_bad", always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = bus.connection().await;
    let err = client
        .call_method(
            Some("fi.nexus1.test_roam_bad"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Roam",
            &(vec![0u8, 1, 2],),
        )
        .await
        .expect_err("bad bssid");
    let s = format!("{err:?}");
    assert!(s.contains("InvalidArgument"), "got {s}");
    assert!(
        ops.calls()
            .iter()
            .all(|c| !matches!(c, RecordedCall::WifiRoam { .. })),
        "no Roam dispatch on bad input"
    );
    handle.stop().await;
}

#[tokio::test]
async fn wifi_roam_accepted_dispatches() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_roam_ok", always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = bus.connection().await;
    let bssid: Vec<u8> = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
    client
        .call_method(
            Some("fi.nexus1.test_roam_ok"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Roam",
            &(bssid.clone(),),
        )
        .await
        .expect("Roam");
    assert!(matches!(
        ops.calls().first(),
        Some(RecordedCall::WifiRoam { ifname, bssid: m })
            if ifname == "wlan0" && m.0 == [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]
    ));
    handle.stop().await;
}

#[tokio::test]
async fn wifi_connect_unknown_profile_returns_not_found() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_conn_nf", always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let client = bus.connection().await;
    let path: ObjectPath<'_> =
        ObjectPath::try_from("/fi/nexus1/profile/wifi/01HZZZZZZZZZZZZZZZZZZZZZZZ").unwrap();
    let err = client
        .call_method(
            Some("fi.nexus1.test_conn_nf"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Connect",
            &(OwnedObjectPath::from(path),),
        )
        .await
        .expect_err("nf");
    let s = format!("{err:?}");
    assert!(s.contains("NotFound"), "got {s}");
    assert!(ops.calls().is_empty());
    handle.stop().await;
}

#[tokio::test]
async fn wifi_connect_known_profile_dispatches() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) =
        spawn(&bus, "fi.nexus1.test_conn_ok", always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Add a Wi-Fi profile via D-Bus so we have a path to use.
    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some("fi.nexus1.test_conn_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(build_wifi_settings_dict(),),
        )
        .await
        .expect("AddWifiProfile");
    let path: OwnedObjectPath = reply.body().deserialize().unwrap();

    client
        .call_method(
            Some("fi.nexus1.test_conn_ok"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Connect",
            &(path,),
        )
        .await
        .expect("Connect");
    assert!(
        ops.calls()
            .iter()
            .any(|c| matches!(c, RecordedCall::WifiConnect { ifname, .. } if ifname == "wlan0"))
    );
    handle.stop().await;
}

#[tokio::test]
async fn remove_profile_denied_returns_auth_failed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Mixed policy: allow add, deny modify.
    let policy = Arc::new(PolicyMapChecker::new(nexus_dbus::AuthDecision::Denied));
    policy.allow(actions::PROFILE_ADD);
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_rm_deny",
        policy.clone(),
        NoopOps::arc(),
    )
    .await;
    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some("fi.nexus1.test_rm_deny"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(build_wifi_settings_dict(),),
        )
        .await
        .expect("AddWifiProfile");
    let path: OwnedObjectPath = reply.body().deserialize().unwrap();
    let err = client
        .call_method(
            Some("fi.nexus1.test_rm_deny"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "RemoveProfile",
            &(path,),
        )
        .await
        .expect_err("rm denied");
    let s = format!("{err:?}");
    assert!(s.contains("AuthFailed"), "got {s}");
    handle.stop().await;
}

#[tokio::test]
async fn remove_profile_accepted_unregisters_object() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(&bus, "fi.nexus1.test_rm_ok", always_allow(), NoopOps::arc()).await;
    let client = bus.connection().await;
    let add = client
        .call_method(
            Some("fi.nexus1.test_rm_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(build_wifi_settings_dict(),),
        )
        .await
        .expect("add");
    let path: OwnedObjectPath = add.body().deserialize().unwrap();
    client
        .call_method(
            Some("fi.nexus1.test_rm_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "RemoveProfile",
            &(path.clone(),),
        )
        .await
        .expect("rm");

    // After the removal command lands, give the registry loop a
    // tick and confirm the profile is gone.
    tokio::time::sleep(Duration::from_millis(80)).await;
    let err = client
        .call_method(
            Some("fi.nexus1.test_rm_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "FindWifiProfile",
            &(b"corp".to_vec(),),
        )
        .await
        .expect_err("find should fail after remove");
    let s = format!("{err:?}");
    assert!(s.contains("NotFound"), "got {s}");
    handle.stop().await;
}

#[tokio::test]
async fn profile_update_label_persists() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_upd_ok",
        always_allow(),
        NoopOps::arc(),
    )
    .await;
    let client = bus.connection().await;
    let add = client
        .call_method(
            Some("fi.nexus1.test_upd_ok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(build_wifi_settings_dict(),),
        )
        .await
        .expect("add");
    let path: OwnedObjectPath = add.body().deserialize().unwrap();

    let mut update: HashMap<String, OwnedValue> = HashMap::new();
    update.insert("label".into(), str_value("new label"));
    client
        .call_method(
            Some("fi.nexus1.test_upd_ok"),
            path.as_str(),
            Some("fi.nexus.Profile"),
            "Update",
            &(update,),
        )
        .await
        .expect("Update");

    let reply = client
        .call_method(
            Some("fi.nexus1.test_upd_ok"),
            path.as_str(),
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("fi.nexus.Profile", "Label"),
        )
        .await
        .expect("Get Label");
    let label_v: OwnedValue = reply.body().deserialize().unwrap();
    let label: &str = <&str>::try_from(&*label_v).unwrap();
    assert_eq!(label, "new label");
    handle.stop().await;
}

#[tokio::test]
async fn roaming_mode_parses() {
    // Sanity test for the public RoamingMode enum re-exported
    // alongside the mutating methods.
    assert_eq!(RoamingMode::parse("nexus"), Some(RoamingMode::Nexus));
    assert_eq!(RoamingMode::parse("none"), None);
}

#[tokio::test]
async fn wifi_supplicant_and_roaming_mode_properties_are_populated() {
    // Regression for the "nexusctl wifi show reports
    // Wifi.Supplicant / Wifi.RoamingMode = ''" side-note: the
    // configured supplicant kind and roam mode get stamped onto
    // every Wi-Fi interface's state on InterfaceDiscovered (DD-006
    // §6.3).
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, event_tx) = spawn(
        &bus,
        "fi.nexus1.test_sup_pop",
        always_allow(),
        NoopOps::arc(),
    )
    .await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let read_prop = |prop: &'static str| {
        let client = client.clone();
        async move {
            let r = client
                .call_method(
                    Some("fi.nexus1.test_sup_pop"),
                    "/fi/nexus1/interface/wlan0",
                    Some("org.freedesktop.DBus.Properties"),
                    "Get",
                    &("fi.nexus.Wifi", prop),
                )
                .await
                .expect("Get");
            let v: OwnedValue = r.body().deserialize().unwrap();
            <&str>::try_from(&*v).unwrap().to_owned()
        }
    };
    assert_eq!(read_prop("Supplicant").await, "wpa_supplicant");
    assert_eq!(read_prop("RoamingMode").await, "supplicant");
    handle.stop().await;
}

// ---------------------------------------------------------------------------
// DD-006 §6.3 / §9 — Wifi.Connect / Disconnect completion signals.
//
// Connect/Disconnect now return `(job_id: s)` and the terminal edge
// fires via `Wifi.ConnectComplete` / `Wifi.DisconnectComplete`. These
// tests exercise the per-branch reasons (success, credentials_invalid,
// handshake_timeout, rf_killed, cancelled) and the `Wifi.StateChanged`
// typed signal that mirrors `Interface.StateChanged`.
// ---------------------------------------------------------------------------

const WLAN_IFINDEX: u32 = 3;

/// Add a Wi-Fi profile via D-Bus and return its object path. Used to
/// give `Connect` something to point at.
async fn add_wifi_profile_get_path(
    bus: &Bus,
    bus_name: &str,
) -> OwnedObjectPath {
    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some(bus_name),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddWifiProfile",
            &(build_wifi_settings_dict(),),
        )
        .await
        .expect("AddWifiProfile");
    reply.body().deserialize().unwrap()
}

/// Subscribe to `fi.nexus.Wifi.<member>` signals on the given path.
async fn subscribe_wifi_signal(
    client: &zbus::Connection,
    member: &str,
) -> zbus::MessageStream {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("fi.nexus.Wifi")
        .unwrap()
        .member(member)
        .unwrap()
        .path("/fi/nexus1/interface/wlan0")
        .unwrap()
        .build();
    zbus::MessageStream::for_match_rule(rule, client, None)
        .await
        .expect("subscribe")
}

/// Wait for the next signal whose body deserializes as
/// `(String, bool, String)` (the ConnectComplete / DisconnectComplete
/// payload) and return it. Returns `None` on timeout.
async fn next_complete(
    stream: &mut zbus::MessageStream,
    timeout: Duration,
) -> Option<(String, bool, String)> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let remaining =
            deadline.saturating_duration_since(std::time::Instant::now()) + Duration::from_millis(1);
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Ok(payload) = msg.body().deserialize::<(String, bool, String)>() {
                    return Some(payload);
                }
            }
            _ => return None,
        }
    }
    None
}

/// Issue Connect for a freshly-added profile and return the job_id.
async fn issue_connect(bus: &Bus, bus_name: &str) -> String {
    let path = add_wifi_profile_get_path(bus, bus_name).await;
    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some(bus_name),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Connect",
            &(path,),
        )
        .await
        .expect("Connect");
    reply.body().deserialize::<String>().expect("job_id (s)")
}

#[tokio::test]
async fn wifi_connect_returns_job_id_and_emits_complete_on_connected() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let bus_name = "fi.nexus1.test_cc_ok";
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut stream = subscribe_wifi_signal(&client, "ConnectComplete").await;

    let job_id = issue_connect(&bus, bus_name).await;
    assert!(!job_id.is_empty(), "Connect returned empty job_id");

    // Drive the backend to Connected. The service event loop sees
    // WifiStateChanged, takes the pending connect job, and emits
    // ConnectComplete(success=true, reason="").
    event_tx
        .send(NexusEvent::WifiStateChanged {
            ifindex: WLAN_IFINDEX,
            state: WifiState::Connected {
                bssid: MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
                ssid: Ssid::new(b"corp".to_vec()).unwrap(),
                frequency: 5180,
                signal_dbm: -50,
                security: SecurityMode::Wpa2Psk,
            },
        })
        .unwrap();

    let (jid, success, reason) = next_complete(&mut stream, Duration::from_millis(800))
        .await
        .expect("ConnectComplete");
    assert_eq!(jid, job_id);
    assert!(success);
    assert_eq!(reason, "");
    handle.stop().await;
}

async fn assert_connect_failure_reason(
    bus_name: &str,
    reason_variant: DisconnectReason,
    expected_wire_reason: &str,
) {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut stream = subscribe_wifi_signal(&client, "ConnectComplete").await;
    let job_id = issue_connect(&bus, bus_name).await;
    event_tx
        .send(NexusEvent::WifiStateChanged {
            ifindex: WLAN_IFINDEX,
            state: WifiState::Disconnected { reason: reason_variant },
        })
        .unwrap();
    let (jid, success, wire) = next_complete(&mut stream, Duration::from_millis(800))
        .await
        .expect("ConnectComplete");
    assert_eq!(jid, job_id);
    assert!(!success);
    assert_eq!(wire, expected_wire_reason);
    handle.stop().await;
}

#[tokio::test]
async fn wifi_connect_complete_credentials_invalid() {
    assert_connect_failure_reason(
        "fi.nexus1.test_cc_creds",
        DisconnectReason::CredentialsInvalid,
        "credentials_invalid",
    )
    .await;
}

#[tokio::test]
async fn wifi_connect_complete_handshake_timeout() {
    assert_connect_failure_reason(
        "fi.nexus1.test_cc_hs",
        DisconnectReason::HandshakeTimeout,
        "handshake_timeout",
    )
    .await;
}

#[tokio::test]
async fn wifi_connect_complete_rf_killed() {
    assert_connect_failure_reason(
        "fi.nexus1.test_cc_rfk",
        DisconnectReason::RfKilled,
        "rf_killed",
    )
    .await;
}

#[tokio::test]
async fn wifi_connect_complete_supplicant_unavailable() {
    assert_connect_failure_reason(
        "fi.nexus1.test_cc_sup",
        DisconnectReason::SupplicantUnavailable,
        "supplicant_unavailable",
    )
    .await;
}

#[tokio::test]
async fn wifi_connect_complete_ap_initiated_maps_to_other() {
    // Disconnect reasons not in the named set fall under "other"
    // — ApInitiated is the canonical example.
    assert_connect_failure_reason(
        "fi.nexus1.test_cc_other",
        DisconnectReason::ApInitiated,
        "other",
    )
    .await;
}

#[tokio::test]
async fn wifi_disconnect_returns_job_id_and_emits_complete_on_success() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let bus_name = "fi.nexus1.test_dc_ok";
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut stream = subscribe_wifi_signal(&client, "DisconnectComplete").await;

    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let reply = client
        .call_method(
            Some(bus_name),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Disconnect",
            &(empty,),
        )
        .await
        .expect("Disconnect");
    let job_id: String = reply.body().deserialize().expect("job_id");
    assert!(!job_id.is_empty());

    let (jid, success, reason) = next_complete(&mut stream, Duration::from_millis(800))
        .await
        .expect("DisconnectComplete");
    assert_eq!(jid, job_id);
    assert!(success);
    assert_eq!(reason, "");
    handle.stop().await;
}

#[tokio::test]
async fn wifi_disconnect_emits_complete_other_when_backend_errors() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    // Inject an error so the backend's wifi_disconnect returns Err.
    ops.inject_error(DbusError::Unsupported("supplicant gone".into()));
    let bus_name = "fi.nexus1.test_dc_fail";
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut stream = subscribe_wifi_signal(&client, "DisconnectComplete").await;
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let reply = client
        .call_method(
            Some(bus_name),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Disconnect",
            &(empty,),
        )
        .await
        .expect("Disconnect");
    let job_id: String = reply.body().deserialize().expect("job_id");

    let (jid, success, reason) = next_complete(&mut stream, Duration::from_millis(800))
        .await
        .expect("DisconnectComplete");
    assert_eq!(jid, job_id);
    assert!(!success);
    assert_eq!(reason, "other");
    handle.stop().await;
}

#[tokio::test]
async fn wifi_disconnect_cancels_in_flight_connect() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let bus_name = "fi.nexus1.test_cancel";
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut connect_stream = subscribe_wifi_signal(&client, "ConnectComplete").await;
    let mut disconnect_stream = subscribe_wifi_signal(&client, "DisconnectComplete").await;

    // Issue Connect, then Disconnect before any state transition
    // resolves the connect.
    let connect_job = issue_connect(&bus, bus_name).await;
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let reply = client
        .call_method(
            Some(bus_name),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Disconnect",
            &(empty,),
        )
        .await
        .expect("Disconnect");
    let disconnect_job: String = reply.body().deserialize().expect("job_id");

    let (cjid, csuccess, creason) =
        next_complete(&mut connect_stream, Duration::from_millis(800))
            .await
            .expect("ConnectComplete");
    assert_eq!(cjid, connect_job);
    assert!(!csuccess);
    assert_eq!(creason, "cancelled");

    let (djid, dsuccess, _) =
        next_complete(&mut disconnect_stream, Duration::from_millis(800))
            .await
            .expect("DisconnectComplete");
    assert_eq!(djid, disconnect_job);
    assert!(dsuccess);
    handle.stop().await;
}

// ---------------------------------------------------------------------------
// DD-006 §6.3 / §9 — Wifi.Scan completion signal.
// ---------------------------------------------------------------------------

fn make_bss(bssid: [u8; 6], ssid: &[u8], rssi: i32) -> BssInfo {
    BssInfo {
        bssid: MacAddr(bssid),
        ssid: Ssid::new(ssid.to_vec()).unwrap(),
        frequency: 2412,
        signal_dbm: rssi,
        capabilities: BssCapabilities::default(),
        security: vec![SecurityMode::Wpa2Psk],
        age_ms: 0,
    }
}

/// Wait for the next ScanComplete signal and return its payload.
async fn next_scan_complete(
    stream: &mut zbus::MessageStream,
    timeout: Duration,
) -> Option<(String, bool, u32, String)> {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        let remaining =
            deadline.saturating_duration_since(std::time::Instant::now()) + Duration::from_millis(1);
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Ok(p) = msg.body().deserialize::<(String, bool, u32, String)>() {
                    return Some(p);
                }
            }
            _ => return None,
        }
    }
    None
}

async fn issue_scan(bus: &Bus, bus_name: &str) -> String {
    let client = bus.connection().await;
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let reply = client
        .call_method(
            Some(bus_name),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Scan",
            &(empty,),
        )
        .await
        .expect("Scan");
    reply.body().deserialize::<String>().expect("job_id (s)")
}

#[tokio::test]
async fn wifi_scan_returns_job_id_and_emits_complete_on_success() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let bus_name = "fi.nexus1.test_sc_ok";
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut stream = subscribe_wifi_signal(&client, "ScanComplete").await;
    let job_id = issue_scan(&bus, bus_name).await;
    assert!(!job_id.is_empty(), "Scan returned empty job_id");

    // Drive the backend to a successful scan with one BSS.
    event_tx
        .send(NexusEvent::WifiScanComplete {
            ifindex: WLAN_IFINDEX,
            success: true,
            results: vec![make_bss([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF], b"corp", -55)],
        })
        .unwrap();

    let (jid, success, count, reason) = next_scan_complete(&mut stream, Duration::from_millis(800))
        .await
        .expect("ScanComplete");
    assert_eq!(jid, job_id);
    assert!(success);
    assert_eq!(count, 1);
    assert_eq!(reason, "");
    handle.stop().await;
}

#[tokio::test]
async fn wifi_scan_complete_reports_aborted_when_supplicant_aborts() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let bus_name = "fi.nexus1.test_sc_abort";
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut stream = subscribe_wifi_signal(&client, "ScanComplete").await;
    let job_id = issue_scan(&bus, bus_name).await;

    event_tx
        .send(NexusEvent::WifiScanComplete {
            ifindex: WLAN_IFINDEX,
            success: false,
            results: vec![],
        })
        .unwrap();

    let (jid, success, count, reason) = next_scan_complete(&mut stream, Duration::from_millis(800))
        .await
        .expect("ScanComplete");
    assert_eq!(jid, job_id);
    assert!(!success);
    assert_eq!(count, 0);
    assert_eq!(reason, "aborted");
    handle.stop().await;
}

async fn assert_scan_failure_reason(
    bus_name: &str,
    inject: DbusError,
    expected_wire_reason: &str,
) {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    ops.inject_error(inject);
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let mut stream = subscribe_wifi_signal(&client, "ScanComplete").await;
    let job_id = issue_scan(&bus, bus_name).await;

    let (jid, success, count, reason) = next_scan_complete(&mut stream, Duration::from_millis(800))
        .await
        .expect("ScanComplete");
    assert_eq!(jid, job_id);
    assert!(!success);
    assert_eq!(count, 0);
    assert_eq!(reason, expected_wire_reason);
    handle.stop().await;
}

#[tokio::test]
async fn wifi_scan_complete_busy_when_backend_returns_resource_busy() {
    // ResourceBusy maps to "busy" — the canonical stacked-scan
    // rejection. (DD-006 §6.3: ResourceBusy is no longer a sync
    // error on Scan; it surfaces as ScanComplete reason='busy'.)
    assert_scan_failure_reason(
        "fi.nexus1.test_sc_busy",
        DbusError::ResourceBusy("scan in flight".into()),
        "busy",
    )
    .await;
}

#[tokio::test]
async fn wifi_scan_complete_rf_killed_when_backend_returns_io() {
    // nexus-daemon maps WifiError::Rfkill to DbusError::Io. A test
    // with a real /dev/rfkill writer would exercise the same path.
    assert_scan_failure_reason(
        "fi.nexus1.test_sc_rfk",
        DbusError::Io(std::io::Error::other("rfkill blocked")),
        "rf_killed",
    )
    .await;
}

#[tokio::test]
async fn wifi_scan_complete_supplicant_unavailable_on_not_found() {
    // NotFound (e.g., NotAttached / UnknownInterface from the
    // backend) maps to "supplicant_unavailable".
    assert_scan_failure_reason(
        "fi.nexus1.test_sc_sup",
        DbusError::NotFound("interface not attached".into()),
        "supplicant_unavailable",
    )
    .await;
}

#[tokio::test]
async fn wifi_state_changed_signal_emitted_on_each_transition() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let bus_name = "fi.nexus1.test_state";
    let (handle, event_tx) = spawn(&bus, bus_name, always_allow(), ops.clone()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("fi.nexus.Wifi")
        .unwrap()
        .member("StateChanged")
        .unwrap()
        .path("/fi/nexus1/interface/wlan0")
        .unwrap()
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &client, None)
        .await
        .expect("subscribe Wifi.StateChanged");

    let bssid = MacAddr([0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    let ssid = Ssid::new(b"corp".to_vec()).unwrap();
    for s in [
        WifiState::Scanning,
        WifiState::Connecting {
            bssid,
            ssid: ssid.clone(),
        },
        WifiState::Connected {
            bssid,
            ssid: ssid.clone(),
            frequency: 2412,
            signal_dbm: -55,
            security: SecurityMode::Wpa2Psk,
        },
    ] {
        event_tx
            .send(NexusEvent::WifiStateChanged {
                ifindex: WLAN_IFINDEX,
                state: s,
            })
            .unwrap();
    }

    let mut got: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(800);
    while got.len() < 3 && std::time::Instant::now() < deadline {
        let remaining =
            deadline.saturating_duration_since(std::time::Instant::now()) + Duration::from_millis(1);
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                if let Ok((label, _details)) =
                    msg.body().deserialize::<(String, HashMap<String, OwnedValue>)>()
                {
                    got.push(label);
                }
            }
            _ => break,
        }
    }
    assert!(
        got.iter().any(|s| s == "scanning"),
        "expected `scanning` state, got {got:?}"
    );
    assert!(
        got.iter().any(|s| s == "connecting"),
        "expected `connecting` state, got {got:?}"
    );
    assert!(
        got.iter().any(|s| s == "connected"),
        "expected `connected` state, got {got:?}"
    );
    handle.stop().await;
}

// ---------------------------------------------------------------------------
// DD-006 §6.4 — fi.nexus.Bluetooth.Powered (writable property).
//
// Mirrors the Wi-Fi `Powered` setter coverage: PolicyKit-deny path
// surfaces AuthFailed; allow path reaches BackendOps::bt_set_powered;
// disabling the bluetooth feature returns FeatureDisabled.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bluetooth_set_powered_denied_returns_auth_failed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let ops = RecordingOps::new();
    let (handle, event_tx) = spawn(
        &bus,
        "fi.nexus1.test_btpwr_deny",
        always_deny(),
        Arc::clone(&ops) as Arc<dyn BackendOps>,
    )
    .await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(hci_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let err = client
        .call_method(
            Some("fi.nexus1.test_btpwr_deny"),
            "/fi/nexus1/interface/hci0",
            Some("org.freedesktop.DBus.Properties"),
            "Set",
            &(
                "fi.nexus.Bluetooth",
                "Powered",
                Value::new(true),
            ),
        )
        .await
        .expect_err("Set should be rejected");
    let name = err.to_string();
    assert!(
        name.contains("AuthFailed") || name.contains("Auth failed"),
        "expected AuthFailed, got {name}"
    );
    // Backend must NOT see the call when polkit denied.
    assert!(
        ops.calls()
            .iter()
            .all(|c| !matches!(c, RecordedCall::BtSetPowered { .. })),
        "ops saw bt_set_powered despite deny: {:?}",
        ops.calls()
    );
    handle.stop().await;
}

#[tokio::test]
async fn bluetooth_set_powered_allowed_calls_backend() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let policy = Arc::new(PolicyMapChecker::new(nexus_dbus::AuthDecision::Denied));
    policy.allow(actions::SET_POWER);
    let ops = RecordingOps::new();
    let (handle, event_tx) = spawn(
        &bus,
        "fi.nexus1.test_btpwr_ok",
        policy as Arc<dyn nexus_dbus::AuthChecker>,
        Arc::clone(&ops) as Arc<dyn BackendOps>,
    )
    .await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(hci_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    client
        .call_method(
            Some("fi.nexus1.test_btpwr_ok"),
            "/fi/nexus1/interface/hci0",
            Some("org.freedesktop.DBus.Properties"),
            "Set",
            &(
                "fi.nexus.Bluetooth",
                "Powered",
                Value::new(true),
            ),
        )
        .await
        .expect("Set Powered=true should succeed");

    let calls = ops.calls();
    let hit = calls.iter().any(|c| matches!(
        c,
        RecordedCall::BtSetPowered { ifname, on: true } if ifname == "/org/bluez/hci0"
    ));
    assert!(
        hit,
        "expected BtSetPowered{{/org/bluez/hci0,true}}, got {calls:?}"
    );
    handle.stop().await;
}

#[tokio::test]
async fn bluetooth_set_powered_returns_feature_disabled_when_off() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, st) = store().await;
    let cfg = DbusConfig {
        bus_name: "fi.nexus1.test_btpwr_off".to_owned(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth: always_allow(),
        ops: NoopOps::arc(),
        rate_limits: nexus_dbus::RateLimits::default(),
        // Bluetooth feature off — Powered set must short-circuit.
        enabled_features: nexus_dbus::EnabledFeatures {
            ethernet: true,
            wifi: true,
            bluetooth: false,
            gnss: true,
        },
        ethernet_auth_backend: "none".to_owned(),
        wifi_supplicant: "wpa_supplicant".to_owned(),
        wifi_roaming_mode: "supplicant".to_owned(),
    };
    let handle = spawn_dbus_service(event_tx.subscribe(), st, cfg)
        .await
        .unwrap();
    event_tx
        .send(NexusEvent::InterfaceDiscovered(hci_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = bus.connection().await;
    let err = client
        .call_method(
            Some("fi.nexus1.test_btpwr_off"),
            "/fi/nexus1/interface/hci0",
            Some("org.freedesktop.DBus.Properties"),
            "Set",
            &(
                "fi.nexus.Bluetooth",
                "Powered",
                Value::new(true),
            ),
        )
        .await
        .expect_err("Set should reject when bluetooth feature is off");
    let name = err.to_string();
    assert!(
        name.contains("FeatureDisabled") || name.contains("feature_disabled"),
        "expected FeatureDisabled, got {name}"
    );
    handle.stop().await;
}
