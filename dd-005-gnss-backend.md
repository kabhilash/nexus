# DD-005: GNSS Backend — Detailed Design

**Parent:** [Nexus Architecture](./nexus-architecture.md)
**Depends on:** [DD-001: Interface Discovery](./dd-001-interface-discovery.md), [DD-007: Profile Store](./dd-007-profile-store.md) (minimal — only device preferences)
**Referenced by:** [DD-006: D-Bus API](./dd-006-dbus-api.md)
**Status:** Draft
**Scope:** Design of the GNSS Backend — how Nexus surfaces position, velocity, time, and satellite information from gpsd to the rest of the system, handles device lifecycle, and deals with the operational quirks of satellite navigation hardware (long fix acquisition, intermittent reception, moving vs stationary modes).

---

## Table of Contents

1. [Context](#1-context)
   - 1.1 [Repo Layout](#11-repo-layout)
2. [Responsibilities](#2-responsibilities)
3. [Device Lifecycle](#3-device-lifecycle)
   - 3.1 [States](#31-states)
   - 3.2 [State Transitions](#32-state-transitions)
   - 3.3 [Why GNSS is different from the other backends](#33-why-gnss-is-different-from-the-other-backends)
4. [gpsd Abstraction](#4-gpsd-abstraction)
   - 4.1 [Trait Definition](#41-trait-definition)
   - 4.2 [Shared Types](#42-shared-types)
5. [Core Lifecycle Logic](#5-core-lifecycle-logic)
6. [gpsd Protocol Integration](#6-gpsd-protocol-integration)
   - 6.1 [Connecting to gpsd](#61-connecting-to-gpsd)
   - 6.2 [Device Registration](#62-device-registration)
   - 6.3 [Watch Mode and Message Parsing](#63-watch-mode-and-message-parsing)
   - 6.4 [Reconnection](#64-reconnection)
7. [Fix Quality and Filtering](#7-fix-quality-and-filtering)
   - 7.1 [Fix Modes](#71-fix-modes)
   - 7.2 [Quality Thresholds](#72-quality-thresholds)
   - 7.3 [Stationary vs Moving](#73-stationary-vs-moving)
8. [Configuration](#8-configuration)
   - 8.1 [gpsd Deployment Assumptions](#81-gpsd-deployment-assumptions)
   - 8.2 [Time-Source Semantics](#82-time-source-semantics)
   - 8.3 [D-Bus Path Mapping for GNSS Devices](#83-d-bus-path-mapping-for-gnss-devices)
9. [Device Profile](#9-device-profile)
10. [Power Management](#10-power-management)
11. [Error Handling and Observability](#11-error-handling-and-observability)
    - 11.1 [Fault Classes](#111-fault-classes)
    - 11.2 [Observability](#112-observability)
12. [Testing Strategy](#12-testing-strategy)
13. [Implementation Phases](#13-implementation-phases)

---

## 1. Context

GNSS — GPS, GLONASS, Galileo, BeiDou, and the rest — is the odd one out among Nexus's managed technologies. Ethernet, Wi-Fi, and Bluetooth all *carry traffic*; they make connectivity happen. GNSS carries nothing. It is a one-way receive-only sensor that occasionally reports "here's where I think I am." But it shares enough with the other technologies — device discovery, per-device lifecycle, operator-visible state, event-bus integration — that it fits naturally as a Nexus backend rather than a separately-managed subsystem.

The GNSS Backend does not decode NMEA or u-blox UBX or any other binary protocol. [ADR-005](./nexus-architecture.md#43-key-architectural-decisions) delegates that to gpsd: gpsd handles the zoo of receiver quirks (warm start vs cold start, differing PVT message layouts, multi-constellation output, time jumps at rollover) and exposes a uniform JSON protocol over a local TCP socket. Nexus becomes a gpsd client that translates gpsd's messages into `NexusEvent` and surfaces them via D-Bus.

This keeps the backend small. Most of the complexity is not protocol parsing but operational policy: when is a fix "good enough" to expose to consumers? How often should position updates fire? What does the D-Bus surface look like for a device that is physically present but hasn't seen the sky in hours?

### 1.1 Repo Layout

The code for this component lives at:

```
crates/
  nexus-gnss/                   <- GNSS Backend
    Cargo.toml
    src/
      lib.rs                    <- entry point (spawn_gnss_backend)
      backend.rs                <- GnssBackend top-level orchestrator
      lifecycle.rs              <- per-device state machine (§3, §5)
      gpsd/                     <- gpsd client abstraction
        mod.rs                  <- GpsdClient trait
        json_client.rs          <- default impl (TCP JSON)
        messages.rs             <- gpsd JSON message types
        parse.rs                <- JSON -> structured types
      fix.rs                    <- Fix, FixMode, quality thresholds (§7)
      profile.rs                <- GnssDeviceProfile (§9)
      errors.rs
    tests/
      parse.rs                  <- gpsd JSON parsing
      lifecycle.rs              <- state machine unit tests
      fix_filter.rs             <- quality threshold tests
```

**Key dependencies:**

| Crate | Purpose |
|---|---|
| `tokio` | Async runtime; TCP socket to gpsd |
| `serde` + `serde_json` | gpsd JSON message parsing |
| `thiserror` | Error types |
| `async-trait` | Trait method async |
| `tracing` | Logging |

No dependency on `gpsd-client` or similar — the JSON protocol is simple enough that rolling the minimal subset Nexus needs is less overhead than pulling in a full client crate (which would also tend to reinvent its own reconnect logic we don't need).

---

## 2. Responsibilities

The GNSS Backend is responsible for:

1. **Receiving device-discovered events** from the Interface Monitor (`NexusEvent::InterfaceDiscovered` with `InterfaceKind::Gnss`) and establishing a gpsd subscription for each.
2. **Maintaining a connection to gpsd** and reconnecting automatically when the daemon restarts or its socket closes.
3. **Translating gpsd messages** (TPV position, SKY satellite info, DEVICES device list) into `NexusEvent` values on the bus.
4. **Applying fix-quality filters** so that downstream consumers don't see noisy or nonsensical position fixes during acquisition.
5. **Tracking per-device state** (connected to gpsd, receiving fix, no-fix degraded, gone) and exposing it to the D-Bus layer.
6. **Loading and honoring per-device profiles** (fix-quality thresholds, update rates, enabled-at-boot flag) from the Profile Store.

The GNSS Backend is explicitly **not** responsible for:

- Parsing NMEA or binary GNSS protocols — that is gpsd's job.
- Running gpsd itself — gpsd is managed by systemd (or equivalent) and has its own activation model. Nexus assumes it exists; the integrator ensures it.
- Dead-reckoning, map-matching, or fusion with IMU/odometry — these are application-layer concerns.
- Exposing raw satellite ephemeris, RAIM data, or DGPS correction streams — not needed by Nexus's operational model and not universally provided by gpsd.
- Time synchronization via GNSS PPS — chrony/ntpd consume gpsd directly for that use case without going through Nexus.

---

## 3. Device Lifecycle

### 3.1 States

```rust
enum GnssDeviceState {
    /// Device discovered by the Interface Monitor; gpsd has been told
    /// about it (or will be, at next reconcile if gpsd is down). Waiting
    /// for the first quality-passing fix. A GNSS cold-start can legitimately
    /// take minutes in this state before producing a usable fix.
    Acquiring { since: Instant },

    /// Receiving fixes meeting the configured quality threshold (§7.2).
    /// This is the steady-state "working" state.
    Tracking {
        last_fix: GnssFix,
        last_fix_at: Instant,
    },

    /// Was tracking, then signal deteriorated — recent fixes failed
    /// quality threshold, or TPV messages stopped arriving for >= timeout
    /// (default 30 s). Still registered with gpsd; will return to Tracking
    /// if the signal recovers.
    Degraded {
        last_good_fix: Option<GnssFix>,
        since: Instant,
    },

    /// Device physically removed (udev remove event from DD-001) or
    /// permanent gpsd-reported error. Interface object is about to
    /// disappear.
    Gone,
}
```

Whether gpsd itself is reachable is a *backend-wide* concern, not per-device — exposed via the `GpsdConnected` D-Bus property (DD-006 §6.5). Per-device state reflects fix-level behavior, not subsystem health. A newly-discovered device on a system with gpsd down enters `Acquiring` (intent is to be tracking) and stays there until gpsd reconnects and TPV messages start arriving.

### 3.2 State Transitions

```
                    InterfaceDiscovered(kind=Gnss)
  [initial] ─────────────────────────────────────────►  Acquiring
                                                          │
                              ┌───────────────────────────┤
                              │                           │
                              │ first fix passes          │ acquisition timeout
                              │ quality threshold         │ (default 5 min)
                              ▼                           ▼
                          Tracking                   Degraded
                              │                           │
              ┌───────────────┤                           │
              │               │                           │
              │ quality       │ fix quality OK            │
              │ degradation   │                           │
              │ OR TPV stall  │                           │
              ▼               │                           │
          Degraded ───────────┘◄──────────────────────────┘
              │
              │ InterfaceRemoved
              ▼
           Gone
```

- **Acquisition timeout** (default 5 minutes) exists because a GNSS cold start can take minutes; we don't want to declare a device `Degraded` before it has had a chance to acquire. Configured globally via `[gnss] acquisition_timeout_s` (§8); not currently per-device configurable.
- **TPV stall** — if the backend stops receiving TPV messages for >= 30 s while the device is tracking, treat it as signal loss and transition to Degraded. gpsd itself doesn't send "signal lost" events; the stall detection is Nexus's responsibility. Configured globally via `[gnss] tpv_stall_timeout_s`.

### 3.3 Why GNSS is different from the other backends

Ethernet and Wi-Fi are traffic carriers: their "healthy" state is binary — up or down. GNSS has a three-way operational state (acquiring, tracking, degraded) that can't be meaningfully collapsed to carrier/no-carrier. The lifecycle enum reflects this.

Also unique to GNSS: **the device itself is almost never the bottleneck.** A healthy GNSS receiver in a bad location (indoors, urban canyon, under a metal roof) behaves identically to a broken one at the gpsd interface — both just don't report fixes. The lifecycle treats "no fix for a while" as a degraded state, not a device failure, and doesn't try to distinguish "device is broken" from "the sky isn't visible right now." Operator diagnostics examine the satellite count and SNR values (exposed via the D-Bus `SatellitesInView` property) to make that distinction.

Finally: there is **no connect/disconnect action** in the GNSS Backend's API. The backend is purely reactive — devices appear, gpsd reports what it reports, fix events propagate. The closest analog to "connect" is the automatic gpsd subscription on discovery; there is no operator-triggered equivalent.

---

## 4. gpsd Abstraction

### 4.1 Trait Definition

```rust
/// A gpsd client backend. Implementations manage the connection to
/// gpsd and translate its JSON protocol into structured events.
///
/// Construction convention (not part of the object-safe trait):
/// ```ignore
/// impl JsonGpsdClient {
///     pub async fn new(
///         endpoint: SocketAddr,           // default 127.0.0.1:2947
///         event_tx: broadcast::Sender<NexusEvent>,
///     ) -> Result<Self> { ... }
/// }
/// ```
///
/// Progress is reported asynchronously via the Nexus event bus:
/// every concrete impl emits `NexusEvent::GnssTpvReceived` and
/// `NexusEvent::GnssSatellites` as gpsd messages arrive. The GNSS
/// Backend subscribes to these raw events, applies quality filtering
/// and rate-capping (§5.1 two-tier architecture), and re-emits
/// `NexusEvent::GnssFixChanged` for downstream consumers. The trait
/// impl does not poll or emit filtered events — that's the backend's
/// job.
#[async_trait]
pub trait GpsdClient: Send + Sync {
    /// Establish or re-establish the connection to gpsd.
    /// Idempotent — calling when already connected is a no-op.
    /// On success, emits NexusEvent::GnssGpsdConnected.
    async fn connect(&mut self) -> Result<()>;

    /// Report whether the client currently has a live connection to
    /// gpsd. Used by the reconcile supervisor (§6.4) to decide whether
    /// to retry connect(). Implementations should return quickly — this
    /// is called 1/second.
    fn is_connected(&self) -> bool;

    /// Inform gpsd about a newly-discovered device path. This maps to
    /// a `?DEVICE={"path": "...", "activate": true}` command if the
    /// device isn't already in gpsd's DEVICES list, otherwise no-op.
    ///
    /// Note: gpsd autodetects many devices itself (via hotplug hooks
    /// or its configured device list). On systems where gpsd has
    /// already picked up the device, this call is just a check.
    async fn add_device(&mut self, path: &str) -> Result<()>;

    /// Request removal of a device from gpsd's watch list. Typically
    /// triggered when the Interface Monitor reports InterfaceRemoved.
    async fn remove_device(&mut self, path: &str) -> Result<()>;

    /// Query the current fix for a specific device. Diagnostics only;
    /// not used in the steady-state event loop (which is push-driven
    /// by gpsd's JSON message stream). Implementations may back this
    /// with gpsd's ?POLL command, or with an internal per-device cache.
    async fn current_fix(&self, path: &str) -> Result<Option<GnssFix>>;

    /// Backend identifier for logging and metrics. Typically "gpsd-json".
    fn name(&self) -> &'static str;
}
```

### 4.2 Shared Types

These types live in `nexus-gnss` and are the canonical representations used across the backend and by consumers via D-Bus.

```rust
/// A single position/velocity/time fix.
#[derive(Debug, Clone)]
pub struct GnssFix {
    /// Receiver timestamp, satellite-derived UTC (not the local wall
    /// clock — see §8.2). Parsed from gpsd's ISO-8601 time field. If
    /// the TPV arrived without a parseable time, falls back to wall
    /// clock; consumers that need high-confidence satellite time should
    /// check that the fix's `time` is close to `Utc::now()` — if it
    /// matches exactly, the gpsd time was missing.
    pub time: DateTime<Utc>,

    /// Fix dimensionality and confidence.
    pub mode: FixMode,

    /// Degrees, WGS84. Present for Fix2D and Fix3D.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,

    /// Altitude in meters, reference frame as reported by gpsd:
    /// - If gpsd provided `altHAE` (height above WGS84 ellipsoid), that value.
    /// - Else if gpsd provided `altMSL` (height above mean sea level
    ///   per the EGM geoid model gpsd was built with), that value.
    /// - Otherwise None.
    /// Consumers that need to distinguish the two can't, at this layer —
    /// altHAE and altMSL for the same point can differ by 10-50 m.
    /// For most applications this doesn't matter; for surveying, prefer
    /// a direct gpsd client that preserves the distinction.
    /// Present for Fix3D.
    pub altitude_m: Option<f64>,

    /// Meters/second over ground. Zero when stationary or unknown.
    pub speed_mps: Option<f64>,

    /// Degrees true, 0..360, direction of motion. Undefined at zero
    /// speed; typically None when stationary.
    pub track_deg: Option<f64>,

    /// Estimated horizontal position error, meters (95% CI). From
    /// gpsd's `eph` if present, otherwise synthesized from `epx` and `epy`.
    pub horizontal_error_m: Option<f64>,

    /// Estimated vertical error, meters (95% CI). From gpsd's `epv`.
    pub vertical_error_m: Option<f64>,

    /// Count of satellites used in the fix (not merely in view).
    pub satellites_used: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixMode {
    NoFix,      // gpsd mode 0 or 1
    Fix2D,      // gpsd mode 2
    Fix3D,      // gpsd mode 3
}

/// Per-satellite information from the SKY message.
#[derive(Debug, Clone)]
pub struct SatInfo {
    /// gpsd's "gnssid" field. 0=GPS, 1=SBAS, 2=Galileo, 3=BeiDou,
    /// 5=QZSS, 6=GLONASS, 7=IRNSS. Maps to the GNSS constellation.
    pub gnss_id: u8,

    /// Satellite identifier within its constellation (PRN).
    pub sv_id: u16,

    /// Signal-to-noise ratio, dB-Hz. Typical good values are 30-50.
    pub snr_db: Option<f32>,

    /// Elevation above horizon, degrees.
    pub elevation_deg: Option<f32>,

    /// Azimuth from true north, degrees.
    pub azimuth_deg: Option<f32>,

    /// Whether this satellite contributed to the most recent fix.
    pub used: bool,
}
```

`GnssFix` and `SatInfo` are re-exported from `nexus-gnss` for any component that needs to work with GNSS data — primarily the D-Bus layer (DD-006), but also any future fleet-tracking or logging component.

---

## 5. Core Lifecycle Logic

### 5.1 Event Flow Overview

The GNSS Backend uses a **two-tier event architecture** to cleanly separate raw gpsd data from the filtered, rate-capped view exposed to downstream consumers:

```
 gpsd ──TPV JSON──► JsonGpsdClient ──NexusEvent::GnssTpvReceived──► bus
                                                                     │
                                                                     ▼
                                           GnssBackend.handle_event(GnssTpvReceived)
                                             • updates state machine
                                             • applies §7 quality filter
                                             • applies §7.3 emission policy
                                                                     │
                                                                     ▼
                                      NexusEvent::GnssFixChanged  ──► bus
                                        (filtered; consumed by D-Bus layer,
                                         recorders, etc.)

 gpsd ──SKY JSON──► JsonGpsdClient ──NexusEvent::GnssSatellites ────► bus
                                                                     │
                                    ┌────────────────────────────────┤
                                    ▼                                ▼
                      GnssBackend.handle_event(GnssSatellites)   D-Bus layer
                       • updates per-device snapshot              • emits
                         (for D-Bus Satellites* properties)         SatellitesChanged
                                                                    signal (coalesced)
```

Two distinct `NexusEvent` variants carry position data:

- **`GnssTpvReceived`** — every TPV the gpsd client receives. Unfiltered. Consumers that need raw data (diagnostic recorders, fleet-logging, NTRIP-style flows) subscribe to this.
- **`GnssFixChanged`** — quality-filtered and rate-capped fixes emitted by the GNSS Backend itself. This is what the D-Bus layer (DD-006 §6.5) translates into `fi.nexus.Gnss.FixChanged`.

The backend subscribes only to `GnssTpvReceived`; it never subscribes to `GnssFixChanged` (avoiding a subscribe-to-own-emission loop).

`GnssSatellites` uses a **single-tier** flow — the gpsd client emits it from every SKY message; both the backend (for state snapshot) and the D-Bus layer (for signal emission, with coalescing per DD-006 §12.2) subscribe independently. A separate filtered variant for satellites isn't needed because SKY messages are lower-rate than TPV and their D-Bus equivalent is already coalesced.

### 5.2 Lifecycle Handler

```rust
/// Per-device state held by the GNSS Backend.
struct GnssDeviceEntry {
    info: InterfaceInfo,
    profile: GnssDeviceProfile,
    state: GnssDeviceState,
    /// When we last saw a TPV message for this device (used for
    /// stall detection in §3.2).
    last_tpv_at: Option<Instant>,
    /// When we last emitted GnssFixChanged for this device. Used
    /// for max_update_hz rate capping and heartbeat emission.
    last_fix_emit_at: Option<Instant>,
    /// The fix content of the last emitted GnssFixChanged, used for
    /// movement-threshold comparison when report_movement_only is set.
    last_emitted_fix: Option<GnssFix>,
    /// The most recent satellite snapshot for this device, used by
    /// the D-Bus layer to serve SatellitesInView / SatellitesUsed.
    last_satellites: Vec<SatInfo>,
}

/// Handler called when a NexusEvent arrives from the bus.
/// All state transitions go through here.
async fn handle_event(&mut self, event: NexusEvent) -> Result<()> {
    match event {
        NexusEvent::InterfaceDiscovered(info)
            if matches!(info.kind, InterfaceKind::Gnss { .. }) =>
        {
            let ifindex = info.ifindex;
            let device_path = match &info.kind {
                InterfaceKind::Gnss { device_path, .. } => device_path.clone(),
                _ => unreachable!(),
            };

            let profile = self
                .profile_store
                .load_gnss_profile_by_path(&device_path)
                .await?
                .unwrap_or_else(|| GnssDeviceProfile::default_for(&device_path));

            self.devices.insert(ifindex, GnssDeviceEntry {
                info,
                profile: profile.clone(),
                state: GnssDeviceState::Acquiring { since: Instant::now() },
                last_tpv_at: None,
                last_fix_emit_at: None,
                last_emitted_fix: None,
                last_satellites: Vec::new(),
            });

            // Ask gpsd to start watching the device, but only if the
            // profile has auto_activate set (the default). Operators who
            // want a device visible to Nexus but not initially streaming
            // can set auto_activate = false. The add_device call may fail
            // silently if gpsd isn't up yet — the reconcile loop (§6.4)
            // retries when gpsd reconnects.
            if profile.auto_activate {
                let _ = self.gpsd.add_device(&device_path).await;
            }
        }

        NexusEvent::InterfaceRemoved { ifindex } => {
            if let Some(entry) = self.devices.remove(&ifindex) {
                let device_path = match &entry.info.kind {
                    InterfaceKind::Gnss { device_path, .. } => device_path.clone(),
                    _ => return Ok(()),
                };
                let _ = self.gpsd.remove_device(&device_path).await;
                // State implicitly becomes Gone via the entry being removed.
                // Consumers see the InterfaceRemoved event directly.
            }
        }

        NexusEvent::GnssTpvReceived { device, fix } => {
            self.on_tpv(&device, fix).await?;
        }

        NexusEvent::GnssSatellites { device, satellites } => {
            self.on_satellites(&device, satellites);
        }

        NexusEvent::GnssGpsdConnected => {
            // Re-register every known device (profile.auto_activate only).
            // See §6.4.
            self.reregister_devices().await;
        }

        NexusEvent::GnssGpsdDisconnected => {
            // The reader task has exited; the reconcile loop will
            // attempt reconnection. Record the outage start time for
            // the prolonged-outage notification path (§6.4). We do NOT
            // flip per-device state to Degraded here — that's the TPV
            // stall timeout's job, which intentionally keeps running
            // through brief gpsd blips.
            if self.first_outage_at.is_none() {
                self.first_outage_at = Some(Instant::now());
            }
        }

        _ => {}
    }

    Ok(())
}

/// Handle a raw TPV. Updates state machine, applies §7 filters, and
/// emits a filtered GnssFixChanged if the emission policy (§7.3) allows.
///
/// Borrow discipline: the device entry is borrowed mutably only inside
/// the inner scope. Once we know whether to emit, the borrow is dropped
/// so the event_tx.send() call can proceed without fighting the
/// borrow checker.
async fn on_tpv(&mut self, device_path: &str, fix: GnssFix) -> Result<()> {
    let now = Instant::now();

    let emit_fix = {
        let Some(entry) = self.device_by_path_mut(device_path) else {
            return Ok(());   // TPV for an unknown device; drop
        };

        entry.last_tpv_at = Some(now);

        // Apply quality filter (§7.2). Fixes below threshold do not
        // advance the state machine into Tracking and do not emit
        // GnssFixChanged.
        let passes = fix_quality_ok(&fix, &entry.profile);

        match (&entry.state, passes) {
            (GnssDeviceState::Acquiring { .. }, true)
            | (GnssDeviceState::Degraded { .. }, true)
            | (GnssDeviceState::Tracking { .. }, true) => {
                entry.state = GnssDeviceState::Tracking {
                    last_fix: fix.clone(),
                    last_fix_at: now,
                };
            }
            (GnssDeviceState::Tracking { last_fix, .. }, false) => {
                entry.state = GnssDeviceState::Degraded {
                    last_good_fix: Some(last_fix.clone()),
                    since: now,
                };
            }
            _ => {}  // no transition (e.g. failing fix while Acquiring/Degraded)
        }

        if !passes || !should_emit(entry, &fix, now) {
            None
        } else {
            entry.last_fix_emit_at = Some(now);
            entry.last_emitted_fix = Some(fix.clone());
            Some(fix)
        }
    };

    if let Some(fix) = emit_fix {
        let _ = self.event_tx.send(NexusEvent::GnssFixChanged {
            device: device_path.to_string(),
            fix,
        });
    }

    Ok(())
}

/// Update the per-device satellite snapshot. No state-machine effect;
/// D-Bus layer reads the snapshot when clients query SatellitesInView
/// / SatellitesUsed.
fn on_satellites(&mut self, device_path: &str, satellites: Vec<SatInfo>) {
    if let Some(entry) = self.device_by_path_mut(device_path) {
        entry.last_satellites = satellites;
    }
}
```

**Emission policy** in `should_emit` combines the two knobs from §7.3. The profile loader clamps `max_update_hz >= 1` at load time — a zero value in the TOML is treated as 1 with a warn log, so division-by-zero can't occur in steady state.

```rust
fn should_emit(entry: &GnssDeviceEntry, fix: &GnssFix, now: Instant) -> bool {
    let profile = &entry.profile;

    // max_update_hz cap: at most 1 emission per (1/max_update_hz) seconds.
    // max_update_hz is clamped to >= 1 at profile load (see §9).
    if let Some(last) = entry.last_fix_emit_at {
        let min_interval = Duration::from_secs_f32(1.0 / profile.max_update_hz.max(1) as f32);
        if now.duration_since(last) < min_interval {
            return false;
        }
    }

    if !profile.report_movement_only {
        return true;
    }

    // Stationary receiver optimization: emit only if movement >= threshold
    // OR heartbeat_interval has elapsed.
    match (&entry.last_emitted_fix, entry.last_fix_emit_at) {
        (Some(prev), Some(last_emit)) => {
            let moved = great_circle_distance_m(prev, fix)
                .map_or(false, |d| d >= profile.movement_threshold_m);
            let heartbeat_due = now.duration_since(last_emit)
                >= Duration::from_secs(profile.heartbeat_interval_s as u64);
            moved || heartbeat_due
        }
        // No prior emission (first emission for this device since
        // startup, or last_emitted_fix cleared): emit unconditionally.
        _ => true,
    }
}
```

**Acquisition timeout and TPV-stall detection** run in a separate periodic task (1 Hz tick) that walks the device map, compares `since` / `last_tpv_at` against the global config thresholds, and transitions `Acquiring → Degraded` or `Tracking → Degraded` as appropriate. Keeping this out of `handle_event` avoids coupling the timeouts to event arrival:

```rust
/// Periodic tick at 1 Hz. Walks every device and applies timeout
/// rules. Called from the same task as handle_event so locking isn't
/// needed — this is a plain method on GnssBackend.
fn check_timeouts(&mut self, now: Instant) {
    let acq_timeout = Duration::from_secs(self.config.acquisition_timeout_s as u64);
    let stall_timeout = Duration::from_secs(self.config.tpv_stall_timeout_s as u64);

    for entry in self.devices.values_mut() {
        match &entry.state {
            GnssDeviceState::Acquiring { since } => {
                if now.duration_since(*since) >= acq_timeout {
                    entry.state = GnssDeviceState::Degraded {
                        last_good_fix: None,
                        since: now,
                    };
                }
            }
            GnssDeviceState::Tracking { .. } => {
                if let Some(last_tpv) = entry.last_tpv_at {
                    if now.duration_since(last_tpv) >= stall_timeout {
                        // Capture last_fix before overwriting state.
                        let last_good = match &entry.state {
                            GnssDeviceState::Tracking { last_fix, .. } => {
                                Some(last_fix.clone())
                            }
                            _ => None,
                        };
                        entry.state = GnssDeviceState::Degraded {
                            last_good_fix: last_good,
                            since: now,
                        };
                    }
                }
            }
            _ => {} // Degraded, Gone: no timeout-driven transitions
        }
    }
}
```

---

## 6. gpsd Protocol Integration

### 6.1 Connecting to gpsd

gpsd listens on `127.0.0.1:2947` by default (TCP, not Unix socket, for historical reasons; gpsd's `-G` flag enables IPv6 binding, so `[::1]:2947` works only on gpsd instances configured accordingly). The protocol is line-oriented JSON: the client sends `?VERBS={...}` commands; the daemon replies and can stream unsolicited `TPV` / `SKY` / `DEVICES` messages in response to a `?WATCH={"enable":true,"json":true}` subscription.

```rust
struct JsonGpsdClient {
    endpoint: SocketAddr,
    event_tx: broadcast::Sender<NexusEvent>,
    writer: Option<OwnedWriteHalf>,
    /// Handle to the reader task. If Some, the connection is live.
    /// If None, either never connected or reader exited.
    reader_task: Option<tokio::task::JoinHandle<()>>,
}

impl JsonGpsdClient {
    fn is_connected(&self) -> bool {
        self.reader_task
            .as_ref()
            .map_or(false, |h| !h.is_finished())
    }
}

#[async_trait]
impl GpsdClient for JsonGpsdClient {
    async fn connect(&mut self) -> Result<()> {
        if self.is_connected() {
            return Ok(());
        }

        // Drop any stale reader task handle and writer half.
        if let Some(handle) = self.reader_task.take() {
            handle.abort();
        }
        self.writer = None;

        let stream = TcpStream::connect(self.endpoint).await
            .context("connect to gpsd")?;

        // gpsd sends a VERSION message on connection. Read it so our
        // reader loop doesn't trip over it. Validate proto_major >= 3
        // (current version series) and log the release. Accept higher
        // major versions with a warning — the TPV/SKY message shapes
        // have been stable for years and a future gpsd 4.x is very
        // likely still compatible.
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        let mut version_line = String::new();
        reader.read_line(&mut version_line).await?;
        let version: VersionMessage = serde_json::from_str(&version_line)?;
        if version.proto_major < 3 {
            return Err(GnssError::GpsdProtocolTooOld { got: version.proto_major });
        }
        if version.proto_major > 3 {
            warn!(
                proto_major = version.proto_major,
                proto_minor = version.proto_minor,
                release = %version.release,
                "gpsd protocol version newer than tested; accepting",
            );
        }

        // Subscribe to JSON stream for all devices. gpsd will send DEVICES
        // immediately and TPV/SKY as they arrive.
        let watch_cmd = r#"?WATCH={"enable":true,"json":true}"#;
        writer.write_all(watch_cmd.as_bytes()).await?;
        writer.write_all(b"\n").await?;

        // Spawn the reader task, giving it sole ownership of the reader
        // half. The returned handle lets is_connected() and the supervisor
        // observe reader death.
        let event_tx = self.event_tx.clone();
        let reader_task = tokio::spawn(reader_loop(reader, event_tx));

        self.writer = Some(writer);
        self.reader_task = Some(reader_task);

        // Tell the backend the connection is live so it can re-register
        // known devices.
        let _ = self.event_tx.send(NexusEvent::GnssGpsdConnected);

        Ok(())
    }
    // ... add_device, remove_device, current_fix, name below
}
```

### 6.2 Device Registration

Most GNSS devices are autodetected by gpsd via its hotplug hook (`/lib/udev/gpsd.hotplug` on Debian-family distributions) — when the kernel creates `/dev/ttyUSB0`, udev fires gpsd's hotplug script, which calls `?DEVICE={"path":"...","activate":true}` over gpsd's control socket. By the time Nexus's Interface Monitor reports the device, gpsd usually already knows about it.

`add_device` covers the case where it doesn't:

```rust
async fn add_device(&mut self, path: &str) -> Result<()> {
    let Some(writer) = self.writer.as_mut() else {
        return Err(GnssError::NotConnected);
    };

    // Serialize via serde_json to escape any special characters in path
    // safely. While udev-normal device paths like "/dev/ttyUSB0" don't
    // need escaping, writing this manually with format! would break on
    // pathological inputs (embedded quotes, backslashes). gpsd's DEVICE
    // command with activate=true is idempotent — safe to call even if
    // gpsd already has the device.
    let body = serde_json::json!({
        "path": path,
        "activate": true,
    });
    let cmd = format!("?DEVICE={}\n", body);
    writer.write_all(cmd.as_bytes()).await?;
    Ok(())
}
```

No response correlation is required; gpsd emits an updated DEVICES message reflecting the new state, which the reader task picks up.

### 6.3 Watch Mode and Message Parsing

gpsd's JSON messages Nexus consumes:

- **VERSION** — sent once on connection. Captured in `connect`.
- **DEVICES** — list of currently-watched devices. Used to reconcile Nexus's view with gpsd's after reconnect.
- **DEVICE** — individual device status change (activated, deactivated).
- **TPV** — time/position/velocity. The primary fix message.
- **SKY** — satellites in view and their SNRs. Drives `GnssSatellites`.
- **ERROR** — operator-visible error from gpsd.

All other message classes (WATCH, POLL, ATT, PPS, etc.) are ignored.

The reader loop parses each line once into a `#[serde(tag = "class")]`-tagged enum, dispatching by variant. This is cheaper than the common two-pass pattern (parse a class-field struct, then parse the full message) for high-rate receivers.

```rust
#[derive(Deserialize)]
#[serde(tag = "class")]
enum GpsdMessage {
    #[serde(rename = "TPV")]
    Tpv(TpvMessage),
    #[serde(rename = "SKY")]
    Sky(SkyMessage),
    #[serde(rename = "DEVICES")]
    Devices(DevicesMessage),
    #[serde(rename = "DEVICE")]
    Device(DeviceMessage),
    #[serde(rename = "VERSION")]
    Version(VersionMessage),
    #[serde(rename = "WATCH")]
    Watch(WatchMessage),
    #[serde(rename = "ERROR")]
    Error(ErrorMessage),
    #[serde(other)]
    Other,
}

async fn reader_loop(
    mut reader: BufReader<OwnedReadHalf>,
    event_tx: broadcast::Sender<NexusEvent>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,                 // EOF -> gpsd closed
            Ok(_) => {}
            Err(e) => {
                warn!(error = ?e, "gpsd read error");
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<GpsdMessage>(trimmed) {
            Ok(GpsdMessage::Tpv(msg)) => {
                if let Some((device, fix)) = parse_tpv(&msg) {
                    let _ = event_tx.send(
                        NexusEvent::GnssTpvReceived { device, fix }
                    );
                }
            }
            Ok(GpsdMessage::Sky(msg)) => {
                if let Some((device, sats)) = parse_sky(&msg) {
                    let _ = event_tx.send(
                        NexusEvent::GnssSatellites {
                            device,
                            satellites: sats,
                        }
                    );
                }
            }
            Ok(GpsdMessage::Device(msg)) => {
                // gpsd DEVICE message: a device's activation state changed.
                // `activated: null` means gpsd has stopped watching the
                // device (it went silent, was unplugged, or was explicitly
                // deactivated). We surface this as a hint event so the
                // backend can move the device to Degraded faster than
                // waiting for the 30 s TPV stall timeout. The Interface
                // Monitor (DD-001) is still the authoritative source for
                // presence — InterfaceRemoved fires when udev reports the
                // kernel device is gone, not when gpsd gives up on it.
                if msg.activated.is_none() {
                    if let Some(path) = msg.path {
                        debug!(device = %path, "gpsd deactivated device");
                        // Future enhancement: emit a
                        // NexusEvent::GnssGpsdDeviceDeactivated that the
                        // backend uses to short-circuit TPV stall timeout.
                        // For v0.1 we rely on the stall timeout path.
                    }
                }
            }
            Ok(GpsdMessage::Error(err)) => {
                warn!(message = %err.message, "gpsd ERROR");
            }
            Ok(_) => {}  // Devices/Version/Watch/Other — observability only
            Err(e) => {
                // Malformed JSON from gpsd — very rare. Log and continue.
                warn!(line = %trimmed, error = ?e, "malformed gpsd JSON; skipping");
            }
        }
    }

    // Reader loop exited -> connection broken. The backend's supervisor
    // (§6.4) detects this via is_connected() and triggers reconnect.
    let _ = event_tx.send(NexusEvent::GnssGpsdDisconnected);
}
```

The gpsd JSON message types are defined in `gpsd/messages.rs`:

```rust
#[derive(Deserialize)]
pub struct VersionMessage {
    pub release: String,
    pub proto_major: u32,
    pub proto_minor: u32,
}

/// TPV (Time Position Velocity). All fields except `class` and
/// `device` are optional per gpsd's protocol; they are present only
/// when the receiver has reported that datum.
#[derive(Deserialize)]
pub struct TpvMessage {
    pub device: Option<String>,
    /// ISO-8601 string per gpsd 3.x (e.g., "2026-04-22T13:52:00.000Z").
    /// Older gpsd releases sent a float Unix-epoch seconds, but
    /// proto_major >= 3 standardized on ISO-8601.
    pub time: Option<String>,
    /// gpsd mode: 0=unset, 1=no fix, 2=2D, 3=3D.
    #[serde(default)]
    pub mode: u8,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Altitude, height above ellipsoid (WGS84), meters.
    #[serde(rename = "altHAE")]
    pub alt_hae: Option<f64>,
    /// Altitude, mean sea level, meters. Older field; still populated
    /// by some receivers. Prefer altHAE when both are present.
    #[serde(rename = "altMSL")]
    pub alt_msl: Option<f64>,
    /// Speed over ground, m/s.
    pub speed: Option<f64>,
    /// Course over ground, degrees true (0..360).
    pub track: Option<f64>,
    /// Estimated horizontal position error (95% CI), meters. Direct.
    pub eph: Option<f64>,
    /// Estimated longitude error, meters. Combined with epy to
    /// synthesize eph when eph is absent.
    pub epx: Option<f64>,
    /// Estimated latitude error, meters.
    pub epy: Option<f64>,
    /// Estimated vertical error, meters.
    pub epv: Option<f64>,
    /// Satellites used in this fix.
    pub used: Option<u32>,
}

#[derive(Deserialize)]
pub struct SkyMessage {
    pub device: Option<String>,
    #[serde(default)]
    pub satellites: Vec<SkySat>,
}

#[derive(Deserialize)]
pub struct SkySat {
    /// gpsd's "gnssid": 0=GPS, 1=SBAS, 2=Galileo, 3=BeiDou, 5=QZSS,
    /// 6=GLONASS, 7=IRNSS.
    #[serde(default)]
    pub gnssid: u8,
    /// PRN / SV ID.
    #[serde(default)]
    pub svid: u16,
    pub ss: Option<f32>,           // SNR dB-Hz
    #[serde(rename = "el")]
    pub elevation: Option<f32>,
    #[serde(rename = "az")]
    pub azimuth: Option<f32>,
    #[serde(default)]
    pub used: bool,
}

#[derive(Deserialize)]
pub struct DevicesMessage {
    #[serde(default)]
    pub devices: Vec<DeviceInfo>,
}

#[derive(Deserialize)]
pub struct DeviceMessage {
    pub path: Option<String>,
    pub activated: Option<String>,
}

#[derive(Deserialize)]
pub struct DeviceInfo {
    pub path: String,
    pub activated: Option<String>,
}

#[derive(Deserialize)]
pub struct WatchMessage {
    pub enable: bool,
    pub json: bool,
}

#[derive(Deserialize)]
pub struct ErrorMessage {
    pub message: String,
}
```

`parse_tpv` and `parse_sky` handle gpsd's optional-field conventions (many fields are absent when the data isn't available) and convert to the internal structured types:

```rust
fn parse_tpv(msg: &TpvMessage) -> Option<(String, GnssFix)> {
    let device = match msg.device.clone() {
        Some(d) => d,
        None => {
            // Malformed or unusual gpsd TPV without a device field.
            // In practice every TPV carries device; a missing one
            // typically indicates a gpsd bug or corrupt stream.
            debug!("TPV without device field; dropping");
            return None;
        }
    };
    let fix = GnssFix {
        // gpsd 3.x emits time as an ISO-8601 string (e.g.,
        // "2026-04-22T13:52:00.000Z"). Parsing failure falls back to
        // wall-clock time (which is what gpsd-less consumers would see
        // anyway), with a debug log for observability.
        time: msg.time.as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc))
            .unwrap_or_else(|| {
                debug!(device = %device, "TPV without parseable time; using wall clock");
                Utc::now()
            }),
        mode: match msg.mode {
            0 | 1 => FixMode::NoFix,
            2 => FixMode::Fix2D,
            // If mode claims 3D but altitude is missing, downgrade to 2D
            // — consumers relying on Fix3D expecting altitude are better
            // served by a conservative classification (see §11.1).
            3 if msg.alt_hae.is_some() || msg.alt_msl.is_some() => FixMode::Fix3D,
            3 => FixMode::Fix2D,
            _ => FixMode::NoFix,
        },
        latitude: msg.lat,
        longitude: msg.lon,
        altitude_m: msg.alt_hae.or(msg.alt_msl),
        speed_mps: msg.speed,
        track_deg: msg.track,
        horizontal_error_m: msg.eph.or_else(|| {
            // If no direct eph, approximate from epx+epy.
            match (msg.epx, msg.epy) {
                (Some(x), Some(y)) => Some((x * x + y * y).sqrt()),
                _ => None,
            }
        }),
        vertical_error_m: msg.epv,
        satellites_used: msg.used.unwrap_or(0) as u32,
    };
    Some((device, fix))
}

fn parse_sky(msg: &SkyMessage) -> Option<(String, Vec<SatInfo>)> {
    let device = msg.device.clone()?;
    let satellites = msg.satellites.iter().map(|s| SatInfo {
        gnss_id: s.gnssid,
        sv_id: s.svid,
        snr_db: s.ss,
        elevation_deg: s.elevation,
        azimuth_deg: s.azimuth,
        used: s.used,
    }).collect();
    Some((device, satellites))
}
```

### 6.4 Reconnection

gpsd occasionally restarts (upgrade, crash, config reload). The GNSS Backend handles this with a simple supervisor loop:

- A periodic 1-second tick runs `reconcile()`.
- If `self.gpsd.is_connected()` returns `false` (the reader task exited), `reconcile()` calls `self.gpsd.connect()` with bounded exponential backoff (1 s → 2 s → 4 s, cap 30 s).
- On successful connection, the gpsd client emits `NexusEvent::GnssGpsdConnected`. The backend's `handle_event` match arm for this event calls `reregister_devices()`, which walks `self.devices` and calls `gpsd.add_device(path)` for every device whose profile has `auto_activate = true`. This is the single path for device (re-)registration — reconcile itself does not re-register devices directly.
- Per-device state is preserved across reconnection. Devices in `Tracking` stay in `Tracking` until a fresh fix arrives or the TPV-stall timeout triggers a transition to `Degraded`.

A brief sketch of the reconcile supervisor:

```rust
/// Supervisor tick: run at 1 Hz.
async fn reconcile(&mut self) {
    if self.gpsd.is_connected() {
        // Reset backoff on healthy connection.
        self.reconnect_attempts = 0;
        return;
    }

    // Connection is down. Apply backoff: 1 s → 2 s → 4 s → ... → 30 s cap.
    let backoff = std::cmp::min(
        Duration::from_secs(1 << self.reconnect_attempts.min(5)),
        Duration::from_secs(30),
    );
    if self.last_reconnect_attempt
        .map_or(true, |t| t.elapsed() >= backoff)
    {
        self.last_reconnect_attempt = Some(Instant::now());
        self.reconnect_attempts += 1;
        // connect() emits GnssGpsdConnected on success, which
        // triggers reregister_devices() via handle_event.
        let _ = self.gpsd.connect().await;

        // Outage notification after 60 s cumulative.
        if self.first_outage_at
            .map_or(false, |t| t.elapsed().as_secs() >= self.config.gpsd_outage_notify_s
                && !self.outage_notified)
        {
            self.emit_notification("subsystem_unavailable", "gpsd", t.elapsed());
            self.outage_notified = true;
        }
    }
}

/// Re-register every known device with gpsd after (re)connection.
/// Called from the GnssGpsdConnected handler in handle_event.
async fn reregister_devices(&mut self) {
    for entry in self.devices.values() {
        if !entry.profile.auto_activate {
            continue;
        }
        let path = match &entry.info.kind {
            InterfaceKind::Gnss { device_path, .. } => device_path.clone(),
            _ => continue,
        };
        let _ = self.gpsd.add_device(&path).await;
    }
    // Clear outage state on successful reconnect.
    self.first_outage_at = None;
    self.outage_notified = false;
    if self.outage_notified {
        self.emit_notification("subsystem_recovered", "gpsd", Duration::ZERO);
    }
}
```

This keeps the backend operational through gpsd restarts without escalating every blip to a user-visible notification. A prolonged gpsd outage (>= `gpsd_outage_notify_s`, default 60 s) produces a `Manager.NotificationEvent { kind: "subsystem_unavailable", data: { subsystem: "gpsd", duration_s: u } }` (see DD-006 §5.3) so operators can investigate; recovery produces `"subsystem_recovered"`.

---

## 7. Fix Quality and Filtering

### 7.1 Fix Modes

gpsd reports a `mode` field on each TPV:

| gpsd mode | Meaning | Nexus handling |
|---|---|---|
| 0 | Mode not set | Treat as NoFix; don't advance state |
| 1 | No fix | NoFix; don't advance state |
| 2 | 2D fix (lat/lon, no altitude) | Fix2D; evaluate against threshold |
| 3 | 3D fix (lat/lon/altitude) | Fix3D; evaluate against threshold |

A mode-0 or mode-1 TPV is a "heartbeat" from gpsd — it tells us the device is alive but hasn't produced position data yet. We update `last_tpv_at` but don't create a `GnssFix` record.

### 7.2 Quality Thresholds

Raw gpsd TPV messages are noisy, especially during acquisition and in challenging environments. Nexus applies a per-device quality threshold before promoting a fix into `Tracking` or `GnssFixChanged` to D-Bus:

```rust
fn fix_quality_ok(fix: &GnssFix, profile: &GnssDeviceProfile) -> bool {
    // Must have at least the minimum fix dimensionality.
    let mode_ok = match profile.min_fix_mode {
        FixMode::NoFix => true,
        FixMode::Fix2D => matches!(fix.mode, FixMode::Fix2D | FixMode::Fix3D),
        FixMode::Fix3D => matches!(fix.mode, FixMode::Fix3D),
    };
    if !mode_ok { return false; }

    // Must have at least the minimum satellite count.
    if fix.satellites_used < profile.min_satellites {
        return false;
    }

    // Horizontal error check is lenient-by-default: a fix without a
    // reported horizontal_error_m passes regardless of the profile's
    // max_horizontal_error_m threshold. Rationale: some receivers omit
    // eph (older firmware, certain NMEA sentence subsets) while still
    // producing usable fixes; rejecting them would lock out those devices
    // entirely. Deployments that want to reject "no eph reported" as a
    // class can use profile.strict_quality = true (§9) to force the
    // presence check.
    if let Some(max) = profile.max_horizontal_error_m {
        match fix.horizontal_error_m {
            Some(eph) if eph > max => return false,
            None if profile.strict_quality => return false,
            _ => {}
        }
    }

    true
}
```

Default thresholds (per `GnssDeviceProfile::default_for`):

- `min_fix_mode = Fix2D` — permit 2D fixes; 3D-only is a deployment choice.
- `min_satellites = 4` — baseline for a real GPS fix is 4 (three for trilateration plus one for time).
- `max_horizontal_error_m = Some(100.0)` — 100 m cap excludes clearly-bogus fixes during acquisition.

Operators who need stricter or looser thresholds override via the device profile (§9).

### 7.3 Stationary vs Moving

A common gpsd quirk: stationary receivers produce fix jitter of a few meters even when nothing is moving. For operators who care about "did we move" rather than "where are we exactly," the profile flag `report_movement_only: bool` (default `false`) changes the `GnssFixChanged` emission rate:

- When `report_movement_only = true`, the backend suppresses `GnssFixChanged` emission unless:
  - the fix differs from the last-emitted fix by more than `movement_threshold_m` (default 10 m), OR
  - the configured `heartbeat_interval_s` has elapsed since the last emission (default 60 s — ensures D-Bus consumers see steady-state signals).
- When `report_movement_only = false`, every quality-passing fix fires `GnssFixChanged`, subject to the update-rate cap (`max_update_hz`, default 1).

This is an optimization for bus quietness and downstream logging volume; the internal state machine always sees every fix regardless of this setting.

---

## 8. Configuration

Global GNSS Backend config in `nexus.toml`:

```toml
[gnss]
# Whether the GNSS Backend is enabled. Set false on devices
# without a GNSS receiver to skip the backend entirely.
enabled = true

# gpsd endpoint. String is parsed as a SocketAddr; wrap an IPv6
# literal in brackets: "[::1]:2947". Note that gpsd only binds IPv6
# if started with the -G flag; the default IPv4 loopback works
# without special configuration.
gpsd_endpoint = "127.0.0.1:2947"

# How long to wait for first fix before declaring Degraded (§3.2).
acquisition_timeout_s = 300

# How long without TPV messages before declaring Degraded (§3.2).
tpv_stall_timeout_s = 30

# Notify operators via D-Bus NotificationEvent after this many
# seconds of gpsd being unreachable.
gpsd_outage_notify_s = 60

# Default thresholds, overridable per-device via Profile Store.
[gnss.defaults]
min_fix_mode = "fix_2d"          # "no_fix" | "fix_2d" | "fix_3d"
min_satellites = 4
max_horizontal_error_m = 100.0
strict_quality = false           # if true, reject fixes missing eph
max_update_hz = 1
report_movement_only = false
movement_threshold_m = 10.0
heartbeat_interval_s = 60
```

### 8.1 gpsd Deployment Assumptions

Nexus assumes:

- **A single gpsd instance** services all GNSS devices on the host, at one well-known endpoint. Deployments that run multiple gpsd instances (e.g., `gpsd -N -b /dev/ttyUSB0` per device on different ports) are not supported. The typical embedded pattern — one gpsd with its socket-activation unit listening on 2947, all devices handed off via the udev hotplug script — matches this assumption.
- **The data socket only.** Nexus uses gpsd's data socket (port 2947) for reading and subscribing. gpsd's control socket (`-F /var/run/gpsd.sock`) allows runtime device configuration (sending UBX commands, enabling WAAS, etc.) and is explicitly out of scope: Nexus does not configure receivers. Integrators who need receiver configuration script it at gpsd or udev level.
- **gpsd >= 3.0 protocol.** See §6.1 — `proto_major < 3` is rejected; `proto_major > 3` is accepted with a warning log under the assumption that future major versions keep TPV/SKY message shapes backward-compatible (which has held throughout the 3.x series).

### 8.2 Time-Source Semantics

`GnssFix.time` is satellite-derived UTC as reported by the receiver, not the local wall clock. Consumers correlating fixes with other logged events on the same host need to be aware of this: on an unsynchronized device, wall clock and satellite time can differ by seconds, minutes, or (in extreme cases like an RTC-backup-battery failure) years. The intended use is GNSS-sourced timestamping for fleet telemetry; applications that want wall-clock-correlated positions should timestamp with `Instant::now()` at the point of consumption.

### 8.3 D-Bus Path Mapping for GNSS Devices

Per DD-001 §5.4, a GNSS device's `InterfaceInfo.ifname` is its kernel device path, e.g. `/dev/ttyUSB0`. DD-006 §4 escapes ifnames into D-Bus path components using the rule `[A-Za-z0-9_]` preserved, `-` → `_`, everything else percent-escaped as `_XX`. This produces long but deterministic object paths for GNSS devices:

| Device path | D-Bus object path |
|---|---|
| `/dev/ttyUSB0` | `/fi/nexus1/interface/_2fdev_2fttyUSB0` |
| `/dev/gnss-primary` (via udev symlink) | `/fi/nexus1/interface/_2fdev_2fgnss_primary` |
| `/dev/ttyS2` | `/fi/nexus1/interface/_2fdev_2fttyS2` |

These paths are ugly but functional — clients that want a human-readable identifier should read the `Ifname` property on the Interface object (which returns the unescaped path) or the `DevicePath` property on the `fi.nexus.Gnss` interface. The ObjectManager (DD-006 §5.2) is the canonical way to enumerate GNSS objects without needing to construct paths by hand.

---

## 9. Device Profile

Per-device profile, stored in the Profile Store as `gnss/<ulid>.toml`. Small — just threshold overrides and an autoactivate flag. Profiles are optional: devices without a stored profile use `GnssDeviceProfile::default_for(&path)`, which returns the global defaults.

```rust
/// Per-device GNSS configuration.
#[derive(Debug, Clone)]
pub struct GnssDeviceProfile {
    pub id: Ulid,
    pub schema_version: u32,
    pub metadata: ProfileMetadata,

    /// Human-identifiable device path (e.g., "/dev/ttyUSB0"). Used
    /// to associate a profile with its device; not used to derive
    /// the filename (ULID is).
    pub device_path: String,

    /// Optional vendor/model string for operator UI. Copied from
    /// udev's ID_MODEL when known.
    pub vendor_model: Option<String>,

    // --- Thresholds, override global defaults ---
    pub min_fix_mode: FixMode,
    pub min_satellites: u32,
    pub max_horizontal_error_m: Option<f64>,
    /// If true, a fix that doesn't report horizontal_error_m is
    /// rejected whenever max_horizontal_error_m is set. Default false —
    /// see §7.2 lenient policy.
    pub strict_quality: bool,
    pub max_update_hz: u32,
    pub report_movement_only: bool,
    pub movement_threshold_m: f64,
    pub heartbeat_interval_s: u32,

    /// Whether to auto-activate this device on discovery (default true).
    /// Setting false means the device is visible via Nexus but gpsd
    /// is not told to watch it unless an operator explicitly connects
    /// (via a future D-Bus method — not implemented in v0.1).
    pub auto_activate: bool,
}
```

Profile Store integration: `ProfileStore` in DD-007 gains the following additive methods (ULID-keyed to match the other profile types):

```rust
/// Load all GNSS device profiles. Sorted by ULID ascending.
async fn load_gnss(&self) -> Result<Vec<GnssDeviceProfile>>;

/// Look up a profile by kernel device path. This is a linear scan
/// over all stored GNSS profiles (the on-disk filename is the ULID,
/// not a hash of the path — because device paths can change across
/// USB re-enumeration, hashing them as a filename would orphan
/// profiles on every replug). For realistic GNSS device counts
/// (1–2 per system) the linear scan is negligible.
async fn load_gnss_profile_by_path(&self, device_path: &str)
    -> Result<Option<GnssDeviceProfile>>;

/// Store or overwrite a GNSS device profile, keyed by profile.id (ULID).
/// If a profile already exists with the same device_path but a different
/// ULID, returns AlreadyExists — use Update on the existing profile to
/// modify, or remove the old one first.
async fn put_gnss(&self, profile: &GnssDeviceProfile) -> Result<()>;

/// Remove a GNSS device profile by ULID. Consistent with the
/// `/fi/nexus1/profile/gnss/<ulid>` D-Bus path scheme.
async fn remove_gnss(&self, id: &Ulid) -> Result<()>;
```

The `ProfileRef<'_>` enum in DD-007 §5.1 gains a corresponding variant:

```rust
pub enum ProfileRef<'a> {
    Ethernet { ifname: &'a str },
    Wifi { ssid_hash: &'a str },
    /// GNSS profiles are ULID-keyed because device paths can shift
    /// across USB enumeration. Callers looking up by device path use
    /// load_gnss_profile_by_path instead.
    Gnss { id: &'a Ulid },
}
```

This is additive to the DD-007 trait. No credential fields in the GNSS profile, so the on-disk layout is a single straightforward struct — no `SecretString`, no dual-struct pattern, no encryption overhead.

**Profile persistence across device replug.** The matching key used by `load_gnss_profile_by_path` is the kernel device path (`/dev/ttyUSB0`, etc.). USB enumeration order isn't stable — replugging a receiver can change its path to `/dev/ttyUSB1`, breaking the lookup. For v0.1, this limitation is accepted as-is because:

- Embedded deployments almost always hardwire a single GNSS receiver at a known, stable path (often a UART like `/dev/ttyS2` rather than USB).
- The standard Linux workaround is a udev rule: `SUBSYSTEM=="tty", ATTRS{idVendor}=="1546", ATTRS{idProduct}=="01a9", SYMLINK+="gnss-primary"` creates a stable `/dev/gnss-primary` regardless of USB enumeration order. Profiles are keyed against the symlink path, and replug preserves the match.

A future revision may extend DD-001's `InterfaceKind::Gnss` with `udev_stable_id: Option<String>` populated from `ID_SERIAL` / `ID_USB_SERIAL_SHORT`, allowing automatic profile matching across replug without requiring operator-authored udev rules. That change is out of scope for v0.1.

---

## 10. Power Management

GNSS receivers are power-hungry by embedded standards (30-100 mW for standalone u-blox, 200+ mW for some patch-antenna modules). The `PowerState` global from DD-006 §5.1 affects the GNSS Backend:

| PowerState | Behavior |
|---|---|
| `active` | Full operation. gpsd subscription active. All fixes processed and filtered per §5.1. `fi.nexus.Gnss.FixChanged` and `fi.nexus.Gnss.SatellitesChanged` signals fire normally. |
| `background` | gpsd subscription active, but the profile's effective `max_update_hz` is clamped to 0.2 (5 s interval). `GnssFixChanged` emissions are throttled accordingly; `GnssSatellites` raw events continue to flow on the event bus, but the D-Bus layer coalesces `SatellitesChanged` signals to at most 1 per 30 s (via DD-006 §12.2). Internal state machine still sees every fix. |
| `sleep` | gpsd subscription maintained (gpsd itself may or may not keep the receiver powered; that is gpsd's job — see the note below). The backend does NOT emit `GnssFixChanged` while in sleep, and the D-Bus layer suppresses `FixChanged` and `SatellitesChanged` signals. Raw `GnssTpvReceived` and `GnssSatellites` events continue to flow on the internal event bus, so subscribers like a diagnostic recorder can still observe them. The backend stops updating `last_tpv_at` — TPV-stall detection is paused, so a long sleep doesn't falsely transition devices to Degraded on wake. |

On transition from `sleep` → `active`, the backend resumes normal emission but keeps the current state: if the device was `Tracking` at sleep entry and is still receiving quality fixes, it stays `Tracking` without a reacquisition. `last_tpv_at` is reset to the wake time so the stall detector has a fresh baseline.

**What "suspension" means precisely.** The event bus (`NexusEvent`) is internal plumbing — suppressing raw variants there would require disconnecting from gpsd or introducing additional filtered variants, both of which have operational costs. Instead, suspension applies at the two places that actually matter for power: (a) the backend's `GnssFixChanged` emission (which is what causes downstream work), and (b) the D-Bus signals (which wake up external subscribers). Raw events stay available in-process for components that want them.

Note that Nexus does not directly power-gate GNSS hardware. Receivers with `/dev/gpio` or vendor-specific power-control interfaces are outside Nexus's scope; integrators who need fine-grained GNSS power management should script it around Nexus (e.g., `systemctl stop gpsd.socket` then cut power) rather than expecting Nexus to own the receiver's power line.

---

## 11. Error Handling and Observability

### 11.1 Fault Classes

**gpsd unavailable.** TCP connect fails or the connection drops. The backend reconnects with backoff (§6.4) and newly-discovered devices stay in `Acquiring` until gpsd returns. Devices that were `Tracking` before the outage stay `Tracking` until the TPV-stall timeout triggers a transition to `Degraded`, independent of gpsd's connection state — this keeps downstream consumers from seeing a rapid Tracking → Acquiring → Tracking flicker on a brief gpsd restart.

**Device not recognized by gpsd.** After `add_device`, gpsd may refuse the device (unknown model, no driver match, wrong permissions on the tty). The device stays in `Acquiring` with no TPV arriving. Logged; the acquisition timeout eventually transitions it to `Degraded`. Operator action (updating gpsd's device rules, checking permissions on `/dev/ttyUSB0`, checking the gpsd ERROR stream) is required to recover.

**Malformed gpsd JSON.** Skipped with a warn-level log. Has never been observed in the wild but is defended against per §6.3.

**Fix data contradiction.** gpsd reports a TPV with `mode: 3` (3D fix) but no altitude. `parse_tpv` conservatively downgrades such a fix to `FixMode::Fix2D` — consumers relying on `Fix3D` semantically expect altitude, and a fix missing it is effectively 2D regardless of gpsd's mode claim. Similarly, if `satellites_used` in the TPV disagrees with the count of `used = true` entries in the most recent SKY for the same device (which can briefly happen because TPV and SKY arrive as separate messages), the TPV's `used` count is authoritative for the fix itself — it is the count the receiver actually used when computing that specific PVT solution. The D-Bus `SatellitesUsed` property exposes the TPV count; `SatellitesInView` exposes the SKY total.

**Device present but no fixes after acquisition timeout.** Transitions to `Degraded`. Operator diagnostics via `Gnss.SatellitesInView` (DD-006) can distinguish "no sky" (zero satellites in view) from "receiver broken" (hardware fault — usually shows via gpsd ERROR messages).

### 11.2 Observability

Metrics:

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `nexus_gnss_devices` | gauge | — | Number of registered GNSS devices |
| `nexus_gnss_state` | gauge | `device_path`, `state` | Value is `1` iff the device is currently in the labeled state, `0` otherwise. `state` takes values `acquiring` / `tracking` / `degraded` / `gone`. Four series per device (most will be `0` at any given time) |
| `nexus_gnss_tpv_total` | counter | `device_path`, `mode` | TPV messages received from gpsd. `mode` is the FixMode classification (`no_fix`, `fix_2d`, `fix_3d`) after parse-time mode validation per §11.1 |
| `nexus_gnss_fixes_filtered_total` | counter | `device_path`, `reason` | Fixes rejected by the quality filter (§7.2). `reason` is `mode`, `satellites`, or `horizontal_error` |
| `nexus_gnss_emissions_suppressed_total` | counter | `device_path`, `reason` | Quality-passing fixes suppressed by the emission policy (§7.3). `reason` is `rate_limit` or `movement_threshold` |
| `nexus_gnss_satellites_in_view` | gauge | `device_path` | Count from the most recent SKY message |
| `nexus_gnss_satellites_used` | gauge | `device_path` | Count used in the most recent qualifying fix |
| `nexus_gnss_horizontal_error_meters` | gauge | `device_path` | Most recent reported `eph` |
| `nexus_gnss_gpsd_reconnects_total` | counter | — | gpsd connection attempts since startup |
| `nexus_gnss_gpsd_connected` | gauge | — | 1 when gpsd connection is alive, 0 otherwise |
| `nexus_gnss_tpv_stall_events_total` | counter | `device_path` | Transitions to Degraded due to TPV stall |

**Label cardinality note.** `device_path` as a metric label is stable in embedded deployments with hardwired GNSS receivers at fixed paths. Deployments that see USB-enumeration churn (devices cycling through `ttyUSB0` / `ttyUSB1`) will accumulate stale series in Prometheus. For such deployments, configuring a udev `SYMLINK` (§9 profile persistence note) normalizes the path and eliminates churn. Future Nexus versions may switch to a stable udev-derived id as the label.

Logs: standard `tracing` with `target = "nexus::gnss"`. State transitions log at INFO level with structured fields `device_path`, `from_state`, `to_state`, `reason`. Quality-filter rejections and emission suppressions log at DEBUG.

---

## 12. Testing Strategy

### 12.1 Unit Tests

- **gpsd JSON parsing.** Golden-fixture TPV and SKY messages (including edge cases: missing fields, mode 0/1, very small altitudes, negative latitudes) parse correctly and produce the expected `GnssFix` / `SatInfo` values.
- **Quality filter.** A table of `(GnssFix, GnssDeviceProfile) -> bool` cases covering every rejection reason and the passing case.
- **State machine.** For each state, verify the transitions triggered by `on_tpv`, the periodic timeout tick, and `InterfaceRemoved`. No state should have reachable transitions that aren't tested.
- **Fix-emission throttling.** `report_movement_only` and `max_update_hz` behave as specified: emissions skipped when below threshold, heartbeat emission fires at `heartbeat_interval_s`.

### 12.2 Integration Tests

- **Mock gpsd.** A `tokio::net::TcpListener` harness that accepts a connection, sends a canned VERSION, accepts `?WATCH`, then streams pre-recorded TPV/SKY sequences from fixture files. The GNSS Backend connects and its emitted `NexusEvent` stream is verified against expected output.
- **gpsd reconnect.** Mock gpsd drops the connection mid-stream; verify the backend reconnects within the backoff window and resumes emitting events.
- **Interface Monitor round-trip.** Inject an `InterfaceDiscovered(Gnss)` event; verify `gpsd.add_device(path)` is called. Inject `InterfaceRemoved`; verify `remove_device`.

### 12.3 Hardware-in-Loop Tests

- **Real gpsd with gpsfake.** gpsfake feeds pre-recorded NMEA logs into a running gpsd instance, producing a realistic gpsd stream. Used in CI on a Linux host. Logs available from the gpsd project cover: cold-start acquisition, urban canyon (heavy multipath), clear sky, rollover, receiver-disconnect.
- **Power-state transitions.** Cycle `PowerState` via D-Bus; verify emission rate matches §10.

### 12.4 Fault Injection

- **Malformed JSON line.** Feed the reader a JSON line with a missing comma; verify log + skip.
- **Truncated read.** Close the socket mid-line; verify reader task exits, supervisor reconnects.
- **gpsd process hang.** Block gpsd responses for 60 s; verify TPV-stall detection transitions tracking devices to Degraded, and that the `subsystem_unavailable` notification fires.

---

## 13. Implementation Phases

### Phase 1 — Types and gpsd JSON Parsing

`crates/nexus-gnss/src/fix.rs`, `crates/nexus-gnss/src/gpsd/messages.rs`, `parse.rs`. Define `GnssFix`, `FixMode`, `SatInfo`. Implement `serde_json::Deserialize` for `VersionMessage`, `TpvMessage`, `SkyMessage`, `DevicesMessage`. Unit tests for parse correctness using captured gpsd JSON.

**Exit criterion:** Golden-fixture TPV/SKY messages parse to the expected `GnssFix` / `SatInfo` values. 100% branch coverage on the parse module.

### Phase 2 — gpsd JSON Client

`gpsd/json_client.rs`. TCP connect, VERSION handshake, WATCH subscription, reader task emitting `NexusEvent`. Mock-gpsd integration test validates the protocol exchange.

**Exit criterion:** Mock gpsd test streams 100 TPV messages; all 100 appear as `NexusEvent::GnssTpvReceived` on the event bus in order. (The backend's re-emission as `GnssFixChanged` is exercised in phase 4.)

### Phase 3 — Device Lifecycle State Machine

`lifecycle.rs`, `backend.rs`. `GnssDeviceState` and transitions. Integrate with Interface Monitor events. No profile-store integration yet — use hardcoded defaults.

**Exit criterion:** A simulated `InterfaceDiscovered` + TPV stream transitions the device through Acquiring → Tracking. TPV stall transitions Tracking → Degraded. Acquisition timeout transitions Acquiring → Degraded. Unit tests cover every transition.

### Phase 4 — Quality Filtering and Emission Policy

`fix.rs` `fix_quality_ok`, emission throttling based on `max_update_hz`, `report_movement_only`, heartbeat. Hook into state machine so quality-failing fixes don't advance state.

**Exit criterion:** Emission throttling tests pass. Quality-filter rejection metrics are correctly populated.

### Phase 5 — Profile Store Integration

`profile.rs`. Add `load_gnss` / `load_gnss_profile_by_path` / `put_gnss` / `remove_gnss` to `ProfileStore` trait, plus the `Gnss` variant on `ProfileRef`. Plumb per-device profiles through to the lifecycle.

**Exit criterion:** Per-device profile overrides take effect: a device with a stricter `min_satellites` is filtered accordingly.

### Phase 6 — Reconnection and Supervisor

`backend.rs` reconcile tick, gpsd reconnection, device re-registration after reconnect. `Manager.NotificationEvent` emission on prolonged gpsd outage.

**Exit criterion:** gpsd restart test: mock gpsd drops → reconnects within ≤5 s → all devices are re-registered and resume tracking.

### Phase 7 — Power Management

Handle `PowerState` transitions per §10. Suspend TPV-stall detection and event emission in `sleep`. Cap update rate in `background`.

**Exit criterion:** Cycling through all three power states produces the documented emission behavior.

### Phase 8 — Observability and Hardening

Metrics per §11.2. Structured logs for state transitions. Fault-injection tests from §12.4.

**Exit criterion:** All metrics visible via Prometheus scrape. Soak test (72 hours of replayed gpsd logs) produces no leaks and no unexpected state transitions.

---

## Related Documents

- [Nexus Architecture](./nexus-architecture.md) — Parent, including [ADR-005](./nexus-architecture.md#43-key-architectural-decisions) which pins gpsd as the GNSS data source
- [DD-001: Interface Discovery](./dd-001-interface-discovery.md) — Source of `InterfaceDiscovered` / `InterfaceRemoved` events for GNSS devices; §5.4 details udev-based GNSS discovery
- [DD-006: D-Bus API](./dd-006-dbus-api.md) — §6.5 defines the `fi.nexus.Gnss` interface that consumes `GnssFixChanged` and `GnssSatellites` and exposes `State`, `LastFix`, `SatellitesInView`, `SatellitesUsed`, and related properties/signals
- [DD-007: Profile Store](./dd-007-profile-store.md) — Storage for `GnssDeviceProfile` objects; simpler than Wi-Fi/Ethernet since no credential fields. This DD adds ULID-keyed GNSS methods to the `ProfileStore` trait
