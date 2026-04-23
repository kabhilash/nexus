//! End-to-end coverage of phase 7-10 features:
//! - Scan-result objects appear / disappear with the BSS cache.
//! - PropertiesChanged emission for hot properties is coalesced
//!   to ≤ 2 Hz per property.
//! - Per-sender rate limiting rejects bursts beyond the configured
//!   `scan_per_min` limit with `RateLimited`.
//! - `RotateMasterKey`, `FreezeForBackup`, `ReleaseBackupLease`
//!   admin methods enforce auth + serve their data contracts.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use nexus_core::{
    BssCapabilities, BssInfo, InterfaceInfo, InterfaceKind, MacAddr, NexusEvent, OperState,
    SecurityMode, Ssid,
};
use nexus_dbus::{DbusConfig, NoopOps, RateLimits, always_allow, always_deny, spawn_dbus_service};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::sync::broadcast;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

// ---------------------------------------------------------------------------
// Bus harness
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

async fn spawn(
    bus: &Bus,
    bus_name: &str,
    rate_limits: RateLimits,
) -> (nexus_dbus::DbusServiceHandle, broadcast::Sender<NexusEvent>) {
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(1024);
    let (_tmp, st) = store().await;
    let cfg = DbusConfig {
        bus_name: bus_name.to_owned(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth: always_allow(),
        ops: NoopOps::arc(),
        rate_limits,
        enabled_features: nexus_dbus::EnabledFeatures::default(),
    };
    let h = spawn_dbus_service(event_tx.clone(), st, cfg).await.unwrap();
    (h, event_tx)
}

// ---------------------------------------------------------------------------
// Phase 7 — scan results
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scan_result_objects_appear_and_disappear() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, event_tx) = spawn(&bus, "fi.nexus1.test_scanres", RateLimits::default()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    let bss = make_bss([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF], b"corp", -55);
    event_tx
        .send(NexusEvent::WifiScanComplete {
            ifindex: 3,
            results: vec![bss.clone()],
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let client = bus.connection().await;
    // GetManagedObjects must include the scan-result path.
    let reply = client
        .call_method(
            Some("fi.nexus1.test_scanres"),
            "/fi/nexus1",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await
        .unwrap();
    let objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
        reply.body().deserialize().unwrap();
    let path =
        OwnedObjectPath::try_from("/fi/nexus1/interface/wlan0/scan_result/aabbccddeeff").unwrap();
    assert!(
        objects.contains_key(&path),
        "scan-result path missing from object tree"
    );

    // Property read.
    let props = client
        .call_method(
            Some("fi.nexus1.test_scanres"),
            "/fi/nexus1/interface/wlan0/scan_result/aabbccddeeff",
            Some("org.freedesktop.DBus.Properties"),
            "GetAll",
            &("fi.nexus.ScanResult",),
        )
        .await
        .unwrap();
    let dict: HashMap<String, OwnedValue> = props.body().deserialize().unwrap();
    assert!(dict.contains_key("Bssid"));
    assert!(dict.contains_key("Ssid"));
    assert!(dict.contains_key("SignalDbm"));

    // A subsequent scan with different BSSes drops the original.
    event_tx
        .send(NexusEvent::WifiScanComplete {
            ifindex: 3,
            results: vec![make_bss([0x01, 0x02, 0x03, 0x04, 0x05, 0x06], b"corp", -65)],
        })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(150)).await;

    let reply = client
        .call_method(
            Some("fi.nexus1.test_scanres"),
            "/fi/nexus1",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await
        .unwrap();
    let objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> =
        reply.body().deserialize().unwrap();
    let stale =
        OwnedObjectPath::try_from("/fi/nexus1/interface/wlan0/scan_result/aabbccddeeff").unwrap();
    let fresh =
        OwnedObjectPath::try_from("/fi/nexus1/interface/wlan0/scan_result/010203040506").unwrap();
    assert!(!objects.contains_key(&stale), "evicted BSS should be gone");
    assert!(objects.contains_key(&fresh), "new BSS should be registered");

    handle.stop().await;
}

// ---------------------------------------------------------------------------
// Phase 8 — coalescing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn signal_floods_are_coalesced_into_few_properties_changed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, event_tx) = spawn(&bus, "fi.nexus1.test_coal", RateLimits::default()).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    let client = bus.connection().await;

    // Subscribe to PropertiesChanged signals on the wlan0 path
    // BEFORE we start flooding so we don't miss any.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface("org.freedesktop.DBus.Properties")
        .unwrap()
        .member("PropertiesChanged")
        .unwrap()
        .path("/fi/nexus1/interface/wlan0")
        .unwrap()
        .build();
    let mut stream = zbus::MessageStream::for_match_rule(rule, &client, None)
        .await
        .expect("subscribe");

    // Flood 50 signal-poll events in 200 ms — well under one
    // coalescing window. Without coalescing we'd see 50
    // PropertiesChanged signals; with the 500 ms window we expect
    // at most a few.
    let start = Instant::now();
    for i in 0i32..50 {
        event_tx
            .send(NexusEvent::WifiSignalPoll {
                ifindex: 3,
                rssi: -60 - (i % 10),
                frequency: 2412,
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(4)).await;
    }
    // Wait one full coalescing window after the last event so
    // the final batch flushes.
    tokio::time::sleep(Duration::from_millis(700)).await;

    let mut signal_count = 0u32;
    let deadline = Instant::now() + Duration::from_millis(50);
    while Instant::now() < deadline {
        let remaining =
            deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1);
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(_msg))) => signal_count += 1,
            _ => break,
        }
    }
    assert!(
        signal_count <= 4,
        "expected ≤ 4 coalesced PropertiesChanged signals over ~750 ms, got {signal_count} (elapsed {:?})",
        start.elapsed()
    );
    assert!(
        signal_count >= 1,
        "expected at least one PropertiesChanged from coalescing, got 0"
    );
    handle.stop().await;
}

// ---------------------------------------------------------------------------
// Phase 9 — admin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rotate_master_key_denied_returns_auth_failed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Build with always_deny so admin is rejected.
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(32);
    let (_tmp, st) = store().await;
    let cfg = DbusConfig {
        bus_name: "fi.nexus1.test_rotdeny".into(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth: always_deny(),
        ops: NoopOps::arc(),
        rate_limits: RateLimits::default(),
        enabled_features: nexus_dbus::EnabledFeatures::default(),
    };
    let handle = spawn_dbus_service(event_tx, st, cfg).await.unwrap();
    let client = bus.connection().await;

    let err = client
        .call_method(
            Some("fi.nexus1.test_rotdeny"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "RotateMasterKey",
            &(),
        )
        .await
        .expect_err("denied");
    let s = format!("{err:?}");
    assert!(s.contains("AuthFailed"), "got {s}");
    handle.stop().await;
}

#[tokio::test]
async fn rotate_master_key_accepted_returns_job_id() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_rotok",
        // Bump admin per-min so a single test call doesn't get
        // boxed in by the default 1/min limit.
        RateLimits {
            admin_per_min: 5,
            ..RateLimits::default()
        },
    )
    .await;
    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some("fi.nexus1.test_rotok"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "RotateMasterKey",
            &(),
        )
        .await
        .expect("rotate");
    let job_id: String = reply.body().deserialize().unwrap();
    assert!(!job_id.is_empty());
    handle.stop().await;
}

#[tokio::test]
async fn freeze_and_release_lease_round_trip() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (handle, _tx) = spawn(
        &bus,
        "fi.nexus1.test_lease",
        RateLimits {
            admin_per_min: 5,
            ..RateLimits::default()
        },
    )
    .await;
    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some("fi.nexus1.test_lease"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "FreezeForBackup",
            &(),
        )
        .await
        .unwrap();
    let lease: String = reply.body().deserialize().unwrap();
    assert!(
        lease.contains('-') && lease.len() == 36,
        "uuid form: {lease}"
    );

    // Holding a lease blocks a second Freeze.
    let err = client
        .call_method(
            Some("fi.nexus1.test_lease"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "FreezeForBackup",
            &(),
        )
        .await
        .expect_err("dup lease");
    let s = format!("{err:?}");
    assert!(s.contains("ResourceBusy"), "got {s}");

    // Releasing with a wrong token returns NotFound.
    let err = client
        .call_method(
            Some("fi.nexus1.test_lease"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "ReleaseBackupLease",
            &("not-the-real-token",),
        )
        .await
        .expect_err("wrong token");
    let s = format!("{err:?}");
    assert!(s.contains("NotFound"), "got {s}");

    client
        .call_method(
            Some("fi.nexus1.test_lease"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "ReleaseBackupLease",
            &(lease.as_str(),),
        )
        .await
        .expect("release with correct token");

    handle.stop().await;
}

// ---------------------------------------------------------------------------
// Phase 10 — rate limiting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn scan_burst_is_throttled_with_rate_limited() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let limits = RateLimits {
        scan_per_min: 5,
        ..RateLimits::default()
    };
    let (handle, event_tx) = spawn(&bus, "fi.nexus1.test_rl", limits).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    let client = bus.connection().await;
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    // The harness wires `NoopOps`, so a rate-limit-passing call
    // still fails downstream with `Unsupported`. We count
    // anything that is *not* `RateLimited` as a "passed the
    // limiter" outcome — that's exactly what the rate limiter is
    // supposed to gate.
    let mut passed_limiter = 0u32;
    let mut throttled = 0u32;
    let mut retry_hint_seen = false;
    for _ in 0..20 {
        let result = client
            .call_method(
                Some("fi.nexus1.test_rl"),
                "/fi/nexus1/interface/wlan0",
                Some("fi.nexus.Wifi"),
                "Scan",
                &(empty.clone(),),
            )
            .await;
        match result {
            Ok(_) => passed_limiter += 1,
            Err(e) => {
                let s = format!("{e:?}");
                if s.contains("RateLimited") {
                    throttled += 1;
                    if s.contains("retry after") {
                        retry_hint_seen = true;
                    }
                } else if s.contains("Unsupported") {
                    passed_limiter += 1;
                } else {
                    panic!("unexpected error: {s}");
                }
            }
        }
    }
    assert_eq!(
        passed_limiter, 5,
        "exactly the limit's worth of scans should pass the limiter"
    );
    assert_eq!(throttled, 15, "remaining bursts should be throttled");
    assert!(
        retry_hint_seen,
        "at least one RateLimited error should carry a retry_after hint"
    );
    handle.stop().await;
}

// Small touch test for the OpClass debug strings — keeps the
// `OpClass` re-export from `lib.rs` exercised.
#[test]
fn op_class_strings() {
    assert_eq!(nexus_dbus::OpClass::Scan.as_str(), "scan");
    assert_eq!(nexus_dbus::OpClass::Admin.as_str(), "admin");
}

// ---------------------------------------------------------------------------
// FeatureDisabled (DD-006 §11.1)
// ---------------------------------------------------------------------------

async fn spawn_with_features(
    bus: &Bus,
    bus_name: &str,
    enabled: nexus_dbus::EnabledFeatures,
) -> (nexus_dbus::DbusServiceHandle, broadcast::Sender<NexusEvent>) {
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(1024);
    let (_tmp, st) = store().await;
    let cfg = DbusConfig {
        bus_name: bus_name.to_owned(),
        address: Some(bus.addr.clone()),
        use_session_bus: false,
        version: "0.1.0-test".into(),
        auth: always_allow(),
        ops: NoopOps::arc(),
        rate_limits: RateLimits::default(),
        enabled_features: enabled,
    };
    let h = spawn_dbus_service(event_tx.clone(), st, cfg).await.unwrap();
    (h, event_tx)
}

#[tokio::test]
async fn scan_on_disabled_wifi_returns_feature_disabled() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let disabled_wifi = nexus_dbus::EnabledFeatures {
        wifi: false,
        ..nexus_dbus::EnabledFeatures::default()
    };
    let (handle, event_tx) = spawn_with_features(&bus, "fi.nexus1.test_fd", disabled_wifi).await;
    // Register a Wi-Fi interface object so the method dispatcher has
    // something to route to. FeatureDisabled must fire *before* the
    // rate limiter or auth check — i.e. even on a fresh, otherwise
    // valid call.
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    let client = bus.connection().await;
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let err = client
        .call_method(
            Some("fi.nexus1.test_fd"),
            "/fi/nexus1/interface/wlan0",
            Some("fi.nexus.Wifi"),
            "Scan",
            &(empty,),
        )
        .await
        .expect_err("scan on disabled wifi");
    let s = format!("{err:?}");
    assert!(
        s.contains("FeatureDisabled") && s.contains("wifi"),
        "expected FeatureDisabled for wifi, got {s}"
    );
    handle.stop().await;
}

#[tokio::test]
async fn property_read_on_disabled_wifi_still_works() {
    // Property reads don't gate on FeatureDisabled — DD-006 §11.1
    // explicitly says "property reads of non-sensitive summary
    // state remain permitted" so clients can discover what is and
    // isn't available.
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let disabled_wifi = nexus_dbus::EnabledFeatures {
        wifi: false,
        ..nexus_dbus::EnabledFeatures::default()
    };
    let (handle, event_tx) =
        spawn_with_features(&bus, "fi.nexus1.test_fdprop", disabled_wifi).await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(wlan_info()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    let client = bus.connection().await;
    let reply = client
        .call_method(
            Some("fi.nexus1.test_fdprop"),
            "/fi/nexus1/interface/wlan0",
            Some("org.freedesktop.DBus.Properties"),
            "Get",
            &("fi.nexus.Wifi", "Powered"),
        )
        .await
        .expect("Properties.Get Powered");
    let v: OwnedValue = reply.body().deserialize().unwrap();
    let powered: bool = bool::try_from(&v).unwrap();
    assert!(!powered, "disabled wifi reports Powered = false");
    handle.stop().await;
}

#[tokio::test]
async fn add_ethernet_profile_on_disabled_ethernet_returns_feature_disabled() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let disabled_eth = nexus_dbus::EnabledFeatures {
        ethernet: false,
        ..nexus_dbus::EnabledFeatures::default()
    };
    let (handle, _tx) = spawn_with_features(&bus, "fi.nexus1.test_fdeth", disabled_eth).await;
    let client = bus.connection().await;
    // An empty settings dict is sufficient — FeatureDisabled fires
    // before the settings parser runs.
    let empty: HashMap<String, OwnedValue> = HashMap::new();
    let err = client
        .call_method(
            Some("fi.nexus1.test_fdeth"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "AddEthernetProfile",
            &(empty,),
        )
        .await
        .expect_err("add eth profile on disabled feature");
    let s = format!("{err:?}");
    assert!(
        s.contains("FeatureDisabled") && s.contains("ethernet"),
        "expected FeatureDisabled for ethernet, got {s}"
    );
    handle.stop().await;
}

// ---------------------------------------------------------------------------
// Error-name mapping (DD-006 §11.1)
// ---------------------------------------------------------------------------

#[test]
fn error_name_mapping_matches_dd006() {
    // Each internal variant must render as its `fi.nexus.Error.*`
    // well-known name in the outgoing fdo::Error. The prefix is
    // part of the message; clients that programmatically match
    // against names (busctl / dbus-monitor output) expect these
    // exact strings.
    let cases: &[(nexus_dbus::DbusError, &str)] = &[
        (
            nexus_dbus::DbusError::RateLimited {
                op: "scan",
                retry_after_ms: 42,
            },
            "fi.nexus.Error.RateLimited",
        ),
        (
            nexus_dbus::DbusError::FeatureDisabled("wifi".into()),
            "fi.nexus.Error.FeatureDisabled",
        ),
        (
            nexus_dbus::DbusError::ResourceBusy("scan in progress".into()),
            "fi.nexus.Error.ResourceBusy",
        ),
    ];
    for (err, expected_name) in cases {
        let cloned = match err {
            nexus_dbus::DbusError::RateLimited { op, retry_after_ms } => {
                nexus_dbus::DbusError::RateLimited {
                    op,
                    retry_after_ms: *retry_after_ms,
                }
            }
            nexus_dbus::DbusError::FeatureDisabled(m) => {
                nexus_dbus::DbusError::FeatureDisabled(m.clone())
            }
            nexus_dbus::DbusError::ResourceBusy(m) => {
                nexus_dbus::DbusError::ResourceBusy(m.clone())
            }
            _ => unreachable!(),
        };
        let fdo: zbus::fdo::Error = cloned.into();
        let rendered = format!("{fdo:?}");
        assert!(
            rendered.contains(expected_name),
            "expected {expected_name} in {rendered}"
        );
    }
}

// Touch a couple of types from `properties.rs` so the `pub use`
// in lib.rs is verified at compile time.
#[test]
fn coalesce_window_is_500ms() {
    assert_eq!(
        nexus_dbus::COALESCE_WINDOW,
        std::time::Duration::from_millis(500)
    );
}

// Silence "unused import" for items only used inside cfg blocks.
#[allow(dead_code)]
fn _touch(_p: nexus_dbus::PropertyBatcher) {}
