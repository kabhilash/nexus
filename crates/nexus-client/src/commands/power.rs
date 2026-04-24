//! `nexusctl power get`. DD-008 §4.1.

use std::io::Write;

use serde::Serialize;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, Render, RenderContext, escape_terse, json};
use crate::proxy::ManagerOps;

/// Single-field view so every format renders consistently.
#[derive(Debug, Serialize)]
struct PowerView {
    power_state: String,
}

impl Render for PowerView {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> std::io::Result<()> {
        writeln!(w, "Power state: {}", self.power_state)
    }
    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> std::io::Result<()> {
        writeln!(w, "{}", escape_terse(&self.power_state, &ctx.separator))
    }
    fn render_json(&self, w: &mut dyn Write) -> std::io::Result<()> {
        json::write(self, w)
    }
    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> std::io::Result<()> {
        self.render_human(ctx, w)
    }
}

pub async fn get(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    // We reuse `ManagerStatus` which already carries PowerState;
    // a dedicated trait method would duplicate a property read.
    let status = ops.get_manager_status().await?;
    let view = PowerView {
        power_state: status.power_state,
    };
    crate::output::render(&view, format, ctx, w).map_err(io_err)
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
