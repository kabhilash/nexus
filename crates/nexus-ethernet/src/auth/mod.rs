//! Pluggable wired 802.1X authentication backend. See DD-002 §§4-5.
//!
//! The trait is thin (`attach`, `authenticate`, `detach`, `state`)
//! so the Ethernet Backend's event loop stays simple: it drives
//! the backend in response to lifecycle events and consumes the
//! corresponding [`nexus_core::AuthState`] updates that the backend
//! emits asynchronously on the event bus.

pub mod mock;

#[cfg(feature = "auth-wpa_supplicant")]
pub mod wpa_supplicant;

// ead backend is deferred per DD-002 Phase 7 ("optional,
// feature-gated"). Shape:
//
//     pub mod ead { /* WiredAuthBackend for EadBackend */ }
//
// Landing item is tracked alongside DD-002 §8; the module is
// intentionally empty today so the `auth-ead` feature compiles
// without pulling in real code that hasn't been exercised.
#[cfg(feature = "auth-ead")]
pub(crate) mod ead {}

use async_trait::async_trait;
pub use nexus_core::{AuthFailureReason, AuthState};
use nexus_profile_store::Dot1xEapConfig;

use crate::error::Result;

pub use mock::{MockAuthBackend, MockScenario};

/// Wired 802.1X authentication backend.
///
/// Implementations drive an external daemon (wpa_supplicant or ead)
/// over D-Bus. State transitions are reported asynchronously via
/// `NexusEvent::EthAuthStateChanged` on the event bus; callers do
/// not poll [`WiredAuthBackend::state`] in the steady state.
#[async_trait]
pub trait WiredAuthBackend: Send + Sync {
    /// Register the interface with the auth daemon. No
    /// authentication is kicked off here.
    async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()>;

    /// Start authentication on the interface. Safe to re-call for a
    /// retry — implementations clear any prior network entry
    /// before installing the new one.
    async fn authenticate(&mut self, ifindex: u32, config: &Dot1xEapConfig) -> Result<()>;

    /// Stop authentication and unregister. Best-effort.
    async fn detach(&mut self, ifindex: u32) -> Result<()>;

    /// Point-in-time state for diagnostics / D-Bus property reads.
    async fn state(&self, ifindex: u32) -> Result<AuthState>;

    /// Stable identifier for logs and metrics.
    fn name(&self) -> &'static str;
}
