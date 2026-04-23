//! Wi-Fi Backend. See `docs/dd-003-wifi-backend.md`.
//!
//! This crate owns the per-interface Wi-Fi lifecycle: scan
//! scheduling, profile matching, connection flow, retry/blacklist,
//! roaming, and supplicant crash recovery. IP-layer configuration
//! is systemd-networkd's responsibility and lives outside Nexus.
//!
//! The public entry point is [`spawn_wifi_backend`]. Callers wire
//! up the `NexusEvent` bus, a supplicant implementation (typically
//! [`supplicant::MockSupplicant`] in tests or the
//! `wpa_supplicant`-backed real one in production), and a
//! [`ProfileStore`]. The backend
//! drains events on its own task; the returned
//! [`WifiBackendHandle`] exposes power-state mutation and the
//! shutdown token.

use std::sync::Arc;

use nexus_core::NexusEvent;
use nexus_profile_store::ProfileStore;
use tokio::sync::{RwLock, broadcast};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod backend;
pub mod error;
pub mod lifecycle;
pub mod metrics;
pub mod power;
pub mod profile;
pub mod retry;
pub mod roam;
pub mod scan;
pub mod select;
pub mod supplicant;
pub mod types;

pub use backend::{WifiBackend, WifiConfig};
pub use error::{Result, WifiError};
pub use power::PowerState;
pub use supplicant::{SupplicantEvent, WifiSupplicantBackend};

/// Handle returned by [`spawn_wifi_backend`]. Drop the handle to
/// stop the backend — the [`CancellationToken`] it holds is clone
/// of the one passed to `run`.
pub struct WifiBackendHandle {
    pub join: JoinHandle<Result<()>>,
    pub shutdown: CancellationToken,
    pub power: Arc<RwLock<PowerState>>,
}

/// Spawn the Wi-Fi backend event loop. See DD-003 §§3, 5, 6, 7.
pub fn spawn_wifi_backend(
    event_tx: broadcast::Sender<NexusEvent>,
    supplicant_tx: broadcast::Sender<SupplicantEvent>,
    supplicant: Box<dyn WifiSupplicantBackend>,
    profile_store: Arc<dyn ProfileStore>,
    config: WifiConfig,
) -> WifiBackendHandle {
    metrics::register();
    let backend = WifiBackend::new(event_tx, supplicant_tx, supplicant, profile_store, config);
    let power = backend.power_handle();
    let shutdown = CancellationToken::new();
    let shutdown_child = shutdown.clone();
    let join = tokio::spawn(async move { backend.run(shutdown_child).await });
    WifiBackendHandle {
        join,
        shutdown,
        power,
    }
}

/// Test helper: build a [`SecretString`](nexus_profile_store::SecretString)
/// from a plain string literal. Kept here (rather than duplicated
/// per-module) so the unit tests for `profile.rs` and `select.rs`
/// share a single spelling.
#[cfg(test)]
pub(crate) fn secretstring(value: &str) -> nexus_profile_store::SecretString {
    nexus_profile_store::SecretString::from(value)
}
