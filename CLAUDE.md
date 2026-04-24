# Nexus — Claude Code Instructions

Nexus is a platform connectivity manager for embedded Linux devices. It manages Ethernet, Wi-Fi, Bluetooth, and GNSS through a single daemon with a unified D-Bus API, delegating IP configuration to systemd-networkd.

**Language:** Rust (edition 2024)
**Target:** Embedded Linux (wall-powered and battery-powered)

## Before starting any task

Read the architecture doc first, then the detailed design for the component you're working on:

1. `docs/nexus-architecture.md` — high-level design, component responsibilities, ADRs
2. The relevant detailed design doc:
   - Interface Monitor → `docs/dd-001-interface-discovery.md`
   - Ethernet Backend → `docs/dd-002-ethernet-backend.md`
   - Wi-Fi Backend → `docs/dd-003-wifi-backend.md`
   - GNSS Backend → `docs/dd-005-gnss-backend.md`
   - D-Bus API → `docs/dd-006-dbus-api.md`
   - Profile Store → `docs/dd-007-profile-store.md`

Every DD has a §1.1 "Repo Layout" section showing where the code lives, and an "Implementation Phases" section at the end describing build order. Use these. Do not invent your own structure.

## Writing or revising design docs

When asked to draft a new detailed design doc, revise an existing one, or review one, first read `docs/DESIGN-DOCS.md`. It captures conventions for DD structure, the discipline needed to get pseudocode realistic, and the three mechanical passes (name sweep, end-to-end trace, referenced-symbol check) that catch the bulk of drift and oversight issues before a draft goes to review.

## Conventions

### Language and tooling
- Rust **edition 2024**
- `rustfmt` default config; `clippy` with `-D warnings` in CI
- MSRV: latest stable (we move with the ecosystem; this is embedded Linux, not a library for distribution)
- Panics are for programmer errors only. Runtime failures return `Result`.

### Versioning
Every behavior-changing commit bumps the affected crate's `version` in that crate's `Cargo.toml`, in the same commit. Follow [SemVer](https://semver.org/): `x.y.z` where `x` is breaking, `y` adds features, `z` fixes bugs.
- **PATCH** (`0.1.0` → `0.1.1`): bug fixes only. No new public API, no changed wire format, no changed default behaviour. Commit prefix: `fix:`.
- **MINOR** (`0.1.1` → `0.2.0`): backwards-compatible additions — new public items, new trait methods with default impls, new config fields, new D-Bus members, new CLI subcommands. Commit prefix: `feat:`.
- **MAJOR** (`0.2.0` → `1.0.0`): removed or renamed public API, changed D-Bus wire format, altered default behaviour that downstream callers rely on. Commit prefix: `feat!:` or add a `BREAKING CHANGE:` footer.
- Pre-1.0 (where we are today), SemVer treats `0.y` itself as the breaking boundary — `0.1.x` → `0.2.0` is a breaking bump. Still apply PATCH vs MINOR vs MAJOR within that constraint so the intent stays legible.
- When a commit touches multiple crates, bump each affected crate independently. Don't touch the version of a crate whose code didn't change in the same commit.
- Doc-only, comment-only, test-only, and internal refactors with no externally-visible effect do not require a bump.

### Async
- `tokio` is the runtime. No mixing with other executors.
- Prefer `tokio::select!` for multiplexing over spawning extra tasks when shared state is involved (see DD-001 §8 for the Interface Monitor's single-task rationale).
- Broadcast channels (`tokio::sync::broadcast`) for the event bus. Slow consumers must not block producers.

### Error handling
- Library crates (`nexus-monitor`, `nexus-ethernet`, `nexus-wifi`, `nexus-core`, `nexus-auth-eap`): use `thiserror` for typed errors. Keep error variants actionable.
- Binary crate (`nexusd`): use `anyhow` at the outer layer for context chaining.
- Never swallow errors silently. At minimum `tracing::warn!` with context.

### D-Bus
- `zbus` for all D-Bus interaction.
- Use typed proxies (`#[proxy]` macro) rather than raw method calls.
- Subscribe to `NameOwnerChanged` for every external daemon (wpa_supplicant, iwd, BlueZ, gpsd). Crashes happen; handle them.

### Netlink
- Hand-rolled parser for correctness and forward compatibility. Do not use `netlink-packet-*` crates — they add dependencies and don't handle the nl80211 quirks we care about.
- See DD-001 §9.3 and Appendix A for the parsing rules.
- Socket naming matches DD-001 §4 table: `rtnl_fd`, `nl80211_mcast_fd`, `nl80211_rr_fd`, `udev_fd`, `ethtool_fd`.

### Shared types
- `NexusEvent`, `InterfaceInfo`, `InterfaceKind`, `OperState` live in `nexus-core`. Never redefine these elsewhere.
- `Dot1xEapConfig` lives in `nexus-auth-eap`. Used by both the wired 802.1X path (DD-002) and WPA2/WPA3-Enterprise (DD-003).

### Unsafe code
- Avoid it. If you must use `unsafe`, add a `// SAFETY:` comment on every unsafe block explaining the invariant being upheld.
- Common valid cases: FFI for ioctls (ethtool), raw socket syscalls, pointer casts for netlink struct layouts.

### Metrics
- Prometheus-style metric names, following the conventions in DD-001 §9.5.
- Declare all metrics in the component's `metrics.rs` module.

### Testing
- Unit tests live in `#[cfg(test)] mod tests` at the bottom of each module.
- Integration tests live in `tests/` at the crate root.
- Parser tests use hand-crafted byte arrays with comments describing the expected structure (see DD-001 §11.1 for the pattern).
- Integration tests that require the kernel (mac80211_hwsim, veth) are gated behind a `#[cfg(feature = "hw-test")]` flag or skip cleanly when the kernel feature is unavailable.

### Logging
- `tracing` for structured logging. Spans for long-lived operations (per-interface lifecycle, per-scan).
- Log levels: `error` for things that require operator attention; `warn` for recoverable abnormalities; `info` for lifecycle transitions; `debug` for message-level detail.

## When implementing a phase

1. Read the relevant DD's Implementation Phases section and the sections it references.
2. Start with the data types and traits. Unit-test them in isolation before wiring I/O.
3. Build the mock implementation of any trait before the real one — it doubles as a test harness.
4. Wire up metrics as you go, not as an afterthought.
5. Stop at the phase boundary. Do not start the next phase without explicit instruction.

## When auditing existing code

When asked to audit an implementation against a DD, produce a structured list:

- Section of the DD being checked.
- What the DD specifies.
- What the code does.
- Deviation (if any), with severity: `correctness` / `consistency` / `style`.

Do not rewrite the code during an audit. Report findings; let the operator decide what to act on.

## What not to do

- Do not add IP-layer management (DHCP, static addresses, routes, DNS). That is systemd-networkd's job. The delegation boundary is kernel carrier state.
- Do not add support for daemons not listed in the DDs (NetworkManager, ConnMan, dhcpcd, etc.).
- Do not add built-in supplicants. Wi-Fi and wired 802.1X go through wpa_supplicant or iwd/ead over D-Bus.
- Do not paper over upstream bugs silently. If wpa_supplicant or BlueZ has a known quirk that requires a workaround, the workaround gets a `// WORKAROUND:` comment linking to the upstream issue.
- Do not make architectural decisions on your own if they conflict with an ADR. Raise the question back to the operator.
