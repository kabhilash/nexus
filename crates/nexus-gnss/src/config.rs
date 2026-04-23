//! Backend configuration. See DD-005 §8.

use std::net::SocketAddr;

use crate::fix::FixMode;

/// Global GNSS Backend config.
#[derive(Debug, Clone)]
pub struct GnssConfig {
    /// gpsd TCP endpoint. Defaults to `127.0.0.1:2947`.
    pub gpsd_endpoint: SocketAddr,
    /// Time allowed before `Acquiring → Degraded` if no quality
    /// fix has arrived.
    pub acquisition_timeout_s: u32,
    /// TPV silence before `Tracking → Degraded`.
    pub tpv_stall_timeout_s: u32,
    /// Prolonged-outage notification threshold for the reconcile
    /// supervisor.
    pub gpsd_outage_notify_s: u32,
    /// Global default thresholds; a per-device profile overrides
    /// any of these.
    pub defaults: GnssDefaults,
}

impl Default for GnssConfig {
    fn default() -> Self {
        Self {
            gpsd_endpoint: ([127, 0, 0, 1], 2947).into(),
            acquisition_timeout_s: 300,
            tpv_stall_timeout_s: 30,
            gpsd_outage_notify_s: 60,
            defaults: GnssDefaults::default(),
        }
    }
}

/// Threshold defaults applied to every device in the absence of a
/// per-device profile. Mirrors the `[gnss.defaults]` TOML table in
/// DD-005 §8.
#[derive(Debug, Clone)]
pub struct GnssDefaults {
    pub min_fix_mode: FixMode,
    pub min_satellites: u32,
    pub max_horizontal_error_m: Option<f64>,
    pub strict_quality: bool,
    pub max_update_hz: u32,
    pub report_movement_only: bool,
    pub movement_threshold_m: f64,
    pub heartbeat_interval_s: u32,
}

impl Default for GnssDefaults {
    fn default() -> Self {
        Self {
            min_fix_mode: FixMode::Fix2D,
            min_satellites: 4,
            max_horizontal_error_m: Some(100.0),
            strict_quality: false,
            max_update_hz: 1,
            report_movement_only: false,
            movement_threshold_m: 10.0,
            heartbeat_interval_s: 60,
        }
    }
}

/// Power state mirrored from DD-006. Drives the update-rate clamp
/// in `background` and emission suspension in `sleep`.
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
