# Wi-Fi Backend Audit — Remaining Findings

Scope: code under `crates/nexus-wifi/` audited against `dd-003-wifi-backend.md` on 2026-04-24, refreshed after the C/K/S/§14.1 passes.

**Closed since the last refresh** (delivered across `nexus-wifi` 0.13.0 / `nexus-interface-monitor` 0.2.0 / `nexus-core` 0.3.0 / `nexus-dbus` 0.5.0 / `nexus-daemon` 0.10.1):

- Original 1–3 (bad-PSK path, profile/store reconciliation, signal-state refresh) and §9.6 reason fidelity (0.9.0).
- C1–C12 — every correctness item closed (Connected.security, AllowRoam, disconnect cooldown, success/duration metric, PowerState-aware signal poll, wake-from-sleep, driver-wedge, directed roam scan, roam outcome + Roaming state, NetworkRequest plumbing + D-Bus surface, daemon `None` for monitor cmds when disabled).
- K1–K9 — every consistency item closed (split SupplicantState::Associated, BssCapabilities IE/RSN parser, RoamTarget::Auto docs, single SignalInfo bitrate, scan metrics + classifier, typed startup/hidden ScanParams, tolerant BSS.Frequency, attach-failure → Disconnected{SupplicantUnavailable}, `wifi-iwd` compile_error).
- S1–S6 — every style item closed (Idle re-entry comment, `WifiError::UnknownInterface`, `types::profile_security` deletion, Option-returning `extract_bssid_ssid`, `BSSAdded`/`BSSRemoved` watcher, daemon `monitor_cmd_tx` lifetime annotation).
- **§14.1 (this pass)** — every C-pass / K-pass / S5 unit-test gap closed:
  - `helper_tests` mod inside `src/backend.rs` (14 tests): K5 `classify_scan` for all four ScanParams shapes; S4 `placeholder_assoc` + `extract_bssid_ssid` Option behaviour; C5 `signal_poll_interval` × `PowerState`; `is_psk_like`; `intrinsic_security_mode`; C1 `resolve_connected_security` (BSS-cache pick / intrinsic fallback / no-info default); `map_disconnect` covering every `DisconnectHint` variant.
  - `tests/backend_tests.rs` integration tests (9 new): C3 cooldown transitions Disconnected→Idle; C3 negative — CredentialsInvalid blocks the cooldown; C6 wake-from-sleep emits `PostSleepRecovery` on probe failure; C7 driver-wedge dwell→DriverWedge (uses the new `WifiConfig::driver_wedge_threshold` to shrink to 50 ms); C8 low-RSSI nexus-mode triggers a directed scan with `allow_roam=true`; C9 nexus-mode roam dispatches to the strongest candidate (verified via the recorded `roam_calls`); C10 NetworkRequest event round-trips and `ProvideCredential` round-trips back to the supplicant; K8 attach failure → `Disconnected{SupplicantUnavailable}` + no initial scan; S5 `BssCacheStale` does not emit `WifiScanComplete`.
- Supporting changes for testability:
  - `MockSupplicantHandle` now records `scan_calls`, `roam_calls`, and `credential_replies`, and exposes a `set_signal_info_fails` knob.
  - `WifiConfig::driver_wedge_threshold` field replaces the const so tests can shrink it from 30 s to ~50 ms.
  - **Backend bug found by the C9 test and fixed**: `request_scan` no longer overwrites `WifiState::Connected` to `Scanning` — it only folds to Scanning from `Idle`/`Disconnected`/`Gone`. Without this, the heartbeat-driven directed roam-eval scan (C8) flipped the state, which made the subsequent `on_scan_complete`'s `currently_connected` check fail and routed the results through `select_network` instead of `evaluate_roam`. (DD-003 §3.1's lifecycle has no `Connected → Scanning` edge.)

All audit items from the original review are closed. What follows is the remaining test-coverage gap set, which is the only outstanding work.

## Test coverage gaps

- **§14.2**: `tests/hwsim_integration.rs::scan_finds_hostapd_ap` is the only `#[ignore]`d smoke; it spawns hostapd and wpa_supplicant but does not run a Nexus `WifiBackend` against them. No end-to-end driver yet.
- **§14.2 connect matrix** (Open / WPA2-Personal / WPA3-SAE / WPA2-Enterprise / WPA3-Enterprise) on the hwsim fixture.
- **§14.2 roam scenario** between two hostapd APs on different channels sharing an SSID.
- **§14.3 hardware lab** matrix (Intel AX200 / Qualcomm QCA6174 / Broadcom BCM4345 / TI WL1837): Phase 11 explicitly defers to the operator on each landing commit.
- **§14.4 regulatory domain**: no tests at all. Minimally: `iw reg set US` then assert scheduled scans for 5 GHz DFS channels go out as `Type: "passive"`.

## Priority for the next pass

| # | Item | Severity | Effort |
|---|------|----------|--------|
| `backend_fixture::spawn` helper that drives a real `WifiBackend` against an already-running wpa_supplicant in the hwsim harness | — | tests | M |
| WPA3 / Enterprise hostapd matrix in the hwsim harness | — | tests | M |
| Two-AP same-SSID roam scenario in the hwsim harness | — | tests | M |
| §14.4 regulatory-domain DFS test | — | tests | S |

## Not architectural concerns

The event-loop / scheduler / selector / retry / rfkill / supplicant-trait split matches the DD cleanly. After the C+K+S+§14.1 passes: the state-watcher's `PropertiesChanged + 2 s reconcile tick` discipline (§9.5) is faithful; `build_wpa_network_args` (§9.4) is well-tested; the lifecycle reaches the §3.2 cooldown / §13.3 wake / §12.4 wedge / §7.3 directed-scan paths; scan / connect / signal / roam / wedge metrics all have producers; BSS capabilities (HT/VHT/HE/EHT/FT/PMF/WPS) are decoded from IEs + RSN; ifname-lookup errors carry the ifname; BSSAdded/BSSRemoved keep the cache fresh between scans; mock-driven coverage exercises every C/K/S behavioural path. The only remaining work is privileged hwsim end-to-end testing on a CI runner that can load `mac80211_hwsim` and run `hostapd` + `wpa_supplicant` against a live Nexus backend.
