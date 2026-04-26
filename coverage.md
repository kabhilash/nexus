# Coverage Report

**Generated:** 2026-04-25
**Tests:** 813 passing, 0 failing, 11.14s wall
**Total line coverage:** 74.45%
**Total function coverage:** 72.84%

**Reproduce:**

```bash
scripts/build-and-test.sh --test-coverage      # full per-file table
scripts/build-and-test.sh --test-with-coverage # rolled-up summary only
```

Both run inside the `nexus-v2-dev` devcontainer via `cargo llvm-cov`. The
report runs natively on **x86_64** — aarch64-only code paths and any code
behind `#[cfg(target_arch = "aarch64")]` are invisible to this report.

---

## Per-crate rollup

Approximate per-crate line coverage, computed from the per-file table.
Files in the "intentional gaps" categories below are **excluded** from the
"effective" column to give a more honest signal of test discipline on the
code we actually expect to cover.

| Crate | Files | Reported lines % | Effective lines % | Notes |
|---|---|---|---|---|
| `nexus-core` | 4 | ~92% | ~92% | Pure shared types |
| `nexus-profile-store` | 17 | ~91% | ~94% | Strong; types and crypto well-pinned |
| `nexus-dbus` | 24 | ~80% | ~85% | Bulk of remaining gap is `service.rs` orchestration |
| `nexus-daemon` | 7 | ~85% | ~88% | Top-level wiring tested via daemon_tests.rs |
| `nexus-interface-monitor` | 12 | ~84% | ~88% | Netlink parsers near 95%; live monitor loop low |
| `nexus-ethernet` | 9 | ~70% | ~88% | Real wpa_supplicant path uncovered, mock at 88% |
| `nexus-wifi` | 14 | ~75% | ~83% | Same shape: mock supplicant strong, real path low |
| `nexus-bluetooth` | 14 | ~67% | ~83% | Real BlueZ client uncovered, mock at 77% |
| `nexus-gnss` | 12 | ~78% | ~88% | gpsd JSON parser at 99%, real TCP client lower |
| `nexus-client` | 47 | ~58% | ~78% | Auto-generated proxies and interactive paths drag the headline |
| `nexus-auth-eap` | 1 | 0% | n/a | Placeholder crate (see below) |

---

## Files at full coverage (≥ 95%)

These files have line coverage at or above 95% and need no further work:

```
nexus-bluetooth: adapter.rs, device.rs, lib.rs, metrics.rs, pairing.rs
nexus-client:    completions.rs, completion/dynamic.rs, completion/static.rs,
                 output/pretty.rs, output/terse.rs, watch/filter.rs, watch/synthesize.rs
nexus-core:      address.rs, event.rs
nexus-daemon:    bus.rs, config.rs, supervision.rs
nexus-dbus:      interfaces/bluetooth.rs, interfaces/ethernet.rs,
                 paths.rs, profiles/mod.rs, properties.rs, rate_limit.rs,
                 scan_results.rs, state.rs
nexus-ethernet:  profile.rs, retry.rs
nexus-gnss:      fix.rs, gpsd/parse.rs, lib.rs, lifecycle.rs, metrics.rs
nexus-interface-monitor: classify.rs, lib.rs, metrics.rs, netlink/nl80211.rs
nexus-profile-store: crypto/cipher.rs (99%), error.rs, keys/mod.rs, metrics.rs,
                     quarantine.rs (97%), secret.rs, types/ethernet.rs (96%),
                     types/gnss.rs, types/mod.rs (97%), types/wifi.rs (96%)
nexus-wifi:      lifecycle.rs (98%), metrics.rs, profile.rs, retry.rs,
                 roam.rs, scan.rs, select.rs (96%)
```

---

## Intentional gaps — will not be covered

The headline percentage looks lower than the effective coverage because
several categories of file appear in the report at or near 0% **by design**.
Each category below is excluded from the effective column above.

### 1. Demo binaries (0%)

Files: `nexus-bluetooth/src/bin/demo.rs`, `nexus-dbus/src/bin/demo.rs`,
`nexus-gnss/src/bin/demo.rs`, `nexus-interface-monitor/src/bin/demo.rs`.

**Why uncovered:** these are operator-driven manual integration tools that
spin up a real backend against real hardware (or a real session bus). They
are not test targets and have no place in `cargo test`. Their existence is
a development convenience, not production code.

**Action:** none. These files should remain at 0% in the report.

### 2. Auto-generated D-Bus client proxies (0%)

Files (all in `nexus-client/src/proxy/`): `bluetooth.rs`, `bluetooth_device.rs`,
`ethernet.rs`, `gnss.rs`, `interface.rs`, `manager.rs`, `mod.rs`, `profile.rs`,
`scan_result.rs`, `wifi.rs`. Plus `nexus-bluetooth/src/bluez/proxies.rs` and
`nexus-dbus/src/interfaces/bluetooth_device.rs` (in part).

**Why uncovered:** these files are emitted by zbus's `#[proxy]` macro. The
visible code is method-signature plumbing — there is no business logic to
cover. The proxies are exercised end-to-end by the CLI against a real
daemon, but those tests are out-of-process and `cargo llvm-cov` cannot
attribute the calls back to the client crate's compiled binary.

**Action:** none. Don't write tests for trait stubs the macro emits — they
add maintenance cost and pin nothing the macro doesn't already enforce at
compile time.

### 3. Re-export shims and placeholder crates

| File | % | Why |
|---|---|---|
| `nexus-auth-eap/src/lib.rs` | 0.00% | Placeholder crate; `Dot1xEapConfig` currently lives in `nexus-profile-store` per DD-007 §5.2 note. Will be populated when the real EAP backend lands; until then it has no code to cover. |
| `nexus-bluetooth/src/errors.rs` | 0.00% | Single `#[derive(thiserror::Error)]` enum with no constructors invoked from a path that runs in `cargo test`. |
| `nexus-ethernet/src/lib.rs` | 0.00% | Pure `pub use` re-exports — `cargo llvm-cov` doesn't credit re-export modules. |
| `nexus-gnss/src/errors.rs` | 0.00% | Same as `nexus-bluetooth/src/errors.rs`. |
| `nexus-profile-store/src/trait_def.rs` | 0.00% | Trait definition only; impls live in `fs_store.rs` (86%) and a `DummyStore` in `tests/roundtrip.rs`. |
| `nexus-wifi/src/supplicant/mod.rs` | 0.00% | `pub mod` declarations only. |
| `nexus-wifi/src/power.rs` | 0.00% | Power-state hook stubs awaiting the daemon-level coordinator (DD-003 §13). |
| `nexus-client/src/path_resolve.rs` | 0.00% | Resolves CLI path arguments against the running daemon's known interfaces — only invoked by the live `dispatch.rs` path against a real bus. |
| `nexus-client/src/commands/watch.rs` | 0.00% | Streams `fi.nexus.Manager.NotificationEvent`s for the `nexusctl watch` subcommand; long-running async loop that needs a live daemon to make sense. |

**Action:** none until the underlying crate or feature is non-placeholder.

### 4. Real-daemon I/O paths — needs `hw-test` gating

Per CLAUDE.md ("Testing"), tests that require a kernel feature, an external
daemon, or a real radio go behind `#[cfg(feature = "hw-test")]`. None of
those tests run in the default coverage build. The mock half of every
backend is at 80–95%; the real half is the gap below.

| File | Lines % | Real dep | Mock counterpart | Gap reason |
|---|---|---|---|---|
| `nexus-ethernet/src/auth/wpa_supplicant.rs` | 16.98% | `wpa_supplicant` over D-Bus | `auth/mock.rs` (88%) | Real 802.1X path needs a live supplicant attached to a managed wired link. |
| `nexus-bluetooth/src/bluez/zbus_client.rs` | 0.00% | BlueZ over D-Bus | `bluez/mock.rs` (77%) | Real BlueZ object-manager subscription requires a live `bluetoothd`. |
| `nexus-wifi/src/rfkill.rs` | 33.17% | rfkill via netlink | none (no mock — kernel-only) | Needs the kernel rfkill subsystem; no userspace shim that round-trips meaningfully. |
| `nexus-wifi/src/supplicant/wpa_supplicant.rs` | 60.96% | `wpa_supplicant` over D-Bus | `supplicant/mock.rs` (94%) | Partial: integration tests against a stubbed bus exercise some paths; full path needs real supplicant. |
| `nexus-gnss/src/gpsd/json_client.rs` | 55.00% | `gpsd` TCP JSON | `gpsd/mock.rs` (89%) | TCP framing + reconnect logic mocked; live wire format covered by `tests/json_client_tcp.rs` against a fixture server. |

**Action:** these files should be lifted by `hw-test`-gated integration
tests on a CI runner with `mac80211_hwsim`, a session BlueZ + dbus-daemon,
and a `gpsfake` instance. Pursuing them with mocks duplicates work already
done in the mock counterpart and adds no real-world signal.

### 5. Interactive client paths — needs real TTY / polkit

| File | Lines % | Why |
|---|---|---|
| `nexus-client/src/dispatch.rs` | 5.60% | Top-level CLI dispatch — instantiates a real `zbus::Connection`, spawns the polkit prompter, drives subcommands. Covered by running the binary, not by unit tests. |
| `nexus-client/src/interactive/terminal_prompt.rs` | 6.09% | Reads passphrases from `/dev/tty`. No portable way to fake a tty in a unit test. |
| `nexus-client/src/interactive/polkit.rs` | 11.54% | Calls `org.freedesktop.PolicyKit1.Authority.CheckAuthorization` — needs `polkitd` running. |
| `nexus-client/src/interactive/cancellation.rs` | 34.78% | `Ctrl-C` signal handler glue — partial coverage from in-process tests; real signal delivery is unit-test-hostile. |

**Action:** these are best exercised by an end-to-end test that runs
`nexusctl` against a real daemon under expect/pty harness. Not a normal
unit-test target.

### 6. Long-running orchestrators — integration-test territory

| File | Lines % | Lines | Why hard to unit-test |
|---|---|---|---|
| `nexus-interface-monitor/src/monitor.rs` | 36.47% | 913 | Single-task `tokio::select!` over 5 file descriptors (rtnl, nl80211, udev, ethtool, recovery). The branch density is high but each branch needs a coordinated socket-pair fixture. The `tests/lifecycle_tests.rs` harness covers state-transition sub-paths; the rest needs `mac80211_hwsim` + a network namespace. |
| `nexus-dbus/src/service.rs` | 67.79% | 863 | D-Bus service event loop. Happy paths are covered by `tests/dbus_tests.rs` / `tests/mutating_tests.rs` (each spawns a session bus). The remaining 32% is property-changed coalescing, race recovery on `NameOwnerChanged`, and registry-resync paths — best exercised with more spawn-and-script integration tests, not units. |
| `nexus-wifi/src/backend.rs` | 74.34% | 1469 | The wpa_supplicant orchestrator. Already covered by 19 integration tests in `tests/backend_tests.rs` against the mock supplicant. The gap is real-daemon paths — see (4). |
| `nexus-bluetooth/src/agent.rs` | 36.94% | — | BlueZ pairing agent. Methods are async and depend on a backend `mpsc::Sender<BtCommand>` plus per-call `oneshot` registration. Testable with a fake backend task, but each method needs ~30 lines of harness for one assertion — diminishing returns. |

**Action:** lift via integration tests when there's a specific behaviour
to pin (e.g. a regression). Don't treat the residual percentage as a goal
in itself.

---

## Files where more coverage would be valuable

These are the remaining "real" gaps — code that is unit-testable but
hasn't been pinned. Listed in descending order of expected payoff
(absolute uncovered lines × testability):

| File | Lines % | Lines | Approx. payoff | Suggested approach |
|---|---|---|---|---|
| `nexus-client/src/cli.rs` | 39.68% | ~609 | Modest | clap-derive struct definitions; most "uncovered" lines are doc-strings and `#[arg]` attributes. Existing `tests/cli_parse.rs` (31 tests) already covers the parsing logic; adding tests for argument validation edge cases (mutually exclusive groups, default values, value parsers) would lift this incrementally. |
| `nexus-dbus/src/profiles/common.rs` | 67.43% | ~175 | Medium | The async D-Bus methods (`Update`, `Delete`) need a real `Services`. Best extended via `tests/mutating_tests.rs` with cases for: auth-denied on Update, NotFound on Update for a stale path, Ethernet-variant Update, Ethernet-variant Delete (existing tests focus on Wi-Fi). |
| `nexus-client/src/commands/gnss.rs` | 23.40% | ~140 | Medium | CLI handler for `nexusctl gnss …`. Currently only the static-completion path is exercised. Adding tests that mock the proxy responses would lift the formatting/error-translation logic. |
| `nexus-interface-monitor/src/udev.rs` | 59.17% | ~218 | Medium | udev event parser with a moderate amount of pure-string logic; some ATTRS branches and recovery paths uncovered. Testable with hand-crafted udev event byte streams (similar to the netlink parser tests in DD-001 §11.1). |
| `nexus-client/src/output/records.rs` | 46.37% | — | Small | Record-formatting glue between proxy responses and human/json/terse renderers. Pure data, table-driven tests would close this cheaply. |
| `nexus-client/src/commands/profile_mutating.rs` | 65.15% | — | Small | CLI handlers for `profile add/remove/update`; only the success paths exercised. Error-translation branches uncovered. |
| `nexus-wifi/src/types.rs` | 44.44% | 9 | Trivial | 5 uncovered lines in a 9-line file; not worth a round trip but easy to fold into a wider `nexus-wifi` pass. |

---

## What changed in the recent test push

Coverage moved from **72.97% → 74.45%** with **+55 targeted tests** across
five files. The full sequence of commits:

```
98e9208 test(nexus-profile-store,nexus-dbus): add unit tests for low-coverage type and state modules
02e3320 test(nexus-profile-store): cover non-Wifi quarantine paths and master_key_degraded notifications
```

Per-file lift on the targeted modules:

| File | Before | After |
|---|---|---|
| `nexus-profile-store/src/types/wifi.rs` | 50.92% | 96.24% |
| `nexus-profile-store/src/types/gnss.rs` | 26.67% | 100.00% |
| `nexus-profile-store/src/quarantine.rs` | 84.05% | 96.93% |
| `nexus-dbus/src/state.rs` | 45.87% | 100.00% |
| `nexus-dbus/src/interfaces/wifi.rs` | 58.60% | 74.11% |

The fifth file is capped because the bulk of its remaining lines are
async D-Bus methods that depend on a real `Services` — those are the
domain of `tests/mutating_tests.rs`, which already covers the happy
paths.

---

## Caveats

1. **Architecture.** Coverage runs natively on x86_64 inside the
   devcontainer. Any code gated by `#[cfg(target_arch = "aarch64")]` or
   tested only in cross-compiled builds is invisible to this report. The
   default `--target aarch64-unknown-linux-gnu` build that
   `scripts/build-and-test.sh` performs *before* the coverage step is
   wasted work for the report — the subsequent `cargo llvm-cov` rebuilds
   for the host.
2. **Function vs. line coverage.** The function-coverage column counts
   declared functions including unused-but-public APIs that are part of
   the crate's surface. A library crate exposing a feature for downstream
   consumers may legitimately have public functions that no in-tree test
   exercises.
3. **Coverage is not correctness.** A high line-coverage number on a file
   does not imply behavioural correctness. The tests added in this push
   were chosen to pin **observable invariants** (security AAD prefixes,
   serde aliases for legacy on-disk profile schemas, D-Bus wire-format
   labels), not to chase percentages. Treat the report as a map of where
   the test discipline is concentrated, not as a quality score.
