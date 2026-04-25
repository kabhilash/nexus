//! Profile hydration. See DD-005 §9.
//!
//! The on-disk [`nexus_profile_store::GnssDeviceProfile`] has a
//! deliberately narrow field set (DD-007 keeps credential-free
//! profiles simple). The in-memory [`crate::fix::EffectiveProfile`]
//! carries the richer DD-005 threshold vocabulary. This module
//! merges the two with the global [`crate::GnssConfig::defaults`].

use chrono::Utc;
use nexus_profile_store::{GnssDeviceProfile, ProfileMetadata};
use ulid::Ulid;

use crate::config::GnssDefaults;
use crate::fix::EffectiveProfile;

/// Merge the global defaults with an optional on-disk profile and
/// produce the in-memory `EffectiveProfile`. Every threshold the
/// stored profile leaves as `None` falls through to the global
/// defaults; everything else overrides per-device.
pub fn hydrate(
    device_path: &str,
    stored: Option<&GnssDeviceProfile>,
    defaults: &GnssDefaults,
) -> EffectiveProfile {
    let mut eff = EffectiveProfile::defaults_for(device_path);
    eff.min_fix_mode = defaults.min_fix_mode;
    eff.min_satellites = defaults.min_satellites;
    eff.max_horizontal_error_m = defaults.max_horizontal_error_m;
    eff.strict_quality = defaults.strict_quality;
    eff.max_update_hz = defaults.max_update_hz.max(1);
    eff.report_movement_only = defaults.report_movement_only;
    eff.movement_threshold_m = defaults.movement_threshold_m;
    eff.heartbeat_interval_s = defaults.heartbeat_interval_s;

    if let Some(p) = stored {
        eff.label = p.label.clone().or_else(|| p.vendor_model.clone());
        eff.auto_activate = p.auto_activate;
        if let Some(hz) = p.max_update_hz {
            eff.max_update_hz = hz.max(1);
        }
        if let Some(m) = p.max_horizontal_error_m {
            eff.max_horizontal_error_m = Some(m);
        }
        if let Some(mode) = p.min_fix_mode {
            eff.min_fix_mode = mode.into();
        }
        if let Some(n) = p.min_satellites {
            eff.min_satellites = n;
        }
        if let Some(strict) = p.strict_quality {
            eff.strict_quality = strict;
        }
        if let Some(b) = p.report_movement_only {
            eff.report_movement_only = b;
        }
        if let Some(m) = p.movement_threshold_m {
            eff.movement_threshold_m = m;
        }
        if let Some(s) = p.heartbeat_interval_s {
            eff.heartbeat_interval_s = s;
        }
    }
    eff
}

/// Build a fresh on-disk profile from an effective profile. Used
/// when an operator first configures a device (future D-Bus
/// Update path); v0.1 just supports reading.
pub fn build_stored_profile(eff: &EffectiveProfile) -> GnssDeviceProfile {
    let now = Utc::now();
    GnssDeviceProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata {
            created_at: Some(now),
            updated_at: Some(now),
            label: eff.label.clone(),
        },
        device_path: eff.device_path.clone(),
        label: eff.label.clone(),
        vendor_model: None,
        max_update_hz: Some(eff.max_update_hz),
        max_horizontal_error_m: eff.max_horizontal_error_m,
        min_fix_mode: Some(eff.min_fix_mode.into()),
        min_satellites: Some(eff.min_satellites),
        strict_quality: Some(eff.strict_quality),
        report_movement_only: Some(eff.report_movement_only),
        movement_threshold_m: Some(eff.movement_threshold_m),
        heartbeat_interval_s: Some(eff.heartbeat_interval_s),
        auto_activate: eff.auto_activate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::FixMode;
    use nexus_profile_store::FixModeOnDisk;

    fn empty_stored() -> GnssDeviceProfile {
        GnssDeviceProfile {
            id: Ulid::new(),
            schema_version: 1,
            metadata: ProfileMetadata::default(),
            device_path: "/dev/ttyS0".into(),
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

    #[test]
    fn stored_max_rate_overrides_default() {
        let defaults = GnssDefaults {
            max_update_hz: 1,
            ..GnssDefaults::default()
        };
        let stored = GnssDeviceProfile {
            label: Some("ublox".into()),
            max_update_hz: Some(5),
            max_horizontal_error_m: Some(20.0),
            auto_activate: false,
            ..empty_stored()
        };
        let eff = hydrate("/dev/ttyS0", Some(&stored), &defaults);
        assert_eq!(eff.max_update_hz, 5);
        assert_eq!(eff.max_horizontal_error_m, Some(20.0));
        assert_eq!(eff.label.as_deref(), Some("ublox"));
        assert!(!eff.auto_activate);
    }

    #[test]
    fn zero_max_rate_clamps_to_one() {
        let defaults = GnssDefaults {
            max_update_hz: 0,
            ..GnssDefaults::default()
        };
        let eff = hydrate("/dev/ttyS0", None, &defaults);
        assert_eq!(eff.max_update_hz, 1);
    }

    #[test]
    fn missing_stored_uses_defaults() {
        let defaults = GnssDefaults {
            min_fix_mode: FixMode::Fix3D,
            ..GnssDefaults::default()
        };
        let eff = hydrate("/dev/ttyS0", None, &defaults);
        assert_eq!(eff.min_fix_mode, FixMode::Fix3D);
        assert!(eff.auto_activate);
    }

    #[test]
    fn legacy_field_aliases_still_load() {
        // Profiles written before the K1 rename used max_rate_hz /
        // min_horizontal_error_m / auto_attach. They must still
        // deserialize after the rename — the type's serde aliases
        // are the only thing keeping that promise.
        let json = serde_json::json!({
            "id": Ulid::new().to_string(),
            "schema_version": 1,
            "device_path": "/dev/ttyS0",
            "max_rate_hz": 5,
            "min_horizontal_error_m": 25.0,
            "auto_attach": false,
        });
        let parsed: GnssDeviceProfile = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.max_update_hz, Some(5));
        assert_eq!(parsed.max_horizontal_error_m, Some(25.0));
        assert!(!parsed.auto_activate);
    }

    #[test]
    fn stored_overrides_every_threshold() {
        let defaults = GnssDefaults::default();
        let stored = GnssDeviceProfile {
            min_fix_mode: Some(FixModeOnDisk::Fix3D),
            min_satellites: Some(7),
            strict_quality: Some(true),
            report_movement_only: Some(true),
            movement_threshold_m: Some(2.5),
            heartbeat_interval_s: Some(15),
            vendor_model: Some("u-blox F9P".into()),
            ..empty_stored()
        };
        let eff = hydrate("/dev/ttyS0", Some(&stored), &defaults);
        assert_eq!(eff.min_fix_mode, FixMode::Fix3D);
        assert_eq!(eff.min_satellites, 7);
        assert!(eff.strict_quality);
        assert!(eff.report_movement_only);
        assert_eq!(eff.movement_threshold_m, 2.5);
        assert_eq!(eff.heartbeat_interval_s, 15);
        // vendor_model surfaces as the label when no explicit label
        // is set on the stored profile.
        assert_eq!(eff.label.as_deref(), Some("u-blox F9P"));
    }
}
