//! PolicyKit authorization. See DD-006 §10.
//!
//! Every mutating D-Bus method funnels through [`AuthChecker::check`]
//! before reaching a backend or the profile store. The production
//! impl is [`PolicyKitChecker`], which talks to
//! `org.freedesktop.PolicyKit1`. Tests use [`AlwaysAllowChecker`],
//! [`AlwaysDenyChecker`], or [`PolicyMapChecker`] to script the
//! allowed/denied combinations.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use zbus::zvariant::Value;

/// PolicyKit action identifiers — the DD-006 §10.1 table.
pub mod actions {
    pub const READ: &str = "fi.nexus.read";
    pub const SCAN: &str = "fi.nexus.scan";
    pub const CONNECT: &str = "fi.nexus.connect";
    pub const PROFILE_ADD: &str = "fi.nexus.profile.add";
    pub const PROFILE_MODIFY: &str = "fi.nexus.profile.modify";
    pub const PROFILE_READ_CREDENTIALS: &str = "fi.nexus.profile.read_credentials";
    pub const SET_POWER: &str = "fi.nexus.set_power";
    pub const ADMIN: &str = "fi.nexus.admin";
}

/// Authorization decision returned by an [`AuthChecker`]. Mirrors
/// PolicyKit's outcome enumeration with the cases Nexus actually
/// inspects: authorized vs not. We collapse `auth_*_keep` returns
/// (which PolicyKit uses for "authorized for the next 5 minutes")
/// into `Authorized` because the PolicyKit-side caching is not the
/// D-Bus layer's concern — by the time we see `is_authorized = true`,
/// the call has been blessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDecision {
    Authorized,
    Denied,
}

impl AuthDecision {
    pub fn is_authorized(self) -> bool {
        matches!(self, AuthDecision::Authorized)
    }
}

/// Authorization-checker abstraction. The trait lets tests inject
/// scripted policy without standing up a PolicyKit daemon.
#[async_trait]
pub trait AuthChecker: Send + Sync {
    /// Check whether `sender` is authorized for `action`.
    /// Returns `Authorized` on PolicyKit's "yes" / `auth_*` outcomes;
    /// `Denied` otherwise.
    async fn check(&self, action: &str, sender: &str) -> AuthDecision;
}

// ---------------------------------------------------------------------------
// Production impl: PolicyKit over zbus
// ---------------------------------------------------------------------------

/// Real PolicyKit checker. Talks to
/// `org.freedesktop.PolicyKit1` on the supplied connection (which
/// must be on the system bus in production). Constructed lazily —
/// the proxy is built on first use so a test or development setup
/// without PolicyKit can still load the rest of the service.
pub struct PolicyKitChecker {
    connection: zbus::Connection,
}

impl PolicyKitChecker {
    pub fn new(connection: zbus::Connection) -> Self {
        Self { connection }
    }
}

#[async_trait]
impl AuthChecker for PolicyKitChecker {
    async fn check(&self, action: &str, sender: &str) -> AuthDecision {
        // PolicyKit's CheckAuthorization signature:
        //   subject:   (sa{sv})       — { "system-bus-name", { "name": <s> } } for unique-bus subjects
        //   action_id: s
        //   details:   a{ss}
        //   flags:     u   — 0 = none; 1 = AllowUserInteraction
        //   cancellation_id: s
        // returns ( authorized: b, challenge: b, details: a{ss} )
        //
        // Every Nexus mutating-method check passes the unique bus
        // name of the caller. We don't pass `AllowUserInteraction`
        // because Nexus is server-side; we expect PolicyKit's
        // result to be either yes (root / configured group) or no
        // (the Nexus operator guide tells deployments to write a
        // PolicyKit JS rule that allows their nexus-admin group).

        let subject_kind = "system-bus-name";
        let mut subject_details: HashMap<&str, Value<'_>> = HashMap::new();
        subject_details.insert("name", Value::new(sender.to_owned()));
        let subject = (subject_kind, subject_details);
        let details: HashMap<&str, &str> = HashMap::new();
        let flags: u32 = 0;
        let cancellation: &str = "";

        let reply = self
            .connection
            .call_method(
                Some("org.freedesktop.PolicyKit1"),
                "/org/freedesktop/PolicyKit1/Authority",
                Some("org.freedesktop.PolicyKit1.Authority"),
                "CheckAuthorization",
                &(subject, action, details, flags, cancellation),
            )
            .await;

        match reply {
            Ok(msg) => {
                // Reply tuple is `(bbsa{ss})` — actually `(bba{ss})`
                // depending on the PK version: (is_authorized,
                // is_challenge, details).
                let body = msg.body();
                if let Ok((is_authorized, _challenge, _details)) =
                    body.deserialize::<(bool, bool, HashMap<String, String>)>()
                {
                    if is_authorized {
                        AuthDecision::Authorized
                    } else {
                        AuthDecision::Denied
                    }
                } else {
                    tracing::warn!(action, "PolicyKit reply did not deserialize");
                    AuthDecision::Denied
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, action, "PolicyKit CheckAuthorization failed");
                AuthDecision::Denied
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Test mocks
// ---------------------------------------------------------------------------

/// Always returns `Authorized`. Useful for unit tests of the read
/// path or as the default when PolicyKit is intentionally bypassed
/// (e.g. development).
pub struct AlwaysAllowChecker;

#[async_trait]
impl AuthChecker for AlwaysAllowChecker {
    async fn check(&self, _action: &str, _sender: &str) -> AuthDecision {
        AuthDecision::Authorized
    }
}

/// Always returns `Denied`. Useful for verifying the deny path of
/// every mutating method.
pub struct AlwaysDenyChecker;

#[async_trait]
impl AuthChecker for AlwaysDenyChecker {
    async fn check(&self, _action: &str, _sender: &str) -> AuthDecision {
        AuthDecision::Denied
    }
}

/// Configurable per-action policy map. The `Mutex` lets tests
/// flip decisions mid-flight (e.g. simulate a credential going
/// stale).
pub struct PolicyMapChecker {
    pub policies: Mutex<HashMap<String, AuthDecision>>,
    pub default_decision: AuthDecision,
}

impl PolicyMapChecker {
    pub fn new(default_decision: AuthDecision) -> Self {
        Self {
            policies: Mutex::new(HashMap::new()),
            default_decision,
        }
    }

    pub fn allow(&self, action: &str) {
        self.policies
            .lock()
            .unwrap()
            .insert(action.to_owned(), AuthDecision::Authorized);
    }

    pub fn deny(&self, action: &str) {
        self.policies
            .lock()
            .unwrap()
            .insert(action.to_owned(), AuthDecision::Denied);
    }
}

#[async_trait]
impl AuthChecker for PolicyMapChecker {
    async fn check(&self, action: &str, _sender: &str) -> AuthDecision {
        self.policies
            .lock()
            .unwrap()
            .get(action)
            .copied()
            .unwrap_or(self.default_decision)
    }
}

/// Convenience: build an `Arc<dyn AuthChecker>` that always
/// allows.
pub fn always_allow() -> Arc<dyn AuthChecker> {
    Arc::new(AlwaysAllowChecker)
}

/// Convenience: build an `Arc<dyn AuthChecker>` that always
/// denies.
pub fn always_deny() -> Arc<dyn AuthChecker> {
    Arc::new(AlwaysDenyChecker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn always_allow_returns_authorized() {
        let c = AlwaysAllowChecker;
        assert_eq!(
            c.check(actions::SCAN, ":1.42").await,
            AuthDecision::Authorized
        );
    }

    #[tokio::test]
    async fn always_deny_returns_denied() {
        let c = AlwaysDenyChecker;
        assert_eq!(c.check(actions::SCAN, ":1.42").await, AuthDecision::Denied);
    }

    #[tokio::test]
    async fn policy_map_falls_through_to_default() {
        let c = PolicyMapChecker::new(AuthDecision::Denied);
        c.allow(actions::SCAN);
        assert_eq!(
            c.check(actions::SCAN, ":1.1").await,
            AuthDecision::Authorized
        );
        assert_eq!(c.check(actions::ADMIN, ":1.1").await, AuthDecision::Denied);
    }

    #[tokio::test]
    async fn policy_map_can_be_reconfigured_at_runtime() {
        let c = PolicyMapChecker::new(AuthDecision::Authorized);
        c.deny(actions::ADMIN);
        assert_eq!(c.check(actions::ADMIN, ":1.1").await, AuthDecision::Denied);
        c.allow(actions::ADMIN);
        assert_eq!(
            c.check(actions::ADMIN, ":1.1").await,
            AuthDecision::Authorized
        );
    }
}
