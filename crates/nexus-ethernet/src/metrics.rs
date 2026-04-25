//! Prometheus-style metrics. Authoritative list: DD-002 §9.5.

use ::metrics::{counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram};

use crate::auth::AuthFailureReason;

pub const INTERFACES_MANAGED: &str = "nexus_eth_interfaces_managed";
pub const LINK_READY_TOTAL: &str = "nexus_eth_link_ready_total";
pub const LINK_LOST_TOTAL: &str = "nexus_eth_link_lost_total";
pub const AUTH_ATTEMPTS_TOTAL: &str = "nexus_eth_auth_attempts_total";
pub const AUTH_DURATION_SECONDS: &str = "nexus_eth_auth_duration_seconds";
pub const AUTH_RETRIES_TOTAL: &str = "nexus_eth_auth_retries_total";
pub const AUTH_BACKEND_AVAILABLE: &str = "nexus_eth_auth_backend_available";

pub mod link_lost_reason {
    pub const CARRIER_DOWN: &str = "carrier_down";
    pub const AUTH_FAILURE: &str = "auth_failure";
    pub const REMOVED: &str = "removed";
}

pub mod auth_outcome {
    pub const SUCCESS: &str = "success";
    pub const BAD_CREDENTIALS: &str = "bad_credentials";
    pub const SERVER_UNREACHABLE: &str = "server_unreachable";
    pub const CERT_REJECTED: &str = "cert_rejected";
    pub const TIMEOUT: &str = "timeout";
    pub const OTHER: &str = "other";
    /// 802.1X required by the profile but no auth backend was
    /// configured at startup. Distinct from `OTHER` so dashboards
    /// don't conflate operational gaps with real auth failures.
    pub const BACKEND_UNAVAILABLE: &str = "backend_unavailable";
}

pub fn register() {
    describe_gauge!(
        INTERFACES_MANAGED,
        "Interfaces managed by the Ethernet Backend, by current state"
    );
    describe_counter!(LINK_READY_TOTAL, "EthLinkReady emissions, by ifname");
    describe_counter!(
        LINK_LOST_TOTAL,
        "EthLinkLost emissions, by ifname and reason"
    );
    describe_counter!(
        AUTH_ATTEMPTS_TOTAL,
        "802.1X authentication attempts, by ifname and outcome"
    );
    describe_histogram!(
        AUTH_DURATION_SECONDS,
        "Time from entering Authenticating to final Authenticated or AuthFailed"
    );
    describe_counter!(
        AUTH_RETRIES_TOTAL,
        "Retries following a retriable 802.1X failure, by ifname"
    );
    describe_gauge!(
        AUTH_BACKEND_AVAILABLE,
        "1 when the configured auth backend's D-Bus name is present, 0 otherwise"
    );
}

/// Map an `AuthFailureReason` to the `outcome` label.
pub fn outcome_for(reason: &AuthFailureReason) -> &'static str {
    match reason {
        AuthFailureReason::BadCredentials => auth_outcome::BAD_CREDENTIALS,
        AuthFailureReason::ServerUnreachable => auth_outcome::SERVER_UNREACHABLE,
        AuthFailureReason::CertificateRejected => auth_outcome::CERT_REJECTED,
        AuthFailureReason::Timeout => auth_outcome::TIMEOUT,
        AuthFailureReason::Other(_) => auth_outcome::OTHER,
    }
}

// -- helpers --

pub fn set_interfaces_managed(state: &str, count: u64) {
    gauge!(INTERFACES_MANAGED, "state" => state.to_owned()).set(count as f64);
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

pub fn record_auth_attempt(ifname: &str, outcome: &str) {
    counter!(
        AUTH_ATTEMPTS_TOTAL,
        "ifname" => ifname.to_owned(),
        "outcome" => outcome.to_owned(),
    )
    .increment(1);
}

pub fn record_auth_duration(ifname: &str, secs: f64) {
    histogram!(AUTH_DURATION_SECONDS, "ifname" => ifname.to_owned()).record(secs);
}

pub fn record_auth_retry(ifname: &str) {
    counter!(AUTH_RETRIES_TOTAL, "ifname" => ifname.to_owned()).increment(1);
}

pub fn set_auth_backend_available(backend: &str, available: bool) {
    gauge!(AUTH_BACKEND_AVAILABLE, "backend" => backend.to_owned()).set(if available {
        1.0
    } else {
        0.0
    });
}
