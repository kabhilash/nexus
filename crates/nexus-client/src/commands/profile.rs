//! `nexusctl profile …`. DD-008 §4.1. Read-only subset.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::ManagerOps;

pub async fn list(
    ops: &dyn ManagerOps,
    kind: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let rows = ops.list_profiles(kind).await?;
    render(&rows, format, ctx, w).map_err(io_err)
}

pub async fn show(
    ops: &dyn ManagerOps,
    reference: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let detail = ops.show_profile(reference).await?;
    render(&detail, format, ctx, w).map_err(io_err)
}

/// `profile export` writes the rendered TOML to stdout verbatim —
/// no wrapping. Output format flags are ignored (TOML is TOML).
pub async fn export(
    ops: &dyn ManagerOps,
    reference: &str,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let toml = ops.export_profile(reference).await?;
    w.write_all(toml.as_bytes()).map_err(io_err)?;
    if !toml.ends_with('\n') {
        let _ = w.write_all(b"\n");
    }
    Ok(())
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
