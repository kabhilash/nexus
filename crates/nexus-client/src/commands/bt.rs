//! `nexusctl bt …`. DD-008 §4.1 / §5.1. Read-only subset.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::{BluetoothListFilter, ManagerOps};

pub async fn adapters(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let rows = ops.list_bluetooth_adapters().await?;
    render(&rows, format, ctx, w).map_err(io_err)
}

pub async fn list(
    ops: &dyn ManagerOps,
    filter: BluetoothListFilter,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let rows = ops.list_bluetooth_devices(filter).await?;
    render(&rows, format, ctx, w).map_err(io_err)
}

pub async fn show(
    ops: &dyn ManagerOps,
    address: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let detail = ops.show_bluetooth_device(address).await?;
    render(&detail, format, ctx, w).map_err(io_err)
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
