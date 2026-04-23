//! Fix types, quality filter, and emission policy. See DD-005 §7.
//!
//! [`nexus_core::GnssFix`] / [`nexus_core::FixMode`] / [`nexus_core::SatInfo`]
//! are the canonical wire types. Re-exported here so downstream
//! crates only depend on `nexus-gnss`.

pub use nexus_core::{FixMode, GnssFix, SatInfo};

/// In-memory effective thresholds for a device — the merge of the
/// global [`crate::GnssConfig::defaults`] and the on-disk
/// [`nexus_profile_store::GnssDeviceProfile`]. The on-disk profile
/// has a deliberately small field set (DD-007); this struct carries
/// the full DD-005 §9 threshold vocabulary.
#[derive(Debug, Clone)]
pub struct EffectiveProfile {
    /// Kernel device path the entry is keyed by (e.g.
    /// `/dev/ttyUSB0`).
    pub device_path: String,
    /// Operator-friendly name, if set.
    pub label: Option<String>,
    /// Minimum dimensionality a fix must reach to pass.
    pub min_fix_mode: FixMode,
    /// Minimum satellites-used count.
    pub min_satellites: u32,
    /// Upper bound on `horizontal_error_m`. `None` means no bound.
    pub max_horizontal_error_m: Option<f64>,
    /// If set, a fix without `horizontal_error_m` is rejected
    /// whenever `max_horizontal_error_m` is set.
    pub strict_quality: bool,
    /// Max emissions per second (>= 1). See DD-005 §7.3.
    pub max_update_hz: u32,
    /// If true, emit only when the fix has moved farther than
    /// `movement_threshold_m` or `heartbeat_interval_s` has elapsed.
    pub report_movement_only: bool,
    pub movement_threshold_m: f64,
    pub heartbeat_interval_s: u32,
    /// If true, ask gpsd to watch the device on discovery. Default
    /// true; mirrors DD-005 §9 `auto_activate`.
    pub auto_activate: bool,
}

impl EffectiveProfile {
    /// Defaults straight from DD-005 §8.
    pub fn defaults_for(device_path: impl Into<String>) -> Self {
        Self {
            device_path: device_path.into(),
            label: None,
            min_fix_mode: FixMode::Fix2D,
            min_satellites: 4,
            max_horizontal_error_m: Some(100.0),
            strict_quality: false,
            max_update_hz: 1,
            report_movement_only: false,
            movement_threshold_m: 10.0,
            heartbeat_interval_s: 60,
            auto_activate: true,
        }
    }
}

/// Rejection reason for a fix, matched against the thresholds.
/// Used for the `nexus_gnss_fixes_filtered_total{reason}` metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterReason {
    /// Fix mode below `min_fix_mode`.
    Mode,
    /// Satellites-used below `min_satellites`.
    Satellites,
    /// Horizontal error exceeds (or is missing under `strict_quality`)
    /// `max_horizontal_error_m`.
    HorizontalError,
}

impl FilterReason {
    pub fn as_str(self) -> &'static str {
        match self {
            FilterReason::Mode => "mode",
            FilterReason::Satellites => "satellites",
            FilterReason::HorizontalError => "horizontal_error",
        }
    }
}

/// Apply the DD-005 §7.2 quality threshold. Returns `Ok(())` when
/// the fix passes or a structured [`FilterReason`] explaining why
/// it didn't.
pub fn fix_quality_ok(
    fix: &GnssFix,
    profile: &EffectiveProfile,
) -> std::result::Result<(), FilterReason> {
    let mode_ok = match profile.min_fix_mode {
        FixMode::NoFix => true,
        FixMode::Fix2D => matches!(fix.mode, FixMode::Fix2D | FixMode::Fix3D),
        FixMode::Fix3D => matches!(fix.mode, FixMode::Fix3D),
    };
    if !mode_ok {
        return Err(FilterReason::Mode);
    }
    if fix.satellites_used < profile.min_satellites {
        return Err(FilterReason::Satellites);
    }
    if let Some(max) = profile.max_horizontal_error_m {
        match fix.horizontal_error_m {
            Some(eph) if eph > max => return Err(FilterReason::HorizontalError),
            None if profile.strict_quality => return Err(FilterReason::HorizontalError),
            _ => {}
        }
    }
    Ok(())
}

/// Great-circle (haversine) distance in meters between two
/// fixes. Returns `None` when either fix lacks a lat/lon.
pub fn great_circle_distance_m(a: &GnssFix, b: &GnssFix) -> Option<f64> {
    let (lat1, lon1) = (a.latitude?, a.longitude?);
    let (lat2, lon2) = (b.latitude?, b.longitude?);
    let r_earth = 6_371_000.0_f64; // mean earth radius, m
    let to_rad = std::f64::consts::PI / 180.0;
    let (phi1, phi2) = (lat1 * to_rad, lat2 * to_rad);
    let dphi = (lat2 - lat1) * to_rad;
    let dlambda = (lon2 - lon1) * to_rad;
    let a = (dphi / 2.0).sin().powi(2) + phi1.cos() * phi2.cos() * (dlambda / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    Some(r_earth * c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_fix() -> GnssFix {
        GnssFix {
            time: chrono::Utc::now(),
            mode: FixMode::Fix3D,
            latitude: Some(37.0),
            longitude: Some(-122.0),
            altitude_m: Some(50.0),
            speed_mps: None,
            track_deg: None,
            horizontal_error_m: Some(5.0),
            vertical_error_m: Some(10.0),
            satellites_used: 8,
        }
    }

    #[test]
    fn passes_defaults() {
        let p = EffectiveProfile::defaults_for("/dev/ttyS0");
        assert!(fix_quality_ok(&base_fix(), &p).is_ok());
    }

    #[test]
    fn rejects_no_fix() {
        let mut f = base_fix();
        f.mode = FixMode::NoFix;
        let p = EffectiveProfile::defaults_for("/dev/ttyS0");
        assert_eq!(fix_quality_ok(&f, &p), Err(FilterReason::Mode));
    }

    #[test]
    fn rejects_2d_when_3d_required() {
        let mut f = base_fix();
        f.mode = FixMode::Fix2D;
        let mut p = EffectiveProfile::defaults_for("/dev/ttyS0");
        p.min_fix_mode = FixMode::Fix3D;
        assert_eq!(fix_quality_ok(&f, &p), Err(FilterReason::Mode));
    }

    #[test]
    fn rejects_too_few_satellites() {
        let mut f = base_fix();
        f.satellites_used = 3;
        let p = EffectiveProfile::defaults_for("/dev/ttyS0");
        assert_eq!(fix_quality_ok(&f, &p), Err(FilterReason::Satellites));
    }

    #[test]
    fn rejects_large_eph() {
        let mut f = base_fix();
        f.horizontal_error_m = Some(250.0);
        let p = EffectiveProfile::defaults_for("/dev/ttyS0");
        assert_eq!(fix_quality_ok(&f, &p), Err(FilterReason::HorizontalError));
    }

    #[test]
    fn lenient_missing_eph_passes_by_default() {
        let mut f = base_fix();
        f.horizontal_error_m = None;
        let p = EffectiveProfile::defaults_for("/dev/ttyS0");
        assert!(fix_quality_ok(&f, &p).is_ok());
    }

    #[test]
    fn strict_missing_eph_rejects() {
        let mut f = base_fix();
        f.horizontal_error_m = None;
        let mut p = EffectiveProfile::defaults_for("/dev/ttyS0");
        p.strict_quality = true;
        assert_eq!(fix_quality_ok(&f, &p), Err(FilterReason::HorizontalError));
    }

    #[test]
    fn great_circle_distance_returns_some_for_positioned_fixes() {
        let mut a = base_fix();
        let mut b = base_fix();
        b.latitude = Some(37.001);
        b.longitude = Some(-122.001);
        let d = great_circle_distance_m(&a, &b).unwrap();
        // Roughly 0.001 deg lat + 0.001 deg lon ~ 130 m at 37N.
        assert!(d > 50.0 && d < 300.0, "got {d}");
        a.latitude = None;
        assert!(great_circle_distance_m(&a, &b).is_none());
    }
}
