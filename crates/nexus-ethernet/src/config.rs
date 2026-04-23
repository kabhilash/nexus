//! Ethernet-backend configuration. See DD-002 §8.1.

use std::time::Duration;

use crate::retry::RetryPolicy;

/// Which auth backend to use at startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthBackendKind {
    WpaSupplicant,
    Ead,
    None,
}

impl AuthBackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthBackendKind::WpaSupplicant => "wpa_supplicant",
            AuthBackendKind::Ead => "ead",
            AuthBackendKind::None => "none",
        }
    }
}

/// Top-level Ethernet-backend config. Matches the `[ethernet]`
/// block in `nexus.toml`.
#[derive(Debug, Clone, Copy)]
pub struct EthernetConfig {
    pub auth_backend: AuthBackendKind,
    pub retry: RetryPolicy,
}

impl Default for EthernetConfig {
    fn default() -> Self {
        Self {
            auth_backend: AuthBackendKind::WpaSupplicant,
            retry: RetryPolicy::default(),
        }
    }
}

impl EthernetConfig {
    /// Override the retry policy fields individually. Convenient
    /// for tests; production callers usually build the policy up
    /// from TOML deserialization in `nexus-daemon`.
    pub fn with_retry(
        mut self,
        initial: Duration,
        max: Duration,
        multiplier: f64,
        max_attempts: u32,
    ) -> Self {
        self.retry = RetryPolicy {
            initial,
            max,
            multiplier,
            max_attempts,
        };
        self
    }

    pub fn with_backend(mut self, backend: AuthBackendKind) -> Self {
        self.auth_backend = backend;
        self
    }
}
