# DD-005 GNSS Backend — Audit Findings

**Audit date:** 2026-04-24 (revised after C1–C3, K1–K4, S1–S5,
T1–T6 fixes)
**Crates audited:** `crates/nexus-gnss`, plus the GNSS plumbing in
`crates/nexus-core`, `crates/nexus-profile-store`, and
`crates/nexus-daemon`.
**Reference:** `dd-005-gnss-backend.md`
**Build / test status (post-fix):** full workspace `cargo test` passes
in `nexus-v2-dev` devcontainer (744 tests). `nexus-gnss` itself:
33 unit (up from 29 — added 2 Background-floor tests, 1 full-override
test, 1 legacy-alias regression test) + 17 backend mock (up from 11
— added T1–T6) + 3 json_client_tcp; 2 `integration-gpsd` tests gated
behind `--features integration-gpsd -- --ignored`.
`nexus-profile-store` 50 unit + 11 roundtrip + others.

**Crate version trail (per CLAUDE.md SemVer):**
- `nexus-core` 0.3.0 → 0.4.0 (T2/T5: new `NexusEvent::GnssStateChanged`
  variant for backend lifecycle transitions).
- `nexus-gnss` 0.1.0 → 0.2.0 (C1: `should_emit` signature change)
  → 0.3.0 (S4/S5: removed `GnssCommand::CurrentFix` variant and the
  `MockGpsdClient::inject_error` / `set_current_fix` helpers)
  → 0.4.0 (T2/T3/T5: emits `GnssStateChanged`, reintroduces a
  minimal `MockGpsdClient::fail_next_connect` for outage tests).
- `nexus-profile-store` 0.2.0 → 0.3.0 (C3: new public type
  `FixModeOnDisk` + new `GnssDeviceProfile` fields) → 0.4.0 (K1:
  Rust struct field renames; on-disk format stays compatible via
  serde aliases).
- `nexus-daemon` 0.10.1 → 0.11.0 (C2: new public
  `GnssDefaultsSection`).

Severity legend: **C** = correctness, **K** = consistency, **S** = style,
**T** = test-coverage gap.

**Status overview:** C1, C2, C3 — fixed. K1, K2, K3, K4 — fixed.
S1, S2, S3, S4, S5 — fixed. T1, T2, T3, T4, T5, T6 — fixed.
T7, T8, T9, T10 — open (low priority).

---

## Summary

The crate implements DD-005's two-tier event flow, gpsd JSON protocol,
per-device state machine, quality filter, emission throttling,
supervisor reconnect, and metrics correctly in the steady-state happy
paths. The three correctness gaps originally found (configuration
plumbing and the Background power-state branch) and the four
consistency gaps (profile-field naming, trait signature, DD wording
on bus subscription, file-layout drift) are all now closed. The
on-disk profile schema covers every DD-005 §9 knob and uses field
names that match the DD; legacy `min_horizontal_error_m` /
`max_rate_hz` / `auto_attach` keys still load via serde aliases.
Test coverage is solid for filtering / parsing / state transitions
but remains thin for fault injection (§12.4) and timeout-driven
backend transitions — see the open T-list below.

---

## Correctness findings

### C1 — Background power state does not throttle emission rate (RESOLVED)
- **DD-005 §10:** in `Background`, "the profile's effective
  `max_update_hz` is clamped to 0.2 (5 s interval)."
- **Original code:** `apply_power_state` only set `self.power_state`
  and reset `last_tpv_at` on wake. `should_emit` read
  `profile.max_update_hz` directly with no power-state knowledge.
- **Fix:** `should_emit` now takes `power_state: PowerState` and
  enforces a 5 s `BACKGROUND_MIN_INTERVAL` floor on top of the
  per-device profile when `Background`. `backend.rs::on_tpv` captures
  `self.power_state` ahead of the mutable borrow and threads it
  through. Two new lifecycle tests
  (`background_floor_suppresses_within_five_seconds` and
  `background_floor_emits_after_five_seconds`) lock the behaviour in.
  `nexus-gnss` 0.1.0 → 0.2.0 (breaking — public function signature).

### C2 — `[gnss.defaults]` TOML table is unwired (RESOLVED)
- **DD-005 §8:** documents `[gnss.defaults]` for `min_fix_mode`,
  `min_satellites`, `max_horizontal_error_m`, `strict_quality`,
  `max_update_hz`, `report_movement_only`, `movement_threshold_m`,
  `heartbeat_interval_s`.
- **Original code:** `GnssSection` had no `defaults` field;
  `build_gnss_config` always filled `defaults: GnssDefaults::default()`.
- **Fix:** new `GnssDefaultsSection` in
  `nexus-daemon/src/config.rs` with the full DD-005 §8 field set
  (using snake-case `nexus_profile_store::FixModeOnDisk` for
  `min_fix_mode`), exposed through `nexus-daemon`'s lib re-exports
  and translated by `build_gnss_config`. `reload.rs::diff_config`
  reports each `gnss.defaults.*` key as deferred. The
  `parse_with_overrides_round_trips` test was extended to assert
  every new key. `nexus-daemon` 0.10.1 → 0.11.0.

### C3 — Per-device profile fields are a strict subset of DD-005 §9 (RESOLVED)
- **DD-005 §9:** lists `vendor_model`, `min_fix_mode`,
  `min_satellites`, `max_horizontal_error_m`, `strict_quality`,
  `max_update_hz`, `report_movement_only`, `movement_threshold_m`,
  `heartbeat_interval_s`, `auto_activate`.
- **Original code:** `GnssDeviceProfile` persisted only `device_path`,
  `label`, `max_rate_hz`, `min_horizontal_error_m`, `auto_attach`.
  `profile.rs::hydrate` read only those four overrides.
- **Fix:** `GnssDeviceProfile` gains `vendor_model`, `min_fix_mode`
  (`Option<FixModeOnDisk>`), `min_satellites`, `strict_quality`,
  `report_movement_only`, `movement_threshold_m`,
  `heartbeat_interval_s`. All new fields are `Option`-wrapped with
  `#[serde(default)]` so existing v1 profiles still load. New
  `FixModeOnDisk` enum carries the DD §8 snake-case strings and is
  re-exported from the crate root. `hydrate` applies every override;
  `vendor_model` falls into `EffectiveProfile.label` when no explicit
  label is set. New `stored_overrides_every_threshold` unit test
  exercises each override; `roundtrip.rs::sample_gnss` updated to set
  the full field set. `nexus-profile-store` 0.2.0 → 0.3.0.

---

## Consistency findings

### K1 — Profile field naming drifts from DD-005, with one misleading name (RESOLVED)
- DD §9 → on-disk drift was `auto_activate` ↔ `auto_attach`,
  `max_update_hz` ↔ `max_rate_hz`,
  `max_horizontal_error_m` ↔ `min_horizontal_error_m`. The
  `min_horizontal_error_m` name was actively misleading: the field
  was used as a *maximum* threshold in `hydrate`, but the docstring
  claimed it was a minimum.
- **Fix:** `nexus-profile-store/src/types/gnss.rs` renames the three
  Rust fields (`max_rate_hz` → `max_update_hz`,
  `min_horizontal_error_m` → `max_horizontal_error_m`, `auto_attach`
  → `auto_activate`). Each carries `#[serde(alias = "...")]` for the
  legacy on-disk key so existing v1 profiles still deserialize.
  `nexus-gnss::profile::hydrate`, `build_stored_profile`, and every
  test fixture were updated. A new
  `legacy_field_aliases_still_load` test asserts the alias path
  works. The `vendor_model` ↔ `label` "drift" is left as-is because
  the v0.2 schema deliberately keeps both: `vendor_model` is the
  udev-derived hint (DD-005 §9), `label` is operator-customizable.
  `nexus-profile-store` 0.3.0 → 0.4.0 (struct-field rename — Rust
  signature break even though the on-disk format stays compatible).

### K2 — `GpsdClient::connect()` signature deviates from DD pseudocode (RESOLVED)
- **DD-005 §4.1:** previously specified `async fn connect(&mut self)`.
- **Implementation reality:** `gpsd/mod.rs::GpsdClient` uses `&self`
  with interior mutability (`Arc<Mutex<…>>`) so `Arc<dyn GpsdClient>`
  can be shared across the supervisor / discovery / future
  diagnostic call sites without external locking.
- **Fix:** DD-005 §4.1 updated. The trait now reads `async fn
  connect(&self)` (and matching `&self` on `add_device`,
  `remove_device`), with a paragraph documenting why the interior-
  mutability pattern is required given the `Arc<dyn …>` storage
  model.

### K3 — Backend re-subscribes to its own emissions (RESOLVED)
- **DD-005 §5.1:** previously asserted "The backend subscribes only
  to `GnssTpvReceived`; it never subscribes to `GnssFixChanged`."
- **Implementation reality:** `backend.rs::GnssBackend::new` calls
  `event_tx.subscribe()` once and gets a `broadcast::Receiver` that
  fans out every variant. There is no per-variant filter on a
  `tokio::sync::broadcast` channel. `GnssFixChanged` events come
  back to the receiver and are dropped by the catch-all `_ => {}`
  match arm — no logical loop.
- **Fix:** DD-005 §5.1 wording rewritten to describe the actual
  shared-channel pattern (the backend filters at dispatch, not at
  subscription). No code change required.

### K4 — DD-005 §1.1 file layout doesn't match the repo (RESOLVED)
- DD listed `tests/parse.rs`, `tests/lifecycle.rs`,
  `tests/fix_filter.rs`. The actual layout had
  `tests/backend_tests.rs`, `tests/integration_gpsd.rs`,
  `tests/json_client_tcp.rs`, plus inline `#[cfg(test)] mod tests`
  blocks in every `src/*.rs` module.
- **Fix:** DD-005 §1.1 updated to reflect the actual tree, including
  `config.rs`, `metrics.rs`, the `gpsd/mock.rs` testing helper, and
  the `bin/demo.rs` binary — none of which the original layout
  table named.

---

## Style / minor findings

### S1 — Unused EffectiveProfile import + _touch workaround (RESOLVED)
- **Original code:** `backend.rs:23` imported `EffectiveProfile`
  but referenced it only via the no-op `_touch` function at the
  bottom of the file — a workaround to suppress the unused-import
  warning.
- **Fix:** Both removed. `EffectiveProfile` is still re-exported
  from the crate root via `lib.rs`; nothing in `backend.rs`
  needs the type by name.

### S2 — O(n) device-by-path lookup per TPV (RESOLVED)
- **Original code:** `device_by_path_mut` / `device_by_path` linear-
  scanned every entry in the `HashMap<u32, GnssDeviceEntry>` for
  every TPV.
- **Fix:** New `path_index: HashMap<String, u32>` secondary index
  maintained on discover (`on_interface_discovered`) and remove
  (`on_interface_removed`). `device_by_path[_mut]` is now an O(1)
  two-step (path → ifindex → entry) lookup.

### S3 — Wall-clock sleeps in backend tests (RESOLVED)
- **Original code:** Most mock-driven tests used
  `tokio::time::sleep(50ms)` after `spawn_gnss_backend` and after
  bus sends, hoping the backend would have processed by then. Both
  flake-prone on slow hardware and wasteful on fast hardware.
- **Fix:** New `await_calls` test helper polls
  `MockGpsdClient::calls()` until a predicate matches, with a 2 s
  timeout. Every "sleep so the backend processes" is replaced with
  a poll for the observable mock-call side effect (`MockCall::Connect`,
  `MockCall::AddDevice("/dev/ttyS0")`, `MockCall::RemoveDevice(...)`).
  Deliberate timing windows (rate-cap absence checks, the 250 ms
  spacing in `profile_store_lookup_hydrates_effective_profile`)
  remain — those tests verify timing behaviour, not synchronization.

### S4 — Dead `GnssCommand::CurrentFix` path (RESOLVED)
- **Original code:** `GnssCommand::CurrentFix` variant existed and
  routed to `JsonGpsdClient::current_fix` (a permanent `Ok(None)`
  stub). No production caller wired it.
- **Fix:** Removed the `GnssCommand::CurrentFix` variant and its
  dispatch arm. The `GpsdClient::current_fix` trait method stays
  as an extension point for future diagnostic flows. `nexus-gnss`
  0.2.0 → 0.3.0 (public enum variant removal is breaking).

### S5 — Unused MockGpsdClient infrastructure (RESOLVED)
- **Original code:** `MockGpsdClient::inject_error` (per-method
  canned-error queue), `MockGpsdClient::set_current_fix`, and the
  matching `MockState::canned_errors` / `current_fix` fields. No
  test referenced any of them.
- **Fix:** Removed the helpers, the `consume_err` plumbing, the
  `canned_errors` / `current_fix` fields on `MockState`, and the
  unused `GnssError` import. The mock is now strictly call-recording
  + bus-emission; future fault-injection tests (T1–T3) can add
  back the queue when needed.

---

## Test-coverage gaps vs DD-005 §12

### T1 (§12.4 malformed JSON) (RESOLVED)
- New `malformed_gpsd_json_drops_silently` test feeds three
  pathological lines through `MockGpsdClient::feed_line` (truly
  invalid JSON, valid JSON with an unknown class, valid TPV without
  a `device` field) and asserts no `NexusEvent` reaches the bus.
  A trailing well-formed TPV proves the dispatcher recovered.

### T2 (§12.4 TPV stall) (RESOLVED)
- New `tpv_stall_transitions_tracking_to_degraded` test runs the
  backend with `tpv_stall_timeout_s = 1`, drives the device into
  Tracking with a passing TPV, then waits for the
  `GnssStateChanged { from: "tracking", to: "degraded", reason:
  "timeout", … }` event from the reconcile tick. Required adding
  the new `NexusEvent::GnssStateChanged` variant to nexus-core so
  the transition is bus-observable (also useful for the future
  D-Bus `fi.nexus.Gnss.StateChanged` signal).

### T3 (§12.4 prolonged outage notification) (RESOLVED)
- New `prolonged_gpsd_outage_emits_subsystem_unavailable` test runs
  with `gpsd_outage_notify_s = 1`, lets the initial connect succeed,
  then queues 100 `fail_next_connect` failures and calls
  `simulate_disconnect`. The supervisor's reconcile path emits the
  expected `OperatorNotification { kind: "subsystem_unavailable",
  data: { subsystem: "gpsd" } }` within 5 s.

### T4 (§12.3 power-state cycle) (RESOLVED)
- New `background_caps_emission_at_five_second_floor` test stores a
  per-device profile at 5 Hz (200 ms), switches the backend to
  `PowerState::Background`, fires 10 TPVs at 100 ms intervals, and
  asserts at most 1 `GnssFixChanged` slips through — the C1
  Background floor enforced end-to-end through the backend, not
  just the lifecycle helper.

### T5 (Acquisition timeout, end-to-end) (RESOLVED)
- New `acquisition_timeout_transitions_acquiring_to_degraded` test
  runs the backend with `acquisition_timeout_s = 1`, discovers a
  device, never feeds a TPV, and waits for the
  `GnssStateChanged { from: "acquiring", to: "degraded", reason:
  "timeout", … }` event.

### T6 (Strict-quality through the backend) (RESOLVED)
- New `strict_quality_rejects_fix_without_eph` test stores a
  profile with `strict_quality = true` + `max_horizontal_error_m =
  Some(50.0)`, feeds a TPV without `horizontal_error_m`, asserts
  no `GnssFixChanged` emerges, then feeds a valid fix to confirm
  the path is otherwise healthy.

### Still open (low priority)
- **T7 (`proto_major > 3` accept-with-warn).** Rejected-too-old is
  tested (`json_client_rejects_protocol_older_than_3`); the
  newer-than-3 accept path is not.
- **T8 (`current_fix` command).** N/A — `GnssCommand::CurrentFix`
  was removed under S4. The trait method remains as a future
  extension point with no consumers.
- **T9 (SKY snapshot read).** `sky_message_updates_sat_snapshot`
  only checks the backend stays alive; it never reads
  `entry.last_satellites` to confirm the snapshot was written.
  Will be exercised once the D-Bus layer's `SatellitesInView`
  property reader exists.
- **T10 (§12.3 hardware-in-loop).** `tests/integration_gpsd.rs` is
  gated behind `--features integration-gpsd` and `#[ignore]`;
  gpsfake-driven HIL tests are not present at all. Acceptable
  given the embedded target.

---

## What works well

- gpsd JSON parsing handles every documented edge case (mode 0/1,
  missing time, mode-3-without-altitude downgrade, eph synthesis from
  epx/epy, missing device field) and is well-covered.
- The two-tier event flow is structurally clean: raw
  `GnssTpvReceived` is what the gpsd client emits, filtered
  `GnssFixChanged` is what the backend re-emits.
- The state machine is split into pure transition functions
  (`tpv_next_state`, `check_timeouts`, `should_emit`) that are easy to
  unit test and read.
- Reconnect supervisor uses bounded exponential backoff with the
  documented 30 s cap and resets attempts on successful reconnect.
- Metrics names and label sets line up with DD-005 §11.2 1:1.
- The 100-TPV TCP roundtrip (`json_client_tcp.rs`) is exactly the
  phase 2 exit criterion the DD specifies.

---

## Recommended next moves (low priority)

1. T7 — exercise the `proto_major > 3` accept-with-warn handshake
   path against a fake VERSION line.
2. T9 — assert SKY-derived `entry.last_satellites` content from the
   D-Bus side once the `SatellitesInView` property reader lands.
3. T10 — wire a gpsfake-driven HIL test in CI when the embedded
   target's CI image gains gpsd. Optional.
