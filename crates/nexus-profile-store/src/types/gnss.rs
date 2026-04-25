//! GNSS device profile. See DD-007 §5.2 note on GNSS/BT profiles
//! and DD-005 §9.
//!
//! GNSS profiles carry no credential material, so a single struct
//! suffices (no dual-struct pattern needed). Every threshold field
//! is `Option`-wrapped: `None` means "fall through to the global
//! `[gnss.defaults]` value", `Some` overrides per-device.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::ProfileMetadata;

/// Fix-dimensionality classification, on-disk and TOML form. The
/// in-memory canonical type is `nexus_core::FixMode`; this
/// representation is only used at the serialization boundary so
/// the snake-case strings in DD-005 §8 round-trip cleanly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FixModeOnDisk {
    #[serde(rename = "no_fix")]
    NoFix,
    #[serde(rename = "fix_2d")]
    Fix2D,
    #[serde(rename = "fix_3d")]
    Fix3D,
}

impl From<FixModeOnDisk> for nexus_core::FixMode {
    fn from(m: FixModeOnDisk) -> Self {
        match m {
            FixModeOnDisk::NoFix => nexus_core::FixMode::NoFix,
            FixModeOnDisk::Fix2D => nexus_core::FixMode::Fix2D,
            FixModeOnDisk::Fix3D => nexus_core::FixMode::Fix3D,
        }
    }
}

impl From<nexus_core::FixMode> for FixModeOnDisk {
    fn from(m: nexus_core::FixMode) -> Self {
        match m {
            nexus_core::FixMode::NoFix => FixModeOnDisk::NoFix,
            nexus_core::FixMode::Fix2D => FixModeOnDisk::Fix2D,
            nexus_core::FixMode::Fix3D => FixModeOnDisk::Fix3D,
        }
    }
}

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
    /// udev `ID_MODEL` snapshot, kept as a stable hint for the UI
    /// even when the kernel device path changes after replug.
    #[serde(default)]
    pub vendor_model: Option<String>,
    /// Per-device override of `max_update_hz`. `None` falls back to
    /// `[gnss.defaults]`. Accepts the legacy `max_rate_hz` key on
    /// disk so older profiles keep loading.
    #[serde(default, alias = "max_rate_hz")]
    pub max_update_hz: Option<u32>,
    /// Per-device override of `max_horizontal_error_m` — fixes whose
    /// `eph` exceeds this value are rejected. Accepts the legacy
    /// `min_horizontal_error_m` key (which had the same semantics
    /// despite the misleading name) so older profiles keep loading.
    #[serde(default, alias = "min_horizontal_error_m")]
    pub max_horizontal_error_m: Option<f64>,
    /// Per-device override of `min_fix_mode`.
    #[serde(default)]
    pub min_fix_mode: Option<FixModeOnDisk>,
    /// Per-device override of `min_satellites`.
    #[serde(default)]
    pub min_satellites: Option<u32>,
    /// Per-device override of `strict_quality`.
    #[serde(default)]
    pub strict_quality: Option<bool>,
    /// Per-device override of `report_movement_only`.
    #[serde(default)]
    pub report_movement_only: Option<bool>,
    /// Per-device override of `movement_threshold_m`.
    #[serde(default)]
    pub movement_threshold_m: Option<f64>,
    /// Per-device override of `heartbeat_interval_s`.
    #[serde(default)]
    pub heartbeat_interval_s: Option<u32>,
    /// Whether to auto-activate this device on discovery (DD-005
    /// §9). Accepts the legacy `auto_attach` key so older profiles
    /// keep loading.
    #[serde(default = "default_true", alias = "auto_attach")]
    pub auto_activate: bool,
}

fn default_true() -> bool {
    true
}
