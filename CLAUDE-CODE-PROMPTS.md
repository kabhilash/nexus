# Claude Code Prompt Series — Nexus Implementation

This document contains a series of prompts for building Nexus incrementally with Claude Code. Prompts are ordered by dependency; later prompts assume earlier ones are complete.

## How to use this series

**One prompt per Claude Code session.** Paste the whole prompt block into Claude Code at the start of a session. Don't try to chain prompts in a single session — context churns and quality drops after ~300 lines of new code.

**Read the referenced DD section before pasting the prompt.** The prompts are deliberately terse about design specifics because the DDs already cover them. If you (the human) don't have the DD section in front of you, you won't catch it when Claude Code's output drifts from the spec.

**Run the exit criterion before moving on.** Every prompt ends with a specific test or command that proves completion. Don't move to the next prompt until this passes.

**CLAUDE.md is checked in at the repo root.** Every Claude Code session reads it first. It holds the project-wide conventions (Rust edition, lint settings, commit style, etc.) that aren't in any DD.

---

## Legend

| Marker | Meaning |
|---|---|
| 🟢 Unblocked | Can start immediately or after previous unblocked work |
| 🟡 Depends on | Specific prompt must be done first |
| ⏱️ Size | Rough estimate of work scope |

---

## Phase 0 — Workspace and core types

### Prompt 0.1 — Cargo workspace skeleton

🟢 Unblocked · ⏱️ Small (~30 min)

```
Set up a Cargo workspace for Nexus, a platform connectivity manager for
embedded Linux (see /docs/nexus-architecture.md for full context).

Create the top-level Cargo.toml as a workspace manifest with these
member crates, each with a minimal src/lib.rs containing `pub fn
placeholder() {}`:

- nexus-core
- nexus-interface-monitor
- nexus-ethernet
- nexus-wifi
- nexus-bluetooth
- nexus-gnss
- nexus-dbus
- nexus-profile-store
- nexus-auth-eap
- nexus-daemon (binary crate, not library)

Each crate's Cargo.toml should specify:
- edition = "2024"
- rust-version = "1.86"   # or whatever the current stable is
- license = "Apache-2.0"

The workspace Cargo.toml should pin shared dependencies under
[workspace.dependencies] so member crates can use them via
`foo = { workspace = true }`:

- tokio = { version = "1", features = ["full"] }
- tracing = "0.1"
- tracing-subscriber = "0.3"
- thiserror = "2"
- anyhow = "1"
- async-trait = "0.1"
- serde = { version = "1", features = ["derive"] }
- serde_json = "1"
- toml = "0.8"
- ulid = { version = "1", features = ["serde"] }
- zbus = { version = "5", default-features = false, features = ["tokio"] }

Add a workspace-level rustfmt.toml with:
  edition = "2024"
  max_width = 100

Add a workspace-level .gitignore covering /target, /.direnv, *.swp,
and editor dotdirs.

Add CLAUDE.md at the repo root containing project conventions (copy
from the existing /docs/CLAUDE.md in this repo).

Commit each crate addition as a separate atomic commit with a
conventional-commits-style message.

Exit criterion: `cargo build --workspace` succeeds and produces no
warnings. `cargo fmt --all --check` passes.
```

### Prompt 0.2 — nexus-core types

🟡 Depends on 0.1 · ⏱️ Medium (~1-2 hours)

```
Implement the core types crate `nexus-core` that the rest of the
workspace depends on. Full spec is in /docs/nexus-architecture.md §6
(the NexusEvent enum) and /docs/dd-001-interface-discovery.md §6.1
(InterfaceInfo, InterfaceKind, OperState). /docs/dd-003-wifi-backend.md
§4.2 defines MacAddr and Ssid which also live in nexus-core.

Files to create:

  crates/nexus-core/src/
    lib.rs
    event.rs        <- NexusEvent enum + type aliases
    interface.rs    <- InterfaceInfo, InterfaceKind, OperState
    address.rs      <- MacAddr, BluetoothAddrExt trait
    ssid.rs         <- Ssid newtype
    notification.rs <- NotificationData, NotificationValue
    metadata.rs     <- ProfileMetadata

Every enum variant and struct field in the DDs must be present. Do NOT
invent fields or types not specified in the docs.

NexusEvent must have `#[derive(Debug, Clone)]` and a broadcast-channel-
compatible shape. Note that nexus-architecture §6 shows the canonical
variant list; do not omit any.

For any type that references BtDeviceInfo, PairingJobId,
PairingPromptKind, PairingPromptData, BtFailureReason, WifiState,
AuthState, GnssFix, SatInfo, BssInfo, ProfileKind, or
BtAdapterChanged's discovering flag — the types themselves live
outside nexus-core (in nexus-bluetooth, nexus-wifi, etc.) but
nexus-core re-exports them only if they appear in NexusEvent
signatures. Use `pub use` for re-exports where the types must be
visible to NexusEvent's Debug/Clone impls.

Unit tests (#[cfg(test)]):
- MacAddr roundtrips: from_bluez("AA:BB:CC:DD:EE:FF") → to_bluez(),
  plus to_object_path_component() returns "dev_AA_BB_CC_DD_EE_FF"
- MacAddr parse errors on: wrong length, invalid hex, missing colons,
  extra characters
- Ssid construction with valid 1-32 byte inputs and rejection of
  empty and >32-byte inputs
- NotificationData insertion and iteration

Do NOT write any networking or I/O code in this crate. It is pure
types plus trait definitions.

Exit criterion: `cargo test -p nexus-core` passes. `cargo doc -p
nexus-core --no-deps` produces no warnings. Other member crates can
`use nexus_core::{NexusEvent, InterfaceInfo, MacAddr};` without a
compile error (verify by adding a one-line import to
nexus-interface-monitor/src/lib.rs).
```

---

## Phase 1 — Interface Monitor foundation

### Prompt 1.1 — DD-001 Phases 1-3 (parsers, socket wrappers, genl resolution)

🟡 Depends on 0.2 · ⏱️ Large (~3-4 hours)

```
Implement the netlink foundation for the Interface Monitor per
/docs/dd-001-interface-discovery.md §§5.1–5.3 and phases 1-3 in §11.

This prompt covers:
- Hand-rolled netlink message parsers (no pnetlink, no rtnetlink crate)
- Socket wrappers for NETLINK_ROUTE and NETLINK_GENERIC
- Generic Netlink family resolution (CTRL_CMD_GETFAMILY)

Files to create/modify in crates/nexus-interface-monitor/src/:

  netlink/mod.rs
  netlink/parser.rs        <- NetlinkMessageHeader, TLV parser
  netlink/socket.rs        <- NetlinkSocket wrapper over tokio UdpSocket
  netlink/rtnl.rs          <- RTM_NEWLINK, RTM_DELLINK, IFA_* parsing
  netlink/genl.rs          <- Generic Netlink control family resolver
  netlink/nl80211.rs       <- NL80211 command IDs and attribute enums
                              (genl family lookup only; full parsing
                              comes in a later prompt)

Follow DD-001 §5.1 for the parser structure — IE-style TLV with
type + length prefix, aligned to 4 bytes. The parser must handle:
- NLMSG_DONE, NLMSG_ERROR, NLMSG_NOOP
- Truncated messages (return error, don't panic)
- Unknown attribute types (skip, don't error)

Rust patterns to follow:
- All parsers take `&[u8]` and return `Result<(Parsed, &[u8]), ParseError>`
  so the caller can advance through the message stream
- No `unsafe` blocks except where absolutely required for socket ioctls
- All errors via thiserror

Unit tests:
- Parse a captured RTM_NEWLINK message (hex bytes) and verify every
  extracted field against a known good decoding
- Round-trip: build an RTM_GETLINK request, verify the byte layout
- Corrupt input: truncated message, invalid TLV length, unknown family
  — all must error rather than panic

Exit criterion: `cargo test -p nexus-interface-monitor` passes. The
tests must include at least one real captured message (can be
hand-constructed from the rtnetlink(7) man page examples).
```

### Prompt 1.2 — DD-001 Phases 4-6 (cold-boot, event emission, main loop)

🟡 Depends on 1.1 · ⏱️ Large (~3-4 hours)

```
Implement cold-boot enumeration, event emission, and the main loop
for the Interface Monitor per /docs/dd-001-interface-discovery.md
§§5.4–5.5 and phases 4-6 in §11.

This prompt builds on the netlink foundation from the previous
prompt. It adds:

- udev enumeration for Bluetooth (hci*) and GNSS (tty*) devices
- Cold-boot sequence: genl resolve → netlink dump → udev scan
- Emission of NexusEvent::InterfaceDiscovered / InterfaceRemoved /
  LinkStateChanged via tokio broadcast
- Main monitor loop with select! over netlink socket, udev monitor,
  and shutdown signal

Files:
  crates/nexus-interface-monitor/src/
    udev.rs         <- udev enumerator + monitor wrapper (use the
                       `udev` crate, not hand-rolled)
    enumerate.rs    <- cold-boot sequence from §5.4
    registry.rs     <- in-memory InterfaceInfo registry keyed by
                       ifindex (including synthesized ifindex for
                       BT/GNSS per §5.4)
    monitor.rs      <- the main loop; owns all sockets and the
                       registry; emits NexusEvents
    lib.rs          <- re-exports, spawn_interface_monitor()

The spawn function signature:

    pub async fn spawn_interface_monitor(
        event_tx: broadcast::Sender<NexusEvent>,
        shutdown: CancellationToken,
    ) -> Result<JoinHandle<Result<()>>>;

Important constraints from DD-001:
- BT/GNSS ifindex is synthesized (high bit set, see §5.4 "Note on
  identifiers for non-network subsystems")
- Hotplug events during cold-boot enumeration must be queued, not
  lost — drain any pending events from the netlink monitor socket
  AFTER the dump completes (§5.5)
- Wi-Fi interfaces need wiphy capability resolution via NL80211 —
  defer this to a later prompt; for now set `capabilities` to an
  empty Arc<PhyCapabilities>

Unit tests for parsers (covered in prev prompt) plus:
- In-process test: run spawn_interface_monitor against a mock
  netlink socket that replays a recorded RTM_NEWLINK dump. Verify
  the broadcast channel receives an InterfaceDiscovered per link.
- udev enumeration: use a test harness that sets UDEV_TESTDATA to
  a fixture directory. Alternatively, gate real-udev tests behind
  #[cfg(feature = "integration-udev")] and skip them in default
  CI.

Exit criterion: `cargo test -p nexus-interface-monitor` passes.
Add a bin/demo.rs that spawns the monitor, subscribes to events,
and prints them. Run `cargo run -p nexus-interface-monitor --bin
demo` on a real Linux machine — verify it produces InterfaceDiscovered
events for every kernel network interface. Kill it cleanly with Ctrl-C.
```

### Prompt 1.3 — DD-001 Phases 7-10 (classification, errors, metrics, tests)

🟡 Depends on 1.2 · ⏱️ Medium (~2-3 hours)

```
Complete the Interface Monitor per /docs/dd-001-interface-discovery.md
phases 7-10.

Adds:
- Full NL80211 wiphy capability resolution (previously deferred)
- Classification state machine (§7)
- Error handling & recovery with reconnection backoff (§8)
- Prometheus metrics per §9.5
- Integration tests against a real kernel netlink on Linux

Files:
  crates/nexus-interface-monitor/src/
    nl80211.rs      <- extend to parse wiphy dump responses and
                       capability attributes per §5.3
    classify.rs     <- classify InterfaceKind from kernel info
    recover.rs      <- netlink socket reconnection with backoff
    metrics.rs      <- all metrics from §9.5

Do NOT invent new metrics. The table in §9.5 is authoritative:

    nexus_interface_events_total{kind, event}
    nexus_interface_discovery_duration_seconds
    nexus_interface_count{kind}
    nexus_interface_errors_total{source}

Use the `metrics` crate (0.24 or newer) for recording; the actual
/metrics HTTP exposure is added by nexus-dbus/nexus-daemon in a
later prompt — for now metrics just need to register and accept
recording calls.

Integration tests (gated behind `#[cfg(feature =
"integration-linux")]`):
- Add and remove a dummy interface via `ip link add` / `ip link
  del`, verify the monitor emits InterfaceDiscovered /
  InterfaceRemoved
- Bring a dummy up/down, verify LinkStateChanged emissions
- These tests require CAP_NET_ADMIN; mark them #[ignore] unless a
  separate test runner handles them

Exit criterion: `cargo test -p nexus-interface-monitor --all-features`
passes (excluding #[ignore] integration tests). `cargo run -p
nexus-interface-monitor --bin demo` on a Linux machine shows wiphy
capabilities populated for Wi-Fi interfaces. `cargo doc -p
nexus-interface-monitor --no-deps` produces no warnings.
```

---

## Phase 2 — Profile Store foundation

Profile Store is concurrent-with DD-002/003 implementation since DD-002 and DD-003 both depend on the `ProfileStore` trait existing. Run these prompts in parallel with 1.x if you have the capacity.

### Prompt 2.1 — DD-007 Phases 1-2 (types and filesystem store)

🟡 Depends on 0.2 · ⏱️ Medium (~2 hours)

```
Implement the Profile Store types and plaintext filesystem persistence
per /docs/dd-007-profile-store.md phases 1-2.

This prompt covers:
- The ProfileStore trait (§5, including GNSS and Bluetooth methods)
- In-memory and on-disk profile types (§5.2 onward)
- A filesystem-backed implementation (ProfileFileStore) that reads
  and writes TOML files with atomic-write guarantees (§6.2)
- NO encryption yet — that's the next prompt

Files:
  crates/nexus-profile-store/src/
    lib.rs
    trait_def.rs        <- ProfileStore trait, ProfileRef, ProfileKind,
                           RotateReport
    types/
      mod.rs
      ethernet.rs       <- EthernetProfile
      wifi.rs           <- WifiProfile, WifiProfileOnDisk (dual-struct
                           pattern — see §5.3)
      gnss.rs           <- GnssDeviceProfile (forward-declare; actual
                           fields land in nexus-gnss but the Serialize
                           impl lives here)
      bluetooth.rs      <- BluetoothProfile (same pattern)
    secret.rs           <- SecretString newtype (wraps secrecy::SecretString;
                           explicitly NOT Serialize/Deserialize)
    fs_store.rs         <- ProfileFileStore with atomic writes
    error.rs

Rules:
- SecretString is NEVER Serialize or Deserialize. Stage 1 writes
  secrets as plaintext; the encrypt stage in the next prompt wires
  in the encrypted blob type.
- Atomic writes: write to /var/lib/nexus/profiles/<kind>/<name>.toml.tmp,
  fsync, rename. Directory fsync after rename.
- File permissions: 0600 for files, 0700 for directories.
- Missing profile directory on startup: create it.
- Malformed profile on load: log at WARN, skip, continue loading
  the rest. Do NOT fail the whole load.

Tests:
- Round-trip every profile kind through serialize → write → read →
  deserialize
- Atomic write survives mid-write crash simulated via a #[cfg(test)]
  hook that panics after tmp-file write but before rename. After
  recovery, the original file should be intact.
- Load with a malformed profile in the directory: returns the valid
  ones, logs the bad one.
- Permissions: after put, verify file mode is 0600.

Exit criterion: `cargo test -p nexus-profile-store` passes. The
trait definitions compile against their use sites (you can verify
by adding a dummy `impl ProfileStore for ()` anywhere that won't
ship to production).
```

### Prompt 2.2 — DD-007 Phases 3-5 (encryption)

🟡 Depends on 2.1 · ⏱️ Large (~3-4 hours)

```
Add encryption to the Profile Store per /docs/dd-007-profile-store.md
phases 3-5.

This prompt covers:
- ChaCha20-Poly1305 AEAD via the `chacha20poly1305` crate
- HKDF-SHA256 key derivation for per-file keys (§4.3)
- Master key sources: file, TPM-sealed, operator-derived (§4.2)
- Wire encryption into ProfileFileStore's put_* and load_* paths

Files:
  crates/nexus-profile-store/src/
    crypto/
      mod.rs
      cipher.rs     <- Cipher trait (abstract over chacha for testing),
                       plus the actual ChaCha20Poly1305 impl
      kdf.rs        <- HKDF per-file-key derivation
      blob.rs       <- EncryptedBlob on-disk type: {nonce, ciphertext,
                       ad_hash}
    keys/
      mod.rs        <- MasterKeySource trait
      file_source.rs
      tpm_source.rs    <- use `tss-esapi` crate; gate this entire
                          module behind feature = "tpm"
      derived_source.rs  <- operator-password → scrypt-derived

Implement the dual-struct pattern from §5.3: in-memory
WifiProfile has SecretString fields; WifiProfileOnDisk has
EncryptedBlob fields. The ProfileFileStore's write path calls
Cipher::encrypt on each SecretString before constructing the on-disk
struct; the read path does the inverse.

Nonce handling (critical for security):
- Fresh random nonce per write, 12 bytes (ChaCha20Poly1305 standard)
- Associated data for each blob: the profile kind + profile ID +
  field path (e.g., "wifi:01ARZ3...:network.psk"). This ensures a
  ciphertext can't be moved between profiles or fields.

Tests:
- Encrypt → decrypt roundtrip for every SecretString field type
- Wrong AD causes decryption to fail
- Ciphertext is different on every write (nonce freshness)
- File-source key: write master.key, load, verify profile encryption
  works
- TPM-source tests gated behind feature = "tpm-integration" and
  require an actual TPM; default CI skips them
- Derived-source: password → key is deterministic; wrong password
  fails to decrypt existing profiles

Security constraint: NEVER log plaintext secrets, nonces, or key
material. Add a clippy lint or compile-time check that would catch
a regression here. At minimum, ensure SecretString's Debug impl is
redacted (should already be from 2.1).

Exit criterion: `cargo test -p nexus-profile-store` passes.
`cargo test -p nexus-profile-store --features tpm-integration` is
acceptable to be skipped on machines without a TPM. Add a
`cargo run --example roundtrip` binary that demonstrates encrypting
a profile to disk and reading it back.
```

### Prompt 2.3 — DD-007 Phases 6-9 (versioning, rotation, observability, hardening)

🟡 Depends on 2.2 · ⏱️ Medium (~2-3 hours)

```
Complete the Profile Store per /docs/dd-007-profile-store.md phases 6-9.

Adds:
- Schema versioning and migration framework (§8)
- Master key rotation (§4.5)
- Prometheus metrics per §10.4
- Hardening: poisoned-file quarantine, rotation lock, structured logs

Files:
  crates/nexus-profile-store/src/
    migrate.rs      <- schema version detection, migration dispatch
    rotate.rs       <- rotate_master_key implementation
    metrics.rs
    quarantine.rs   <- moving corrupt files to /var/lib/nexus/profiles/
                       .quarantine/ with a tombstone note

Key rotation semantics:
- Take the .rotation.lock exclusive flock
- Iterate all profiles, decrypt with old key, re-encrypt with new key,
  write atomically
- Emit NexusEvent::ProfileStoreRotationProgress every 10 profiles
  or every 500ms, whichever comes first
- On failure mid-rotation: log + emit NotificationEvent{kind:
  "master_key_degraded", ...}; old master.key remains usable for any
  profiles not yet rotated

Quarantine semantics:
- Triggered when load_* encounters a file that decrypts successfully
  but fails structural validation (bad schema_version, missing
  required fields), OR when decryption fails entirely
- Move to .quarantine/<original-name>.<timestamp>
- Write a tombstone note alongside describing the failure reason
- Emit NotificationEvent{kind: "profile_corrupt", ...}

Tests:
- Schema migration: seed a v1 profile, load under v2, verify
  migration applied and on-disk file is now v2
- Rotation: 10 profiles, rotate, verify all decrypt under new key and
  none under old
- Rotation failure: crash after half the profiles are rotated;
  verify startup reports the mixed state cleanly
- Quarantine: inject a truncated ciphertext blob; verify it moves to
  .quarantine/ on load attempt

Exit criterion: `cargo test -p nexus-profile-store` passes. Full
nexus-profile-store public API matches the ProfileStore trait in
/docs/dd-007-profile-store.md §5. `cargo doc -p nexus-profile-store
--no-deps` produces no warnings.
```

---

## Phase 3 — Backends (parallelizable)

Once nexus-core (0.2) and nexus-profile-store through 2.2 are done, the four backends can be implemented in parallel. Each has its own prompt series. Pick whichever order matches your testing capacity.

### Prompt 3a — DD-002 Ethernet Backend (complete)

🟡 Depends on 0.2, 2.1 · ⏱️ Large (~4-6 hours, may span two sessions)

```
Implement the Ethernet Backend per /docs/dd-002-ethernet-backend.md,
all 8 phases.

Start by reading the whole DD end-to-end. Key components:

- Per-interface state machine: Idle → Authenticating → Authenticated
  → LinkUp → LinkDown, with retry loops on auth failure (§3)
- WiredAuthBackend trait (§4) with a mock implementation for tests
  and a wpa_supplicant-backed implementation (§5)
- Retry policy with exponential backoff (§6)
- Auth backend crash recovery (§7) — wpa_supplicant control socket
  reconnection
- Feature-gated ead backend (§8 — optional, skip if short on time)
- Metrics per §9.5
- Integration tests per §10

Crate: crates/nexus-ethernet/

The control-socket code for wpa_supplicant lives in nexus-auth-eap/
(shared with DD-003). If nexus-auth-eap is empty, stub it with just
the control socket wrapper — the Wi-Fi-specific plumbing lands in
DD-003's prompt.

Phase ordering within this prompt:
1. Skeleton + state machine + profile loading (phases 1-2 of DD-002)
2. Mock backend + state machine tests (phase 3)
3. wpa_supplicant backend (phase 4) — can be deferred if
   integration-test infrastructure isn't ready
4. Retry policy (phase 5)
5. Crash recovery (phase 6)
6. Metrics + integration tests (phase 8)

Deferring ead (phase 7) is fine — mark the module as `pub(crate) mod
ead {}` with a TODO linking back to DD-002 §8.

Integration tests (#[cfg(feature = "integration-linux")]):
- Set up a veth pair; enable 802.1X on one end via hostapd; connect
  Nexus's backend; verify Authenticated state
- Kill hostapd mid-authentication; verify backend enters retry loop
- These tests require CAP_NET_ADMIN and hostapd configured;
  #[ignore] them in default CI.

Exit criterion: `cargo test -p nexus-ethernet` (unit tests) passes.
If integration-test infra is set up: `sudo cargo test -p
nexus-ethernet --features integration-linux` passes too. The
NexusEvent::EthAuthStateChanged stream matches the state machine
transitions in DD-002 §3.
```

### Prompt 3b — DD-003 Wi-Fi Backend (Phases 1-4)

🟡 Depends on 0.2, 2.1 · ⏱️ Large (~4-5 hours)

```
Implement the Wi-Fi Backend scaffolding + default wpa_supplicant
backend per /docs/dd-003-wifi-backend.md phases 1-4.

Phases 5+ (scanning, roaming, crash recovery, power management,
iwd) are separate prompts below.

Phases in this prompt:
1. Skeleton and lifecycle state machine (§6 — WifiState transitions)
2. Supplicant trait + mock (§5.1, §5.2)
3. Profile store integration (§6.1 — WifiProfile matching, SSID
   hashing, credentials-invalid flag)
4. wpa_supplicant backend (§5.3) — control socket via
   nexus-auth-eap crate

Crate: crates/nexus-wifi/

The Supplicant trait boundary is critical — every wpa_supplicant
interaction goes through it, which means the mock impl must be
rich enough to drive every NexusEvent::Wifi* variant in tests.

Rules from DD-003 that must be reflected in pseudocode:
- MacAddr from nexus-core (§4.2) — do not define a local type
- SecretString from nexus-profile-store — all PSK/passphrase fields
- Profile ULIDs are stable; SSID hash is the filename key

Tests:
- State machine transitions: every transition in §6 is covered by
  a unit test using the MockSupplicant
- Profile matching: SSID-hash collision handling, credentials-invalid
  path
- Mock supplicant: all SupplicantEvent variants are producible from
  tests

Exit criterion: `cargo test -p nexus-wifi` passes. Phases 5+ are
separate prompts; this prompt leaves Wi-Fi able to connect to a
known SSID with cached profile but NOT able to scan for new APs
or roam.
```

### Prompt 3c — DD-003 Wi-Fi Backend (Phases 5-9)

🟡 Depends on 3b · ⏱️ Large (~4-6 hours)

```
Complete the core Wi-Fi Backend per /docs/dd-003-wifi-backend.md
phases 5-9 (scanning, connection flow, roaming, crash recovery, power
management).

Phase 10 (iwd) is a separate optional prompt.

Phases:
5. Scanning and scan scheduling (§7)
6. Connection flow and failure handling (§8)
7. Roaming (§9)
8. Supplicant crash recovery (§10)
9. Power management (§11)

Key testable behaviors:
- Scan → BSS filtering → network selection → connect
- Connection timeout → retry with backoff → give up → emit
  LinkLost
- Roaming triggered by signal threshold → reassociate → back to
  Connected
- Supplicant crash → mark state Degraded → reconnect control socket
  → reregister → resume
- Power state transitions: active/background/sleep affect scan
  cadence and supplicant lifecycle

Tests build on MockSupplicant from 3b. Every scan-to-connect path
has unit-test coverage.

Integration tests (#[cfg(feature = "integration-linux")]) behind
the same gate as DD-002's integration tests; actual hostapd-hosted
AP infra may be shared.

Exit criterion: `cargo test -p nexus-wifi` passes. Integration
tests (if infrastructure available) connect to a hostapd AP via
wpa_supplicant, scan, connect, disconnect, and roam between two APs.
```

### Prompt 3d — DD-004 Bluetooth Backend (Phases 1-4)

🟡 Depends on 0.2, 2.1 · ⏱️ Large (~4-5 hours)

```
Implement the Bluetooth Backend scaffolding + core D-Bus interactions
per /docs/dd-004-bluetooth-backend.md phases 1-4.

Phases 5+ (pairing, profile store, reconnection, power, observability)
are separate prompts.

Phases in this prompt:
1. Shared types and BlueZ proxies (§6 — BluezClient trait + zbus
   proxy definitions for Adapter1, Device1, AgentManager1,
   ObjectManager)
2. BlueZ client and ObjectManager subscription (§6.1, §7.1 — the
   ZbusBluezClient and its signal subscriptions)
3. Adapter state machine (§4)
4. Device state machine + BlueZ ops (§5)

Crate: crates/nexus-bluetooth/

Implementation notes from the DD that MUST be respected:
- BluezClient trait methods all take `&self` (not `&mut self`) and
  the concrete impl is stored as `Arc<dyn BluezClient>` —
  see DD-004 §6.1 "Concurrency" notes
- The backend's main task and the Agent task communicate via the
  cmd_tx command channel, never via shared state — see §7.2 and
  §8.1
- MacAddr is from nexus-core, not a Bluetooth-specific type. The
  BluetoothAddrExt trait in nexus-bluetooth provides BlueZ-format
  helpers
- DiscoveryTransport enum (for filter) is separate from BtTransport
  (for device info) — §9.2

Tests using a mock BluezClient:
- Adapter lifecycle: Unavailable → Present → Powered → Discovering
  → Gone
- Device lifecycle through every documented transition
- Discovery session start/stop with and without other clients
  holding concurrent sessions
- NexusEvent emissions match DD-004 §7.2

Don't implement pairing yet. The Pair() command returns "not
implemented" for now.

Exit criterion: `cargo test -p nexus-bluetooth` passes. A
bin/demo.rs that connects to the real system BlueZ (if present),
lists adapters, and runs a 10s discovery session — works on a real
machine with BlueZ running.
```

### Prompt 3e — DD-004 Bluetooth Backend (Phases 5-8)

🟡 Depends on 3d · ⏱️ Large (~5-6 hours)

```
Complete the Bluetooth Backend per /docs/dd-004-bluetooth-backend.md
phases 5-8.

Phases:
5. Agent and pairing (§8) — INCLUDING the command-based Agent ↔
   backend coordination from §8.1
6. Profile Store integration (§8.3, §11)
7. Reconnection, supervisor, power management (§12, §13)
8. Observability and hardening (§13.2, §14)

The pairing flow is the most intricate part of this DD. Read §8 in
full before writing any code. Key points:
- The Agent task runs in zbus's object-server task, NOT on the
  backend main task
- It coordinates with the backend via four commands:
  LookupPairingJob, RegisterPromptOneshot, AnswerPairingPrompt,
  LookupAuthorizationPolicy
- The backend owns `pending_prompt_answers` exclusively; the Agent
  only accesses it via commands
- BluezClient::pair() is driven from a spawned task so the main
  loop doesn't block on operator response time
- After successful pair, call set_trusted and persist a
  BluetoothProfile to the store

Tests:
- Pair with numeric comparison: Pair → Agent RequestConfirmation →
  BtPairingPrompt → AnswerPairingPrompt(accept=true) → Paired →
  profile stored → Trusted set on BlueZ
- Pair rejected by operator: AnswerPairingPrompt(accept=false) →
  Failed state, no profile stored
- Operator-timeout: no AnswerPairingPrompt arrives within
  agent_response_timeout_s → Agent method returns error → Failed
- Incoming connection with auto_accept_incoming profile flag:
  RequestAuthorization auto-accepts, no prompt fires
- Incoming connection without the flag: RequestAuthorization
  fires a prompt
- BlueZ restart mid-pair: clean failure, state recovered on
  reconnect

Integration tests (#[cfg(feature = "integration-bluez")]) against
real BlueZ if available; otherwise mark #[ignore].

Exit criterion: `cargo test -p nexus-bluetooth` passes. Against a
real BlueZ with a controllable peer (nRF52, another machine): pair,
connect, disconnect, reconnect via auto-connect, forget. All
metrics populated. `cargo doc -p nexus-bluetooth --no-deps` clean.
```

### Prompt 3f — DD-005 GNSS Backend (complete)

🟡 Depends on 0.2, 2.1 · ⏱️ Large (~4-5 hours)

```
Implement the GNSS Backend per /docs/dd-005-gnss-backend.md, all 8
phases.

Smaller than the others — gpsd is a one-way JSON stream, no control
protocol.

Phases:
1. Types and gpsd JSON parsing (§4)
2. gpsd JSON client (§6)
3. Device lifecycle state machine (§7)
4. Quality filtering and emission policy (§8)
5. Profile store integration (§9)
6. Reconnection and supervisor (§10)
7. Power management (§11)
8. Observability and hardening (§11.2, §12)

Crate: crates/nexus-gnss/

Key design decisions from the DD:
- gpsd is the integration surface (ADR-005). No direct serial
  parsing.
- GnssFix.time is ISO-8601 string from gpsd 3.x — NOT a float
  epoch (§4.1 fixes a past drift)
- Two-tier event emission: GnssTpvReceived (raw from reader task)
  → GnssFixChanged (filtered, emitted by backend) — §8
- The reader task and backend communicate via the event bus only;
  no shared state

Tests:
- JSON parser: all gpsd 3.x message types (TPV, SKY, DEVICES, ERROR,
  WATCH) round-trip through parse → emit → handle
- Quality filter: fixes below min accuracy threshold are dropped
- Rate cap: at most one GnssFixChanged per min_emit_interval_ms
- Reconnection: gpsd goes away, backend emits GnssGpsdDisconnected,
  reader reconnects with backoff, emits GnssGpsdConnected
- Profile by device path: linear scan is fine for expected counts

Integration tests (#[cfg(feature = "integration-gpsd")]): run a
real gpsd with a gpsfake source; verify end-to-end flow.

Exit criterion: `cargo test -p nexus-gnss` passes. A bin/demo.rs
that connects to a running gpsd and prints GnssFixChanged events —
verified working against gpsd 3.22+ with any USB receiver or
gpsfake NMEA log.
```

---

## Phase 4 — D-Bus API

### Prompt 4.1 — DD-006 Phases 1-3 (skeleton, read-only interfaces)

🟡 Depends on 1.3 (for InterfaceInfo events), 2.3, and at least ONE backend from 3a/3b/3d/3f · ⏱️ Large (~4-5 hours)

```
Implement the D-Bus API skeleton and read-only interfaces per
/docs/dd-006-dbus-api.md phases 1-3.

This prompt needs at least one backend implemented (any of Ethernet,
Wi-Fi, Bluetooth, GNSS) so there's real state to surface. Others
can be added in subsequent prompts.

Phases:
1. Service skeleton + Manager object at /fi/nexus1 (§4, §5)
2. Per-technology interface objects, read-only (§6 — Common,
   Ethernet, Wi-Fi, Bluetooth, GNSS, BluetoothDevice)
3. Profile objects, read-only (§7)

Crate: crates/nexus-dbus/

Rules:
- Bus name fi.nexus1 on the system bus (not session bus)
- Object paths from DD-006 §4 — follow exactly
- zbus v5 with tokio runtime integration
- Every Property and Method from DD-006 §6 must be present with
  correct type signatures
- Subscribe to NexusEvent via a broadcast Receiver and translate
  to D-Bus signals + property notifications
- NO mutating methods yet — those come in later prompts

The Manager object exposes:
- Properties: Version, PowerState, Interfaces (ao), Profiles (ao)
- Methods (read-only only for now): GetManagerStatus
- Signals: PowerStateChanged, NotificationEvent, MasterKeyRotated

Tests:
- Spawn the D-Bus service on a session bus (zbus test harness),
  call GetManagerObjects, verify returned tree matches the spawned
  state
- Subscribe to Interfaces property; add an interface via a fake
  NexusEvent; verify the property signal fires
- Interface-specific property reads (Powered on Bluetooth, Signal
  on Wi-Fi, etc.)

Integration tests (#[cfg(feature = "integration-dbus")]) against
the system bus require a running D-Bus daemon and appropriate
policy.

Exit criterion: `cargo test -p nexus-dbus` passes. A bin/demo.rs
spawns the Nexus daemon stack (interface monitor + any backends +
nexus-dbus), registers fi.nexus1 on the session bus, and
`dbus-send --session --print-reply --dest=fi.nexus1 /fi/nexus1
org.freedesktop.DBus.ObjectManager.GetManagedObjects` returns the
expected tree.
```

### Prompt 4.2 — DD-006 Phases 4-6 (PolicyKit, mutations)

🟡 Depends on 4.1 · ⏱️ Large (~4-5 hours)

```
Implement PolicyKit integration and mutating D-Bus methods per
/docs/dd-006-dbus-api.md phases 4-6.

Phases:
4. PolicyKit integration (§10)
5. Mutating methods for Wi-Fi (§6.3 methods)
6. Mutating methods for Ethernet (§6.2 methods) and PowerState

PolicyKit actions from §10 are authoritative; don't invent new
ones. Install the action file to /usr/share/polkit-1/actions/
fi.nexus.policy in the daemon's setup; for now just keep it as
part of the crate's /data/.

Every mutating method does:
1. Extract the D-Bus sender's bus name and uid
2. Call PolicyKit CheckAuthorization with the appropriate action
3. If denied, return fi.nexus.Error.AuthFailed
4. Otherwise, send a command to the corresponding backend's cmd_tx

Tests:
- PolicyKit mock that accepts/rejects based on a configurable
  policy
- Every mutating method has a "denied" test and an "accepted" test
- SetPowerState propagates to every backend

Exit criterion: `cargo test -p nexus-dbus` passes. Against a real
system bus with PolicyKit: admin user can AddWifiProfile; non-admin
user gets AuthFailed.
```

### Prompt 4.3 — DD-006 Phases 7-10 (remaining)

🟡 Depends on 4.2 · ⏱️ Medium (~2-3 hours)

```
Complete the D-Bus API per /docs/dd-006-dbus-api.md phases 7-10.

Phases:
7. Scan result objects (§6.3 ScanResult interface)
8. Signals and coalescing (§9.3)
9. Admin operations (§9.4 — RotateMasterKey, GetMasterKeyInfo, etc.)
10. Rate limiting and error details

Coalescing: hot properties like Wi-Fi signal strength and GNSS
fix should update the property cache but batch the
PropertiesChanged signal at no more than 2 Hz per property.

Rate limiting: per-sender limits per §11 — reject with
fi.nexus.Error.RateLimited when exceeded.

Tests:
- Signal coalescing: flood the event bus with WifiSignalPoll;
  verify outgoing PropertiesChanged is rate-limited
- Rate limiting: call Scan() 100× from one sender; verify rejection
  after the configured limit

Exit criterion: `cargo test -p nexus-dbus` passes end-to-end,
including the above behaviors.
```

---

## Phase 5 — Daemon and integration

### Prompt 5.1 — nexus-daemon binary

🟡 Depends on 4.3 and all backends · ⏱️ Medium (~2-3 hours)

```
Implement the daemon binary that composes everything per
/docs/nexus-architecture.md §5.

Files:
  crates/nexus-daemon/src/
    main.rs
    config.rs       <- load nexus.toml, validate, expose to subsystems
    bus.rs          <- NexusEvent broadcast channel wiring
    supervision.rs  <- spawn/restart logic for each subsystem

The daemon:
1. Parses CLI args (just --config PATH for now)
2. Loads /etc/nexus/nexus.toml (or the --config path)
3. Creates a tokio broadcast channel for NexusEvent with capacity
   from config
4. Spawns each enabled subsystem's main task, passing the channel
   sender
5. Installs SIGTERM/SIGINT handlers that propagate a
   CancellationToken
6. Awaits graceful shutdown

Configuration format: single nexus.toml with per-subsystem tables
(see [bluetooth], [gnss], [interface_monitor], etc. in the
respective DDs).

Subsystem enable/disable: each subsystem's TOML section has an
`enabled = true` flag; daemon honors it to skip spawning.

On subsystem crash: log at ERROR level, emit NotificationEvent,
optionally restart with backoff (config flag
`supervision.restart = true`).

Tests:
- Config loading with every field exercised
- SIGTERM handling: daemon drains and exits within 5s
- Subsystem disabled: no events from it

Exit criterion: `cargo run -p nexus-daemon -- --config tests/
fixtures/minimal.toml` on a real Linux machine starts, discovers
interfaces, serves fi.nexus1 on the system bus. `systemctl stop
nexus` (after install) shuts down cleanly.
```

### Prompt 5.2 — systemd unit and packaging

🟡 Depends on 5.1 · ⏱️ Small (~1-2 hours)

```
Add systemd unit file, installation scripts, and packaging bits.

Files:
  packaging/
    nexus.service         <- systemd unit
    nexus.conf.d/*.conf   <- example drop-in configs
    polkit-1/
      actions/fi.nexus.policy
      rules.d/50-nexus.rules  <- example rules
    tmpfiles.d/nexus.conf     <- /var/lib/nexus creation
    nexus-install.sh          <- dev install script

  docs/
    operator-guide.md     <- brief operator intro
    configuration.md      <- all TOML fields, cross-referenced to DDs

The nexus.service unit:
- Type=notify (use sd_notify on startup completion)
- User/Group=nexus (not root; requires CAP_NET_ADMIN ambient)
- AmbientCapabilities=CAP_NET_ADMIN CAP_NET_RAW
- NoNewPrivileges=yes
- ProtectSystem=strict with ReadWritePaths=/var/lib/nexus
- After=dbus.service bluetooth.service (soft dep via Wants=)

Tests:
- Install script runs on a clean Ubuntu 24.04 VM
- `systemctl start nexus` succeeds, `journalctl -u nexus` shows
  clean startup

Exit criterion: On a clean VM: installing via the script + starting
via systemctl → Nexus is running, visible on D-Bus, processing
interface events.
```

---

## Phase 6 — DD-006 updates (daemon-side additions)

These three prompts bring DD-006's draft-state additions into the running daemon. They are prerequisites for nexusctl Phase 7.4 and Phase 7.5; earlier nexusctl phases (7.1–7.3) can proceed in parallel and don't need these updates.

### Prompt 6.1 — Add `FeatureDisabled` and `RateLimited` errors

🟡 Depends on 4.3 · ⏱️ Small (~45 min)

```
Add two new D-Bus error variants to nexus-dbus, per
/docs/dd-006-dbus-api.md (the canonical error list, see §9 plus the
associated per-method "Errors:" lines).

The two additions:

1. fi.nexus.Error.FeatureDisabled
   Returned when a client calls a method on an interface whose
   backend is disabled in nexus.toml (e.g., [bluetooth.enabled] =
   false). The D-Bus interface and objects still exist so that
   clients can discover what is and isn't available, but mutating
   methods return FeatureDisabled. Property reads of non-sensitive
   summary state (e.g., `Enabled: false`) remain permitted.

2. fi.nexus.Error.RateLimited
   Returned by methods whose per-sender rate limit is exceeded. The
   existing rate-limiting mechanism per DD-006 §11 already exists;
   this prompt only adds the error name it returns. Until now, rate
   limit exceedance has returned ResourceBusy, which is overloaded
   and imprecise. This prompt is purely a rename + propagation.

Files to modify:

  crates/nexus-dbus/src/error.rs
    - Add FeatureDisabled { feature: String } and
      RateLimited { retry_after_ms: u64 } variants to the existing
      NexusDbusError enum.
    - Map each to its D-Bus error name via the existing
      #[zbus::DBusError] derive.

  crates/nexus-dbus/src/rate_limit.rs (or wherever the limiter
    lives — grep for "ResourceBusy" and "rate_limit")
    - Change the return from ResourceBusy to RateLimited. Preserve
      retry_after_ms semantics.

  crates/nexus-dbus/src/{wifi,bluetooth,gnss,ethernet}.rs and any
    other per-interface modules:
    - At the top of every method handler, check whether the backend
      for this kind is enabled (a Config::is_enabled(kind) helper
      or equivalent). If not, return FeatureDisabled with the
      feature name. Do NOT apply this check to property reads that
      expose enable/disable state itself.

  crates/nexus-core/src/error.rs (or wherever backend traits live):
    - If error types there correspond, add matching variants.

Tests:
- Disabled-feature test: spawn the daemon with [wifi.enabled] =
  false in config, call Wifi.Scan() via zbus, expect
  FeatureDisabled.
- Rate-limited test: hammer Scan() faster than the configured
  limit; expect RateLimited. Verify retry_after_ms is populated.
- Error mapping test: each new error's D-Bus name is exactly as
  documented in DD-006.

Exit criterion: `cargo test -p nexus-dbus` passes. Both new errors
round-trip through zbus. The existing integration tests still
pass (ResourceBusy is no longer used for rate limiting, but is
still used for other cases per DD-006).
```

### Prompt 6.2 — Tighten `AnswerPairingPrompt` variant validation

🟡 Depends on 3e (Bluetooth backend pairing) · ⏱️ Medium (~1.5 hours)

```
Tighten AnswerPairingPrompt's variant-type handling per the updated
spec in /docs/dd-006-dbus-api.md §6.4 ("AnswerPairingPrompt" method
docs). The old implementation loosely accepted any variant; the new
spec enforces a per-prompt-kind variant-type mapping and introduces
an explicit "acknowledge" string for DisplayPasskey and DisplayPin.

The per-prompt-kind map (from DD-006):

  RequestPin          -> "s" (PIN string, 4-16 ASCII)
  RequestPasskey      -> "u" (0..999999)
  RequestConfirmation -> "b"
  RequestAuthorization -> "b"
  AuthorizeService    -> "b"
  DisplayPasskey      -> "s" with value "acknowledge"
  DisplayPin          -> "s" with value "acknowledge"

Plus the global "cancel" special: answer = "s":"cancel" cancels any
prompt regardless of kind.

Wrong variant type → fi.nexus.Error.InvalidArgument.
Wrong string value for Display* (not "acknowledge" or "cancel") →
InvalidArgument.

Implementation points:

1. crates/nexus-bluetooth/src/pairing.rs (or agent.rs — wherever
   PairingAnswer is consumed): enforce the mapping in the command
   handler that receives AnswerPairingPrompt. The backend already
   knows the pending prompt kind (indexed by job_id), so the check
   is a match arm.

2. crates/nexus-dbus/src/bluetooth.rs: at the D-Bus edge, decode
   the zbus Value into PairingAnswer. Return InvalidArgument on
   mismatch before forwarding to the backend.

3. The Acknowledge variant in PairingAnswer (from DD-004 §6.2) is
   already defined as a unit variant in Rust. The D-Bus-to-Rust
   translation is: string "acknowledge" → PairingAnswer::Acknowledge.
   String "cancel" → PairingAnswer::Cancel. Any other string for a
   non-PIN prompt kind → InvalidArgument.

Tests:
- Valid: each prompt kind receives its valid variant type; backend
  transitions pairing state appropriately.
- Invalid variant type: RequestConfirmation + s:"yes" → InvalidArgument.
- Invalid string value: DisplayPasskey + s:"ok" → InvalidArgument.
- Acknowledge roundtrip: DisplayPasskey + s:"acknowledge" → backend
  returns control to BlueZ; pairing continues.
- Cancel override: any kind + s:"cancel" → backend transitions to
  Failed(reason=rejected).

Exit criterion: `cargo test -p nexus-bluetooth -p nexus-dbus`
passes. A manual test with bluetoothctl-style peer driven by the
nexusctl-equivalent test harness (or via busctl) exercises all 7
prompt kinds.
```

### Prompt 6.3 — Implement `ReloadConfig` Manager method

🟡 Depends on 5.1 (daemon binary) · ⏱️ Large (~4-5 hours)

```
Implement Manager.ReloadConfig per /docs/dd-006-dbus-api.md §5.2.
This is the largest DD-006 addition and touches every subsystem.

Spec recap from DD-006 §5.2:

  ReloadConfig() -> (report: a{sv})

  Returns a dict with:
    "applied":  as — config sections whose new values took effect
    "deferred": as — sections that require a restart (change is ignored)
    "errors":  a(ss) — (section, reason) pairs for sections that
                        failed to reload

  Gated by fi.nexus.admin PolicyKit action.

Implementation strategy — follow this order:

STEP 1: Classify every field in nexus.toml as reloadable vs
startup-only. Do this in crates/nexus-daemon/src/config.rs by
annotating the struct fields with a marker attribute (a helper
derive macro, or a hand-written ReloadClassification enum).

  Startup-only (always in "deferred" when changed):
    - dbus.bus_name
    - dbus.object_root
    - profile_store.path
    - master_key.source (changes require a rotate workflow)
    - interface_monitor.netlink.buffer_size
    - logging.json_stderr

  Reloadable (eligible for "applied"):
    - All [wifi.*] scan cadences, timeouts, roaming thresholds
    - All [bluetooth.*] discovery timeouts, pairing agent timeouts,
      auto-connect policy
    - All [gnss.*] emit policy, quality filtering, rate cap
    - All [ethernet.*] retry/backoff policy
    - [logging.level]  (tracing-subscriber supports runtime reload)
    - [metrics.*] if metrics endpoint already bound
    - [power.*] default state

STEP 2: Each backend exposes a reload() method on its command/control
channel. The method takes the new subsystem config and returns
Result<AppliedFields, ReloadError> where AppliedFields is a Vec<&'static
str> naming fields that actually changed. The daemon's reload handler
calls each backend's reload() in sequence. Backends that are disabled
(not running) return Ok(vec![]).

Files to touch per subsystem:
  - crates/nexus-interface-monitor/src/monitor.rs: reload(new cfg)
  - crates/nexus-wifi/src/lib.rs: reload(new cfg)
  - crates/nexus-bluetooth/src/lib.rs: reload(new cfg)
  - crates/nexus-gnss/src/lib.rs: reload(new cfg)
  - crates/nexus-ethernet/src/lib.rs: reload(new cfg)

Each backend's reload():
  - Diff old-vs-new config
  - For each reloadable field that differs, apply the change (update
    the in-memory cfg, possibly cancel+restart a timer, etc.)
  - For each startup-only field that differs, append to a "deferred"
    list
  - On any error, append to "errors" and revert that specific field
    (not the whole reload)

STEP 3: Wire ReloadConfig into the D-Bus Manager.

  crates/nexus-dbus/src/manager.rs:
    - Add the method handler
    - Check PolicyKit fi.nexus.admin
    - Call into nexus-daemon's reload coordinator
    - Translate the AppliedFields/DeferredFields/Errors into the
      a{sv} report structure

STEP 4: The daemon binary reads the config file from disk fresh on
each ReloadConfig call (not a cached copy). Path is the same
--config argument it was started with.

File rereads:
  - On reload, re-read /etc/nexus/nexus.toml (or the --config path)
  - Re-validate the whole file; structural/parse errors → return
    IoError from the method, no partial apply
  - Diff against the live config, call each backend's reload()

Tests:
- Reload with no changes: all lists empty, no errors.
- Reload a reloadable field (wifi.scan.cadence_active_s): applied
  list contains it, next scan uses the new cadence.
- Reload a startup-only field (dbus.bus_name): deferred list
  contains it, live bus name is unchanged.
- Reload with syntax error in TOML: method returns IoError, live
  config unchanged.
- Reload with semantically-invalid value (wifi.scan.cadence_active_s
  = -1): errors list contains the section, that field is unchanged.
- Reload without auth: AuthFailed.
- Reload while a backend is mid-scan: scan is not interrupted;
  the new cadence applies on next scan.

Exit criterion: `cargo test -p nexus-daemon -p nexus-dbus` passes.
Against a running daemon: `busctl call fi.nexus1 /fi/nexus1
fi.nexus.Manager ReloadConfig` returns the expected dict. Changing a
reloadable field in nexus.toml + ReloadConfig → next backend
operation uses the new value, verified by metric or log.
```

---

## Phase 7 — nexusctl client

Eight prompts mapping DD-008's eight implementation phases. Early phases (7.1–7.3) are independent of Phase 6 and can run in parallel with Phase 6 prompts. Later phases depend on Phase 6.

### Prompt 7.1 — Skeleton, connection, `status` and `iface list`

🟡 Depends on 4.1 (D-Bus read-only interfaces) · ⏱️ Medium (~2-3 hours)

```
Bootstrap the nexusctl binary crate per /docs/dd-008-nexusctl-client.md
phases 1 from §12. Cover the skeleton, D-Bus connection, and the
first two commands end-to-end.

Crate: crates/nexus-client/ (binary name `nexusctl`).

Dependencies: clap (v4, derive), zbus (v5, tokio), tokio, serde,
serde_json, comfy-table, anstream, tracing-subscriber. No dependency
on nexus-core or any nexus-* backend crate — nexusctl talks only
D-Bus to nexusd.

Files to create:

  crates/nexus-client/Cargo.toml
  crates/nexus-client/src/
    main.rs               <- #[tokio::main]; parse args; dispatch
    cli.rs                <- clap derive structs (see DD-008 §4.1)
    dispatch.rs           <- top-level command → handler dispatch
    errors.rs             <- NexusctlError enum per DD-008 §9
                             (start with: AuthDenied, Timeout,
                             NotInteractive, NexusdUnreachable,
                             Other)
    proxy/
      mod.rs
      manager.rs          <- #[zbus::proxy] for fi.nexus.Manager at
                             /fi/nexus1
      interface.rs        <- #[zbus::proxy] for fi.nexus.Interface
                             (common; read-only for now)
    commands/
      status.rs           <- nexusctl status
      iface.rs            <- nexusctl iface list (only)
    output/
      mod.rs              <- OutputFormat enum: Human | Json
      human.rs            <- comfy-table renderer (iface list only
                             for now)
      json.rs             <- serde_json emission

Subcommands to implement in this prompt:

  nexusctl status
    - Call Manager.GetManagerStatus (or equivalent — check DD-006
      §5.2)
    - In human mode: print daemon version, PowerState, a count of
      interfaces per kind, BlueZ/gpsd availability as vertical
      key/value
    - In JSON mode: emit a single object with the same fields

  nexusctl iface list
    - Call Manager.Interfaces property, iterate interface object
      paths
    - For each, read the common Interface properties (iface name,
      kind, state, mac)
    - Human: comfy-table with IFACE, KIND, STATE, MAC columns.
      Leave the state-prefix column and DETAILS as TODO for later
      phases.
    - JSON: array of flat objects

Global options to wire (partial — later prompts add the rest):
  --json           <- forces OutputFormat::Json
  --format <fmt>   <- matches "human" or "json" (add terse/pretty
                      in Phase 7.2)
  --bus <address>  <- use zbus::Connection::for_address instead of
                      system()
  --verbose / -v   <- set tracing-subscriber level to DEBUG
  --help / -h      <- clap default

Exit codes (implement the ones used here; add the rest as needed in
later phases):
  0 on success
  6 when zbus returns ServiceUnknown
  1 on any other error

Tests:
- Snapshot tests for human and JSON output using a fixture
  Manager proxy (Claude Code may need to invent a MockManagerProxy
  pattern for unit tests — one clean approach is to abstract the
  proxy behind a trait in proxy/mod.rs and have commands take
  `impl ManagerOps` so tests pass in a mock)
- Argument parse tests: clap rejects unknown subcommands, accepts
  abbreviated (`nexusctl stat` → status)

Manual verification:
- Against a running nexusd (from Prompt 5.1), run `nexusctl status`
  and `nexusctl iface list`. Output matches DD-008 §5.1 examples
  (minus the state-prefix column).
- `nexusctl --json iface list | jq '.[0].iface'` returns a string.
- `nexusctl status` with nexusd stopped exits 6 with
  "nexusd is not running" message.

Exit criterion: Both commands work in both formats against a real
nexusd. `cargo test -p nexus-client` passes. The crate builds
against the workspace without pulling in backend dependencies
(verify by running `cargo tree -p nexus-client | grep nexus-` —
should list only nexus-client itself).
```

### Prompt 7.2 — Output formats and error translation

🟡 Depends on 7.1 · ⏱️ Medium (~3 hours)

```
Complete the output-format and error-translation infrastructure per
/docs/dd-008-nexusctl-client.md §5 and §9 in full. After this prompt
every future subcommand just implements its data-gathering logic
and hands off to the output layer — no more per-command formatting.

Four output formats to support: human (default), terse, json, pretty.
Rules are in DD-008 §5.1–§5.4; follow them verbatim.

Files to add / extend:

  src/output/
    mod.rs        <- OutputFormat enum (Human, Terse, Json, Pretty);
                     dispatch to per-format renderers via a Renderer
                     trait
    human.rs      <- comfy-table for tables + vertical-block renderer
                     for single records; state-prefix column per
                     DD-008 §5.1 (the A/O/R/F/! logic lives here,
                     driven by per-kind StateClassifier impls)
    terse.rs      <- one record per line; --fields parsing; escape
                     embedded separator chars per DD-008 §5.2
    json.rs       <- serde_json::to_string; field names snake_case;
                     enum values lowercase
    pretty.rs     <- verbose multi-line single-record layout
  src/state_prefix.rs  <- StateClassifier trait with per-kind impls
                          producing "*AOF!" strings from properties

  src/errors.rs   <- extend NexusctlError to the full variant list
                     in DD-008 §9 (AuthDenied, Timeout, NotInteractive,
                     NexusdUnreachable, FeatureDisabled, InvalidState,
                     InvalidArgument, NotFound, AlreadyExists,
                     UnknownDevice, UnknownPairingJob, ConnectionFailed,
                     BluezUnavailable, SupplicantUnavailable,
                     NotPowered, NotPaired, ResourceBusy, IoError,
                     CryptoError, Unsupported, Other)

  src/errors_map.rs  <- translate zbus::Error (MethodError) into
                        NexusctlError per the table in DD-008 §9.
                        Pay attention to the FeatureDisabled and
                        RateLimited handling — those require Prompt
                        6.1 to have landed.

Global options to wire (add to whatever Prompt 7.1 established):
  --terse / -t
  --pretty
  --format = {human, terse, json, pretty}
  --no-color / --color <when>
  --timeout <s>
  --quiet / -q
  --no-interactive
  --config <path>

Env vars:
  NEXUSCTL_FORMAT
  NEXUSCTL_TIMEOUT
  NEXUSCTL_PSK (recognized but not yet used — Phase 7.5 activates)

Exit codes: implement the full table from DD-008 §4.3.

Terse mode:
  --fields=<comma-separated> selects columns
  --separator=<str> (default ":")
  Escape the separator in field values with backslash.
  When --fields produces a single field, emit the value with no
  separator.

Pretty mode:
  Vertical one-record layout with aligned key:values.
  Long values wrap with a hanging indent.

JSON mode:
  Every command produces a single top-level value (array or object).
  Never mix JSON with other stderr messages on stdout.
  Errors in JSON mode write to stderr as a JSON object per DD-008
  §9 example.

Tests:
- insta snapshot tests for each format × each fixture. Fixtures:
  `iface list` with four interfaces (one of each kind), `iface show
  <interface>` for each kind.
- Field selection: --fields=iface,state produces the expected
  output; single field omits separator.
- Terse escape: a Wi-Fi SSID containing ":" renders escaped.
- Error translation: every row of the DD-008 §9 table has a test
  that passes a mocked zbus MethodError into the translator and
  asserts the resulting NexusctlError, exit code, and human message.

Exit criterion: `cargo test -p nexus-client` passes including
snapshot tests. The existing `status` and `iface list` commands
now render in all four formats correctly. `nexusctl --pretty iface
list` errors sensibly (pretty is a single-record format — either
reject with usage error, or render each record as a separate
pretty block with a blank line between).
```

### Prompt 7.3 — Read-only subcommands for every domain

🟡 Depends on 7.2, and 4.1 (the daemon's read-only D-Bus
interfaces) · ⏱️ Large (~4-5 hours)

```
Implement every read-only nexusctl subcommand per /docs/dd-008-
nexusctl-client.md §4.1. No mutating operations — those land in
Phase 7.4.

Subcommands to implement:

  iface
    list [--kind <kind>]
    show <iface>
    events <iface>      <- last N events (see notes)

  eth
    list
    show <iface>

  wifi
    list
    show <iface>
    (scan comes in Phase 7.4 since scan triggers a mutating call)

  bt
    adapters
    list [--paired|--connected]
    show <address>

  gnss
    list
    show [<device>]
    satellites [<device>]

  profile
    list [--kind <kind>]
    show <ulid|name>
    export <ulid>       <- writes the TOML to stdout

  power
    get

  admin
    master-key-info     <- reads MasterKeySource property

  status (already done in 7.1, but extend to include PowerState,
          BlueZ/gpsd availability)

Each subcommand has its own file under src/commands/. Each one:
1. Resolves the interface path per DD-008 §7.2 (Manager.GetInterface
   for ifname; property walks for Bluetooth address, GNSS device
   path, profile ulid/name).
2. Reads the required properties via the zbus proxy.
3. Hands a typed record to the output layer.

Proxies to add:

  src/proxy/
    wifi.rs           <- fi.nexus.Wifi (read-only subset)
    ethernet.rs       <- fi.nexus.Ethernet
    bluetooth.rs      <- fi.nexus.Bluetooth (adapter)
    bluetooth_device.rs
    gnss.rs           <- fi.nexus.Gnss
    profile.rs        <- fi.nexus.Profile
    wifi_profile.rs
    bluetooth_profile.rs
    gnss_profile.rs
    ethernet_profile.rs

Path resolution: a src/path_resolve.rs module with fns like
resolve_interface_by_ifname, resolve_bluetooth_device_by_address,
resolve_gnss_by_device, resolve_profile_by_ref (accepts ULID or
label).

The `events` subcommand needs a bit of thought — the daemon doesn't
store historical events. Options:
  (a) Return an empty list with a note
  (b) Maintain a short ring buffer in nexusd (a new D-Bus method)
  (c) Skip the command in v1 and mark it as future-work
Pick (c) for this prompt — add the CLI parsing but have the handler
print "event history is a planned feature; use `nexusctl watch` for
live events" and exit 1. Future iterations add the ring buffer.

State-prefix column: the per-kind StateClassifier trait from Prompt
7.2 needs real implementations now. Follow DD-008 §5.1 per-kind
derivation table exactly.

Tests:
- Each command has a snapshot test against the same fixtures used
  in 7.2, extended as needed
- Multi-interface disambiguation: `wifi show` with two Wi-Fi
  interfaces and no arg errors with usage code 2 listing them

Manual verification:
- Every DD-008 §5.1 example output renders correctly (state-prefix
  column populated, truncation working)

Exit criterion: Every read-only command from DD-008 §4.1 works
against a real nexusd in all four output formats. Snapshot tests
cover all of them. `nexusctl --terse --fields=iface,state iface
list` produces output usable in shell scripts.
```

### Prompt 7.4 — Mutating commands (non-interactive path)

🟡 Depends on 7.3 and 4.2 (D-Bus mutating methods) · ⏱️ Large
(~4-5 hours)

```
Implement every mutating nexusctl subcommand that doesn't require
interactive terminal prompts per /docs/dd-008-nexusctl-client.md
§4.1. Interactive flows (pairing, PSK prompts) land in Phase 7.5.

Subcommands to implement:

  wifi
    scan [<iface>]                <- triggers Scan(), waits for
                                     ScanCompleted signal, prints
                                     results
    connect <ssid> --psk <PSK>    <- no PSK prompt yet; Phase 7.5
                                     adds interactive
    connect-profile <profile>
    disconnect [<iface>]
    forget <ssid|ulid>

  eth
    (no mutating commands — Ethernet is profile-driven per DD-002)

  bt
    power <hci> on|off            <- property write on Powered
    connect <address>
    disconnect <address>
    forget <address>
    trust <address> on|off
    scan [<hci>] [--duration N]   <- StartDiscovery + timer +
                                     StopDiscovery

  profile
    add-wifi <ssid> --psk <PSK> [opts]
    add-wifi --file <path> | -    <- reads TOML from file or stdin
    add-ethernet <iface> [opts]
    add-ethernet --file <path> | -
    import [--kind <kind>]        <- stdin or --file PATH, daemon
                                     infers kind from TOML or from
                                     --kind arg
    remove <ulid|name>
    update <ulid> --field <path> --value <value>
                                  <- for non-interactive field
                                     edits; see DD-008 §6.5 final
                                     paragraph

  power
    set <active|background|sleep>

  admin
    rotate-master-key             <- fires-and-forgets (issues
                                     RotateMasterKey, prints job_id,
                                     user can `nexusctl watch` for
                                     the MasterKeyRotated signal)
    freeze-backup                 <- prints the lease token
    release-backup <lease>
    diagnostics [--out <path>]    <- streams CollectDiagnostics's
                                     fd to a file or stdout
    reload-config                 <- DEPENDS ON Prompt 6.3 being
                                     complete; calls Manager.ReloadConfig
                                     and renders the applied/deferred/
                                     errors report

Each mutating command:
1. Performs path resolution (same as Phase 7.3)
2. May need to spawn pkttyagent — see Phase 7.5 for the full
   treatment. For now: if PolicyKit denial happens and no agent
   is registered, report the error cleanly with exit code 3 and a
   hint. Full auto-spawning lands in 7.5.
3. Issues the D-Bus call
4. For commands with async outcomes (scan, rotate-master-key),
   subscribes to the completion signal before issuing the call
   (avoids race), then waits with timeout

--psk leak warning: when --psk is used on the command line and not
via NEXUSCTL_PSK, print the DD-008 §6.2 warning to stderr. Suppress
with --no-warn-psk or NEXUSCTL_NO_WARN_PSK=1.

Exit codes continue to follow DD-008 §4.3.

Tests:
- Mock-nexusd integration tests for every mutating command
- PolicyKit-denied path: every command returns exit 3 with the
  right message
- Rate-limit path: Scan() returning RateLimited exits 1 with
  retry_after_ms in the message (requires Prompt 6.1 complete)
- FeatureDisabled path: wifi commands against a daemon with
  [wifi.enabled] = false exit 1 with "Wi-Fi backend is disabled
  in nexusd" (requires Prompt 6.1 complete)
- Profile import from stdin: `echo "$toml" | nexusctl profile
  import --kind wifi` works

Manual verification:
- Every mutating command works end-to-end against a real nexusd
- Concurrent `nexusctl wifi scan` + `nexusctl wifi scan` from two
  shells succeeds on both
- nexusctl bt power hci0 on; nexusctl bt power hci0 off; verify
  via `bluetoothctl show`

Exit criterion: Every non-interactive mutating command in DD-008
§4.1 works against a real nexusd. `cargo test -p nexus-client`
passes. `nexusctl profile add-wifi corp-net --psk foo` stores a
profile (visible in `nexusctl profile list`); without the
--no-warn-psk flag the leak warning appears on stderr.
```

### Prompt 7.5 — Interactive flows

🟡 Depends on 7.4, and 6.2 (AnswerPairingPrompt validation) ·
⏱️ Large (~5-6 hours)

```
Implement the three interactive flows per /docs/dd-008-nexusctl-client.md
§6: Bluetooth pairing, Wi-Fi passphrase entry, PolicyKit agent
spawning, plus §6.4 cancellation semantics.

The three flows:

1. Bluetooth pairing (nexusctl bt pair <address>)
   Follow DD-008 §6.1 sequence exactly, including the race-resolution
   rule for SIGINT arriving concurrently with PairingComplete.

   Factor per DD-008 §6.1 "Testable factoring" subsection:
     src/interactive/pairing.rs:
       trait Prompt: confirm, ask_passkey, ask_pin, acknowledge,
                     authorize, render_progress, render_outcome
       struct PairingFlow<P: Prompt>: holds prompt + proxy +
                     job_id + overall_timeout; has async fn run()
       struct TerminalPrompt: default impl using dialoguer + stderr
       struct MockPrompt: scripted responses for tests

   The PairingFlow.run() loop uses tokio::select! to multiplex:
     - PairingPrompt signal stream (filtered by job_id)
     - PairingComplete signal stream (filtered by job_id)
     - tokio::signal::ctrl_c()
     - tokio::time::sleep(overall_timeout)

   Answer variants follow DD-006 §6.4 exactly:
     request_confirmation, request_authorization, authorize_service
       -> answer = b (yes/no)
     request_passkey -> answer = u
     request_pin -> answer = s (PIN string)
     display_passkey, display_pin -> answer = s:"acknowledge" after
                                     the operator has seen the value
     (cancel anywhere) -> answer = s:"cancel"

   Requires Prompt 6.2 so the daemon rejects invalid variants rather
   than silently misbehaving.

2. Wi-Fi passphrase entry (nexusctl wifi connect <ssid> with no
   --psk / NEXUSCTL_PSK)
   Follow DD-008 §6.2. dialoguer::Password for the PSK prompt
   (no echo). Re-prompt up to 3 times on auth failure. Profile is
   saved only after a successful connection.

3. PolicyKit agent spawning
   Follow DD-008 §6.3. The sequence:
     a. Try to register as auth agent via
        org.freedesktop.PolicyKit1.Authority.RegisterAuthenticationAgent.
     b. On success: if auto_polkit_agent config is true, fork-exec
        pkttyagent --process $$.
     c. On AlreadyExists-style error from step (a): another agent
        is handling this subject; proceed without spawning.
     d. Run the command.
     e. Cleanup: terminate pkttyagent; UnregisterAuthenticationAgent.

   Implement this once in src/interactive/polkit.rs; every mutating
   command wraps its body in:
     let _agent = maybe_spawn_polkit_agent(&conn, &config).await?;
     // ... command body ...
     // _agent's Drop handles cleanup

Files:

  src/interactive/
    mod.rs
    pairing.rs       <- PairingFlow<P>, trait Prompt, PairingOutcome
    terminal_prompt.rs <- TerminalPrompt impl
    mock_prompt.rs     <- MockPrompt for unit tests (test-only)
    passphrase.rs    <- Wi-Fi PSK flow (simpler than pairing —
                        straight sequence, no state machine)
    polkit.rs        <- PolkitAgent guard type, maybe_spawn helper
    cancellation.rs  <- common SIGINT handling per DD-008 §6.4

Signal handling per DD-008 §6.4:
  - Read-only commands: immediate exit on SIGINT (tokio::signal
    drops the future)
  - Mutating non-interactive: 2s grace for cleanup
  - Interactive pairing: CancelPairing, 2s grace for the
    PairingComplete(reason=cancelled) to arrive
  - Double-SIGINT within 500ms: immediate termination

Exit codes:
  5 for any interactive command invoked with no TTY (detect via
    atty / is_terminal on stdin)
  130 for SIGINT-terminated commands

Tests:

  Unit tests on PairingFlow<MockPrompt>:
  - Happy path: request_confirmation with accept=true → Paired
  - Operator rejects confirmation → Failed(reason="rejected")
  - Operator takes too long (MockPrompt times out) → outer timeout
    triggers CancelPairing, outcome=TimedOut, exit 4
  - Ctrl-C before any prompt → Cancelled, exit 130
  - Ctrl-C during prompt → prompt cancelled, Cancelled, exit 130
  - PairingComplete(success=true) arrives simultaneously with
    SIGINT → outcome=Paired (race resolution per DD-008 §6.1
    step 6)

  Integration tests on TerminalPrompt: driven by an expect-style
  harness (spawn nexusctl as a subprocess, feed scripted stdin).

  Manual:
  - Pair with a real device showing numeric comparison. Verify the
    passkey display matches what nexusctl shows; confirm; pair
    completes.
  - Pair with a device requiring 4-digit PIN; type the PIN at the
    prompt; pairing completes.
  - Ctrl-C mid-pairing; verify CancelPairing is issued and
    PairingComplete(reason=cancelled) arrives before nexusctl
    exits 130.

Exit criterion: Pairing against at least two real Bluetooth device
types (one with numeric comparison, one with PIN) works
end-to-end. Wi-Fi connect with interactive PSK prompt works.
`nexusctl profile add-wifi` without --psk and without a TTY exits
5 with the correct message. `cargo test -p nexus-client --features
interactive-flows-testing` passes including the MockPrompt-driven
tests.
```

### Prompt 7.6 — `watch` command and signal subscription

🟡 Depends on 7.3 · ⏱️ Large (~3-4 hours)

```
Implement the `watch` command per /docs/dd-008-nexusctl-client.md
§7.4. This is the single largest signal-handling chunk in nexusctl.

Subcommands:
  nexusctl watch            <- subscribes to all event kinds below
  nexusctl watch events     <- same as above
  nexusctl watch iface
  nexusctl watch wifi
  nexusctl watch bt
  nexusctl watch gnss

Signal synthesis: follow the 16-row table in DD-008 §7.4 exactly.
Each row maps (D-Bus signal, optional filter condition) to
(watch event kind, output fields).

Implementation:

  src/commands/watch.rs:
    fn run(filters: Vec<Filter>, format: OutputFormat, subset: WatchSubset)
      -> Result<()>

    Subscribe via the relevant zbus proxies (Manager,
    ObjectManager for arrivals/departures, per-interface proxies
    for StateChanged) and tokio::select! over the merged streams.

    For each arriving signal, translate to a WatchEvent with
    fields listed in DD-008 §7.4 table. Emit via the output layer.

  src/watch/
    event.rs         <- WatchEvent type; serde serialization (flat
                        key-value pairs, not nested)
    filter.rs        <- Filter struct (field, glob); globset crate
                        for the wildcard matching; AND semantics
                        across multiple --filter flags
    subscribe.rs     <- subscription setup per subset (iface/wifi/
                        bt/gnss/events)

Flat dict rule (per DD-008 §7.4): NotificationEvent.data is
a{sv}; flatten its keys into the top-level WatchEvent.

Output:
  - human: columns auto-align; ANSI color for event severity (green
    for healthy transitions, yellow for transient, red for errors)
  - terse: whitespace-separated fields per the event row
  - json: one JSON object per line (NDJSON-style)
  - pretty: not supported for watch (pretty is single-record);
    --pretty with watch issues a warning and falls back to human

Filters:
  --filter 'field=glob'   (may repeat)
  AND semantics: all filters must match
  Shell-glob syntax (*, ?, [abc])
  Fields filterable: kind, iface, state, address, adapter,
    subsystem, and any other top-level WatchEvent key

Ctrl-C: clean exit code 0.
Connection loss: clean exit code 0 with "nexusd disconnected" to
stderr. No automatic reconnection.

Tests:
- Mock D-Bus fires each signal kind from the synthesis table;
  verify the expected WatchEvent kind, fields, and JSON output
- Filter tests: every operator combination (single filter, two
  filters with AND, wildcard match, no match)
- Termination tests: Ctrl-C exits 0; simulated disconnect exits 0
- Watch subsets: `watch wifi` receives only wifi-* events

Manual verification:
- `nexusctl watch` then a separate shell: `nexusctl wifi scan`;
  verify wifi-scan event appears
- `nexusctl --json watch events` produces valid NDJSON (each line
  parses independently with jq)
- `nexusctl watch --filter 'iface=eth0' --filter 'kind=link-*'`
  narrows correctly

Exit criterion: All 16 rows in DD-008 §7.4's synthesis table are
covered by tests. Against a real nexusd, triggering each kind of
event (via corresponding mutations) produces the expected watch
output in all three applicable formats.
```

### Prompt 7.7 — `shell` REPL

🟡 Depends on 7.6 · ⏱️ Medium (~3 hours)

```
Implement the `nexusctl shell` interactive REPL per DD-008 §4.1
and §12 Phase 7.

The REPL is a secondary mode of operation (the primary being
one-shot commands). It holds a single D-Bus connection across
command invocations and adds tab completion inside the shell.

Dependencies: rustyline (v14+) for readline + history + completion.

Files:
  src/commands/shell.rs         <- Repl struct, run loop

Behavior:

  $ nexusctl shell
  nexusctl> status
  [output of `nexusctl status`]
  nexusctl> iface list
  [output]
  nexusctl> help
  [list of subcommands]
  nexusctl> wifi connect corp-net
  [interactive PSK flow]
  nexusctl> exit
  $

Rules:
1. Prompt: "nexusctl> " when not in any sub-context.
2. Each command is parsed through the same clap CLI struct as
   one-shot mode, but with the initial "nexusctl " stripped.
3. The D-Bus connection and any stateful resources (e.g., a
   spawned pkttyagent) persist across commands within the shell
   session.
4. `exit`, `quit`, Ctrl-D terminate the shell (exit 0).
5. Ctrl-C during a command cancels the command (per Phase 7.5
   cancellation semantics); does NOT exit the shell.
6. History: ~/.cache/nexusctl/history (fallback: $HOME) with a
   1024-entry cap.
7. Tab completion: for subcommands, complete from the clap tree.
   For argument values (interface names, Bluetooth addresses,
   etc.), query nexusd live with a 500ms timeout; fall back to
   no completion if slow.
8. Interactive flows within the REPL work exactly as in one-shot
   mode — pairing prompts, PSK entry, etc.

Tab-completion helpers: the same path-resolution module from
Prompt 7.3 provides the candidate lists (resolve_interface_by_ifname
— but now listing all ifnames; similar for Bluetooth addresses,
profile labels). Cache the completion candidates for 2s within
the shell to avoid hammering D-Bus on every Tab.

Tests:
- REPL parse: every subcommand accepted with and without the
  "nexusctl" prefix (both should work)
- History persistence: exit, re-enter shell, up-arrow recalls
  last command
- Tab completion: fixture nexusd with known interfaces; Tab after
  "wifi show " produces the Wi-Fi interface names

Manual verification:
- `nexusctl shell` drops into a working prompt; all one-shot
  commands work identically inside; interactive pairing flows
  work; Ctrl-C cancels commands; Ctrl-D exits cleanly; history
  survives between sessions.

Exit criterion: REPL works against a real nexusd. `nexusctl
shell` followed by 10 assorted commands including one pairing
flow produces identical results to running each command in a
fresh one-shot nexusctl invocation.
```

### Prompt 7.8 — Shell completion and polish

🟡 Depends on 7.7 · ⏱️ Medium (~2-3 hours)

```
Add shell completion script generation plus final polish per
DD-008 §10, §12 Phase 8.

Files:
  src/commands/completions.rs   <- `nexusctl completions <shell>`
  src/completion/
    static.rs     <- clap_complete-generated static completion
                     (subcommands, flags)
    dynamic.rs    <- dynamic candidate helpers (interface names,
                     Bluetooth addresses, profile labels) —
                     factored out of Prompt 7.7's REPL completion

Supported shells: bash, zsh, fish, powershell (pwsh).

Static completion: clap_complete produces this directly from the
CLI struct. `nexusctl completions bash > /etc/bash_completion.d/
nexusctl` is the install flow.

Dynamic completion is trickier because bash/zsh/fish completion
scripts run in the shell, not in nexusctl. The pattern: emit
completion scripts that, when an argument that needs dynamic
completion is being completed, call back into `nexusctl --terse
--fields=...` to get the candidate list. This is how nmcli does
it. Implement in each shell's completion output.

Example for bash: when completing `nexusctl wifi show <TAB>`, the
completion script runs:

  compreply=$(nexusctl --terse --fields=iface iface list --kind wireless)

with a 1s timeout (via `timeout` coreutil) so slow daemons don't
freeze the shell.

Install path documentation: per DD-008 §10 table. nexusctl doesn't
install the scripts itself — packagers redirect output in their
post-install.

Final polish:
- Man pages generated from clap help, shipped under
  packaging/man/nexusctl.1 (use clap_mangen).
- --help output reviewed end-to-end for clarity (every subcommand
  has a descriptive help string and at least one example in its
  long help).
- All error messages end without trailing punctuation/newline
  (let the terminal handle that) and start lowercase (Unix
  convention).
- Exit codes verified against DD-008 §4.3 for every documented
  failure path.

Tests:
- `nexusctl completions bash | bash -n` parses without errors
  (bash syntax check)
- zsh and fish similar syntax checks
- Dynamic completion: fixture shell invocation calls the stubbed
  `nexusctl --terse ...` command and the completion list is right

Manual verification on a clean VM:
- Install bash completion to /etc/bash_completion.d/nexusctl
- New shell → Tab-complete `nexusctl w<TAB>` → offers wifi, watch
- `nexusctl wifi show <TAB>` → offers current Wi-Fi interface
  names by ifname
- `nexusctl bt pair <TAB>` → offers known Bluetooth addresses
- E2E tests from the VM pass

Exit criterion: All four shells get working completion. Man page
renders with `man -l packaging/man/nexusctl.1`. Every subcommand
documented in DD-008 §4.1 has a non-trivial --help entry. CI
includes a full `nexusctl shell`-driven integration test against
a VM-hosted nexusd.
```

---

## Tips for running this series

**When a prompt feels too large.** Break it at a sensible phase boundary — the DD phases are numbered deliberately and each has its own exit criterion. Use the sub-phase number in the commit messages so you can track which prompt you ran.

**When Claude Code drifts from the DD.** Stop the session. Re-paste the prompt with a `IMPORTANT: read /docs/dd-NNN.md §X.Y before starting` line at the top. Claude Code is less likely to wing it if the doc reference is prominent.

**When tests fail.** Let Claude Code iterate on them, but cap at 3 iterations. If it can't fix it in 3 tries, the prompt was ambiguous — rewrite the prompt with the specific failure mode called out.

**When you want to skip ahead.** 3a, 3b-3c, 3d-3e, and 3f are truly independent. If you only care about Wi-Fi right now, skip the others. If you want to demo Bluetooth first, do 3d-3e after Phase 0-1 and skip the rest.

**Phase 6 vs Phase 7 scheduling.** Phase 6's three prompts are daemon-side additions. Phase 7's first three (7.1–7.3) are nexusctl read-only work that doesn't touch the affected D-Bus surface, so those can start in parallel with Phase 6. Phase 7.4 (mutating commands) depends on Phase 6.1 for the `FeatureDisabled`/`RateLimited` errors; Phase 7.4 also benefits from Phase 6.3 if you want `reload-config` to work end-to-end. Phase 7.5 (interactive pairing) depends on Phase 6.2 for the tightened variant validation — otherwise the daemon will silently mishandle bad answers. Practical ordering:

1. Run 6.1, 6.2, 7.1, 7.2 in whichever order suits. Stable and independent.
2. Run 7.3 after 7.2.
3. Run 6.3 in parallel with 7.3 or 7.4 — they don't share files.
4. Run 7.4 after 6.1 + 7.3.
5. Run 7.5 after 6.2 + 7.4.
6. Run 7.6–7.8 in order after 7.5.

**For integration tests requiring hardware.** Build a tiny test-rig: VM with virtual Ethernet, wpa_supplicant linked against a virtual Wi-Fi radio (mac80211_hwsim kernel module), BlueZ with a USB dongle, and a gpsfake setup. This is a one-time setup; once working, every integration-test prompt just points at the rig. Pairing tests in Prompt 7.5 additionally need at least two real Bluetooth devices with different pairing methods (numeric comparison + PIN entry) — plan the rig with this in mind.

**Commit discipline.** Every prompt says "commit each step atomically." Enforce this — when reviewing Claude Code's output, look at `git log` to verify the commits are logical units, not one giant dump.

---

## Related documents

- [nexus-architecture.md](./nexus-architecture.md) — overall system design
- [CLAUDE.md](./CLAUDE.md) — conventions every Claude Code session must respect
- [DESIGN-DOCS.md](./DESIGN-DOCS.md) — how the DDs themselves are written
- DD-001 through DD-008 — per-subsystem specs that prompts above reference
