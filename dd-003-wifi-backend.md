# DD-003: Wi-Fi Backend — Detailed Design

**Parent:** [Nexus Architecture](./nexus-architecture.md)
**Depends on:** [DD-001: Interface Discovery](./dd-001-interface-discovery.md)
**Related:** [DD-002: Ethernet Backend](./dd-002-ethernet-backend.md) — shares the supplicant trait abstraction
**Status:** Implemented (wpa_supplicant backend) — see §15 for shipped scope and open residuals.
**Scope:** Design of the Wi-Fi Backend — lifecycle management for wireless station-mode interfaces, scanning, network selection, connection, roaming, and integration with wpa_supplicant (default) or iwd (alternative).

---

## Table of Contents

1. [Context](#1-context)
   - 1.1 [Repo Layout](#11-repo-layout)
2. [Responsibilities](#2-responsibilities)
3. [Interface Lifecycle](#3-interface-lifecycle)
   - 3.1 [States](#31-states)
   - 3.2 [State Transitions](#32-state-transitions)
   - 3.3 [Why this is different from Ethernet](#33-why-this-is-different-from-ethernet)
4. [Supplicant Abstraction](#4-supplicant-abstraction)
   - 4.1 [Trait Definition](#41-trait-definition)
   - 4.2 [Shared Types](#42-shared-types)
   - 4.3 [Network Configuration](#43-network-configuration)
   - 4.4 [Backend Selection](#44-backend-selection)
   - 4.5 [Why the abstraction is thicker than the Ethernet one](#45-why-the-abstraction-is-thicker-than-the-ethernet-one)
5. [Scanning](#5-scanning)
   - 5.1 [Scan Triggers](#51-scan-triggers)
   - 5.2 [Scan Parameters](#52-scan-parameters)
   - 5.3 [Scan Results Processing](#53-scan-results-processing)
   - 5.4 [Scan Scheduling](#54-scan-scheduling)
6. [Network Selection and Connection](#6-network-selection-and-connection)
   - 6.1 [Profile Matching](#61-profile-matching)
   - 6.2 [Connection Sequence](#62-connection-sequence)
   - 6.3 [Failure Modes During Connection](#63-failure-modes-during-connection)
   - 6.4 [Connection Policy](#64-connection-policy)
   - 6.5 [Network Handle Lifecycle](#65-network-handle-lifecycle)
7. [Roaming](#7-roaming)
   - 7.1 [Roaming Modes](#71-roaming-modes)
   - 7.2 [Signal Monitoring](#72-signal-monitoring)
   - 7.3 [Roam Trigger (Nexus-driven Mode)](#73-roam-trigger-nexus-driven-mode)
   - 7.4 [Fast BSS Transition (802.11r)](#74-fast-bss-transition-80211r)
8. [Security Modes](#8-security-modes)
   - 8.1 [Supported Modes](#81-supported-modes)
   - 8.2 [PMF (Protected Management Frames) Policy](#82-pmf-protected-management-frames-policy)
   - 8.3 [Security Compatibility Matching](#83-security-compatibility-matching)
   - 8.4 [Enterprise Configuration](#84-enterprise-configuration)
9. [wpa_supplicant Backend (Default)](#9-wpa_supplicant-backend-default)
   - 9.1 [D-Bus Service](#91-d-bus-service)
   - 9.2 [Attach](#92-attach)
   - 9.3 [Scan](#93-scan)
   - 9.4 [Connect](#94-connect)
   - 9.5 [State Translation](#95-state-translation)
   - 9.6 [Disconnect Reason Mapping](#96-disconnect-reason-mapping)
10. [iwd Backend (Alternative)](#10-iwd-backend-alternative)
    - 10.1 [D-Bus Service](#101-d-bus-service)
    - 10.2 [Attach](#102-attach)
    - 10.3 [Connect](#103-connect)
    - 10.4 [State Translation](#104-state-translation)
    - 10.5 [Limitations and Caveats](#105-limitations-and-caveats)
11. [Configuration](#11-configuration)
    - 11.1 [Global Configuration](#111-global-configuration)
    - 11.2 [Per-Profile Configuration](#112-per-profile-configuration)
12. [Error Handling](#12-error-handling)
    - 12.1 [Supplicant Crash or Restart](#121-supplicant-crash-or-restart)
    - 12.2 [Scan Failure](#122-scan-failure)
    - 12.3 [Connection Retry Storm](#123-connection-retry-storm)
    - 12.4 [Driver/Firmware Wedges](#124-driverfirmware-wedges)
    - 12.5 [Observability](#125-observability)
13. [Power Management](#13-power-management)
    - 13.1 [Power States](#131-power-states)
    - 13.2 [Power-Aware Scan Scheduling](#132-power-aware-scan-scheduling)
    - 13.3 [Wake from Sleep](#133-wake-from-sleep)
    - 13.4 [Wake-on-WLAN (WoWLAN)](#134-wake-on-wlan-wowlan)
    - 13.5 [RF-Kill](#135-rf-kill)
14. [Testing Strategy](#14-testing-strategy)
    - 14.1 [Unit Tests](#141-unit-tests)
    - 14.2 [Integration Tests](#142-integration-tests)
    - 14.3 [Hardware Lab Validation](#143-hardware-lab-validation)
    - 14.4 [Regulatory Domain Tests](#144-regulatory-domain-tests)
15. [Implementation Phases](#15-implementation-phases)

---

## 1. Context

The Wi-Fi Backend is the most complex of Nexus's technology backends. Wi-Fi involves asynchronous events at multiple layers: driver-level scan results, supplicant state transitions, authentication handshakes, beacon loss detection, and roaming decisions. The backend must translate all of this into a clean lifecycle model that the core state machine and D-Bus API can present to external consumers without leaking protocol details.

Per [ADR-002](./nexus-architecture.md#43-key-architectural-decisions), Nexus defaults to wpa_supplicant because embedded Wi-Fi modules are certified against it. iwd is supported as an alternative for integrators who have validated it against their specific chipset. The two supplicants differ significantly in their D-Bus APIs and state machines, but the Wi-Fi Backend hides these differences behind a common trait so the rest of Nexus sees a single unified model.

For architectural context, read [nexus-architecture.md](./nexus-architecture.md) first. For how wireless interfaces are discovered and carrier events are produced, read [dd-001-interface-discovery.md](./dd-001-interface-discovery.md). For the pluggable-backend pattern this doc reuses, read [dd-002-ethernet-backend.md](./dd-002-ethernet-backend.md).

### 1.1 Repo Layout

The code for this component lives at:

```
crates/
  nexus-wifi/               <- the Wi-Fi Backend component
    Cargo.toml
    src/
      lib.rs                <- entry point (spawn_wifi_backend)
      backend.rs            <- WifiBackend struct, event loop
      lifecycle.rs          <- WifiInterfaceState machine (§3)
      supplicant/           <- pluggable supplicant trait
        mod.rs              <- WifiSupplicantBackend trait, shared types
        wpa_supplicant.rs   <- WpaSupplicantBackend (default)
        iwd.rs              <- IwdBackend (alternative; feature-gated)
        mock.rs             <- for tests
      scan.rs               <- ScanScheduler, scan result cache (§5)
      select.rs             <- profile matching / network selection (§6)
      roam.rs               <- roaming logic (§7)
      security.rs           <- security mode matching (§8)
      profile.rs            <- per-SSID profile loading
      retry.rs              <- connection retry policy, BSSID blacklist
      power.rs              <- power-state-aware scheduling (§13)
      metrics.rs
      config.rs
    tests/
      select_tests.rs       <- profile matching coverage (§14.1)
      hwsim_integration.rs  <- hostapd + hwsim fixture (§14.2)
```

Shared types reused from other crates:

- `nexus-core` — `NexusEvent`, `InterfaceInfo`, `InterfaceKind::Wireless`, `OperState`.
- `nexus-auth-eap` — `Dot1xEapConfig` (same struct used by DD-002 for wired 802.1X). WPA2/WPA3-Enterprise reuses this type; the Wi-Fi backend translates it into supplicant-specific D-Bus args.

Cargo features gate the supplicant backends:

```toml
[features]
default = ["wifi-wpa_supplicant"]
wifi-wpa_supplicant = ["zbus"]
wifi-iwd = ["zbus"]
```

---

## 2. Responsibilities

The Wi-Fi Backend is responsible for:

1. Registering each wireless station-mode interface with the configured supplicant.
2. Driving scans, either on-demand (from user/D-Bus requests) or automatically (when no auto-connect profile is connected).
3. Matching scan results against the saved profile store and selecting a network to connect to.
4. Initiating connections and monitoring the supplicant's authentication and key-handshake state machine.
5. Signaling `LinkReady` to the core once the interface is associated, authenticated, and has carrier.
6. Monitoring signal quality and triggering roaming to better BSSes for the same SSID when available.
7. Handling disconnection, AP loss, and key rotation failures.
8. Translating supplicant-specific events into unified Nexus events.
9. Gracefully handling supplicant crashes and restarts.

The Wi-Fi Backend is explicitly **not** responsible for:

- Implementing EAPOL, EAP, or WPA/WPA2/WPA3 key derivation — the supplicant does that.
- IP configuration — systemd-networkd handles that.
- Regulatory domain management — the kernel (via CRDA or the in-kernel regdb) handles that.
- AP mode, mesh mode, or P2P — out of scope for v0.1.

---

## 3. Interface Lifecycle

### 3.1 States

```rust
enum WifiInterfaceState {
    /// Registered with supplicant, no network selected.
    /// This is the entry point after a successful InterfaceDiscovered
    /// followed by supplicant.attach().
    Idle,

    /// Scan in progress.
    Scanning,

    /// Connection attempt in progress (pre-authentication).
    Connecting { bssid: MacAddr, ssid: Ssid },

    /// Authenticating (WPA2/WPA3 personal 4-way start, or EAP for Enterprise).
    Authenticating { bssid: MacAddr, ssid: Ssid },

    /// 4-way handshake in progress (WPA2/WPA3).
    Handshaking { bssid: MacAddr, ssid: Ssid },

    /// Connected, carrier up, associated. Ready for IP.
    Connected {
        bssid: MacAddr,
        ssid: Ssid,
        frequency: u32,
        signal_dbm: i32,
        security: SecurityMode,
    },

    /// Evaluating or executing a roam to a better BSS.
    Roaming { from: MacAddr, to: MacAddr, ssid: Ssid },

    /// Disconnected with a specific reason.
    /// Transitions to Idle after a cool-down (§6.4) or remains here
    /// indefinitely if the reason is permanent (e.g. CredentialsInvalid).
    Disconnected { reason: DisconnectReason },

    /// Interface removed.
    Gone,
}
```

**iwd state collapse.** When the iwd supplicant backend is in use, the separate `Authenticating` and `Handshaking` states are never observed — iwd's `Station.State` exposes only `connecting` for the whole pre-`connected` span, and the Wi-Fi Backend faithfully reports that as `Connecting` for the full window. See §10.4 for the translation table. This is a minor loss of observability with no functional impact; the interface still transitions to `Connected` on success and `Disconnected` on failure, same as with wpa_supplicant.

Interfaces enter the state machine at `Idle` immediately after the Wi-Fi backend successfully calls `supplicant.attach()` in response to `InterfaceDiscovered`. The time spent between discovery and attach completion is a transient implementation detail not reflected in this enum.

### 3.2 State Transitions

```
               InterfaceDiscovered + supplicant.attach() ok
 [initial] ────────────────────────────────────────────────►  Idle
                                                               │
                     ┌──────────────────────────┬──────────────┘
                     │ scan trigger             │ profile match on cached scan
                     ▼                          │
                 Scanning                       │
                     │                          │
                     │ scan_done                │
                     ▼                          │
              (match profiles)                  │
                     │                          ▼
                     └─────────────────►  Connecting
                                              │
                                              │ assoc ok
                                              ▼
                                          Authenticating   (wpa_supplicant only;
                                              │              iwd collapses this)
                                              │ auth ok
                                              ▼
                                           Handshaking    (WPA2/WPA3; iwd collapses;
                                              │            skipped for Open/OWE)
                                              │ 4-way done
                                              ▼
                                           Connected ◄──────── roam_to_bssid complete
                                              │
                    ┌─────────────────────────┼─────────────────┐
                    │ signal degraded         │ deauth/disconnect │ carrier lost
                    ▼                         ▼                   ▼
                 Roaming              Disconnected {reason}    Disconnected {reason}
                    │                         │                   │
                    │ success                 │                   │
                    └────────► Connected      │                   │
                                              │                   │
                                              │ cool-down (2s)    │
                                              │ AND reason is     │
                                              │ transient         │
                                              ▼                   ▼
                                            Idle ─────────────► Scanning
                                              ▲                   │
                                              │                   │ no match
                                              └───────────────────┘
```

Notes on the diagram:

- **Permanent-reason Disconnected.** If `reason == CredentialsInvalid` (or any other reason classified as permanent by `DisconnectReason::is_permanent()`), the interface stays in `Disconnected` and does NOT cool down into `Idle`. Operator action (via D-Bus) clears the state.
- **Cool-down.** The 2 s cool-down in `Disconnected → Idle` exists so that a quick deauth/reconnect cycle doesn't produce a retry storm. Configurable via `connect_retry_cooldown_s` (§11.1).
- **Roaming.** On a successful roam, the `to` BSSID replaces `bssid` in the new `Connected` state. On failure, the interface falls back to `Disconnected` and re-enters the retry loop.
- **RF-kill.** Rfkill is a first-class state-machine input. Any rfkill-off edge — hardware switch, operator `Wifi.Powered = false`, or any out-of-band sysfs write — moves any non-`Gone` state to `Disconnected { RfKilled }` via `apply_radio_off`, which also releases dwell timers, in-flight scan/roam handles, and the `signal_info` heartbeat. `RfKilled` is classified permanent (`DisconnectReason::is_permanent() == true`), so the cooldown sweep cannot auto-promote the interface back to `Idle`. The radio-on edge runs `apply_radio_on`, which folds `Disconnected{RfKilled}` to `Idle` and kicks the scan scheduler. See §13.5 for the read/write paths and the full helper set.

### 3.3 Why this is different from Ethernet

Unlike the Ethernet backend, which is essentially reactive to carrier events, the Wi-Fi Backend is **proactively driving the supplicant**. Carrier alone is not enough — a wireless interface may have `IFLA_CARRIER=1` briefly during probing but not actually be associated. The authoritative state comes from the supplicant's own state machine, with carrier used only as a secondary check (carrier transitions to 1 after the 4-way handshake completes on most drivers).

This means the Wi-Fi Backend subscribes to **two event streams** for every interface:

1. **Interface Monitor events** (`CarrierChanged`, `OperstateChanged`, `InterfaceRemoved`) — from the kernel via rtnetlink.
2. **Supplicant D-Bus signals** (`PropertiesChanged`, `ScanDone`, `BSSAdded`, etc.) — from wpa_supplicant or iwd.

The supplicant is the source of truth for connection state. Carrier is used only to detect catastrophic driver/firmware issues where the supplicant reports "connected" but the kernel disagrees.

---

## 4. Supplicant Abstraction

### 4.1 Trait Definition

```rust
/// A Wi-Fi supplicant backend driving an external daemon over D-Bus.
#[async_trait]
pub trait WifiSupplicantBackend: Send + Sync {
    /// Register a wireless interface with the supplicant.
    /// Creates the supplicant-side interface object.
    async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()>;

    /// Unregister a wireless interface from the supplicant.
    async fn detach(&mut self, ifindex: u32) -> Result<()>;

    /// Request an active or passive scan.
    /// Scan results arrive asynchronously via ScanComplete events.
    async fn scan(&mut self, ifindex: u32, params: ScanParams) -> Result<()>;

    /// Read the most recent scan results. Supplicant caches these
    /// between scans.
    async fn get_scan_results(&self, ifindex: u32) -> Result<Vec<BssInfo>>;

    /// Add a network configuration and select it for connection.
    /// Returns an opaque network handle that can be used later.
    async fn connect(
        &mut self,
        ifindex: u32,
        network: &NetworkConfig,
    ) -> Result<NetworkHandle>;

    /// Disconnect from the current network, if any.
    async fn disconnect(&mut self, ifindex: u32) -> Result<()>;

    /// Remove a previously-added network.
    async fn forget_network(
        &mut self,
        ifindex: u32,
        handle: NetworkHandle,
    ) -> Result<()>;

    /// Trigger a roam to a specific BSS or let the supplicant decide.
    async fn roam(
        &mut self,
        ifindex: u32,
        target: RoamTarget,
    ) -> Result<()>;

    /// Query current signal quality (RSSI, noise, rate).
    async fn signal_info(&self, ifindex: u32) -> Result<SignalInfo>;

    /// Backend identifier for logging and metrics.
    fn name(&self) -> &'static str;
}

#[derive(Debug, Clone)]
pub struct ScanParams {
    pub ssids: Vec<Ssid>,        // empty = broadcast scan
    pub frequencies: Vec<u32>,    // empty = all supported
    pub active: bool,             // true = probe requests, false = passive
    /// Whether the underlying supplicant may use this scan's results to
    /// autonomously switch BSSes. Set `true` only when the roaming mode is
    /// `supplicant` (§7.1) or when explicitly driving a roam evaluation.
    pub allow_roam: bool,
}

#[derive(Debug, Clone)]
pub struct BssInfo {
    pub bssid: MacAddr,
    pub ssid: Ssid,
    pub frequency: u32,
    pub signal_dbm: i32,
    pub capabilities: BssCapabilities,
    pub security: Vec<SecurityMode>,    // may offer multiple
    pub age_ms: u64,                     // time since last heard
}

pub struct NetworkHandle(pub String);  // opaque identifier

pub enum RoamTarget {
    Auto,                    // supplicant chooses
    Bss(MacAddr),           // roam to specific BSS
}
```

**Trait contract for `connect` and `forget_network`.** Each `connect` call adds a new supplicant-side network entry and returns a fresh `NetworkHandle`. The trait does NOT automatically forget prior handles; the Wi-Fi Backend is responsible for calling `forget_network` on the old handle before issuing a new `connect` to the same interface (see §6.5 for the discipline). Implementations that auto-cleanup would violate this contract because callers need stable handles to refer back to previously-added networks.

### 4.2 Shared Types

The types appearing in the trait signatures above are defined in `nexus-core` (types shared across technologies) or `nexus-wifi` (Wi-Fi-specific). Listed here for reference:

```rust
// In nexus-core (used by Wi-Fi, Bluetooth, and anywhere else MAC-like
// addresses are needed).
pub struct MacAddr(pub [u8; 6]);
// Debug prints as "aa:bb:cc:dd:ee:ff".

// In nexus-wifi.
pub struct Ssid(pub Vec<u8>);
// Raw SSID bytes. 0-32 bytes per 802.11. Not required to be UTF-8.

/// Security mode as advertised by a BSS or negotiated with one.
/// Distinct from SecurityConfig (below, §4.3), which describes what a
/// *profile* requires. SecurityMode describes what a *BSS* supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityMode {
    Open,
    Owe,
    Wep,                      // legacy; not connectable, just recognizable
    Wpa2Psk,
    Wpa3Sae,
    Wpa2Wpa3Transition,       // AP advertises both PSK and SAE
    Wpa2Eap,
    Wpa3Eap,                  // WPA-EAP-SHA256
    Wpa3EapSuiteB192,
}

/// Per-BSS 802.11 capability flags, populated from scan result IEs.
#[derive(Debug, Clone, Default)]
pub struct BssCapabilities {
    pub ht: bool,             // 802.11n
    pub vht: bool,            // 802.11ac
    pub he: bool,              // 802.11ax (Wi-Fi 6)
    pub eht: bool,             // 802.11be (Wi-Fi 7)
    pub ft: bool,              // 802.11r Fast Transition
    pub pmf_required: bool,    // 802.11w MFPR bit
    pub pmf_capable: bool,     // 802.11w MFPC bit
    pub wps: bool,             // WPS advertised (informational only)
}

/// Present-connection signal metrics. Populated via supplicant calls
/// that translate nl80211 NL80211_CMD_GET_STATION responses.
#[derive(Debug, Clone)]
pub struct SignalInfo {
    pub bssid: MacAddr,
    pub rssi_dbm: i32,
    pub noise_dbm: Option<i32>,
    pub snr_db: Option<i32>,
    pub tx_bitrate_mbps: f32,
    pub rx_bitrate_mbps: f32,
    pub frequency: u32,
}

/// Reason for a Disconnected transition, used both in the state
/// machine and in the D-Bus StateChanged details dict.
#[derive(Debug, Clone)]
pub enum DisconnectReason {
    Unspecified,
    ApInitiated,
    AuthExpired,
    LocalRequest,
    Inactivity,
    ProtocolError,
    HandshakeTimeout,
    EapFailure,
    CredentialsInvalid,        // permanent until operator intervention
    RfKilled,                  // rfkill asserted (§13.5)
    SupplicantUnavailable,
    PostSleepRecovery,         // transient; from wake (§13.3)
    DriverWedge,                // detected per §12.4
    Other(String),
}

impl DisconnectReason {
    /// True if the reason keeps the interface in Disconnected
    /// until operator intervention rather than cooling down to Idle.
    pub fn is_permanent(&self) -> bool {
        matches!(self, DisconnectReason::CredentialsInvalid)
    }
}
```

### 4.3 Network Configuration

```rust
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub ssid: Ssid,
    pub hidden: bool,                      // scan for hidden SSID
    pub security: SecurityConfig,
    pub priority: i32,                     // higher wins when multiple match
    pub bssid_preferred: Option<MacAddr>,  // prefer this BSS if available
    pub bssid_blacklist: Vec<MacAddr>,     // never connect to these
    pub scan_freqs: Vec<u32>,              // limit scans to these frequencies
}

#[derive(Debug, Clone)]
pub enum SecurityConfig {
    Open,
    /// Opportunistic Wireless Encryption; encrypted open network.
    Owe,
    Wpa2Personal { psk: WpaPsk },
    Wpa3Personal { passphrase: SecretString },
    /// WPA2/WPA3 transition mode. Must be a passphrase — transition mode
    /// cannot use a raw pre-shared key because SAE derives its own.
    Wpa2Wpa3Personal { passphrase: SecretString },
    /// WPA2-Enterprise. PMF is capable (optional) — accepts whatever the
    /// AP advertises.
    Wpa2Enterprise(Dot1xEapConfig),
    /// WPA3-Enterprise. PMF is required. Requires stronger key management
    /// (WPA-EAP-SHA256 or SUITE-B-192).
    Wpa3Enterprise(Dot1xEapConfig),
}

#[derive(Debug, Clone)]
pub enum WpaPsk {
    Passphrase(SecretString),   // 8-63 ASCII chars, supplicant derives PSK
    RawPsk([u8; 32]),          // 64 hex chars, pre-computed; personal only
}
```

### 4.4 Backend Selection

Same pattern as DD-002. Selected at startup from configuration:

```toml
# /etc/nexus/nexus.toml
[wifi]
backend = "wpa_supplicant"   # default; "iwd" is the alternative
```

Cargo features:

```toml
[features]
default = ["wifi-wpa_supplicant"]
wifi-wpa_supplicant = ["zbus"]
wifi-iwd = ["zbus"]
```

Both backends can be compiled in simultaneously, with runtime selection via config. Embedded builds can exclude either feature to minimize binary size.

### 4.5 Why the abstraction is thicker than the Ethernet one

The `WiredAuthBackend` in DD-002 has a narrow surface: attach, authenticate, detach. `WifiSupplicantBackend` is broader because Wi-Fi has fundamentally more operations: scanning, network management, roaming, signal polling. There is no practical way to make this narrower without baking wpa_supplicant-specific semantics into the trait.

The trade-off accepted here: the trait is somewhat leaky (e.g., `NetworkHandle` is an opaque string because wpa_supplicant and iwd identify networks differently), but it's a reasonable fit for both backends without forcing either into awkward shapes.

---

## 5. Scanning

### 5.1 Scan Triggers

Scans are triggered by:

1. **Initial startup** — one broadcast scan after the supplicant interface is attached, to populate the scan cache.
2. **No auto-connect match** — if no configured network matches any visible BSS, scan periodically (default: every 60 seconds) to pick up networks that come into range.
3. **User/D-Bus request** — external callers can request a scan via the Nexus D-Bus API.
4. **Roaming trigger** — when signal degrades below a threshold, scan for alternate BSSes of the current SSID (see [Section 7](#7-roaming)).
5. **Hidden SSID probe** — for profiles with `hidden = true`, include the SSID in active scan probe requests.

### 5.2 Scan Parameters

```rust
pub struct ScanParams {
    pub ssids: Vec<Ssid>,
    pub frequencies: Vec<u32>,
    pub active: bool,
}
```

- **Broadcast scan:** `ssids = []`, `active = true`. Probe requests with no SSID; all APs respond.
- **Directed scan for hidden SSID:** `ssids = [hidden_ssid]`, `active = true`. Probe requests with specific SSID; hidden APs respond.
- **Passive scan:** `active = false`. Listen only, no probe requests. Required on some regulatory domains for certain channels (e.g., DFS channels in 5 GHz).
- **Targeted scan for roaming:** `frequencies = [current_freq, neighbor_freqs...]`. Limits scan to likely channels, reducing scan time.

### 5.3 Scan Results Processing

On `ScanComplete`, the backend:

1. Fetches the scan results from the supplicant.
2. Updates an internal BSS cache keyed by `(ifindex, bssid)`.
3. Emits `WifiScanComplete { ifindex, results }` on the Nexus event bus.
4. If the interface state is `Idle` or in a retry phase, evaluates profile matches (see [Section 6](#6-network-selection-and-connection)).
5. If the interface state is `Connected` and roaming is in scope, evaluates roam candidates.

The BSS cache is used by the D-Bus API to respond to `GetScanResults` calls without re-scanning.

### 5.4 Scan Scheduling

To avoid excessive power use on battery-powered devices, scheduled scans use an adaptive interval:

```rust
struct ScanScheduler {
    base_interval: Duration,       // e.g., 60s
    max_interval: Duration,         // e.g., 10min
    current_interval: Duration,
    next_scan: Instant,
    consecutive_empty: u32,
}

impl ScanScheduler {
    /// Call when a scan completes. `matched_profile` is true if the scan
    /// led to a connection attempt against a known profile.
    fn on_scan_complete(&mut self, matched_profile: bool) {
        if matched_profile {
            self.current_interval = self.base_interval;
            self.consecutive_empty = 0;
        } else {
            self.consecutive_empty += 1;
            self.current_interval = (self.current_interval * 2).min(self.max_interval);
        }
        self.next_scan = Instant::now() + self.current_interval;
    }

    /// Returns None if scanning is paused (e.g., sleep power state).
    /// Otherwise returns the Instant at which the next scan should run.
    fn next_scan_at(&self, power_state: PowerState) -> Option<Instant> {
        match power_state {
            PowerState::Sleep => None,
            PowerState::Active => Some(self.next_scan),
            PowerState::Background => Some(self.next_scan + self.current_interval),
        }
    }
}
```

The scheduler is integrated into the Wi-Fi backend's `tokio::select!` loop via a `tokio::time::sleep_until(scheduler.next_scan_at(...))` arm analogous to the `EthtoolPollerSet` pattern in [DD-001 §8](./dd-001-interface-discovery.md#8-concurrency-model). Scheduled scanning is further suspended while `WifiInterfaceState::Connected` unless the roaming mode is `nexus` and signal has degraded.

Scheduled scanning can be further deferred when the device is in sleep state (see [Section 13](#13-power-management)).

---

## 6. Network Selection and Connection

### 6.1 Profile Matching

A `WifiProfile` is the in-memory form of a stored profile (after loading from the Profile Store and decrypting credentials). It is the *input* to matching; `NetworkConfig` in §4.3 is the *output* passed to the supplicant trait.

```rust
/// In-memory profile loaded from the Profile Store.
#[derive(Debug, Clone)]
pub struct WifiProfile {
    pub ssid: Ssid,
    pub hidden: bool,
    pub security: SecurityConfig,
    pub priority: i32,
    pub auto_connect: bool,
    pub bssid_preferred: Option<MacAddr>,
    pub bssid_blacklist: Vec<MacAddr>,
    pub fast_transition: bool,
    pub scan_freqs: Vec<u32>,
    /// Set by the retry subsystem; do not modify directly.
    pub credentials_invalid: bool,
}

impl WifiProfile {
    /// Produce a NetworkConfig for passing to the supplicant.
    pub fn to_network_config(&self) -> NetworkConfig {
        NetworkConfig {
            ssid: self.ssid.clone(),
            hidden: self.hidden,
            security: self.security.clone(),
            priority: self.priority,
            bssid_preferred: self.bssid_preferred,
            bssid_blacklist: self.bssid_blacklist.clone(),
            scan_freqs: self.scan_freqs.clone(),
        }
    }
}
```

When a scan completes and the interface is `Idle`, the backend matches visible BSSes against loaded profiles:

```rust
fn select_network(
    profiles: &[WifiProfile],
    visible_bsses: &[BssInfo],
    paused: &HashSet<Ulid>,    // see DD-006 §6.3 Wifi.Disconnect(pause_auto_connect)
) -> Option<(WifiProfile, BssInfo)> {
    let mut candidates: Vec<(WifiProfile, BssInfo)> = Vec::new();

    for profile in profiles {
        if !profile.auto_connect { continue; }
        if profile.credentials_invalid { continue; }
        if paused.contains(&profile.id) { continue; }

        for bss in visible_bsses {
            if bss.ssid != profile.ssid { continue; }
            if !security_compatible(&profile.security, &bss.security) { continue; }
            if profile.bssid_blacklist.contains(&bss.bssid) { continue; }
            candidates.push((profile.clone(), bss.clone()));
        }
    }

    // Rank candidates (lexicographic sort, descending):
    //   1. Profile priority — higher wins.
    //   2. Preferred-BSSID match — true beats false.
    //   3. last_connected_at — most-recent successful Connected
    //      first. None sorts last so a known-good profile beats a
    //      stranger even if the stranger's RSSI is briefly stronger.
    //   4. Signal strength — stronger RSSI wins; final tiebreaker
    //      among never-connected profiles.
    candidates.sort_by(|(p1, b1), (p2, b2)| {
        let pref = |p: &WifiProfile, b: &BssInfo| -> bool {
            p.bssid_preferred.as_ref().is_some_and(|pref| pref == &b.bssid)
        };
        p2.priority.cmp(&p1.priority)
            .then_with(|| pref(p2, b2).cmp(&pref(p1, b1)))
            .then_with(|| match (p1.last_connected_at, p2.last_connected_at) {
                (Some(t1), Some(t2)) => t2.cmp(&t1),  // newer first
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            .then_with(|| b2.signal_dbm.cmp(&b1.signal_dbm))
    });

    candidates.into_iter().next()
}
```

`last_connected_at` is stamped on every successful Connected transition (§6.5) and persisted to the profile store via `ProfileStore::set_last_connected` so it survives daemon restart and rebooting. The `paused` set is runtime-only — operator intent from `Wifi.Disconnect(pause_auto_connect=true)` (DD-006 §6.3); it does not survive a daemon restart.

### 6.2 Connection Sequence

```
Nexus                  Supplicant               AP                    RADIUS (Enterprise)
  │                        │                     │                            │
  │ select_network()        │                     │                            │
  │ returns (profile, bss)  │                     │                            │
  │                        │                     │                            │
  │ state=Connecting        │                     │                            │
  │                        │                     │                            │
  │ connect(NetworkConfig) │                     │                            │
  ├───────────────────────►│ Auth Req            │                            │
  │                        ├────────────────────►│                            │
  │                        │ Auth Resp           │                            │
  │                        │◄────────────────────┤                            │
  │                        │ Assoc Req           │                            │
  │                        ├────────────────────►│                            │
  │                        │ Assoc Resp          │                            │
  │                        │◄────────────────────┤                            │
  │                        │                     │                            │
  │ (Enterprise only)      │ EAPOL exchange     │ RADIUS Access-Request     │
  │                        ├────────────────────►├───────────────────────────►│
  │                        │ EAP-Success         │ RADIUS Access-Accept       │
  │                        │◄────────────────────┤◄───────────────────────────┤
  │                        │                     │                            │
  │ state=Authenticating   │                     │                            │
  │◄───── PropertiesChanged┤                     │                            │
  │                        │ 4-way handshake     │                            │
  │                        │◄───────────────────►│ (PTK installed)            │
  │                        │                     │                            │
  │ state=Handshaking      │                     │                            │
  │◄───── PropertiesChanged┤                     │                            │
  │                        │ GTK msg             │                            │
  │                        │◄────────────────────┤                            │
  │                        │                     │                            │
  │ state=Connected        │                     │                            │
  │◄───── PropertiesChanged┤                     │                            │
  │ (State=completed)      │                     │                            │
  │                        │                     │                            │
  │ wait IFLA_CARRIER=1     │                     │                            │
  │ (kernel confirmation)  │                     │                            │
  │                        │                     │                            │
  │ emit WifiLinkReady     │                     │                            │
  ▼                        │                     │                            │
```

The middle phases (Authenticating, Handshaking) are distinct for wpa_supplicant but collapsed into Connecting when using the iwd backend (see §10.4). For `SecurityConfig::Open` and `SecurityConfig::Owe`, there is no EAPOL exchange; for `Open` there is no 4-way handshake either.

### 6.3 Failure Modes During Connection

| Supplicant event | Nexus action |
|---|---|
| Association timeout (no response from AP) | Blacklist BSSID for 60s, retry scan, try next candidate |
| Authentication failure (bad PSK) | Mark profile as `credentials_invalid`, notify operator via D-Bus signal, do not retry same profile |
| 4-way handshake timeout | Retry up to 3 times, then blacklist BSSID and try next candidate |
| EAP-Failure (Enterprise) | Same as wired 802.1X — fail fast for credential errors, retry with backoff for server errors |
| Driver/firmware error | Log, blacklist BSSID for 5min, retry |

BSSID blacklisting is time-limited, in-memory state. It does not persist across restarts. A profile-level `credentials_invalid` flag does persist until the operator updates the profile.

### 6.4 Connection Policy

The backend maintains a single "intended" network per interface. When a profile is in `Connected` state, scheduled scans are suspended (unless roaming is being evaluated). When disconnection occurs, the backend:

1. Emits `WifiLinkLost`.
2. Transitions to `Disconnected { reason }`.
3. After a short delay (default 2s), transitions to `Idle` and triggers a new scan.
4. If `reason` indicates a permanent issue (e.g., `CredentialsInvalid`), waits for operator intervention instead of retrying.

### 6.5 Network Handle Lifecycle

wpa_supplicant and iwd both accumulate network configurations over time if the backend is not careful. Repeated connection attempts to the same profile would otherwise leave stale entries in the supplicant that consume memory and can cause ambiguous `SelectNetwork` behavior.

Nexus's policy: **at most one active `NetworkHandle` per (ifindex, profile) pair at any time.**

Concretely:

- `WifiBackend` tracks `active_handle: Option<(ProfileId, NetworkHandle)>` per interface.
- On a new `connect()` call for an interface:
  1. If `active_handle` is `Some`, call `supplicant.forget_network(ifindex, old_handle)` to remove the prior configuration.
  2. Call `supplicant.connect(ifindex, network)`; store the returned handle in `active_handle`.
- On `detach()` (interface removal or supplicant restart), the handle is implicitly invalidated; `active_handle` is cleared without a `forget_network` call.
- On clean disconnect initiated by Nexus, the handle is kept so the same network can be reselected quickly; it is only forgotten when a *different* profile is selected.

This keeps the supplicant-side network list bounded by the number of distinct profiles connected during the lifetime of the interface, not by the number of connection attempts.

---

## 7. Roaming

### 7.1 Roaming Modes

Three roaming modes are supported, selectable per-interface:

- **`off`** — Never roam. Once connected, stay on the same BSS until disconnection.
- **`supplicant`** — Let the supplicant decide. Both wpa_supplicant and iwd have internal roaming logic based on signal thresholds.
- **`nexus`** — Nexus drives roaming explicitly. Periodically scans and instructs the supplicant to roam when a significantly better BSS is found.

Default is `supplicant` for most use cases. `nexus` mode is useful when the integrator wants to apply custom roaming policy (e.g., prefer specific BSSes by policy, avoid congested channels).

### 7.2 Signal Monitoring

When `Connected`, the backend polls signal information at a configurable interval (default 5s). Signal data is obtained from the supplicant, which in turn reads it from the kernel via nl80211 `NL80211_CMD_GET_STATION`.

```rust
pub struct SignalInfo {
    pub bssid: MacAddr,
    pub rssi_dbm: i32,
    pub noise_dbm: Option<i32>,
    pub snr_db: Option<i32>,
    pub tx_bitrate_mbps: f32,
    pub rx_bitrate_mbps: f32,
    pub frequency: u32,
}
```

Signal readings are emitted as `WifiSignalPoll` events on the Nexus event bus. The D-Bus API exposes the latest reading on the interface's `Wireless.Signal` property.

### 7.3 Roam Trigger (Nexus-driven Mode)

```rust
async fn evaluate_roam(&mut self, ifindex: u32) -> Result<()> {
    // Pull the values we need out of the Connected state by borrowing.
    // WifiInterfaceState is not Copy (contains Ssid, SecurityMode).
    let (ssid, signal_dbm) = {
        let entry = self.interfaces.get(&ifindex).context("no entry")?;
        let WifiInterfaceState::Connected { ref ssid, signal_dbm, .. } = entry.state
        else {
            return Ok(()); // not connected, nothing to roam from
        };
        (ssid.clone(), signal_dbm)
    };

    if signal_dbm > self.config.roam_trigger_dbm {
        return Ok(()); // signal is good, don't roam
    }

    let frequencies = self.get_likely_frequencies(ifindex).await?;

    // Trigger a directed scan on same SSID
    self.supplicant.scan(ifindex, ScanParams {
        ssids: vec![ssid],
        frequencies,
        active: true,
    }).await?;

    // Scan results arrive via WifiScanComplete event; roam evaluation
    // continues in on_scan_complete_while_connected().
    Ok(())
}

async fn on_scan_complete_while_connected(
    &mut self,
    ifindex: u32,
    results: Vec<BssInfo>,
) -> Result<()> {
    let (current_bssid, current_ssid, current_rssi) = {
        let entry = self.interfaces.get(&ifindex).context("no entry")?;
        let WifiInterfaceState::Connected { bssid, ref ssid, signal_dbm, .. } = entry.state
        else {
            return Ok(());
        };
        (bssid, ssid.clone(), signal_dbm)
    };

    let best = results.iter()
        .filter(|b| b.ssid == current_ssid)
        .filter(|b| b.bssid != current_bssid)
        .max_by_key(|b| b.signal_dbm);

    let Some(candidate) = best else { return Ok(()); };

    // Only roam if candidate is significantly better
    if candidate.signal_dbm < current_rssi + self.config.roam_hysteresis_db {
        return Ok(());
    }

    self.supplicant.roam(ifindex, RoamTarget::Bss(candidate.bssid)).await?;
    Ok(())
}
```

Default thresholds: `roam_trigger_dbm = -75`, `roam_hysteresis_db = 8`. These are conservative and can be tuned per-deployment.

**`get_likely_frequencies(ifindex)`** returns a pruned list of channels to scan for a roam, rather than the full PHY-supported set. It is the union of: (a) channels on which any BSS with the current SSID has been seen in the last 10 minutes, and (b) the common 2.4 GHz channels 1/6/11 and the 5 GHz UNII-1 band when the wiphy advertises support. The pruning reduces scan time — a full-spectrum scan takes 3–5 seconds on a typical chipset, while a targeted scan of 4–6 channels completes in under a second, which matters a lot for roaming latency.

### 7.4 Fast BSS Transition (802.11r)

If the PHY capabilities indicate FT support and the profile opts in (`fast_transition = true`), the supplicant handles the fast BSS transition handshake internally. Nexus simply initiates the roam; the supplicant takes care of MD-ID, PMK-R0/R1 caching, and the reduced-latency handshake.

FT requires compatible APs in the same mobility domain. If FT negotiation fails, the supplicant falls back to full re-authentication. Nexus treats this transparently — the `Roaming` state covers both.

---

## 8. Security Modes

### 8.1 Supported Modes

| Mode | `SecurityConfig` variant | Key Management | PMF |
|---|---|---|---|
| Open | `Open` | `NONE` | disabled |
| OWE | `Owe` | `OWE` | required |
| WPA2-Personal | `Wpa2Personal` | `WPA-PSK` | capable |
| WPA3-Personal | `Wpa3Personal` | `SAE` | required |
| WPA2/WPA3 transition | `Wpa2Wpa3Personal` | `WPA-PSK SAE` | required |
| WPA2-Enterprise | `Wpa2Enterprise(Dot1xEapConfig)` | `WPA-EAP` | capable |
| WPA3-Enterprise | `Wpa3Enterprise(Dot1xEapConfig)` | `WPA-EAP-SHA256` | required |

### 8.2 PMF (Protected Management Frames) Policy

PMF (802.11w) has three settings in the supplicant: `0` (disabled), `1` (capable/optional), `2` (required). Nexus's policy:

- **`disabled`** — only for `Open` networks.
- **`capable`** (`1`) — accepts APs whether they advertise PMF or not; uses PMF when the AP supports it. Used for WPA2-Personal and WPA2-Enterprise.
- **`required`** (`2`) — refuses to associate with APs that don't support PMF. Used for WPA3, OWE, and transition mode. This matches the Wi-Fi Alliance's WPA3 certification requirements.

WPA3-Enterprise also supports the "192-bit" (SUITE-B-192) mode for high-security deployments. The supplicant selects SUITE-B-192 automatically when both sides advertise it, so Nexus does not need a separate `SecurityConfig` variant. If a deployment needs to *force* SUITE-B-192 and reject weaker negotiations, the integrator can override `ieee80211w` and `key_mgmt` via a backend-specific override — out of scope for v0.1.

### 8.3 Security Compatibility Matching

During profile matching, a profile's `SecurityConfig` variant is matched against each BSS's advertised capabilities. For example:

- Profile `Wpa3Personal` matches a BSS advertising SAE.
- Profile `Wpa2Wpa3Personal` matches a BSS advertising WPA2-PSK, SAE, or both.
- Profile `Wpa2Personal` matches a BSS advertising WPA2-PSK but **not** a BSS advertising only SAE.
- Profile `Wpa2Enterprise(...)` matches a BSS advertising WPA-EAP (WPA2 802.11i).
- Profile `Wpa3Enterprise(...)` matches a BSS advertising WPA-EAP-SHA256 (WPA3-Enterprise).

This prevents the backend from attempting connections that will fail due to mismatched crypto requirements.

### 8.4 Enterprise Configuration

Both `Wpa2Enterprise` and `Wpa3Enterprise` carry the same `Dot1xEapConfig` struct defined in [DD-002 §5.2](./dd-002-ethernet-backend.md#52-configuration-struct), keeping the EAP configuration format uniform between wired and wireless 802.1X. The backend converts this into supplicant-specific D-Bus arguments when connecting.

---

## 9. wpa_supplicant Backend (Default)

### 9.1 D-Bus Service

Same service name as wired 802.1X in DD-002: `fi.w1.wpa_supplicant1`. The Wi-Fi Backend and Ethernet Backend share the D-Bus connection where possible, but register interfaces independently. Wi-Fi uses `Driver: "nl80211"` while wired uses `Driver: "wired"`.

### 9.2 Attach

```rust
async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()> {
    let root = WpaSupplicant1Proxy::new(&self.conn).await?;

    let path = match root.create_interface(HashMap::from([
        ("Ifname".into(), Value::from(ifname)),
        ("Driver".into(), Value::from("nl80211")),
    ])).await {
        Ok(p) => p,
        Err(e) if is_interface_exists_error(&e) => {
            root.get_interface(ifname).await?
        }
        Err(e) => return Err(e.into()),
    };

    let iface = Interface1Proxy::builder(&self.conn)
        .path(path.clone())?
        .build().await?;

    // Subscribe to all relevant signals. Each call returns a stream that
    // yields D-Bus signal events.
    let state_stream = iface.receive_properties_changed().await?;
    let scan_done_stream = iface.receive_scan_done().await?;
    let bss_added_stream = iface.receive_bss_added().await?;
    let bss_removed_stream = iface.receive_bss_removed().await?;
    let network_req_stream = iface.receive_network_request().await?;

    // Spawn a background task that multiplexes these five streams and
    // translates each signal into a NexusEvent on self.event_tx:
    //   PropertiesChanged(State)    -> WifiStateChanged (see §9.5 mapping)
    //   PropertiesChanged(BSSs)     -> (update local BSS cache)
    //   ScanDone                    -> WifiScanComplete (after reading BSSs)
    //   BSSAdded / BSSRemoved       -> update local BSS cache
    //   NetworkRequest              -> credential-refresh flow (e.g., OTP)
    // The task exits when any stream closes (typically on detach).
    self.spawn_signal_handlers(
        ifindex,
        state_stream,
        scan_done_stream,
        bss_added_stream,
        bss_removed_stream,
        network_req_stream,
    );

    self.registered.insert(ifindex, RegisteredInterface {
        ifname: ifname.to_string(),
        dbus_path: path,
        // iface proxy is not stored; zbus proxies are cheap to reconstruct
        // from the path when needed by other methods. Storing them risks
        // holding stale references if the D-Bus connection drops.
        known_networks: HashMap::new(),
    });
    Ok(())
}
```

### 9.3 Scan

```rust
async fn scan(&mut self, ifindex: u32, params: ScanParams) -> Result<()> {
    let entry = self.registered.get(&ifindex).context("not attached")?;
    let iface = Interface1Proxy::builder(&self.conn)
        .path(&entry.dbus_path)?
        .build().await?;

    let mut args = HashMap::new();
    args.insert("Type".into(), Value::from(if params.active { "active" } else { "passive" }));

    // AllowRoam controls whether wpa_supplicant may use this scan's results
    // to autonomously switch BSSes. Only permit when Nexus is either in
    // "supplicant" roaming mode (delegated to the supplicant) or explicitly
    // performing a roam evaluation scan. For "off" and "nexus" modes during
    // normal scans, AllowRoam must be false or wpa_supplicant will compete
    // with Nexus's own roaming decisions.
    args.insert("AllowRoam".into(), Value::from(params.allow_roam));

    if !params.ssids.is_empty() {
        let ssids: Vec<Value> = params.ssids.iter()
            .map(|s| Value::from(s.as_bytes()))
            .collect();
        args.insert("SSIDs".into(), Value::from(ssids));
    }
    if !params.frequencies.is_empty() {
        args.insert("Channels".into(), Value::from(
            params.frequencies.iter().map(|f| Value::from(*f)).collect::<Vec<_>>()
        ));
    }

    iface.scan(args).await?;
    Ok(())
}
```

Scan completion triggers `ScanDone` signal. Results are read by walking the `BSSs` property (array of object paths, each with `SSID`, `BSSID`, `Signal`, `Frequency`, `RSN`, `WPA` properties).

### 9.4 Connect

```rust
async fn connect(
    &mut self,
    ifindex: u32,
    config: &NetworkConfig,
) -> Result<NetworkHandle> {
    let entry = self.registered.get_mut(&ifindex).context("not attached")?;
    let iface = Interface1Proxy::builder(&self.conn)
        .path(&entry.dbus_path)?
        .build().await?;

    let args = build_wpa_network_args(config)?;  // security-specific mapping
    let net_path = iface.add_network(args).await?;
    iface.select_network(&net_path).await?;

    let handle = NetworkHandle(net_path.to_string());
    entry.known_networks.insert(handle.clone(), net_path);
    Ok(handle)
}

fn build_wpa_network_args(config: &NetworkConfig) -> Result<HashMap<String, Value<'static>>> {
    // Note: zbus::Value<'_> borrows strings. Building a HashMap that outlives
    // this function body requires owned strings — the patterns below use
    // Value::from(owned_string) or Value::new_str(&str).to_owned() in the real
    // implementation. Signatures written with Value shown for clarity.
    let mut args = HashMap::new();
    args.insert("ssid".into(), Value::from(config.ssid.as_bytes().to_vec()));

    if config.hidden {
        args.insert("scan_ssid".into(), Value::from(1u32));
    }

    match &config.security {
        SecurityConfig::Open => {
            args.insert("key_mgmt".into(), Value::from("NONE".to_string()));
        }
        SecurityConfig::Owe => {
            args.insert("key_mgmt".into(), Value::from("OWE".to_string()));
            args.insert("ieee80211w".into(), Value::from(2u32));  // PMF required
        }
        SecurityConfig::Wpa2Personal { psk } => {
            args.insert("key_mgmt".into(), Value::from("WPA-PSK".to_string()));
            match psk {
                WpaPsk::Passphrase(p) => {
                    args.insert("psk".into(), Value::from(p.expose_secret().to_string()));
                }
                WpaPsk::RawPsk(bytes) => {
                    args.insert("psk".into(), Value::from(hex::encode(bytes)));
                }
            }
            args.insert("ieee80211w".into(), Value::from(1u32));  // PMF capable
        }
        SecurityConfig::Wpa3Personal { passphrase } => {
            args.insert("key_mgmt".into(), Value::from("SAE".to_string()));
            args.insert("sae_password".into(), Value::from(passphrase.expose_secret().to_string()));
            args.insert("ieee80211w".into(), Value::from(2u32));  // PMF required
        }
        SecurityConfig::Wpa2Wpa3Personal { passphrase } => {
            // Transition mode: offer both key managements. The supplicant
            // picks SAE when the AP advertises it and falls back to PSK
            // otherwise. A single passphrase covers both — SAE derives its
            // own material; PSK-only APs use it as the pre-shared passphrase.
            args.insert("key_mgmt".into(), Value::from("WPA-PSK SAE".to_string()));
            args.insert("psk".into(), Value::from(passphrase.expose_secret().to_string()));
            args.insert("sae_password".into(), Value::from(passphrase.expose_secret().to_string()));
            args.insert("ieee80211w".into(), Value::from(2u32));  // PMF required per WFA transition rules
        }
        SecurityConfig::Wpa2Enterprise(eap) => {
            args.insert("key_mgmt".into(), Value::from("WPA-EAP".to_string()));
            add_eap_args(&mut args, eap)?;
            args.insert("ieee80211w".into(), Value::from(1u32));  // PMF capable
        }
        SecurityConfig::Wpa3Enterprise(eap) => {
            // WPA3-Enterprise requires stronger KM and mandatory PMF.
            // WPA-EAP-SHA256 covers both the "basic" and "192-bit" (SUITE-B)
            // WPA3-Enterprise modes; the AP advertisement determines which
            // cipher suite wpa_supplicant negotiates.
            args.insert("key_mgmt".into(), Value::from("WPA-EAP-SHA256".to_string()));
            add_eap_args(&mut args, eap)?;
            args.insert("ieee80211w".into(), Value::from(2u32));  // PMF required
        }
    }

    if let Some(bssid) = &config.bssid_preferred {
        args.insert("bssid".into(), Value::from(bssid.to_string()));
    }
    if !config.bssid_blacklist.is_empty() {
        let list: Vec<String> = config.bssid_blacklist.iter().map(|b| b.to_string()).collect();
        args.insert("bssid_blacklist".into(), Value::from(list.join(" ")));
    }

    args.insert("priority".into(), Value::from(config.priority));
    Ok(args)
}

/// Translate a Dot1xEapConfig into wpa_supplicant network-block keys.
/// Mirrors the EAP field mapping used for wired 802.1X (DD-002 §6.3) but
/// without the `eapol_flags=0` setting, since wireless EAP does participate
/// in the 4-way handshake.
fn add_eap_args(
    args: &mut HashMap<String, Value<'static>>,
    eap: &Dot1xEapConfig,
) -> Result<()> {
    args.insert("eap".into(), Value::from(eap.eap.clone()));
    args.insert("identity".into(), Value::from(eap.identity.clone()));
    if let Some(anon) = &eap.anonymous_identity {
        args.insert("anonymous_identity".into(), Value::from(anon.clone()));
    }
    if let Some(ca) = &eap.ca_cert {
        args.insert("ca_cert".into(), Value::from(ca.to_string_lossy().into_owned()));
    }
    if let Some(cert) = &eap.client_cert {
        args.insert("client_cert".into(), Value::from(cert.to_string_lossy().into_owned()));
    }
    if let Some(key) = &eap.private_key {
        args.insert("private_key".into(), Value::from(key.to_string_lossy().into_owned()));
    }
    if let Some(passwd) = &eap.private_key_passwd {
        args.insert("private_key_passwd".into(), Value::from(passwd.expose_secret().to_string()));
    }
    if let Some(pw) = &eap.password {
        args.insert("password".into(), Value::from(pw.expose_secret().to_string()));
    }
    if let Some(phase2) = &eap.phase2 {
        args.insert("phase2".into(), Value::from(phase2.clone()));
    }
    if let Some(domain) = &eap.domain_suffix_match {
        args.insert("domain_suffix_match".into(), Value::from(domain.clone()));
    }
    Ok(())
}
```

### 9.5 State Translation

wpa_supplicant's `State` property maps to Nexus `WifiInterfaceState`:

| wpa_supplicant state | Nexus state |
|---|---|
| `disconnected` | `Idle` or `Disconnected` (depending on prior state) |
| `inactive` | `Idle` |
| `scanning` | `Scanning` |
| `authenticating` | `Authenticating` |
| `associating` | `Connecting` |
| `associated` | `Connecting` (pre-4way) |
| `4way_handshake` | `Handshaking` |
| `group_handshake` | `Handshaking` |
| `completed` | `Connected` |

The backend watches `PropertiesChanged` for `State` changes and translates them to Nexus events. On `completed`, it also reads `CurrentBSS` and `CurrentNetwork` to populate the `Connected` variant fields.

**State-read discipline.** Two subtleties bit the initial implementation:

1. `CurrentBSS` and `State` can be emitted in *separate* `PropertiesChanged` signals — observing just one misses the other half of the transition.
2. Some drivers advance wpa_supplicant from `4way_handshake → completed` without firing a `PropertiesChanged` signal at all. Reading from the signal arguments alone leaves the backend stuck in `Handshaking` even though the interface has a lease.

The state watcher is therefore built as a `tokio::select!` over *(a) the `PropertiesChanged` stream filtered to `State | CurrentBSS` changes* and *(b) a 2 s reconciliation tick* (`tokio::time::interval` with `MissedTickBehavior::Delay`). Either trigger calls a shared `evaluate_and_emit` helper that re-reads `State` fresh from the supplicant via D-Bus — never trusting the signal's payload — and emits `WifiStateChanged` only when the translated state actually changed from the backend's last cached value. The 2 s cadence is a cheap safety net; the fresh read is the source of truth.

**Same-association `Connected` re-emit dedup.** The reconciliation tick periodically re-reads `State` and `CurrentBSS` and re-publishes the authoritative value as a guard against known wpa_supplicant `PropertiesChanged` drops. Without further care this looks like a fresh `not-connected → connected` edge to downstream consumers. The fix gates the `LinkReady` fan-out on `prev_connected == false || prev_bssid != new_bssid`: a refresh on the same association is `After::None`, no `WifiLinkReady` is sent, and the cached `signal_dbm` is preserved (rather than being clobbered with the -50 dBm sentinel that the connect path uses to seed the value before the first signal poll). Only a transition that actually crosses associations or emerges from a non-connected state fans out `WifiLinkReady`.

**Background-scan `Connected → Scanning` fold suppression.** wpa_supplicant fires `State=scanning` for both standalone pre-association scans *and* background scans done while connected. DD-003 §3.1 has no `Connected → Scanning` edge. The supplicant-event arm of `on_supplicant_state` keeps the cached state at `Connected` (or `Roaming`) when a `scanning` arrives during an active association — without this, the supplicant's post-scan `State=completed` re-emit looks like a fresh `not-connected → connected` transition, bypassing the dedup above and producing one spurious `WifiLinkReady` per background scan.

**Skip `signal_info` while rfkilled.** The 1 Hz heartbeat that polls `signal_info` against the supplicant proxy is gated by `radio_off`. The supplicant's reply on a rfkilled radio is meaningless and would error every tick — and earlier builds left `last_signal_poll` updated only on `Ok`, which pinned the call rate at 1 Hz on every error. The poll is now updated regardless of outcome, and skipped entirely while `radio_off`. See §13.5.

### 9.6 Disconnect Reason Mapping

On transition to `disconnected`, wpa_supplicant provides `DisconnectReason` — a signed integer where negative values are locally-initiated, positive values are 802.11 reason codes. Key mappings:

| Reason | Meaning | Nexus `DisconnectReason` |
|---|---|---|
| 0 | Unspecified | `Unspecified` |
| 1 | Unspecified reason (AP-initiated) | `ApInitiated` |
| 2 | Previous auth no longer valid | `AuthExpired` |
| 3 | Deauthenticated (STA leaving) | `LocalRequest` |
| 4 | Disassociated due to inactivity | `Inactivity` |
| 6 | Class 2 frame from non-authed STA | `ProtocolError` |
| 15 | 4-way handshake timeout | `HandshakeTimeout` |
| 23 | 802.1X auth failed | `EapFailure` |
| -3 | Local disconnect (Nexus-initiated) | `LocalRequest` |

---

## 10. iwd Backend (Alternative)

This section documents the iwd-specific aspects of the supplicant trait implementation. Operations that follow the same pattern as the wpa_supplicant backend (§9) — `disconnect()`, `forget_network()`, `signal_info()`, the signal-handler task, the disconnect-reason dispatch — are not repeated here. Refer to the corresponding sections in §9 and substitute the iwd D-Bus method names where applicable.

### 10.1 D-Bus Service

iwd exposes `net.connman.iwd` on the system bus. The object hierarchy differs from wpa_supplicant's:

```
net.connman.iwd
  /
    net.connman.iwd.Manager (on ObjectManager)
  /net/connman/iwd/{phy_id}
    net.connman.iwd.Adapter
  /net/connman/iwd/{phy_id}/{station_id}
    net.connman.iwd.Device
    net.connman.iwd.Station   (when in station mode)
  /net/connman/iwd/{phy_id}/{station_id}/{network_id}
    net.connman.iwd.Network
  /net/connman/iwd/{phy_id}/{station_id}/{bssid}
    net.connman.iwd.Bss
```

The conceptual differences that the abstraction has to paper over:

- iwd models `Adapter` (wiphy), `Device` (netdev), and `Station` (station-mode operations) as separate objects. wpa_supplicant folds all of this into the `Interface` object.
- iwd identifies networks by `(SSID, security_type)` tuple via its `Network` objects; wpa_supplicant identifies networks by opaque numeric ID within an interface.
- iwd does not expose a direct `Connect(ssid)` method — you connect to a specific `Network` object obtained from the Station's `GetOrderedNetworks`.

### 10.2 Attach

iwd does not have `CreateInterface`. It automatically detects wireless interfaces via nl80211 and exposes them as `Device` objects. Attach in the iwd backend therefore means:

1. Look up the `Device` object for the given ifindex by walking the ObjectManager.
2. Verify the device is in station mode (`Device.Mode == "station"`); if not, issue a mode change.
3. Obtain the `Station` interface on that object path.
4. Subscribe to `PropertiesChanged` and `GetOrderedNetworks` updates.

```rust
async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()> {
    let om = ObjectManagerProxy::builder(&self.conn)
        .destination("net.connman.iwd")?
        .path("/")?
        .build().await?;

    let objects = om.get_managed_objects().await?;

    // Walk managed objects looking for a Device interface whose Name matches ifname.
    // zbus Value fields require explicit string extraction via Value::downcast_ref
    // (or equivalent) rather than TryInto<String>.
    let device_path = objects.iter()
        .find_map(|(path, ifaces)| {
            let props = ifaces.get("net.connman.iwd.Device")?;
            let name_value = props.get("Name")?;
            let name: &str = name_value.downcast_ref().ok()?;
            (name == ifname).then(|| path.clone())
        })
        .context("device not found in iwd")?;

    // Ensure station mode
    let device = DeviceProxy::builder(&self.conn)
        .path(&device_path)?
        .build().await?;
    if device.mode().await? != "station" {
        device.set_mode("station").await?;
    }

    // The Station object lives on the same path
    let station = StationProxy::builder(&self.conn)
        .path(&device_path)?
        .build().await?;

    let state_stream = station.receive_properties_changed().await?;
    self.spawn_signal_handlers(ifindex, state_stream);

    self.registered.insert(ifindex, RegisteredInterface {
        ifname: ifname.to_string(),
        device_path,
    });
    Ok(())
}
```

### 10.3 Connect

iwd requires that network profiles be written to disk before connecting. Profiles live under `/var/lib/iwd/` with the filename encoding the SSID and security type (e.g., `MyNetwork.psk`, `Corp.8021x`). The backend writes the appropriate file, then instructs iwd to connect to that network.

```rust
async fn connect(
    &mut self,
    ifindex: u32,
    config: &NetworkConfig,
) -> Result<NetworkHandle> {
    let entry = self.registered.get(&ifindex).context("not attached")?;

    // Write the profile to disk
    self.write_iwd_profile(&config).await?;

    // Find the Network object for this SSID + security type
    let station = StationProxy::builder(&self.conn)
        .path(&entry.device_path)?
        .build().await?;

    // GetOrderedNetworks returns (object_path, signal) pairs
    let networks = station.get_ordered_networks().await?;

    let network_path = networks.iter()
        .find(|(path, _)| {
            // Check if the Network object matches our SSID
            self.network_matches_ssid(path, &config.ssid)
        })
        .map(|(p, _)| p.clone())
        .context("network not visible")?;

    let network = NetworkProxy::builder(&self.conn)
        .path(&network_path)?
        .build().await?;

    network.connect().await?;

    Ok(NetworkHandle(network_path.to_string()))
}
```

### 10.4 State Translation

iwd's `Station.State` property values:

| iwd state | Nexus state |
|---|---|
| `disconnected` | `Idle` or `Disconnected` |
| `connecting` | `Connecting` |
| `connected` | `Connected` |
| `disconnecting` | transitioning — no direct Nexus state |
| `roaming` | `Roaming` |

iwd does not expose separate states for the 4-way handshake or authentication phases; `connecting` covers everything from association through key installation. This means the Nexus `Authenticating` and `Handshaking` states are collapsed to `Connecting` when using iwd. This is a minor loss of observability but doesn't affect functional behavior.

### 10.5 Limitations and Caveats

Documented for integrators who choose iwd:

- iwd requires kernel ≥ 4.20 for full feature coverage; some features (PMF, FT) may degrade on older kernels.
- iwd does not support all EAP methods that wpa_supplicant does. If the deployment uses EAP-FAST, EAP-SIM, or EAP-AKA, use wpa_supplicant.
- iwd's built-in DHCP client must be disabled (`[General] EnableNetworkConfiguration=false` in `/etc/iwd/main.conf`) so that systemd-networkd handles IP per Nexus's architecture.
- Certification: see [ADR-002](./nexus-architecture.md#43-key-architectural-decisions). Validate with module vendor before production use.

---

## 11. Configuration

### 11.1 Global Configuration

In `/etc/nexus/nexus.toml`:

```toml
[wifi]
# Backend: "wpa_supplicant" (default) | "iwd"
backend = "wpa_supplicant"

# Scan scheduling
scan_initial_delay_ms = 500
scan_idle_interval_s = 60
scan_idle_max_interval_s = 600

# Roaming
roaming_mode = "supplicant"   # "off" | "supplicant" | "nexus"
roam_trigger_dbm = -75
roam_hysteresis_db = 8
signal_poll_interval_s = 5

# Connection policy
connect_retry_max = 3
bssid_blacklist_duration_s = 60
handshake_timeout_s = 10
```

### 11.2 Per-Profile Configuration

In `/var/lib/nexus/wifi/{ssid_hash}.toml`:

```toml
# WPA2-Personal with passphrase
schema_version = 1
id = "01HPQY8S2N0Z8K9M7V3Y2F4T5W6"   # ULID; see DD-007

[network]
ssid = "MyHomeNetwork"
hidden = false
priority = 10
auto_connect = true
fast_transition = true   # use 802.11r where available

[network.security]
type = "wpa2_personal"
# passphrase stored encrypted per DD-007 §4.4:
passphrase = { enc = "v1", nonce = "...", ct = "..." }

# WPA3-Personal
# [network.security]
# type = "wpa3_personal"
# passphrase = { enc = "v1", nonce = "...", ct = "..." }

# WPA2-Enterprise (EAP-PEAP example)
# [network.security]
# type = "wpa2_enterprise"
# [network.security.eap]
# eap = "PEAP"
# identity = "user@corp.example.com"
# anonymous_identity = "anonymous@corp.example.com"
# password = { enc = "v1", nonce = "...", ct = "..." }
# ca_cert = "/etc/nexus/certs/corp-ca.pem"
# phase2 = "auth=MSCHAPV2"
# domain_suffix_match = "corp.example.com"

# WPA3-Enterprise (EAP-TLS example)
# [network.security]
# type = "wpa3_enterprise"
# [network.security.eap]
# eap = "TLS"
# identity = "device-0001@corp.example.com"
# ca_cert = "/etc/nexus/certs/corp-ca.pem"
# client_cert = "/etc/nexus/certs/device-0001.pem"
# private_key = "/etc/nexus/certs/device-0001.key"
# private_key_passwd = { enc = "v1", nonce = "...", ct = "..." }
# domain_suffix_match = "corp.example.com"
```

See [DD-007: Profile Store](./dd-007-profile-store.md) §3.3 for the canonical profile wrapper (including `schema_version` and `id`), §4.4 for the encrypted-field wire format, and §8 for schema-version semantics.

The `type` field uses a Serde-tagged representation for `SecurityConfig`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SecurityConfig {
    Open,
    Owe,
    Wpa2Personal { #[serde(flatten)] psk: WpaPsk },
    Wpa3Personal { passphrase: SecretString },
    Wpa2Wpa3Personal { passphrase: SecretString },
    Wpa2Enterprise { eap: Dot1xEapConfig },
    Wpa3Enterprise { eap: Dot1xEapConfig },
}
```

Profile filenames use a stable hash of the SSID (not the SSID itself) to support SSIDs with non-filesystem-safe characters and to provide privacy at rest.

---

## 12. Error Handling

### 12.1 Supplicant Crash or Restart

Detected via `NameOwnerChanged` on `fi.w1.wpa_supplicant1` (or `net.connman.iwd`).

- All Wi-Fi interfaces transition to `Disconnected { reason: SupplicantUnavailable }`.
- `WifiLinkLost` is emitted for any interface that was `Connected`.
- The backend subscribes to the bus name and retries `attach()` when the name reappears.
- After re-attach, the backend resumes normal operation: a scan kicks off, profile matching runs, and the previously-connected network is re-selected if still visible.

Credentials and profiles are stored by Nexus, not the supplicant — so recovery doesn't require operator intervention.

### 12.2 Scan Failure

`NL80211_CMD_SCAN_ABORTED` or an error from `Scan()` D-Bus call:

- Log the error.
- If consecutive scan failures exceed threshold (default 5), trigger an interface reset: detach and re-attach.
- If reset also fails, mark the interface as unhealthy and emit a D-Bus signal for operator visibility.

### 12.3 Connection Retry Storm

Prevent pathological retry loops:

- Per-BSSID retry count — after 3 failures on the same BSSID, blacklist for 60s.
- Per-profile credential-invalid flag — after an auth failure, do not retry the same profile until the flag is cleared by the operator (via D-Bus or profile file update).
- Overall connection attempt rate limit — no more than one connection attempt per interface per 2 seconds, regardless of state.

When a profile is marked `credentials_invalid`, the backend emits an operator-visible D-Bus signal on the interface's D-Bus object so UIs can prompt for credential refresh. The signal name and payload are specified in [DD-006: D-Bus API](./dd-006-dbus-api.md) §9.

### 12.4 Driver/Firmware Wedges

Sometimes the Wi-Fi chipset enters a state where the kernel driver is alive but no frames can be transmitted. Detection:

- Supplicant state stuck in `associating` or `4way_handshake` for > 30s.
- `IFLA_OPERSTATE` remains `dormant` despite supplicant reporting `completed`.
- Signal polls return rate = 0 for > 15s while connected.

Recovery: detach the interface from the supplicant, administratively down the interface via rtnetlink (`RTM_NEWLINK` with `ifi_flags` clearing `IFF_UP` and `ifi_change = IFF_UP`), wait 2s, bring it up (`RTM_NEWLINK` with `IFF_UP` set), re-attach to the supplicant. The netlink operations go through the rtnetlink socket owned by the Interface Monitor, requested via a `MonitorCommand::SetAdminUp { ifindex, up }` message on the command channel. This kicks most drivers out of wedged states without requiring a driver reload.

Do not shell out to `ip link set` — the binary is not always present on embedded rootfs, the operation is faster and atomic via netlink, and `unsafe` FFI or subprocess spawning is avoided.

### 12.5 Observability

The Wi-Fi Backend exposes the following metrics:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `nexus_wifi_interfaces_managed` | gauge | `state` | Interfaces by current state (`idle`/`scanning`/`connecting`/`authenticating`/`handshaking`/`connected`/`roaming`/`disconnected`) |
| `nexus_wifi_scans_total` | counter | `ifname`, `type` (`broadcast`/`directed`/`roam`/`hidden`), `outcome` (`success`/`aborted`/`failed`) | Scan attempts by type and outcome |
| `nexus_wifi_scan_duration_seconds` | histogram | `ifname`, `type` | Scan duration |
| `nexus_wifi_bss_cache_entries` | gauge | `ifname` | Current size of per-interface BSS cache |
| `nexus_wifi_connect_attempts_total` | counter | `ifname`, `security`, `outcome` (`success`/`assoc_timeout`/`auth_failure`/`handshake_timeout`/`credentials_invalid`/`other`) | Connection attempt outcomes by security mode |
| `nexus_wifi_connect_duration_seconds` | histogram | `ifname`, `security` | Time from `Connecting` to `Connected` |
| `nexus_wifi_link_ready_total` | counter | `ifname` | `WifiLinkReady` emissions |
| `nexus_wifi_link_lost_total` | counter | `ifname`, `reason` | `WifiLinkLost` emissions |
| `nexus_wifi_signal_dbm` | gauge | `ifname` | Most recent signal reading for the connected BSS |
| `nexus_wifi_roams_total` | counter | `ifname`, `mode` (`supplicant`/`nexus`), `outcome` | Roam attempts |
| `nexus_wifi_bssid_blacklisted` | gauge | `ifname` | Current blacklisted BSSID count |
| `nexus_wifi_profile_credentials_invalid` | gauge | — | Profiles currently marked credentials_invalid |
| `nexus_wifi_supplicant_available` | gauge | `backend` (`wpa_supplicant`/`iwd`) | 1 if the D-Bus name is present, 0 otherwise |
| `nexus_wifi_driver_wedge_recoveries_total` | counter | `ifname` | Driver-wedge recovery attempts (§12.4) |

---

## 13. Power Management

For battery-powered devices, the Wi-Fi Backend integrates with the system's power state to reduce scanning and signal-polling activity when the device is idle.

### 13.1 Power States

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// Full operation. Normal scan intervals, 5s signal polling.
    Active,
    /// User is present but device is idle. Scan intervals doubled,
    /// signal polling every 15s.
    Background,
    /// Device suspended or deep-idle. Scheduled scans paused; only
    /// connect-time scans run. Signal polling paused.
    Sleep,
}
```

Nexus exposes a D-Bus method `fi.nexus.Manager.SetPowerState(state)` that external power management (e.g., a battery service or lid-close handler) can call to transition between these states.

### 13.2 Power-Aware Scan Scheduling

The scheduler described in [Section 5.4](#54-scan-scheduling) respects the current power state:

```rust
fn effective_interval(&self, power_state: PowerState) -> Option<Duration> {
    match power_state {
        PowerState::Active => Some(self.current_interval),
        PowerState::Background => Some(self.current_interval * 2),
        PowerState::Sleep => None,  // paused entirely
    }
}
```

### 13.3 Wake from Sleep

When `SetPowerState` transitions from `Sleep` back to `Active` (or `Background`), the backend:

1. Resets each interface's `ScanScheduler::next_scan` to `Instant::now()` so a scan fires immediately rather than waiting out the remaining interval from before sleep.
2. For any interface in `Connected` state, immediately polls signal once to confirm the link is still alive (kernel state may be stale after a suspend).
3. If signal polling returns an error or the supplicant reports `disconnected`, transitions the interface to `Disconnected { reason: PostSleepRecovery }` and lets the normal reconnection path take over.

The backend does NOT preserve any "pre-sleep scan schedule." It treats wake-up as a fresh start.

### 13.4 Wake-on-WLAN (WoWLAN)

nl80211 supports configuring the Wi-Fi chipset to wake the host on specific events: magic packet, disconnect, or pattern match. Nexus v0.1 does not configure WoWLAN — this is left to the integrator via out-of-band tooling if needed. A future revision may expose WoWLAN configuration via the D-Bus API.

### 13.5 RF-Kill

Independent of the system-wide `PowerState`, each wireless interface has a kernel `rfkill` switch. When rfkill is asserted, the interface is hard-disabled — no scans, no connections, minimal power draw. The `fi.nexus.Wifi.Powered` D-Bus property (DD-006 §6.3) exposes this state.

**Shipped implementation.** `nexus-wifi::rfkill` opens `/dev/rfkill` twice:

- **Read path.** A non-blocking fd wrapped in `tokio::io::unix::AsyncFd` drives a reader task that filters to `RFKILL_TYPE_WLAN` events, resolves each event's `idx` to its wiphy via `/sys/class/rfkill/rfkillN/name`, and emits `RfkillState { wiphy_name, powered }` over an mpsc to the Wi-Fi backend. The kernel sends synthetic `RFKILL_OP_ADD` events at `open()` time, so the initial state of every registered rfkill is captured with no separate sysfs enumeration. Out-of-band kernel events — hardware switches, `rfkill` userspace, direct sysfs writes — flow the same way. The backend republishes each edge as `NexusEvent::WifiRfkillChanged { ifindex, powered }` after resolving wiphy_name → ifindex through its local interface registry; the D-Bus service.rs handler writes that into `WifiInterfaceState.powered`.
- **Write path.** A second, blocking fd holds the write side. `RfkillWriter::set_blocked(wiphy_name, block)` resolves wiphy_name → rfkill idx via `/sys/class/rfkill/rfkill*`, constructs an 8-byte `rfkill_event` with `op = RFKILL_OP_CHANGE`, and issues a single `write(2)` (run inside `tokio::task::spawn_blocking`). `WifiCommand::SetPowered` — sent by `nexus-daemon::wifi_ops` on a D-Bus `Powered` property write — routes through this path.
- **Race seed.** Because the watcher's synthetic ADD events fire before the Wi-Fi backend has observed `InterfaceDiscovered`, the backend re-reads `/sys/class/rfkill/rfkillN/{soft,hard}` on each wifi `InterfaceDiscovered` and emits a synthetic `WifiRfkillChanged` so the initial `Powered` property value is correct.
- **Feature gate.** When the Wi-Fi feature is disabled in `EnabledFeatures`, `fi.nexus.Wifi.Powered` reads `false` unconditionally, matching the "we aren't managing this interface" semantics documented on `check_feature`.

**State-machine integration.** Every rfkill edge — operator `Powered=false`, hardware switch, direct sysfs write, kernel `RFKILL_OP_CHANGE` echoed back from the watcher after a write — runs through a single pair of helpers (`apply_radio_off` / `apply_radio_on`) that move the cached `WifiState` in lockstep with the radio bit. Rfkill is a first-class state-machine input, not just a flag. Specifically:

- **`apply_radio_off`.** Forces the cached state to `Disconnected{RfKilled}`, releases every per-interface slot whose semantics depend on a live radio (`dwell_since`, `connect_started_at`, `active_handle`, cooldowns, `roam_in_flight`, `scan_in_flight`, `last_signal_poll`), and emits `WifiLinkLost{rfkill}` if the prior state was associated. Idempotent — a duplicate edge from the kernel echo is a no-op.
- **`apply_radio_on`.** Folds `Disconnected{RfKilled}` to `Idle` and calls `sched.fire_now(...)` so auto-select resumes on the next scan tick. `DisconnectReason::RfKilled` is classified `is_permanent() == true` precisely so the cooldown sweep cannot auto-promote a rfkilled interface back to Idle on its own; the explicit radio-on edge is the only way out.
- **Entry-point gating.** Every entry point that needs a live radio (`request_scan`, `operator_connect`, `try_connect`, `operator_roam`, `dispatch_roam`) checks `radio_off` first. `operator_connect` returns `WifiError::Rfkill` rather than transitioning to `Connecting{...}`, which would otherwise look like a stuck-firmware signature to the driver-wedge detector after 30 s.
- **Scheduler skip.** `fire_scheduled_scans` and `earliest_scan_deadline` skip rfkilled radios so the run-loop's `select!` arm doesn't tight-loop on a stale scheduler deadline that `request_scan` would only refuse.
- **Late supplicant suppression.** `on_supplicant_state` early-returns when `radio_off`. A `wpa_supplicant Disconnected{LocalRequest|other}` arriving a moment after `Powered=false` (the supplicant noticing the kernel kill) cannot overwrite the authoritative `Disconnected{RfKilled}` reason on the wire.
- **Skip `signal_info` on rfkilled radios.** The 1 Hz heartbeat that polls `signal_info` against the supplicant proxy is suppressed while the radio is off — the supplicant's reply is meaningless and the call would error every tick.
- **Defense in depth.** The D-Bus layer's `Powered=false` clamp (DD-006 §6.3 "Powered=off side effects") is left intact. The DD-006 §12.4 edge-ordering / dedup gate suppresses the duplicate `StateChanged` that arrives a moment later from the backend, and the synthesized D-Bus signal still fires for any out-of-band edge that bypassed the backend (e.g., a sysfs write the watcher delivers before the backend's first interface evaluation).

**Degrade path.** If `/dev/rfkill` can't be opened (container without the device, non-root, kernel without rfkill support), `spawn_wifi_backend` logs a warning and proceeds with the reader/writer unwired. `SetPowered` then returns `fi.nexus.Error.Io` ("rfkill writer not available") and the `Powered` property stays at its default (`false`). This keeps dev builds on container hosts running.

The rfkill state is distinct from `PowerState::Sleep` — rfkill is a hard hardware block; sleep is a software scheduling decision. A device in `Active` power state with rfkill asserted stays blocked; a device in `Sleep` power state with rfkill unblocked only fires connect-time scans.

---

## 14. Testing Strategy

### 14.1 Unit Tests

- Profile matching logic: given a set of profiles and scan results, assert the correct candidate is selected.
- Security compatibility matrix: every (profile security, BSS security) pair checked for compatibility.
- Scan scheduler math: interval progression across success/failure sequences.
- State machine transitions: feed synthetic supplicant events into the lifecycle, assert correct Nexus state at each step.

### 14.2 Integration Tests

- **mac80211_hwsim + hostapd:** Set up a virtual AP with hostapd, a virtual station with mac80211_hwsim, and run Nexus against it. Validate:
  - Open, WPA2-PSK, WPA3-SAE, WPA2-Enterprise connections
  - Scan → match → connect flow
  - Disconnect and reconnect
  - Roaming between two hostapd APs on different channels (same SSID, different BSSIDs)

- **Supplicant crash recovery:** Kill wpa_supplicant mid-connection, verify Nexus transitions to `Disconnected` and reconnects when wpa_supplicant restarts.

- **iwd backend parity:** Run the same test matrix with `backend = "iwd"` to catch behavioral drift.

### 14.3 Hardware Lab Validation

Before release, validate on at least:

- One Intel mac80211 chipset (e.g., AX200)
- One Qualcomm mac80211 chipset (e.g., QCA6174 via ath10k)
- One Broadcom fullmac chipset (e.g., BCM4345 via brcmfmac) — common on Raspberry Pi CM4 and many embedded SOMs
- One TI chipset (e.g., WL1837 via wilink) — common on industrial SOMs

For each, verify WPA2-Personal and WPA3-Personal connections and smooth roaming between two APs.

### 14.4 Regulatory Domain Tests

Verify behavior under different regulatory domains:

- `iw reg set US` and ensure DFS channels (5 GHz) use passive-only scans.
- `iw reg set JP` for Japan-specific channel rules.
- World-roaming (`iw reg set 00`) for most-restrictive defaults.

---

## 15. Implementation Phases

Wi-Fi is the most complex backend. Phases reflect a layered build-up — trait first, default supplicant path next, refinements last. DD-001 must be at phase 5 (event emission) before starting; DD-002 can proceed in parallel.

**Shipped status (v0.x).** Phases 1–9 and 11 landed and are live on the Raspberry Pi reference hardware against wpa_supplicant + BCM4345 (brcmfmac). Phase 10 (iwd backend) is the only phase not started. Phase 11's integration harness is in tree but its automatic execution waits on a privileged CI runner; see §14.2 and the Phase 11 notes below.

### Phase 1 — Skeleton and Lifecycle — **Shipped**

`crates/nexus-wifi/src/backend.rs` and `src/lifecycle.rs`.

- `WifiInterfaceState` enum and transition table (§3).
- `WifiBackend` struct consuming `NexusEvent` from the bus.
- Handle `InterfaceDiscovered { kind: Wireless, .. }` — register in local table, move to `Registered`. No supplicant interaction yet.

**Exit criterion:** Bringing up a wireless interface (e.g., via mac80211_hwsim) causes the backend to register it and emit a log line. Unit tests for the state machine pass.

### Phase 2 — Supplicant Trait + Mock — **Shipped**

`src/supplicant/mod.rs` (trait) + `src/supplicant/mock.rs`.

- Define `WifiSupplicantBackend` trait exactly as in §4.1.
- Implement a mock that can be programmed with scripted scan results, connection outcomes, and state transitions.
- Wire the backend to go through the trait for all supplicant operations.

**Exit criterion:** With the mock, the backend can be driven through scan → match → connect → connected via unit tests. No real D-Bus yet.

### Phase 3 — Profile Store and Matching — **Shipped**

`src/profile.rs` and `src/select.rs`.

- Parse per-SSID profile TOML per §11.2 (plaintext credentials for now; DD-007 adds encryption).
- Implement `select_network()` with priority ordering and BSSID preference (§6.1).
- Unit tests for the matching rules across security modes and priority tiers.
- Hot-reload: the backend subscribes to `NexusEvent::ProfileChanged { kind: Wifi, .. }` (emitted by `nexus-profile-store::fs_store`) and refreshes its in-memory profile table without a daemon restart. `nexusctl profile add-wifi` therefore takes effect immediately.

**Exit criterion:** Given a set of profiles and scan results (as structs), `select_network()` returns the correct candidate for every test case.

### Phase 4 — wpa_supplicant Backend (Default) — **Shipped**

`src/supplicant/wpa_supplicant.rs`. This is the primary production path (§9).

- `attach` via `CreateInterface` with `Driver: "nl80211"`.
- `scan` with active/passive, SSID, channel parameters.
- `connect` / `disconnect` / `forget_network` / `roam` / `signal_info` via the `WpaInterfaceProxy` + `BssProxy` typed zbus bindings.
- Full security-mode coverage through the §9.4 network-dict builder — all seven `SecurityConfig` variants from §8.1, including OWE, WPA3-Personal (SAE), WPA2/WPA3 transition, and both Enterprise flavors.
- FT / 802.11r key-management composition via the `fast_transition` flag on the resolved `NetworkConfig`.
- State translation from wpa_supplicant's `State` property to `WifiInterfaceState` (§9.5), with the `tokio::select!` stream + 2 s reconcile-tick discipline documented there.
- Disconnect reason mapping (§9.6).

**Exit criterion:** Against a hostapd + mac80211_hwsim fixture, Nexus connects and disconnects on every security mode in the test matrix. The hwsim harness is tracked on Phase 11; on real hardware (BCM4345) the WPA2-Personal happy path and forget/reconnect have been verified.

### Phase 5 — Scanning and Scan Scheduling — **Shipped**

`src/scan.rs`.

- Scan triggers (startup, no-match, user request, roaming) per §5.1.
- Adaptive scheduler with backoff per §5.4.
- Scan result cache keyed by `(ifindex, bssid)`.
- `WifiCommand::Scan` carries the typed `ScanParams` (active/passive, SSIDs, channel list) built in `nexus-dbus::backend_ops`; the builder narrows to supplicant D-Bus `SSIDs` + `Channels` entries so clients can direct-probe a specific SSID or sweep a channel subset.

**Exit criterion:** A disconnected device scans at the expected cadence and backs off when no profiles match. Connected devices don't scan except for roaming evaluation.

### Phase 6 — Connection Flow and Failure Handling — **Shipped**

`src/retry.rs` + event handlers in `backend.rs`.

- BSSID blacklist with time-limited entries.
- Credentials-invalid persistent flag.
- Retry rate limit (one attempt per 2s per interface).
- Handshake timeout detection.
- `WifiCommand::{Connect, Disconnect, Roam, SetRoamingMode}` mpsc + `oneshot::Receiver` reply pattern between the D-Bus `BackendOps` adapter (`nexus-daemon::wifi_ops`) and the backend task. Errors (`NoProfileMatch`, `ProfileNotFound`, `NotAttached`, supplicant-busy) map to the D-Bus error vocabulary in `map_wifi_error`.

**Exit criterion:** Fault injection with wrong PSK, unreachable AP, and handshake timeouts produces the documented retry behavior in §6.3 and §12.3.

### Phase 7 — Roaming — **Shipped**

`src/roam.rs`.

- Three modes: `off`, `supplicant`, `nexus` (§7.1). `WifiCommand::SetRoamingMode` routes property writes from `fi.nexus.Wifi.RoamingMode` into the backend.
- Signal polling with `WifiSignalPoll` emission (§7.2). Driven by a 1 s heartbeat tick pinned in the backend's `run()` loop plus a per-interface `last_signal_poll` map so polls dilate under `PowerState::Background` / `Sleep`.
- Nexus-driven roam: directed scan, hysteresis evaluation, `Roam()` invocation (§7.3).
- 802.11r (FT) passthrough when PHY capabilities advertise support (§7.4) — composed into `key_mgmt` (e.g. `"FT-PSK WPA-PSK"`) when the profile sets `fast_transition = true`.

**Exit criterion:** With two hostapd APs sharing an SSID on different channels, a moving station (simulated by adjusting hwsim signal) roams cleanly between them. *Residual — pending the hwsim harness (Phase 11).*

### Phase 8 — Supplicant Crash Recovery — **Shipped**

Handle `NameOwnerChanged` on `fi.w1.wpa_supplicant1` per §12.1.

- Transition all interfaces to `Disconnected { SupplicantUnavailable }` on disappearance.
- Re-attach and resume on reappearance.

**Exit criterion:** Killing wpa_supplicant mid-connection causes clean recovery once systemd restarts it.

### Phase 9 — Power Management — **Shipped**

`src/power.rs`. Per-power-state scan and poll scheduling (§13).

- D-Bus method on Nexus's manager for `SetPowerState`.
- Scheduler adjustments per state.
- `fi.nexus.Wifi.Powered`: `/dev/rfkill` reader + writer in `nexus-wifi::rfkill` (§13.5). Read path emits `NexusEvent::WifiRfkillChanged`; write path routes through `WifiCommand::SetPowered` into `RfkillWriter::set_blocked`.

**Exit criterion:** Transitioning to `background` doubles scan intervals; transitioning to `sleep` pauses scheduled scans. `Powered` read/write round-trips verified on real hardware: property writes flip the kernel rfkill state (validated via `/sys/class/rfkill/rfkillN/soft`); out-of-band sysfs writes propagate back through the property within the single-`read(2)` latency of the watcher task.

### Phase 10 — iwd Backend (Optional, Feature-Gated) — **Not started**

`src/supplicant/iwd.rs`. Behind `wifi-iwd` Cargo feature. Per §10.

- ObjectManager traversal to locate `Device`.
- Profile file writing under `/var/lib/iwd/`.
- State translation with documented gaps (`Authenticating`/`Handshaking` collapse to `Connecting`).

**Exit criterion:** With the feature enabled, core scenarios (Open, WPA2-PSK, WPA3-SAE) work. Enterprise support is best-effort.

### Phase 11 — Metrics & Integration Tests — **Shipped (harness in place; CI runner pending)**

- Full metric set per the conventions established in DD-001 §9.5.
- Integration test harness at `crates/nexus-wifi/tests/hwsim/` + `tests/hwsim_integration.rs`, gated behind the `integration-linux` Cargo feature. RAII helpers for `mac80211_hwsim` (`modprobe` load + `rmmod` unload), `hostapd` (config generation + child-process teardown), and `wpa_supplicant` (per-interface spawn) make new scenarios additive. First smoke test (`scan_finds_hostapd_ap`) is `#[ignore]`d so stock `cargo test` skips it; capable hosts run `sudo cargo test -p nexus-wifi --features integration-linux --test hwsim_integration -- --ignored`.
- Regulatory domain tests per §14.4.
- Hardware lab validation matrix per §14.3.

**Exit criterion:** CI green. Backend ready for production use with wpa_supplicant; iwd marked experimental. *Residual — a privileged CI runner that can load kernel modules (for automatic `--ignored` execution of the hwsim suite). On real hardware (BCM4345 via brcmfmac on a Pi 4), the full scan → WPA2-Personal connect → disconnect → rfkill block/unblock → reconnect flow is verified manually and documented in each landing commit.*

### Parallel work and dependencies

- **Blocked until DD-001 phase 5:** Phase 1 here (needs `InterfaceDiscovered` events).
- **Parallel with DD-002:** Phases 1–3 (shared types in `nexus-core` and `nexus-auth-eap`).
- **Sequential within DD-003:** Phases 4–9 build on each other.
- **Independent:** Phase 10 (iwd backend) can be done any time after phase 4.

---

## Related Documents

- [Nexus Architecture](./nexus-architecture.md) — Parent architecture document, [ADR-002](./nexus-architecture.md#43-key-architectural-decisions) for backend selection rationale
- [DD-001: Interface Discovery](./dd-001-interface-discovery.md) — How wireless interfaces and PHY capabilities are discovered
- [DD-002: Ethernet Backend](./dd-002-ethernet-backend.md) — Shares the `Dot1xEapConfig` for Enterprise authentication
- [DD-006: D-Bus API](./dd-006-dbus-api.md) — How scan results, connection state, and signal info are exposed externally
- [DD-007: Profile Store](./dd-007-profile-store.md) — Encryption of PSKs, passphrases, and enterprise credentials
