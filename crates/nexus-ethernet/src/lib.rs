//! Ethernet Backend. See DD-002.
//!
//! Tracks every Ethernet interface surfaced by the Interface
//! Monitor, loads its profile from the Profile Store, drives
//! 802.1X authentication (when configured) via a pluggable
//! [`auth::WiredAuthBackend`], and emits `EthLinkReady` /
//! `EthLinkLost` so systemd-networkd can begin (or tear down)
//! layer-3 configuration.

pub mod auth;
pub mod backend;
pub mod config;
pub mod error;
pub mod lifecycle;
pub mod metrics;
pub mod profile;
pub mod retry;

pub use auth::{AuthFailureReason, AuthState, MockAuthBackend, MockScenario, WiredAuthBackend};
pub use backend::EthernetBackend;
pub use config::{AuthBackendKind, EthernetConfig};
pub use error::{EthernetError, Result};
pub use lifecycle::{EthInterfaceEntry, EthInterfaceState};
pub use profile::{default_ethernet_profile, profile_requires_auth};
pub use retry::{RetryPolicy, is_retriable};

use std::sync::Arc;

use nexus_core::NexusEvent;
use nexus_profile_store::ProfileStore;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Spawn the Ethernet Backend task. Setup is synchronous; the
/// returned `JoinHandle` resolves when the event bus closes or
/// `shutdown` is cancelled.
pub async fn spawn_ethernet_backend(
    event_tx: broadcast::Sender<NexusEvent>,
    profile_store: Arc<dyn ProfileStore>,
    auth_backend: Option<Box<dyn WiredAuthBackend>>,
    config: EthernetConfig,
    shutdown: CancellationToken,
) -> Result<JoinHandle<Result<()>>> {
    metrics::register();
    let backend = EthernetBackend::new(event_tx, profile_store, auth_backend, config);
    Ok(tokio::spawn(backend.run(shutdown)))
}
