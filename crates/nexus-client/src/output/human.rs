//! Human renderer. DD-008 §5.1.
//!
//! Lists use comfy-table; single records use a vertical key/value
//! block. `iface list` grows a `S` (state-prefix) column populated
//! by [`crate::state_prefix`].
//!
//! Colours are not yet wired — the colour choice is plumbed here
//! via [`RenderContext`] and ready for a future commit; today the
//! renderer is plain-text so the snapshot tests stay stable.

use std::io::{self, Write};

use comfy_table::{Cell, ContentArrangement, Table, presets::NOTHING};

use crate::output::{Render, RenderContext};
use crate::proxy::{InterfaceSummary, ManagerStatus};
use crate::state_prefix;

impl Render for ManagerStatus {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        render_status_block(self, w)
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        super::terse::render_status_terse(self, ctx, w)
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        // Status is already a single-record view; pretty matches
        // the human block today. Future phases may add gaps /
        // dim hint text.
        render_status_block(self, w)
    }
}

/// Translate the camelCase wire-state strings into a short
/// human-friendly label. Keep the wire string visible when we don't
/// recognise it (forward-compat with daemons newer than this client).
fn format_connectivity(state: &str) -> String {
    match state {
        "" => "unavailable (older daemon)".to_owned(),
        "internetUnknown" => "unknown (probe not yet run)".to_owned(),
        "internetOnline" => "online".to_owned(),
        "internetCaptivePortal" => "captive portal".to_owned(),
        "internetOffline" => "offline".to_owned(),
        other => other.to_owned(),
    }
}

fn render_status_block(status: &ManagerStatus, w: &mut dyn Write) -> io::Result<()> {
    let key_w = "Capabilities:".len();
    writeln!(w, "{:<key_w$} {}", "Version:", status.version)?;
    writeln!(w, "{:<key_w$} {}", "Power state:", status.power_state)?;
    writeln!(
        w,
        "{:<key_w$} {} ({} ethernet, {} wifi, {} bluetooth, {} gnss)",
        "Interfaces:",
        status.interface_count,
        status.ethernet_count,
        status.wifi_count,
        status.bluetooth_count,
        status.gnss_count,
    )?;
    writeln!(
        w,
        "{:<key_w$} {} wifi, {} ethernet, {} bluetooth",
        "Profiles:",
        status.wifi_profile_count,
        status.ethernet_profile_count,
        status.bluetooth_profile_count
    )?;
    writeln!(
        w,
        "{:<key_w$} {}",
        "BlueZ:",
        if status.bluez_available {
            "reachable"
        } else {
            "not reachable"
        }
    )?;
    writeln!(
        w,
        "{:<key_w$} {}",
        "gpsd:",
        if status.gpsd_available {
            "reachable"
        } else {
            "not reachable"
        }
    )?;
    writeln!(w, "{:<key_w$} {}", "Master key:", status.master_key_source)?;
    writeln!(
        w,
        "{:<key_w$} {}",
        "Internet:",
        format_connectivity(&status.internet_connectivity)
    )?;
    if !status.api_capabilities.is_empty() {
        writeln!(
            w,
            "{:<key_w$} {}",
            "Capabilities:",
            status.api_capabilities.join(", ")
        )?;
    }
    Ok(())
}

impl Render for Vec<InterfaceSummary> {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        if self.is_empty() {
            writeln!(w, "no interfaces")?;
            return Ok(());
        }
        let mut table = Table::new();
        table
            .load_style(NOTHING)
            .set_content_arrangement(ContentArrangement::Disabled)
            .set_header(vec![
                Cell::new(""),
                Cell::new("IFACE"),
                Cell::new("KIND"),
                Cell::new("STATE"),
                Cell::new("MAC"),
            ]);
        for r in self {
            let prefix = state_prefix::classify(r).render();
            let mac = r.mac.as_deref().unwrap_or("—");
            table.add_row(vec![
                Cell::new(prefix),
                Cell::new(&r.iface),
                Cell::new(&r.kind),
                Cell::new(&r.state),
                Cell::new(mac),
            ]);
        }
        // comfy-table pads the last column with trailing spaces.
        // Strip them so shell consumers and snapshot tests get the
        // minimal, predictable output.
        for line in table.to_string().lines() {
            writeln!(w, "{}", line.trim_end())?;
        }
        Ok(())
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        super::terse::render_iface_list_terse(self, ctx, w)
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        super::pretty::render_iface_list_pretty(self, ctx, w)
    }
}
