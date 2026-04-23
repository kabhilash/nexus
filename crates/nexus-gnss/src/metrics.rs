//! GNSS Backend metrics. See DD-005 §11.2.

use ::metrics::{counter, describe_counter, describe_gauge, gauge};

pub const DEVICES: &str = "nexus_gnss_devices";
pub const STATE: &str = "nexus_gnss_state";
pub const TPV_TOTAL: &str = "nexus_gnss_tpv_total";
pub const FIXES_FILTERED_TOTAL: &str = "nexus_gnss_fixes_filtered_total";
pub const EMISSIONS_SUPPRESSED_TOTAL: &str = "nexus_gnss_emissions_suppressed_total";
pub const SATELLITES_IN_VIEW: &str = "nexus_gnss_satellites_in_view";
pub const SATELLITES_USED: &str = "nexus_gnss_satellites_used";
pub const HORIZONTAL_ERROR: &str = "nexus_gnss_horizontal_error_meters";
pub const GPSD_RECONNECTS_TOTAL: &str = "nexus_gnss_gpsd_reconnects_total";
pub const GPSD_CONNECTED: &str = "nexus_gnss_gpsd_connected";
pub const TPV_STALL_EVENTS_TOTAL: &str = "nexus_gnss_tpv_stall_events_total";

pub const STATE_LABELS: &[&str] = &["acquiring", "tracking", "degraded", "gone"];

pub mod fix_mode {
    pub const NO_FIX: &str = "no_fix";
    pub const FIX_2D: &str = "fix_2d";
    pub const FIX_3D: &str = "fix_3d";
}

pub fn register() {
    describe_gauge!(DEVICES, "Number of registered GNSS devices");
    describe_gauge!(STATE, "1 iff the device is in the labeled state");
    describe_counter!(TPV_TOTAL, "TPV messages received from gpsd");
    describe_counter!(FIXES_FILTERED_TOTAL, "Fixes rejected by the quality filter");
    describe_counter!(
        EMISSIONS_SUPPRESSED_TOTAL,
        "Quality-passing fixes suppressed by the emission policy"
    );
    describe_gauge!(SATELLITES_IN_VIEW, "Most recent SKY count per device");
    describe_gauge!(SATELLITES_USED, "Most recent fix's satellites-used");
    describe_gauge!(HORIZONTAL_ERROR, "Most recent reported eph");
    describe_counter!(GPSD_RECONNECTS_TOTAL, "gpsd connect() attempts");
    describe_gauge!(GPSD_CONNECTED, "1 when gpsd connection is alive");
    describe_counter!(TPV_STALL_EVENTS_TOTAL, "TPV stall transitions");
}

pub fn set_devices(n: u64) {
    gauge!(DEVICES).set(n as f64);
}

pub fn set_state(device_path: &str, state: &str, active: bool) {
    gauge!(
        STATE,
        "device_path" => device_path.to_owned(),
        "state" => state.to_owned(),
    )
    .set(if active { 1.0 } else { 0.0 });
}

pub fn record_tpv(device_path: &str, mode: &str) {
    counter!(
        TPV_TOTAL,
        "device_path" => device_path.to_owned(),
        "mode" => mode.to_owned(),
    )
    .increment(1);
}

pub fn record_fix_filtered(device_path: &str, reason: &str) {
    counter!(
        FIXES_FILTERED_TOTAL,
        "device_path" => device_path.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

pub fn record_emission_suppressed(device_path: &str, reason: &str) {
    counter!(
        EMISSIONS_SUPPRESSED_TOTAL,
        "device_path" => device_path.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

pub fn set_satellites_in_view(device_path: &str, n: u64) {
    gauge!(SATELLITES_IN_VIEW, "device_path" => device_path.to_owned()).set(n as f64);
}

pub fn set_satellites_used(device_path: &str, n: u64) {
    gauge!(SATELLITES_USED, "device_path" => device_path.to_owned()).set(n as f64);
}

pub fn set_horizontal_error(device_path: &str, meters: f64) {
    gauge!(HORIZONTAL_ERROR, "device_path" => device_path.to_owned()).set(meters);
}

pub fn record_gpsd_reconnect() {
    counter!(GPSD_RECONNECTS_TOTAL).increment(1);
}

pub fn set_gpsd_connected(up: bool) {
    gauge!(GPSD_CONNECTED).set(if up { 1.0 } else { 0.0 });
}

pub fn record_tpv_stall(device_path: &str) {
    counter!(TPV_STALL_EVENTS_TOTAL, "device_path" => device_path.to_owned()).increment(1);
}

/// Label for the `mode` dimension on `nexus_gnss_tpv_total`.
pub fn mode_label(mode: crate::fix::FixMode) -> &'static str {
    match mode {
        crate::fix::FixMode::NoFix => fix_mode::NO_FIX,
        crate::fix::FixMode::Fix2D => fix_mode::FIX_2D,
        crate::fix::FixMode::Fix3D => fix_mode::FIX_3D,
    }
}
