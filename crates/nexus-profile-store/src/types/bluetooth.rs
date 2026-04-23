//! Bluetooth bond profile. See DD-007 §5.2 and DD-004 §8.3.
//!
//! Forward-declared here — the canonical field set lands in
//! `nexus-bluetooth` once that crate is implemented. BlueZ keeps
//! link keys in its own on-disk store; Nexus stores only
//! operator-visible preferences and a bond-adapter tag. No
//! credential material → single struct, directly Serialize.

use nexus_core::MacAddr;
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::ProfileMetadata;

/// Per-bond Bluetooth profile. ULID-keyed on disk so a single peer
/// can be bonded to multiple adapters without filename collision
/// (DD-004 §8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BluetoothProfile {
    pub id: Ulid,
    pub schema_version: u32,
    #[serde(default)]
    pub metadata: ProfileMetadata,
    /// BlueZ adapter object path, e.g. `/org/bluez/hci0`. Scopes
    /// this bond to one adapter so multi-adapter devices stay
    /// distinguishable.
    pub adapter_path: String,
    /// Peer device address.
    pub device_address: MacAddr,
    /// Operator-friendly name pulled from the device's GAP record,
    /// if available.
    #[serde(default)]
    pub device_name: Option<String>,
    /// Automatically reconnect when the device comes in range.
    #[serde(default)]
    pub auto_connect: bool,
    /// Auto-accept incoming service-level connection requests for
    /// this already-paired device.
    #[serde(default)]
    pub auto_accept_incoming: bool,
    /// Local preferences vector — reserved for per-profile
    /// overrides (HFP vs. A2DP preference, etc.). Stored as a free
    /// string map so the Bluetooth backend can extend it without
    /// requiring a schema migration.
    #[serde(default)]
    pub preferences: std::collections::BTreeMap<String, String>,
}
