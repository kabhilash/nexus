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
/// produce the in-memory `EffectiveProfile`.
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
        eff.label = p.label.clone();
        eff.auto_activate = p.auto_attach;
        if let Some(hz) = p.max_rate_hz {
            eff.max_update_hz = hz.max(1);
        }
        if let Some(m) = p.min_horizontal_error_m {
            eff.max_horizontal_error_m = Some(m);
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
        max_rate_hz: Some(eff.max_update_hz),
        min_horizontal_error_m: eff.max_horizontal_error_m,
        auto_attach: eff.auto_activate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::FixMode;

    #[test]
    fn stored_max_rate_overrides_default() {
        let defaults = GnssDefaults {
            max_update_hz: 1,
            ..GnssDefaults::default()
        };
        let stored = GnssDeviceProfile {
            id: Ulid::new(),
            schema_version: 1,
            metadata: ProfileMetadata::default(),
            device_path: "/dev/ttyS0".into(),
            label: Some("ublox".into()),
            max_rate_hz: Some(5),
            min_horizontal_error_m: Some(20.0),
            auto_attach: false,
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
}
