//! GNSS Backend. See `dd-005-gnss-backend.md`.
//!
//! Public entry point is [`spawn_gnss_backend`], which wires up the
//! [`GpsdClient`], the event bus, and a small command channel, then
//! drops a running task in a [`GnssBackendHandle`].
//!
//! Two-tier event flow (DD-005 §5.1):
//! - the `GpsdClient` emits raw
//!   [`nexus_core::NexusEvent::GnssTpvReceived`] /
//!   [`nexus_core::NexusEvent::GnssSatellites`];
//! - the backend subscribes, applies the §7 quality filter and §7.3
//!   emission policy, and re-emits
//!   [`nexus_core::NexusEvent::GnssFixChanged`].
//!
//! The backend never subscribes to its own `GnssFixChanged` — no
//! subscribe-to-own-emission loop by construction.

use std::sync::Arc;

use nexus_core::NexusEvent;
use nexus_profile_store::ProfileStore;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod backend;
pub mod config;
pub mod errors;
pub mod fix;
pub mod gpsd;
pub mod lifecycle;
pub mod metrics;
pub mod profile;

pub use backend::{GnssBackend, GnssCommand};
pub use config::{GnssConfig, GnssDefaults, PowerState};
pub use errors::{GnssError, Result};
pub use fix::{EffectiveProfile, FilterReason, FixMode, GnssFix, SatInfo, fix_quality_ok};
pub use gpsd::{GpsdClient, JsonGpsdClient, MockGpsdClient};
pub use lifecycle::{GnssDeviceEntry, GnssDeviceState, Outcome, SuppressReason};
pub use profile::hydrate;

/// Handle returned by [`spawn_gnss_backend`]. Drop the handle (or
/// cancel `shutdown`) to stop the backend.
pub struct GnssBackendHandle {
    pub join: JoinHandle<Result<()>>,
    pub shutdown: CancellationToken,
    pub cmd_tx: mpsc::Sender<GnssCommand>,
}

/// Spawn the GNSS Backend event loop.
pub fn spawn_gnss_backend(
    gpsd: Arc<dyn GpsdClient>,
    profile_store: Arc<dyn ProfileStore>,
    event_tx: broadcast::Sender<NexusEvent>,
    config: GnssConfig,
) -> GnssBackendHandle {
    metrics::register();
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let backend = GnssBackend::new(
        gpsd,
        profile_store,
        event_tx,
        cmd_tx.clone(),
        cmd_rx,
        config,
    );
    let shutdown = CancellationToken::new();
    let shutdown_child = shutdown.clone();
    let join = tokio::spawn(async move { backend.run(shutdown_child).await });
    GnssBackendHandle {
        join,
        shutdown,
        cmd_tx,
    }
}
