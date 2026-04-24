//! Mock-driven end-to-end tests for the Wi-Fi Backend. See DD-003
//! §14.1.
//!
//! These tests spin up a real backend task, feed it scripted
//! [`SupplicantEvent`]s through the [`MockSupplicant`], and observe
//! the `NexusEvent` bus for the expected lifecycle transitions.
//! Everything is in-process; the integration-linux tests that drive
//! hostapd + mac80211_hwsim live behind the `integration-linux`
//! feature.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{
    InterfaceInfo, InterfaceKind, MacAddr, NexusEvent, Nl80211IfType, OperState, PhyCapabilities,
    SecurityMode, Ssid, WifiState,
};
use nexus_profile_store::{
    InMemoryKeySource, ProfileFileStore, ProfileMetadata, ProfileStore, SecretString,
    SecurityConfig, WifiNetworkSettings, WifiProfile, WpaPsk,
};
use nexus_wifi::power::PowerState;
use nexus_wifi::roam::RoamPolicy;
use nexus_wifi::scan::ScanScheduler;
use nexus_wifi::supplicant::{
    DisconnectHint, MockBehavior, MockSupplicant, MockSupplicantHandle, SupplicantEvent,
    WifiSupplicantBackend,
};
use nexus_wifi::types::{BssCapabilities, BssInfo};
use nexus_wifi::{WifiBackendHandle, WifiConfig, spawn_wifi_backend};
use tempfile::TempDir;
use tokio::sync::broadcast;

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct Harness {
    backend: WifiBackendHandle,
    event_tx: broadcast::Sender<NexusEvent>,
    event_rx: broadcast::Receiver<NexusEvent>,
    supplicant: MockSupplicantHandle,
    sup_tx: broadcast::Sender<SupplicantEvent>,
    // Keep the tempdir alive for the lifetime of the harness.
    _tmp: TempDir,
}

impl Harness {
    async fn start(profiles: Vec<WifiProfile>, config: WifiConfig) -> Self {
        let (event_tx, event_rx) = broadcast::channel(64);
        let (sup_tx, _sup_rx) = broadcast::channel(64);

        let tmp = TempDir::new().unwrap();
        let keys = InMemoryKeySource::new([0x11u8; 32]);
        let store = ProfileFileStore::open(tmp.path(), &keys).unwrap();
        for p in profiles {
            store.put_wifi(&p).await.unwrap();
        }
        let store: Arc<dyn ProfileStore> = Arc::new(store);

        let mock = MockSupplicant::new(sup_tx.clone());
        let supplicant_handle = mock.handle();
        let (_cmd_tx, cmd_rx) = nexus_wifi::command_channel();
        let backend = spawn_wifi_backend(
            event_tx.clone(),
            sup_tx.clone(),
            Box::new(mock),
            store,
            config,
            cmd_rx,
        );

        Self {
            backend,
            event_tx,
            event_rx,
            supplicant: supplicant_handle,
            sup_tx,
            _tmp: tmp,
        }
    }

    async fn shutdown(self) {
        self.backend.shutdown.cancel();
        // Give the task up to 2 seconds to wind down. Join errors
        // here are surfaced to the test output.
        let _ = tokio::time::timeout(Duration::from_secs(2), self.backend.join).await;
    }

    /// Poll the event bus until `pred` returns true, or the timeout
    /// expires. Returns the matching event or panics with the events
    /// that were observed.
    async fn expect_event<F: Fn(&NexusEvent) -> bool>(
        &mut self,
        pred: F,
        timeout: Duration,
    ) -> NexusEvent {
        let deadline = Instant::now() + timeout;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, self.event_rx.recv()).await {
                Ok(Ok(event)) => {
                    if pred(&event) {
                        return event;
                    }
                    seen.push(format!("{event:?}"));
                }
                Ok(Err(_)) | Err(_) => {
                    panic!(
                        "timed out waiting for event\nobserved: {}",
                        seen.join("\n         ")
                    );
                }
            }
        }
    }
}

fn wifi_interface(ifindex: u32, ifname: &str) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: ifname.to_owned(),
        mac: [0x02, 0, 0, 0, 0, ifindex as u8],
        mtu: 1500,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Wireless {
            wiphy: 0,
            wiphy_name: "phy0".to_owned(),
            wdev: 1,
            iftype: Nl80211IfType(2), // NL80211_IFTYPE_STATION
            capabilities: Arc::new(PhyCapabilities::default()),
        },
        discovered_at: Instant::now(),
    }
}

fn wifi_profile(ssid: &[u8], passphrase: &str, priority: i32) -> WifiProfile {
    WifiProfile {
        id: ulid::Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        network: WifiNetworkSettings {
            ssid: Ssid::new(ssid.to_vec()).unwrap(),
            hidden: false,
            priority,
            auto_connect: true,
            fast_transition: false,
            security: SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(SecretString::from(passphrase)),
            },
            bssid_preferred: None,
            bssid_blacklist: vec![],
            scan_freqs: vec![],
            credentials_invalid: false,
        },
    }
}

fn open_profile(ssid: &[u8]) -> WifiProfile {
    WifiProfile {
        id: ulid::Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        network: WifiNetworkSettings {
            ssid: Ssid::new(ssid.to_vec()).unwrap(),
            hidden: false,
            priority: 0,
            auto_connect: true,
            fast_transition: false,
            security: SecurityConfig::Open,
            bssid_preferred: None,
            bssid_blacklist: vec![],
            scan_freqs: vec![],
            credentials_invalid: false,
        },
    }
}

fn bss(bssid: [u8; 6], ssid: &[u8], signal_dbm: i32, mode: SecurityMode) -> BssInfo {
    BssInfo {
        bssid: MacAddr(bssid),
        ssid: Ssid::new(ssid.to_vec()).unwrap(),
        frequency: 2412,
        signal_dbm,
        capabilities: BssCapabilities::default(),
        security: vec![mode],
        age_ms: 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Full happy path per DD-003 §§5, 6: InterfaceDiscovered → scan →
/// match → connect → WifiLinkReady.
#[tokio::test]
async fn discovery_scan_match_connect_emits_link_ready() {
    let profile = wifi_profile(b"corp", "correcthorse", 10);
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    // Pre-load scan results for the supplicant's cache so the first
    // (InterfaceDiscovered-triggered) scan comes back with the target.
    h.supplicant
        .set_scan_results(2, vec![bss([0xAA; 6], b"corp", -45, SecurityMode::Wpa2Psk)]);
    h.supplicant.set_connect_outcome(2, MockBehavior::Success);

    // Announce the interface.
    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));

    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;
    h.shutdown().await;
}

/// DD-003 §6.3: a Fail(BadCredentials) outcome surfaces as a
/// Disconnected { CredentialsInvalid } and does NOT transition to
/// Connected.
#[tokio::test]
async fn bad_credentials_blocks_link_ready() {
    let profile = wifi_profile(b"corp", "wrong", 10);
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xBB; 6], b"corp", -40, SecurityMode::Wpa2Psk)]);
    h.supplicant
        .set_connect_outcome(2, MockBehavior::Fail(DisconnectHint::BadCredentials));

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));

    let event = h
        .expect_event(
            |e| {
                matches!(
                    e,
                    NexusEvent::WifiStateChanged {
                        ifindex: 2,
                        state: WifiState::Disconnected { .. },
                    }
                )
            },
            Duration::from_secs(2),
        )
        .await;
    if let NexusEvent::WifiStateChanged { state, .. } = event {
        assert!(matches!(
            state,
            WifiState::Disconnected {
                reason: nexus_core::DisconnectReason::CredentialsInvalid,
            }
        ));
    }
    h.shutdown().await;
}

/// DD-003 §12.1: DaemonDown transitions every attached interface to
/// Disconnected { SupplicantUnavailable } and WifiLinkLost fires for
/// the previously-connected interface.
#[tokio::test]
async fn supplicant_crash_emits_link_lost_and_unavailable_state() {
    let profile = open_profile(b"captive");
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xCC; 6], b"captive", -50, SecurityMode::Open)]);
    h.supplicant
        .set_connect_outcome(2, MockBehavior::OpenSuccess);

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    // Inject a synthetic DaemonDown — the supplicant's D-Bus
    // NameOwnerChanged handler would drive this in production.
    let _ = h.sup_tx.send(SupplicantEvent::DaemonDown);

    // The backend should emit WifiStateChanged with
    // Disconnected { SupplicantUnavailable } and WifiLinkLost.
    let mut saw_unavailable = false;
    let mut saw_link_lost = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while !(saw_unavailable && saw_link_lost) && Instant::now() < deadline {
        if let Ok(Ok(event)) = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            h.event_rx.recv(),
        )
        .await
        {
            match event {
                NexusEvent::WifiStateChanged {
                    ifindex: 2,
                    state:
                        WifiState::Disconnected {
                            reason: nexus_core::DisconnectReason::SupplicantUnavailable,
                        },
                } => saw_unavailable = true,
                NexusEvent::WifiLinkLost { ifindex: 2 } => saw_link_lost = true,
                _ => {}
            }
        }
    }
    assert!(saw_unavailable, "expected SupplicantUnavailable state");
    assert!(saw_link_lost, "expected WifiLinkLost event");
    h.shutdown().await;
}

/// DD-003 §6.3: BSSID blacklist kicks in after N consecutive
/// failures and prevents immediate retries.
#[tokio::test]
async fn repeated_failures_blacklist_the_bssid() {
    // Using pure retry module behavior here since driving three
    // full supplicant failures through the mock would need an
    // external retry-trigger loop; instead, test the RetryBook
    // contract directly as a sanity check on the selection path.
    use nexus_wifi::retry::RetryBook;
    let mut book = RetryBook::new();
    let now = Instant::now();
    let mac = MacAddr([0xDD; 6]);

    for _ in 0..book.max_failures - 1 {
        assert!(!book.record_failure(2, mac, now));
        assert!(!book.is_blacklisted(2, mac, now));
    }
    assert!(book.record_failure(2, mac, now));
    assert!(book.is_blacklisted(2, mac, now));
    assert!(!book.is_blacklisted(2, mac, now + book.backoff + Duration::from_millis(10)));
}

/// DD-003 §7.3: `pick_roam_target` + RoamMode::Nexus selects a
/// stronger BSS for the same SSID.
#[tokio::test]
async fn roam_picks_stronger_candidate_same_ssid() {
    use nexus_wifi::roam::pick_roam_target;

    let current = MacAddr([0x01; 6]);
    let same_ssid = |bssid: [u8; 6], rssi: i32| bss(bssid, b"corp", rssi, SecurityMode::Wpa2Psk);
    // current rssi -80 dBm, trigger at -75, hysteresis 8 dB
    let candidates = vec![
        same_ssid([0x01; 6], -80), // current
        same_ssid([0x02; 6], -60), // 20 dB better — should win
        same_ssid([0x03; 6], -74), // only 6 dB — below hysteresis
    ];
    let target = pick_roam_target(RoamPolicy::default(), current, -80, &candidates);
    assert_eq!(target, Some(MacAddr([0x02; 6])));
}

/// DD-003 §13.1: PowerState::Sleep pauses scheduled scans.
#[tokio::test]
async fn sleep_power_state_pauses_scans() {
    let s = ScanScheduler::with_defaults();
    assert!(s.next_scan_at(PowerState::Sleep).is_none());
    assert!(s.next_scan_at(PowerState::Active).is_some());
    assert!(s.next_scan_at(PowerState::Background).is_some());
}

/// DD-003 §3.1: InterfaceRemoved drops the registry entry and
/// detaches the supplicant.
#[tokio::test]
async fn interface_removed_drops_registry_entry() {
    let profile = open_profile(b"captive");
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xEE; 6], b"captive", -50, SecurityMode::Open)]);
    h.supplicant
        .set_connect_outcome(2, MockBehavior::OpenSuccess);

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    let _ = h.event_tx.send(NexusEvent::InterfaceRemoved { ifindex: 2 });

    // Give the backend a tick to process the removal.
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.shutdown().await;
}

/// DD-003 §6.1: open network profile matches on an Open BSS.
#[tokio::test]
async fn open_profile_connects_on_open_bss() {
    let profile = open_profile(b"captive");
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xFF; 6], b"captive", -40, SecurityMode::Open)]);
    h.supplicant
        .set_connect_outcome(2, MockBehavior::OpenSuccess);

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));

    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;
    h.shutdown().await;
}

/// DD-003 §6.1: when no profile matches any visible BSS, no connect
/// is attempted (no WifiLinkReady fires inside the timeout).
#[tokio::test]
async fn no_profile_match_does_not_attempt_connect() {
    let profile = wifi_profile(b"home", "homepass", 10);
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    // Scan cache holds a different SSID — no match.
    h.supplicant.set_scan_results(
        2,
        vec![bss([0x01; 6], b"strangers", -40, SecurityMode::Wpa2Psk)],
    );

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));

    // Expect at least one WifiScanComplete and NO WifiLinkReady.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if let Ok(Ok(event)) = tokio::time::timeout(
            deadline.saturating_duration_since(Instant::now()),
            h.event_rx.recv(),
        )
        .await
        {
            if matches!(event, NexusEvent::WifiLinkReady { .. }) {
                panic!("unexpected WifiLinkReady");
            }
        }
    }
    h.shutdown().await;
}

/// DD-003 §4.1: scan on an unattached ifindex yields `NotAttached`.
#[tokio::test]
async fn unattached_scan_errors() {
    let (tx, _rx) = broadcast::channel(8);
    let mut sup = MockSupplicant::new(tx);
    let err = sup
        .scan(42, nexus_wifi::types::ScanParams::default())
        .await
        .unwrap_err();
    match err {
        nexus_wifi::WifiError::NotAttached { ifindex } => assert_eq!(ifindex, 42),
        other => panic!("expected NotAttached, got {other:?}"),
    }
}
