//! Phase-6 `watch` tests. DD-008 §7.4.
//!
//! Covers:
//! - all 16 synthesis-table rows round-trip through the output
//!   layer in JSON + human + terse formats;
//! - filter matching (single, multi-AND, wildcard, no-match);
//! - subset classification (`watch wifi` drops non-wifi events);
//! - termination (stream end → Ok + "nexusd disconnected"
//!   stderr; SIGINT via scripted future → Ok).

#![cfg(feature = "interactive-flows-testing")]

use std::collections::HashMap;

use nexus_client::commands;
use nexus_client::output::{OutputFormat, RenderContext};
use nexus_client::watch::synthesize;
use nexus_client::watch::{FieldValue, Filter, MockWatchStream, WatchEvent, WatchSubset};

fn fixture_events() -> Vec<WatchEvent> {
    // One event per DD-008 §7.4 row.
    let mut data: HashMap<String, FieldValue> = HashMap::new();
    data.insert("subsystem".into(), FieldValue::String("bluez".into()));
    data.insert("duration_s".into(), FieldValue::Uint(60));
    vec![
        synthesize::interface_added("eth0", "ethernet"),
        synthesize::interface_removed("eth0"),
        synthesize::link_state("eth0", "up"),
        synthesize::eth_auth_state("eth0", "authenticated"),
        synthesize::wifi_state("wlan0", "connected"),
        synthesize::wifi_scan("wlan0", 4),
        synthesize::wifi_signal("wlan0", -52),
        synthesize::bt_adapter_state("hci0", "powered"),
        synthesize::bt_device_state("hci0", "AA:BB:CC:DD:EE:01", "connected"),
        synthesize::bt_pairing_started("hci0", "job-1", "/org/bluez/hci0/dev_AA"),
        synthesize::bt_pairing_prompt(
            "hci0",
            "job-1",
            "request_confirmation",
            Some(123_456),
            None,
            None,
        ),
        synthesize::bt_pairing_complete("hci0", "job-1", true, ""),
        synthesize::gnss_fix("/dev/gps0", 3, 51.5, -0.08, Some(12.0), Some(2.1)),
        synthesize::profile_changed("wifi", "added", "01H9"),
        synthesize::notification("subsystem_unavailable", data),
        synthesize::master_key_rotated("job-2", "success", 3, 120),
    ]
}

async fn run_watch(
    events: Vec<WatchEvent>,
    subset: WatchSubset,
    filters: Vec<Filter>,
    format: OutputFormat,
) -> (String, String) {
    let stream = Box::new(MockWatchStream::new(events));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    commands::watch::run(
        stream,
        subset,
        filters,
        format,
        &RenderContext::default(),
        &mut stdout,
        &mut stderr,
        // Never-firing "signal" — the mock closes when the queue
        // drains, which ends the run cleanly.
        std::future::pending::<std::io::Result<()>>(),
    )
    .await
    .expect("watch run");
    (
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

/// Every row in the §7.4 synthesis table produces one NDJSON line
/// on stdout.
#[tokio::test]
async fn all_sixteen_rows_emit_json_lines() {
    let events = fixture_events();
    let expected_kinds: Vec<String> = events.iter().map(|e| e.kind.clone()).collect();
    let (stdout, _stderr) =
        run_watch(events, WatchSubset::Events, vec![], OutputFormat::Json).await;
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 16, "expected 16 lines, got: {stdout}");
    for (i, line) in lines.iter().enumerate() {
        // Each line parses as valid JSON.
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {line} ({e})"));
        assert_eq!(v["kind"], expected_kinds[i]);
        assert!(v["time"].is_string());
    }
}

#[tokio::test]
async fn json_output_is_ndjson_single_line_per_event() {
    let events = vec![synthesize::wifi_scan("wlan0", 3)];
    let (stdout, _) = run_watch(events, WatchSubset::Events, vec![], OutputFormat::Json).await;
    // Exactly one newline.
    assert_eq!(stdout.matches('\n').count(), 1, "got {stdout:?}");
    // No pretty-printed newlines inside the object.
    assert!(
        !stdout.contains("\n  "),
        "pretty-printed JSON leaked: {stdout}"
    );
}

#[tokio::test]
async fn human_output_has_time_and_kind_leading() {
    let events = vec![synthesize::link_state("eth0", "up")];
    let (stdout, _) = run_watch(events, WatchSubset::Events, vec![], OutputFormat::Human).await;
    let line = stdout.lines().next().unwrap();
    // `<time>  <kind>  iface=eth0 state=up`.
    let parts: Vec<&str> = line.splitn(3, "  ").collect();
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[1], "link-state");
    assert!(parts[2].contains("iface=eth0"));
    assert!(parts[2].contains("state=up"));
}

#[tokio::test]
async fn terse_output_uses_separator_and_key_eq_value() {
    let events = vec![synthesize::link_state("eth0", "up")];
    let (stdout, _) = run_watch(events, WatchSubset::Events, vec![], OutputFormat::Terse).await;
    let line = stdout.lines().next().unwrap();
    // Default separator is `:`.
    assert!(line.contains("link-state"));
    assert!(line.contains("iface=eth0"));
    assert!(line.contains("state=up"));
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

#[tokio::test]
async fn single_filter_narrows_output() {
    let events = fixture_events();
    let filters = vec![Filter::parse("iface=eth0").unwrap()];
    let (stdout, _) = run_watch(events, WatchSubset::Events, filters, OutputFormat::Json).await;
    let lines: Vec<&str> = stdout.lines().collect();
    // eth0 appears in: interface-added, interface-removed,
    // link-state, eth-auth-state → 4 rows.
    assert_eq!(lines.len(), 4, "got {stdout}");
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["iface"], "eth0");
    }
}

#[tokio::test]
async fn multi_filter_and_semantics() {
    let events = fixture_events();
    let filters = vec![
        Filter::parse("iface=eth0").unwrap(),
        Filter::parse("kind=link-*").unwrap(),
    ];
    let (stdout, _) = run_watch(events, WatchSubset::Events, filters, OutputFormat::Json).await;
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "got {stdout}");
    let v: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(v["kind"], "link-state");
}

#[tokio::test]
async fn wildcard_filter_matches_by_prefix() {
    let events = fixture_events();
    let filters = vec![Filter::parse("kind=bt-*").unwrap()];
    let (stdout, _) = run_watch(events, WatchSubset::Events, filters, OutputFormat::Json).await;
    let lines: Vec<&str> = stdout.lines().collect();
    // bt-adapter-state, bt-device-state, bt-pairing-started,
    // bt-pairing-prompt, bt-pairing-complete → 5 rows.
    assert_eq!(lines.len(), 5);
}

#[tokio::test]
async fn filter_no_match_means_no_output() {
    let events = fixture_events();
    let filters = vec![Filter::parse("iface=wlan9").unwrap()];
    let (stdout, _) = run_watch(events, WatchSubset::Events, filters, OutputFormat::Json).await;
    assert_eq!(stdout.lines().count(), 0);
}

// ---------------------------------------------------------------------------
// Subsets
// ---------------------------------------------------------------------------

#[tokio::test]
async fn watch_wifi_subset_only_wifi_events() {
    let events = fixture_events();
    let (stdout, _) = run_watch(events, WatchSubset::Wifi, vec![], OutputFormat::Json).await;
    let kinds: Vec<String> = stdout
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["kind"].as_str().unwrap().to_owned()
        })
        .collect();
    for k in &kinds {
        assert!(k.starts_with("wifi-"), "unexpected kind: {k}");
    }
    assert_eq!(kinds.len(), 3);
}

#[tokio::test]
async fn watch_bt_subset_only_bt_events() {
    let events = fixture_events();
    let (stdout, _) = run_watch(events, WatchSubset::Bt, vec![], OutputFormat::Json).await;
    let kinds: Vec<String> = stdout
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["kind"].as_str().unwrap().to_owned()
        })
        .collect();
    for k in &kinds {
        assert!(k.starts_with("bt-"), "unexpected kind: {k}");
    }
    assert_eq!(kinds.len(), 5);
}

#[tokio::test]
async fn watch_iface_subset_picks_interface_and_link_state() {
    let events = fixture_events();
    let (stdout, _) = run_watch(events, WatchSubset::Iface, vec![], OutputFormat::Json).await;
    let kinds: Vec<String> = stdout
        .lines()
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["kind"].as_str().unwrap().to_owned()
        })
        .collect();
    assert!(kinds.contains(&"interface-added".to_string()));
    assert!(kinds.contains(&"interface-removed".to_string()));
    assert!(kinds.contains(&"link-state".to_string()));
    assert!(!kinds.iter().any(|k| k.starts_with("wifi-")));
    assert!(!kinds.iter().any(|k| k.starts_with("bt-")));
}

// ---------------------------------------------------------------------------
// Termination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_end_emits_disconnected_on_stderr_and_exits_0() {
    let (stdout, stderr) = run_watch(
        fixture_events(),
        WatchSubset::Events,
        vec![],
        OutputFormat::Json,
    )
    .await;
    assert_eq!(stdout.lines().count(), 16);
    assert!(stderr.contains("nexusd disconnected"), "stderr: {stderr}");
}

#[tokio::test]
async fn sigint_before_any_event_exits_0() {
    // An open stream with no events → select! hands control to the
    // signal future, which fires immediately.
    let stream = Box::new(MockWatchStream::new_open(vec![]));
    let (tx, rx) = tokio::sync::oneshot::channel();
    let _ = tx.send(Ok::<(), std::io::Error>(()));
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    commands::watch::run(
        stream,
        WatchSubset::Events,
        vec![],
        OutputFormat::Json,
        &RenderContext::default(),
        &mut stdout,
        &mut stderr,
        async move { rx.await.unwrap() },
    )
    .await
    .expect("clean sigint exit");
    assert_eq!(stdout.len(), 0);
}

#[tokio::test]
async fn pretty_format_warns_and_falls_back_to_human() {
    let events = vec![synthesize::link_state("eth0", "up")];
    let (stdout, stderr) =
        run_watch(events, WatchSubset::Events, vec![], OutputFormat::Pretty).await;
    assert!(stderr.contains("--pretty"), "stderr: {stderr}");
    // Human fallback retains the "link-state" line.
    assert!(stdout.contains("link-state"));
}
