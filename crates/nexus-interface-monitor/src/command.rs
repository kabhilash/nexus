//! Operator- / backend-initiated commands that go through the
//! Interface Monitor's rtnetlink socket. See DD-001 §8 command
//! channel and DD-003 §12.4 for the driver-wedge recovery that
//! drives the first `SetAdminUp` variant.
//!
//! The monitor already owns the rtnl socket with `NLM_F_REQUEST`
//! capability, so concentrating writes here avoids each backend
//! having to open its own socket (and, per DD-001, keeps the
//! workspace's only rtnetlink writer in one place).

use tokio::sync::oneshot;

/// A request that the Interface Monitor task acts on. Senders
/// build these and drop them into the monitor's command channel;
/// the matching task arm validates and dispatches.
#[derive(Debug)]
pub enum MonitorCommand {
    /// Flip `IFF_UP` on an interface via `RTM_NEWLINK` + `ifi_change
    /// = IFF_UP`. Used by the Wi-Fi backend's driver-wedge recovery
    /// (DD-003 §12.4) to kick a stuck chipset without shelling out
    /// to `ip link`.
    SetAdminUp {
        ifindex: u32,
        up: bool,
        /// Optional ack channel. The monitor sends `Ok(())` once
        /// the kernel acknowledges the request, or an `Err` string
        /// with the errno / parse failure. Callers that fire-and-
        /// forget leave this as `None`.
        reply: Option<oneshot::Sender<Result<(), String>>>,
    },
}
