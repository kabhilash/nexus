//! `nexusctl iface …` and the per-kind list aliases (`eth list`,
//! `wifi list`, `gnss list`). DD-008 §5.1.
//!
//! Each handler:
//! 1. Pulls its data via `ManagerOps`.
//! 2. (Optionally) filters by kind.
//! 3. Hands the view off to `output::render`.

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
    let rows = ops.list_interfaces().await?;
    let filtered: Vec<_> = match kind {
        // "wifi" in nexusctl args covers the "wireless" wire label
        // too (DD-001 §5 calls the NL80211 kind wireless).
        Some("wifi") => rows
            .into_iter()
            .filter(|r| r.kind == "wifi" || r.kind == "wireless")
            .collect(),
        Some(k) => rows.into_iter().filter(|r| r.kind == k).collect(),
        None => rows,
    };
    render(&filtered, format, ctx, w).map_err(map_io_error)
}

/// `iface show <iface>` / `eth show` / `wifi show` share this
/// handler. `expected_kind` lets the alias commands surface a
/// useful error when the operator points at the wrong kind.
pub async fn show(
    ops: &dyn ManagerOps,
    iface: &str,
    expected_kind: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let detail = ops.show_interface(iface).await?;
    if let Some(k) = expected_kind {
        let ok = match k {
            "wifi" => detail.summary.kind == "wifi" || detail.summary.kind == "wireless",
            other => detail.summary.kind == other,
        };
        if !ok {
            return Err(NexusctlError::InvalidArgument {
                message: format!(
                    "interface `{iface}` is kind `{}`; expected `{k}`",
                    detail.summary.kind
                ),
            });
        }
    }
    render(&detail, format, ctx, w).map_err(map_io_error)
}

/// `wifi show` without an interface argument. When exactly one
/// Wi-Fi interface is registered the handler uses it automatically;
/// with zero or two+ it returns the DD-008 §4.1 "usage error with
/// list" shape (exit 2 is mapped by the dispatcher; we express
/// that via `InvalidArgument`).
pub async fn show_wifi(
    ops: &dyn ManagerOps,
    iface: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let target = match iface {
        Some(name) => name.to_owned(),
        None => {
            let rows = ops.list_interfaces().await?;
            let wifis: Vec<_> = rows
                .iter()
                .filter(|r| r.kind == "wifi" || r.kind == "wireless")
                .collect();
            match wifis.len() {
                0 => {
                    return Err(NexusctlError::NotFound {
                        reference: "<any wifi interface>".into(),
                    });
                }
                1 => wifis[0].iface.clone(),
                _ => {
                    let names: Vec<&str> = wifis.iter().map(|r| r.iface.as_str()).collect();
                    return Err(NexusctlError::InvalidArgument {
                        message: format!(
                            "multiple Wi-Fi interfaces available; specify one: {}",
                            names.join(", ")
                        ),
                    });
                }
            }
        }
    };
    show(ops, &target, Some("wifi"), format, ctx, w).await
}

/// DD-008 §4.1 `iface events`: the daemon doesn't yet keep a ring
/// buffer of historical NexusEvent entries, so this handler is a
/// stub that points operators at `nexusctl watch` (Phase 7.6) and
/// exits with "general failure" per the prompt.
pub fn events_stub(w: &mut dyn Write) -> Result<(), NexusctlError> {
    let _ = writeln!(
        w,
        "event history is a planned feature; use `nexusctl watch` for live events"
    );
    Err(NexusctlError::Unsupported {
        detail: "iface events is not yet available; use `nexusctl watch`".into(),
    })
}

fn map_io_error(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
