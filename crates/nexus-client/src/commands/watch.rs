//! `nexusctl watch …` — long-running signal subscription. DD-008
//! §7.4.
//!
//! The command loops pulling events from a [`WatchStream`] until
//! one of:
//!   - the stream yields `None` (daemon disconnected / subscription
//!     dropped — clean exit 0 per DD-008 §3),
//!   - Ctrl-C fires (clean exit 0 per DD-008 §6.4's
//!     "long-running" row).
//!
//! Filter matching + subset classification happen per-event; only
//! events that pass both land on the output renderer.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext};
use crate::watch::{Filter, WatchStream, WatchSubset, filter};

/// Run the watch loop. Caller passes the `sigint` future so tests
/// can drive it deterministically; production callers pass
/// `tokio::signal::ctrl_c()`.
#[allow(clippy::too_many_arguments)]
pub async fn run<S>(
    mut stream: Box<dyn WatchStream>,
    subset: WatchSubset,
    filters: Vec<Filter>,
    format: OutputFormat,
    ctx: &RenderContext,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    sigint: S,
) -> Result<(), NexusctlError>
where
    S: std::future::Future<Output = std::io::Result<()>> + Send,
{
    if format == OutputFormat::Pretty {
        // DD-008 §5.4 says pretty is single-record; §7.4 is an
        // event stream. Warn once and proceed with the human
        // renderer (which `WatchEvent::render_pretty` delegates
        // to).
        let _ = writeln!(
            stderr,
            "nexusctl: watch doesn't support --pretty (single-record format); using human"
        );
    }
    tokio::pin!(sigint);
    loop {
        tokio::select! {
            biased;
            ev = stream.next() => match ev {
                Ok(Some(event)) => {
                    if !subset.includes(&event.kind) {
                        continue;
                    }
                    if !filter::passes(&event, &filters) {
                        continue;
                    }
                    crate::output::render(&event, format, ctx, stdout)
                        .map_err(map_io_error)?;
                    stdout.flush().map_err(map_io_error)?;
                }
                Ok(None) => {
                    let _ = writeln!(stderr, "nexusd disconnected");
                    return Ok(());
                }
                Err(e) => return Err(e),
            },
            _ = &mut sigint => {
                // Clean exit per DD-008 §6.4 "long-running" row
                // (code 0, no cancel dance).
                return Ok(());
            }
        }
    }
}

fn map_io_error(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        // stdout piped to head(1) closed — not an error.
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
