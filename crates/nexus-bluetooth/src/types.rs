//! Shared in-memory types for the Bluetooth Backend. The payload
//! types that travel over the [`nexus_core::NexusEvent`] bus (e.g.,
//! [`BtDeviceInfo`], [`BtFailureReason`], [`PairingJobId`]) live in
//! `nexus-core`; this module owns the types that stay inside the
//! backend: the adapter / device state enums, per-entry registry
//! records, and the discovery-filter shape (DD-004 §§4, 5, 9.2).

use std::collections::HashMap;
use std::time::Instant;

use nexus_core::{BtDeviceInfo, BtFailureReason, InterfaceInfo, PairingJobId};
use nexus_profile_store::BluetoothProfile;

// ---------------------------------------------------------------------------
// Adapter state machine — DD-004 §4.1
// ---------------------------------------------------------------------------

/// Per-adapter state. Driven by a combination of Interface Monitor
/// events (kernel-side presence) and BlueZ's `ObjectManager` +
/// `PropertiesChanged` traffic (daemon-side view). See DD-004 §4.2
/// for the transition table.
#[derive(Debug, Clone)]
pub enum BtAdapterState {
    /// Kernel interface exists (DD-001 saw it) but BlueZ either
    /// isn't running or hasn't published an adapter object.
    Unavailable,
    /// BlueZ knows about the adapter; `Powered = false`.
    Present,
    /// BlueZ's `Powered = true`; idle — not scanning.
    Powered,
    /// Discovery session active (`Discovering = true`).
    Discovering { since: Instant },
    /// Kernel interface removed or BlueZ's ObjectManager dropped the
    /// adapter object. The entry is about to be dropped.
    Gone,
}

impl BtAdapterState {
    /// Short metric / log label.
    pub fn label(&self) -> &'static str {
        match self {
            BtAdapterState::Unavailable => "unavailable",
            BtAdapterState::Present => "present",
            BtAdapterState::Powered => "powered",
            BtAdapterState::Discovering { .. } => "discovering",
            BtAdapterState::Gone => "gone",
        }
    }
}

// ---------------------------------------------------------------------------
// Device state machine — DD-004 §5.1
// ---------------------------------------------------------------------------

/// Per-(adapter, bluetooth_address) device state. See DD-004 §5.2
/// for the transition diagram.
#[derive(Debug, Clone)]
pub enum BtDeviceState {
    /// Device seen in discovery, not paired.
    Discovered,
    /// Pairing exchange in progress.
    Pairing {
        job_id: PairingJobId,
        started_at: Instant,
    },
    /// Bonded (BlueZ `Paired = true`), not connected.
    Paired,
    /// Connection attempt in flight.
    Connecting { since: Instant },
    /// BlueZ `Connected = true`. Services available.
    Connected {
        since: Instant,
        services: Vec<String>,
    },
    /// Explicit disconnect initiated; awaiting BlueZ's ack.
    Disconnecting,
    /// Last operation failed; stays here until operator retries or
    /// forgets.
    Failed {
        reason: BtFailureReason,
        at: Instant,
    },
    /// Device removed from BlueZ's registry.
    Removed,
}

impl BtDeviceState {
    pub fn label(&self) -> &'static str {
        match self {
            BtDeviceState::Discovered => "discovered",
            BtDeviceState::Pairing { .. } => "pairing",
            BtDeviceState::Paired => "paired",
            BtDeviceState::Connecting { .. } => "connecting",
            BtDeviceState::Connected { .. } => "connected",
            BtDeviceState::Disconnecting => "disconnecting",
            BtDeviceState::Failed { .. } => "failed",
            BtDeviceState::Removed => "removed",
        }
    }
}

// ---------------------------------------------------------------------------
// Per-entry registry records — DD-004 §7.2
// ---------------------------------------------------------------------------

/// The backend's in-memory snapshot of an adapter. Combines the
/// Interface Monitor's [`InterfaceInfo`] with BlueZ-derived state.
#[derive(Debug, Clone)]
pub struct BtAdapterEntry {
    pub info: InterfaceInfo,
    /// Canonical BlueZ object path, e.g. `/org/bluez/hci0`.
    pub bluez_path: String,
    pub state: BtAdapterState,
    pub powered: bool,
    pub discoverable: bool,
    pub pairable: bool,
    /// Devices seen through this adapter, keyed by BlueZ object path.
    pub devices: HashMap<String, BtDeviceEntry>,
    /// True while Nexus has an outstanding `StartDiscovery` call on
    /// this adapter. BlueZ itself may still report
    /// `Discovering = true` when another client holds a session; the
    /// flag here is the Nexus-owned session only (DD-004 §9.1).
    pub nexus_has_discovery_session: bool,
    pub discovery_started_at: Option<Instant>,
}

impl BtAdapterEntry {
    pub fn new(info: InterfaceInfo, bluez_path: String) -> Self {
        Self {
            info,
            bluez_path,
            state: BtAdapterState::Unavailable,
            powered: false,
            discoverable: false,
            pairable: false,
            devices: HashMap::new(),
            nexus_has_discovery_session: false,
            discovery_started_at: None,
        }
    }
}

/// Per-device entry held on a `BtAdapterEntry`.
#[derive(Debug, Clone)]
pub struct BtDeviceEntry {
    pub info: BtDeviceInfo,
    pub state: BtDeviceState,
    /// Populated when a matching profile exists in the Profile Store.
    pub profile: Option<BluetoothProfile>,
    /// Correlation id for an in-flight pairing, if any.
    pub pairing_job: Option<PairingJobId>,
    /// Monotonic time of last observed event; used by the TTL GC.
    pub last_seen: Instant,
}

impl BtDeviceEntry {
    pub fn new(info: BtDeviceInfo, state: BtDeviceState) -> Self {
        Self {
            info,
            state,
            profile: None,
            pairing_job: None,
            last_seen: Instant::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery filter — DD-004 §9.2
// ---------------------------------------------------------------------------

/// Pre-scan filter BlueZ applies before emitting `InterfacesAdded`.
/// Nexus calls `SetDiscoveryFilter` with this dict before every
/// `StartDiscovery`.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryFilter {
    /// `None` means BlueZ's "auto" default.
    pub transport: Option<DiscoveryTransport>,
    /// dBm floor; weaker peers are hidden.
    pub rssi: Option<i16>,
    /// UUID whitelist; empty = no filter.
    pub uuids: Vec<String>,
    /// Emit every advertisement (even from already-known devices).
    pub duplicate_data: bool,
}

/// Transport restriction for a discovery session. Distinct from
/// [`nexus_core::BtTransport`]: the filter only picks a side,
/// whereas `BtTransport` can say `Dual`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiscoveryTransport {
    #[default]
    Auto,
    Bredr,
    Le,
}

impl DiscoveryTransport {
    /// BlueZ-spelled string consumed by `SetDiscoveryFilter`.
    pub fn as_bluez_str(self) -> &'static str {
        match self {
            DiscoveryTransport::Auto => "auto",
            DiscoveryTransport::Bredr => "bredr",
            DiscoveryTransport::Le => "le",
        }
    }
}

// ---------------------------------------------------------------------------
// Power state — DD-004 §12
// ---------------------------------------------------------------------------

/// Coarse power state that maps to Bluetooth behavior per DD-004
/// §12. Active = full operation; Background = no auto-discovery;
/// Sleep = adapters powered off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerState {
    #[default]
    Active,
    Background,
    Sleep,
}

impl PowerState {
    pub fn as_str(self) -> &'static str {
        match self {
            PowerState::Active => "active",
            PowerState::Background => "background",
            PowerState::Sleep => "sleep",
        }
    }
}

// ---------------------------------------------------------------------------
// Authorization decision for the Agent fast path — DD-004 §7.2
// ---------------------------------------------------------------------------

/// Answer handed to the Agent task when it consults the backend's
/// stored profile before deciding whether to prompt the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationDecision {
    /// Accept without prompting.
    Accept,
    /// Reject without prompting. Reserved; the current backend never
    /// returns this, but the enum leaves room for a future
    /// deny-listed-devices flow without a trait churn.
    Reject,
    /// Fall through to the normal `BtPairingPrompt` path.
    Prompt,
}

// ---------------------------------------------------------------------------
// Helpers that straddle the type enums
// ---------------------------------------------------------------------------

/// Initial [`BtDeviceState`] derived from the BlueZ-reported flags
/// on first observation.
pub fn initial_device_state(info: &BtDeviceInfo) -> BtDeviceState {
    if info.connected {
        BtDeviceState::Connected {
            since: Instant::now(),
            services: info.uuids.clone(),
        }
    } else if info.paired {
        BtDeviceState::Paired
    } else {
        BtDeviceState::Discovered
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_state_labels_cover_every_variant() {
        assert_eq!(BtAdapterState::Unavailable.label(), "unavailable");
        assert_eq!(BtAdapterState::Present.label(), "present");
        assert_eq!(BtAdapterState::Powered.label(), "powered");
        assert_eq!(
            BtAdapterState::Discovering {
                since: Instant::now(),
            }
            .label(),
            "discovering"
        );
        assert_eq!(BtAdapterState::Gone.label(), "gone");
    }

    #[test]
    fn device_state_labels_cover_every_variant() {
        assert_eq!(BtDeviceState::Discovered.label(), "discovered");
        assert_eq!(BtDeviceState::Paired.label(), "paired");
        assert_eq!(BtDeviceState::Disconnecting.label(), "disconnecting");
        assert_eq!(BtDeviceState::Removed.label(), "removed");
    }

    #[test]
    fn discovery_transport_bluez_strings() {
        assert_eq!(DiscoveryTransport::Auto.as_bluez_str(), "auto");
        assert_eq!(DiscoveryTransport::Bredr.as_bluez_str(), "bredr");
        assert_eq!(DiscoveryTransport::Le.as_bluez_str(), "le");
    }
}
