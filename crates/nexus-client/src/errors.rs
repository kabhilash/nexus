//! `NexusctlError` and exit-code mapping. Mirrors the table in
//! DD-008 §9 verbatim; adding a new error class requires updating
//! both this enum and [`exit_code_for`] below, plus the per-row
//! translator in [`crate::errors_map`].
//!
//! In `--json` mode, [`json_error_object`] serialises a
//! [`NexusctlError`] into the on-the-wire envelope DD-008 §9
//! specifies.

use serde_json::json;
use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum NexusctlError {
    /// `fi.nexus.Error.AuthFailed` or
    /// `org.freedesktop.DBus.Error.AccessDenied`.
    #[error("permission denied: {action} requires {hint}")]
    AuthDenied { action: String, hint: String },

    /// `fi.nexus.Error.Timeout` or `org.freedesktop.DBus.Error.NoReply`.
    /// `duration_s` is `None` when the wire didn't carry one.
    #[error("operation timed out{}", format_timeout(*duration_s))]
    Timeout {
        operation: String,
        duration_s: Option<u64>,
    },

    /// A command needed a TTY (PSK prompt, pairing) but stdin
    /// isn't a terminal. Phase 1 doesn't surface this; later
    /// interactive commands will.
    #[error("interactive prompt required but stdin is not a terminal: {operation}")]
    NotInteractive { operation: String },

    /// `org.freedesktop.DBus.Error.ServiceUnknown` or any
    /// connection-level failure.
    #[error("nexusd is not running (try `systemctl start nexus`)")]
    NexusdUnreachable,

    /// `fi.nexus.Error.FeatureDisabled` — the named backend is
    /// disabled in `nexus.toml`.
    #[error("feature disabled: {feature} is turned off in nexus.toml")]
    FeatureDisabled { feature: String },

    /// `fi.nexus.Error.InvalidState`.
    #[error("operation not valid in current state: {state}")]
    InvalidState { operation: String, state: String },

    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },

    #[error("not found: {reference}")]
    NotFound { reference: String },

    #[error("already exists: {reference}")]
    AlreadyExists { reference: String },

    #[error("device not known: {address}")]
    UnknownDevice { address: String },

    #[error("no pairing with job id {job_id}")]
    UnknownPairingJob { job_id: String },

    #[error("connection failed: {reason}")]
    ConnectionFailed { reason: String },

    #[error("BlueZ is not running or not reachable")]
    BluezUnavailable,

    #[error("wpa_supplicant/iwd is not available")]
    SupplicantUnavailable,

    #[error("adapter {adapter} is not powered")]
    NotPowered { adapter: String },

    #[error("device {device} is not paired")]
    NotPaired { device: String },

    #[error("resource busy: {resource}")]
    ResourceBusy { resource: String },

    /// `fi.nexus.Error.RateLimited` — per-sender rate limit
    /// exceeded. DD-008 §9 didn't include this until Prompt 6.1
    /// landed it in DD-006; we surface a dedicated variant rather
    /// than overloading `ResourceBusy` because the operator remedy
    /// is "wait and retry," not "stop the colliding op."
    #[error("rate limited: {op}{}", format_retry(*retry_after_ms))]
    RateLimited {
        op: String,
        retry_after_ms: Option<u64>,
    },

    #[error("I/O error: {detail}")]
    IoError { detail: String },

    #[error("crypto error: {detail}")]
    CryptoError { detail: String },

    #[error("unsupported: {detail}")]
    Unsupported { detail: String },

    /// Catch-all. Wraps the original D-Bus name + message so the
    /// operator can still see what nexusd actually said.
    #[error("{raw}")]
    Other { raw: String },
}

fn format_timeout(s: Option<u64>) -> String {
    match s {
        Some(n) => format!(" after {n}s"),
        None => String::new(),
    }
}

fn format_retry(ms: Option<u64>) -> String {
    match ms {
        Some(n) => format!(" (retry after {n}ms)"),
        None => String::new(),
    }
}

impl NexusctlError {
    pub fn exit_code(&self) -> i32 {
        exit_code_for(self)
    }

    /// Stable kebab-case identifier used in `--json` error envelopes.
    /// Tests assert these exact strings — don't rename casually.
    pub fn json_kind(&self) -> &'static str {
        match self {
            NexusctlError::AuthDenied { .. } => "auth_denied",
            NexusctlError::Timeout { .. } => "timeout",
            NexusctlError::NotInteractive { .. } => "not_interactive",
            NexusctlError::NexusdUnreachable => "nexusd_unreachable",
            NexusctlError::FeatureDisabled { .. } => "feature_disabled",
            NexusctlError::InvalidState { .. } => "invalid_state",
            NexusctlError::InvalidArgument { .. } => "invalid_argument",
            NexusctlError::NotFound { .. } => "not_found",
            NexusctlError::AlreadyExists { .. } => "already_exists",
            NexusctlError::UnknownDevice { .. } => "unknown_device",
            NexusctlError::UnknownPairingJob { .. } => "unknown_pairing_job",
            NexusctlError::ConnectionFailed { .. } => "connection_failed",
            NexusctlError::BluezUnavailable => "bluez_unavailable",
            NexusctlError::SupplicantUnavailable => "supplicant_unavailable",
            NexusctlError::NotPowered { .. } => "not_powered",
            NexusctlError::NotPaired { .. } => "not_paired",
            NexusctlError::ResourceBusy { .. } => "resource_busy",
            NexusctlError::RateLimited { .. } => "rate_limited",
            NexusctlError::IoError { .. } => "io_error",
            NexusctlError::CryptoError { .. } => "crypto_error",
            NexusctlError::Unsupported { .. } => "unsupported",
            NexusctlError::Other { .. } => "other",
        }
    }
}

/// DD-008 §4.3 exit-code table.
pub fn exit_code_for(err: &NexusctlError) -> i32 {
    match err {
        NexusctlError::AuthDenied { .. } => 3,
        NexusctlError::Timeout { .. } => 4,
        NexusctlError::NotInteractive { .. } => 5,
        NexusctlError::NexusdUnreachable => 6,
        NexusctlError::FeatureDisabled { .. } => 7,
        // Everything else collapses to "general failure". The
        // human message distinguishes the cases.
        NexusctlError::InvalidState { .. }
        | NexusctlError::InvalidArgument { .. }
        | NexusctlError::NotFound { .. }
        | NexusctlError::AlreadyExists { .. }
        | NexusctlError::UnknownDevice { .. }
        | NexusctlError::UnknownPairingJob { .. }
        | NexusctlError::ConnectionFailed { .. }
        | NexusctlError::BluezUnavailable
        | NexusctlError::SupplicantUnavailable
        | NexusctlError::NotPowered { .. }
        | NexusctlError::NotPaired { .. }
        | NexusctlError::ResourceBusy { .. }
        | NexusctlError::RateLimited { .. }
        | NexusctlError::IoError { .. }
        | NexusctlError::CryptoError { .. }
        | NexusctlError::Unsupported { .. }
        | NexusctlError::Other { .. } => 1,
    }
}

/// Build the JSON object DD-008 §9 specifies for `--json` mode
/// errors. `error` is the kebab-case kind, `message` is the
/// human-readable string, and any structured fields (action, hint,
/// reference, …) are included alongside.
pub fn json_error_object(err: &NexusctlError) -> serde_json::Value {
    let mut obj = json!({
        "error": err.json_kind(),
        "message": err.to_string(),
    });
    let map = obj.as_object_mut().expect("object");
    match err {
        NexusctlError::AuthDenied { action, hint } => {
            map.insert("action".into(), json!(action));
            map.insert("hint".into(), json!(hint));
        }
        NexusctlError::Timeout {
            operation,
            duration_s,
        } => {
            map.insert("operation".into(), json!(operation));
            if let Some(d) = duration_s {
                map.insert("duration_s".into(), json!(d));
            }
        }
        NexusctlError::NotInteractive { operation }
        | NexusctlError::InvalidState { operation, .. } => {
            map.insert("operation".into(), json!(operation));
            if let NexusctlError::InvalidState { state, .. } = err {
                map.insert("state".into(), json!(state));
            }
        }
        NexusctlError::FeatureDisabled { feature } => {
            map.insert("feature".into(), json!(feature));
        }
        NexusctlError::InvalidArgument { message } => {
            map.insert("detail".into(), json!(message));
        }
        NexusctlError::NotFound { reference } | NexusctlError::AlreadyExists { reference } => {
            map.insert("reference".into(), json!(reference));
        }
        NexusctlError::UnknownDevice { address } => {
            map.insert("address".into(), json!(address));
        }
        NexusctlError::UnknownPairingJob { job_id } => {
            map.insert("job_id".into(), json!(job_id));
        }
        NexusctlError::ConnectionFailed { reason } => {
            map.insert("reason".into(), json!(reason));
        }
        NexusctlError::NotPowered { adapter } => {
            map.insert("adapter".into(), json!(adapter));
        }
        NexusctlError::NotPaired { device } => {
            map.insert("device".into(), json!(device));
        }
        NexusctlError::ResourceBusy { resource } => {
            map.insert("resource".into(), json!(resource));
        }
        NexusctlError::RateLimited { op, retry_after_ms } => {
            map.insert("op".into(), json!(op));
            if let Some(ms) = retry_after_ms {
                map.insert("retry_after_ms".into(), json!(ms));
            }
        }
        NexusctlError::IoError { detail }
        | NexusctlError::CryptoError { detail }
        | NexusctlError::Unsupported { detail } => {
            map.insert("detail".into(), json!(detail));
        }
        NexusctlError::Other { raw } => {
            map.insert("raw".into(), json!(raw));
        }
        NexusctlError::NexusdUnreachable
        | NexusctlError::BluezUnavailable
        | NexusctlError::SupplicantUnavailable => {}
    }
    obj
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_dd008() {
        assert_eq!(exit_code_for(&NexusctlError::NexusdUnreachable), 6);
        assert_eq!(
            exit_code_for(&NexusctlError::AuthDenied {
                action: "x".into(),
                hint: "y".into()
            }),
            3
        );
        assert_eq!(
            exit_code_for(&NexusctlError::Timeout {
                operation: "scan".into(),
                duration_s: Some(30)
            }),
            4
        );
        assert_eq!(
            exit_code_for(&NexusctlError::NotInteractive {
                operation: "psk".into()
            }),
            5
        );
        assert_eq!(
            exit_code_for(&NexusctlError::FeatureDisabled {
                feature: "wifi".into()
            }),
            7
        );
        assert_eq!(exit_code_for(&NexusctlError::Other { raw: "x".into() }), 1);
        assert_eq!(
            exit_code_for(&NexusctlError::NotFound {
                reference: "wlan9".into()
            }),
            1
        );
    }

    #[test]
    fn json_kind_strings_are_kebab_case() {
        // Spot-check a few; the per-variant test is the
        // round-trip in `tests/error_translation.rs`.
        assert_eq!(
            NexusctlError::NexusdUnreachable.json_kind(),
            "nexusd_unreachable"
        );
        assert_eq!(
            NexusctlError::FeatureDisabled {
                feature: "wifi".into()
            }
            .json_kind(),
            "feature_disabled"
        );
    }

    #[test]
    fn json_envelope_includes_structured_fields() {
        let err = NexusctlError::AuthDenied {
            action: "fi.nexus.profile.add".into(),
            hint: "authenticate as admin".into(),
        };
        let v = json_error_object(&err);
        assert_eq!(v["error"], "auth_denied");
        assert_eq!(v["action"], "fi.nexus.profile.add");
        assert_eq!(v["hint"], "authenticate as admin");
        assert!(
            v["message"].as_str().unwrap().contains("permission denied"),
            "got {v}"
        );
    }
}
