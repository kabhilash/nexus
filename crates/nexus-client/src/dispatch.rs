//! Top-level command → handler dispatch. Keeps `main.rs` tiny.

use std::io::Write;

use crate::cli::{Cli, Command, IfaceSub};
use crate::commands;
use crate::errors::NexusctlError;
use crate::output::OutputFormat;
use crate::proxy::ManagerOps;

pub async fn dispatch(
    cli: &Cli,
    ops: &dyn ManagerOps,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let format = cli.global.output_format();
    match &cli.command {
        // No subcommand → DD-008 §4.1: print the same one-screen
        // status summary `status` produces.
        None => commands::status::run(ops, format, w).await,
        Some(Command::Status) => commands::status::run(ops, format, w).await,
        Some(Command::Iface { sub }) => match sub {
            IfaceSub::List => commands::iface::list(ops, format, w).await,
        },
    }
}

/// Convenience wrapper used by binary tests that already have a
/// resolved [`OutputFormat`] (i.e., bypassing CLI parsing). Phase 1
/// keeps both paths identical; future phases may diverge if
/// `nexusctl shell` ends up reusing the dispatcher with its own
/// format-resolution rules.
pub async fn run_command(
    command: &Command,
    format: OutputFormat,
    ops: &dyn ManagerOps,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    match command {
        Command::Status => commands::status::run(ops, format, w).await,
        Command::Iface { sub } => match sub {
            IfaceSub::List => commands::iface::list(ops, format, w).await,
        },
    }
}
