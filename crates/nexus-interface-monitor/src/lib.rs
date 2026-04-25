//! Interface Monitor — network/Wi-Fi/Bluetooth/GNSS discovery and
//! lifecycle events for the rest of Nexus. See DD-001.

pub mod classify;
pub mod command;
pub mod enumerate;
pub mod metrics;
pub mod monitor;
pub mod netlink;
pub mod recover;
pub mod registry;
pub mod udev;

use nexus_core::NexusEvent;
use thiserror::Error;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub use command::MonitorCommand;
pub use monitor::MonitorTask;

/// Default depth of the command channel returned by
/// [`spawn_interface_monitor`]. Sized so a burst of wedge-recovery
/// requests from multiple Wi-Fi interfaces doesn't block, but a
/// stuck monitor task is eventually visible as a `TrySendError::Full`
/// rather than silent backpressure.
pub const COMMAND_CHANNEL_DEPTH: usize = 16;

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
/// `commands` is the receiver side of the [`MonitorCommand`]
/// channel — the Wi-Fi backend holds a matching sender so its
/// driver-wedge recovery (DD-003 §12.4) can dispatch
/// `SetAdminUp` through the monitor's rtnetlink socket. Callers
/// that don't need the channel can build it with
/// [`command_channel`] and drop the sender.
///
/// The task exits cleanly when `shutdown` is cancelled.
pub async fn spawn_interface_monitor(
    event_tx: broadcast::Sender<NexusEvent>,
    shutdown: CancellationToken,
    commands: mpsc::Receiver<MonitorCommand>,
) -> Result<JoinHandle<Result<()>>> {
    metrics::register();
    let task = MonitorTask::bootstrap(event_tx).await?;
    Ok(tokio::spawn(task.run(shutdown, commands)))
}

/// Convenience constructor for the [`MonitorCommand`] channel.
/// Callers that outlive the monitor task itself — daemons that
/// supervise the monitor through restarts, for example — should
/// build the channel here so the sender survives re-spawns.
pub fn command_channel() -> (
    mpsc::Sender<MonitorCommand>,
    mpsc::Receiver<MonitorCommand>,
) {
    mpsc::channel(COMMAND_CHANNEL_DEPTH)
}
