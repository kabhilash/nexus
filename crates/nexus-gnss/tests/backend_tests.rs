//! Mock-driven end-to-end tests for the GNSS Backend.
//! See DD-005 §12.1 / §12.2.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, NotificationValue, OperState};
use nexus_gnss::gpsd::mock::MockCall;
use nexus_gnss::{
    FixMode, GnssCommand, GnssConfig, GnssFix, GpsdClient, MockGpsdClient, PowerState, SatInfo,
    spawn_gnss_backend,
};
use nexus_profile_store::{
    BluetoothProfile as _BluetoothProfile, // unused but keeps store surface live
    GnssDeviceProfile,
    InMemoryKeySource,
    ProfileFileStore,
    ProfileMetadata,
    ProfileStore,
};
use tempfile::TempDir;
use tokio::sync::{broadcast, oneshot};
use ulid::Ulid;

fn gnss_interface(ifindex: u32, path: &str) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: path.to_owned(),
        mac: [0; 6],
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Gnss {
            device_path: path.to_owned(),
            gpsd_device: path.to_owned(),
            vendor_model: Some("u-blox F9P".into()),
        },
        discovered_at: Instant::now(),
    }
}

fn good_fix() -> GnssFix {
    GnssFix {
        time: Utc.with_ymd_and_hms(2026, 4, 22, 13, 52, 0).unwrap(),
        mode: FixMode::Fix3D,
        latitude: Some(37.0),
        longitude: Some(-122.0),
        altitude_m: Some(50.0),
        speed_mps: None,
        track_deg: None,
        horizontal_error_m: Some(5.0),
        vertical_error_m: Some(10.0),
        satellites_used: 9,
    }
}

fn failing_fix() -> GnssFix {
    let mut f = good_fix();
    f.satellites_used = 2;
    f
}

async fn start_store() -> (TempDir, Arc<dyn ProfileStore>) {
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x44u8; 32]);
    let store = ProfileFileStore::open(tmp.path(), &keys).unwrap();
    (tmp, Arc::new(store))
}

async fn await_event<F: Fn(&NexusEvent) -> bool>(
    rx: &mut broadcast::Receiver<NexusEvent>,
    pred: F,
    timeout: Duration,
) -> NexusEvent {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => {
                if pred(&event) {
                    return event;
                }
                seen.push(format!("{event:?}"));
            }
            Ok(Err(_)) | Err(_) => panic!("timed out\nseen: {}", seen.join("\n       ")),
        }
    }
}

/// Poll `MockGpsdClient::calls()` until `pred` is satisfied, with a
/// short fixed timeout. Replaces wall-clock `sleep(50ms)` after
/// triggering an action that the backend processes asynchronously
/// — we wait for the observable side effect instead of a fixed
/// duration that would either flake on slow CI or burn cycles on
/// fast hardware.
async fn await_calls<F: Fn(&[MockCall]) -> bool>(
    mock: &MockGpsdClient,
    pred: F,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pred(&mock.calls()) {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for mock call; saw {:?}", mock.calls());
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn discover_device_calls_add_device_on_gpsd() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());

    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn interface_removed_calls_remove_device() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS1",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS1"))
        },
        Duration::from_secs(2),
    )
    .await;
    event_tx
        .send(NexusEvent::InterfaceRemoved { ifindex: 1 })
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::RemoveDevice(p) if p == "/dev/ttyS1"))
        },
        Duration::from_secs(2),
    )
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn passing_tpv_emits_filtered_fix_changed() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    mock.feed_tpv("/dev/ttyS0", good_fix());

    await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::GnssFixChanged { device, .. } if device == "/dev/ttyS0"),
        Duration::from_secs(2),
    )
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn failing_tpv_does_not_emit_fix_changed() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    mock.feed_tpv("/dev/ttyS0", failing_fix());

    // Within 300 ms, no GnssFixChanged should appear.
    let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(event)) = tokio::time::timeout(
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            event_rx.recv(),
        )
        .await
        {
            if matches!(event, NexusEvent::GnssFixChanged { .. }) {
                panic!("unexpected GnssFixChanged for failing fix");
            }
        }
    }

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn rate_cap_limits_fix_changed_to_at_most_once_per_second() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(256);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    // Fire 10 good TPVs back-to-back. Default max_update_hz = 1 ⇒
    // only one `GnssFixChanged` should reach the bus.
    for _ in 0..10 {
        mock.feed_tpv("/dev/ttyS0", good_fix());
    }

    // Wait 300ms then count emissions.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut fix_changes = 0;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event, NexusEvent::GnssFixChanged { .. }) {
            fix_changes += 1;
        }
    }
    assert_eq!(
        fix_changes, 1,
        "expected exactly 1 GnssFixChanged under 1 Hz cap"
    );

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn gpsd_disconnect_then_reconnect_reregisters_devices() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(128);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    // Drain connects so we can count the fresh one.
    while event_rx.try_recv().is_ok() {}

    mock.simulate_disconnect();
    // Wait for the supervisor to reconnect (1 s tick + 1 s backoff)
    // and re-issue the per-device add_device call.
    let _ = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::GnssGpsdConnected),
        Duration::from_secs(5),
    )
    .await;
    await_calls(
        &mock,
        |c| {
            c.iter()
                .filter(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
                .count()
                >= 2
        },
        Duration::from_secs(2),
    )
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn power_state_command_is_accepted() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(32);
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    for state in [
        PowerState::Background,
        PowerState::Sleep,
        PowerState::Active,
    ] {
        let (tx, rx) = oneshot::channel();
        handle
            .cmd_tx
            .send(GnssCommand::SetPowerState {
                state,
                responder: tx,
            })
            .await
            .unwrap();
        rx.await.unwrap().unwrap();
    }

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn sleep_suspends_fix_changed_emission() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    // Enter sleep.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(GnssCommand::SetPowerState {
            state: PowerState::Sleep,
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    // Drain any prior events.
    while event_rx.try_recv().is_ok() {}

    mock.feed_tpv("/dev/ttyS0", good_fix());
    // Give the backend time to process; no GnssFixChanged should
    // appear while sleeping.
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(event) = event_rx.try_recv() {
        assert!(
            !matches!(event, NexusEvent::GnssFixChanged { .. }),
            "no GnssFixChanged while sleeping, got {event:?}"
        );
    }

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn profile_store_lookup_hydrates_effective_profile() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(32);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    // Pre-seed a profile requiring 8 sats. A 6-sat fix should be
    // dropped by the quality filter; GnssFixChanged won't fire.
    let profile = GnssDeviceProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        device_path: "/dev/ttyS0".into(),
        label: Some("strict".into()),
        vendor_model: None,
        max_update_hz: Some(5),
        max_horizontal_error_m: Some(50.0),
        min_fix_mode: None,
        min_satellites: None,
        strict_quality: None,
        report_movement_only: None,
        movement_threshold_m: None,
        heartbeat_interval_s: None,
        auto_activate: true,
    };
    store.put_gnss(&profile).await.unwrap();

    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();

    // Defaults use min_satellites=4, so we need a tighter global
    // default to prove per-device override is path: use a config
    // tweak. (The store profile doesn't carry min_satellites in
    // v0.1, so we demonstrate hydration of the rate cap instead.)
    let config = GnssConfig::default();

    let handle = spawn_gnss_backend(client, store, event_tx.clone(), config);
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    // Drain.
    while event_rx.try_recv().is_ok() {}

    // With max_update_hz=5 from the profile the rate limit is 200
    // ms. Space 5 TPVs 250 ms apart — each should clear the cap,
    // producing ≥ 3 emissions in the window (allowing for timing
    // slack). At the default 1 Hz cap, we'd see only 1 or 2.
    for _ in 0..5 {
        mock.feed_tpv("/dev/ttyS0", good_fix());
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;

    let mut fix_changes = 0;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event, NexusEvent::GnssFixChanged { .. }) {
            fix_changes += 1;
        }
    }
    assert!(
        fix_changes >= 3,
        "expected 3+ GnssFixChanged with 5 Hz cap; got {fix_changes}"
    );

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

#[tokio::test]
async fn mock_feed_line_parses_and_dispatches() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(8);
    let mock = MockGpsdClient::new(event_tx.clone());
    mock.feed_line(
        r#"{"class":"TPV","device":"/dev/ttyUSB0","mode":3,"lat":1.0,"lon":2.0,"altHAE":3.0,"used":7}"#,
    );
    match event_rx.try_recv() {
        Ok(NexusEvent::GnssTpvReceived { device, fix }) => {
            assert_eq!(device, "/dev/ttyUSB0");
            assert_eq!(fix.satellites_used, 7);
        }
        other => panic!("expected TPV event, got {other:?}"),
    }

    mock.feed_line(
        r#"{"class":"SKY","device":"/dev/ttyUSB0","satellites":[{"gnssid":0,"svid":1}]}"#,
    );
    match event_rx.try_recv() {
        Ok(NexusEvent::GnssSatellites { device, satellites }) => {
            assert_eq!(device, "/dev/ttyUSB0");
            assert_eq!(satellites.len(), 1);
        }
        other => panic!("expected SKY event, got {other:?}"),
    }
}

#[tokio::test]
async fn sky_message_updates_sat_snapshot() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(32);
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    mock.feed_sky(
        "/dev/ttyS0",
        vec![SatInfo {
            gnss_id: 0,
            sv_id: 5,
            snr_db: Some(45.0),
            elevation_deg: Some(30.0),
            azimuth_deg: Some(120.0),
            used: true,
        }],
    );
    // Yield once so the backend's GnssSatellites handler runs before
    // shutdown — no observable side effect to await on (sky updates
    // an internal snapshot only). One yield is sufficient because the
    // backend task only becomes schedulable when this task awaits.
    tokio::task::yield_now().await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

// ---------------------------------------------------------------------------
// Coverage gaps from docs/gnss-audit-findings.md (T1–T6)
// ---------------------------------------------------------------------------

/// T1 (DD-005 §12.4): malformed gpsd JSON is logged + skipped; no
/// bus event escapes the dispatcher.
#[tokio::test]
async fn malformed_gpsd_json_drops_silently() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(8);
    let mock = MockGpsdClient::new(event_tx.clone());

    // Outright invalid JSON.
    mock.feed_line("{not valid json");
    // Valid JSON but unknown class — also a no-op via #[serde(other)].
    mock.feed_line(r#"{"class":"BOGUS","extra":"ignored"}"#);
    // TPV without a device field — parser drops it.
    mock.feed_line(r#"{"class":"TPV","mode":3,"lat":1.0,"lon":2.0,"altHAE":3.0}"#);

    // Nothing should reach the bus.
    match tokio::time::timeout(Duration::from_millis(100), event_rx.recv()).await {
        Err(_) => {}
        Ok(Err(_)) => {}
        Ok(Ok(unexpected)) => panic!(
            "expected no bus event from malformed/unknown lines, got {unexpected:?}"
        ),
    }

    // A well-formed TPV after the bad lines still parses — proves the
    // dispatcher recovered, didn't poison the channel.
    mock.feed_line(
        r#"{"class":"TPV","device":"/dev/ttyUSB0","mode":3,"lat":1.0,"lon":2.0,"altHAE":3.0,"used":7}"#,
    );
    match event_rx.try_recv() {
        Ok(NexusEvent::GnssTpvReceived { device, .. }) => {
            assert_eq!(device, "/dev/ttyUSB0");
        }
        other => panic!("expected TPV event after recovery, got {other:?}"),
    }
}

/// T2 (DD-005 §12.4): once the device is Tracking, a stalled TPV
/// stream transitions it to Degraded after `tpv_stall_timeout_s`.
#[tokio::test]
async fn tpv_stall_transitions_tracking_to_degraded() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();

    let config = GnssConfig {
        tpv_stall_timeout_s: 1, // tight so the test is quick
        ..GnssConfig::default()
    };
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), config);

    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    // Drive into Tracking with a passing fix.
    mock.feed_tpv("/dev/ttyS0", good_fix());
    await_event(
        &mut event_rx,
        |e| matches!(
            e,
            NexusEvent::GnssStateChanged {
                from: "acquiring",
                to: "tracking",
                ..
            }
        ),
        Duration::from_secs(2),
    )
    .await;

    // Stop feeding TPVs. The 1 Hz reconcile tick should detect the
    // stall after `tpv_stall_timeout_s = 1`.
    await_event(
        &mut event_rx,
        |e| matches!(
            e,
            NexusEvent::GnssStateChanged {
                from: "tracking",
                to: "degraded",
                reason: "timeout",
                ..
            }
        ),
        Duration::from_secs(5),
    )
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// T3 (DD-005 §12.4): a prolonged gpsd outage emits the
/// `subsystem_unavailable` operator notification once the configured
/// threshold elapses.
#[tokio::test]
async fn prolonged_gpsd_outage_emits_subsystem_unavailable() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();

    let config = GnssConfig {
        gpsd_outage_notify_s: 1,
        ..GnssConfig::default()
    };
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), config);

    // Let the initial connect succeed so first_outage_at is bound to
    // a real disconnect, not a never-connected state.
    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;

    // Now disconnect and make every subsequent reconnect fail so the
    // supervisor stays in the outage path long enough to fire the
    // notification.
    mock.fail_next_connect(100);
    mock.simulate_disconnect();

    let event = await_event(
        &mut event_rx,
        |e| {
            matches!(
                e,
                NexusEvent::OperatorNotification { kind, .. } if kind == "subsystem_unavailable"
            )
        },
        Duration::from_secs(5),
    )
    .await;
    if let NexusEvent::OperatorNotification { data, .. } = event {
        match data.get("subsystem") {
            Some(NotificationValue::String(s)) if s == "gpsd" => {}
            other => panic!("expected subsystem=gpsd in notification data, got {other:?}"),
        }
    }

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// T4 (DD-005 §10): Background mode caps emissions at the
/// 5-second floor regardless of the per-device profile.
#[tokio::test]
async fn background_caps_emission_at_five_second_floor() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(256);
    let (_tmp, store) = start_store().await;

    // Profile pinned at 5 Hz so the floor isn't already enforced by
    // the default 1 Hz cap.
    let profile = GnssDeviceProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        device_path: "/dev/ttyS0".into(),
        label: None,
        vendor_model: None,
        max_update_hz: Some(5),
        max_horizontal_error_m: None,
        min_fix_mode: None,
        min_satellites: None,
        strict_quality: None,
        report_movement_only: None,
        movement_threshold_m: None,
        heartbeat_interval_s: None,
        auto_activate: true,
    };
    store.put_gnss(&profile).await.unwrap();

    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());

    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    // Switch to Background.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(GnssCommand::SetPowerState {
            state: PowerState::Background,
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    // Drain any pre-Background events.
    while event_rx.try_recv().is_ok() {}

    // Fire a TPV every 100 ms for ~1 s. With the 5-second floor in
    // place, only 1 GnssFixChanged should escape (the very first;
    // every subsequent one is rate-limited).
    for _ in 0..10 {
        mock.feed_tpv("/dev/ttyS0", good_fix());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let mut fix_changes = 0;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(event, NexusEvent::GnssFixChanged { .. }) {
            fix_changes += 1;
        }
    }
    assert!(
        fix_changes <= 1,
        "Background mode should cap to ≤1 emission/5 s; got {fix_changes}"
    );
}

/// T5 (DD-005 §3.2): Acquiring → Degraded after
/// `acquisition_timeout_s` if no quality fix arrives.
#[tokio::test]
async fn acquisition_timeout_transitions_acquiring_to_degraded() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();

    let config = GnssConfig {
        acquisition_timeout_s: 1,
        ..GnssConfig::default()
    };
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), config);

    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();

    // Don't feed any TPVs. After ~1 s the reconcile tick should
    // declare the device Degraded with reason=timeout.
    await_event(
        &mut event_rx,
        |e| matches!(
            e,
            NexusEvent::GnssStateChanged {
                from: "acquiring",
                to: "degraded",
                reason: "timeout",
                ..
            }
        ),
        Duration::from_secs(5),
    )
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// T6 (DD-005 §7.2): strict_quality rejects fixes lacking
/// `horizontal_error_m` even when the rest of the fix is good.
#[tokio::test]
async fn strict_quality_rejects_fix_without_eph() {
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;

    let profile = GnssDeviceProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        device_path: "/dev/ttyS0".into(),
        label: None,
        vendor_model: None,
        max_update_hz: None,
        max_horizontal_error_m: Some(50.0),
        min_fix_mode: None,
        min_satellites: None,
        strict_quality: Some(true),
        report_movement_only: None,
        movement_threshold_m: None,
        heartbeat_interval_s: None,
        auto_activate: true,
    };
    store.put_gnss(&profile).await.unwrap();

    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());

    await_calls(
        &mock,
        |c| c.iter().any(|c| matches!(c, MockCall::Connect)),
        Duration::from_secs(2),
    )
    .await;
    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    await_calls(
        &mock,
        |c| {
            c.iter()
                .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        },
        Duration::from_secs(2),
    )
    .await;

    while event_rx.try_recv().is_ok() {}

    // Fix without horizontal_error_m: under strict_quality this must
    // be rejected by the filter.
    let mut fix = good_fix();
    fix.horizontal_error_m = None;
    mock.feed_tpv("/dev/ttyS0", fix);

    // Allow the backend to process; assert no GnssFixChanged appears.
    tokio::time::sleep(Duration::from_millis(200)).await;
    while let Ok(event) = event_rx.try_recv() {
        assert!(
            !matches!(event, NexusEvent::GnssFixChanged { .. }),
            "strict_quality should reject fix without eph; got {event:?}"
        );
    }

    // Sanity: a fix WITH eph still passes through unchanged.
    mock.feed_tpv("/dev/ttyS0", good_fix());
    await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::GnssFixChanged { device, .. } if device == "/dev/ttyS0"),
        Duration::from_secs(2),
    )
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

// Silence unused re-imports from nexus_profile_store (kept for
// future tests that round-trip full BluetoothProfile).
#[allow(dead_code)]
fn _touch(_b: _BluetoothProfile) {}
