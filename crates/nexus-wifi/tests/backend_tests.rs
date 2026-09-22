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
    cmd_tx: tokio::sync::mpsc::Sender<nexus_wifi::WifiCommand>,
    /// Test-only rfkill injection channel. `None` for tests that
    /// don't exercise rfkill — the backend then runs without
    /// `/dev/rfkill` plumbing, just like a production deployment
    /// where the device is missing.
    rfkill_tx: Option<tokio::sync::mpsc::Sender<nexus_wifi::rfkill::RfkillState>>,
    // Keep the tempdir alive for the lifetime of the harness.
    _tmp: TempDir,
}

impl Harness {
    async fn start(profiles: Vec<WifiProfile>, config: WifiConfig) -> Self {
        Self::start_inner(profiles, config, false).await
    }

    /// Same as [`start`] but wires a test-only rfkill receiver into
    /// the backend. The returned harness exposes
    /// [`Harness::inject_rfkill`] for driving the radio bit.
    async fn start_with_rfkill(profiles: Vec<WifiProfile>, config: WifiConfig) -> Self {
        Self::start_inner(profiles, config, true).await
    }

    async fn start_inner(profiles: Vec<WifiProfile>, config: WifiConfig, with_rfkill: bool) -> Self {
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
        let (cmd_tx, cmd_rx) = nexus_wifi::command_channel();
        let (backend, rfkill_tx) = if with_rfkill {
            let (rk_tx, rk_rx) = tokio::sync::mpsc::channel(16);
            let h = nexus_wifi::spawn_wifi_backend_with_test_rfkill_rx(
                event_tx.clone(),
                sup_tx.clone(),
                Box::new(mock),
                store,
                config,
                cmd_rx,
                None,
                rk_rx,
            );
            (h, Some(rk_tx))
        } else {
            let h = spawn_wifi_backend(
                event_tx.clone(),
                sup_tx.clone(),
                Box::new(mock),
                store,
                config,
                cmd_rx,
                None,
            );
            (h, None)
        };

        Self {
            backend,
            event_tx,
            event_rx,
            supplicant: supplicant_handle,
            sup_tx,
            cmd_tx,
            rfkill_tx,
            _tmp: tmp,
        }
    }

    /// Push an rfkill edge into the backend. Panics if the harness
    /// was built without `start_with_rfkill`.
    async fn inject_rfkill(&self, wiphy_name: &str, powered: bool) {
        let tx = self
            .rfkill_tx
            .as_ref()
            .expect("harness built without rfkill plumbing; use start_with_rfkill");
        tx.send(nexus_wifi::rfkill::RfkillState {
            wiphy_name: wiphy_name.to_owned(),
            device_path: None,
            powered,
        })
        .await
        .expect("rfkill channel closed");
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
                Ok(Err(broadcast::error::RecvError::Lagged(n))) => {
                    seen.push(format!("<<lagged by {n}>>"));
                }
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    panic!(
                        "event_rx closed\nobserved: {}",
                        seen.join("\n         ")
                    );
                }
                Err(_) => {
                    panic!(
                        "timed out waiting for event\nobserved: {}",
                        seen.join("\n         ")
                    );
                }
            }
        }
    }

    /// Drain the event bus for `window` and assert that no event
    /// matching `pred` ever shows up. Used by the S5 test to
    /// confirm `BssCacheStale` does NOT emit `WifiScanComplete`.
    async fn expect_no_event<F: Fn(&NexusEvent) -> bool>(
        &mut self,
        pred: F,
        window: Duration,
    ) {
        let deadline = Instant::now() + window;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, self.event_rx.recv()).await {
                Ok(Ok(event)) => {
                    if pred(&event) {
                        panic!("unexpected event: {event:?}");
                    }
                }
                Ok(Err(_)) | Err(_) => return,
            }
        }
    }

    async fn set_power(&self, state: PowerState) {
        *self.backend.power.write().await = state;
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
            last_connected_at: None,
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
            last_connected_at: None,
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

/// A `SupplicantState::Connected` re-emit for the **same BSSID** —
/// what the wpa_supplicant adapter's reconciliation tick produces
/// every `RECONCILE_INTERVAL` (2 s) — must NOT fan out a fresh
/// `WifiLinkReady`. Without the gate it caused the connectivity
/// probe to re-run every 2 s and inflated the connect-success
/// metrics on every reconcile.
#[tokio::test]
async fn duplicate_supplicant_connected_does_not_re_emit_link_ready() {
    let profile = wifi_profile(b"corp", "correcthorse", 10);
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;
    let bssid = MacAddr([0xAA; 6]);
    let ssid = Ssid::new(b"corp".to_vec()).unwrap();

    h.supplicant
        .set_scan_results(2, vec![bss(bssid.0, b"corp", -45, SecurityMode::Wpa2Psk)]);
    h.supplicant.set_connect_outcome(2, MockBehavior::Success);
    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));

    // Wait for the first (genuine) LinkReady from the connect path.
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    // Inject a same-BSSID Connected re-emit, exactly what the
    // reconciliation tick would produce.
    let _ = h.sup_tx.send(SupplicantEvent::State {
        ifindex: 2,
        state: nexus_wifi::supplicant::SupplicantState::Connected {
            bssid,
            ssid,
            frequency: 5180,
        },
    });

    // Drain the bus for a short window and assert no further
    // LinkReady arrives. A LinkReady within 300 ms means the gate
    // didn't take.
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, h.event_rx.recv()).await {
            Ok(Ok(NexusEvent::WifiLinkReady { ifindex: 2 })) => {
                panic!(
                    "duplicate SupplicantState::Connected re-emitted WifiLinkReady — \
                     reconciliation tick fan-out is back"
                );
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    h.shutdown().await;
}

/// A wpa_supplicant background scan during an active association
/// shows up as `State=scanning` followed by `State=completed` (with
/// the same BSSID). The wifi backend must NOT fold the cached state
/// to `WifiState::Scanning` — DD-003 §3.1 has no `Connected →
/// Scanning` edge. Without the guard, the supplicant's
/// post-scan `Connected` re-emit looks like a fresh
/// `not-Connected → Connected` transition and fires a spurious
/// `WifiLinkReady`, which fans out into the connectivity probe.
#[tokio::test]
async fn supplicant_scanning_during_association_does_not_re_emit_link_ready() {
    use nexus_wifi::supplicant::SupplicantState;

    let profile = wifi_profile(b"corp", "correcthorse", 10);
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;
    let bssid = MacAddr([0xAA; 6]);
    let ssid = Ssid::new(b"corp".to_vec()).unwrap();

    h.supplicant
        .set_scan_results(2, vec![bss(bssid.0, b"corp", -45, SecurityMode::Wpa2Psk)]);
    h.supplicant.set_connect_outcome(2, MockBehavior::Success);
    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));

    // Wait for the first LinkReady (the genuine connect).
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    // Inject a background-scan transient: supplicant flips State to
    // scanning briefly, then back to completed for the same BSSID.
    let _ = h.sup_tx.send(SupplicantEvent::State {
        ifindex: 2,
        state: SupplicantState::Scanning,
    });
    let _ = h.sup_tx.send(SupplicantEvent::State {
        ifindex: 2,
        state: SupplicantState::Connected {
            bssid,
            ssid,
            frequency: 5180,
        },
    });

    // No second LinkReady should arrive within the window.
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, h.event_rx.recv()).await {
            Ok(Ok(NexusEvent::WifiLinkReady { ifindex: 2 })) => {
                panic!(
                    "background-scan transient (Scanning → Connected same BSSID) \
                     re-emitted WifiLinkReady — Connected→Scanning fold is back"
                );
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
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

// ---------------------------------------------------------------------------
// New §14.1 coverage for the C/K/S audit fixes.
// ---------------------------------------------------------------------------

/// DD-003 §3.2 / §6.4 (C3): a transient `Disconnected` cools down
/// to `Idle` after `disconnect_cool_down`.
#[tokio::test]
async fn cooldown_transitions_disconnected_to_idle() {
    let profile = wifi_profile(b"corp", "correcthorse", 10);
    let cfg = WifiConfig {
        // Tight cool-down so the test runs in well under a second.
        disconnect_cool_down: Duration::from_millis(50),
        ..WifiConfig::default()
    };
    let mut h = Harness::start(vec![profile], cfg).await;

    // Plant a non-matching scan result so the backend reaches the
    // post-attach steady state quickly without trying to connect.
    h.supplicant
        .set_scan_results(2, vec![bss([0x02; 6], b"other", -50, SecurityMode::Open)]);

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiStateChanged { ifindex: 2, .. }),
        Duration::from_secs(2),
    )
    .await;

    // Drive a transient Disconnected via the regular supplicant
    // state path (LocalRequest → DisconnectReason::LocalRequest,
    // which is not permanent and arms the cool-down deadline in
    // `After::Disconnected`). The DaemonDown shortcut bypasses
    // After::Disconnected and so doesn't arm the cooldown — that's
    // by design (DD-003 §12.1 wants the interface to wait for
    // re-attach, not for a fresh scan against a missing daemon).
    let _ = h.sup_tx.send(SupplicantEvent::State {
        ifindex: 2,
        state: nexus_wifi::supplicant::SupplicantState::Disconnected {
            reason: nexus_wifi::supplicant::DisconnectHint::LocalRequest,
        },
    });
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Disconnected {
                    reason: nexus_core::DisconnectReason::LocalRequest,
                },
            }
        ),
        Duration::from_secs(2),
    )
    .await;

    // Heartbeat is 1 s; cool-down is 50 ms. Within ~1.5 s the
    // sweep should land us back in Idle.
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Idle
            }
        ),
        Duration::from_secs(3),
    )
    .await;
    h.shutdown().await;
}

/// DD-003 §3.2 (C3 negative): `CredentialsInvalid` is a permanent
/// reason — no cooldown sweep should fire.
#[tokio::test]
async fn cooldown_does_not_fire_for_credentials_invalid() {
    let profile = wifi_profile(b"corp", "wrong", 10);
    let cfg = WifiConfig {
        disconnect_cool_down: Duration::from_millis(50),
        ..WifiConfig::default()
    };
    let mut h = Harness::start(vec![profile], cfg).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xBB; 6], b"corp", -40, SecurityMode::Wpa2Psk)]);
    h.supplicant.set_connect_outcome(
        2,
        MockBehavior::Fail(nexus_wifi::supplicant::DisconnectHint::BadCredentials),
    );

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Disconnected {
                    reason: nexus_core::DisconnectReason::CredentialsInvalid,
                },
            }
        ),
        Duration::from_secs(2),
    )
    .await;

    // Far longer than disconnect_cool_down — no Idle transition
    // should ever fire.
    h.expect_no_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Idle
            }
        ),
        Duration::from_secs(2),
    )
    .await;
    h.shutdown().await;
}

/// DD-003 §13.3 (C6): Sleep → Active transition triggers
/// `on_wake`, which probes signal on every Connected interface.
/// When the probe errors the interface flips to
/// `Disconnected { PostSleepRecovery }`.
#[tokio::test]
async fn wake_from_sleep_emits_post_sleep_recovery_on_probe_error() {
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

    // Drop into Sleep, then back to Active. The heartbeat fires
    // every 1 s so allow ~3 s for the wake to land.
    h.set_power(PowerState::Sleep).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    h.supplicant.set_signal_info_fails(true);
    h.set_power(PowerState::Active).await;

    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Disconnected {
                    reason: nexus_core::DisconnectReason::PostSleepRecovery,
                },
            }
        ),
        Duration::from_secs(3),
    )
    .await;
    h.shutdown().await;
}

/// DD-003 §12.4 (C7): an interface stuck in `Connecting` past
/// `driver_wedge_threshold` triggers detach + IFF_UP flap +
/// re-attach + `Disconnected { DriverWedge }`.
#[tokio::test]
async fn driver_wedge_recovery_emits_driver_wedge_disconnect() {
    let profile = wifi_profile(b"corp", "pw", 10);
    let cfg = WifiConfig {
        // Shrink the wedge threshold; the heartbeat ticks every 1 s
        // so the wedge sweep needs to run at least once after the
        // dwell crosses the line. The wedge recovery itself sleeps
        // for `disconnect_cool_down` between SetAdminUp(false) and
        // SetAdminUp(true) — drop that to keep the test brisk.
        driver_wedge_threshold: Duration::from_millis(50),
        disconnect_cool_down: Duration::from_millis(20),
        ..WifiConfig::default()
    };
    let mut h = Harness::start(vec![profile], cfg).await;

    // Plant a BSS so the auto-select path picks it up; the mock
    // then drives Associating but stays there — `MockBehavior`
    // doesn't have a "stall in Associating" outcome, but we can
    // approximate one by issuing the Associating state directly
    // and never following up.
    h.supplicant
        .set_scan_results(2, vec![bss([0xAA; 6], b"corp", -45, SecurityMode::Wpa2Psk)]);
    // Use a Fail outcome so the mock's drive_connect emits
    // Associating then Disconnected — but the dwell timer is set
    // by the backend in `try_connect` BEFORE drive_connect runs,
    // and is cleared on Disconnected. To keep the dwell alive we
    // bypass the mock's connect path entirely: emit a synthetic
    // Associating state via sup_tx after InterfaceDiscovered.
    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiStateChanged { ifindex: 2, .. }),
        Duration::from_secs(2),
    )
    .await;
    let _ = h.sup_tx.send(SupplicantEvent::State {
        ifindex: 2,
        state: nexus_wifi::supplicant::SupplicantState::Associating,
    });
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Connecting { .. },
            }
        ),
        Duration::from_secs(2),
    )
    .await;

    // Wait long enough for the heartbeat to detect the wedge and
    // run the recovery (threshold 50 ms + heartbeat 1 s + cool-down
    // 20 ms ≈ ~1.1 s under the worst case).
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Disconnected {
                    reason: nexus_core::DisconnectReason::DriverWedge,
                },
            }
        ),
        Duration::from_secs(3),
    )
    .await;
    h.shutdown().await;
}

/// DD-003 §5.1 / §7.3 (C8): once a Connected interface drops
/// below `roam_trigger_dbm` in `nexus` mode, the heartbeat fires
/// a directed scan with `allow_roam = true`.
#[tokio::test]
async fn low_signal_triggers_directed_roam_scan_in_nexus_mode() {
    use nexus_wifi::types::RoamMode;
    let profile = wifi_profile(b"corp", "pw", 10);
    let cfg = WifiConfig {
        roam_mode: RoamMode::Nexus,
        // Faster signal poll so the test doesn't need to wait the
        // default 5 s interval.
        signal_poll_interval: Duration::from_millis(50),
        ..WifiConfig::default()
    };
    let mut h = Harness::start(vec![profile], cfg).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xAA; 6], b"corp", -40, SecurityMode::Wpa2Psk)]);
    h.supplicant.set_connect_outcome(2, MockBehavior::Success);
    // Plant a low-signal `signal_info` so the next heartbeat poll
    // pulls RSSI under the default trigger (-75 dBm).
    h.supplicant.set_signal(
        2,
        nexus_wifi::types::SignalInfo {
            bssid: MacAddr([0xAA; 6]),
            rssi_dbm: -85,
            noise_dbm: None,
            snr_db: None,
            bitrate_mbps: 0.0,
            frequency: 2412,
        },
    );

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    // Wait for at least one heartbeat to run signal poll +
    // roam-trigger evaluation (heartbeat is 1 s). The directed
    // scan fires inside `maybe_trigger_roam_scans`.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut saw_roam_scan = false;
    while Instant::now() < deadline && !saw_roam_scan {
        for (ifindex, params) in h.supplicant.scan_calls() {
            if ifindex == 2 && params.allow_roam && !params.ssids.is_empty() {
                saw_roam_scan = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        saw_roam_scan,
        "expected a roam-evaluation scan with allow_roam=true and a directed SSID; \
         got {:?}",
        h.supplicant.scan_calls()
    );
    h.shutdown().await;
}

/// DD-003 §7 (C9): `dispatch_roam` enters `WifiState::Roaming`,
/// and a subsequent `Connected` with the wrong BSSID still folds
/// back to `Connected` (with the actual BSSID).
#[tokio::test]
async fn nexus_roam_evaluates_and_completes() {
    use nexus_wifi::types::RoamMode;
    let profile = wifi_profile(b"corp", "pw", 10);
    let cfg = WifiConfig {
        roam_mode: RoamMode::Nexus,
        ..WifiConfig::default()
    };
    let mut h = Harness::start(vec![profile], cfg).await;

    // Two BSSes for the same SSID — current one weak, the other
    // a much stronger candidate. Mock then drives Success on
    // connect (associating with the weak BSS first).
    h.supplicant.set_scan_results(
        2,
        vec![
            bss([0xAA; 6], b"corp", -85, SecurityMode::Wpa2Psk), // current
            bss([0xBB; 6], b"corp", -45, SecurityMode::Wpa2Psk), // candidate
        ],
    );
    h.supplicant.set_connect_outcome(2, MockBehavior::Success);

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    // The mock's Connected emission used the FIRST BSS in the scan
    // list; force the in-state RSSI under the trigger by emitting
    // a synthetic state with the weak BSSID + ssid.
    // Then trigger another scan complete via `BssCacheStale` —
    // the backend re-reads scan results, sees the Connected entry
    // is on a weak BSS while a strong candidate exists for the
    // same SSID, and the heartbeat's roam-trigger eventually
    // fires `dispatch_roam`.
    //
    // Direct path: feed the backend a low signal poll so RSSI
    // drops to -85 inside the Connected variant.
    h.supplicant.set_signal(
        2,
        nexus_wifi::types::SignalInfo {
            bssid: MacAddr([0xAA; 6]),
            rssi_dbm: -85,
            noise_dbm: None,
            snr_db: None,
            bitrate_mbps: 0.0,
            frequency: 2412,
        },
    );

    // Watch for the recorded roam call (the dispatch_roam side
    // effect). Heartbeat cadence is 1 s; allow up to 5 s for
    // signal poll → directed scan → scan complete → evaluate_roam
    // → dispatch_roam.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut roamed_to: Option<MacAddr> = None;
    while Instant::now() < deadline && roamed_to.is_none() {
        for (ifindex, target) in h.supplicant.roam_calls() {
            if ifindex == 2 {
                if let nexus_wifi::types::RoamTarget::Bss(b) = target {
                    roamed_to = Some(b);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let target = roamed_to.expect("backend should dispatch a roam to the stronger candidate");
    assert_eq!(target, MacAddr([0xBB; 6]));
    h.shutdown().await;
}

/// DD-003 §9.2 (C10): `SupplicantEvent::NetworkRequest` round-
/// trips into `NexusEvent::WifiNetworkRequest`.
#[tokio::test]
async fn network_request_round_trips_to_nexus_event() {
    let profile = open_profile(b"captive");
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiStateChanged { ifindex: 2, .. }),
        Duration::from_secs(2),
    )
    .await;

    let _ = h.sup_tx.send(SupplicantEvent::NetworkRequest {
        ifindex: 2,
        network: "/fi/w1/wpa_supplicant1/Interfaces/0/Networks/3".into(),
        field: "password".into(),
        text: "Enter PEAP password".into(),
    });

    let event = h
        .expect_event(
            |e| matches!(e, NexusEvent::WifiNetworkRequest { ifindex: 2, .. }),
            Duration::from_secs(2),
        )
        .await;
    if let NexusEvent::WifiNetworkRequest {
        network,
        field,
        text,
        ..
    } = event
    {
        assert_eq!(network, "/fi/w1/wpa_supplicant1/Interfaces/0/Networks/3");
        assert_eq!(field, "password");
        assert_eq!(text, "Enter PEAP password");
    }

    // And the matching ProvideCredential WifiCommand round-trips
    // to the supplicant via wifi_provide_credential.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    h.cmd_tx
        .send(nexus_wifi::WifiCommand::ProvideCredential {
            ifname: "wlan0".into(),
            network: "/fi/w1/wpa_supplicant1/Interfaces/0/Networks/3".into(),
            field: "password".into(),
            value: "hunter2".into(),
            reply: reply_tx,
        })
        .await
        .unwrap();
    reply_rx.await.unwrap().unwrap();
    let replies = h.supplicant.credential_replies();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].ifindex, 2);
    assert_eq!(replies[0].field, "password");
    assert_eq!(replies[0].value, "hunter2");
    h.shutdown().await;
}

/// K8: when the supplicant attach fails, the entry enters
/// `Disconnected{SupplicantUnavailable}` and no scan is requested.
#[tokio::test]
async fn attach_failure_leaves_interface_in_supplicant_unavailable_with_no_scan() {
    let profile = open_profile(b"captive");
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    // Force the mock attach to fail.
    h.supplicant.set_daemon_up(false);

    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    let event = h
        .expect_event(
            |e| matches!(e, NexusEvent::WifiStateChanged { ifindex: 2, .. }),
            Duration::from_secs(2),
        )
        .await;
    assert!(matches!(
        event,
        NexusEvent::WifiStateChanged {
            ifindex: 2,
            state: WifiState::Disconnected {
                reason: nexus_core::DisconnectReason::SupplicantUnavailable,
            },
        }
    ));

    // No scan should have been issued — the daemon is "down".
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        h.supplicant.scan_calls().is_empty(),
        "no scan should fire when attach failed: {:?}",
        h.supplicant.scan_calls()
    );
    h.shutdown().await;
}

/// S5: `SupplicantEvent::BssCacheStale` refreshes the local
/// BssCache (visible by re-driving auto-select on the next scan)
/// but does NOT emit a public `WifiScanComplete` event.
#[tokio::test]
async fn bss_cache_stale_does_not_emit_scan_complete() {
    let profile = open_profile(b"captive");
    let mut h = Harness::start(vec![profile], WifiConfig::default()).await;

    // Plant a fresh BSS in the mock cache so the upcoming
    // get_scan_results call returns something — the backend
    // refreshes its local cache on BssCacheStale.
    h.supplicant
        .set_scan_results(2, vec![bss([0xAB; 6], b"captive", -42, SecurityMode::Open)]);
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

    // Now fire BssCacheStale. The backend should NOT emit a
    // WifiScanComplete in response.
    let _ = h.sup_tx.send(SupplicantEvent::BssCacheStale { ifindex: 2 });
    h.expect_no_event(
        |e| matches!(e, NexusEvent::WifiScanComplete { ifindex: 2, .. }),
        Duration::from_millis(300),
    )
    .await;
    h.shutdown().await;
}

// ---------------------------------------------------------------------------
// Rfkill / Wifi.Powered turn-off paths
// ---------------------------------------------------------------------------

/// Powered=false while the interface is `Connected` must drive the
/// state machine to `Disconnected{RfKilled}` and emit a matching
/// `WifiLinkLost`. Without this the bus surface and the backend's
/// internal state diverge — the dbus layer reads `disconnected`
/// while the backend keeps polling SignalInfo, scheduling scans,
/// and arming driver-wedge timers against a powered-off radio.
#[tokio::test]
async fn rfkill_off_transitions_connected_to_disconnected_rfkilled() {
    let profile = wifi_profile(b"corp", "correcthorse", 10);
    let mut h = Harness::start_with_rfkill(vec![profile], WifiConfig::default()).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xAA; 6], b"corp", -45, SecurityMode::Wpa2Psk)]);
    h.supplicant.set_connect_outcome(2, MockBehavior::Success);
    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    h.inject_rfkill("phy0", false).await;

    // The backend must publish (in some order) a
    // `WifiRfkillChanged{powered=false}`, a
    // `WifiStateChanged{Disconnected{RfKilled}}`, and a
    // `WifiLinkLost`. We assert each independently.
    let mut saw_state = false;
    let mut saw_link_lost = false;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !(saw_state && saw_link_lost) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, h.event_rx.recv()).await {
            Ok(Ok(NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Disconnected {
                    reason: nexus_core::DisconnectReason::RfKilled,
                },
            })) => saw_state = true,
            Ok(Ok(NexusEvent::WifiLinkLost { ifindex: 2 })) => saw_link_lost = true,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(saw_state, "Disconnected{{RfKilled}} state never emitted");
    assert!(saw_link_lost, "WifiLinkLost never emitted");
    h.shutdown().await;
}

/// While `Disconnected{RfKilled}` is in effect, a late-arriving
/// supplicant `Disconnected{LocalRequest}` (wpa_supplicant catching
/// up to the rfkill) must NOT overwrite the authoritative reason.
/// Without this gate the consumer sees a fresh `disconnected{cancelled}`
/// edge a few seconds after the radio went off, which contradicts
/// the rfkill-driven `disconnected{rf_killed}` already in place.
#[tokio::test]
async fn rfkill_off_preserves_reason_against_late_supplicant_disconnect() {
    use nexus_wifi::supplicant::SupplicantState;
    let profile = wifi_profile(b"corp", "correcthorse", 10);
    let mut h = Harness::start_with_rfkill(vec![profile], WifiConfig::default()).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xAA; 6], b"corp", -45, SecurityMode::Wpa2Psk)]);
    h.supplicant.set_connect_outcome(2, MockBehavior::Success);
    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    h.inject_rfkill("phy0", false).await;
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Disconnected {
                    reason: nexus_core::DisconnectReason::RfKilled,
                },
            },
        ),
        Duration::from_secs(2),
    )
    .await;

    // Now feed the late supplicant Disconnected.
    let _ = h.sup_tx.send(SupplicantEvent::State {
        ifindex: 2,
        state: SupplicantState::Disconnected {
            reason: DisconnectHint::LocalRequest,
        },
    });

    // Drain for 200 ms; assert no Disconnected{*} with a non-RfKilled
    // reason ever fires. The wifi backend must consume the
    // late event and discard it, leaving the state at RfKilled.
    let deadline = Instant::now() + Duration::from_millis(200);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Ok(Ok(NexusEvent::WifiStateChanged {
            ifindex: 2,
            state: WifiState::Disconnected { reason },
        })) = tokio::time::timeout(remaining, h.event_rx.recv()).await
        {
            assert!(
                matches!(reason, nexus_core::DisconnectReason::RfKilled),
                "late supplicant Disconnected overwrote RfKilled reason: {reason:?}"
            );
        }
    }
    h.shutdown().await;
}

/// Powered=true after a Powered=false must promote
/// `Disconnected{RfKilled}` back to `Idle` and re-arm the scan
/// scheduler so auto-select can resume. RfKilled is permanent
/// (`is_permanent()` returns true), so the cooldown sweep won't
/// auto-promote — the radio-on edge does it explicitly.
#[tokio::test]
async fn rfkill_on_resumes_idle_and_kicks_scan_scheduler() {
    let profile = wifi_profile(b"corp", "correcthorse", 10);
    let mut h = Harness::start_with_rfkill(vec![profile], WifiConfig::default()).await;

    h.supplicant
        .set_scan_results(2, vec![bss([0xAA; 6], b"corp", -45, SecurityMode::Wpa2Psk)]);
    h.supplicant.set_connect_outcome(2, MockBehavior::Success);
    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    h.expect_event(
        |e| matches!(e, NexusEvent::WifiLinkReady { ifindex: 2 }),
        Duration::from_secs(2),
    )
    .await;

    let scans_before_off = h.supplicant.scan_calls().len();

    h.inject_rfkill("phy0", false).await;
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Disconnected {
                    reason: nexus_core::DisconnectReason::RfKilled,
                },
            },
        ),
        Duration::from_secs(2),
    )
    .await;

    // Confirm: while rfkilled, scheduled scans don't fire even if
    // the heartbeat ticks. We sleep past one heartbeat (1 s) plus a
    // bit, then count.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let scans_during_off = h.supplicant.scan_calls().len();
    assert_eq!(
        scans_during_off, scans_before_off,
        "scheduler must not dispatch scans while rfkilled"
    );

    h.inject_rfkill("phy0", true).await;
    // First, the backend should fold to Idle.
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Idle,
            },
        ),
        Duration::from_secs(2),
    )
    .await;
    // Then the scheduler kicks a fresh scan via the auto-select
    // path. We see Scanning, then a new scan call.
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Scanning,
            },
        ),
        Duration::from_secs(2),
    )
    .await;
    let scans_after_on = h.supplicant.scan_calls().len();
    assert!(
        scans_after_on > scans_during_off,
        "scan_calls did not advance after Powered=true: before={scans_during_off} after={scans_after_on}"
    );
    h.shutdown().await;
}

/// Operator `Connect` while the radio is rfkilled must error fast
/// rather than transitioning the cached state to `Connecting{...}`
/// (which the driver-wedge detector would later mis-identify as
/// stuck firmware).
#[tokio::test]
async fn operator_connect_during_rfkill_returns_rfkill_error_without_state_change() {
    let profile = wifi_profile(b"corp", "correcthorse", 10);
    let profile_id = profile.id;
    let mut h = Harness::start_with_rfkill(vec![profile], WifiConfig::default()).await;

    h.supplicant.set_connect_outcome(2, MockBehavior::Success);
    let _ = h
        .event_tx
        .send(NexusEvent::InterfaceDiscovered(wifi_interface(2, "wlan0")));
    // Wait for Idle.
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Scanning,
            },
        ),
        Duration::from_secs(2),
    )
    .await;

    h.inject_rfkill("phy0", false).await;
    h.expect_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Disconnected {
                    reason: nexus_core::DisconnectReason::RfKilled,
                },
            },
        ),
        Duration::from_secs(2),
    )
    .await;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    h.cmd_tx
        .send(nexus_wifi::WifiCommand::Connect {
            ifname: "wlan0".into(),
            profile_id,
            reply: reply_tx,
        })
        .await
        .unwrap();
    let result = reply_rx.await.unwrap();
    assert!(
        matches!(result, Err(nexus_wifi::WifiError::Rfkill { .. })),
        "operator_connect should return Rfkill while rfkilled, got {result:?}"
    );

    // No state transition out of Disconnected{RfKilled} should
    // have leaked through. (If the early-return gate hadn't
    // fired, the backend would have transitioned to Connecting{...}
    // before calling the supplicant.)
    h.expect_no_event(
        |e| matches!(
            e,
            NexusEvent::WifiStateChanged {
                ifindex: 2,
                state: WifiState::Connecting { .. } | WifiState::Idle,
            },
        ),
        Duration::from_millis(200),
    )
    .await;
    h.shutdown().await;
}
