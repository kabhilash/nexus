//! `nexusctl admin …`. Read-only subset — only `master-key-info`
//! lands in Phase 3; `rotate-master-key`, `freeze-backup`,
//! `release-backup`, `diagnostics`, `reload-config` are Phase 7.4
//! mutating calls.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::ManagerOps;

pub async fn master_key_info(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let info = ops.master_key_info().await?;
    render(&info, format, ctx, w).map_err(io_err)
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
