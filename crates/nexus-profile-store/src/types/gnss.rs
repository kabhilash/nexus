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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_id() -> Ulid {
        Ulid::from_parts(0x0123_4567_89AB, 0xCDEF_0123_4567_89AB_CDEF_0123)
    }

    fn minimal_profile() -> GnssDeviceProfile {
        GnssDeviceProfile {
            id: sample_id(),
            schema_version: 1,
            metadata: ProfileMetadata::default(),
            device_path: "/dev/ttyUSB0".into(),
            label: None,
            vendor_model: None,
            max_update_hz: None,
            max_horizontal_error_m: None,
            min_fix_mode: None,
            min_satellites: None,
            strict_quality: None,
            report_movement_only: None,
            movement_threshold_m: None,
            heartbeat_interval_s: None,
            auto_activate: true,
        }
    }

    // --- FixModeOnDisk From / Serde ------------------------------------

    #[test]
    fn fix_mode_on_disk_to_core_round_trip() {
        for m in [
            FixModeOnDisk::NoFix,
            FixModeOnDisk::Fix2D,
            FixModeOnDisk::Fix3D,
        ] {
            let core: nexus_core::FixMode = m.into();
            let back: FixModeOnDisk = core.into();
            assert_eq!(m, back);
        }
    }

    #[test]
    fn fix_mode_on_disk_serializes_to_snake_case() {
        // Quoted literal — these strings are the DD-005 §8 contract.
        #[derive(serde::Serialize)]
        struct W {
            mode: FixModeOnDisk,
        }
        for (variant, expected) in [
            (FixModeOnDisk::NoFix, "no_fix"),
            (FixModeOnDisk::Fix2D, "fix_2d"),
            (FixModeOnDisk::Fix3D, "fix_3d"),
        ] {
            let text = toml::to_string(&W { mode: variant }).unwrap();
            assert!(
                text.contains(&format!("\"{expected}\"")),
                "{variant:?} serialized to: {text}"
            );
        }
    }

    #[test]
    fn fix_mode_on_disk_deserializes_from_snake_case() {
        #[derive(serde::Deserialize)]
        struct W {
            mode: FixModeOnDisk,
        }
        for (input, expected) in [
            ("no_fix", FixModeOnDisk::NoFix),
            ("fix_2d", FixModeOnDisk::Fix2D),
            ("fix_3d", FixModeOnDisk::Fix3D),
        ] {
            let parsed: W = toml::from_str(&format!("mode = \"{input}\"")).unwrap();
            assert_eq!(parsed.mode, expected);
        }
    }

    // --- GnssDeviceProfile serde --------------------------------------

    #[test]
    fn gnss_round_trip_with_all_fields_none() {
        // Realistic "fall through to global defaults" profile —
        // every Option is None, only the required fields populated.
        let p = minimal_profile();
        let text = toml::to_string(&p).unwrap();
        let back: GnssDeviceProfile = toml::from_str(&text).unwrap();
        assert_eq!(p, back);
    }

    #[test]
    fn legacy_max_rate_hz_alias_loads() {
        let text = format!(
            r#"
id = "{id}"
schema_version = 1
device_path = "/dev/ttyUSB0"
max_rate_hz = 5
auto_activate = true
"#,
            id = sample_id(),
        );
        let p: GnssDeviceProfile = toml::from_str(&text).unwrap();
        assert_eq!(p.max_update_hz, Some(5));
    }

    #[test]
    fn legacy_min_horizontal_error_m_alias_loads() {
        let text = format!(
            r#"
id = "{id}"
schema_version = 1
device_path = "/dev/ttyUSB0"
min_horizontal_error_m = 2.5
auto_activate = true
"#,
            id = sample_id(),
        );
        let p: GnssDeviceProfile = toml::from_str(&text).unwrap();
        assert_eq!(p.max_horizontal_error_m, Some(2.5));
    }

    #[test]
    fn legacy_auto_attach_alias_loads() {
        let text = format!(
            r#"
id = "{id}"
schema_version = 1
device_path = "/dev/ttyUSB0"
auto_attach = false
"#,
            id = sample_id(),
        );
        let p: GnssDeviceProfile = toml::from_str(&text).unwrap();
        assert!(!p.auto_activate);
    }

    #[test]
    fn auto_activate_defaults_to_true_when_absent() {
        let text = format!(
            r#"
id = "{id}"
schema_version = 1
device_path = "/dev/ttyUSB0"
"#,
            id = sample_id(),
        );
        let p: GnssDeviceProfile = toml::from_str(&text).unwrap();
        assert!(p.auto_activate);
    }

    #[test]
    fn metadata_defaults_when_absent() {
        let text = format!(
            r#"
id = "{id}"
schema_version = 1
device_path = "/dev/ttyUSB0"
"#,
            id = sample_id(),
        );
        let p: GnssDeviceProfile = toml::from_str(&text).unwrap();
        assert_eq!(p.metadata, ProfileMetadata::default());
    }
}
