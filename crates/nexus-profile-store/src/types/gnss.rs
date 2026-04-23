//! GNSS device profile. See DD-007 §5.2 note on GNSS/BT profiles
//! and DD-005 §9.
//!
//! Forward-declared here — the canonical field set lands in
//! `nexus-gnss` once that crate is implemented. The Profile Store's
//! responsibility is serialization only; the backend decides what
//! the tuning knobs mean.
//!
//! GNSS profiles carry no credential material, so a single struct
//! suffices (no dual-struct pattern needed).

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::ProfileMetadata;

/// Per-device GNSS configuration. ULID-keyed on disk because device
/// paths aren't stable across USB replug (see DD-005 §9.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GnssDeviceProfile {
    pub id: Ulid,
    pub schema_version: u32,
    #[serde(default)]
    pub metadata: ProfileMetadata,
    /// Device node path as matched at enumerate time, e.g.
    /// `/dev/ttyUSB0`. Stored so operators can recognize the
    /// profile; matching logic in DD-005 §9.2 tolerates replug-
    /// induced churn.
    pub device_path: String,
    /// Optional operator-friendly name (e.g. `"external u-blox F9P"`).
    #[serde(default)]
    pub label: Option<String>,
    /// Maximum fix rate to accept from this device, in Hz. `None`
    /// means no rate cap.
    #[serde(default)]
    pub max_rate_hz: Option<u32>,
    /// Minimum horizontal-error threshold below which fixes are
    /// dropped, in meters. `None` accepts any error.
    #[serde(default)]
    pub min_horizontal_error_m: Option<f64>,
    /// Whether this device should auto-attach when discovered.
    #[serde(default = "default_true")]
    pub auto_attach: bool,
}

fn default_true() -> bool {
    true
}
