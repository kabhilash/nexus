//! `NexusctlError` and exit-code mapping. See DD-008 §9 + §4.3.
//!
//! Phase 1 covers the variants used by `status` and `iface list`:
//!
//! - [`NexusctlError::AuthDenied`]      — PolicyKit said no (exit 3).
//! - [`NexusctlError::Timeout`]         — D-Bus call timed out (exit 4).
//! - [`NexusctlError::NotInteractive`]  — would prompt with no TTY (exit 5).
//! - [`NexusctlError::NexusdUnreachable`] — `org.freedesktop.DBus.Error.ServiceUnknown`
//!                                          or any address-level error (exit 6).
//! - [`NexusctlError::Other`]           — fallback for everything else (exit 1).
//!
//! Later phases (§9 in DD-008) add `NotFound`, `InvalidArgument`,
//! `RateLimited`, `FeatureDisabled`, etc. Adding a variant here also
//! requires adding an arm to [`exit_code_for`] and to
//! [`from_zbus_error`].

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NexusctlError {
    /// PolicyKit denied the call (`fi.nexus.Error.AuthFailed` or any
    /// authorization-class wire error). Maps to exit 3.
    #[error("authorization denied: {0}")]
    AuthDenied(String),

    /// The D-Bus method call exceeded its deadline. Maps to exit 4.
    #[error("operation timed out: {0}")]
    Timeout(String),

    /// A command needed a TTY (e.g. PSK prompt) but stdin is not a
    /// terminal. Phase 1 doesn't surface this directly but the
    /// variant is here to anchor exit-code wiring.
    #[error("interactive prompt required but stdin is not a terminal: {0}")]
    NotInteractive(String),

    /// `nexusd` isn't listening on the bus, the service isn't
    /// activatable, or the bus address is wrong. Maps to exit 6.
    #[error("nexusd is not running (try `systemctl start nexus`): {0}")]
    NexusdUnreachable(String),

    /// Catch-all for everything else. Maps to exit 1.
    #[error("{0}")]
    Other(String),
}

impl NexusctlError {
    /// `exit_code_for(&self)` shorthand on `self`.
    pub fn exit_code(&self) -> i32 {
        exit_code_for(self)
    }
}

/// Map a [`NexusctlError`] onto the DD-008 §4.3 exit-code table.
pub fn exit_code_for(err: &NexusctlError) -> i32 {
    match err {
        NexusctlError::Other(_) => 1,
        NexusctlError::AuthDenied(_) => 3,
        NexusctlError::Timeout(_) => 4,
        NexusctlError::NotInteractive(_) => 5,
        NexusctlError::NexusdUnreachable(_) => 6,
    }
}

/// Translate a `zbus::Error` into a [`NexusctlError`]. Phase 1
/// recognises ServiceUnknown (exit 6) and AuthFailed (exit 3); any
/// other shape falls through to [`NexusctlError::Other`] so the
/// underlying message is still surfaced. Later phases extend this
/// to cover the full DD-008 §9 table.
pub fn from_zbus_error(err: zbus::Error) -> NexusctlError {
    match &err {
        zbus::Error::MethodError(name, detail, _) => {
            let n = name.as_str();
            let msg = detail.clone().unwrap_or_default();
            if n == "org.freedesktop.DBus.Error.ServiceUnknown" {
                NexusctlError::NexusdUnreachable(msg)
            } else if n.contains("AuthFailed")
                || n == "org.freedesktop.DBus.Error.AccessDenied"
                || n == "org.freedesktop.PolicyKit1.Error.NotAuthorized"
            {
                NexusctlError::AuthDenied(format!("{n}: {msg}"))
            } else {
                NexusctlError::Other(format!("{n}: {msg}"))
            }
        }
        // zbus surfaces address-resolution failures (no socket,
        // wrong path) and connection-setup errors as InputOutput /
        // Address. Treat those as "not running" — same operator
        // remedy.
        zbus::Error::InputOutput(_) | zbus::Error::Address(_) | zbus::Error::Handshake(_) => {
            NexusctlError::NexusdUnreachable(err.to_string())
        }
        _ => NexusctlError::Other(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_match_dd008() {
        assert_eq!(NexusctlError::Other("x".into()).exit_code(), 1);
        assert_eq!(NexusctlError::AuthDenied("x".into()).exit_code(), 3);
        assert_eq!(NexusctlError::Timeout("x".into()).exit_code(), 4);
        assert_eq!(NexusctlError::NotInteractive("x".into()).exit_code(), 5);
        assert_eq!(NexusctlError::NexusdUnreachable("x".into()).exit_code(), 6);
    }
}
