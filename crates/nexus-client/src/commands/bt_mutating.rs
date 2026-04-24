//! `nexusctl bt …` mutating commands. DD-008 §4.1.
//!
//! Kept separate from `commands/bt.rs` so the read-only handlers
//! stay small. Dispatch sends both paths through `commands::bt` —
//! this module's functions are exported from there.

use std::io::Write;
use std::time::Duration;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::{ManagerOps, MutationOutcome};

pub async fn power(
    ops: &dyn ManagerOps,
    hci: &str,
    on: bool,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    ops.bt_set_powered(hci, on).await?;
    let outcome = MutationOutcome {
        action: "bt power".into(),
        subject: hci.to_owned(),
        id: None,
        note: Some(if on { "on".into() } else { "off".into() }),
    };
    render(&outcome, format, ctx, w).map_err(io_err)
}

pub async fn scan(
    ops: &dyn ManagerOps,
    hci: Option<&str>,
    duration_s: u64,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let rows = ops.bt_scan(hci, Duration::from_secs(duration_s)).await?;
    render(&rows, format, ctx, w).map_err(io_err)
}

pub async fn connect(
    ops: &dyn ManagerOps,
    address: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    ops.bt_connect_device(address).await?;
    render(
        &MutationOutcome {
            action: "bt connect".into(),
            subject: address.to_owned(),
            id: None,
            note: None,
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

pub async fn disconnect(
    ops: &dyn ManagerOps,
    address: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    ops.bt_disconnect_device(address).await?;
    render(
        &MutationOutcome {
            action: "bt disconnect".into(),
            subject: address.to_owned(),
            id: None,
            note: None,
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

pub async fn forget(
    ops: &dyn ManagerOps,
    address: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    ops.bt_forget_device(address).await?;
    render(
        &MutationOutcome {
            action: "bt forget".into(),
            subject: address.to_owned(),
            id: None,
            note: None,
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

pub async fn trust(
    ops: &dyn ManagerOps,
    address: &str,
    on: bool,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    ops.bt_set_trusted(address, on).await?;
    render(
        &MutationOutcome {
            action: "bt trust".into(),
            subject: address.to_owned(),
            id: None,
            note: Some(if on { "on".into() } else { "off".into() }),
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
