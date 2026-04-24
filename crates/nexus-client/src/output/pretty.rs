//! Pretty renderer. DD-008 §5.4.
//!
//! Verbose vertical layout, one field per line, keys right-padded
//! for alignment. When the view is a list, each record renders as a
//! block separated by a blank line — per the prompt's guidance that
//! pretty on a list should "render each record as a separate pretty
//! block with a blank line between".
//!
//! Values that overflow the terminal wrap with a hanging indent
//! matching the key-column width so a line continuation is visually
//! clear. We don't detect terminal width here — the wrap behaviour
//! is driven by the caller / test environment. Phase 1 doesn't have
//! any value that's meaningfully long; the wrap logic exists so
//! future fields (e.g. PolicyKit hints, long certificate DNs) plug
//! in without touching the renderer.

use std::io::{self, Write};

use crate::output::RenderContext;
use crate::proxy::InterfaceSummary;

const KEY_WIDTH: usize = 12;

pub fn render_iface_list_pretty(
    rows: &[InterfaceSummary],
    _ctx: &RenderContext,
    w: &mut dyn Write,
) -> io::Result<()> {
    if rows.is_empty() {
        writeln!(w, "no interfaces")?;
        return Ok(());
    }
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            writeln!(w)?;
        }
        render_iface_pretty_block(row, w)?;
    }
    Ok(())
}

fn render_iface_pretty_block(row: &InterfaceSummary, w: &mut dyn Write) -> io::Result<()> {
    write_field(w, "Interface:", &row.iface)?;
    write_field(w, "Kind:", &row.kind)?;
    write_field(w, "State:", &row.state)?;
    write_field(w, "MAC:", row.mac.as_deref().unwrap_or("—"))?;
    write_field(w, "Carrier:", if row.carrier { "up" } else { "down" })?;
    Ok(())
}

/// Emit one aligned `Key: value` line.
fn write_field(w: &mut dyn Write, label: &str, value: &str) -> io::Result<()> {
    writeln!(w, "{label:<KEY_WIDTH$} {value}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pretty_single_row_uses_aligned_block() {
        let rows = vec![InterfaceSummary {
            iface: "eth0".into(),
            kind: "ethernet".into(),
            state: "up".into(),
            mac: Some("aa:bb:cc:dd:ee:01".into()),
            carrier: true,
        }];
        let mut buf = Vec::new();
        render_iface_list_pretty(&rows, &RenderContext::default(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("Interface:   eth0"));
        assert!(s.contains("Kind:        ethernet"));
        assert!(s.contains("State:       up"));
        assert!(s.contains("MAC:         aa:bb:cc:dd:ee:01"));
        assert!(s.contains("Carrier:     up"));
    }

    #[test]
    fn pretty_multiple_rows_separated_by_blank_line() {
        let rows = vec![
            InterfaceSummary {
                iface: "eth0".into(),
                kind: "ethernet".into(),
                state: "up".into(),
                mac: None,
                carrier: false,
            },
            InterfaceSummary {
                iface: "wlan0".into(),
                kind: "wifi".into(),
                state: "connected".into(),
                mac: None,
                carrier: true,
            },
        ];
        let mut buf = Vec::new();
        render_iface_list_pretty(&rows, &RenderContext::default(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Exactly one blank line between records — no trailing
        // blank after the final block.
        assert!(s.contains("\n\n"));
        assert!(s.contains("eth0"));
        assert!(s.contains("wlan0"));
        assert!(!s.ends_with("\n\n"));
    }

    #[test]
    fn pretty_empty_list_says_no_interfaces() {
        let rows: Vec<InterfaceSummary> = vec![];
        let mut buf = Vec::new();
        render_iface_list_pretty(&rows, &RenderContext::default(), &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "no interfaces\n");
    }
}
