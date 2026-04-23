//! gpsd abstraction. See DD-005 §4.1.
//!
//! Steady-state TPV / SKY flow is push-driven: the concrete impl
//! emits [`nexus_core::NexusEvent::GnssTpvReceived`] /
//! [`nexus_core::NexusEvent::GnssSatellites`] onto the event bus as
//! gpsd lines arrive. The [`GpsdClient`] trait only exposes the
//! control-plane actions (connect, add/remove device, current-fix
//! diagnostic, name).

pub mod json_client;
pub mod messages;
pub mod mock;
pub mod parse;

use async_trait::async_trait;

use crate::errors::Result;
use crate::fix::GnssFix;

pub use json_client::JsonGpsdClient;
pub use mock::MockGpsdClient;

/// The control-plane API for a gpsd client. Everything streaming
/// goes through the event bus — see the module-level comment.
#[async_trait]
pub trait GpsdClient: Send + Sync {
    /// Connect or reconnect. Idempotent: no-op when already up.
    /// Emits [`nexus_core::NexusEvent::GnssGpsdConnected`] on success.
    async fn connect(&self) -> Result<()>;

    /// Cheap check; called at 1 Hz by the supervisor.
    fn is_connected(&self) -> bool;

    /// Ask gpsd to watch a device (`?DEVICE={"path":"…","activate":true}`).
    /// Idempotent at gpsd's level.
    async fn add_device(&self, path: &str) -> Result<()>;

    /// Request gpsd stop watching a device.
    async fn remove_device(&self, path: &str) -> Result<()>;

    /// Diagnostic: current cached fix, if any. Not used in the
    /// steady-state event loop.
    async fn current_fix(&self, path: &str) -> Result<Option<GnssFix>>;

    /// Backend identifier for logs and metrics.
    fn name(&self) -> &'static str;
}
