//! Per-device GNSS state machine. See DD-005 §§3, 5.
//!
//! The state enum and `next_state` transitions are pure — the
//! backend owns the map of [`GnssDeviceEntry`] records and drives
//! the side effects (emitting `GnssFixChanged`, updating metrics).

use std::time::{Duration, Instant};

use nexus_core::InterfaceInfo;

use crate::config::PowerState;
use crate::fix::{EffectiveProfile, GnssFix, SatInfo};

/// Background-mode emission floor per DD-005 §10: at most one
/// `GnssFixChanged` every 5 s regardless of the per-device profile.
pub const BACKGROUND_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Per-device lifecycle state (DD-005 §3.1).
#[derive(Debug, Clone)]
pub enum GnssDeviceState {
    /// Discovered and registered with gpsd; waiting for the first
    /// quality-passing fix.
    Acquiring { since: Instant },
    /// Steady state: receiving quality-passing fixes.
    Tracking {
        last_fix: GnssFix,
        last_fix_at: Instant,
    },
    /// Was tracking; quality degraded or TPV stream stalled.
    Degraded {
        last_good_fix: Option<GnssFix>,
        since: Instant,
    },
    /// About to be dropped.
    Gone,
}

impl GnssDeviceState {
    pub fn label(&self) -> &'static str {
        match self {
            GnssDeviceState::Acquiring { .. } => "acquiring",
            GnssDeviceState::Tracking { .. } => "tracking",
            GnssDeviceState::Degraded { .. } => "degraded",
            GnssDeviceState::Gone => "gone",
        }
    }
}

/// In-memory bookkeeping for one GNSS device.
#[derive(Debug, Clone)]
pub struct GnssDeviceEntry {
    pub info: InterfaceInfo,
    pub profile: EffectiveProfile,
    pub state: GnssDeviceState,
    /// Last TPV arrival (either passing or failing). Used for the
    /// TPV-stall timeout. Cleared / frozen while `PowerState::Sleep`.
    pub last_tpv_at: Option<Instant>,
    /// Last time `GnssFixChanged` was emitted for this device.
    pub last_fix_emit_at: Option<Instant>,
    /// Content of the last emitted fix, for movement-threshold
    /// comparison.
    pub last_emitted_fix: Option<GnssFix>,
    /// Most recent SKY snapshot (D-Bus consumers read this).
    pub last_satellites: Vec<SatInfo>,
}

impl GnssDeviceEntry {
    pub fn new(info: InterfaceInfo, profile: EffectiveProfile, now: Instant) -> Self {
        Self {
            info,
            profile,
            state: GnssDeviceState::Acquiring { since: now },
            last_tpv_at: None,
            last_fix_emit_at: None,
            last_emitted_fix: None,
            last_satellites: Vec::new(),
        }
    }

    /// Device path pulled out of the [`InterfaceInfo`]. Returns an
    /// empty string when the kind isn't `Gnss`, which should never
    /// happen in practice.
    pub fn device_path(&self) -> &str {
        match &self.info.kind {
            nexus_core::InterfaceKind::Gnss { device_path, .. } => device_path,
            _ => "",
        }
    }
}

/// TPV-driven transition. See DD-005 §5.2 `on_tpv`.
///
/// `passes` indicates whether the fix cleared the quality filter.
/// Returns the new state (possibly unchanged).
pub fn tpv_next_state(
    current: &GnssDeviceState,
    fix: &GnssFix,
    passes: bool,
    now: Instant,
) -> GnssDeviceState {
    use GnssDeviceState as S;
    match (current, passes) {
        // Quality-passing fix from Acquiring / Degraded / Tracking —
        // advance or stay in Tracking.
        (_, true) => S::Tracking {
            last_fix: fix.clone(),
            last_fix_at: now,
        },
        // Quality-failing fix while Tracking — degrade, holding on
        // to the last good fix.
        (S::Tracking { last_fix, .. }, false) => S::Degraded {
            last_good_fix: Some(last_fix.clone()),
            since: now,
        },
        // Failing fix in any other state leaves the state alone —
        // the acquisition / stall timers handle progression.
        (other, false) => other.clone(),
    }
}

/// Timer-driven transitions. See DD-005 §5.2 `check_timeouts`.
pub fn check_timeouts(
    current: &GnssDeviceState,
    last_tpv_at: Option<Instant>,
    acq_timeout: Duration,
    stall_timeout: Duration,
    now: Instant,
) -> Option<GnssDeviceState> {
    match current {
        GnssDeviceState::Acquiring { since } => {
            if now.saturating_duration_since(*since) >= acq_timeout {
                Some(GnssDeviceState::Degraded {
                    last_good_fix: None,
                    since: now,
                })
            } else {
                None
            }
        }
        GnssDeviceState::Tracking { last_fix, .. } => {
            if let Some(t) = last_tpv_at {
                if now.saturating_duration_since(t) >= stall_timeout {
                    return Some(GnssDeviceState::Degraded {
                        last_good_fix: Some(last_fix.clone()),
                        since: now,
                    });
                }
            }
            None
        }
        _ => None,
    }
}

/// Emission policy — DD-005 §§7.3, 10. Returns `true` when the
/// backend should actually emit `GnssFixChanged` for this `fix`.
///
/// The `power_state` argument enforces the §10 background-mode
/// floor: when `Background`, the effective rate cap is the
/// stricter of the per-device profile and the 5-second
/// [`BACKGROUND_MIN_INTERVAL`]. Sleep mode is handled upstream
/// (the backend short-circuits before reaching here).
pub fn should_emit(
    entry: &GnssDeviceEntry,
    fix: &GnssFix,
    now: Instant,
    power_state: PowerState,
) -> Outcome {
    let profile = &entry.profile;

    // Rate cap first: at most one emission per `1/max_update_hz`
    // seconds. `max_update_hz.max(1)` defends against a 0 sneaking
    // in from the TOML. In `Background` we additionally enforce a
    // 5 s floor regardless of the profile (DD-005 §10).
    if let Some(last) = entry.last_fix_emit_at {
        let profile_interval =
            Duration::from_secs_f32(1.0 / profile.max_update_hz.max(1) as f32);
        let min_interval = match power_state {
            PowerState::Background => profile_interval.max(BACKGROUND_MIN_INTERVAL),
            _ => profile_interval,
        };
        if now.saturating_duration_since(last) < min_interval {
            return Outcome::Suppressed(SuppressReason::RateLimit);
        }
    }

    if !profile.report_movement_only {
        return Outcome::Emit;
    }

    // Stationary optimization: only emit when we've moved past the
    // threshold, or the heartbeat interval has elapsed.
    match (&entry.last_emitted_fix, entry.last_fix_emit_at) {
        (Some(prev), Some(last_emit)) => {
            let moved = crate::fix::great_circle_distance_m(prev, fix)
                .is_some_and(|d| d >= profile.movement_threshold_m);
            let heartbeat_due = now.saturating_duration_since(last_emit)
                >= Duration::from_secs(profile.heartbeat_interval_s as u64);
            if moved || heartbeat_due {
                Outcome::Emit
            } else {
                Outcome::Suppressed(SuppressReason::MovementThreshold)
            }
        }
        // No prior emission → unconditional emit.
        _ => Outcome::Emit,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Emit,
    Suppressed(SuppressReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuppressReason {
    RateLimit,
    MovementThreshold,
}

impl SuppressReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SuppressReason::RateLimit => "rate_limit",
            SuppressReason::MovementThreshold => "movement_threshold",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fix::FixMode;

    fn good_fix() -> GnssFix {
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
    fn acquiring_to_tracking_on_passing_fix() {
        let s0 = GnssDeviceState::Acquiring {
            since: Instant::now(),
        };
        let next = tpv_next_state(&s0, &good_fix(), true, Instant::now());
        assert!(matches!(next, GnssDeviceState::Tracking { .. }));
    }

    #[test]
    fn tracking_to_degraded_on_failing_fix() {
        let s0 = GnssDeviceState::Tracking {
            last_fix: good_fix(),
            last_fix_at: Instant::now(),
        };
        let next = tpv_next_state(&s0, &good_fix(), false, Instant::now());
        assert!(matches!(next, GnssDeviceState::Degraded { .. }));
    }

    #[test]
    fn acquiring_failing_stays_acquiring() {
        let s0 = GnssDeviceState::Acquiring {
            since: Instant::now(),
        };
        let next = tpv_next_state(&s0, &good_fix(), false, Instant::now());
        assert!(matches!(next, GnssDeviceState::Acquiring { .. }));
    }

    #[test]
    fn acquisition_timeout_transitions_to_degraded() {
        let start = Instant::now() - Duration::from_secs(600);
        let s0 = GnssDeviceState::Acquiring { since: start };
        let got = check_timeouts(
            &s0,
            None,
            Duration::from_secs(300),
            Duration::from_secs(30),
            Instant::now(),
        );
        assert!(matches!(got, Some(GnssDeviceState::Degraded { .. })));
    }

    #[test]
    fn tracking_stall_timeout_transitions_to_degraded() {
        let s0 = GnssDeviceState::Tracking {
            last_fix: good_fix(),
            last_fix_at: Instant::now() - Duration::from_secs(60),
        };
        let got = check_timeouts(
            &s0,
            Some(Instant::now() - Duration::from_secs(60)),
            Duration::from_secs(300),
            Duration::from_secs(30),
            Instant::now(),
        );
        assert!(matches!(got, Some(GnssDeviceState::Degraded { .. })));
    }

    #[test]
    fn tracking_recent_tpv_keeps_state() {
        let s0 = GnssDeviceState::Tracking {
            last_fix: good_fix(),
            last_fix_at: Instant::now(),
        };
        let got = check_timeouts(
            &s0,
            Some(Instant::now()),
            Duration::from_secs(300),
            Duration::from_secs(30),
            Instant::now(),
        );
        assert!(got.is_none());
    }

    #[test]
    fn emit_suppressed_by_rate_limit() {
        let info = InterfaceInfo {
            ifindex: 1,
            ifname: "/dev/ttyS0".into(),
            mac: [0; 6],
            mtu: 0,
            operstate: nexus_core::OperState::Up,
            carrier: true,
            kind: nexus_core::InterfaceKind::Gnss {
                device_path: "/dev/ttyS0".into(),
                gpsd_device: "/dev/ttyS0".into(),
                vendor_model: None,
            },
            discovered_at: Instant::now(),
        };
        let mut entry = GnssDeviceEntry::new(
            info,
            EffectiveProfile::defaults_for("/dev/ttyS0"),
            Instant::now(),
        );
        entry.last_fix_emit_at = Some(Instant::now());
        let got = should_emit(&entry, &good_fix(), Instant::now(), PowerState::Active);
        assert_eq!(got, Outcome::Suppressed(SuppressReason::RateLimit));
    }

    #[test]
    fn emit_allowed_after_interval() {
        let info = InterfaceInfo {
            ifindex: 1,
            ifname: "/dev/ttyS0".into(),
            mac: [0; 6],
            mtu: 0,
            operstate: nexus_core::OperState::Up,
            carrier: true,
            kind: nexus_core::InterfaceKind::Gnss {
                device_path: "/dev/ttyS0".into(),
                gpsd_device: "/dev/ttyS0".into(),
                vendor_model: None,
            },
            discovered_at: Instant::now(),
        };
        let mut entry = GnssDeviceEntry::new(
            info,
            EffectiveProfile::defaults_for("/dev/ttyS0"),
            Instant::now(),
        );
        entry.last_fix_emit_at = Some(Instant::now() - Duration::from_secs(2));
        let got = should_emit(&entry, &good_fix(), Instant::now(), PowerState::Active);
        assert_eq!(got, Outcome::Emit);
    }

    #[test]
    fn background_floor_suppresses_within_five_seconds() {
        let info = InterfaceInfo {
            ifindex: 1,
            ifname: "/dev/ttyS0".into(),
            mac: [0; 6],
            mtu: 0,
            operstate: nexus_core::OperState::Up,
            carrier: true,
            kind: nexus_core::InterfaceKind::Gnss {
                device_path: "/dev/ttyS0".into(),
                gpsd_device: "/dev/ttyS0".into(),
                vendor_model: None,
            },
            discovered_at: Instant::now(),
        };
        // Profile says 5 Hz (200 ms) but Background floor is 5 s.
        let mut profile = EffectiveProfile::defaults_for("/dev/ttyS0");
        profile.max_update_hz = 5;
        let mut entry = GnssDeviceEntry::new(info, profile, Instant::now());
        entry.last_fix_emit_at = Some(Instant::now() - Duration::from_secs(2));
        let got = should_emit(&entry, &good_fix(), Instant::now(), PowerState::Background);
        assert_eq!(got, Outcome::Suppressed(SuppressReason::RateLimit));
    }

    #[test]
    fn background_floor_emits_after_five_seconds() {
        let info = InterfaceInfo {
            ifindex: 1,
            ifname: "/dev/ttyS0".into(),
            mac: [0; 6],
            mtu: 0,
            operstate: nexus_core::OperState::Up,
            carrier: true,
            kind: nexus_core::InterfaceKind::Gnss {
                device_path: "/dev/ttyS0".into(),
                gpsd_device: "/dev/ttyS0".into(),
                vendor_model: None,
            },
            discovered_at: Instant::now(),
        };
        let mut profile = EffectiveProfile::defaults_for("/dev/ttyS0");
        profile.max_update_hz = 5;
        let mut entry = GnssDeviceEntry::new(info, profile, Instant::now());
        entry.last_fix_emit_at = Some(Instant::now() - Duration::from_secs(6));
        let got = should_emit(&entry, &good_fix(), Instant::now(), PowerState::Background);
        assert_eq!(got, Outcome::Emit);
    }

    #[test]
    fn movement_only_suppresses_without_movement() {
        let info = InterfaceInfo {
            ifindex: 1,
            ifname: "/dev/ttyS0".into(),
            mac: [0; 6],
            mtu: 0,
            operstate: nexus_core::OperState::Up,
            carrier: true,
            kind: nexus_core::InterfaceKind::Gnss {
                device_path: "/dev/ttyS0".into(),
                gpsd_device: "/dev/ttyS0".into(),
                vendor_model: None,
            },
            discovered_at: Instant::now(),
        };
        let mut profile = EffectiveProfile::defaults_for("/dev/ttyS0");
        profile.report_movement_only = true;
        profile.movement_threshold_m = 50.0;
        let mut entry = GnssDeviceEntry::new(info, profile, Instant::now());
        entry.last_fix_emit_at = Some(Instant::now() - Duration::from_secs(2));
        entry.last_emitted_fix = Some(good_fix());
        // Same position => moved = 0 < 50.
        let got = should_emit(&entry, &good_fix(), Instant::now(), PowerState::Active);
        assert_eq!(got, Outcome::Suppressed(SuppressReason::MovementThreshold));
    }

    #[test]
    fn movement_only_heartbeat_emits() {
        let info = InterfaceInfo {
            ifindex: 1,
            ifname: "/dev/ttyS0".into(),
            mac: [0; 6],
            mtu: 0,
            operstate: nexus_core::OperState::Up,
            carrier: true,
            kind: nexus_core::InterfaceKind::Gnss {
                device_path: "/dev/ttyS0".into(),
                gpsd_device: "/dev/ttyS0".into(),
                vendor_model: None,
            },
            discovered_at: Instant::now(),
        };
        let mut profile = EffectiveProfile::defaults_for("/dev/ttyS0");
        profile.report_movement_only = true;
        profile.heartbeat_interval_s = 10;
        let mut entry = GnssDeviceEntry::new(info, profile, Instant::now());
        entry.last_fix_emit_at = Some(Instant::now() - Duration::from_secs(60));
        entry.last_emitted_fix = Some(good_fix());
        let got = should_emit(&entry, &good_fix(), Instant::now(), PowerState::Active);
        assert_eq!(got, Outcome::Emit);
    }
}
