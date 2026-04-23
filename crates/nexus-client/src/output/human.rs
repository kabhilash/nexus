//! Human-friendly renderers. comfy-table for tables; vertical
//! key/value for single records. DD-008 §5.1.
//!
//! The state-prefix column from §5.1's example output is deferred
//! to Phase 2 — it requires per-kind state-machine context that
//! Phase 1's `InterfaceSummary` doesn't carry. For now `iface list`
//! emits IFACE / KIND / STATE / MAC.

use std::io::{self, Write};

use comfy_table::{Cell, ContentArrangement, Table};

use crate::proxy::{InterfaceSummary, ManagerStatus};

pub fn render_status(status: &ManagerStatus, w: &mut dyn Write) -> io::Result<()> {
    // Vertical key/value layout — single-record displays use this
    // shape per §5.1 ("Single-record displays use a vertical
    // layout"). Right-align the keys for visual alignment without a
    // table border.
    let key_w = "Master key:".len();
    writeln!(w, "{:<key_w$} {}", "Version:", status.version)?;
    writeln!(w, "{:<key_w$} {}", "Power state:", status.power_state)?;
    writeln!(w, "{:<key_w$} {}", "Interfaces:", status.interface_count)?;
    writeln!(
        w,
        "{:<key_w$} {} wifi, {} ethernet, {} bluetooth",
        "Profiles:",
        status.wifi_profile_count,
        status.ethernet_profile_count,
        status.bluetooth_profile_count
    )?;
    writeln!(w, "{:<key_w$} {}", "Master key:", status.master_key_source)?;
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

pub fn render_iface_list(rows: &[InterfaceSummary], w: &mut dyn Write) -> io::Result<()> {
    if rows.is_empty() {
        writeln!(w, "no interfaces")?;
        return Ok(());
    }
    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(vec![
            Cell::new("IFACE"),
            Cell::new("KIND"),
            Cell::new("STATE"),
            Cell::new("MAC"),
        ]);
    for r in rows {
        let mac = r.mac.as_deref().unwrap_or("—");
        table.add_row(vec![
            Cell::new(&r.iface),
            Cell::new(&r.kind),
            Cell::new(&r.state),
            Cell::new(mac),
        ]);
    }
    writeln!(w, "{table}")
}
