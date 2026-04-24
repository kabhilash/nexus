//! `zbus::Error` → [`NexusctlError`] translator. DD-008 §9.
//!
//! # Wire format quirk
//!
//! nexusd's D-Bus layer uses `zbus::fdo::Error::Failed(prefixed)`
//! for every `fi.nexus.Error.*` it returns — see
//! `crates/nexus-dbus/src/errors.rs::name`. So clients receive
//! `MethodError("org.freedesktop.DBus.Error.Failed", Some("fi.nexus.Error.X: <msg>"), _)`
//! rather than a properly-named D-Bus error. This translator
//! parses the well-known prefix out of the message string.
//!
//! Pure standard `org.freedesktop.DBus.Error.*` names
//! (`ServiceUnknown`, `NoReply`, `AccessDenied`) come through as
//! their actual names and are matched directly.
//!
//! # Adding a new error
//!
//! 1. Add a variant to [`crate::errors::NexusctlError`].
//! 2. Add the wire-name → variant arm in [`from_zbus_error`].
//! 3. Add a row to `tests/error_translation.rs` with the exact
//!    name + message the daemon emits.

use crate::errors::NexusctlError;

/// Map any `zbus::Error` to a [`NexusctlError`]. Every method-call
/// failure path on this side of the bus eventually goes through
/// here.
pub fn from_zbus_error(err: zbus::Error) -> NexusctlError {
    match &err {
        zbus::Error::MethodError(name, detail, _) => {
            let wire_name = name.as_str();
            let raw_msg = detail.clone().unwrap_or_default();
            translate_method_error(wire_name, &raw_msg)
        }
        // Connection-level failures: address parse, no socket on
        // the path, handshake failed. All map to "nexusd not
        // reachable" — same operator remedy.
        zbus::Error::InputOutput(_) | zbus::Error::Address(_) | zbus::Error::Handshake(_) => {
            NexusctlError::NexusdUnreachable
        }
        _ => NexusctlError::Other {
            raw: err.to_string(),
        },
    }
}

/// Translate a `(wire_name, message)` pair. Split out so unit
/// tests can drive it without a real `zbus::Error`.
pub fn translate_method_error(wire_name: &str, raw_msg: &str) -> NexusctlError {
    // Standard freedesktop names short-circuit.
    match wire_name {
        "org.freedesktop.DBus.Error.ServiceUnknown" => {
            return NexusctlError::NexusdUnreachable;
        }
        "org.freedesktop.DBus.Error.NoReply" => {
            return NexusctlError::Timeout {
                operation: "d-bus call".into(),
                duration_s: None,
            };
        }
        "org.freedesktop.DBus.Error.AccessDenied" => {
            return NexusctlError::AuthDenied {
                action: "d-bus method call".into(),
                hint: "is `nexus.conf` policy installed?".into(),
            };
        }
        _ => {}
    }

    // The Failed/`fi.nexus.Error.X: msg` pattern. If the prefix
    // doesn't match we fall through to Other so the operator
    // still sees what nexusd said.
    let (well_known, inner) = match split_nexus_error(raw_msg) {
        Some(parts) => parts,
        None => {
            // Some other freedesktop error nexusctl doesn't model
            // explicitly. Surface verbatim.
            return NexusctlError::Other {
                raw: format!("{wire_name}: {raw_msg}"),
            };
        }
    };

    match well_known {
        "fi.nexus.Error.AuthFailed" => NexusctlError::AuthDenied {
            action: action_from_message(inner),
            hint: "authenticate as a member of the nexus-admin group, \
                   or run as root"
                .into(),
        },
        "fi.nexus.Error.Timeout" => NexusctlError::Timeout {
            operation: inner.to_owned(),
            duration_s: parse_duration_s(inner),
        },
        "fi.nexus.Error.FeatureDisabled" => NexusctlError::FeatureDisabled {
            feature: inner.trim().to_owned(),
        },
        "fi.nexus.Error.RateLimited" => {
            // Daemon emits "RateLimited: <op>: retry after <ms> ms".
            // The split above stripped the "fi.nexus.Error.RateLimited:"
            // prefix; the rest is the inner.
            let (op, retry_after_ms) = parse_rate_limited(inner);
            NexusctlError::RateLimited { op, retry_after_ms }
        }
        "fi.nexus.Error.InvalidState" => NexusctlError::InvalidState {
            operation: "operation".into(),
            state: inner.to_owned(),
        },
        "fi.nexus.Error.InvalidArgument" => NexusctlError::InvalidArgument {
            message: inner.to_owned(),
        },
        "fi.nexus.Error.NotFound" => NexusctlError::NotFound {
            reference: inner.to_owned(),
        },
        "fi.nexus.Error.AlreadyExists" => NexusctlError::AlreadyExists {
            reference: inner.to_owned(),
        },
        "fi.nexus.Error.UnknownDevice" => NexusctlError::UnknownDevice {
            address: inner.to_owned(),
        },
        "fi.nexus.Error.UnknownPairingJob" => NexusctlError::UnknownPairingJob {
            job_id: inner.to_owned(),
        },
        "fi.nexus.Error.ConnectionFailed" => NexusctlError::ConnectionFailed {
            reason: inner.to_owned(),
        },
        "fi.nexus.Error.BluezUnavailable" => NexusctlError::BluezUnavailable,
        "fi.nexus.Error.SupplicantUnavailable" => NexusctlError::SupplicantUnavailable,
        "fi.nexus.Error.NotPowered" => NexusctlError::NotPowered {
            adapter: inner.to_owned(),
        },
        "fi.nexus.Error.NotPaired" => NexusctlError::NotPaired {
            device: inner.to_owned(),
        },
        "fi.nexus.Error.ResourceBusy" => NexusctlError::ResourceBusy {
            resource: inner.to_owned(),
        },
        "fi.nexus.Error.IoError" => NexusctlError::IoError {
            detail: inner.to_owned(),
        },
        "fi.nexus.Error.CryptoError" => NexusctlError::CryptoError {
            detail: inner.to_owned(),
        },
        "fi.nexus.Error.Unsupported" => NexusctlError::Unsupported {
            detail: inner.to_owned(),
        },
        // A `fi.nexus.Error.*` we don't model. Surface verbatim.
        other => NexusctlError::Other {
            raw: format!("{other}: {inner}"),
        },
    }
}

/// Split `"fi.nexus.Error.X: rest of message"` into
/// `("fi.nexus.Error.X", "rest of message")`. Returns `None` if
/// the message doesn't carry the prefix.
fn split_nexus_error(msg: &str) -> Option<(&str, &str)> {
    let (head, rest) = msg.split_once(": ")?;
    if head.starts_with("fi.nexus.Error.") {
        Some((head, rest))
    } else {
        None
    }
}

/// `"<op>: retry after <ms> ms"` → `(op, Some(ms))`. Falls back
/// to `(inner, None)` when the message doesn't match.
fn parse_rate_limited(inner: &str) -> (String, Option<u64>) {
    if let Some((op, tail)) = inner.split_once(": retry after ") {
        if let Some(ms_str) = tail.strip_suffix(" ms") {
            if let Ok(ms) = ms_str.parse::<u64>() {
                return (op.to_owned(), Some(ms));
            }
        }
    }
    (inner.to_owned(), None)
}

/// Best-effort extraction of the PolicyKit action that was denied
/// from the daemon's "policykit denied 'fi.nexus.X' for sender
/// '...'" message. When the format doesn't match (e.g., a
/// translated message), fall back to a generic label.
fn action_from_message(msg: &str) -> String {
    if let Some(after) = msg.strip_prefix("policykit denied '") {
        if let Some(idx) = after.find('\'') {
            return after[..idx].to_owned();
        }
    }
    "the requested action".to_owned()
}

/// `Timeout` messages don't currently carry a structured duration;
/// we make a best-effort scan for `"after <N>s"` so future daemon
/// changes upgrade automatically.
fn parse_duration_s(msg: &str) -> Option<u64> {
    let after = msg.split_once("after ")?.1;
    let num: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if num.is_empty() {
        return None;
    }
    num.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_handles_well_known_prefix() {
        let (head, rest) = split_nexus_error("fi.nexus.Error.NotFound: wlan9").unwrap();
        assert_eq!(head, "fi.nexus.Error.NotFound");
        assert_eq!(rest, "wlan9");
    }

    #[test]
    fn split_returns_none_when_prefix_missing() {
        assert!(split_nexus_error("plain failed message").is_none());
    }

    #[test]
    fn parse_rate_limited_extracts_op_and_ms() {
        let (op, ms) = parse_rate_limited("scan: retry after 1234 ms");
        assert_eq!(op, "scan");
        assert_eq!(ms, Some(1234));
    }

    #[test]
    fn parse_rate_limited_falls_back_when_format_unfamiliar() {
        let (op, ms) = parse_rate_limited("garbage");
        assert_eq!(op, "garbage");
        assert_eq!(ms, None);
    }

    #[test]
    fn action_from_message_extracts_quoted_action() {
        let s = "policykit denied 'fi.nexus.profile.add' for sender ':1.42'";
        assert_eq!(action_from_message(s), "fi.nexus.profile.add");
    }
}
