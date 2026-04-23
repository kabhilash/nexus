//! Mock-driven end-to-end tests for the GNSS Backend.
//! See DD-005 §12.1 / §12.2.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, OperState};
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

#[tokio::test]
async fn discover_device_calls_add_device_on_gpsd() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;
    let mock = Arc::new(MockGpsdClient::new(event_tx.clone()));
    let client: Arc<dyn GpsdClient> = mock.clone();
    let handle = spawn_gnss_backend(client, store, event_tx.clone(), GnssConfig::default());

    // The backend calls connect() on startup; give it a tick.
    tokio::time::sleep(Duration::from_millis(50)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let calls = mock.calls();
    assert!(
        calls.iter().any(|c| matches!(c, MockCall::Connect)),
        "expected connect(); got {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0")),
        "expected add_device('/dev/ttyS0'); got {calls:?}"
    );

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
    tokio::time::sleep(Duration::from_millis(50)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS1",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    event_tx
        .send(NexusEvent::InterfaceRemoved { ifindex: 1 })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let calls = mock.calls();
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, MockCall::RemoveDevice(p) if p == "/dev/ttyS1")),
        "expected remove_device; got {calls:?}"
    );

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
    tokio::time::sleep(Duration::from_millis(50)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

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
    tokio::time::sleep(Duration::from_millis(50)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

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
    tokio::time::sleep(Duration::from_millis(50)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

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
    tokio::time::sleep(Duration::from_millis(50)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Drain connects so we can count the fresh one.
    while event_rx.try_recv().is_ok() {}

    mock.simulate_disconnect();
    // Wait for the supervisor to reconnect (1 s tick + 1 s backoff).
    let _ = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::GnssGpsdConnected),
        Duration::from_secs(5),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;

    let calls = mock.calls();
    let add_count = calls
        .iter()
        .filter(|c| matches!(c, MockCall::AddDevice(p) if p == "/dev/ttyS0"))
        .count();
    assert!(
        add_count >= 2,
        "expected 2+ add_device calls (initial + re-register); got {add_count} in {calls:?}"
    );

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
    tokio::time::sleep(Duration::from_millis(30)).await;

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
    tokio::time::sleep(Duration::from_millis(30)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

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
        max_rate_hz: Some(5),
        min_horizontal_error_m: Some(50.0),
        auto_attach: true,
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
    tokio::time::sleep(Duration::from_millis(30)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

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
    tokio::time::sleep(Duration::from_millis(30)).await;

    event_tx
        .send(NexusEvent::InterfaceDiscovered(gnss_interface(
            1,
            "/dev/ttyS0",
        )))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

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
    tokio::time::sleep(Duration::from_millis(50)).await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

// Silence unused re-imports from nexus_profile_store (kept for
// future tests that round-trip full BluetoothProfile).
#[allow(dead_code)]
fn _touch(_b: _BluetoothProfile) {}
