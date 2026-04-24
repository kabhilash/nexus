//! Ctrl-C / double-Ctrl-C plumbing. DD-008 §6.4.
//!
//! Two behaviours are exposed:
//! - [`first_sigint`] — a future that resolves on the first SIGINT.
//!   Wrap your mutating command in `tokio::select!` against it.
//! - [`install_double_sigint_watchdog`] — starts a background task
//!   that exits the process immediately on a second Ctrl-C
//!   received within 500 ms of the first, matching the
//!   curl / git convention for impatient operators.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Wait for the first SIGINT. Returns `Ok(())` when it arrives,
/// `Err(io::Error)` if the signal-handler install failed.
pub async fn first_sigint() -> std::io::Result<()> {
    tokio::signal::ctrl_c().await
}

/// Install the DD-008 §6.4 double-Ctrl-C watchdog. Call once at
/// the top of a mutating command. The returned flag is flipped to
/// `true` when the first SIGINT fires; a second SIGINT within
/// [`DOUBLE_SIGINT_WINDOW`] calls `std::process::exit(130)`.
///
/// Returns the flag so callers that choose to observe Ctrl-C via
/// `tokio::select!` on [`first_sigint`] can coordinate with the
/// watchdog — the flag is shared via `Arc<AtomicBool>` and
/// guaranteed to be `true` by the time the `first_sigint` future
/// resolves.
pub fn install_double_sigint_watchdog() -> Arc<AtomicBool> {
    pub const DOUBLE_SIGINT_WINDOW: Duration = Duration::from_millis(500);
    let flag = Arc::new(AtomicBool::new(false));
    let flag_c = Arc::clone(&flag);
    tokio::spawn(async move {
        // First Ctrl-C.
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        let first_at = Instant::now();
        flag_c.store(true, Ordering::SeqCst);
        // Second Ctrl-C within the window → immediate exit.
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        if first_at.elapsed() <= DOUBLE_SIGINT_WINDOW {
            std::process::exit(130);
        }
    });
    flag
}
