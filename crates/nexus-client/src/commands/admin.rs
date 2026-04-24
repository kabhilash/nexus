//! `nexusctl admin …`. Read-only subset — only `master-key-info`
//! lands in Phase 3; `rotate-master-key`, `freeze-backup`,
//! `release-backup`, `diagnostics`, `reload-config` are Phase 7.4
//! mutating calls.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::ManagerOps;

use crate::proxy::MutationOutcome;

pub async fn master_key_info(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let info = ops.master_key_info().await?;
    render(&info, format, ctx, w).map_err(io_err)
}

pub async fn rotate_master_key(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let job_id = ops.rotate_master_key().await?;
    render(
        &MutationOutcome {
            action: "admin rotate-master-key".into(),
            subject: "profile store".into(),
            id: Some(job_id),
            note: Some(
                "rotation continues asynchronously; watch `nexusctl watch events` for \
                 `master-key-rotated`"
                    .into(),
            ),
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

pub async fn freeze_backup(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let lease = ops.freeze_for_backup().await?;
    render(
        &MutationOutcome {
            action: "admin freeze-backup".into(),
            subject: "lease".into(),
            id: Some(lease.clone()),
            note: Some(format!("use `admin release-backup {lease}` to release")),
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

pub async fn release_backup(
    ops: &dyn ManagerOps,
    lease: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    ops.release_backup_lease(lease).await?;
    render(
        &MutationOutcome {
            action: "admin release-backup".into(),
            subject: lease.to_owned(),
            id: None,
            note: None,
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

/// `diagnostics` stub. DD-008 §4.1 spec has the command streaming
/// a bundle fd to stdout or `--out`; the daemon-side D-Bus method
/// returns an fd that needs careful zbus fd handling, and full
/// fd-streaming lands alongside the Phase 7.5 interactive work.
/// Today we fail with a clear message so shell scripts can branch
/// on it rather than silently producing an empty file.
pub fn diagnostics_stub(
    out: Option<&std::path::Path>,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let hint = match out {
        Some(p) => format!("diagnostics would stream to {}", p.display()),
        None => "diagnostics would stream to stdout".into(),
    };
    let _ = writeln!(
        w,
        "admin diagnostics is a planned feature (fd streaming); {hint}"
    );
    Err(NexusctlError::Unsupported {
        detail: "admin diagnostics lands in a later phase (fd streaming)".into(),
    })
}

pub async fn reload_config(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let report = ops.reload_config().await?;
    render(&report, format, ctx, w).map_err(io_err)
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
