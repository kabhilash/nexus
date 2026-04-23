//! Wi-Fi Backend metrics. See DD-003 §12.5.

use ::metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

pub const INTERFACES_MANAGED: &str = "nexus_wifi_interfaces_managed";
pub const SCANS_TOTAL: &str = "nexus_wifi_scans_total";
pub const SCAN_DURATION: &str = "nexus_wifi_scan_duration_seconds";
pub const BSS_CACHE_ENTRIES: &str = "nexus_wifi_bss_cache_entries";
pub const CONNECT_ATTEMPTS: &str = "nexus_wifi_connect_attempts_total";
pub const CONNECT_DURATION: &str = "nexus_wifi_connect_duration_seconds";
pub const LINK_READY_TOTAL: &str = "nexus_wifi_link_ready_total";
pub const LINK_LOST_TOTAL: &str = "nexus_wifi_link_lost_total";
pub const SIGNAL_DBM: &str = "nexus_wifi_signal_dbm";
pub const ROAMS_TOTAL: &str = "nexus_wifi_roams_total";
pub const BSSID_BLACKLISTED: &str = "nexus_wifi_bssid_blacklisted";
pub const PROFILE_CREDENTIALS_INVALID: &str = "nexus_wifi_profile_credentials_invalid";
pub const SUPPLICANT_AVAILABLE: &str = "nexus_wifi_supplicant_available";
pub const DRIVER_WEDGE_RECOVERIES: &str = "nexus_wifi_driver_wedge_recoveries_total";

pub mod scan_type {
    pub const BROADCAST: &str = "broadcast";
    pub const DIRECTED: &str = "directed";
    pub const ROAM: &str = "roam";
    pub const HIDDEN: &str = "hidden";
}

pub mod scan_outcome {
    pub const SUCCESS: &str = "success";
    pub const ABORTED: &str = "aborted";
    pub const FAILED: &str = "failed";
}

pub mod connect_outcome {
    pub const SUCCESS: &str = "success";
    pub const ASSOC_TIMEOUT: &str = "assoc_timeout";
    pub const AUTH_FAILURE: &str = "auth_failure";
    pub const HANDSHAKE_TIMEOUT: &str = "handshake_timeout";
    pub const CREDENTIALS_INVALID: &str = "credentials_invalid";
    pub const OTHER: &str = "other";
}

pub mod link_lost_reason {
    pub const DEAUTH: &str = "deauth";
    pub const CARRIER_DOWN: &str = "carrier_down";
    pub const SUPPLICANT_DOWN: &str = "supplicant_down";
    pub const RFKILL: &str = "rfkill";
}

pub fn register() {
    describe_gauge!(INTERFACES_MANAGED, "Wi-Fi interfaces by state");
    describe_counter!(SCANS_TOTAL, "Scan attempts by ifname, type, outcome");
    describe_histogram!(SCAN_DURATION, "Scan duration seconds by ifname, type");
    describe_gauge!(BSS_CACHE_ENTRIES, "BSS cache size per interface");
    describe_counter!(
        CONNECT_ATTEMPTS,
        "Connect attempts by ifname, security, outcome"
    );
    describe_histogram!(
        CONNECT_DURATION,
        "Time from Connecting to Connected by ifname, security"
    );
    describe_counter!(LINK_READY_TOTAL, "WifiLinkReady emissions by ifname");
    describe_counter!(LINK_LOST_TOTAL, "WifiLinkLost emissions by ifname, reason");
    describe_gauge!(SIGNAL_DBM, "Most recent RSSI for the connected BSS");
    describe_counter!(ROAMS_TOTAL, "Roam attempts by ifname, mode, outcome");
    describe_gauge!(
        BSSID_BLACKLISTED,
        "Currently-blacklisted BSSIDs per interface"
    );
    describe_gauge!(
        PROFILE_CREDENTIALS_INVALID,
        "Profiles currently marked credentials_invalid"
    );
    describe_gauge!(
        SUPPLICANT_AVAILABLE,
        "1 if the supplicant D-Bus name is present, 0 otherwise"
    );
    describe_counter!(
        DRIVER_WEDGE_RECOVERIES,
        "Driver-wedge recovery attempts per ifname (§12.4)"
    );
}

pub fn set_interfaces_managed(state: &str, count: u64) {
    gauge!(INTERFACES_MANAGED, "state" => state.to_owned()).set(count as f64);
}

pub fn record_scan(ifname: &str, scan_type: &str, outcome: &str) {
    counter!(
        SCANS_TOTAL,
        "ifname" => ifname.to_owned(),
        "type" => scan_type.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

pub fn record_scan_duration(ifname: &str, scan_type: &str, secs: f64) {
    histogram!(
        SCAN_DURATION,
        "ifname" => ifname.to_owned(),
        "type" => scan_type.to_owned(),
    )
    .record(secs);
}

pub fn set_bss_cache_entries(ifname: &str, n: u64) {
    gauge!(BSS_CACHE_ENTRIES, "ifname" => ifname.to_owned()).set(n as f64);
}

pub fn record_connect(ifname: &str, security: &str, outcome: &str) {
    counter!(
        CONNECT_ATTEMPTS,
        "ifname" => ifname.to_owned(),
        "security" => security.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

pub fn record_connect_duration(ifname: &str, security: &str, secs: f64) {
    histogram!(
        CONNECT_DURATION,
        "ifname" => ifname.to_owned(),
        "security" => security.to_owned(),
    )
    .record(secs);
}

pub fn record_link_ready(ifname: &str) {
    counter!(LINK_READY_TOTAL, "ifname" => ifname.to_owned()).increment(1);
}

pub fn record_link_lost(ifname: &str, reason: &str) {
    counter!(
        LINK_LOST_TOTAL,
        "ifname" => ifname.to_owned(),
        "reason" => reason.to_owned(),
    )
    .increment(1);
}

pub fn set_signal_dbm(ifname: &str, rssi: i32) {
    gauge!(SIGNAL_DBM, "ifname" => ifname.to_owned()).set(rssi as f64);
}

pub fn record_roam(ifname: &str, mode: &str, outcome: &str) {
    counter!(
        ROAMS_TOTAL,
        "ifname" => ifname.to_owned(),
        "mode" => mode.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

pub fn set_bssid_blacklisted(ifname: &str, n: u64) {
    gauge!(BSSID_BLACKLISTED, "ifname" => ifname.to_owned()).set(n as f64);
}

pub fn set_profile_credentials_invalid(n: u64) {
    gauge!(PROFILE_CREDENTIALS_INVALID).set(n as f64);
}

pub fn set_supplicant_available(backend: &str, up: bool) {
    gauge!(SUPPLICANT_AVAILABLE, "backend" => backend.to_owned()).set(if up { 1.0 } else { 0.0 });
}

pub fn record_driver_wedge_recovery(ifname: &str) {
    counter!(DRIVER_WEDGE_RECOVERIES, "ifname" => ifname.to_owned()).increment(1);
}
