//! Auth + dispatch coverage for the DD-006 phase 4-6 mutating
//! methods. Each method has a "denied" path (PolicyKit returns
//! Denied → caller sees `fi.nexus.Error.AuthFailed`) and an
//! "accepted" path (PolicyKit returns Authorized → backend op
//! is invoked).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, OperState};
use nexus_dbus::backend_ops::BackendOps;
use nexus_dbus::{
    DbusConfig, NoopOps, PolicyMapChecker, RecordedCall, RecordingOps, RoamingMode, ScanParams,
    actions, always_allow, always_deny, spawn_dbus_service,
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
