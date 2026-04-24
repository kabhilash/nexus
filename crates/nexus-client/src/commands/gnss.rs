//! `nexusctl gnss …`. DD-008 §4.1. Read-only.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::ManagerOps;

pub async fn show(
    ops: &dyn ManagerOps,
    device: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    // `gnss show` reuses the generic `iface show` plumbing; it
    // just needs to resolve the GNSS interface first when no arg
    // is given.
    let target = match device {
        Some(name) => name.to_owned(),
        None => {
            let rows = ops.list_interfaces().await?;
            let gnsss: Vec<_> = rows.iter().filter(|r| r.kind == "gnss").collect();
            match gnsss.len() {
                0 => {
                    return Err(NexusctlError::NotFound {
                        reference: "<any gnss interface>".into(),
                    });
                }
                1 => gnsss[0].iface.clone(),
                _ => {
                    let names: Vec<&str> = gnsss.iter().map(|r| r.iface.as_str()).collect();
                    return Err(NexusctlError::InvalidArgument {
                        message: format!(
                            "multiple GNSS devices available; specify one: {}",
                            names.join(", ")
                        ),
                    });
                }
            }
        }
    };
    crate::commands::iface::show(ops, &target, Some("gnss"), format, ctx, w).await
}

pub async fn satellites(
    ops: &dyn ManagerOps,
    device: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let view = ops.gnss_satellites(device).await?;
    render(&view, format, ctx, w).map_err(io_err)
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
