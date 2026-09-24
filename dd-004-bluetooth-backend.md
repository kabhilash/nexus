# DD-004: Bluetooth Backend — Detailed Design

**Parent:** [Nexus Architecture](./nexus-architecture.md)
**Depends on:** [DD-001: Interface Discovery](./dd-001-interface-discovery.md), [DD-007: Profile Store](./dd-007-profile-store.md)
**Referenced by:** [DD-006: D-Bus API](./dd-006-dbus-api.md)
**Status:** Draft
**Scope:** Design of the Bluetooth Backend — how Nexus manages Bluetooth adapters (HCI controllers), discovers and tracks devices (both BLE and Classic), handles pairing and bonding, and surfaces connection state. Integration is via BlueZ's D-Bus API ([ADR-004](./nexus-architecture.md#43-key-architectural-decisions)); Nexus does not speak HCI directly.

---

## Table of Contents

1. [Context](#1-context)
   - 1.1 [Repo Layout](#11-repo-layout)
2. [Responsibilities](#2-responsibilities)
3. [Failure Modes](#3-failure-modes)
4. [Adapter Lifecycle](#4-adapter-lifecycle)
   - 4.1 [Adapter States](#41-adapter-states)
   - 4.2 [Adapter Transitions](#42-adapter-transitions)
5. [Device Lifecycle](#5-device-lifecycle)
   - 5.1 [Device States](#51-device-states)
   - 5.2 [Device Transitions](#52-device-transitions)
   - 5.3 [BLE vs Classic](#53-ble-vs-classic)
6. [BlueZ Abstraction](#6-bluez-abstraction)
   - 6.1 [Trait Definition](#61-trait-definition)
   - 6.2 [Shared Types](#62-shared-types)
7. [Core Backend Logic](#7-core-backend-logic)
   - 7.1 [Event Flow Overview](#71-event-flow-overview)
   - 7.2 [Lifecycle Handler](#72-lifecycle-handler)
   - 7.3 [Supervisor and Supporting Helpers](#73-supervisor-and-supporting-helpers)
8. [Pairing and Bonding](#8-pairing-and-bonding)
   - 8.1 [The Agent](#81-the-agent)
   - 8.2 [Pairing Flow](#82-pairing-flow)
   - 8.3 [Bond Storage](#83-bond-storage)
9. [Discovery](#9-discovery)
   - 9.1 [Discovery Sessions](#91-discovery-sessions)
   - 9.2 [Discovery Filters](#92-discovery-filters)
10. [Configuration](#10-configuration)
11. [Device Profile](#11-device-profile)
12. [Power Management](#12-power-management)
13. [Error Handling and Observability](#13-error-handling-and-observability)
    - 13.1 [Fault Classes](#131-fault-classes)
    - 13.2 [Observability](#132-observability)
14. [Testing Strategy](#14-testing-strategy)
15. [Implementation Phases](#15-implementation-phases)

---

## 1. Context

Bluetooth is the most stateful and most interactive of Nexus's managed technologies. Ethernet just needs a carrier. Wi-Fi needs a credential and an AP. GNSS needs the sky. Bluetooth needs: an adapter that's powered, a discovery session that produced a device, a pairing exchange (possibly with user confirmation of a 6-digit passkey), a bond stored on both sides, a profile connection, and an ongoing state that survives disconnection-for-range-and-reconnection-when-back. And it has two mostly-disjoint stacks (Classic BR/EDR and BLE) with different pairing models, different discovery patterns, and different connection semantics.

Nexus does not reimplement any of this. [ADR-004](./nexus-architecture.md#43-key-architectural-decisions) delegates the entire Bluetooth stack to BlueZ, which exposes a mature D-Bus API (`org.bluez`). Nexus becomes a BlueZ client: it watches `ObjectManager` for adapter and device objects, drives discovery and connections via `org.bluez.Adapter1` / `Device1` methods, and registers an Agent for pairing interaction. Everything HCI-specific — L2CAP, SDP, SM, ATT — is BlueZ's job.

What Nexus adds on top of BlueZ is:

- A uniform lifecycle model that fits the Nexus pattern (adapter-as-interface, device-as-subordinate-state).
- Per-device profiles in the Profile Store, so a paired device's preferences (auto-connect, trusted flag, friendly name) persist across BlueZ restarts and Nexus upgrades.
- An operator-facing D-Bus surface that's simpler than BlueZ's (no need for clients to understand ObjectManager introspection quirks).
- An event model that integrates with `NexusEvent`, so other Nexus subsystems can react to Bluetooth state changes without subscribing to BlueZ directly.

### 1.1 Repo Layout

The code for this component lives at:

```
crates/
  nexus-bluetooth/              <- Bluetooth Backend
    Cargo.toml
    src/
      lib.rs                    <- entry point (spawn_bluetooth_backend)
      backend.rs                <- BluetoothBackend top-level orchestrator
      adapter.rs                <- per-adapter state machine (§4)
      device.rs                 <- per-device state machine (§5)
      bluez/                    <- BlueZ D-Bus client abstraction
        mod.rs                  <- BluezClient trait
        zbus_client.rs          <- default impl (zbus proxies)
        proxies.rs              <- zbus #[proxy] types for Adapter1/Device1/...
        object_manager.rs       <- ObjectManager subscription + reconciliation
      agent.rs                  <- fi.nexus Agent for pairing callbacks (§8.1)
      pairing.rs                <- pairing helpers (PairingJobId mint,
                                   classify_pair_error, prompt-notification
                                   builders). The main pairing state
                                   machine lives on BluetoothBackend in
                                   backend.rs (see §7.2).
      discovery.rs              <- discovery session management (§9)
      profile.rs                <- BluetoothProfile (§11)
      errors.rs
    tests/
      adapter_lifecycle.rs
      device_lifecycle.rs
      pairing.rs
      discovery.rs
```

**Key dependencies:**

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime |
| `zbus` | D-Bus client to `org.bluez` and Agent registration |
| `serde` | Profile serialization |
| `thiserror` | Error types |
| `async-trait` | Trait method async |
| `tracing` | Logging |

No dependency on `bluer` or similar — zbus with hand-written proxy types is less overhead than pulling in a full BlueZ client crate, especially since the subset of BlueZ Nexus uses is narrow and stable.

---

## 2. Responsibilities

The Bluetooth Backend is responsible for:

1. **Receiving adapter-discovered events** from the Interface Monitor (`NexusEvent::InterfaceDiscovered` with `InterfaceKind::Bluetooth`) and initializing a per-adapter state machine for each.
2. **Maintaining a connection to BlueZ** via D-Bus and reconciling state when BlueZ restarts.
3. **Subscribing to BlueZ's `ObjectManager`** to learn about device arrivals, departures, and property changes.
4. **Driving adapter operations:** power on/off, start/stop discovery, set discoverable and pairable flags.
5. **Managing device lifecycle:** pairing, bonding, connection, disconnection.
6. **Serving as the pairing Agent** for BlueZ, translating BlueZ's pairing callbacks (PIN request, passkey display, passkey confirmation) into Nexus events that D-Bus clients can respond to.
7. **Persisting device profiles** in the Profile Store, keyed by Bluetooth address.
8. **Emitting `NexusEvent` variants** for adapter and device state changes, for consumption by the D-Bus layer (DD-006) and other subsystems.

The Bluetooth Backend is explicitly **not** responsible for:

- Any HCI-level operation — this is BlueZ's job.
- Profile-specific logic for A2DP, HFP, HID, PBAP, etc. — BlueZ handles these; Nexus only manages connection state at the device level.
- GATT client or server logic — applications that need GATT talk to BlueZ's `GattManager1` / `GattCharacteristic1` interfaces directly. Nexus exposes that a device has GATT services but does not itself read characteristics.
- Bluetooth-over-IP (PAN, NAP) — handled by BlueZ's `network1` interface; Nexus does not mediate.
- BLE advertising as a peripheral — Nexus treats the local adapter as a central; peripheral-role use cases require application-level BlueZ clients.
- Audio routing (PulseAudio, PipeWire integration) — out of scope; these subsystems register with BlueZ directly.

---

## 3. Failure Modes

Bluetooth is error-prone by nature (RF, user interaction, device variation). Enumerating the failure modes up front shapes the rest of the design.

**BlueZ unavailable.** BlueZ isn't running, or it's restarting. The backend cannot query adapters, cannot drive discovery, cannot pair. Devices that were connected stay connected from the kernel's perspective until the connection times out, but Nexus's view of them becomes stale.

**Adapter disappears mid-operation.** USB Bluetooth dongle unplugged during a discovery session. BlueZ emits InterfacesRemoved for `/org/bluez/hci0`; the backend must tear down in-flight operations and move the adapter state to `Unavailable`.

**Pairing failures — many flavors.**
- User rejects the pairing request (either side).
- PIN/passkey mismatch.
- Timeout (30 s default; BlueZ configurable).
- Peer times out or disconnects mid-pairing.
- Legacy PIN-based pairing on an SSP-capable adapter (unusual but happens).

**Bonded device won't reconnect.** Peer cleared its bond (factory-reset), peer changed its address (random BLE addresses without IRK), or out of range. From BlueZ's perspective, the device just doesn't respond.

**Multiple adapters.** `hci0` and `hci1` coexisting. Operator actions need to specify *which* adapter — no silent defaulting.

**Agent conflict.** Another process (e.g., `bluetoothctl`, a desktop environment, `blueman`) has already registered as the pairing agent. BlueZ returns `org.bluez.Error.AlreadyExists` on our `RegisterAgent` call.

**Classic vs BLE conflation.** A dual-mode device (phone, laptop) shows up with both Classic and BLE addresses. BlueZ usually presents them as a single device object with `AddressType = "public"` and appropriate UUID lists, but edge cases exist (separate objects, address-type mismatches).

**Discovery churn.** Advertising BLE devices may appear and disappear within a single discovery window. The backend must not thrash the event bus with arrival-and-departure spam.

**Random-address unbonded BLE peripherals.** BLE privacy-enabled peripherals that aren't bonded rotate their address on every advertising interval (typically every 15 minutes, sometimes more frequently). BlueZ emits `InterfacesAdded` for each new address — the peripheral appears to Nexus as a *new device* each rotation. Without bonding, there's no IRK to resolve rotations back to an identity, so there's no mechanical way to recognize "same physical device." Two consequences:
- The device registry accumulates entries proportional to rotation frequency × discovery runtime. The `discovery_device_ttl_s` cleanup (§13.2) bounds this.
- Any operator workflow that involves "select a device, then pair with it" has a race with the next rotation. Nexus surfaces the address in `BtDeviceInfo`; whether the operator UI refreshes fast enough is a UI problem, not a backend one.

Paired (bonded) random-address devices are unaffected — BlueZ handles IRK-based resolution transparently and device paths stay stable.

**BlueZ version skew.** Target baseline is **BlueZ 5.50** (shipping on Debian 10 / Ubuntu 20.04 / Yocto Dunfell) up through **5.72** (current as of this writing). The API surface Nexus uses — `Adapter1`, `Device1`, `AgentManager1`, `ObjectManager`, `SetDiscoveryFilter` — has been stable across this range. Features explicitly *not* used (and therefore not a compatibility concern):
- `LEAdvertisingManager1` (peripheral-role advertising) — Nexus is central-only.
- `GattManager1` / GATT client APIs — applications that need GATT talk to BlueZ directly.
- Experimental features requiring BlueZ's `--experimental` flag (battery reporting, ISO channels, etc.).

The backend does not branch on BlueZ version. If a future BlueZ release renames or removes a method Nexus relies on, integration tests against that version are expected to catch it; the fix is to adapt the `bluez/proxies.rs` zbus proxy definitions rather than a runtime version check.

**Powered-off adapter.** An adapter whose kernel interface is present but `Powered = false` on BlueZ. Operations like discovery should return a clear "adapter not powered" error, not silently hang.

**Trust vs pairing.** BlueZ's `Trusted` property is orthogonal to bonding — a device can be paired but not trusted (prompts on each connection) or trusted-not-paired (unusual). Nexus has a `trusted` bit on its profile that maps to BlueZ's `Trusted`.

**Reverse connection.** The peer initiates the connection (incoming phone call, HID keyboard waking up). The backend must accept this without operator action, but only if the device is in the store with `auto_accept_incoming = true`.

Each of these shapes at least one concrete decision downstream: the state machines have explicit "Failed" branches; adapter ops are always per-adapter-path (§6.1 trait); agent registration is tolerant of conflicts (§8.1); pairing state tracks the in-flight operation so Agent callbacks can be correlated.

---

## 4. Adapter Lifecycle

### 4.1 Adapter States

```rust
enum BtAdapterState {
    /// Kernel interface exists (InterfaceMonitor saw it) but BlueZ
    /// either isn't running or hasn't created an adapter object for it.
    /// Most often seen at startup before BlueZ is up, or briefly during
    /// BlueZ restart.
    Unavailable,

    /// BlueZ knows about the adapter. `Powered = false` — device is
    /// visible but not usable. Commands to power up the adapter
    /// transition it to Powered.
    Present,

    /// BlueZ's `Powered = true` for this adapter. Idle; not actively
    /// scanning.
    Powered,

    /// Discovery session active. BlueZ's `Discovering = true`. The
    /// backend owns one outstanding `StartDiscovery` call.
    Discovering { since: Instant },

    /// Kernel interface removed (udev remove event from DD-001),
    /// or BlueZ's ObjectManager removed the object. Adapter is about
    /// to be dropped from the registry.
    Gone,
}
```

### 4.2 Adapter Transitions

```
                   InterfaceDiscovered(kind=Bluetooth)
  [initial] ───────────────────────────────────────────►  Unavailable
                                                              │
                                                              │ ObjectManager
                                                              │ InterfacesAdded(/org/bluez/hciN)
                                                              ▼
                                                            Present
                                                              │
                                                              │ Powered property
                                                              │ becomes true
                                                              ▼
                                                            Powered ◄────┐
                                                              │           │
                                                              │           │ StopDiscovery
                                                              │ Start-    │
                                                              │ Discovery │
                                                              ▼           │
                                                          Discovering ────┘
                                                              │
                                                              │ Powered
                                    ┌─────────────────────────┤ becomes
                                    │                         │ false,
                                    │                         │ or
                                    │ InterfacesRemoved       │ InterfacesRemoved
                                    ▼                         ▼
                                 Gone ◄─── (from any state) ─┘
```

The transitions are driven by:

- **`InterfaceDiscovered(kind=Bluetooth)`** from the Interface Monitor. Creates the adapter entry in `Unavailable` state.
- **`ObjectManager.InterfacesAdded`** for `/org/bluez/hciN` with `org.bluez.Adapter1`. Transitions `Unavailable → Present`.
- **`ObjectManager.InterfacesRemoved`** for the same path. Transitions anything → `Gone`.
- **`PropertiesChanged` on `org.bluez.Adapter1.Powered`.** `Present → Powered` or `Powered → Present`.
- **`PropertiesChanged` on `org.bluez.Adapter1.Discovering`.** `Powered ↔ Discovering`. The backend tracks whether *it* started the discovery or whether another client did; see §9.1.
- **`InterfaceRemoved`** from the Interface Monitor. Force-transitions to `Gone` regardless of BlueZ's view (kernel is authoritative for hardware presence).

Note: `Unavailable → Powered` is possible as a single atomic transition if BlueZ appears with the adapter already powered (common on boot). The state machine handles this as `Unavailable → Present → Powered` in two observations, not a special case.

---

## 5. Device Lifecycle

### 5.1 Device States

Device state is per-`(adapter, bluetooth_address)` pair. A device seen by two adapters is two separate entries in the backend.

```rust
enum BtDeviceState {
    /// Device seen in discovery, not paired. Transient for
    /// non-bondable BLE peripherals (beacons, etc.) — they stay
    /// in this state and are pruned when discovery ends.
    Discovered,

    /// Pairing exchange in progress. The Agent is handling callbacks
    /// (PIN, passkey, confirmation). Holds a correlation id tying
    /// Agent callbacks back to the in-flight operation.
    Pairing { job_id: PairingJobId, started_at: Instant },

    /// Bonded (BlueZ Paired=true). Not currently connected.
    Paired,

    /// Connection attempt in flight. BlueZ's Connect() call
    /// outstanding, or waiting for peer to complete its side.
    Connecting { since: Instant },

    /// BlueZ's Connected=true. Services (for Classic) or GATT (for
    /// BLE) are available to consumers.
    Connected {
        since: Instant,
        services: Vec<String>,  // UUIDs from BlueZ's UUIDs property
    },

    /// Explicit disconnect initiated; waiting for BlueZ to ack.
    Disconnecting,

    /// Last operation (pair or connect) failed. Error details in the
    /// variant so D-Bus clients can present a useful message. The
    /// device stays in this state until explicit operator action
    /// (retry, forget, or discovery restart).
    Failed { reason: BtFailureReason, at: Instant },

    /// Device removed from BlueZ's registry (forgotten, or adapter
    /// was reset). Entry is about to be dropped.
    Removed,
}

enum BtFailureReason {
    PairingRejected,      // local or peer rejected
    PairingTimeout,
    PairingAuthFailed,    // PIN/passkey mismatch
    ConnectionFailed,     // peer unreachable or BlueZ error
    Unknown(String),      // BlueZ error text we don't map explicitly
}
```

### 5.2 Device Transitions

```
    InterfacesAdded(/org/bluez/hciN/dev_XX_XX...)
  [initial] ────────────────────────────────────►  Discovered
                                                       │
                                                       │ operator calls Pair()
                                                       ▼
                                              ┌── Pairing ──┐
                                              │             │
                                 pair success │             │ pair failure
                                              ▼             ▼
                                            Paired       Failed
                                              │             │
                                              │             │ operator
                                              │             │ retry
                                              │             │ (pair or connect)
                                              │             ▼
                                              ├─► Connecting ◄──┐
                                              │       │         │
                                   operator   │       │ connect │
                                     calls    │       │ success │
                                  Connect()   │       ▼         │
                                              │   Connected     │
                                              │       │         │
                                              │       │ peer    │
                                              │       │ disconn │
                                              │       │ or      │
                                              │       │ operator│
                                              │       │ calls   │
                                              │       │ Disconn │
                                              │       ▼         │
                                              │  Disconnecting  │
                                              │       │         │
                                              │       │ ack     │
                                              │       ▼         │
                                              └── Paired ───────┘

  From any state, InterfacesRemoved or Forget() → Removed
```

Transitions are driven by:

- **`InterfacesAdded`** for a device object. Creates the entry in `Discovered` state, populating initial properties (name, RSSI, UUIDs, address type).
- **`PropertiesChanged`** on the device object. Drives transitions based on which property changed:
  - `Paired = true` from any state: move to `Paired` if not already there.
  - `Connected = true` from `Paired` or `Connecting`: move to `Connected`.
  - `Connected = false` from `Connected`: move to `Paired` (bonded devices remain known) or `Discovered` (unbonded BLE).
- **Operator method calls** (D-Bus): `Pair` transitions `Discovered → Pairing`; `Connect` transitions `Paired → Connecting` or `Discovered → Connecting` (unbonded BLE); `Disconnect` transitions `Connected → Disconnecting`; `Forget` transitions any state → `Removed`.
- **Agent callback resolution.** Pairing ends when the `Pair()` call returns success (transition to `Paired`) or error (transition to `Failed`).
- **`InterfacesRemoved`** for the device object. Force-transitions to `Removed`.

### 5.3 BLE vs Classic

BlueZ presents both BLE and Classic devices through the same `org.bluez.Device1` interface, with `AddressType` distinguishing them (`"public"` / `"random"` for BLE, `"bredr"` for Classic). This means most of the state machine is unified, but a few behaviors differ:

- **Discovery.** Classic devices respond to inquiry scans; BLE devices advertise. BlueZ's discovery session drives both, but RSSI and distance estimates are only available during active scanning (BlueZ's `Transport = "auto"` default handles this).
- **Pairing.** Classic pairing typically uses Secure Simple Pairing (SSP) with numeric comparison or passkey entry; some legacy devices use PIN-only. BLE pairing uses the Security Manager with LE Secure Connections (4.2+) or LE Legacy Pairing; involvement of out-of-band data (OOB) is rare but supported.
- **Unbonded connections.** BLE devices can be connected without bonding (e.g., ephemeral sensors, non-security-relevant peripherals). Classic does not support this in practice. The state machine allows `Discovered → Connecting` specifically for BLE devices; Classic devices must pair first.
- **Random-resolvable addresses.** BLE privacy-enabled peripherals rotate their address periodically; the IRK (identity resolving key) exchanged during bonding lets the adapter recognize the same peer across rotations. BlueZ handles this transparently — device objects are keyed by the *identity* address once bonded.
- **Services property.** For Classic, `UUIDs` reflects SDP records. For BLE, it reflects GATT services. Nexus presents both as strings but doesn't further interpret them; the operator or higher-level app decides what each UUID means.

A device's `transport` is exposed in `BtDeviceInfo` and in the D-Bus `fi.nexus.BluetoothDevice` interface (DD-006 §6.6) as the string `"bredr"` / `"le"` / `"dual"`.

---

## 6. BlueZ Abstraction

### 6.1 Trait Definition

```rust
/// A BlueZ client backend. Implementations drive BlueZ via D-Bus and
/// translate its ObjectManager + PropertiesChanged signals into
/// structured callbacks on the Bluetooth Backend.
///
/// Construction convention (not part of the object-safe trait):
/// ```ignore
/// impl ZbusBluezClient {
///     pub async fn new(
///         connection: zbus::Connection,     // usually system bus
///         event_tx: broadcast::Sender<NexusEvent>,
///     ) -> Result<Self> { ... }
/// }
/// ```
///
/// Progress is reported asynchronously via the Nexus event bus. The
/// backend subscribes to NexusEvent::Bt* variants to drive state
/// machines. The one exception is `refresh_adapter`: the reconcile
/// tick (§7.3) polls it once per adapter per second as a self-healing
/// backstop against a missed `InterfacesAdded`/`PropertiesChanged`
/// signal, and as the *only* way to learn Address on hardware with no
/// kernel sysfs address attribute (UART/serdev-attached controllers)
/// — everything else stays event-driven.
#[async_trait]
pub trait BluezClient: Send + Sync {
    /// Establish or re-establish the connection to BlueZ. Idempotent.
    /// On success, emits NexusEvent::BluezConnected and begins
    /// republishing the current ObjectManager tree (triggers
    /// BtAdapterChanged / BtDeviceDiscovered for every existing
    /// adapter and device).
    ///
    /// Note: takes &self because implementations store connection
    /// state behind internal synchronization (Mutex / OnceCell). This
    /// lets the backend keep the client as `Arc<dyn BluezClient>` and
    /// clone handles into spawned tasks (e.g., the pair-driver task
    /// in §7.2) without a borrow-checker fight.
    async fn connect(&self) -> Result<()>;

    /// Report whether the client currently has a live connection to
    /// BlueZ. Used by the reconcile supervisor (§7.2) to decide
    /// whether to retry connect(). Should return quickly.
    fn is_connected(&self) -> bool;

    /// Re-read Powered/Discovering/Address for `adapter` directly,
    /// bypassing the signal stream. Returns (powered, discovering,
    /// address). Used by the reconcile supervisor (§7.3) as a
    /// self-healing backstop against a missed
    /// InterfacesAdded/PropertiesChanged signal, and — for Address —
    /// as the authoritative source outright: BlueZ's own
    /// Adapter1.Address is correct even on hardware where the kernel
    /// never exposes a sysfs address at all (UART/serdev-attached
    /// controllers), which the udev-based discovery path in
    /// nexus-interface-monitor can't cover no matter how it's probed.
    async fn refresh_adapter(&self, adapter: &str) -> Result<(bool, bool, MacAddr)>;

    /// Set the adapter's Powered property.
    async fn set_powered(&self, adapter: &str, on: bool) -> Result<()>;

    /// Set the adapter's Discoverable property.
    async fn set_discoverable(&self, adapter: &str, on: bool) -> Result<()>;

    /// Set the adapter's Pairable property. When false, BlueZ rejects
    /// incoming pairing requests at the HCI level — useful for locked-down
    /// deployments.
    async fn set_pairable(&self, adapter: &str, on: bool) -> Result<()>;

    /// Begin a discovery session on the adapter. Idempotent — if
    /// already discovering, returns Ok without calling BlueZ again.
    /// `filter` applies a transport / RSSI / UUID-list filter before
    /// starting (§9.2). Caller is responsible for stopping the session.
    async fn start_discovery(
        &self,
        adapter: &str,
        filter: DiscoveryFilter,
    ) -> Result<()>;

    /// Stop a discovery session on the adapter. Idempotent.
    async fn stop_discovery(&self, adapter: &str) -> Result<()>;

    /// Initiate pairing with a device. The Agent registered by the
    /// backend handles any callbacks (PIN, passkey). Returns when
    /// BlueZ's Pair() method returns — success, error, or timeout.
    async fn pair(&self, device_path: &str) -> Result<()>;

    /// Cancel an in-flight pairing. Sends BlueZ's CancelPairing.
    async fn cancel_pairing(&self, device_path: &str) -> Result<()>;

    /// Mark a paired device trusted. Trusted devices can auto-connect
    /// without per-connection confirmation from the agent.
    async fn set_trusted(&self, device_path: &str, on: bool) -> Result<()>;

    /// Connect to a device. For Classic, typically follows pairing.
    /// For BLE, can be called on an unbonded device.
    async fn connect_device(&self, device_path: &str) -> Result<()>;

    /// Disconnect a device but keep it bonded.
    async fn disconnect_device(&self, device_path: &str) -> Result<()>;

    /// Forget a device: remove its bond from BlueZ, drop from its
    /// adapter's Devices list. Maps to BlueZ's RemoveDevice.
    async fn forget_device(
        &self,
        adapter: &str,
        device_path: &str,
    ) -> Result<()>;

    /// Backend identifier for logging and metrics. Typically "bluez-zbus".
    fn name(&self) -> &'static str;
}
```

The trait is deliberately narrow — it's the operational vocabulary, nothing else. Property-change notifications come in through the event bus (emitted by the client's internal zbus signal subscriptions), not through trait methods. This keeps the trait small enough to mock for tests while exposing every operation the backend actually needs.

### 6.2 Shared Types

```rust
// nexus-core already defines MacAddr (see DD-003 §4.2), used for Wi-Fi
// BSSIDs. Bluetooth addresses are the same 48-bit MAC-family format, so
// DD-004 reuses the type rather than introducing a BtAddr synonym.
// Bluetooth-format helpers live in an extension trait so they can be
// defined in nexus-bluetooth without making nexus-core aware of BlueZ's
// string conventions:
pub trait BluetoothAddrExt {
    /// Parse BlueZ's "XX:XX:XX:XX:XX:XX" format (uppercase, colons).
    fn from_bluez(s: &str) -> Result<MacAddr, ParseMacAddrError>;
    /// Format as BlueZ's canonical "XX:XX:XX:XX:XX:XX".
    fn to_bluez(&self) -> String;
    /// Format as BlueZ's device object path component,
    /// e.g. "dev_AA_BB_CC_DD_EE_FF" (uppercase hex, underscore separators).
    fn to_object_path_component(&self) -> String;
}

impl BluetoothAddrExt for MacAddr { /* ... */ }

// In nexus-bluetooth:

/// Snapshot of a device's current BlueZ-known properties. Emitted in
/// NexusEvent::BtDeviceDiscovered and on every property change.
#[derive(Debug, Clone)]
pub struct BtDeviceInfo {
    /// The adapter this device is scoped to, e.g., "/org/bluez/hci0".
    pub adapter: String,

    /// BlueZ's object path for the device, e.g.,
    /// "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".
    pub device_path: String,

    /// Bluetooth address.
    pub address: MacAddr,

    /// Address type as reported by BlueZ.
    pub address_type: BtAddressType,

    /// Friendly name from the device's GAP record, if resolved.
    /// Some BLE advertisers don't include a name.
    pub name: Option<String>,

    /// Alias — BlueZ's editable local label. If the local operator
    /// has set this, it's surfaced here; otherwise equals `name`.
    pub alias: Option<String>,

    /// RSSI in dBm, if recently observed. Stale after the adapter
    /// leaves discovery mode.
    pub rssi: Option<i16>,

    /// Transmit power the peer is advertising (BLE only, optional).
    pub tx_power: Option<i16>,

    /// Service UUIDs. For Classic devices, from SDP; for BLE, from
    /// GATT. Strings are lowercase full-form UUIDs
    /// ("0000180f-0000-1000-8000-00805f9b34fb").
    pub uuids: Vec<String>,

    /// Bluetooth transport. BlueZ does not expose this as a single
    /// property on Device1; the backend synthesizes it from
    /// `address_type` and the `UUIDs` list at parse time:
    /// - `address_type = Bredr` → `BtTransport::Bredr`
    /// - `address_type = LePublic | LeRandom` without any Classic-only
    ///   service UUID (e.g., HFP, A2DP) → `BtTransport::Le`
    /// - `address_type = LePublic` with at least one Classic service
    ///   UUID → `BtTransport::Dual` (typical of phones, laptops)
    /// The `Dual` classification is a best-effort heuristic — it errs
    /// toward Dual when ambiguous to avoid hiding capabilities.
    pub transport: BtTransport,

    /// Manufacturer data from GAP/advertisement: keyed by the IEEE
    /// manufacturer ID, value is the raw manufacturer-specific bytes.
    pub manufacturer_data: HashMap<u16, Vec<u8>>,

    /// Current BlueZ flags that the backend surfaces for state decisions.
    pub paired: bool,
    pub bonded: bool,
    pub trusted: bool,
    pub blocked: bool,
    pub connected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtAddressType {
    /// Classic BR/EDR public address.
    Bredr,
    /// BLE public identity address.
    LePublic,
    /// BLE random address — may be static random, RPA (resolvable
    /// private), or NRPA. BlueZ doesn't distinguish further in the
    /// AddressType property.
    LeRandom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtTransport {
    Bredr,
    Le,
    Dual,   // device supports both (e.g., phone, laptop)
}

/// Correlation id for an in-flight pairing operation. Emitted by the
/// backend when pairing starts, referenced by Agent callback events
/// so D-Bus clients can tie UI prompts back to the pairing they were
/// asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PairingJobId(pub Ulid);

/// Kind of Agent callback, mapped from BlueZ's Agent1 methods (per
/// the BlueZ 5.x D-Bus API: `doc/agent-api.txt` in the BlueZ source).
/// Determines which fields of PairingPromptData are meaningful and
/// which PairingAnswer variant the operator should produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingPromptKind {
    /// BlueZ invoked `RequestPinCode(device) -> string`. Legacy PIN
    /// entry for Classic devices that don't support SSP. Operator
    /// enters a PIN (typically 4-6 digits). Response:
    /// PairingAnswer::Pin.
    RequestPin,
    /// BlueZ invoked `RequestPasskey(device) -> uint32`. The peer is
    /// displaying a 6-digit passkey; the operator reads it off the peer
    /// and enters it here. Response: PairingAnswer::Passkey.
    RequestPasskey,
    /// BlueZ invoked `DisplayPasskey(device, passkey, entered)`. A
    /// notification, not a question: Nexus displays `passkey` to the
    /// operator, who types it on the peer. `entered` is how many
    /// digits the peer has reported so far (not surfaced by Nexus).
    /// The method returns void — no PairingAnswer is expected, but the
    /// backend still allocates a oneshot so the D-Bus client can signal
    /// "the operator has seen the prompt" via
    /// PairingAnswer::Acknowledge. The Agent returns to BlueZ as soon
    /// as the acknowledge arrives (or after pairing_timeout_s).
    DisplayPasskey,
    /// BlueZ invoked `DisplayPinCode(device, pincode)`. Same shape as
    /// DisplayPasskey but for legacy-PIN peers. Also notification-only;
    /// uses PairingAnswer::Acknowledge.
    DisplayPin,
    /// BlueZ invoked `RequestConfirmation(device, passkey)`. Both sides
    /// have computed the same 6-digit passkey via SSP numeric
    /// comparison; the operator confirms they match.
    /// Response: PairingAnswer::Accept(bool).
    RequestConfirmation,
    /// BlueZ invoked `RequestAuthorization(device)`. Peer is trying to
    /// connect to a paired device without a pending pairing (e.g., a
    /// phone reconnecting to a keyboard). Operator authorizes.
    /// Response: PairingAnswer::Accept(bool).
    RequestAuthorization,
    /// BlueZ invoked `AuthorizeService(device, uuid)`. Peer wants to
    /// use a specific service UUID on an already-paired device. Profile
    /// flag `auto_accept_incoming` may auto-respond (§11).
    /// Response: PairingAnswer::Accept(bool).
    AuthorizeService,
}

/// Data accompanying a PairingPromptKind. Each kind populates a
/// different subset; unused fields are None.
#[derive(Debug, Clone)]
pub struct PairingPromptData {
    /// BlueZ device path the prompt is about.
    pub device_path: String,
    /// Passkey to display or confirm (DisplayPasskey, DisplayPin,
    /// RequestConfirmation). Always a 6-digit 000000..999999 per the
    /// Bluetooth SSP spec for the first three; the PIN variants allow
    /// shorter strings (see PairingAnswer::Pin).
    pub passkey: Option<u32>,
    /// PIN code to display (DisplayPin only). Strings rather than u32
    /// because legacy PINs can be 4-16 characters of printable ASCII.
    pub pincode: Option<String>,
    /// Service UUID being authorized (AuthorizeService only).
    pub service_uuid: Option<String>,
}

/// The answer to a pairing prompt, sent back to the waiting Agent
/// method via the pending_prompt_answers oneshot.
#[derive(Debug, Clone)]
pub enum PairingAnswer {
    /// PIN entered by the operator (RequestPin).
    Pin(String),
    /// Passkey entered by the operator (RequestPasskey).
    Passkey(u32),
    /// Yes/no for any confirmation-style prompt (RequestConfirmation,
    /// RequestAuthorization, AuthorizeService).
    Accept(bool),
    /// The operator has seen a notification-only prompt (DisplayPasskey,
    /// DisplayPin). Tells the Agent it can return to BlueZ now; the
    /// peer-side entry has presumably happened or will happen soon.
    Acknowledge,
    /// Operator cancelled or the prompt timed out. The Agent returns
    /// an error to BlueZ, which aborts the pairing.
    Cancel,
}
```

---

## 7. Core Backend Logic

### 7.1 Event Flow Overview

```
BlueZ ─ObjectManager signals─► ZbusBluezClient ──┐
       ─PropertiesChanged────►                   │
                                                 │
                         (translates to events)  │
                                                 ▼
                                    NexusEvent::Bt* ─► bus
                                                 │
                              ┌──────────────────┤
                              ▼                  ▼
               BluetoothBackend.handle_event   D-Bus layer
                • updates adapter/device         • emits fi.nexus.Bluetooth
                  state machines                   signals
                • routes Agent callbacks to      • updates Interface /
                  NotificationEvent              Device properties
                • persists profiles
                                                 ▲
                                                 │
 Agent D-Bus interface ─► AgentCallbacks ────────┘
   (registered with                   (translated to NexusEvent)
    BlueZ at startup)
```

Key points:

- The BlueZ client emits `NexusEvent` variants from both ObjectManager signals and PropertiesChanged; the backend is the sole state-machine owner.
- The **Agent** is a separate D-Bus object the backend registers with BlueZ. Its callbacks (PIN request, passkey confirmation) are translated into `NexusEvent::BtPairingPrompt` with a `PairingJobId`, allowing D-Bus clients to respond via a `fi.nexus.Bluetooth.AnswerPairingPrompt` method.
- The backend never subscribes to its own emitted events; loops are impossible by construction.

### 7.2 Lifecycle Handler

```rust
struct BtAdapterEntry {
    info: InterfaceInfo,           // from DD-001
    bluez_path: String,            // "/org/bluez/hci0"
    state: BtAdapterState,
    powered: bool,
    discoverable: bool,
    pairable: bool,
    /// Devices known to this adapter, keyed by BlueZ device path.
    devices: HashMap<String, BtDeviceEntry>,
    /// True when Nexus has an outstanding StartDiscovery on this
    /// adapter. See §9.1.
    nexus_has_discovery_session: bool,
    discovery_started_at: Option<Instant>,
}

struct BtDeviceEntry {
    info: BtDeviceInfo,
    state: BtDeviceState,
    profile: Option<BluetoothProfile>,  // populated if we have a stored profile
    /// In-flight pairing job. None when not pairing.
    pairing_job: Option<PairingJobId>,
}

/// Commands sent to the backend from other tasks. The D-Bus layer,
/// the registered Agent, and operator-facing D-Bus method handlers
/// all funnel their requests through this channel. The backend's
/// main task pulls from the event-bus subscription (NexusEvent) AND
/// from this command channel, so all state changes happen on one task.
enum BtCommand {
    // -- Operator-driven (from D-Bus methods in DD-006 §6.4) --
    Pair { device_path: String, responder: oneshot::Sender<Result<PairingJobId>> },
    Connect { device_path: String, responder: oneshot::Sender<Result<()>> },
    Disconnect { device_path: String, responder: oneshot::Sender<Result<()>> },
    Forget {
        adapter: String,
        device_path: String,
        responder: oneshot::Sender<Result<()>>,
    },
    SetAdapterPowered { adapter: String, on: bool, responder: oneshot::Sender<Result<()>> },
    StartDiscovery {
        adapter: String,
        filter: DiscoveryFilter,
        responder: oneshot::Sender<Result<()>>,
    },
    StopDiscovery { adapter: String, responder: oneshot::Sender<Result<()>> },
    CancelPairing { device_path: String, responder: oneshot::Sender<Result<()>> },

    /// Operator response to an Agent prompt. The backend looks up the
    /// pending oneshot in pending_prompt_answers and resolves it,
    /// which wakes the Agent task so it can return the answer to BlueZ.
    AnswerPairingPrompt {
        job_id: PairingJobId,
        answer: PairingAnswer,
        responder: oneshot::Sender<Result<()>>,
    },

    // -- Agent-driven (from the zbus Agent task; see §8.1) --

    /// The Agent is about to await an operator response for a pairing
    /// prompt. It sends this command to deposit a oneshot Sender with
    /// the backend, then awaits its own Receiver. The backend hands the
    /// Sender to whoever calls AnswerPairingPrompt with the matching
    /// job_id; the Agent then proceeds to return the answer to BlueZ.
    ///
    /// The Agent decides the job_id (usually by looking up the device's
    /// current pairing_job via a query command) — see §8.1 for the
    /// Agent task flow.
    RegisterPromptOneshot {
        job_id: PairingJobId,
        sender: oneshot::Sender<PairingAnswer>,
        /// Also used to emit BtPairingPrompt through the backend, since
        /// only the backend should touch event_tx directly.
        kind: PairingPromptKind,
        data: PairingPromptData,
    },

    /// The Agent asks the backend which pairing_job (if any) is
    /// currently in flight for the given device path. Used by the
    /// Agent when BlueZ invokes a callback — it needs a job_id to
    /// correlate with; the backend owns the pairing_job state.
    LookupPairingJob {
        device_path: String,
        responder: oneshot::Sender<Option<PairingJobId>>,
    },

    /// The Agent asks the backend whether an incoming-authorization
    /// callback (RequestAuthorization or AuthorizeService) should be
    /// auto-responded based on the device's stored profile. Lets the
    /// Agent short-circuit the prompt path for devices the operator
    /// has pre-authorized — keyboards, mice, known peripherals —
    /// without emitting BtPairingPrompt events the UI would have to
    /// auto-dismiss.
    ///
    /// Used by RequestAuthorization (service_uuid = None) and by
    /// AuthorizeService (service_uuid = Some(uuid)). The backend's
    /// logic is:
    ///   - No stored profile → AuthorizationDecision::Prompt.
    ///   - Profile exists, auto_accept_incoming = false → Prompt.
    ///   - Profile exists, auto_accept_incoming = true, service_uuid
    ///     is None → Accept.
    ///   - Profile exists, auto_accept_incoming = true,
    ///     authorized_services empty → Accept (operator opted in
    ///     to everything).
    ///   - Profile exists, auto_accept_incoming = true, service_uuid
    ///     in authorized_services → Accept.
    ///   - Otherwise → Prompt.
    /// See §8.1 for the Agent-side flow.
    LookupAuthorizationPolicy {
        device_path: String,
        service_uuid: Option<String>,
        responder: oneshot::Sender<AuthorizationDecision>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationDecision {
    /// Accept without asking the operator.
    Accept,
    /// Reject without asking (future use; currently the backend never
    /// returns this, but the enum allows for an explicit block-list
    /// flow without introducing a third command later).
    Reject,
    /// Fall through to the normal Agent prompt path —
    /// RegisterPromptOneshot + BtPairingPrompt.
    Prompt,
}

/// Backend struct, showing only the fields relevant to handle_event
/// and the pairing flow.
struct BluetoothBackend {
    adapters: HashMap<u32 /* ifindex */, BtAdapterEntry>,
    /// Arc so the pair-driver task (§7.2 start_pairing) can clone a
    /// handle for its spawned work. All BluezClient trait methods take
    /// &self; the concrete zbus impl uses interior mutability for its
    /// connection state.
    bluez: Arc<dyn BluezClient>,
    profile_store: Arc<dyn ProfileStore>,
    event_tx: broadcast::Sender<NexusEvent>,
    event_rx: broadcast::Receiver<NexusEvent>,
    cmd_rx: mpsc::Receiver<BtCommand>,
    /// Clone given to each D-Bus handler and to the Agent task so they
    /// can send commands back.
    cmd_tx: mpsc::Sender<BtCommand>,
    config: BluetoothConfig,

    /// Pairing-prompt oneshot senders deposited by the Agent task via
    /// RegisterPromptOneshot. Resolved by AnswerPairingPrompt, which
    /// wakes the corresponding Agent method handler so it can return
    /// the answer to BlueZ. All access is through the main task; the
    /// Agent task never touches this map directly.
    ///
    /// Assumption: at most one outstanding prompt per PairingJobId.
    /// BlueZ's SSP flow invokes at most one Agent method per pairing
    /// (RequestConfirmation, DisplayPasskey, etc.); legacy multi-step
    /// flows (PIN then confirm) aren't seen in practice on modern
    /// stacks. If two Agent callbacks fired concurrently for the same
    /// job_id, the second RegisterPromptOneshot would replace the
    /// first Sender and the first operator answer would be dropped.
    /// If this assumption is ever violated, replace the value type
    /// with `Vec<oneshot::Sender<PairingAnswer>>`.
    pending_prompt_answers: HashMap<PairingJobId, oneshot::Sender<PairingAnswer>>,

    /// When BlueZ was last reachable; used to trigger the
    /// subsystem_unavailable notification after the configured
    /// outage threshold.
    first_bluez_outage_at: Option<Instant>,
    outage_notified: bool,
    last_reconnect_attempt: Option<Instant>,
    reconnect_attempts: u32,
}

/// Handler called when a NexusEvent arrives.
async fn handle_event(&mut self, event: NexusEvent) -> Result<()> {
    match event {
        NexusEvent::InterfaceDiscovered(info)
            if matches!(info.kind, InterfaceKind::Bluetooth { .. }) =>
        {
            let (bluez_path, hci_name) = match &info.kind {
                InterfaceKind::Bluetooth { bluez_path, hci_name, .. } => {
                    (bluez_path.clone(), hci_name.clone())
                }
                _ => unreachable!(),
            };

            self.adapters.insert(info.ifindex, BtAdapterEntry {
                info,
                bluez_path,
                state: BtAdapterState::Unavailable,
                powered: false,
                discoverable: false,
                pairable: false,
                devices: HashMap::new(),
                nexus_has_discovery_session: false,
                discovery_started_at: None,
            });

            // If BlueZ is already connected, the ObjectManager republish
            // will emit BtAdapterChanged for this adapter shortly. If
            // not, the state stays Unavailable until BlueZ reconnects.
        }

        NexusEvent::InterfaceRemoved { ifindex } => {
            if let Some(entry) = self.adapters.remove(&ifindex) {
                // Force-transition regardless of BlueZ state.
                for (_, device_entry) in &entry.devices {
                    // Emit one BtDeviceDisconnected per previously-connected
                    // device so consumers clean up.
                    if matches!(device_entry.state,
                        BtDeviceState::Connected { .. } | BtDeviceState::Connecting { .. })
                    {
                        let _ = self.event_tx.send(NexusEvent::BtDeviceDisconnected {
                            adapter: entry.bluez_path.clone(),
                            address: device_entry.info.address,
                        });
                    }
                }
                // State becomes Gone via removal from registry.
            }
        }

        NexusEvent::BluezConnected => {
            // BlueZ is live. The client will have started republishing
            // ObjectManager entries, which arrive as subsequent
            // BtAdapterChanged / BtDeviceDiscovered events. Nothing to
            // do here except log.
            info!("BlueZ connected; awaiting ObjectManager republish");
        }

        NexusEvent::BluezDisconnected => {
            // Mark all adapters Unavailable but keep their entries;
            // they'll be republished when BlueZ returns.
            for entry in self.adapters.values_mut() {
                entry.state = BtAdapterState::Unavailable;
                entry.devices.clear();   // devices are reconstructed from ObjectManager
            }
            if self.first_bluez_outage_at.is_none() {
                self.first_bluez_outage_at = Some(Instant::now());
            }
        }

        NexusEvent::BtAdapterChanged { adapter, powered, discovering } => {
            self.on_adapter_props_changed(&adapter, powered, discovering).await?;
        }

        NexusEvent::BtDeviceDiscovered(info) => {
            self.on_device_added(info).await?;
        }

        NexusEvent::BtDeviceConnected { adapter, address } => {
            self.on_device_connected(&adapter, address).await?;
        }

        NexusEvent::BtDeviceDisconnected { adapter, address } => {
            self.on_device_disconnected(&adapter, address).await?;
        }

        NexusEvent::BtPairingPrompt { job_id, kind, data } => {
            // Agent callback translated to an event. Forward to D-Bus
            // (via DD-006 §5.3 NotificationEvent path) so operators
            // can respond. No backend-side state change — we're just
            // routing.
            self.on_pairing_prompt(job_id, kind, data).await?;
        }

        NexusEvent::BtPairingComplete { job_id, success, reason } => {
            // The pair driver task (spawned in start_pairing) emitted
            // this. Finalize the device state and persist a profile
            // on success.
            self.on_pairing_complete(job_id, success, reason).await?;
        }

        _ => {}
    }

    Ok(())
}

/// Handle an adapter's property change (powered and/or discovering).
/// Called from both PropertiesChanged signals and from the
/// ObjectManager republish after BlueZ reconnect.
async fn on_adapter_props_changed(
    &mut self,
    bluez_path: &str,
    powered: bool,
    discovering: bool,
) -> Result<()> {
    let Some(entry) = self.adapter_by_bluez_path_mut(bluez_path) else {
        // Adapter path we don't know about — BlueZ saw something the
        // Interface Monitor hasn't. Ignore; DD-001 is authoritative
        // for adapter presence.
        return Ok(());
    };

    entry.powered = powered;

    entry.state = match (&entry.state, powered, discovering) {
        (BtAdapterState::Unavailable, false, _) => BtAdapterState::Present,
        (BtAdapterState::Unavailable, true, false) => BtAdapterState::Powered,
        (BtAdapterState::Unavailable, true, true)
            => BtAdapterState::Discovering { since: Instant::now() },
        (BtAdapterState::Present, true, false) => BtAdapterState::Powered,
        (BtAdapterState::Present, true, true)
            => BtAdapterState::Discovering { since: Instant::now() },
        (BtAdapterState::Powered, false, _) => BtAdapterState::Present,
        (BtAdapterState::Powered, true, true)
            => BtAdapterState::Discovering { since: Instant::now() },
        (BtAdapterState::Discovering { since }, true, true)
            => BtAdapterState::Discovering { since: *since },
        (BtAdapterState::Discovering { .. }, true, false) => BtAdapterState::Powered,
        (BtAdapterState::Discovering { .. }, false, _) => BtAdapterState::Present,
        (current, _, _) => current.clone(),  // no transition
    };

    Ok(())
}

/// Handle a device being added to the ObjectManager.
async fn on_device_added(&mut self, info: BtDeviceInfo) -> Result<()> {
    let Some(adapter_entry) = self.adapter_by_bluez_path_mut(&info.adapter) else {
        return Ok(());
    };

    // Did we have a stored profile for this device?
    let profile = self.profile_store
        .load_bluetooth_profile_by_address(&info.address)
        .await?;

    // Figure out the initial state based on current BlueZ-reported flags.
    let initial_state = if info.connected {
        BtDeviceState::Connected {
            since: Instant::now(),
            services: info.uuids.clone(),
        }
    } else if info.paired {
        BtDeviceState::Paired
    } else {
        BtDeviceState::Discovered
    };

    // Capture auto_connect decision before moving profile into the entry.
    let auto_connect = profile.as_ref()
        .map(|p| p.auto_connect && !info.blocked)
        .unwrap_or(false);
    let device_path = info.device_path.clone();

    adapter_entry.devices.insert(info.device_path.clone(), BtDeviceEntry {
        info,
        state: initial_state.clone(),
        profile,
        pairing_job: None,
    });

    // Auto-connect if: we have a profile with auto_connect=true, the
    // device is currently Paired (not Connected, not Discovered-unbonded),
    // and it isn't blocked. The Paired check avoids double-Connect
    // races — on first appearance we only try if BlueZ currently
    // shows the device as not-connected.
    if auto_connect && matches!(initial_state, BtDeviceState::Paired) {
        // Fire a Connect through the normal command path so metrics
        // and state transitions happen via the usual flow. Use a
        // dummy oneshot since there's nobody to receive the result.
        let (tx, _rx) = oneshot::channel();
        let _ = self.cmd_tx.send(BtCommand::Connect {
            device_path,
            responder: tx,
        }).await;
    }

    Ok(())
}

/// Handle a device connection, driven by BlueZ's Connected=true
/// property change. Transitions Paired|Connecting|Discovered to Connected.
async fn on_device_connected(
    &mut self,
    adapter_bluez_path: &str,
    address: MacAddr,
) -> Result<()> {
    let Some(device_entry) = self.device_by_address_mut(adapter_bluez_path, &address) else {
        return Ok(());
    };
    device_entry.info.connected = true;
    device_entry.state = BtDeviceState::Connected {
        since: Instant::now(),
        services: device_entry.info.uuids.clone(),
    };
    Ok(())
}

/// Handle a device disconnection, driven by BlueZ's Connected=false
/// property change. Bonded devices fall back to Paired; unbonded BLE
/// devices fall back to Discovered.
async fn on_device_disconnected(
    &mut self,
    adapter_bluez_path: &str,
    address: MacAddr,
) -> Result<()> {
    let Some(device_entry) = self.device_by_address_mut(adapter_bluez_path, &address) else {
        return Ok(());
    };
    device_entry.info.connected = false;
    device_entry.state = if device_entry.info.paired {
        BtDeviceState::Paired
    } else {
        BtDeviceState::Discovered
    };
    Ok(())
}

/// Route an Agent callback (translated to a BtPairingPrompt event) to
/// downstream consumers. The backend doesn't change state here — the
/// state is already Pairing — but it does emit a NotificationEvent so
/// the D-Bus layer can surface the prompt to operator UI.
///
/// The oneshot sender that BlueZ's Agent method is awaiting was already
/// placed in `pending_prompt_answers` when the Agent method fired; it
/// gets signalled by the matching BtCommand::AnswerPairingPrompt.
async fn on_pairing_prompt(
    &mut self,
    job_id: PairingJobId,
    kind: PairingPromptKind,
    data: PairingPromptData,
) -> Result<()> {
    // Surface the prompt via a NotificationEvent (DD-006 §5.3).
    // The D-Bus layer picks this up and emits the operator-visible
    // signal. D-Bus clients respond by calling
    // fi.nexus.Bluetooth.AnswerPairingPrompt with the job_id.
    let _ = self.event_tx.send(NexusEvent::OperatorNotification {
        kind: "bluetooth_pairing_prompt".to_string(),
        data: build_prompt_notification(job_id, kind, data),
    });
    Ok(())
}

/// Process a command from another task — D-Bus method handlers, the
/// registered Agent, or internal driver tasks. This is where operator-
/// driven actions are translated into BluezClient calls and where the
/// Agent coordinates with the backend's owned state (pending_prompt_answers,
/// pairing_job lookups).
async fn handle_command(&mut self, cmd: BtCommand) {
    match cmd {
        // -- Operator commands --

        BtCommand::Pair { device_path, responder } => {
            let result = self.start_pairing(&device_path).await;
            let _ = responder.send(result);
        }
        BtCommand::AnswerPairingPrompt { job_id, answer, responder } => {
            let result = match self.pending_prompt_answers.remove(&job_id) {
                Some(tx) => tx.send(answer).map_err(|_| BtError::PairingJobGone),
                None => Err(BtError::UnknownPairingJob(job_id)),
            };
            let _ = responder.send(result);
        }
        BtCommand::Connect { device_path, responder } => {
            if let Some(entry) = self.device_by_path_mut(&device_path) {
                entry.state = BtDeviceState::Connecting { since: Instant::now() };
            }
            let result = self.bluez.connect_device(&device_path).await;
            let _ = responder.send(result);
        }
        BtCommand::Disconnect { device_path, responder } => {
            if let Some(entry) = self.device_by_path_mut(&device_path) {
                entry.state = BtDeviceState::Disconnecting;
            }
            let result = self.bluez.disconnect_device(&device_path).await;
            let _ = responder.send(result);
        }
        BtCommand::Forget { adapter, device_path, responder } => {
            let result = self.forget_device(&adapter, &device_path).await;
            let _ = responder.send(result);
        }
        BtCommand::CancelPairing { device_path, responder } => {
            // Operator-driven cancel. The in-flight pair-driver task
            // will see Pair() return a cancellation error and emit
            // BtPairingComplete with PairingRejected. We do NOT drop
            // the pending_prompt_answers entry here; on_pairing_complete
            // will clean up.
            let result = self.bluez.cancel_pairing(&device_path).await;
            let _ = responder.send(result);
        }
        BtCommand::SetAdapterPowered { adapter, on, responder } => {
            let result = self.bluez.set_powered(&adapter, on).await;
            let _ = responder.send(result);
        }
        BtCommand::StartDiscovery { adapter, filter, responder } => {
            let result = self.bluez.start_discovery(&adapter, filter).await;
            if result.is_ok() {
                if let Some(entry) = self.adapter_by_bluez_path_mut(&adapter) {
                    entry.nexus_has_discovery_session = true;
                    entry.discovery_started_at = Some(Instant::now());
                }
            }
            let _ = responder.send(result);
        }
        BtCommand::StopDiscovery { adapter, responder } => {
            let result = self.bluez.stop_discovery(&adapter).await;
            // Clear regardless of BlueZ call outcome — if the call failed,
            // our session is already inconsistent with BlueZ and clearing
            // the flag avoids double-counting.
            if let Some(entry) = self.adapter_by_bluez_path_mut(&adapter) {
                entry.nexus_has_discovery_session = false;
                entry.discovery_started_at = None;
            }
            let _ = responder.send(result);
        }

        // -- Agent commands --

        BtCommand::LookupPairingJob { device_path, responder } => {
            let job_id = self.device_by_path(&device_path)
                .and_then(|e| e.pairing_job);
            let _ = responder.send(job_id);
        }
        BtCommand::LookupAuthorizationPolicy { device_path, service_uuid, responder } => {
            // Defaults to Prompt if the device (or its profile) is
            // absent. The Agent falls through to the normal prompt
            // path in that case.
            let decision = match self.device_by_path(&device_path) {
                Some(entry) => match &entry.profile {
                    None => AuthorizationDecision::Prompt,
                    Some(p) if !p.auto_accept_incoming => AuthorizationDecision::Prompt,
                    Some(p) => match &service_uuid {
                        None => AuthorizationDecision::Accept,
                        Some(_) if p.authorized_services.is_empty()
                            => AuthorizationDecision::Accept,
                        Some(uuid) if p.authorized_services.iter()
                            .any(|s| s.eq_ignore_ascii_case(uuid))
                            => AuthorizationDecision::Accept,
                        Some(_) => AuthorizationDecision::Prompt,
                    },
                },
                None => AuthorizationDecision::Prompt,
            };
            let _ = responder.send(decision);
        }
        BtCommand::RegisterPromptOneshot { job_id, sender, kind, data } => {
            // Verify the job_id still corresponds to an in-flight
            // pairing. If the pair has already completed (race between
            // BlueZ's Agent callback and the driver task emitting
            // BtPairingComplete), just drop the sender — the Agent's
            // Receiver.await will error with "Sender dropped," its
            // method returns an error to BlueZ, which is fine.
            let job_still_live = self.adapters.values()
                .flat_map(|a| a.devices.values())
                .any(|e| e.pairing_job == Some(job_id));
            if job_still_live {
                self.pending_prompt_answers.insert(job_id, sender);
                // Emit the prompt event so the D-Bus layer surfaces
                // it to operator UI via OperatorNotification.
                let _ = self.event_tx.send(NexusEvent::BtPairingPrompt {
                    job_id,
                    kind,
                    data: data.clone(),
                });
                // Also emit the operator-facing notification directly.
                let _ = self.event_tx.send(NexusEvent::OperatorNotification {
                    kind: "bluetooth_pairing_prompt".to_string(),
                    data: build_prompt_notification(job_id, kind, data),
                });
            }
            // If the job isn't live, the sender drops here and the
            // Agent's receiver errors out. No explicit responder on
            // this command — the Agent's oneshot receiver is the
            // effective responder.
        }
    }
}

/// Start pairing. Transitions the device to Pairing, emits
/// BtPairingStarted, and spawns a driver task that awaits
/// BluezClient::pair(). Returns the job_id immediately.
///
/// Concurrency model: BluezClient::pair can block for tens of seconds
/// (user response time). Running it on the main task would freeze
/// command processing, which in turn would break the Agent coordination
/// path (see §8.2). Spawning a driver task keeps the main loop
/// responsive; the pair outcome arrives asynchronously as
/// NexusEvent::BtPairingComplete, which on_pairing_complete handles.
async fn start_pairing(&mut self, device_path: &str) -> Result<PairingJobId> {
    let job_id = PairingJobId(Ulid::new());

    if let Some(entry) = self.device_by_path_mut(device_path) {
        if matches!(entry.state, BtDeviceState::Pairing { .. }) {
            return Err(BtError::AlreadyPairing);
        }
        entry.state = BtDeviceState::Pairing {
            job_id,
            started_at: Instant::now(),
        };
        entry.pairing_job = Some(job_id);
    } else {
        return Err(BtError::UnknownDevice(device_path.to_string()));
    }

    let _ = self.event_tx.send(NexusEvent::BtPairingStarted {
        job_id,
        device: device_path.to_string(),
    });

    // Spawn the driver. Arc<dyn BluezClient> lets the spawned task
    // call pair() without a borrow-checker fight; the trait's &self
    // signatures make this safe.
    let bluez = Arc::clone(&self.bluez);
    let event_tx = self.event_tx.clone();
    let device_path_owned = device_path.to_string();
    tokio::spawn(async move {
        let outcome = bluez.pair(&device_path_owned).await;
        let (success, reason) = match &outcome {
            Ok(()) => (true, None),
            Err(e) => (false, Some(classify_pair_error(e))),
        };
        let _ = event_tx.send(NexusEvent::BtPairingComplete {
            job_id,
            success,
            reason,
        });
    });

    Ok(job_id)
}

/// Finalize a pairing on BtPairingComplete. Called from handle_event.
/// Separate from start_pairing because the outcome arrives
/// asynchronously via the event bus, not as a direct return value.
async fn on_pairing_complete(
    &mut self,
    job_id: PairingJobId,
    success: bool,
    reason: Option<BtFailureReason>,
) -> Result<()> {
    // Find the device whose pairing_job matches.
    let device_path = self.adapters.values()
        .find_map(|adapter| {
            adapter.devices.iter()
                .find(|(_, e)| e.pairing_job == Some(job_id))
                .map(|(path, _)| path.clone())
        });

    let Some(device_path) = device_path else {
        // Pairing job for a device we no longer know about (removed
        // mid-pair). Drop the pending prompt entry and move on.
        self.pending_prompt_answers.remove(&job_id);
        return Ok(());
    };

    if let Some(entry) = self.device_by_path_mut(&device_path) {
        entry.state = if success {
            BtDeviceState::Paired
        } else {
            BtDeviceState::Failed {
                reason: reason.unwrap_or(BtFailureReason::Unknown("".into())),
                at: Instant::now(),
            }
        };
        entry.pairing_job = None;
    }

    // Drop any still-pending prompt entry for this job.
    self.pending_prompt_answers.remove(&job_id);

    // Persist a profile on success.
    if success {
        if let Some(entry) = self.device_by_path(&device_path) {
            // Resolve the adapter MAC from the registry. The adapter
            // entry is keyed by ifindex; we look it up by matching
            // bluez_path on the device entry.
            let adapter_mac = self.adapters.values()
                .find(|a| a.bluez_path == entry.info.adapter)
                .map(|a| MacAddr(a.info.mac))
                .unwrap_or(MacAddr([0; 6]));   // should always be present
            let profile = BluetoothProfile::from_paired_device(
                &entry.info,
                adapter_mac,
            );
            self.profile_store.put_bluetooth(&profile).await?;

            // Apply the profile's trusted flag to BlueZ itself. Without
            // this, BlueZ's own Trusted property stays false and the
            // Agent is re-invoked on every subsequent connection
            // attempt, even though Nexus thinks the device is trusted.
            if profile.trusted {
                if let Err(e) = self.bluez.set_trusted(&device_path, true).await {
                    warn!(
                        error = ?e,
                        device = %device_path,
                        "failed to set Trusted=true on BlueZ after pair",
                    );
                }
            }
        }
    }

    Ok(())
}

// --- Helpers used throughout ---

/// Forget a device: remove BlueZ's bond, drop our profile, mark the
/// state Removed. The device entry stays in the registry just long
/// enough to emit a final BtDeviceDisconnected if it was connected;
/// it will disappear from the registry on the subsequent
/// InterfacesRemoved from BlueZ.
async fn forget_device(
    &mut self,
    adapter: &str,
    device_path: &str,
) -> Result<()> {
    // Capture the profile id (if any) before mutating BlueZ — we want
    // to remove the profile even if BlueZ's RemoveDevice fails.
    let profile_id = self.device_by_path(device_path)
        .and_then(|e| e.profile.as_ref().map(|p| p.id));

    let bluez_result = self.bluez.forget_device(adapter, device_path).await;

    if let Some(id) = profile_id {
        if let Err(e) = self.profile_store.remove_bluetooth(&id).await {
            warn!(error = ?e, "failed to remove bluetooth profile during forget");
        }
    }

    if let Some(entry) = self.device_by_path_mut(device_path) {
        entry.state = BtDeviceState::Removed;
    }

    bluez_result
}

fn adapter_by_bluez_path_mut(&mut self, bluez_path: &str) -> Option<&mut BtAdapterEntry> {
    self.adapters
        .values_mut()
        .find(|e| e.bluez_path == bluez_path)
}

fn adapter_by_bluez_path(&self, bluez_path: &str) -> Option<&BtAdapterEntry> {
    self.adapters.values().find(|e| e.bluez_path == bluez_path)
}

fn device_by_path_mut(&mut self, device_path: &str) -> Option<&mut BtDeviceEntry> {
    self.adapters
        .values_mut()
        .find_map(|a| a.devices.get_mut(device_path))
}

fn device_by_path(&self, device_path: &str) -> Option<&BtDeviceEntry> {
    self.adapters
        .values()
        .find_map(|a| a.devices.get(device_path))
}

fn device_by_address_mut(
    &mut self,
    adapter_bluez_path: &str,
    address: &MacAddr,
) -> Option<&mut BtDeviceEntry> {
    let adapter = self.adapter_by_bluez_path_mut(adapter_bluez_path)?;
    adapter.devices.values_mut().find(|d| &d.info.address == address)
}
```

The backend's main task multiplexes between the event-bus subscription and the command channel:

```rust
/// Main task loop. Owns the backend struct; processes events and
/// commands serially. Runs until shutdown.
async fn run(mut self) {
    let mut reconcile_tick = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            biased;

            // Handle inbound commands first (operator-driven) so UI
            // responsiveness doesn't starve behind event-bus traffic.
            Some(cmd) = self.cmd_rx.recv() => {
                self.handle_command(cmd).await;
            }
            Ok(event) = self.event_rx.recv() => {
                if let Err(e) = self.handle_event(event).await {
                    warn!(error = ?e, "handle_event failed");
                }
            }
            _ = reconcile_tick.tick() => {
                self.reconcile().await;
            }
            else => break,  // all inputs closed
        }
    }
}
```

**Agent callbacks, in detail.** When BlueZ invokes a method on `/fi/nexus/bluez_agent` (e.g., `RequestConfirmation`), the zbus handler for that method:

1. Looks up the in-flight pairing job for the target device.
2. Creates a `oneshot::channel<PairingAnswer>`, stores the `Sender` in `pending_prompt_answers[job_id]`.
3. Emits `NexusEvent::BtPairingPrompt` with the job_id and prompt details (done by sending a command on the backend's command channel, since the Agent runs in a separate zbus task).
4. Awaits the `Receiver`. Returns the answer (translated to the appropriate D-Bus return type) to BlueZ.

If the operator never answers, the Agent method times out after `pairing_timeout_s` — both the zbus method returns a timeout error to BlueZ and the backend cleans up the pending entry.

### 7.3 Supervisor and Supporting Helpers

The reconcile tick handles BlueZ reconnection and outage notification. While connected, it also re-reads each known adapter's `Powered`/`Discovering`/`Address` properties directly:

- `Powered`/`Discovering` re-emit as `BtAdapterChanged` unconditionally (a repeat of the cached value is a harmless no-op downstream) — a self-healing backstop for the case where the initial `ObjectManager` snapshot or a `PropertiesChanged` signal was missed (e.g. a boot-time race between `nexusd` and `bluetoothd` starting in the same instant), so the cache converges within one reconcile interval instead of requiring a restart.
- `Address` is diffed against the cached value first, and a real difference emits `NexusEvent::MacChanged` — the same correction path the udev `change`-event handling in `nexus-interface-monitor` already feeds. This is where BlueZ's `Adapter1.Address` earns "authoritative": on UART/serdev-attached controllers the kernel never exposes a sysfs address at all, so this reconcile-driven read is the *only* path that ever corrects it, not just a race backstop.

```rust
/// Periodic tick at 1 Hz. Polls BlueZ connectivity and handles the
/// outage-notification threshold. On first-connect failure, subsequent
/// ticks retry with bounded exponential backoff.
async fn reconcile(&mut self) {
    if self.bluez.is_connected() {
        // Clear outage tracking and emit a subsystem_recovered
        // notification if we were previously notifying an outage.
        if self.first_bluez_outage_at.is_some() && self.outage_notified {
            self.emit_notification_event(
                "subsystem_recovered",
                &[("subsystem", "bluez")],
            );
        }
        self.first_bluez_outage_at = None;
        self.outage_notified = false;
        self.reconnect_attempts = 0;
        self.refresh_adapter_properties().await;  // self-healing backstop, §7.3
        return;
    }

    // Backoff: 1s, 2s, 4s, 8s, 16s, cap 30s.
    let backoff = std::cmp::min(
        Duration::from_secs(1 << self.reconnect_attempts.min(5)),
        Duration::from_secs(30),
    );
    if self.last_reconnect_attempt
        .map_or(true, |t| t.elapsed() >= backoff)
    {
        self.last_reconnect_attempt = Some(Instant::now());
        self.reconnect_attempts += 1;
        let _ = self.bluez.connect().await;  // emits BluezConnected on success
    }

    // Prolonged-outage notification.
    if let Some(t) = self.first_bluez_outage_at {
        if t.elapsed().as_secs() >= self.config.bluez_outage_notify_s as u64
            && !self.outage_notified
        {
            self.emit_notification_event(
                "subsystem_unavailable",
                &[
                    ("subsystem", "bluez"),
                    ("duration_s", &t.elapsed().as_secs().to_string()),
                ],
            );
            self.outage_notified = true;
        }
    }
}
```

The supporting helpers are straightforward:

```rust
/// Map a BluezClient error into a structured failure reason for
/// Failed / BtPairingComplete.
///
/// Fragility: this matches substrings of BlueZ's D-Bus error names
/// (e.g., "org.bluez.Error.AuthenticationFailed"). BlueZ's error-name
/// surface is stable across 5.x, but a BlueZ 6.x rework could rename
/// any of these. The match order matters: more-specific names first.
/// Unmapped BlueZ errors fall through to BtFailureReason::Unknown with
/// the raw message preserved, so an unknown error surfaces at the
/// D-Bus layer as a readable string rather than being silently
/// classified as something generic. Test coverage should include each
/// mapped error string (integration tests against mock BlueZ, plus a
/// fuzz-style test that generates random error names and verifies the
/// fallback doesn't panic).
fn classify_pair_error(e: &BtError) -> BtFailureReason {
    match e {
        BtError::Bluez(msg) if msg.contains("AuthenticationFailed") =>
            BtFailureReason::PairingAuthFailed,
        BtError::Bluez(msg) if msg.contains("AuthenticationRejected") =>
            BtFailureReason::PairingRejected,
        BtError::Bluez(msg) if msg.contains("AuthenticationTimeout") =>
            BtFailureReason::PairingTimeout,
        BtError::Bluez(msg) if msg.contains("ConnectionAttemptFailed") =>
            BtFailureReason::ConnectionFailed,
        BtError::Bluez(msg) => BtFailureReason::Unknown(msg.clone()),
        other => BtFailureReason::Unknown(format!("{other:?}")),
    }
}

/// Translate a pairing prompt into the NotificationEvent.data dict
/// used by DD-006 §5.3. Keys are human-readable; the D-Bus layer
/// emits these as variants.
fn build_prompt_notification(
    job_id: PairingJobId,
    kind: PairingPromptKind,
    data: PairingPromptData,
) -> NotificationData {
    let mut out = NotificationData::new();
    out.insert("job_id".into(), job_id.0.to_string());
    out.insert("device_path".into(), data.device_path);
    out.insert("kind".into(), format!("{kind:?}").to_lowercase());
    if let Some(pk) = data.passkey {
        out.insert("passkey".into(), format!("{pk:06}"));
    }
    if let Some(uuid) = data.service_uuid {
        out.insert("service_uuid".into(), uuid);
    }
    out
}

impl BluetoothProfile {
    /// Construct a fresh profile from a newly-paired device's info
    /// and the adapter MAC it was bonded with.
    /// Profile defaults: auto_connect = true, trusted = true (the
    /// operator just explicitly paired it), auto_accept_incoming =
    /// false (requires a separate operator decision), no service
    /// authorization filter.
    pub fn from_paired_device(info: &BtDeviceInfo, adapter_address: MacAddr) -> Self {
        BluetoothProfile {
            id: Ulid::new(),
            schema_version: 1,
            metadata: ProfileMetadata::now(),
            address: info.address,
            adapter_address,
            alias: info.alias.clone().or_else(|| info.name.clone()),
            auto_connect: true,
            trusted: true,
            auto_accept_incoming: false,
            authorized_services: Vec::new(),
        }
    }
}

impl BluetoothBackend {
    /// Emit an OperatorNotification with a dict built from string pairs.
    /// Convenience over hand-constructing NotificationData every time
    /// — keeps the call sites (subsystem_unavailable, bluetooth_pairing_prompt,
    /// etc.) one-liners.
    fn emit_notification_event(&self, kind: &str, fields: &[(&str, &str)]) {
        let mut data = NotificationData::new();
        for (k, v) in fields {
            data.insert(k.to_string(), v.to_string());
        }
        let _ = self.event_tx.send(NexusEvent::OperatorNotification {
            kind: kind.to_string(),
            data,
        });
    }
}
```

The `BluezClient` trait object is stored as `Arc<dyn BluezClient>` in the backend (shown as `Box<dyn BluezClient>` in the struct sketch for brevity — actual impl uses `Arc` so it can be cheaply cloned into spawned tasks like the pair driver). `clone_arc()` is `Arc::clone(&self.bluez)`.

Errors:

```rust
#[derive(thiserror::Error, Debug)]
pub enum BtError {
    #[error("BlueZ D-Bus error: {0}")]
    Bluez(String),
    #[error("BlueZ D-Bus not connected")]
    NotConnected,
    #[error("unknown device: {0}")]
    UnknownDevice(String),
    #[error("device already has a pairing in flight")]
    AlreadyPairing,
    #[error("unknown pairing job: {0:?}")]
    UnknownPairingJob(PairingJobId),
    #[error("pairing job gone before answer arrived")]
    PairingJobGone,
    #[error("agent capability conflict (another agent is registered)")]
    AgentConflict,
    #[error("operation on powered-off adapter: {0}")]
    AdapterNotPowered(String),
    /// Wraps the Profile Store error type defined in DD-007. That
    /// error type is not yet declared in DD-007's draft; when it is,
    /// this variant should derive From<> via #[from].
    #[error("profile store error: {0}")]
    ProfileStore(String),
}
```

`NotificationData` is defined in `nexus-core` (see architecture doc §6) as a typed dict with variant values (string, u32, u64, bool, ObjectPath) that map cleanly to D-Bus `a{sv}` when the D-Bus layer emits `fi.nexus.Manager.NotificationEvent`.

Emission helper (method on `BluetoothBackend`):

```rust
/// Emit an OperatorNotification on the event bus. The D-Bus layer
/// subscribes and translates into fi.nexus.Manager.NotificationEvent.
fn emit_notification_event(&self, kind: &str, fields: &[(&str, &str)]) {
    let mut data = NotificationData::new();
    for (k, v) in fields {
        data.insert(*k, NotificationValue::String(v.to_string()));
    }
    let _ = self.event_tx.send(NexusEvent::OperatorNotification {
        kind: kind.to_string(),
        data,
    });
}
```

**Adapter and device property changes** from BlueZ's `PropertiesChanged` come in as refined event variants (`NexusEvent::BtDeviceConnected`, etc., plus new variants `BtDevicePropertiesChanged` for finer-grained updates — see §13.2). The backend applies them by looking up the entry, mutating state, and letting the D-Bus layer pick up the property update via its own event subscription.

**Agent callback routing** lives in §8.

---

## 8. Pairing and Bonding

### 8.1 The Agent

BlueZ's pairing model requires exactly one registered `org.bluez.Agent1` per system, which BlueZ calls with a small set of methods when a pairing operation needs input:

- `RequestPinCode(device)` — legacy PIN entry (returns a string).
- `DisplayPinCode(device, pincode)` — show this PIN to the user.
- `RequestPasskey(device)` — user enters a 6-digit passkey.
- `DisplayPasskey(device, passkey, entered)` — show a 6-digit passkey.
- `RequestConfirmation(device, passkey)` — user confirms the displayed passkey matches (numeric comparison).
- `RequestAuthorization(device)` — permission for an incoming connection.
- `AuthorizeService(device, uuid)` — permission for a specific service UUID.
- `Cancel()` — BlueZ giving up on the pairing.

Nexus registers a `fi.nexus.BluezAgent` at object path `/fi/nexus/bluez_agent` during startup. The registration runs from `spawn_bluetooth_backend` (the `lib.rs` entry point), after the `BluezClient` has connected but before the main `run()` loop starts:

```rust
// In spawn_bluetooth_backend, pseudocode:
//   let mut backend = BluetoothBackend::new(..., zbus_connection.clone()).await?;
//   if backend.config.register_agent {
//       backend.register_agent().await?;
//   }
//   tokio::spawn(backend.run());

async fn register_agent(&mut self) -> Result<()> {
    let agent = Agent::new(self.event_tx.clone());
    self.zbus_connection
        .object_server()
        .at("/fi/nexus/bluez_agent", agent)
        .await?;

    let agent_mgr: AgentManager1Proxy = AgentManager1Proxy::new(&self.zbus_connection).await?;

    match agent_mgr.register_agent("/fi/nexus/bluez_agent", "KeyboardDisplay").await {
        Ok(()) => {
            agent_mgr.request_default_agent("/fi/nexus/bluez_agent").await?;
            info!("Nexus registered as BlueZ pairing agent");
        }
        Err(zbus::Error::MethodError(name, _, _))
            if name.as_str() == "org.bluez.Error.AlreadyExists" =>
        {
            // Another process (bluetoothctl, blueman, a DE) already
            // holds the agent. This is common on desktop systems and
            // not an error. Log and continue — Nexus-originated
            // pairings still work if the other agent co-operates, but
            // PIN/passkey prompts will go through the existing agent's
            // UI rather than Nexus's D-Bus surface. Embedded deployments
            // that need Nexus to own pairing should configure their
            // system to disable competing agents.
            warn!(
                "Another process has already registered as BlueZ agent; \
                 Nexus pairings will use the existing agent's prompts"
            );
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
```

The capability `"KeyboardDisplay"` covers all SSP methods. It means: "I can both display a passkey and capture one from the user." Other valid values are `"DisplayOnly"`, `"DisplayYesNo"`, `"KeyboardOnly"`, `"NoInputNoOutput"` — BlueZ picks the weakest-common capability for SSP. Nexus uses `"KeyboardDisplay"` because the actual I/O happens over D-Bus (the operator UI has whatever capabilities it has); claiming the most capable reduces the chance of falling back to a less secure pairing mode.

**Agent method skeleton.** Each BlueZ Agent method follows the same shape — look up the job, deposit a oneshot, await with timeout, return the answer (or a D-Bus error on timeout/cancel):

```rust
#[zbus::interface(name = "org.bluez.Agent1")]
impl BluezAgent {
    async fn request_confirmation(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
        passkey: u32,
    ) -> zbus::fdo::Result<()> {
        let device_path = device.to_string();

        // Ask the backend which job this callback belongs to.
        let (tx_job, rx_job) = oneshot::channel();
        self.cmd_tx.send(BtCommand::LookupPairingJob {
            device_path: device_path.clone(),
            responder: tx_job,
        }).await.map_err(|_| zbus::fdo::Error::Failed("backend gone".into()))?;
        let job_id = rx_job.await
            .map_err(|_| zbus::fdo::Error::Failed("lookup dropped".into()))?
            .ok_or_else(|| zbus::fdo::Error::Failed(
                "no in-flight pairing for device".into()
            ))?;

        // Register the oneshot with the backend and include the prompt
        // details so the backend can emit BtPairingPrompt atomically.
        let (tx_answer, rx_answer) = oneshot::channel();
        self.cmd_tx.send(BtCommand::RegisterPromptOneshot {
            job_id,
            sender: tx_answer,
            kind: PairingPromptKind::RequestConfirmation,
            data: PairingPromptData {
                device_path,
                passkey: Some(passkey),
                service_uuid: None,
                pincode: None,
            },
        }).await.map_err(|_| zbus::fdo::Error::Failed("backend gone".into()))?;

        // Await the operator's answer, bounded by the agent timeout.
        // If it fires, the backend's on_pairing_complete path still
        // cleans up pending_prompt_answers; see §7.2.
        let answer = tokio::time::timeout(
            Duration::from_secs(self.agent_response_timeout_s),
            rx_answer,
        )
            .await
            .map_err(|_| zbus::fdo::Error::Failed("operator response timed out".into()))?
            .map_err(|_| zbus::fdo::Error::Failed("oneshot dropped".into()))?;

        match answer {
            PairingAnswer::Accept(true) => Ok(()),
            PairingAnswer::Accept(false) | PairingAnswer::Cancel => {
                // org.bluez.Error.Rejected maps to a failed pair attempt
                // at BlueZ's level.
                Err(zbus::fdo::Error::Failed("rejected".into()))
            }
            _ => Err(zbus::fdo::Error::Failed("wrong answer variant".into())),
        }
    }

    // request_pin_code, request_passkey, display_passkey, display_pin_code
    // follow the same shape as request_confirmation with the
    // PairingPromptKind and return-type variants that match BlueZ's
    // agent-api.txt spec.

    /// BlueZ invoked RequestAuthorization — the peer is trying to
    /// reconnect to a paired device without a pending pairing (a
    /// HID keyboard waking up, a phone coming back into range).
    ///
    /// Unlike the confirmation callbacks above, this one is not tied
    /// to a pairing job — the device is already bonded. The Agent
    /// consults the backend's profile via LookupAuthorizationPolicy
    /// before deciding whether to prompt the operator. This is where
    /// the profile's `auto_accept_incoming` flag is actually applied.
    async fn request_authorization(
        &self,
        device: zvariant::ObjectPath<'_>,
    ) -> zbus::fdo::Result<()> {
        let device_path = device.to_string();

        // Ask the backend what to do.
        let (tx_policy, rx_policy) = oneshot::channel();
        self.cmd_tx.send(BtCommand::LookupAuthorizationPolicy {
            device_path: device_path.clone(),
            service_uuid: None,
            responder: tx_policy,
        }).await.map_err(|_| zbus::fdo::Error::Failed("backend gone".into()))?;
        let decision = rx_policy.await
            .map_err(|_| zbus::fdo::Error::Failed("policy lookup dropped".into()))?;

        match decision {
            AuthorizationDecision::Accept => return Ok(()),
            AuthorizationDecision::Reject => {
                return Err(zbus::fdo::Error::Failed("rejected by policy".into()));
            }
            AuthorizationDecision::Prompt => {
                // Fall through to the prompt flow. For incoming
                // authorization outside a pairing, we synthesize a
                // new PairingJobId — the operator sees a prompt for
                // an authorization decision, and AnswerPairingPrompt
                // resolves it the same way as a pairing prompt.
            }
        }

        let job_id = PairingJobId(Ulid::new());
        let (tx_answer, rx_answer) = oneshot::channel();
        self.cmd_tx.send(BtCommand::RegisterPromptOneshot {
            job_id,
            sender: tx_answer,
            kind: PairingPromptKind::RequestAuthorization,
            data: PairingPromptData {
                device_path,
                passkey: None,
                pincode: None,
                service_uuid: None,
            },
        }).await.map_err(|_| zbus::fdo::Error::Failed("backend gone".into()))?;

        let answer = tokio::time::timeout(
            Duration::from_secs(self.agent_response_timeout_s),
            rx_answer,
        )
            .await
            .map_err(|_| zbus::fdo::Error::Failed("operator response timed out".into()))?
            .map_err(|_| zbus::fdo::Error::Failed("oneshot dropped".into()))?;

        match answer {
            PairingAnswer::Accept(true) => Ok(()),
            _ => Err(zbus::fdo::Error::Failed("rejected".into())),
        }
    }

    /// BlueZ invoked AuthorizeService — same shape as
    /// request_authorization, but scoped to a specific service UUID.
    /// The backend's policy-lookup also checks the profile's
    /// `authorized_services` list.
    async fn authorize_service(
        &self,
        device: zvariant::ObjectPath<'_>,
        uuid: &str,
    ) -> zbus::fdo::Result<()> {
        // Identical to request_authorization except the
        // service_uuid is populated in both LookupAuthorizationPolicy
        // and (on prompt) PairingPromptData. Implementation elided.
        todo!()
    }
}
```

Agent-side timeout is distinct from BlueZ's own `pairing_timeout_s`. The agent returns an error (via the `tokio::time::timeout` wrapper) slightly before BlueZ would time out its Pair() call, so the error attributed to the pairing is `PairingTimeout` rather than a generic BlueZ failure. Metrics: the agent's timeout path increments `nexus_bluetooth_pairings_total{outcome="timeout"}` even though BlueZ itself didn't time out — otherwise operator-unresponsive timeouts would be invisible in the metric (B9).

### 8.2 Pairing Flow

Four tokio tasks coordinate through the command channel and the event bus:

- **Backend main loop** — owns `pending_prompt_answers` and the device state machine. Processes commands and events serially.
- **D-Bus method handler** (`fi.nexus.Bluetooth.Pair`) — runs in a zbus server task. Converts the operator call into `BtCommand::Pair`, awaits the response.
- **Pair driver** — spawned per pairing by `start_pairing`. Owns `BluezClient::pair()`'s await; emits `BtPairingComplete` when done.
- **Agent** — separate zbus server task for `/fi/nexus/bluez_agent`. BlueZ invokes its methods during `Pair()`; it coordinates with the backend via `BtCommand::LookupPairingJob` and `BtCommand::RegisterPromptOneshot`.

Happy path, chronological:

```
     operator                                              BlueZ
        │                                                   │
        │ fi.nexus.Bluetooth.Pair(device)                   │
        ▼                                                   │
 ┌──────────────────┐                                       │
 │ D-Bus handler    │── BtCommand::Pair ───────────────────►│
 └──────────────────┘                                       │
                                                            ▼
                                                  ┌──────────────────┐
                                                  │ backend main loop│
                                                  │  • allocate id   │
                                                  │  • set Pairing   │
                                                  │  • emit          │
                                                  │    BtPairingStart│
                                                  │  • spawn driver  │
                                                  └──────────────────┘
                                                            │
                            ┌───────────────────────────────┤
                            ▼                               │
                   ┌──────────────────┐                     │
                   │ pair driver task │                     │
                   │ BluezClient.pair │──────► BlueZ ◄──────┤  (main loop
                   │ (awaiting)       │        Pair()       │   continues
                   └──────────────────┘                     │   handling
                            │                               │   commands)
                            │                               │
                            │ BlueZ invokes Agent (e.g.     │
                            │ RequestConfirmation)          │
                            │                               ▼
                            │            ┌────────────────────────────┐
                            │            │ Agent zbus task (sep task) │
                            │            │  • BtCommand::LookupPairing│
                            │            │    Job → main loop         │
                            │            │  • creates oneshot         │
                            │            │  • BtCommand::RegisterPrompt│
                            │            │    Oneshot → main loop     │
                            │            │  • awaits Receiver         │
                            │            └────────────────────────────┘
                            │                               │
                            │            ┌──────────────────┤
                            │            ▼                  │
                            │   operator sees prompt,       │
                            │   calls fi.nexus.Bluetooth.   │
                            │   AnswerPairingPrompt         │
                            │            │                  │
                            │            ▼                  │
                            │  ┌──────────────────┐         │
                            │  │ D-Bus handler    │         │
                            │  │ BtCommand::      │────────►│
                            │  │ AnswerPairingPrompt        │
                            │  └──────────────────┘         │
                            │                               │
                            │ (main loop removes the oneshot
                            │  and sends answer; Agent's await
                            │  resolves; Agent returns to BlueZ)
                            │
                            │ BlueZ.pair() returns Ok
                            ▼
                 pair driver emits
                 NexusEvent::BtPairingComplete
                            │
                            ▼
                 ┌──────────────────┐
                 │ backend main loop│
                 │  on_pairing_     │
                 │  complete()      │
                 │  • state=Paired  │
                 │  • put_bluetooth │
                 │  • set_trusted   │
                 └──────────────────┘
```

Failure paths: if the operator calls `AnswerPairingPrompt` with `PairingAnswer::Cancel`, or if the Agent's oneshot times out (§13.1), the Agent returns an error to BlueZ, `Pair()` returns an error, the driver task emits `BtPairingComplete { success: false, reason }`, and `on_pairing_complete` moves the device to `Failed`.

### 8.2.1 Correlation

The `PairingJobId` links four events / commands across tasks:

- **`BtPairingStarted`** — emitted by the backend when `Pair` command is handled.
- **`BtCommand::LookupPairingJob`** — Agent asks the backend for the device's current job_id.
- **`BtCommand::RegisterPromptOneshot`** — Agent deposits a oneshot Sender for the job_id.
- **`BtCommand::AnswerPairingPrompt`** / **`BtPairingPrompt`** — operator response referenced against the job_id.
- **`BtPairingComplete`** — pair driver emits with the same job_id.

D-Bus clients use the id to correlate prompts to operations they know about. A client that never called `Pair()` won't recognize any `BtPairingPrompt` job id, so it knows to ignore (or surface as "another process is pairing") rather than act.

### 8.3 Bond Storage

BlueZ stores bonding material (link keys, IRKs) on disk under `/var/lib/bluetooth/<adapter-addr>/<device-addr>/info`. This is BlueZ's concern, not Nexus's. What Nexus stores is metadata about the bond, in the Profile Store:

```rust
/// Per-device Bluetooth profile. Persists preferences that Nexus
/// applies when a device is discovered or connects.
#[derive(Debug, Clone)]
pub struct BluetoothProfile {
    pub id: Ulid,
    pub schema_version: u32,
    pub metadata: ProfileMetadata,

    /// Device Bluetooth address. Used as the stable identifier for
    /// matching this profile to a BlueZ device object when the
    /// backend sees InterfacesAdded.
    pub address: MacAddr,

    /// The adapter address this bond was originally established with.
    /// Bonds are tied to a specific adapter — if the adapter is
    /// replaced (USB dongle swapped), bonds must be re-established.
    /// Recorded so operator-facing tools can present "this profile
    /// belongs to adapter X, which is not currently connected."
    ///
    /// Note: this is a best-effort identifier. If the operator swaps
    /// a USB dongle and the replacement happens to take the same
    /// `hciN` slot, Nexus will still see profiles tagged with the old
    /// adapter address even though the new adapter has a different one.
    /// This shows up as a `NotificationData { kind: "bt_adapter_mismatch" }`
    /// warning, not a fatal error — the profile's metadata is stale
    /// but not wrong, and re-pairing produces a fresh profile with
    /// the new adapter's address.
    pub adapter_address: MacAddr,

    /// Friendly name for UI. Populated from BlueZ's `Name` at pairing
    /// time; may be updated if the peer's GAP changes.
    pub alias: Option<String>,

    /// Whether to auto-reconnect on discovery / after reboot.
    pub auto_connect: bool,

    /// Maps to BlueZ's Trusted property. Trusted devices don't require
    /// per-connection authorization prompts.
    pub trusted: bool,

    /// Whether to accept incoming connections from this device without
    /// prompting the operator. Default false; set true for keyboards,
    /// mice, and similar devices once pairing is done.
    pub auto_accept_incoming: bool,

    /// Service UUIDs the operator has marked as authorized for auto-
    /// connection. Empty means "all services advertised by the device
    /// may be connected."
    pub authorized_services: Vec<String>,
}
```

No credential fields (link keys live in BlueZ's disk store, not here), so no `SecretString` / dual-struct pattern; profile encryption is still opt-in-enabled per DD-007 but the ciphertext covers only metadata — an attacker with the profile store and without `/var/lib/bluetooth` cannot impersonate a bonded device.

Profile lifecycle:

- **Created** on successful pairing (backend calls `put_bluetooth` inside the Pair flow after the `Paired` transition).
- **Updated** when operator-driven properties change (alias rename, auto-connect toggle).
- **Removed** on `Forget`. Forget also calls BlueZ's `RemoveDevice` so the link key goes with it.

ProfileStore integration, added to DD-007:

```rust
async fn load_bluetooth(&self) -> Result<Vec<BluetoothProfile>>;

/// Linear scan, similar to load_gnss_profile_by_path in DD-005 §9.
/// Bluetooth addresses are small enough that this is not a concern
/// (embedded systems rarely have more than 20-30 bonded devices).
async fn load_bluetooth_profile_by_address(&self, address: &MacAddr)
    -> Result<Option<BluetoothProfile>>;

async fn put_bluetooth(&self, profile: &BluetoothProfile) -> Result<()>;

async fn remove_bluetooth(&self, id: &Ulid) -> Result<()>;
```

And a corresponding `ProfileRef` variant in DD-007 §5.1:

```rust
pub enum ProfileRef<'a> {
    Ethernet { ifname: &'a str },
    Wifi { ssid_hash: &'a str },
    Gnss { id: &'a Ulid },
    Bluetooth { id: &'a Ulid },
}
```

---

## 9. Discovery

### 9.1 Discovery Sessions

BlueZ's discovery model is session-based: a client calls `StartDiscovery`, and BlueZ keeps the adapter discovering as long as at least one client has an outstanding session. When all clients have called `StopDiscovery` (or crashed), BlueZ stops the adapter.

Nexus holds at most one outstanding discovery session per adapter, tracked by an explicit boolean on the adapter entry:

```rust
struct BtAdapterEntry {
    // ... (other fields from §7.2) ...

    /// True when Nexus has an outstanding StartDiscovery call to BlueZ
    /// on this adapter. Set by the StartDiscovery command, cleared by
    /// StopDiscovery. Note: BlueZ's own Discovering property may be true
    /// even when this is false — other clients can run their own
    /// sessions concurrently. See §9.1 for how Nexus observes that.
    nexus_has_discovery_session: bool,

    /// When the Nexus-owned discovery session started, for the
    /// discovery_timeout_s auto-stop (§10).
    discovery_started_at: Option<Instant>,
}
```

Session lifecycle:

- Operator calls `fi.nexus.Bluetooth.StartDiscovery(adapter)` → backend handles `BtCommand::StartDiscovery`, calls `BluezClient::start_discovery`, sets `nexus_has_discovery_session = true`.
- Operator calls `StopDiscovery(adapter)` → `BtCommand::StopDiscovery`, calls `BluezClient::stop_discovery`, clears the flag.
- Reconcile tick observes `discovery_started_at.elapsed() >= discovery_timeout_s` → auto-stops the session (prevents orphan sessions if the operator forgets).
- Nexus process crashes → its D-Bus connection to BlueZ drops → BlueZ notices and stops the session on its own (no explicit cleanup needed from Nexus — the crash case is BlueZ's responsibility).

**Other clients.** If another BlueZ client (bluetoothctl, a desktop UI) also starts discovery, BlueZ's `Discovering` property fires true independently of Nexus's flag. Nexus observes this in `PropertiesChanged` (arriving as `BtAdapterChanged { discovering: true }`) and transitions the adapter state to `Discovering` even though `nexus_has_discovery_session` stays false. `StopDiscovery` from Nexus affects only Nexus's session; the other client's session keeps the adapter discovering. This is standard BlueZ semantics.

### 9.2 Discovery Filters

BlueZ's `SetDiscoveryFilter` narrows results before discovery begins:

```rust
#[derive(Debug, Clone, Default)]
pub struct DiscoveryFilter {
    /// Restrict to a transport. None means BlueZ's default, which is
    /// "auto" (both BR/EDR and LE).
    pub transport: Option<DiscoveryTransport>,

    /// Minimum RSSI (dBm). Peers weaker than this are filtered out
    /// by BlueZ before emitting InterfacesAdded. Useful for
    /// distance-limited discovery (e.g., "pair only with devices
    /// within ~1 meter").
    pub rssi: Option<i16>,

    /// Restrict to devices advertising these service UUIDs. BlueZ's
    /// pattern matching is case-insensitive.
    pub uuids: Vec<String>,

    /// If true, BlueZ emits every advertisement as a separate
    /// InterfacesAdded even if the device is already in the registry.
    /// Default false (duplicates suppressed).
    pub duplicate_data: bool,
}

/// Transport restriction for discovery. Maps directly to BlueZ's
/// Transport string filter: "auto" | "bredr" | "le". Note that
/// BtTransport (§6.2) has a `Dual` variant for describing a device's
/// capabilities, which has no counterpart here — filters can only
/// target one transport or accept both via Auto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiscoveryTransport {
    #[default]
    Auto,
    Bredr,
    Le,
}
```

The filter is applied per `start_discovery` call. Changing the filter mid-session requires stop+start.

---

## 10. Configuration

Global Bluetooth Backend config in `nexus.toml`:

```toml
[bluetooth]
# Whether the Bluetooth Backend is enabled. Set false on devices
# without Bluetooth hardware to skip the backend entirely.
enabled = true

# Whether to register Nexus as the BlueZ pairing Agent at startup.
# If false, Nexus still drives pairings via BluezClient.pair() but
# BlueZ will forward agent callbacks to whatever other process holds
# the agent role. Useful on desktop systems that already run blueman
# or similar.
register_agent = true

# Default pairing timeout. BlueZ's Pair() blocks for up to this long
# before returning a timeout error. Applies to the whole pairing
# exchange, including user response time.
pairing_timeout_s = 60

# Max time the backend will hold a pending Agent oneshot waiting for
# an operator's AnswerPairingPrompt. Typically shorter than
# pairing_timeout_s so the Agent method returns an error to BlueZ
# before BlueZ itself times out (gives cleaner error classification).
agent_response_timeout_s = 45

# Default discovery timeout. StartDiscovery sessions older than this
# are stopped automatically by the backend's periodic tick. 0 means
# no automatic stop (caller must StopDiscovery explicitly).
discovery_timeout_s = 30

# Garbage-collect unpaired, unbonded, unconnected device entries from
# the registry after this many seconds of no observed activity
# (no PropertiesChanged, no rediscovery). Prevents the registry (and
# the associated metric label set) from growing unbounded in
# high-BLE-advertising environments. Paired devices are kept
# indefinitely. 0 disables GC.
discovery_device_ttl_s = 300

# How long an unpaired, unbonded device seen only in discovery is
# kept in the backend's in-memory registry before being garbage-
# collected. Prevents metric cardinality blowup from ephemeral BLE
# beacons. Paired/bonded devices are kept indefinitely regardless
# of this setting.
discovery_device_ttl_s = 300

# Notify operators via D-Bus NotificationEvent after BlueZ is
# unreachable for this many seconds.
bluez_outage_notify_s = 60

# Whether a powered-off adapter should be powered on automatically at
# startup, if a profile exists.
auto_power_on_startup = true

# Default discovery filter applied when an operator calls StartDiscovery
# without specifying one. Profiles may override.
[bluetooth.default_discovery_filter]
transport = "auto"               # "bredr" | "le" | "auto"
rssi = -90                       # discard peers weaker than this
uuids = []                       # no UUID filter
duplicate_data = false
```

---

## 11. Device Profile

See §8.3 for `BluetoothProfile`. Key properties:

- Keyed by ULID on disk (`bluetooth/<ulid>.toml`); D-Bus path uses the ULID.
- Matched to BlueZ devices by `address` field (linear scan).
- Stored adapter-binding: a profile includes the adapter address it was originally bonded with, because BlueZ's link key is adapter-specific. A matching warning is emitted via `NotificationEvent` when a profile is present but the adapter it was bonded to is absent.
- No secrets; the actual bonding material is in BlueZ's disk store.

---

## 12. Power Management

The `PowerState` global from DD-006 §5.1 maps to Bluetooth behavior as follows:

| PowerState | Behavior |
|---|---|
| `active` | Full operation. Adapters auto-power according to `auto_power_on_startup`. Discovery, pairing, and connections proceed normally. |
| `background` | No automatic discovery sessions. Existing connections are maintained. Outgoing Connect() calls triggered by profile `auto_connect = true` still proceed (e.g., when a HID keyboard advertises and matches a stored profile). Incoming connections honored. |
| `sleep` | All adapters powered off (`Powered = false` set on each). Existing connections break. Returning to `active` re-powers adapters and triggers `auto_connect` reconnection for profiled devices. |

Notes:

- Bluetooth radios are among the higher power draws on battery-powered devices (~5-50 mW idle, ~100+ mW during discovery). Powering off in sleep is worthwhile.
- BlueZ's `Powered = false` is soft — it disables radio transmissions but leaves the adapter object present. Wake-on-Bluetooth (if the hardware and BlueZ support it) is NOT configured by Nexus; integrators who need it must configure BlueZ directly.

---

## 13. Error Handling and Observability

### 13.1 Fault Classes

**BlueZ unavailable.** Backend transitions all adapters to `Unavailable`. The reconcile supervisor retries `connect()` with backoff. A `subsystem_unavailable` notification fires after `bluez_outage_notify_s`.

**Adapter disappears mid-operation.** Interface Monitor emits `InterfaceRemoved`; backend force-transitions to `Gone` and tears down in-flight operations for that adapter (pending pairings fail with `PairingInterrupted`; connecting devices fail with `ConnectionFailed`).

**Pairing failure.** Device state → `Failed { reason }`. The `BtPairingComplete { success: false }` event fires, and the backend emits `NotificationEvent { kind: "pairing_failed", data: { device, reason } }`.

**Agent registration conflict.** Logged at warn level (§8.1). Nexus continues operating; pairings fall back to whichever agent BlueZ has registered.

**Peer spoofing its address.** Not detected. BLE random-address rotation is expected and handled via IRK; genuine spoofing (peer lying about a public address it doesn't own) is not a threat Nexus guards against — that's BlueZ's SMP implementation and, ultimately, the Bluetooth security model itself.

**Bond lost on peer.** Detected at connection time: BlueZ returns `org.bluez.Error.AuthenticationFailed`, backend transitions device → `Failed { reason: PairingAuthFailed }`. Operator must `Forget` and re-pair.

**BlueZ restart mid-connection.** The reader detects D-Bus disconnection; backend emits `BluezDisconnected`; on reconnect, ObjectManager republish rebuilds the tree. Previously-connected devices will have `Connected = true` if the underlying ACL link survived the BlueZ restart (rare but possible on brief restarts); otherwise they come back as `Paired` and the operator-configured `auto_connect` flag drives reconnection.

**Commands during the BlueZ-disconnected window.** Between `BluezDisconnected` and the next `BluezConnected`, `BluezClient::is_connected()` returns false and every operational call returns `BtError::NotConnected`. Device entries are cleared on `BluezDisconnected` (line `entry.devices.clear()`), so a D-Bus call like `fi.nexus.BluetoothDevice.Connect()` issued during the window returns `UnknownDevice` even for a device that was valid moments earlier. The window is usually short (BlueZ restarts complete in under a second on typical systems); clients that want graceful behavior during BlueZ upgrades should retry on these errors or observe `BtAdapterChanged` before acting. Nexus does not queue commands to replay after reconnect — the operator's intent may be stale by then, and silent replay is worse than a visible failure.

**Discovery for a device that never appears.** Nothing to recover — just discovery timing. The discovery timeout (default 30 s) stops the session; operator can retry.

**Concurrent Pair and Connect on the same device.** Backend serializes: the device state machine's `Pairing { ... }` guard blocks `Connect()` until pairing completes. Returns `fi.nexus.Error.InvalidState` to the conflicting caller.

**Concurrent pairings on different devices.** Each `Pair()` spawns its own driver task (§7.2); they run in parallel. All state they share — `pending_prompt_answers`, device entries, adapter entries — is owned exclusively by the backend main task and accessed only through commands (`BtCommand::RegisterPromptOneshot`, `BtCommand::AnswerPairingPrompt`). The main task processes commands serially, so there's no data race between two concurrent pairings, even if their `BtPairingComplete` events arrive in the same tokio poll cycle. The only cross-pairing shared resource is the BlueZ adapter itself — BlueZ handles at most one pairing per adapter at a time and serializes requests internally; Nexus does not need to duplicate that gating.

### 13.2 Observability

Metrics:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `nexus_bluetooth_adapters` | gauge | — | Number of registered adapters |
| `nexus_bluetooth_adapter_state` | gauge | `adapter`, `state` | `1` iff the adapter is currently in the labeled state. `adapter` is the BlueZ path (e.g., `/org/bluez/hci0`) |
| `nexus_bluetooth_devices` | gauge | `adapter` | Devices currently known per adapter |
| `nexus_bluetooth_device_state` | gauge | `adapter`, `address`, `state` | `1` iff the device is in the labeled state. See cardinality note below |
| `nexus_bluetooth_discoveries_total` | counter | `adapter` | Discovery sessions started |
| `nexus_bluetooth_pairings_total` | counter | `adapter`, `outcome` | Pairings attempted. `outcome` is `success`, `rejected`, `timeout`, `auth_failed`, `other` |
| `nexus_bluetooth_connections_total` | counter | `adapter`, `outcome` | Connection attempts. `outcome` is `success`, `failed` |
| `nexus_bluetooth_agent_callbacks_total` | counter | `kind` | Agent callbacks received. `kind` is `pin`, `passkey`, `confirmation`, `authorization`, `service` |
| `nexus_bluetooth_bluez_reconnects_total` | counter | — | BlueZ connection attempts |
| `nexus_bluetooth_bluez_connected` | gauge | — | `1` when BlueZ D-Bus connection is alive |

**Label cardinality note.** `address` on `nexus_bluetooth_device_state` can accumulate stale series as transient BLE advertisers come and go — a beacon advertising every 100 ms for a week will create one series forever. The backend mitigates this by garbage-collecting unpaired-unbonded devices from its registry after `discovery_device_ttl_s` (default 300 s) of no observation, which also drops the metric. Paired devices are kept indefinitely. Fleet deployments in high-BLE environments should configure a stricter TTL or omit `address` from aggregation.

**Logs.** Standard `tracing` with `target = "nexus::bluetooth"`. Levels and fields:

| Event | Level | Fields |
|---|---|---|
| Adapter state transition | INFO | `adapter`, `from_state`, `to_state`, `trigger` (`"interface_discovered"`, `"bluez_props"`, `"interface_removed"`) |
| Device state transition | INFO | `adapter`, `address`, `from_state`, `to_state`, `reason` |
| Pairing started | INFO | `job_id`, `adapter`, `address` |
| Pairing prompt fired | INFO | `job_id`, `kind`, `address` |
| Pairing complete | INFO | `job_id`, `success`, `reason` (if failure) |
| Connect initiated (operator or auto) | INFO | `adapter`, `address`, `initiated_by` (`"operator"`, `"auto_connect"`, `"incoming"`) |
| BlueZ connection established | INFO | — |
| BlueZ connection lost | WARN | `prior_uptime_s` |
| BlueZ D-Bus call failure | WARN | `method`, `adapter`, `address` (if applicable), `error` |
| Malformed BlueZ property value | WARN | `adapter`, `address`, `property`, `expected_type`, `got_type` |
| Agent registration conflict | WARN | (see §8.1 for the text) |
| Profile write failure | ERROR | `address`, `error` |

Device-address values are logged in the canonical `AA:BB:CC:DD:EE:FF` form to match BlueZ's own logs; correlate with BlueZ via `journalctl -u bluetooth` using the address as the grep key.

---

## 14. Testing Strategy

### 14.1 Unit Tests

- **Adapter state machine.** For each state, verify the transitions triggered by every incoming event. No state should have reachable transitions that aren't tested.
- **Device state machine.** Same coverage, with particular attention to the Pair → Failed → retry path and to BLE-specific `Discovered → Connecting` shortcuts.
- **Agent callback translation.** For each of BlueZ's Agent method calls, verify the corresponding `NexusEvent::BtPairingPrompt` kind and data fields.
- **MacAddr parsing.** `"AA:BB:CC:DD:EE:FF"` round-trips through `from_bluez` / `to_bluez` / `to_object_path_component`. Malformed strings (wrong length, invalid hex, wrong separator) produce a typed error.

### 14.2 Integration Tests

- **Mock BlueZ.** A `zbus::ObjectServer` harness that exposes `org.bluez`-shaped interfaces with scripted behaviors (add an adapter, add a device with specific properties, fire PropertiesChanged, accept or reject RegisterAgent). Verifies the full ObjectManager → event → state-machine path.
- **BlueZ reconnect.** Mock BlueZ disappears and reappears; verify all adapters transition Unavailable → (on reconnect) repopulated from republish.
- **Agent registration conflict.** Mock BlueZ returns AlreadyExists on RegisterAgent; verify the warn log and graceful continuation.
- **Pairing with numeric comparison.** End-to-end: operator → Pair → Agent RequestConfirmation → BtPairingPrompt → AnswerPairingPrompt(accept=true) → Paired.
- **Pairing rejected.** Same flow with AnswerPairingPrompt(accept=false); verify Failed state and BtPairingComplete event.
- **Forget → re-pair.** Bond a device; Forget; verify profile removed and BlueZ's RemoveDevice called; re-pair same device; verify new profile.

### 14.3 Hardware-in-Loop Tests

- **Real BlueZ with a USB dongle.** Uses a second machine (or an nRF52 dev kit in peripheral mode) as the peer. Covers pairing, connection, RSSI tracking, graceful and ungraceful disconnect.
- **Multi-adapter.** Two USB dongles on the same host; verify per-adapter isolation and per-adapter discovery.
- **Power cycle.** Power-state transitions cycle adapters on/off; verify `auto_connect` re-establishes stored sessions.

### 14.4 Fault Injection

- **BlueZ crash during pairing.** Kill BlueZ mid-Pair(); verify the in-flight `PairingJobId` is failed with a reason and the D-Bus client sees `BtPairingComplete { success: false }`.
- **Adapter unplug during discovery.** Unplug the USB dongle mid-discovery; verify adapter transitions to `Gone` and the discovery session ends cleanly.
- **Malformed BlueZ property.** Inject a PropertiesChanged with an unexpected type (e.g., RSSI as string); verify the parse error is logged and the rest of the message is processed (per-field resilience).
- **Agent method timeout.** Simulate a D-Bus client that receives `BtPairingPrompt` but never calls `AnswerPairingPrompt`; verify the Agent call times out after `pairing_timeout_s` and the backend cleans up the pending job.

---

## 15. Implementation Phases

### Phase 1 — Shared types and BlueZ proxies

`crates/nexus-bluetooth/src/bluez/proxies.rs`. Hand-written zbus `#[proxy]` types for the BlueZ interfaces Nexus uses: `Adapter1`, `Device1`, `AgentManager1`, `ObjectManager`. Plus `MacAddr` in `nexus-core`. Unit tests on `MacAddr` parsing.

**Exit criterion:** `MacAddr::from_bluez("AA:BB:CC:DD:EE:FF")` round-trips through every conversion method. Proxies compile and can be instantiated against a mock bus.

### Phase 2 — BlueZ client and ObjectManager

`bluez/zbus_client.rs`, `bluez/object_manager.rs`. `ZbusBluezClient` with connect, is_connected, and the passive ObjectManager subscription that emits `BtAdapterChanged` / `BtDeviceDiscovered` on `InterfacesAdded` / `PropertiesChanged`.

**Exit criterion:** Mock BlueZ test: publish an adapter object; `BtAdapterChanged` fires within 100 ms. Add a device; `BtDeviceDiscovered` fires. Change `Connected = true`; `BtDeviceConnected` fires.

### Phase 3 — Adapter state machine

`adapter.rs`, part of `backend.rs`. Per-adapter `BtAdapterEntry` and state transitions. Integrate with `InterfaceDiscovered` / `InterfaceRemoved` from DD-001. No device handling yet.

**Exit criterion:** Simulated adapter lifecycle transitions through `Unavailable → Present → Powered → Discovering → Powered → Gone` with the right triggers.

### Phase 4 — Device state machine and BlueZ ops

`device.rs`, plus BluezClient operations: `set_powered`, `set_discoverable`, `start_discovery`, `stop_discovery`, `connect_device`, `disconnect_device`, `forget_device`. Device entries populated from ObjectManager; state transitions driven by PropertiesChanged.

**Exit criterion:** End-to-end `StartDiscovery → device appears → Connect → Connected → Disconnect → Paired` against mock BlueZ. Every transition has a corresponding metric increment.

### Phase 5 — Agent and pairing

`agent.rs`, `pairing.rs`. Register `/fi/nexus/bluez_agent`, translate Agent callbacks to `BtPairingPrompt` events, track `PairingJobId` correlation, implement `AnswerPairingPrompt` through a oneshot channel per job.

**Exit criterion:** Pair a device with numeric comparison: operator Pair → prompt → AnswerPairingPrompt(accept=true) → Paired. Reject path: AnswerPairingPrompt(accept=false) → Failed. Agent-already-registered path: warn log + continue.

### Phase 6 — Profile Store integration

`profile.rs`. `BluetoothProfile` with storage via `put_bluetooth` / `load_bluetooth_profile_by_address`. Profiles created on successful pairing; applied to newly-discovered devices for auto-connect decisions.

**Exit criterion:** Pair a device; profile appears in the store; restart Nexus; profile is reloaded and auto-connects when the device appears. Forget removes both the profile and BlueZ's bond.

### Phase 7 — Reconnection, supervisor, and power

Reconcile tick polling `is_connected()`; backoff on BlueZ outage; `subsystem_unavailable` notification; `PowerState` transitions affecting adapter power.

**Exit criterion:** Kill BlueZ; backend reconnects; all adapters republished; previously-connected devices either reconnected (if surviving) or awaiting auto-connect. Cycle `PowerState`; verify adapters power off in sleep and back on in active.

### Phase 8 — Observability and hardening

Metrics per §13.2. Structured logs. Fault-injection tests from §14.4. Hardware soak test against a real dongle and a controllable peer.

**Exit criterion:** All metrics visible via Prometheus scrape. Soak test (48 hours, repeated discovery + connect + disconnect cycles) produces no leaks and no unexpected state transitions.

---

## Related Documents

- [Nexus Architecture](./nexus-architecture.md) — Parent, including [ADR-004](./nexus-architecture.md#43-key-architectural-decisions) which pins BlueZ over D-Bus as the integration model
- [DD-001: Interface Discovery](./dd-001-interface-discovery.md) — Source of `InterfaceDiscovered` / `InterfaceRemoved` events for Bluetooth adapters; §5.4 details udev-based HCI discovery; defines `InterfaceKind::Bluetooth` with the `bluez_path` field this DD consumes
- [DD-006: D-Bus API](./dd-006-dbus-api.md) — Consumer of `BtAdapterChanged`, `BtDeviceDiscovered`, `BtDeviceConnected`, `BtDeviceDisconnected`, and pairing events; §6.4 (Bluetooth Interface) and a forthcoming §6.6 (BluetoothDevice Interface) define the operator-facing surface
- [DD-007: Profile Store](./dd-007-profile-store.md) — Storage for `BluetoothProfile` objects; ULID-keyed with linear-scan address lookup. This DD adds the Bluetooth methods and `ProfileRef::Bluetooth` variant to the `ProfileStore` trait
