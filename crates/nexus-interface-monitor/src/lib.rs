//! Interface Monitor — network/Wi-Fi/Bluetooth/GNSS discovery and
//! lifecycle events for the rest of Nexus. See DD-001.

pub mod enumerate;
pub mod monitor;
pub mod netlink;
pub mod registry;
pub mod udev;

use nexus_core::NexusEvent;
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use monitor::MonitorTask;

/// Result alias used across the crate. The single error type below
/// covers netlink I/O, parse failures, and genl resolution.
pub type Result<T> = std::result::Result<T, MonitorError>;

/// Top-level failure mode for the monitor. Setup errors surface via
/// [`spawn_interface_monitor`]; runtime errors propagate out of the
/// task's [`JoinHandle`].
#[derive(Debug, Error)]
pub enum MonitorError {
    #[error("netlink I/O: {0}")]
    Io(#[from] std::io::Error),

    #[error("netlink parse: {0}")]
    Parse(#[from] netlink::ParseError),

    #[error("generic-netlink resolve: {0}")]
    Resolve(#[from] netlink::genl::ResolveError),

    /// The kernel returned a negative errno for an rtnl dump request.
    #[error("rtnl dump returned errno {0}")]
    RtnlDumpFailed(i32),
}

/// Spawn the Interface Monitor task.
///
/// The setup phase (socket open, nl80211 family resolution, cold-boot
/// enumeration) happens before the returned `JoinHandle` starts
/// processing the event loop. Setup errors are returned directly;
/// runtime errors propagate through the task's result.
///
/// The task exits cleanly when `shutdown` is cancelled.
pub async fn spawn_interface_monitor(
    event_tx: broadcast::Sender<NexusEvent>,
    shutdown: CancellationToken,
) -> Result<JoinHandle<Result<()>>> {
    let task = MonitorTask::bootstrap(event_tx).await?;
    Ok(tokio::spawn(task.run(shutdown)))
}
