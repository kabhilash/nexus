//! Top-level command → handler dispatch. Keeps `main.rs` tiny.

use std::io::Write;

use crate::cli::{Cli, Command, IfaceSub};
use crate::commands;
use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext};
use crate::proxy::ManagerOps;

pub async fn dispatch(
    cli: &Cli,
    ops: &dyn ManagerOps,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let format = cli.global.output_format();
    let ctx = cli.global.render_context();
    match &cli.command {
        // No subcommand → DD-008 §4.1: print the same one-screen
        // status summary `status` produces.
        None => commands::status::run(ops, format, &ctx, w).await,
        Some(Command::Status) => commands::status::run(ops, format, &ctx, w).await,
        Some(Command::Iface { sub }) => match sub {
            IfaceSub::List => commands::iface::list(ops, format, &ctx, w).await,
        },
    }
}

/// Convenience wrapper for tests that already have a resolved
/// [`OutputFormat`] and [`RenderContext`] and want to bypass clap
/// parsing.
pub async fn run_command(
    command: &Command,
    format: OutputFormat,
    ctx: &RenderContext,
    ops: &dyn ManagerOps,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    match command {
        Command::Status => commands::status::run(ops, format, ctx, w).await,
        Command::Iface { sub } => match sub {
            IfaceSub::List => commands::iface::list(ops, format, ctx, w).await,
        },
    }
}
