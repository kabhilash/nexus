# DD-006: D-Bus API — Detailed Design

**Parent:** [Nexus Architecture](./nexus-architecture.md)
**Depends on:** [DD-001](./dd-001-interface-discovery.md), [DD-002](./dd-002-ethernet-backend.md), [DD-003](./dd-003-wifi-backend.md), [DD-007](./dd-007-profile-store.md)
**Status:** Draft
**Scope:** Design of Nexus's external D-Bus API — the contract through which CLI tools, UIs, fleet-management agents, and other local services interact with the daemon. Covers object hierarchy, methods, properties, signals, authorization, versioning, and error semantics.

---

## Table of Contents

1. [Context](#1-context)
   - 1.1 [Repo Layout](#11-repo-layout)
2. [Responsibilities](#2-responsibilities)
3. [API Design Principles](#3-api-design-principles)
   - 3.1 [Fresh Design vs ConnMan Compatibility](#31-fresh-design-vs-connman-compatibility)
   - 3.2 [Naming](#32-naming)
   - 3.3 [Versioning](#33-versioning)
4. [Object Hierarchy](#4-object-hierarchy)
5. [Manager Interface](#5-manager-interface)
   - 5.1 [Properties](#51-properties)
   - 5.2 [Methods](#52-methods)
   - 5.3 [Signals](#53-signals)
6. [Interface Objects](#6-interface-objects)
   - 6.1 [Common Interface](#61-common-interface)
   - 6.2 [Ethernet Interface](#62-ethernet-interface)
   - 6.3 [Wi-Fi Interface](#63-wi-fi-interface)
   - 6.4 [Bluetooth Interface](#64-bluetooth-interface)
   - 6.5 [GNSS Interface](#65-gnss-interface)
   - 6.6 [BluetoothDevice Interface](#66-bluetoothdevice-interface)
7. [Profile Objects](#7-profile-objects)
   - 7.1 [Common Profile Interface](#71-common-profile-interface)
   - 7.2 [Wi-Fi Profile](#72-wi-fi-profile)
   - 7.3 [Ethernet Profile](#73-ethernet-profile)
8. [Scan Result Objects](#8-scan-result-objects)
9. [Signals](#9-signals)
10. [Authorization (PolicyKit)](#10-authorization-polkit)
    - 10.1 [Policy Actions](#101-policy-actions)
    - 10.2 [Authorization Check Flow](#102-authorization-check-flow)
    - 10.3 [Agent-less Deployments](#103-agent-less-deployments)
11. [Error Semantics](#11-error-semantics)
    - 11.1 [Error Namespace](#111-error-namespace)
    - 11.2 [Error Payloads](#112-error-payloads)
    - 11.3 [Backpressure vs Error](#113-backpressure-vs-error)
12. [Property Change Semantics](#12-property-change-semantics)
    - 12.1 [PropertiesChanged](#121-propertieschanged)
    - 12.2 [Coalescing](#122-coalescing)
    - 12.3 [Atomicity](#123-atomicity)
13. [Introspection and Discoverability](#13-introspection-and-discoverability)
14. [Service Activation and Readiness](#14-service-activation-and-readiness)
    - 14.1 [Service Activation](#service-activation)
    - 14.2 [Readiness and Backend Initialization](#readiness-and-backend-initialization)
15. [Rate Limiting and Backpressure](#15-rate-limiting-and-backpressure)
16. [Example Flows](#16-example-flows)
    - 16.1 [Connect to a Known Wi-Fi Network](#161-connect-to-a-known-wi-fi-network)
    - 16.2 [Add a New Wi-Fi Profile](#162-add-a-new-wi-fi-profile)
    - 16.3 [Scan and Enumerate Results](#163-scan-and-enumerate-results)
17. [Testing Strategy](#17-testing-strategy)
18. [Implementation Phases](#18-implementation-phases)

---

## 1. Context

Nexus's D-Bus API is how every external component talks to the daemon — the CLI tool, the operator-facing UI, cloud fleet agents, custom integrations. Everything that isn't an internal Rust call happens here. This makes the API a long-lived commitment. Once shipped and depended on, changes are expensive because other software will be built against it.

The API needs to:

- Cover the full operational surface: connect, disconnect, scan, add/remove profiles, monitor state.
- Stay comprehensible to humans. A developer browsing `d-feet` should be able to figure out how to connect to a Wi-Fi network in under 5 minutes.
- Support both synchronous operations (get a property, add a profile) and asynchronous ones (wait for scan results, watch connection state).
- Integrate with Linux conventions: PolicyKit for authorization, structured errors, introspection XML, systemd service activation.
- Be stable across Nexus versions, or fail loudly when breaking.

This doc defines every object path, method, property, and signal that Nexus exposes, plus the cross-cutting concerns (auth, errors, versioning). The technology backends (DD-002, DD-003, future DD-004) produce the data; this layer serializes it onto the bus.

### 1.1 Repo Layout

The code for this component lives at:

```
crates/
  nexus-dbus/                   <- D-Bus service layer
    Cargo.toml
    src/
      lib.rs                    <- entry point (spawn_dbus_service)
      service.rs                <- DbusService top-level orchestrator
      manager.rs                <- fi.nexus.Manager interface
      interfaces/               <- per-technology interface objects
        mod.rs
        common.rs               <- fi.nexus.Interface (shared base)
        ethernet.rs             <- fi.nexus.Ethernet
        wifi.rs                 <- fi.nexus.Wifi
        bluetooth.rs            <- fi.nexus.Bluetooth (future)
      profiles/                 <- fi.nexus.Profile.* interfaces
        mod.rs
        ethernet.rs
        wifi.rs
      scan_results.rs           <- fi.nexus.ScanResult (per-BSS)
      signals.rs                <- signal emission helpers
      authz.rs                  <- PolicyKit integration
      errors.rs                 <- D-Bus error mapping
      properties.rs             <- property-change coalescing
    tests/
      introspection.rs          <- validates XML matches implementation
      policykit.rs              <- mocked PolicyKit tests
```

`nexus-dbus` consumes the event bus (`NexusEvent`) and holds references to the technology backends' command channels. It does not own any connection state of its own — it is a translation layer between the bus and D-Bus clients.

---

## 2. Responsibilities

The D-Bus service layer is responsible for:

1. Owning the `fi.nexus1` bus name on the system bus.
2. Exposing the object hierarchy (§4) — manager, per-interface objects, per-profile objects, per-scan-result objects.
3. Translating backend events into D-Bus signals and property updates.
4. Translating D-Bus method calls into backend commands via the existing `mpsc::Sender<BackendCommand>` channels each backend provides.
5. Enforcing authorization via PolicyKit (§10) before any mutating operation.
6. Coalescing rapid property changes to avoid signal storms (§12).
7. Providing introspection XML for every object path.
8. Returning structured, well-named D-Bus errors (§11).

The D-Bus service layer is explicitly **not** responsible for:

- Implementing connection logic — that's in the technology backends.
- Persisting profiles — that's the Profile Store (DD-007). This layer invokes its CRUD methods.
- Enforcing rate limits on actual operations — the backends handle retry storms internally; D-Bus rate limiting (§15) protects the bus itself from abusive clients.

---

## 3. API Design Principles

### 3.1 Fresh Design vs ConnMan Compatibility

**Decision: fresh design. Not ConnMan compatible.**

ConnMan has a mature D-Bus API with broad tooling support. Reusing it would let existing `connmanctl`-style tools work without changes. Evaluated and rejected for three reasons:

- **WPA3 and modern Wi-Fi.** ConnMan's security model predates WPA3, SAE, OWE, and PMF. Retrofitting them into ConnMan's flat "security = wpa2" string would require either breaking the API or tunneling modern fields through vendor extensions — either way not actually compatible.
- **Credential callbacks.** Enterprise network onboarding needs interactive credential prompts ("your certificate is expiring; paste the renewal"). ConnMan's `Agent` API works but was designed for a desktop PolicyKit-less world. A PolicyKit-based authorization model (§10) works better for embedded fleet deployments.
- **Fleet operations.** Nexus targets embedded fleet deployment, where common operations include "apply this set of profiles," "rotate credentials across N devices," "collect diagnostic snapshots." None of these have good ConnMan analogs.

Rejected alternatives: NetworkManager compatibility (same problems, plus a larger and more desktop-oriented surface); iwd-style API (good but too Wi-Fi-specific for Nexus's multi-technology role).

The cost: existing ConnMan tooling doesn't work. Mitigation: a `nexusctl` CLI tool that covers the same use cases, plus clear documentation targeted at porting.

### 3.2 Naming

- **Bus name:** `fi.nexus1` (system bus). The `1` suffix indicates the API version (§3.3).
- **Object path root:** `/fi/nexus1`.
- **Interface names:** `fi.nexus.Manager`, `fi.nexus.Interface`, `fi.nexus.Ethernet`, `fi.nexus.Wifi`, `fi.nexus.Profile.Wifi`, etc. Dot-separated, `PascalCase` components, no version suffix on individual interfaces (the bus name carries the version).
- **Method names:** `PascalCase` verbs or verb phrases: `Scan`, `Connect`, `GetProfile`, `ListInterfaces`.
- **Property names:** `PascalCase` nouns: `Ssid`, `SignalDbm`, `State`.
- **Signal names:** `PascalCase` past-tense or state-change phrasing: `InterfaceAdded`, `StateChanged`, `ScanCompleted`.

These follow D-Bus conventions used by BlueZ, systemd-networkd, and NetworkManager — familiar to anyone who has written D-Bus client code on Linux.

### 3.3 Versioning

The API version is encoded in the bus name (`fi.nexus1`). A breaking change produces `fi.nexus2`, which can coexist with `fi.nexus1` so clients can migrate gradually.

Within an API version:

- **Additive changes** (new methods, new properties, new signals, new enum values) are always allowed. Clients must gracefully handle unknown enum values they receive.
- **Removing** a method, property, or signal requires a major version bump.
- **Changing** the signature or semantics of an existing method requires a major version bump.
- **Deprecating** is allowed: a deprecated method remains callable but emits a warning log on the server side. The next major version removes it.

Properties have a monotonic `ApiCapabilities` array on the Manager (§5) so clients can feature-detect without bumping the bus name for every additive change.

---

## 4. Object Hierarchy

```
/fi/nexus1                                   Manager
/fi/nexus1/interface/<ifname>                one Interface per net/BT adapter/GNSS device
/fi/nexus1/interface/<ifname>/scan_result/<bssid>    one per cached BSS (Wi-Fi only)
/fi/nexus1/profile/ethernet/<ulid>           one per stored Ethernet profile
/fi/nexus1/profile/wifi/<ulid>               one per stored Wi-Fi profile
/fi/nexus1/profile/bluetooth/<ulid>          one per stored BT pairing (future)
```

Object paths:

- **`/fi/nexus1`** is the root Manager object. Clients start here.
- **`/fi/nexus1/interface/<ifname>`** — escaped interface name. The ifname is escaped to the D-Bus path character set `[A-Za-z0-9_]` by replacing `-` with `_` and percent-escaping any other character as `_XX` (hex). For example, `wlan-foo` becomes `wlan_foo`; the unusual `eth0:1` becomes `eth0_3a1`. ifname is chosen over ifindex because D-Bus paths are strings and ifnames are human-meaningful.
- **`/fi/nexus1/interface/<ifname>/scan_result/<bssid>`** — BSSID encoded as lowercase hex without separators, e.g., `aabbccddeeff`. Only exists for Wi-Fi interfaces with cached scan results.
- **`/fi/nexus1/profile/<kind>/<ulid>`** — ULID is the profile ID from DD-007. ULIDs are 26 characters, URL-safe, stable across any field edit including SSID renames.

**Profile uniqueness.** At most one Wi-Fi profile exists per SSID (see DD-007 §3.2 — the SSID hash is the on-disk filename, and a second profile with the same SSID would collide). Clients adding a second profile for an already-stored SSID get `fi.nexus.Error.AlreadyExists` unless they use `Profile.Update` to modify the existing one. Similarly, at most one Ethernet profile exists per `ifname`. Multi-security-type networks (WPA2/WPA3 transition, for example) are covered by a single profile using the `Wpa2Wpa3Personal` or equivalent `SecurityConfig` variant.

All objects implement `org.freedesktop.DBus.Properties` and `org.freedesktop.DBus.Introspectable` as standard.

The Manager additionally implements `org.freedesktop.DBus.ObjectManager` so clients can enumerate all objects in one call and receive `InterfacesAdded` / `InterfacesRemoved` signals for dynamic changes (new interfaces appearing, profiles being deleted). **ObjectManager is the canonical source of truth for the current object set.** Convenience properties like `Manager.WifiProfiles` and `Wifi.ScanResults` reflect the same information but may momentarily lag ObjectManager during concurrent modifications — subscribe to ObjectManager if precise ordering matters.

---

## 5. Manager Interface

**Interface:** `fi.nexus.Manager`
**Object path:** `/fi/nexus1`

### 5.1 Properties

| Name | Type | Access | Meaning |
|---|---|---|---|
| `Version` | `s` | read | Semantic version string of the running Nexus daemon, e.g., `"0.3.1"` |
| `ApiCapabilities` | `as` | read | Monotonic list of capability tokens for feature-detection. Tokens are additive per API version |
| `PowerState` | `s` | read/write | `"active"` \| `"background"` \| `"sleep"` |
| `Interfaces` | `ao` | read | Array of object paths for all registered interfaces |
| `EthernetProfiles` | `ao` | read | Array of object paths for stored Ethernet profiles |
| `WifiProfiles` | `ao` | read | Array of object paths for stored Wi-Fi profiles |
| `MasterKeySource` | `s` | read | `"tpm"` \| `"keyring"` \| `"file"` — active source per DD-007 §4.2 |

Example `ApiCapabilities` tokens: `"wifi.wpa3"`, `"wifi.owe"`, `"eth.dot1x"`, `"profile.rotate"`. Clients check for tokens rather than comparing versions.

### 5.2 Methods

```
GetInterface(ifname: s) -> (path: o)
    Returns the object path for an interface by name. Convenience wrapper
    around ObjectManager enumeration.
    Errors: fi.nexus.Error.NotFound

FindWifiProfile(ssid: ay) -> (path: o)
    Returns the profile object path for a given SSID. There is at most one
    profile per SSID (see §4 Profile uniqueness), so this returns a unique
    path or NotFound.
    SSID is a byte array to handle non-UTF-8 names correctly.
    Errors: fi.nexus.Error.NotFound

AddWifiProfile(settings: a{sv}) -> (path: o)
    Create a new Wi-Fi profile from a settings dict.
    Settings dict keys are detailed in §7.
    Returns the path of the newly-created profile object.
    Credentials in the dict (passphrase, EAP password) are treated as
    cleartext here and encrypted by the Profile Store on write.
    If a profile already exists for the given SSID, returns AlreadyExists —
    use Profile.Update on the existing profile to modify it.
    Errors: fi.nexus.Error.InvalidArgument, fi.nexus.Error.AlreadyExists, fi.nexus.Error.AuthFailed

AddEthernetProfile(settings: a{sv}) -> (path: o)
    Same as AddWifiProfile for Ethernet. Uniqueness is per ifname — a second
    profile for the same interface returns AlreadyExists.
    Errors: fi.nexus.Error.InvalidArgument, fi.nexus.Error.AlreadyExists, fi.nexus.Error.AuthFailed

RemoveProfile(path: o) -> ()
    Remove a profile. The backend is notified and any active connection
    using this profile is disconnected.
    Errors: fi.nexus.Error.NotFound, fi.nexus.Error.AuthFailed

SetPowerState(state: s) -> ()
    Change the global power state. state must be one of the
    values listed in the PowerState property.
    Errors: fi.nexus.Error.InvalidArgument, fi.nexus.Error.AuthFailed

RotateMasterKey() -> (job_id: s)
    Start a master-key rotation. Returns immediately with a job ID.
    Completion is signaled via Manager.MasterKeyRotated(job_id, report).
    Rotation duration is proportional to profile count; expect ~10 ms
    per profile plus ~100 ms fixed overhead. Starting another rotation
    while one is in progress returns ResourceBusy.
    Errors: fi.nexus.Error.ResourceBusy, fi.nexus.Error.AuthFailed

FreezeForBackup() -> (lease: s)
    Acquire a backup lease to prevent profile writes. Lease expires
    after 60 seconds if not explicitly released.
    Returns a lease token: a UUID v4 rendered as 36-character hex with
    hyphens (e.g., "550e8400-e29b-41d4-a716-446655440000"). 122 bits of
    entropy — not guessable in any practical sense. The server stores
    the active token in memory; at most one lease is outstanding at a
    time, and Freeze returns ResourceBusy when another lease is held.
    Errors: fi.nexus.Error.ResourceBusy, fi.nexus.Error.AuthFailed

ReleaseBackupLease(lease: s) -> ()
    Release a previously-acquired backup lease. Idempotent.
    The server compares the supplied token against the current active
    token byte-for-byte; mismatches return NotFound rather than releasing
    (so a stale or guessed token can't release someone else's lease).
    Releasing an already-expired lease also returns NotFound — this is
    safe for scripts that always call Release regardless of Freeze outcome.
    Errors: fi.nexus.Error.NotFound

CollectDiagnostics() -> (bundle_fd: h)
    Produce a diagnostic bundle (structured tarball) for support.
    Returns a readable file descriptor the caller reads until EOF.
    Using a file descriptor rather than a byte array avoids D-Bus
    message-size limits for large bundles. Credentials are redacted.
    Typical bundle size is 10-100 KB; can be several MB with verbose
    logs enabled.
    Errors: fi.nexus.Error.AuthFailed, fi.nexus.Error.IoError

ReloadConfig() -> (report: a{sv})
    Re-read /etc/nexus/nexus.toml (or the path passed to nexusd via
    --config) and apply those settings that are safely re-applicable
    at runtime. The report dict contains:
      "applied": as — config sections whose new values took effect
                      (e.g., ["wifi.scan", "bluetooth.power",
                      "gnss.emit"])
      "deferred": as — sections whose changes require a restart to
                       take effect (e.g., ["dbus.bus_name"] would be
                       listed here if a bus-name change were
                       attempted; restart-requiring changes are
                       ignored at reload time, not refused)
      "errors": a(ss) — (section, reason) pairs for sections that
                        failed to reload due to invalid values; the
                        daemon continues with the previous values
                        for those sections
    Safe-to-reload sections include backend timeouts, scan cadences,
    power-state defaults, metric exposure. Unsafe sections include
    the D-Bus bus name, the profile-store root path, and the
    master-key source — changing these requires a daemon restart.
    Errors: fi.nexus.Error.AuthFailed, fi.nexus.Error.IoError
            (config file unreadable)
```

### 5.3 Signals

`InterfacesAdded` and `InterfacesRemoved` are inherited from `org.freedesktop.DBus.ObjectManager` and fire whenever the Manager's child objects change.

Additional Manager-level signals:

```
PowerStateChanged(state: s)
    Fired when PowerState changes (whether via SetPowerState or internal trigger).

NotificationEvent(kind: s, data: a{sv})
    Operator-facing notifications that don't fit a specific interface.
    Defined kinds (the set is extensible; clients handle unknown kinds
    by displaying the data dict as-is, since keys are human-readable):
      "credentials_invalid"      — a profile needs fresh credentials.
                                   data: { profile: o, ssid: ay (wifi only) }
      "auth_backend_unavailable" — wpa_supplicant / ead / iwd went away.
                                   data: { backend: s, affected_interfaces: ao }
      "subsystem_unavailable"    — an external subsystem (gpsd, BlueZ, etc.)
                                   has been unreachable longer than its
                                   configured outage threshold.
                                   data: { subsystem: s, duration_s: u }
      "profile_corrupt"          — a profile file failed to parse or decrypt.
                                   data: { kind: s, key: s, reason: s }
      "master_key_degraded"      — profile store running without decryption.
                                   data: { source: s, reason: s }
      "subsystem_recovered"      — a previously-degraded subsystem came back.
                                   data: { subsystem: s }

MasterKeyRotated(job_id: s, report: a{sv})
    Fired when a RotateMasterKey job completes (successfully or not).
    report dict contains "outcome" (s: "success"|"failed"),
    "profiles_rewritten" (u), "duration_ms" (t), and on failure "error" (s).
```

---

## 6. Interface Objects

### 6.1 Common Interface

**Interface:** `fi.nexus.Interface`
Implemented by every `/fi/nexus1/interface/<ifname>` object regardless of technology.

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `Ifname` | `s` | read | Kernel interface name, e.g., `"eth0"`, `"wlp2s0"`, `"hci0"` |
| `Ifindex` | `u` | read | Kernel ifindex (per DD-001 §5.5; synthesized for non-network subsystems) |
| `Mac` | `ay` | read | Hardware address (6 bytes for Ethernet/Wi-Fi/BT, empty for GNSS) |
| `Kind` | `s` | read | `"ethernet"` \| `"wifi"` \| `"bluetooth"` \| `"gnss"` |
| `OperState` | `s` | read | Current operstate: `"up"`, `"down"`, `"dormant"`, etc. (per DD-001 §5.5) |
| `Carrier` | `b` | read | Link-layer carrier present |
| `ManagedProfile` | `o` | read | Object path of the profile currently in effect, or `"/"` if none |

Depending on `Kind`, the object additionally implements one of the technology-specific interfaces below.

### 6.2 Ethernet Interface

**Interface:** `fi.nexus.Ethernet`
Added when `Kind == "ethernet"`.

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `State` | `s` | read | `"waiting_carrier"` \| `"link_ready"` \| `"authenticating"` \| `"authenticated"` \| `"auth_failed"` |
| `AuthBackend` | `s` | read | `"wpa_supplicant"` \| `"ead"` \| `"none"` — active auth backend for this interface |
| `AuthFailureReason` | `s` | read | On `auth_failed`, one of `"bad_credentials"`, `"server_unreachable"`, `"certificate_rejected"`, `"timeout"`, `"other"`. Empty string otherwise |
| `EapMethod` | `s` | read | EAP method of the active auth attempt (e.g., `"TLS"`, `"PEAP"`). Empty when not authenticating |

State-context detail (reason codes, EAP method, etc.) is emitted in the `StateChanged` signal's `details` dict (§9), matching the Wi-Fi pattern — no tuple-typed `AuthState` property.

**Methods:** None. Ethernet is reactive to carrier events; there's nothing to tell it to do. Profile changes (which *implicitly* change behavior) happen via `Manager.AddEthernetProfile` etc.

### 6.3 Wi-Fi Interface

**Interface:** `fi.nexus.Wifi`
Added when `Kind == "wifi"`.

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `State` | `s` | read | Per DD-003 §3.1: `"idle"` \| `"scanning"` \| `"connecting"` \| `"authenticating"` \| `"handshaking"` \| `"connected"` \| `"roaming"` \| `"disconnected"` |
| `ConnectedBss` | `(sayayuis)` | read | Tuple of `(ssid_utf8_lossy: s, ssid_bytes: ay, bssid: ay, frequency: u, signal_dbm: i, security: s)`. When not connected, all fields are default-valued — `ssid_utf8_lossy` is empty, `ssid_bytes` and `bssid` are empty arrays, `frequency` and `signal_dbm` are `0`. Clients should check `State` rather than inferring connection from this property. BSSIDs are always 6-byte binary throughout the API; string rendering happens only on the client side |

The sentinel tuple is a D-Bus-layer construct — the Wi-Fi Backend's state machine (DD-003 §3.1) has no "zero Connected" variant; `Connected` is only present when actually connected. The D-Bus layer manufactures the all-empty tuple when the backend is in any non-`Connected` state and publishes it as the current property value so clients always see a well-formed `(sayayuis)` rather than an error.
| `SignalDbm` | `i` | read | Most recent RSSI reading, or `0` if not connected |
| `Frequency` | `u` | read | Center frequency in MHz of the connected BSS, or `0` if not connected |
| `ScanResults` | `ao` | read | Array of scan-result object paths (§8) |
| `Supplicant` | `s` | read | `"wpa_supplicant"` \| `"iwd"` |
| `RoamingMode` | `s` | read/write | `"off"` \| `"supplicant"` \| `"nexus"` |
| `Powered` | `b` | read/write | Whether rfkill is released for this interface |

**Methods:**

```
Scan(params: a{sv}) -> ()
    Trigger a scan. params may include:
      "active"        (b)  active probing (default true)
      "ssids"         (aay) specific SSIDs to probe (default empty)
      "frequencies"   (au) specific frequencies in MHz (default all)
      "allow_roam"    (b)  whether the supplicant may autonomously roam based
                           on scan results (default false; ignored unless
                           RoamingMode == "supplicant"). See DD-003 §5.
    Scan completes asynchronously; clients watch ScanCompleted signal.
    Clients receiving ResourceBusy should wait at least 2 seconds before
    retrying (DD-003 §6.3 minimum attempt interval). RateLimited uses the
    `retry_after_ms` hint it carries.
    Errors: fi.nexus.Error.InvalidArgument, fi.nexus.Error.AuthFailed,
            fi.nexus.Error.ResourceBusy, fi.nexus.Error.RateLimited,
            fi.nexus.Error.FeatureDisabled

Connect(profile: o) -> ()
    Connect to the given Wi-Fi profile. The profile path must be under
    /fi/nexus1/profile/wifi/; passing an Ethernet profile path returns
    InvalidArgument. Connecting bypasses the profile's auto_connect flag —
    an operator-requested connection is treated as an explicit override
    for the current session.

    Lifecycle after explicit Connect on a non-auto-connect profile: the
    backend treats the profile as "sticky" until explicit Disconnect or
    until the interface experiences a permanent Disconnected (e.g.
    CredentialsInvalid). It will NOT spontaneously deselect the profile
    on the next scan tick just because auto_connect is false. If signal
    degrades and the profile matches no nearby BSS, the interface
    transitions to Disconnected with a transient reason, cools down to
    Idle, then scans again and attempts to reconnect to this same profile
    (still sticky). Only explicit Disconnect or daemon restart clears
    the stickiness.

    Returns immediately; progress is reported via StateChanged.
    Errors: fi.nexus.Error.NotFound, fi.nexus.Error.InvalidArgument, fi.nexus.Error.AuthFailed

Disconnect() -> ()
    Disconnect from the current network.
    Errors: fi.nexus.Error.AuthFailed

Roam(bssid: ay) -> ()
    Request a roam to the given BSSID. Only valid in RoamingMode == "nexus".
    Errors: fi.nexus.Error.InvalidState, fi.nexus.Error.AuthFailed
```

To update credentials on an existing profile (for credentials-invalid recovery flows), use `fi.nexus.Profile.Update` on the profile object with the new credential fields in the settings dict. When `Update` changes a credential field on a profile with `CredentialsInvalid = true`, the backend clears the flag and the next auto-connect cycle retries the connection.

### 6.4 Bluetooth Interface

**Interface:** `fi.nexus.Bluetooth`
Added when `Kind == "bluetooth"`. Detailed design: [DD-004: Bluetooth Backend](./dd-004-bluetooth-backend.md). This interface represents a Bluetooth *adapter* (HCI controller). Individual paired/discovered devices are child objects exposing `fi.nexus.BluetoothDevice` (§6.6).

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `Address` | `s` | read | Adapter Bluetooth address as `"XX:XX:XX:XX:XX:XX"` |
| `Powered` | `b` | read/write | Whether the adapter radio is on. Writing calls BlueZ's Powered property |
| `Discoverable` | `b` | read/write | Whether the adapter responds to inquiry scans |
| `Pairable` | `b` | read/write | Whether the adapter accepts new pairing requests |
| `Discovering` | `b` | read | Whether any discovery session is active (Nexus's or another client's) |
| `NexusDiscovering` | `b` | read | Whether Nexus specifically has an outstanding discovery session |
| `KnownDevices` | `ao` | read | Object paths of `fi.nexus.BluetoothDevice` children under this adapter |
| `State` | `s` | read | Per DD-004 §4.1: `"unavailable"` \| `"present"` \| `"powered"` \| `"discovering"` \| `"gone"` |

**Methods:**

```
StartDiscovery(filter: a{sv}) -> ()
    Start a discovery session. filter keys are optional: "transport" (s:
    "auto"|"bredr"|"le"), "rssi" (n: dBm threshold), "uuids" (as), and
    "duplicate_data" (b). Omit the dict or pass empty for defaults.
    Idempotent — returns Ok if Nexus already has a session active.
    Errors: fi.nexus.Error.NotPowered, fi.nexus.Error.BluezUnavailable

StopDiscovery() -> ()
    Stop Nexus's discovery session. Does not affect sessions owned by
    other BlueZ clients. Idempotent.

Pair(device: o) -> (job_id: s)
    Initiate pairing with the device. device must be a child object path.
    Returns a pairing job id (ULID string) that correlates subsequent
    PairingPrompt and PairingComplete signals on the adapter object.
    Errors: fi.nexus.Error.UnknownDevice, fi.nexus.Error.InvalidState
    (device already pairing or paired), fi.nexus.Error.NotPowered

CancelPairing(device: o) -> ()
    Cancel an in-flight pairing. Maps to BlueZ's CancelPairing.
    Errors: fi.nexus.Error.InvalidState (no pairing in flight)

AnswerPairingPrompt(job_id: s, answer: v) -> ()
    Respond to a PairingPrompt. answer's variant depends on the prompt
    kind. Valid variant types per prompt kind:
      - RequestPin          → "s" (PIN string; 4-16 ASCII chars)
      - RequestPasskey      → "u" (6-digit passkey, 0..999999)
      - RequestConfirmation → "b" (true = passkeys match, false = don't)
      - RequestAuthorization → "b" (true = authorize, false = reject)
      - AuthorizeService    → "b" (true = authorize, false = reject)
      - DisplayPasskey      → "s" with value "acknowledge"
      - DisplayPin          → "s" with value "acknowledge"
    DisplayPasskey and DisplayPin are notification-only (the operator
    reads the value off Nexus's UI and types it on the peer). The
    "acknowledge" answer tells the backend the operator has seen the
    prompt so the backend's Agent method can return to BlueZ. Any
    string value other than "acknowledge" or "cancel" for these prompt
    kinds returns fi.nexus.Error.InvalidArgument.
    A special answer of "s":"cancel" cancels any prompt outright,
    regardless of kind — the backend treats this as operator rejection
    and the pairing fails with reason "rejected".
    Errors: fi.nexus.Error.UnknownPairingJob,
            fi.nexus.Error.InvalidArgument (wrong variant type for
            the prompt kind)

Connect(device: o) -> ()
    Connect to a paired device (Classic) or any device (BLE). Transitions
    the device's State through Connecting to Connected on success.
    Errors: fi.nexus.Error.UnknownDevice, fi.nexus.Error.NotPaired
    (Classic), fi.nexus.Error.ConnectionFailed

Disconnect(device: o) -> ()
    Disconnect from a device but keep any bond.
    Errors: fi.nexus.Error.UnknownDevice, fi.nexus.Error.InvalidState

Forget(device: o) -> ()
    Remove the bond and drop any stored profile. Maps to BlueZ's
    RemoveDevice plus Nexus's profile removal.
    Errors: fi.nexus.Error.UnknownDevice
```

**Signals:**

```
PairingStarted(job_id: s, device: o)
    A pairing has begun. Correlates with subsequent PairingPrompt
    and PairingComplete signals via job_id.

PairingPrompt(job_id: s, kind: s, data: a{sv})
    BlueZ's Agent needs a human response. kind is "request_pin",
    "request_passkey", "display_passkey", "display_pin",
    "request_confirmation", "request_authorization", or
    "authorize_service". data includes the relevant details:
      - "device": o (always present)
      - "passkey": u (for display_passkey, request_confirmation)
      - "pin": s (for display_pin)
      - "service_uuid": s (for authorize_service)
    Clients respond via AnswerPairingPrompt.

PairingComplete(job_id: s, success: b, reason: s)
    Pairing has ended. On success, the device's State is now Paired.
    On failure, reason is one of "rejected", "timeout", "auth_failed",
    "connection_failed", "other".

StateChanged(state: s)
    Adapter state changed — maps to BtAdapterState transitions.
    Fired from the common Interface.PropertiesChanged path, but also
    repeated here for clients that want adapter-specific signal
    subscriptions without walking PropertiesChanged.
```

### 6.5 GNSS Interface

**Interface:** `fi.nexus.Gnss`
Added when `Kind == "gnss"`. Detailed design: [DD-005: GNSS Backend](./dd-005-gnss-backend.md).

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `State` | `s` | read | Per DD-005 §3.1: `"pending"` \| `"acquiring"` \| `"tracking"` \| `"degraded"` |
| `DevicePath` | `s` | read | The kernel device path, e.g., `"/dev/ttyUSB0"` |
| `VendorModel` | `s` | read | Optional vendor/model from udev's `ID_MODEL`, empty if unknown |
| `LastFix` | `(xidddddddu)` | read | Tuple of `(time_unix_ms: x, mode: i, latitude: d, longitude: d, altitude_m: d, speed_mps: d, track_deg: d, horizontal_error_m: d, vertical_error_m: d, satellites_used: u)`. When no fix is available, all fields are zero-valued — clients should check `State` rather than inferring from this property. `mode` maps to `FixMode` as `0=NoFix`, `2=Fix2D`, `3=Fix3D` (following gpsd's convention). Optional source fields (e.g., `vertical_error_m` when the receiver doesn't report it) are published as `0.0` with no distinction from "actually zero" — consumers that care can read the internal `GnssFix` via a future diagnostic method |
| `SatellitesInView` | `u` | read | Count from the most recent SKY message |
| `SatellitesUsed` | `u` | read | Count used in the most recent qualifying fix |
| `HorizontalErrorM` | `d` | read | Most recent reported horizontal error in meters, or `0.0` when not reported |
| `GpsdConnected` | `b` | read | Whether the backend's connection to gpsd is currently alive |

**Signals:**

```
FixChanged(fix: (xidddddddu))
    Fired whenever a filtered fix (DD-005 §5.1 two-tier event model) is emitted
    by the backend. The filter applies the per-device quality threshold (DD-005
    §7.2) and emission rate cap (§7.3), so this signal corresponds to what
    downstream consumers actually care about, not every raw TPV.

SatellitesChanged(in_view: u, used: u)
    Fired on each SKY message from gpsd. Coalesced per §12.2; clients should
    read SatellitesInView / SatellitesUsed properties for current values.
```

**Methods:** None in v0.1. Future versions may expose `ActivateDevice` / `DeactivateDevice` for devices configured with `auto_activate = false`.

### 6.6 BluetoothDevice Interface

**Interface:** `fi.nexus.BluetoothDevice`
Exposed on each object under `/fi/nexus1/interface/<adapter-ifindex>/device/<device-address>`, where `<device-address>` is the Bluetooth address with colons replaced by underscores (matching BlueZ's convention: `AA_BB_CC_DD_EE_FF`). Detailed design: [DD-004: Bluetooth Backend §5](./dd-004-bluetooth-backend.md#5-device-lifecycle).

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `Address` | `s` | read | Device Bluetooth address as `"XX:XX:XX:XX:XX:XX"` |
| `AddressType` | `s` | read | `"bredr"` \| `"le_public"` \| `"le_random"` |
| `Transport` | `s` | read | `"bredr"` \| `"le"` \| `"dual"`. Synthesized; see DD-004 §6.2 |
| `Name` | `s` | read | Friendly name from GAP, or empty string if unresolved |
| `Alias` | `s` | read/write | Local editable label. Writing calls BlueZ's `Alias` property and updates the Nexus profile if one exists |
| `Rssi` | `n` | read | Most recent RSSI in dBm, or 0 if not observed since last discovery |
| `TxPower` | `n` | read | Advertised tx power (BLE), or 0 if not reported |
| `Uuids` | `as` | read | Service UUIDs (lowercase canonical form) |
| `ManufacturerData` | `a{qay}` | read | Manufacturer ID → advertisement bytes |
| `State` | `s` | read | Per DD-004 §5.1: `"discovered"` \| `"pairing"` \| `"paired"` \| `"connecting"` \| `"connected"` \| `"disconnecting"` \| `"failed"` \| `"removed"` |
| `Paired` | `b` | read | Mirrors BlueZ's Paired property |
| `Bonded` | `b` | read | Mirrors BlueZ's Bonded property. Always implies Paired |
| `Trusted` | `b` | read/write | Mirrors BlueZ's Trusted. Setting calls `BluezClient::set_trusted` and updates the profile |
| `Blocked` | `b` | read/write | Mirrors BlueZ's Blocked |
| `Connected` | `b` | read | Mirrors BlueZ's Connected |
| `Adapter` | `o` | read | Object path of the owning adapter (`fi.nexus.Bluetooth`) |
| `Profile` | `o` | read | Object path of the stored `fi.nexus.Profile.Bluetooth` profile, or `/` if none stored |

**Methods:**

```
Pair() -> (job_id: s)
    Shortcut for fi.nexus.Bluetooth.Pair(this). Returns the pairing
    job id for correlating with PairingPrompt signals on the adapter
    object. See §6.4 for the shared pairing event flow.
    Errors: fi.nexus.Error.InvalidState, fi.nexus.Error.NotPowered

CancelPairing() -> ()
    Shortcut for fi.nexus.Bluetooth.CancelPairing(this).
    Errors: fi.nexus.Error.InvalidState

Connect() -> ()
    Connect to the device. Classic devices must be Paired first;
    BLE devices may be Connect()ed without pairing.
    Errors: fi.nexus.Error.NotPaired (Classic), fi.nexus.Error.ConnectionFailed

Disconnect() -> ()
    Disconnect; keep the bond.
    Errors: fi.nexus.Error.InvalidState

Forget() -> ()
    Remove the bond and any stored profile. Shortcut for
    fi.nexus.Bluetooth.Forget(this).
    Errors: none; idempotent once removal starts
```

**Signals:**

```
StateChanged(state: s)
    Fired on every device state transition, with the new state as per
    the State property. Also surfaces via the common
    Interface.PropertiesChanged path; this signal exists for clients
    that want to subscribe to device-specific transitions without
    walking the property-change dict.

ConnectionChanged(connected: b)
    Finer-grained signal for just the Connected bool, for clients
    that care about connection state but not the full state machine.
```

**Note on adapter-vs-device signals.** Pairing prompts (`PairingStarted`, `PairingPrompt`, `PairingComplete`) fire on the *adapter* object (`fi.nexus.Bluetooth`, §6.4), not on this per-device object — a pairing is an adapter-scoped operation. Clients interested in a particular device's pairing use the `device` field in the pairing-event `data` dict to filter.

---

## 7. Profile Objects

Profiles live at `/fi/nexus1/profile/<kind>/<ulid>`.

### 7.1 Common Profile Interface

**Interface:** `fi.nexus.Profile`
Implemented by every profile object.

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `Id` | `s` | read | ULID from DD-007 |
| `Kind` | `s` | read | `"ethernet"` \| `"wifi"` |
| `Label` | `s` | read/write | Human-friendly label |
| `CreatedAt` | `s` | read | RFC 3339 timestamp |
| `UpdatedAt` | `s` | read | RFC 3339 timestamp |
| `CredentialsInvalid` | `b` | read | Set by the backend after an auth failure; cleared when `Update` modifies any credential field (see §7 note on recovery flows) |

**Methods:**

```
Update(settings: a{sv}) -> ()
    Partial update of the profile. Only keys present in settings are modified.
    Credentials follow the same cleartext-in / encrypted-on-disk pattern.
    Errors: fi.nexus.Error.InvalidArgument, fi.nexus.Error.AuthFailed

Delete() -> ()
    Remove this profile. Equivalent to Manager.RemoveProfile(this).
    Errors: fi.nexus.Error.AuthFailed
```

### 7.2 Wi-Fi Profile

**Interface:** `fi.nexus.Profile.Wifi`
Added to `/fi/nexus1/profile/wifi/<ulid>` in addition to `fi.nexus.Profile`.

**Properties:** All the fields of `WifiProfile` from DD-007 §5.2, except credentials which are exposed via a credential-presence map:

| Name | Type | Access | Meaning |
|---|---|---|---|
| `Ssid` | `ay` | read | Raw SSID bytes |
| `Hidden` | `b` | read/write | |
| `Priority` | `i` | read/write | |
| `AutoConnect` | `b` | read/write | |
| `FastTransition` | `b` | read/write | |
| `Security` | `a{sv}` | read | Dict with `"type"` and non-credential fields |
| `HasCredentials` | `a{sb}` | read | Map from credential field name to boolean presence flag — see below |
| `BssidPreferred` | `ay` | read/write | Empty if none |
| `BssidBlacklist` | `aay` | read/write | |
| `ScanFrequencies` | `au` | read/write | |

The `HasCredentials` map enumerates every credential field and whether a value is stored. Keys depend on the security type:

- **WPA2/WPA3/Transition Personal**: `"passphrase"` or `"psk"` (mutually exclusive)
- **OWE / Open**: empty map (no credentials)
- **WPA2/WPA3 Enterprise**: `"password"` (for password-based EAP), `"private_key_passwd"` (for cert-based EAP with encrypted key), `"pin"` (for smartcard-backed EAP, forthcoming)

A field present in the map with value `true` means a value is stored for it; value `false` is used only transiently during edits. Clients should treat absence and `false` equivalently. Credential material never leaves the process through `Properties.Get`. To update credentials, use `Update(settings)` on the profile object with the credentials in the settings dict.

### 7.3 Ethernet Profile

**Interface:** `fi.nexus.Profile.Ethernet`
Added to `/fi/nexus1/profile/ethernet/<ulid>`.

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `Ifname` | `s` | read/write | Interface name (e.g., `"eth0"`) |
| `AutoConnect` | `b` | read/write | |
| `Dot1xEnabled` | `b` | read/write | Whether 802.1X is configured |
| `Dot1xEap` | `s` | read | EAP method name (e.g., `"TLS"`, `"PEAP"`), empty when Dot1xEnabled is false |
| `HasCredentials` | `a{sb}` | read | Map from credential field name to boolean presence flag |

`HasCredentials` keys for Ethernet:
- `"password"` — for password-based EAP (PEAP/TTLS with inner MSCHAPv2)
- `"private_key_passwd"` — for EAP-TLS with encrypted private key

Same access semantics as the Wi-Fi profile: credential values are never returned; updates go through `Update(settings)` on the profile object.

---

## 8. Scan Result Objects

One object per cached BSS, at `/fi/nexus1/interface/<ifname>/scan_result/<bssid>`.

**Interface:** `fi.nexus.ScanResult`

**Properties:**

| Name | Type | Access | Meaning |
|---|---|---|---|
| `Bssid` | `ay` | read | 6 bytes |
| `Ssid` | `ay` | read | Raw SSID bytes |
| `Frequency` | `u` | read | MHz |
| `SignalDbm` | `i` | read | Most recent RSSI |
| `SecurityOffered` | `as` | read | Array of security-mode strings the AP advertises |
| `AgeMs` | `t` | read | Milliseconds since last seen |
| `Capabilities` | `a{sv}` | read | Dict with HT/VHT/HE indicators, 802.11r support, PMF required, etc. |

Scan result objects are transient — they appear on `InterfacesAdded` when a new BSS is cached and disappear on `InterfacesRemoved` when evicted (stale, replaced, or explicit cache clear). Clients should subscribe through ObjectManager rather than polling.

**Privacy note.** Any local process that can talk to the system bus and has `fi.nexus.read` authorization can enumerate visible BSSes via ObjectManager, effectively seeing every nearby Wi-Fi network the device has scanned. On embedded appliances with a single trusted workload this is a non-issue. On multi-tenant devices or devices hosting untrusted local users, consider tightening `fi.nexus.read` to a specific group rather than leaving it at the `yes` default (§10.1). There is no way to expose "connected network only" to unprivileged callers without losing the ability to present a scan picker UI to privileged ones — this is an intentional design trade-off favoring the embedded fleet use case.

---

## 9. Signals

Signals are the primary way clients learn about state changes. The complete list, grouped by emitter:

**Manager (`fi.nexus.Manager`):**

Defined in full in §5.3: `PowerStateChanged`, `NotificationEvent`, `MasterKeyRotated`. ObjectManager's `InterfacesAdded` / `InterfacesRemoved` fire whenever child objects (interfaces, profiles, scan results) appear or disappear.

**Interface (`fi.nexus.Interface`):**

```
StateChanged(new_state: s, details: a{sv})
    Emitted for every state transition on any per-technology interface.
    The object path on which the signal is emitted (implicit in D-Bus signal
    semantics) identifies the interface. Clients match by interface+member
    and filter by path if needed.
```

`details` dict keys by technology and transition:

| Key | Type | Present when | Meaning |
|---|---|---|---|
| `"reason"` | `s` | entering Disconnected/AuthFailed | Short reason code: `"credentials_invalid"`, `"server_unreachable"`, `"handshake_timeout"`, `"rf_killed"`, `"supplicant_unavailable"`, etc. |
| `"bssid"` | `ay` | Wi-Fi Connecting / Authenticating / Handshaking / Connected / Roaming | Target or current BSSID as 6 raw bytes |
| `"ssid_bytes"` | `ay` | Wi-Fi Connecting onwards | Raw SSID bytes |
| `"frequency"` | `u` | Wi-Fi Connected | MHz |
| `"signal_dbm"` | `i` | Wi-Fi Connected | RSSI at connection time |
| `"security"` | `s` | Wi-Fi Connected | `"open"` \| `"owe"` \| `"wpa2_personal"` \| etc. |
| `"profile"` | `o` | any technology, when a stored profile drove the transition | Object path of the profile |
| `"eap_method"` | `s` | Ethernet Authenticating | EAP method name |

Clients must handle missing keys gracefully — the set of populated keys is a function of the transition and may grow in future versions.

**Wi-Fi (`fi.nexus.Wifi`):**

```
ScanCompleted(results_count: u)
    Fires after a scan settles. Clients walk ScanResults to fetch details.

SignalLevel(rssi: i, frequency: u)
    Periodic signal poll. Emitted at the Wi-Fi backend's polling rate (§DD-003 §7.2).

RoamStarted(from_bssid: ay, to_bssid: ay)
RoamCompleted(bssid: ay, success: b)
```

**Ethernet (`fi.nexus.Ethernet`):**

No Ethernet-specific signals. Auth state transitions flow through the generic `fi.nexus.Interface.StateChanged` signal, with the `details` dict carrying `"reason"` (on `auth_failed`) and `"eap_method"` (during authenticating). See §9 StateChanged details table.

**Profile (`fi.nexus.Profile`):**

```
CredentialsInvalidChanged(invalid: b)
    Fires when a profile flips between valid and invalid states. Paired with
    Manager.NotificationEvent for operator-visible UI.
```

**ObjectManager:**

```
InterfacesAdded(path: o, interfaces: a{sa{sv}})
InterfacesRemoved(path: o, interfaces: as)
```

All signals are documented in the generated introspection XML with full argument typing.

---

## 10. Authorization (PolicyKit)

Nexus is on the system bus, which means any local user can talk to it. Most operations require authorization. Nexus delegates to PolicyKit (`polkit`) for this, matching the pattern used by systemd, NetworkManager, and BlueZ.

### 10.1 Policy Actions

Each action has a unique identifier and a policy file at `/usr/share/polkit-1/actions/fi.nexus.policy`. Summary:

| Action | Default | Purpose |
|---|---|---|
| `fi.nexus.read` | `yes` | Read any property, list objects, introspect |
| `fi.nexus.scan` | `auth_self_keep` | Trigger a Wi-Fi scan |
| `fi.nexus.connect` | `auth_self_keep` | Connect/disconnect using an existing profile |
| `fi.nexus.profile.add` | `auth_admin_keep` | Add a new profile |
| `fi.nexus.profile.modify` | `auth_admin_keep` | Update or delete a profile |
| `fi.nexus.profile.read_credentials` | `no` | Read decrypted credential fields — disabled by default |
| `fi.nexus.set_power` | `auth_self_keep` | Change power state |
| `fi.nexus.admin` | `auth_admin_keep` | Rotate master key, freeze for backup, collect diagnostics, reload config |

Defaults can be adjusted by operators via standard PolicyKit rules (`/etc/polkit-1/rules.d/...`). For headless devices, the typical setup grants full access to a single `nexus-admin` group.

**On the `fi.nexus.read` default.** Defaulting `fi.nexus.read` to `yes` (any local user can read) is appropriate for the primary target: single-workload embedded appliances where every local process belongs to the operator. On multi-tenant devices — e.g., a Linux desktop or a shared compute node — the default is too permissive because the scan-result objects leak "which Wi-Fi networks are near this device" to any local user (see §8 Privacy note). Such deployments should override the default via a PolicyKit rule that restricts read access to a specific group:

```javascript
polkit.addRule(function(action, subject) {
    if (action.id == "fi.nexus.read" &&
        !subject.isInGroup("nexus-read")) {
        return polkit.Result.NO;
    }
});
```

This is deployment-specific configuration, not an API change — Nexus ships the lenient default that fits the embedded-fleet use case.

### 10.2 Authorization Check Flow

For every mutating method call:

1. Extract the caller's bus name from the D-Bus message.
2. Ask `org.freedesktop.DBus` for the PID and UID of the caller.
3. Call `org.freedesktop.PolicyKit1.Authority.CheckAuthorization` with the action ID and the caller's subject.
4. If authorized, proceed with the operation. Otherwise, return `fi.nexus.Error.AuthFailed` with a `reason` detail.

The check is async and usually returns in under 10 ms. Results are cached for the duration of the D-Bus call — a method that internally calls multiple helpers doesn't re-check.

### 10.3 Agent-less Deployments

Embedded fleets often run headless with no interactive user. The PolicyKit defaults work fine: `auth_admin_keep` succeeds when the caller is root. Deployments that want finer-grained control without interactive auth use PolicyKit JS rules to allow specific UIDs or groups to act admin-equivalent:

```javascript
polkit.addRule(function(action, subject) {
    if (action.id.indexOf("fi.nexus.") === 0 &&
        subject.isInGroup("nexus-admin")) {
        return polkit.Result.YES;
    }
});
```

This is documented in the Nexus operator guide; not in the API.

---

## 11. Error Semantics

### 11.1 Error Namespace

All errors use `fi.nexus.Error.*` as the D-Bus error name. Concrete errors:

| Error | Maps to | Typical cause |
|---|---|---|
| `fi.nexus.Error.NotFound` | ENOENT | Interface, profile, or BSS doesn't exist |
| `fi.nexus.Error.AlreadyExists` | EEXIST | Creating a profile that would collide with an existing unique key (SSID for Wi-Fi, ifname for Ethernet) |
| `fi.nexus.Error.InvalidArgument` | EINVAL | Malformed dict, unsupported enum value, invalid MAC |
| `fi.nexus.Error.InvalidState` | EPERM | Operation not valid in current state (e.g., Roam while disconnected) |
| `fi.nexus.Error.AuthFailed` | EACCES | PolicyKit denied |
| `fi.nexus.Error.ResourceBusy` | EBUSY | Scan in progress, rotation in progress, backup lease held |
| `fi.nexus.Error.RateLimited` | EAGAIN | Per-sender rate limit exceeded (§15). Carries a `retry_after_ms` hint |
| `fi.nexus.Error.FeatureDisabled` | ENODEV | Backend for this interface is disabled in `nexus.toml` (e.g., `[bluetooth].enabled = false`). The D-Bus objects still exist so clients can discover availability; mutating calls return this error. Property reads of non-sensitive summary state (e.g., `Enabled: false`) remain permitted. |
| `fi.nexus.Error.Timeout` | ETIMEDOUT | Backend didn't respond within the internal deadline |
| `fi.nexus.Error.SupplicantUnavailable` | EAGAIN | wpa_supplicant/iwd D-Bus name missing |
| `fi.nexus.Error.IoError` | EIO | Underlying filesystem, netlink, or D-Bus failure |
| `fi.nexus.Error.CryptoError` | — (no direct errno; maps from internal crypto errors) | Encryption/decryption failure (profile store) |
| `fi.nexus.Error.Unsupported` | ENOTSUP | Feature not available (e.g., WPA3 on a WPA2-only chipset) |

### 11.2 Error Payloads

Every error carries a human-readable message as its primary payload. For programmatic clients, errors may also carry a detail dict as a second argument when the caller negotiated that capability. Example:

```
fi.nexus.Error.InvalidArgument:
  message: "unknown security type 'wpa4_personal'"
  details: { field: "security.type", hint: "valid values: open, owe, wpa2_personal, ..." }
```

The detail dict is optional; clients must handle errors with only a message.

### 11.3 Backpressure vs Error

Two method-call failure modes deserve explicit mention:

- `ResourceBusy` — *retry later* is the right response. Scans in particular often return this when the driver is busy.
- `Timeout` — the operation *might* have succeeded; the server didn't get a response in time. Clients should reconcile state via properties/signals rather than blindly retrying.

---

## 12. Property Change Semantics

### 12.1 PropertiesChanged

Every property change fires `org.freedesktop.DBus.Properties.PropertiesChanged` per standard D-Bus convention. The Nexus service layer emits these automatically when a cached property value changes.

### 12.2 Coalescing

Some properties change rapidly — `SignalDbm` during a bad-signal event can fire 5 Hz. Nexus coalesces:

- **Hot properties** (`SignalDbm`, `Frequency`, scan result `AgeMs`): coalesced to at most 1 Hz per property per object. The value shown via `Properties.Get` is always current; only the signal frequency is throttled.
- **Cold properties** (state transitions, profile changes, `OperState`): emitted immediately with no coalescing. These carry semantic meaning; throttling would lose events.

**Implementation.** Each object holds a `PropertyBatcher` with:

- A `HashMap<&'static str, Value>` of dirty properties (name → current value).
- A single `tokio::time::Sleep` future that fires 1 s after the first change is recorded.

When a backend event updates a property, the batcher records the new value and, if the sleep future hasn't been armed, arms it. Further updates to the same or other properties before the sleep fires accumulate into the map; the sleep is not rescheduled. When the sleep fires, the batcher drains the map and emits a single `PropertiesChanged` with all accumulated changes, then clears the armed state. This gives at most one signal per second per object regardless of how many hot-property updates arrive, with at most ~1 s of latency before clients see the final value.

Cold properties bypass the batcher entirely and emit `PropertiesChanged` immediately. The distinction is made at the property declaration level via a `#[coalesced]` attribute on the zbus interface macros; no runtime list of hot vs cold property names is needed.

### 12.3 Atomicity

When multiple properties change in response to a single backend event, Nexus emits a single `PropertiesChanged` with all of them batched. For example, when a Wi-Fi interface connects, `State`, `ConnectedBss`, `SignalDbm`, and `Frequency` all change together and appear in one signal.

---

## 13. Introspection and Discoverability

Every object implements `org.freedesktop.DBus.Introspectable.Introspect() -> (xml: s)`. The returned XML enumerates every interface, method, property, and signal with full argument types and Doc-style annotations where available.

**Generation and golden-file verification.** `zbus` 5.x's `#[interface]` macro generates introspection XML at compile time as part of each interface impl. The XML is not directly emitted from the build artifact; instead, a small test binary (`nexus-dbus-introspect`) starts the service in a unit-test harness and calls `Introspect()` on each canonical object path, writing the XML to files under `tests/introspection/golden/`. CI runs this binary and compares output against committed goldens — any API change fails CI until the goldens are regenerated with `cargo run --bin nexus-dbus-introspect -- --update-goldens`. This makes every API change reviewable as a diff.

**CI requirements.** The introspect binary needs a real D-Bus session to call `Introspect()` through. CI provisions this via `dbus-run-session` wrapping the test invocation — no actual system bus needed, no privilege required, and no mocking of the bus itself (mocking would produce XML that doesn't match real runtime behavior). The binary seeds the service with a fixed set of canonical objects (one Ethernet interface, one Wi-Fi interface with a single scan result, one Ethernet profile, one Wi-Fi profile, one Bluetooth stub) so the golden output is deterministic. Adding a new interface or method regenerates the goldens; reviewers see the XML diff alongside the Rust change.

External clients can use `busctl introspect fi.nexus1 /fi/nexus1` on a running system to browse the full hierarchy interactively.

---

## 14. Service Activation and Readiness

### Service Activation

Nexus supports D-Bus service activation: a client calling `fi.nexus1` for the first time causes `dbus-daemon` to start Nexus via systemd. The activation files:

- `/usr/share/dbus-1/system-services/fi.nexus1.service`:
  ```
  [D-BUS Service]
  Name=fi.nexus1
  Exec=/usr/libexec/nexusd
  User=nexus
  SystemdService=nexus.service
  ```

- `/lib/systemd/system/nexus.service`:
  ```
  [Unit]
  Description=Nexus connectivity manager
  Requires=dbus.socket
  After=dbus.socket systemd-networkd.service

  [Service]
  Type=dbus
  BusName=fi.nexus1
  ExecStart=/usr/libexec/nexusd
  User=nexus
  ```

With this configuration, `busctl` calls from a shell will start Nexus on demand. Embedded deployments that want Nexus running at boot unconditionally can enable the service directly (`systemctl enable nexus.service`) — the activation files remain harmless.

**Minimum dbus-daemon version.** The `SystemdService=` directive requires `dbus-daemon` 1.8 or newer (released 2014) with `--enable-systemd` at build time. All modern distributions ship dbus with systemd integration enabled, but minimal embedded builds (e.g., `dbus-broker` as a drop-in replacement, or very old `dbus-daemon` without systemd support) may not. On such systems, `dbus-daemon` falls back to `Exec=` activation alone — the service still starts on demand, but without systemd coordination. Integrators using `dbus-broker` (1.x) should verify systemd-activation support; basic `Exec=` activation works in all cases.

### Readiness and Backend Initialization

A client that calls a method immediately after Nexus starts may find that the backend for the target interface hasn't completed initialization yet. Concrete timing:

- The Interface Monitor's cold-boot enumeration takes up to 1.5 s ([DD-001 §5.6](./dd-001-interface-discovery.md#56-boot-time-budget)).
- Each Wi-Fi interface's supplicant attach takes another 100–500 ms after discovery.
- PolicyKit's first authorization check adds ~10 ms.

Behavior during the readiness window:

- **Property reads** on yet-unregistered interfaces: return `fi.nexus.Error.NotFound`. Clients should not treat early-boot `NotFound` as terminal — the interface may appear once discovery completes. Subscribe to `InterfacesAdded` on the Manager to be notified.
- **Mutating methods** (`Scan`, `Connect`, `SetPowerState`, etc.) on an interface that exists but whose backend hasn't finished attaching: the D-Bus layer queues the call, blocks up to 5 seconds on a readiness channel, and either proceeds when the backend is ready or returns `fi.nexus.Error.ResourceBusy` with `retry_after_ms`. The 5-second ceiling prevents hanging D-Bus clients that don't understand async activation.
- **Manager methods** (`GetInterface`, profile CRUD, backup lease): available as soon as the service owns its bus name — these don't depend on backend state.

The readiness channel is a simple `tokio::sync::Notify` that fires once per backend, toggled by the backends themselves when their initial `attach`/enumeration completes. The D-Bus layer subscribes at construction time and tracks per-backend readiness.

---

## 15. Rate Limiting and Backpressure

Malicious or buggy clients can DoS Nexus by hammering methods. The D-Bus layer applies rate limits per bus connection:

| Operation class | Limit |
|---|---|
| Property reads | 1000 / minute |
| Scans (per interface) | 10 / minute |
| Connects / Disconnects | 30 / minute |
| Profile add/modify/remove | 30 / minute |
| Rotations and backups | 1 / 60 seconds |

Limit exceeded → `fi.nexus.Error.RateLimited` with a `retry_after_ms` hint. Limits reset on a rolling window. (Prior revisions of this DD reused `ResourceBusy` for rate-limit rejections; that was imprecise — `ResourceBusy` now means genuine backpressure only, e.g., another scan or rotation already in progress.)

**Counting policy.** A method call that returns `RateLimited` does NOT consume a slot — otherwise a client that accidentally bursts over the limit would lock itself out further with every retry. A method call that returns `ResourceBusy` (scan already in progress, rotation in progress, backup lease held) DOES consume a slot, matching the bookkeeping of any successful call: the client made a legitimate attempt; the system just can't service it right now. Other errors (`InvalidArgument`, `NotFound`, etc.) also consume a slot, because the client's behavior needs throttling regardless of outcome.

**Layering with PolicyKit.** Rate limiting is applied in two stages:

1. **Cheap pre-filter** on every incoming call, before PolicyKit is consulted. An unauthenticated or unknown caller (no prior successful PolicyKit check) gets a low global limit of 60 requests/minute for any method class. This prevents an attacker from burning PolicyKit CPU with a flood of unauthorized calls (each PolicyKit check is ~10 ms).
2. **Per-class limit** after PolicyKit has authorized the caller. The limits in the table above apply here. A malicious but *authorized* caller still can't burn scan resources or profile-write I/O.

Rate limits are per-connection (per D-Bus unique sender bus name, e.g. `:1.42`), not per-UID, so a misbehaving process can't affect others from the same user. The connection table holds a counter window per connection per limit class — a few hundred bytes per active client. When a connection closes (`NameOwnerChanged` with empty new owner), its state is dropped.

---

## 16. Example Flows

### 16.1 Connect to a Known Wi-Fi Network

```
# Find the profile by SSID. busctl's byte-array syntax for D-Bus "ay" type
# is "ay <count> <byte1> <byte2> ...", with bytes as decimal or 0x-prefixed
# hex. Here 10 bytes spell "corp-wifi\0".
busctl call fi.nexus1 /fi/nexus1 fi.nexus.Manager FindWifiProfile \
    ay 10 0x63 0x6f 0x72 0x70 0x2d 0x77 0x69 0x66 0x69 0x00
# -> o "/fi/nexus1/profile/wifi/01HPQY8S2N0Z8K9M7V3Y2F4T5W6"

# Find the interface
busctl call fi.nexus1 /fi/nexus1 fi.nexus.Manager GetInterface s "wlp2s0"
# -> o "/fi/nexus1/interface/wlp2s0"

# Initiate connection
busctl call fi.nexus1 /fi/nexus1/interface/wlp2s0 fi.nexus.Wifi Connect \
    o "/fi/nexus1/profile/wifi/01HPQY8S2N0Z8K9M7V3Y2F4T5W6"
# -> (no return)

# Watch state via signals:
busctl monitor --match "interface='fi.nexus.Interface',member='StateChanged'"
```

### 16.2 Add a New Wi-Fi Profile

```python
# Using jeepney (pure-Python, no C deps, async-compatible).
# dbus-python works too but is considered legacy.
from jeepney import DBusAddress, new_method_call
from jeepney.io.blocking import open_dbus_connection

manager = DBusAddress(
    "/fi/nexus1",
    bus_name="fi.nexus1",
    interface="fi.nexus.Manager",
)

settings = {
    "ssid":         ("ay", list(b"MyHomeNetwork")),
    "priority":     ("i", 10),
    "auto_connect": ("b", True),
    "security":     ("a{sv}", {
        "type":       ("s", "wpa2_personal"),
        "passphrase": ("s", "correct horse battery staple"),
    }),
}

msg = new_method_call(manager, "AddWifiProfile", "a{sv}", (settings,))
with open_dbus_connection(bus="SYSTEM") as conn:
    reply = conn.send_and_get_reply(msg)
    (profile_path,) = reply.body
    print(f"Created profile at {profile_path}")
```

Alternatively via `gdbus`:

```bash
gdbus call --system \
    --dest fi.nexus1 \
    --object-path /fi/nexus1 \
    --method fi.nexus.Manager.AddWifiProfile \
    "{'ssid': <[byte 0x4d, 0x79, 0x48, 0x6f, 0x6d, 0x65]>,
      'priority': <int32 10>,
      'auto_connect': <true>,
      'security': <{'type': <'wpa2_personal'>,
                    'passphrase': <'correct horse battery staple'>}>}"
```

The passphrase is in memory only; the Profile Store encrypts it on write. The returned object never exposes the decrypted passphrase.

### 16.3 Scan and Enumerate Results

```
# Trigger a scan
busctl call fi.nexus1 /fi/nexus1/interface/wlp2s0 fi.nexus.Wifi Scan a{sv} 0

# Wait for ScanCompleted signal, then read results
busctl get-property fi.nexus1 /fi/nexus1/interface/wlp2s0 \
    fi.nexus.Wifi ScanResults
# -> ao 5 "/fi/nexus1/interface/wlp2s0/scan_result/aabbccddeeff" ...

# Enumerate each result
for path in scan_results:
    busctl get-all-properties fi.nexus1 $path fi.nexus.ScanResult
```

---

## 17. Testing Strategy

### 17.1 Unit Tests

- **Error mapping.** Backend `Result<T, E>` values translate to the correct `fi.nexus.Error.*` with the expected message.
- **Property coalescing.** High-frequency property updates produce at most one signal per coalescing window.
- **Object path escaping.** Interface names with special characters produce valid D-Bus paths.

### 17.2 Integration Tests

- **End-to-end with `zbus` clients.** Spin up Nexus in a test harness, connect with a `zbus` client, exercise every method and assert correct responses and signals.
- **Introspection golden.** The generated XML matches the committed golden file. Any API change requires updating the golden.
- **PolicyKit integration.** Mock the PolicyKit daemon; verify allowed and denied operations produce the correct results.
- **ObjectManager enumeration.** After spinning up Nexus with a known set of interfaces and profiles, a single `GetManagedObjects` call returns everything.

### 17.3 Compatibility Tests

- **`busctl`** usage examples from this doc continue to work. Regression test each §16 example.
- **Python `dbus-python`** client can perform the core flows.
- **Rust `zbus` 5.x** clients work (pinned minor version in CI).

### 17.4 Fault Injection

- **Slow backend.** Insert delays in backend command handling; verify `Timeout` errors are returned cleanly and the service doesn't deadlock.
- **Signal flood.** Generate thousands of state changes per second; verify coalescing prevents the bus from being saturated.
- **Malformed input.** Fuzz method argument dicts; verify `InvalidArgument` is returned without panicking.

---

## 18. Implementation Phases

### Phase 1 — Service Skeleton and Manager

`crates/nexus-dbus/src/service.rs`, `manager.rs`. Own the bus name, serve the Manager object with read-only properties, implement `GetInterface` and `ListInterfaces`. No writes yet.

**Exit criterion:** `busctl introspect fi.nexus1 /fi/nexus1` returns a coherent XML. Listing interfaces matches the Interface Monitor's registry.

### Phase 2 — Per-Technology Interface Objects (Read-Only)

`interfaces/ethernet.rs`, `interfaces/wifi.rs`. Properties for state, signal, connected BSS, etc. Subscribe to `NexusEvent` and translate into `PropertiesChanged`.

**Exit criterion:** State changes in the backends visibly propagate to D-Bus clients via signals.

### Phase 3 — Profile Objects (Read-Only)

`profiles/wifi.rs`, `profiles/ethernet.rs`. Expose stored profiles via ObjectManager. Credential-boolean pattern (§7.2) enforced.

**Exit criterion:** `busctl tree fi.nexus1` shows all profiles. Reading any credential property returns a boolean, never the underlying value.

### Phase 4 — PolicyKit Integration

`authz.rs`. Policy file at `/usr/share/polkit-1/actions/fi.nexus.policy`. Authorization checks on every mutating method (even before those methods are implemented — skeleton returns AuthFailed).

**Exit criterion:** Unauthorized callers receive `AuthFailed`. Authorized callers proceed. Mocked PolicyKit tests cover both paths.

### Phase 5 — Mutating Methods (Wi-Fi)

`Scan`, `Connect`, `Disconnect`, `AddWifiProfile`, `RemoveProfile`, `Update` on profile objects. Each goes through the backend command channel.

**Exit criterion:** The §16.1 and §16.2 example flows work end to end against hostapd+hwsim.

### Phase 6 — Mutating Methods (Ethernet) and Power

`AddEthernetProfile`, `SetPowerState`. Ethernet has fewer mutations; power propagates to all backends.

**Exit criterion:** Toggle power state and observe scan scheduling changes on Wi-Fi interfaces.

### Phase 7 — Scan Result Objects

`scan_results.rs`. Dynamic objects per BSS. Emit `InterfacesAdded` / `InterfacesRemoved` as the BSS cache churns.

**Exit criterion:** A scan produces visible per-BSS objects. Stale BSSes disappear on eviction.

### Phase 8 — Signals and Coalescing

`signals.rs`, `properties.rs`. `StateChanged`, `ScanCompleted`, `SignalLevel`, `NotificationEvent`. Coalescing for hot properties.

**Exit criterion:** Fault-injection test with high-frequency state change sequences never exceeds the coalescing limits.

### Phase 9 — Admin Operations

`RotateMasterKey`, `FreezeForBackup`, `ReleaseBackupLease`, `CollectDiagnostics`, `ReloadConfig`. These gate behind `fi.nexus.admin`.

**Exit criterion:** Can perform a full rotation from a D-Bus client and verify profiles are re-encrypted per DD-007.

### Phase 10 — Rate Limiting and Error Details

§15 rate limits, §11.2 error-detail dicts. A negotiation capability in `ApiCapabilities` for detail support.

**Exit criterion:** DoS tests (§17.4 signal flood) bounce off the limits cleanly.

### Phase 11 — Compatibility and Documentation

Generate introspection XML golden files. Write operator guide for PolicyKit customization. Ship `nexusctl` CLI that exercises every documented flow.

**Exit criterion:** CI enforces golden XML match. `nexusctl --help` covers all §16 flows. Published operator documentation.

---

## Related Documents

- [Nexus Architecture](./nexus-architecture.md) — Parent, notably the event bus (§6) that backends publish on
- [DD-001: Interface Discovery](./dd-001-interface-discovery.md) — Source of `InterfaceAdded` / `InterfaceRemoved` events that become ObjectManager signals
- [DD-002: Ethernet Backend](./dd-002-ethernet-backend.md) — Source of `fi.nexus.Ethernet` property data
- [DD-003: Wi-Fi Backend](./dd-003-wifi-backend.md) — Source of `fi.nexus.Wifi` property data and scan result objects
- [DD-007: Profile Store](./dd-007-profile-store.md) — Backing store for all profile objects; exposes the credential-boolean pattern and rotation mechanism
- DD-004: Bluetooth Backend *(forthcoming)* — Will source `fi.nexus.Bluetooth` property data and bond objects
