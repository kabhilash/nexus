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

/// Interactive pairing. DD-008 §6.1.
///
/// Thin glue around [`crate::interactive::pairing::PairingFlow`]:
/// the handler asks the ops layer for a [`crate::proxy::PairingSession`],
/// constructs a `TerminalPrompt`, runs the flow, and renders the
/// outcome.
pub async fn pair<P>(
    ops: &dyn crate::proxy::ManagerOps,
    address: &str,
    timeout: std::time::Duration,
    prompt: P,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError>
where
    P: crate::interactive::pairing::Prompt + 'static,
{
    let session = ops.start_pairing(address).await?;
    let config = crate::interactive::pairing::PairingFlowConfig {
        overall_timeout: timeout,
        cancel_grace: std::time::Duration::from_secs(2),
    };
    let flow =
        crate::interactive::pairing::PairingFlow::new(prompt, session.events, session.sink, config);
    let outcome = flow.run().await;
    let outcome_str = match &outcome {
        crate::interactive::pairing::PairingOutcome::Paired => "paired",
        crate::interactive::pairing::PairingOutcome::Rejected { .. } => "rejected",
        crate::interactive::pairing::PairingOutcome::Cancelled => "cancelled",
        crate::interactive::pairing::PairingOutcome::TimedOut => "timed_out",
        crate::interactive::pairing::PairingOutcome::Failed { .. } => "failed",
    };
    render(
        &MutationOutcome {
            action: "bt pair".into(),
            subject: address.to_owned(),
            id: Some(session.job_id),
            note: Some(outcome_str.into()),
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)?;
    // Translate non-success outcomes into the DD-008 §4.3 exit
    // codes by returning a NexusctlError whose exit_code matches.
    match outcome {
        crate::interactive::pairing::PairingOutcome::Paired => Ok(()),
        crate::interactive::pairing::PairingOutcome::Cancelled => {
            Err(NexusctlError::Other {
                // Sentinel empty message → main prints nothing, exit 130.
                // We hand-roll the 130 by matching on the outcome at
                // the binary edge (see main.rs).
                raw: "__CANCELLED__".into(),
            })
        }
        crate::interactive::pairing::PairingOutcome::TimedOut => Err(NexusctlError::Timeout {
            operation: format!("pair {address}"),
            duration_s: Some(timeout.as_secs()),
        }),
        crate::interactive::pairing::PairingOutcome::Rejected { reason }
        | crate::interactive::pairing::PairingOutcome::Failed { reason } => {
            Err(NexusctlError::Other { raw: reason })
        }
    }
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
