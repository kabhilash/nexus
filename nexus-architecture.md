# Nexus — Architecture Document

**Version:** 0.1.0-draft
**Status:** Draft
**Language:** Rust
**Target:** Embedded Linux (wall-powered and battery-powered devices)

---

## Table of Contents

1. [Overview](#1-overview)
2. [Design Goals](#2-design-goals)
3. [Scope](#3-scope)
4. [System Architecture](#4-system-architecture)
5. [Component Responsibilities](#5-component-responsibilities)
6. [Event Bus](#6-event-bus)
7. [Detailed Design Documents](#7-detailed-design-documents)
8. [Metrics Index](#8-metrics-index)
9. [Glossary](#9-glossary)

---

## 1. Overview

Nexus is a platform connectivity manager for embedded Linux devices. It provides unified discovery, configuration, lifecycle management, and monitoring of all platform communication interfaces — Ethernet, Wi-Fi, Bluetooth/BLE, and GNSS — through a single daemon with a coherent API surface.

Nexus is implemented in Rust and targets embedded Linux systems, both wall-powered (gateways, industrial devices) and battery-powered (portable devices, sensors). It is designed to be a drop-in replacement for ConnMan in embedded deployments, with added support for Bluetooth and GNSS that ConnMan lacks, and with a modern Rust implementation instead of C.

### 1.1 Comparison with Existing Solutions

| | ConnMan | NetworkManager | Nexus |
|---|---|---|---|
| Target | Embedded | Desktop | Embedded |
| Language | C | C | Rust |
| Wi-Fi supplicant | wpa_supplicant | wpa_supplicant | wpa_supplicant (default); iwd (alternative) |
| Wired 802.1X | wpa_supplicant | wpa_supplicant | wpa_supplicant or ead (pluggable) |
| Bluetooth | Limited (PAN/tethering) | None | Full BLE + Classic via BlueZ |
| GNSS | None | None | gpsd integration |
| IP management | Built-in DHCP client | Built-in DHCP client | Delegates to systemd-networkd |
| Memory safety | Manual | Manual | Compile-time (Rust) |

---

## 2. Design Goals

**Unified interface model.** Every communication technology — regardless of its kernel subsystem, protocol stack, or userspace daemon — is represented through a common `Interface` abstraction. Technology-specific capabilities are exposed as extensions of this model, not as separate subsystems.

**Pluggable backends with sensible defaults.** Nexus does not hardcode assumptions about which userspace daemons drive each technology, but does pick defaults that match the realities of embedded Wi-Fi deployment. Wi-Fi defaults to wpa_supplicant because that is the supplicant against which embedded Wi-Fi modules are certified and tested; iwd is supported as an alternative for deployments that can validate it against their chipset. Wired 802.1X defaults to wpa_supplicant, with ead (iwd's Ethernet Authentication Daemon) as an alternative. Bluetooth is managed via BlueZ. GNSS is consumed via gpsd. Each integration sits behind a trait boundary, swappable at build time via Cargo features or at runtime via configuration.

**Embedded-first.** Nexus targets resource-constrained devices. It avoids unnecessary allocations, minimizes runtime dependencies, supports deterministic startup ordering, and operates correctly under memory pressure. It is designed to start fast, recover from subsystem failures without restarting, and run indefinitely without leaking resources.

**Delegation of IP.** Nexus explicitly does not own layer 3 configuration. It manages link-layer connectivity — bringing interfaces up, authenticating, establishing carrier — and delegates IP address assignment, routing, and DNS to systemd-networkd. The synchronization boundary is the kernel's link carrier state, observed via rtnetlink.

**Observable.** Every state transition, discovery event, connection attempt, and failure is emitted as a structured event on Nexus's D-Bus API and internal event bus. External consumers (CLI tools, web UIs, fleet management agents) can subscribe to these events for monitoring, logging, and policy enforcement.

---

## 3. Scope

### 3.1 In Scope

- Discovery and classification of all platform interfaces (Ethernet, Wi-Fi, Bluetooth, GNSS).
- Lifecycle management: bring interfaces up, authenticate where required, handle carrier events.
- Wi-Fi: scanning, network selection, connection, roaming, profile storage.
- Ethernet: carrier tracking, optional 802.1X authentication.
- Bluetooth: adapter management, device discovery, pairing/bonding, connection management for BLE and Classic profiles.
- GNSS: fix tracking, satellite reporting, device management via gpsd.
- D-Bus API for external control and monitoring.
- Integration with systemd-networkd for IP layer.

### 3.2 Out of Scope

- DHCP, DHCPv6, RA handling (delegated to systemd-networkd).
- Wi-Fi supplicant implementation (delegated to wpa_supplicant or iwd).
- Bluetooth stack (delegated to BlueZ).
- GNSS protocol decoding (delegated to gpsd).
- Firewall, VPN, or tunnel configuration.
- Cellular/WWAN (may be added in a future revision via ModemManager).

---

## 4. System Architecture

### 4.1 High-Level Component Diagram

```
┌──────────────────────────────────────────────────────────────┐
│                      External Consumers                      │
│              (CLI, Web UI, Fleet Agent, etc.)                │
└──────────────────────┬───────────────────────────────────────┘
                       │ D-Bus API
┌──────────────────────┴───────────────────────────────────────┐
│                         NEXUS DAEMON                         │
│                                                              │
│  ┌─────────────────────────────────────────────────────────┐ │
│  │                    D-Bus Service Layer                  │ │
│  │           (fi.nexus.Manager / .Interface / ...)         │ │
│  └────────────────────────┬────────────────────────────────┘ │
│                           │                                  │
│  ┌────────────────────────┴────────────────────────────────┐ │
│  │                  Core State Machine                     │ │
│  │        (Interface Registry, Event Bus, Profiles)        │ │
│  └──┬──────────┬──────────┬──────────┬─────────────────────┘ │
│     │          │          │          │                       │
│  ┌──┴───┐ ┌───┴───┐ ┌───┴───┐ ┌───┴─────┐                    │
│  │ Eth  │ │  WiFi │ │  BT   │ │  GNSS   │  Technology        │
│  │ Back │ │ Back  │ │ Back  │ │  Back   │  Backends          │
│  │ end  │ │ end   │ │ end   │ │  end    │                    │
│  └──┬───┘ └──┬────┘ └──┬────┘ └──┬─────┘                     │
│     │        │         │         │                           │
│  ┌──┴────────┴─────────┴─────────┴─────────────────────────┐ │
│  │              Interface Monitor (Unified)               │ │
│  │          rtnetlink + nl80211 + udev/sysfs              │ │
│  └──┬──────────┬───────────────────────────────────────────┘ │
└─────┼──────────┼─────────────────────────────────────────────┘
      │          │            D-Bus            D-Bus / socket
      │          │              │                    │
  ┌───┴───┐  ┌──┴───┐   ┌─────┴──────┐   ┌────────┴──┐
  │kernel │  │kernel │   │wpa_suppl / │   │   BlueZ   │
  │rtnetlk│  │nl80211│   │  iwd / ead │   │  / gpsd   │
  └───────┘  └───────┘   └────────────┘   └───────────┘
                                                │
                              systemd-networkd handles IP
                              (watches carrier via rtnetlink)
```

### 4.2 Layered View

Nexus is structured in three functional layers:

**Discovery layer (unified).** The Interface Monitor is the single source of truth for what interfaces exist and their current link state. All netlink and udev traffic flows through here. See [DD-001: Interface Discovery](./dd-001-interface-discovery.md) for the detailed design.

**Protocol layer (per-technology).** Each technology backend owns the interaction with its respective external daemon or kernel subsystem. Backends translate technology-specific state machines into a unified lifecycle model that the core can reason about. See detailed designs linked in [Section 7](#7-detailed-design-documents).

**Orchestration layer (unified).** The Core State Machine, Event Bus, D-Bus Service Layer, and Profile Store tie everything together. Independent of technology, they handle policy (which network to connect to, priority ordering), persistence (what profiles exist), and external API (how the rest of the system talks to Nexus).

### 4.3 Key Architectural Decisions

**ADR-001: No built-in IP management.** Nexus delegates IP to systemd-networkd. The sync boundary is the kernel's carrier/operstate, observed via rtnetlink. Rationale: avoids reimplementing DHCP, reuses a well-tested and maintained component, aligns with modern systemd-based deployments. Trade-off: Nexus cannot run on systems without systemd-networkd (or equivalent). This is acceptable given the target audience.

**ADR-002: wpa_supplicant as default Wi-Fi supplicant; iwd as supported alternative.** Nexus uses wpa_supplicant by default for all Wi-Fi operations. iwd is supported behind the same trait abstraction but is not the default, and integrators should validate iwd against their specific SOM before using it in production.

Rationale:

- **Wi-Fi certification is tied to wpa_supplicant in practice.** Embedded SOMs that ship with Wi-Fi certification (FCC modular grant, CE/RED, WFA Certified) are certified against a reference software stack supplied by the module vendor. That stack universally includes wpa_supplicant. The Wi-Fi Alliance's certification test suite and most certification labs' tooling are also built around wpa_supplicant. Substituting iwd results in an uncertified configuration from the regulator's perspective, even though the hardware and driver are unchanged — the supplicant drives the 4-way handshake, PMF, SAE, power save, and roaming behaviors that certification validates. For many deployments this means re-testing or re-certification would be required to ship with iwd.
- **Driver coverage favors wpa_supplicant.** Common embedded chipsets — Broadcom fullmac (`brcmfmac`), TI wilink, vendor-specific Qualcomm drivers — are primarily tested and supported with wpa_supplicant. iwd works best on mac80211 drivers (iwlwifi, ath10k/11k, mt76) and has historically had rougher edges on fullmac chipsets common in embedded products.
- **Ecosystem support.** Module vendor patches, quirk handling, and bug fixes land in wpa_supplicant first and may never make it to iwd. Deploying iwd means owning debugging for the integrator.

Why keep the pluggable abstraction nonetheless:

- The same trait is used for pluggable wired 802.1X (see ADR-003), so the abstraction pays for itself there.
- Mocking the supplicant at the trait boundary makes testing significantly easier than mocking D-Bus itself.
- iwd remains a reasonable choice for specific deployments (Intel/Qualcomm chipsets with aggressive power management, or builds where the OpenSSL dependency is unacceptable). Supporting it costs little once the abstraction exists.

Default configuration: `wifi.backend = "wpa_supplicant"`. Default Cargo features include `wifi-wpa_supplicant` and exclude `wifi-iwd`. Documentation explicitly marks iwd as an alternative requiring integrator validation.

**ADR-003: Pluggable wired 802.1X backend.** Same pattern as Wi-Fi. wpa_supplicant and ead are both supported. Default is wpa_supplicant due to maturity; ead is available for builds where the OpenSSL dependency is undesirable.

**ADR-004: BlueZ over D-Bus only.** Nexus does not talk directly to HCI. Rationale: BlueZ is the de-facto Linux Bluetooth stack, its D-Bus API is stable and well-documented, and reimplementing HCI would be a massive and pointless effort.

**ADR-005: gpsd for GNSS.** Nexus does not parse NMEA or binary GNSS protocols directly. Rationale: gpsd already handles device quirks, multi-constellation output, and client multiplexing. Nexus consumes structured fix data via gpsd's JSON protocol.

**ADR-006: Rust with async (tokio).** Rationale: memory safety at compile time, zero-cost abstractions, strong ecosystem for async I/O and D-Bus (via `zbus`), good embedded story (no_std support in libraries, cross-compilation).

**ADR-007: All internal communication via typed event bus.** Rationale: loose coupling between components, enables the D-Bus service layer to observe all state changes uniformly, simplifies testing (events can be replayed or mocked).

---

## 5. Component Responsibilities

**Interface Monitor.** Owns all netlink sockets (`NETLINK_ROUTE`, nl80211 generic netlink) and the udev monitor socket. Performs initial enumeration and ongoing hotplug detection. Classifies each interface by technology type. Emits `InterfaceDiscovered`, `InterfaceRemoved`, `CarrierChanged`, `OperstateChanged` events. This is the only component that directly parses netlink messages.

**Core State Machine.** Maintains the canonical registry of all known interfaces and their current state. Routes discovery events to the appropriate technology backend. Manages the profile database. Enforces ordering and dependency constraints (e.g., don't start Wi-Fi scan until wpa_supplicant interface is registered). Publishes all state transitions to the event bus.

**Technology Backends.** One per technology. Each implements a common `TechnologyBackend` trait and owns the interaction with its respective external daemon:

- **EthernetBackend** — Carrier tracking and optional 802.1X. Detailed design: [DD-002: Ethernet Backend](./dd-002-ethernet-backend.md)
- **WifiBackend** — Scanning, connection, roaming via wpa_supplicant or iwd. Detailed design: [DD-003: Wi-Fi Backend](./dd-003-wifi-backend.md)
- **BluetoothBackend** — Adapter and device management via BlueZ. Detailed design: [DD-004: Bluetooth Backend](./dd-004-bluetooth-backend.md)
- **GnssBackend** — Fix tracking via gpsd. Detailed design: [DD-005: GNSS Backend](./dd-005-gnss-backend.md)

**D-Bus Service Layer.** Exposes Nexus's capabilities to external consumers. Maps internal events to D-Bus signals. Validates and dispatches incoming method calls to the core state machine. Detailed design: [DD-006: D-Bus API](./dd-006-dbus-api.md)

**Profile Store.** Persists per-technology configuration to disk under `/var/lib/nexus/`. Credentials are encrypted at rest. Detailed design: [DD-007: Profile Store](./dd-007-profile-store.md)

**Cross-cutting subsystems.** A small number of features sit alongside the per-technology backends rather than inside any single one:

- **Rfkill watcher / writer.** Owns `/dev/rfkill` end-to-end. The reader (a non-blocking fd wrapped in `tokio::io::unix::AsyncFd`) republishes every kernel rfkill edge — hardware switches, `rfkill` userspace, sysfs writes, the kernel's synthetic `RFKILL_OP_ADD` events at open time — onto the event bus as `WifiRfkillChanged`. The writer (a blocking fd guarded by `spawn_blocking`) accepts `WifiCommand::SetPowered` from the operator-facing `fi.nexus.Wifi.Powered` property. Rfkill is a first-class state-machine input for the Wi-Fi backend, not just a flag (DD-003 §13.5). Lives in `nexus-wifi::rfkill`; the Bluetooth backend may grow a parallel arm if Bluetooth-specific rfkill ever needs the same first-class treatment.
- **Connectivity probe.** A small daemon-scope task runs an HTTP/1.1 GET against a generate-204 endpoint (default `http://connectivity-check.ubuntu.com/`) on every `WifiLinkReady` / `EthLinkReady`, plus once at startup, and forces `internetOffline` immediately on the last `LinkLost`. Surfaces as the `fi.nexus.Manager.InternetConnectivity` property and `InternetConnectivityChanged` signal (DD-006 §5.4). Event-driven only — no periodic backstop. Lives in `nexus-daemon::connectivity` and produces only `NexusEvent::InternetConnectivityChanged`; it intentionally has no per-interface state of its own.

---

## 6. Event Bus

All internal communication flows through a typed async channel. Events are enums scoped by origin:

```rust
enum NexusEvent {
    // From Interface Monitor
    InterfaceDiscovered(InterfaceInfo),
    InterfaceRemoved { ifindex: u32 },
    CarrierChanged { ifindex: u32, up: bool },
    OperstateChanged { ifindex: u32, state: OperState },
    // Same identity, one field corrected in place — currently only
    // fired for a Bluetooth adapter whose BD_ADDR was still zeroed
    // at discovery time, either because firmware set the real
    // address after udev's initial `Add` event, or because the
    // transport (UART/serdev) has no kernel sysfs address at all and
    // BlueZ's own Adapter1.Address is the only authoritative source.
    MacChanged { ifindex: u32, mac: MacAddr },

    // From Ethernet Backend
    EthAuthStateChanged { ifindex: u32, state: AuthState },
    EthLinkReady { ifindex: u32 },     // carrier up AND authenticated (if required)
    EthLinkLost { ifindex: u32 },      // carrier dropped or authentication ended

    // From Wi-Fi Backend
    WifiScanComplete { ifindex: u32, results: Vec<BssInfo> },
    WifiStateChanged { ifindex: u32, state: WifiState },
    WifiSignalPoll { ifindex: u32, rssi: i32, frequency: u32 },
    WifiLinkReady { ifindex: u32 },    // associated, authenticated, keyed
    WifiLinkLost { ifindex: u32 },     // disconnected or key rotation failed

    // From Bluetooth Backend
    /// Fires in two cases: (1) an adapter first becomes visible via
    /// BlueZ's ObjectManager InterfacesAdded (including the republish
    /// after a BlueZ reconnect), and (2) an existing adapter's Powered
    /// or Discovering property changed. The backend handler treats the
    /// event the same way in both cases — recompute the adapter's state
    /// machine from the (powered, discovering) tuple. Name "Changed"
    /// is a slight misnomer for case 1 (there was no prior state to
    /// change from), but kept for consistency with the existing
    /// variant and to avoid churning every downstream reference.
    BtAdapterChanged {
        adapter: String,            // "/org/bluez/hciN"
        powered: bool,
        discovering: bool,
    },
    BtDeviceDiscovered(BtDeviceInfo),
    BtDeviceConnected { adapter: String, address: MacAddr },
    BtDeviceDisconnected { adapter: String, address: MacAddr },
    // Dropped from the backend's registry (discovery-TTL GC only;
    // paired devices are kept indefinitely and removed via Forget
    // instead, which doesn't fire this).
    BtDeviceRemoved { adapter: String, address: MacAddr },

    /// A pairing operation has started. Emitted by the Bluetooth
    /// Backend when Pair() is called. The PairingJobId correlates
    /// subsequent BtPairingPrompt and BtPairingComplete events.
    BtPairingStarted { job_id: PairingJobId, device: String },

    /// Emitted by the Bluetooth Backend when BlueZ's registered Agent
    /// receives a callback that needs a human response (PIN, passkey,
    /// confirmation, incoming-connection authorization, etc.). The
    /// Agent task itself runs separately from the backend and deposits
    /// a oneshot sender via BtCommand::RegisterPromptOneshot; the
    /// backend then emits this event so the D-Bus layer can surface the
    /// prompt to operator UI. D-Bus clients respond via
    /// fi.nexus.Bluetooth.AnswerPairingPrompt, which routes back through
    /// the command channel to resolve the Agent's oneshot.
    BtPairingPrompt {
        job_id: PairingJobId,
        kind: PairingPromptKind,
        data: PairingPromptData,
    },

    /// Pairing finished. On success, the device's state is Paired.
    /// On failure, the reason is populated and the device is in Failed.
    BtPairingComplete {
        job_id: PairingJobId,
        success: bool,
        reason: Option<BtFailureReason>,
    },

    /// BlueZ D-Bus connection established (or re-established). Backend
    /// uses this to republish adapter/device state from ObjectManager.
    /// Fires both on first connection and on reconnection after a drop.
    BluezConnected,

    /// BlueZ D-Bus connection lost. Backend marks all adapters
    /// Unavailable and retries connect with backoff.
    BluezDisconnected,

    // From GNSS Backend
    /// Raw TPV message from the gpsd client, before quality filtering
    /// or rate-capping. Consumers should prefer GnssFixChanged unless
    /// they specifically want unfiltered data (e.g., a diagnostic
    /// recorder).
    GnssTpvReceived { device: String, fix: GnssFix },

    /// Filtered, rate-capped fix from the GNSS Backend. This is what
    /// the D-Bus layer translates into fi.nexus.Gnss.FixChanged.
    GnssFixChanged { device: String, fix: GnssFix },

    GnssSatellites { device: String, satellites: Vec<SatInfo> },

    /// The gpsd client has lost its connection to gpsd. The backend
    /// supervisor reconnects with backoff (see DD-005 §6.4).
    GnssGpsdDisconnected,

    /// The gpsd client has (re)connected to gpsd. Fires both on first
    /// connection and on reconnection after a drop; the backend uses
    /// this to (re)register all known devices with gpsd.
    GnssGpsdConnected,

    // From Profile Store
    ProfileChanged { kind: ProfileKind, key: String },      // put or remove completed
    ProfileCorrupt { kind: ProfileKind, key: String, reason: String },  // quarantined on load

    // From any backend, to the D-Bus layer
    /// Operator-facing notification. The D-Bus layer translates this
    /// into fi.nexus.Manager.NotificationEvent (DD-006 §5.3) for
    /// consumption by operator UI. Backends use this for "out-of-band"
    /// notifications that don't correspond to a state change on a
    /// specific interface or profile — subsystem outages, pairing
    /// prompts, master-key degradation, etc.
    OperatorNotification {
        kind: String,
        data: NotificationData,
    },
}

/// Key-value dict for OperatorNotification payloads. The D-Bus layer
/// marshals this to an `a{sv}` variant dict when emitting
/// fi.nexus.Manager.NotificationEvent. Keys are human-readable strings
/// chosen by the emitting backend per the kind-specific schema in
/// DD-006 §5.3; values are one of a few D-Bus-mappable types.
pub struct NotificationData(pub BTreeMap<String, NotificationValue>);

#[derive(Debug, Clone)]
pub enum NotificationValue {
    String(String),
    U32(u32),
    U64(u64),
    Bool(bool),
    /// Serialized as D-Bus object path ("o" signature).
    ObjectPath(String),
}

impl NotificationData {
    pub fn new() -> Self { Self(BTreeMap::new()) }
    pub fn insert(
        &mut self,
        k: impl Into<String>,
        v: impl Into<NotificationValue>,
    ) {
        self.0.insert(k.into(), v.into());
    }
}
```

The `*LinkReady` / `*LinkLost` events are the signals to downstream consumers (notably systemd-networkd through its netlink carrier watch, and the D-Bus service layer) that layer-3 configuration can begin or should tear down. They are emitted by technology backends, not by the Interface Monitor — the monitor emits raw kernel events (`CarrierChanged`, `OperstateChanged`), and each backend decides when its interface is truly "ready" per its own semantics.

The core state machine consumes all events. The D-Bus service layer subscribes to events it needs to forward as D-Bus signals. Backends only produce events for their technology and consume events routed to them by the core.

Bus implementation is a broadcast channel (`tokio::sync::broadcast` or equivalent). Slow consumers are detected and dropped rather than allowed to back-pressure producers — the Interface Monitor must never block on netlink parsing because a downstream consumer is slow.

---

## 7. Detailed Design Documents

The following detailed design documents specify the internal design of individual Nexus components. They assume the context established in this architecture document and go deep on single concerns.

| ID | Document | Status | Scope |
|---|---|---|---|
| DD-001 | [Interface Discovery](./dd-001-interface-discovery.md) | Draft | Netlink and udev-based discovery, classification, hotplug handling |
| DD-002 | [Ethernet Backend](./dd-002-ethernet-backend.md) | Draft | Carrier detection, 802.1X authentication, pluggable auth backends |
| DD-003 | [Wi-Fi Backend](./dd-003-wifi-backend.md) | Draft | Supplicant abstraction (wpa_supplicant / iwd), scan/connect/roam state machine |
| DD-004 | [Bluetooth Backend](./dd-004-bluetooth-backend.md) | Draft | BlueZ D-Bus integration, adapter and device lifecycle, BLE vs. Classic, pairing agent |
| DD-005 | [GNSS Backend](./dd-005-gnss-backend.md) | Draft | gpsd integration, fix reporting, device management |
| DD-006 | [D-Bus API](./dd-006-dbus-api.md) | Draft | Object hierarchy, methods, signals, properties |
| DD-007 | [Profile Store](./dd-007-profile-store.md) | Draft | On-disk format, encryption, atomic updates |
| DD-008 | [nexusctl Client](./dd-008-nexusctl-client.md) | Draft | Command-line client wrapping the D-Bus API; command tree, output formats, interactive pairing and Wi-Fi auth flows |

Documents will be added incrementally. When reading a detailed design document, start from this architecture document for context.

---

## 8. Metrics Index

Nexus emits Prometheus-style metrics across its subsystems. Each DD defines its own metrics table; this index cross-references them for a one-stop view.

**Naming convention.** All metric names use the `nexus_<component>_<thing>_<unit>` pattern. Counters end in `_total`, histograms end in the unit (`_seconds`, `_bytes`), gauges have no suffix.

| Component | Defined in | Example metrics |
|---|---|---|
| Interface Monitor | [DD-001 §9.5](./dd-001-interface-discovery.md#95-observability) | `nexus_interface_events_total`, `nexus_interface_discovery_duration_seconds`, `nexus_interface_count` |
| Ethernet Backend | [DD-002 §9.5](./dd-002-ethernet-backend.md) | `nexus_ethernet_auth_attempts_total`, `nexus_ethernet_auth_duration_seconds`, `nexus_ethernet_link_up` |
| Wi-Fi Backend | [DD-003 §12.5](./dd-003-wifi-backend.md) | `nexus_wifi_scans_total`, `nexus_wifi_connect_attempts_total`, `nexus_wifi_roam_events_total`, `nexus_wifi_signal_dbm` |
| Bluetooth Backend | [DD-004 §13.2](./dd-004-bluetooth-backend.md#132-observability) | `nexus_bluetooth_pairings_total`, `nexus_bluetooth_connections_total`, `nexus_bluetooth_agent_callbacks_total`, `nexus_bluetooth_bluez_connected` |
| D-Bus API | DD-006 (forthcoming in phase 10) | `nexus_dbus_calls_total`, `nexus_dbus_rate_limited_total`, `nexus_dbus_policykit_check_duration_seconds` |
| GNSS Backend | [DD-005 §11.2](./dd-005-gnss-backend.md#112-observability) | `nexus_gnss_tpv_total`, `nexus_gnss_satellites_used`, `nexus_gnss_horizontal_error_meters`, `nexus_gnss_gpsd_reconnects_total` |
| Profile Store | [DD-007 §10.4](./dd-007-profile-store.md#104-observability) | `nexus_profiles_loaded_total`, `nexus_profiles_writes_total`, `nexus_profiles_rotation_duration_seconds` |

Metrics are scraped via a `/metrics` HTTP endpoint on `127.0.0.1:9402` by default (configurable; disabled entirely for deployments that don't want it). The endpoint serves no other paths and requires no auth — binding to loopback is the security boundary.

---

## 9. Glossary

- **BSS** — Basic Service Set. A single Wi-Fi access point with a unique BSSID.
- **cfg80211** — Kernel Wi-Fi configuration layer, sits above mac80211 and fullmac drivers.
- **ead** — Ethernet Authentication Daemon. Part of the iwd project. Handles wired 802.1X.
- **EAPOL** — EAP Over LAN. The layer-2 protocol used by 802.1X to carry EAP frames.
- **GNSS** — Global Navigation Satellite System. Umbrella term for GPS, GLONASS, Galileo, BeiDou, etc.
- **HCI** — Host Controller Interface. The interface between the Bluetooth host stack and the controller hardware.
- **iwd** — iNet Wireless Daemon. Intel's Wi-Fi supplicant, alternative to wpa_supplicant.
- **mac80211** — Kernel software MAC layer for Wi-Fi. Used by most Wi-Fi drivers (the exception being fullmac devices like Broadcom).
- **nl80211** — Netlink-based protocol for cfg80211 configuration. The primary kernel/userspace boundary for Wi-Fi.
- **rtnetlink** — `NETLINK_ROUTE` protocol. Kernel interface for network device, address, and route management.
- **Supplicant** — In 802.1X terminology, the entity seeking network access (the client). Also used as the general term for wpa_supplicant and iwd.
- **wiphy** — Wireless PHY. Represents a physical Wi-Fi radio in the kernel. One wiphy can host multiple virtual interfaces.
- **wpa_supplicant** — The de-facto Wi-Fi supplicant on Linux. Also handles wired 802.1X.
