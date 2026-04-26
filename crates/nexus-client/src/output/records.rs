//! `Render` impls for every Phase 3 view type.
//!
//! Each impl follows the same shape:
//! - `render_human`  — comfy-table for lists, vertical key/value
//!                      for single records.
//! - `render_terse`  — one record per line, `emit` helper takes
//!                      `(&str, &str)` pairs so `--fields` filtering
//!                      works uniformly.
//! - `render_json`   — delegate to [`super::json::write`].
//! - `render_pretty` — same vertical block as human for single
//!                      records; list views render per-record
//!                      blocks separated by a blank line.
//!
//! `escape_terse` from the parent module does the separator
//! escaping; `vertical_block` is the shared helper for aligned
//! key/value rendering.

use std::io::{self, Write};

use comfy_table::{Cell, ContentArrangement, Table, presets::NOTHING};

use crate::output::{Render, RenderContext, escape_terse};
use crate::proxy::{
    BluetoothAdapterDetail, BluetoothAdapterSummary, BluetoothDeviceDetail, BluetoothDeviceSummary,
    EthernetDetail, EthernetProfileDetail, GnssDetail, GnssFix, GnssSatellitesView,
    InterfaceDetail, MasterKeyInfo, ProfileDetail, ProfileSummary, WifiDetail, WifiProfileDetail,
    WifiProfileSummary,
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Render `(label, value)` pairs as an aligned vertical block. Used
/// by every `*_show` view. `label_width` defaults to the longest
/// label + 1 so all values align.
pub(crate) fn vertical_block(pairs: &[(&str, String)], w: &mut dyn Write) -> io::Result<()> {
    let label_w = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 1;
    for (label, value) in pairs {
        writeln!(w, "{:<label_w$} {}", format!("{label}:"), value)?;
    }
    Ok(())
}

/// Render a table from labelled rows. Final column's trailing
/// whitespace is stripped per the Phase 2 shell-friendly rule.
fn table_with<F>(headers: &[&str], mut fill: F, w: &mut dyn Write) -> io::Result<()>
where
    F: FnMut(&mut Table),
{
    let mut table = Table::new();
    table
        .load_preset(NOTHING)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_header(headers.iter().map(|h| Cell::new(*h)).collect::<Vec<_>>());
    fill(&mut table);
    for line in table.to_string().lines() {
        writeln!(w, "{}", line.trim_end())?;
    }
    Ok(())
}

/// Terse output for single records. `pairs` is
/// `&[(field_name, value)]`. The ctx's `fields` filter (when set)
/// restricts the emitted set and preserves the requested order.
fn terse_pairs(
    pairs: &[(&'static str, String)],
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> io::Result<()> {
    let selection: Vec<&str> = match &ctx.fields {
        Some(list) => {
            let known: Vec<&str> = pairs.iter().map(|(k, _)| *k).collect();
            for name in list {
                if !known.iter().any(|k| *k == name.as_str()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown field `{name}`; supported: {}", known.join(", ")),
                    ));
                }
            }
            list.iter().map(String::as_str).collect()
        }
        None => pairs.iter().map(|(k, _)| *k).collect(),
    };
    let escaped: Vec<String> = selection
        .iter()
        .map(|f| {
            let v = pairs
                .iter()
                .find(|(k, _)| *k == *f)
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            escape_terse(v, &ctx.separator)
        })
        .collect();
    if escaped.len() == 1 {
        writeln!(w, "{}", escaped[0])
    } else {
        writeln!(w, "{}", escaped.join(&ctx.separator))
    }
}

fn fmt_bool(b: bool) -> String {
    if b { "yes".into() } else { "no".into() }
}

fn fmt_opt(s: &Option<String>) -> String {
    s.clone().unwrap_or_else(|| "—".into())
}

// ---------------------------------------------------------------------------
// InterfaceDetail — `iface show`
// ---------------------------------------------------------------------------

impl Render for InterfaceDetail {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs: Vec<(&str, String)> = vec![
            ("Interface", self.summary.iface.clone()),
            ("Kind", self.summary.kind.clone()),
            ("State", self.summary.state.clone()),
            ("MAC", fmt_opt(&self.summary.mac)),
            ("Carrier", fmt_bool(self.summary.carrier)),
        ];
        if let Some(idx) = self.ifindex {
            pairs.push(("Ifindex", idx.to_string()));
        }
        if let Some(p) = &self.summary.managed_profile {
            pairs.push(("Profile", p.clone()));
        }
        vertical_block(&pairs, w)?;
        if let Some(wifi) = &self.wifi {
            writeln!(w)?;
            render_wifi_block(wifi, w)?;
        }
        if let Some(eth) = &self.ethernet {
            writeln!(w)?;
            render_ethernet_block(eth, w)?;
        }
        if let Some(bt) = &self.bluetooth {
            writeln!(w)?;
            render_bluetooth_block(bt, w)?;
        }
        if let Some(gnss) = &self.gnss {
            writeln!(w)?;
            render_gnss_block(gnss, w)?;
        }
        Ok(())
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let pairs: Vec<(&'static str, String)> = vec![
            ("iface", self.summary.iface.clone()),
            ("kind", self.summary.kind.clone()),
            ("state", self.summary.state.clone()),
            ("mac", self.summary.mac.clone().unwrap_or_default()),
            ("carrier", fmt_bool(self.summary.carrier)),
        ];
        terse_pairs(&pairs, ctx, w)
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        self.render_human(ctx, w)
    }
}

fn render_wifi_block(wifi: &WifiDetail, w: &mut dyn Write) -> io::Result<()> {
    writeln!(w, "[wifi]")?;
    let pairs = vec![
        ("State", wifi.state.clone()),
        ("SSID", fmt_opt(&wifi.ssid)),
        ("BSSID", fmt_opt(&wifi.bssid)),
        ("Frequency", format!("{} MHz", wifi.frequency_mhz)),
        ("Signal", format!("{} dBm", wifi.signal_dbm)),
        ("Security", wifi.security.clone()),
        ("Supplicant", wifi.supplicant.clone()),
        ("Roaming", wifi.roaming_mode.clone()),
        ("Powered", fmt_bool(wifi.powered)),
    ];
    vertical_block(&pairs, w)
}

fn render_ethernet_block(eth: &EthernetDetail, w: &mut dyn Write) -> io::Result<()> {
    writeln!(w, "[ethernet]")?;
    let pairs = vec![
        ("State", eth.state.clone()),
        ("Auth backend", eth.auth_backend.clone()),
        ("Auth failure", non_empty_or_dash(&eth.auth_failure_reason)),
        ("EAP method", non_empty_or_dash(&eth.eap_method)),
    ];
    vertical_block(&pairs, w)
}

fn render_bluetooth_block(bt: &BluetoothAdapterDetail, w: &mut dyn Write) -> io::Result<()> {
    writeln!(w, "[bluetooth]")?;
    let pairs = vec![
        ("Address", bt.address.clone()),
        ("State", bt.state.clone()),
        ("Powered", fmt_bool(bt.powered)),
        ("Discoverable", fmt_bool(bt.discoverable)),
        ("Pairable", fmt_bool(bt.pairable)),
        ("Discovering", fmt_bool(bt.discovering)),
        ("Known devices", bt.known_device_paths.len().to_string()),
    ];
    vertical_block(&pairs, w)
}

fn render_gnss_block(gnss: &GnssDetail, w: &mut dyn Write) -> io::Result<()> {
    writeln!(w, "[gnss]")?;
    let mut pairs = vec![
        ("State", gnss.state.clone()),
        ("Device", gnss.device_path.clone()),
        ("Vendor/model", non_empty_or_dash(&gnss.vendor_model)),
        ("gpsd", fmt_bool(gnss.gpsd_connected)),
        ("Sats in view", gnss.satellites_in_view.to_string()),
        ("Sats used", gnss.satellites_used.to_string()),
        ("Horiz err (m)", format!("{:.1}", gnss.horizontal_error_m)),
    ];
    if let Some(fix) = &gnss.last_fix {
        pairs.push(("Fix mode", fix_mode_label(fix).to_owned()));
        pairs.push((
            "Lat/lon",
            format!("{:.6}, {:.6}", fix.latitude, fix.longitude),
        ));
        if fix.altitude_m != 0.0 {
            pairs.push(("Altitude (m)", format!("{:.1}", fix.altitude_m)));
        }
    }
    vertical_block(&pairs, w)
}

fn fix_mode_label(fix: &GnssFix) -> &'static str {
    match fix.mode {
        2 => "2D",
        3 => "3D",
        _ => "no fix",
    }
}

fn non_empty_or_dash(s: &str) -> String {
    if s.is_empty() {
        "—".into()
    } else {
        s.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Vec<BluetoothAdapterSummary> — `bt adapters`
// ---------------------------------------------------------------------------

impl Render for Vec<BluetoothAdapterSummary> {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        if self.is_empty() {
            writeln!(w, "no bluetooth adapters")?;
            return Ok(());
        }
        table_with(
            &[
                "IFACE",
                "ADDRESS",
                "STATE",
                "POWERED",
                "DISCOVERING",
                "DEVICES",
            ],
            |t| {
                for r in self {
                    t.add_row(vec![
                        Cell::new(&r.ifname),
                        Cell::new(&r.address),
                        Cell::new(&r.state),
                        Cell::new(fmt_bool(r.powered)),
                        Cell::new(fmt_bool(r.discovering)),
                        Cell::new(r.known_device_count.to_string()),
                    ]);
                }
            },
            w,
        )
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for r in self {
            let pairs: Vec<(&'static str, String)> = vec![
                ("ifname", r.ifname.clone()),
                ("address", r.address.clone()),
                ("state", r.state.clone()),
                ("powered", fmt_bool(r.powered)),
                ("discovering", fmt_bool(r.discovering)),
                ("devices", r.known_device_count.to_string()),
            ];
            terse_pairs(&pairs, ctx, w)?;
        }
        Ok(())
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for (i, r) in self.iter().enumerate() {
            if i > 0 {
                writeln!(w)?;
            }
            let pairs = vec![
                ("Interface", r.ifname.clone()),
                ("Address", r.address.clone()),
                ("State", r.state.clone()),
                ("Powered", fmt_bool(r.powered)),
                ("Discovering", fmt_bool(r.discovering)),
                ("Known devices", r.known_device_count.to_string()),
            ];
            vertical_block(&pairs, w)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Vec<BluetoothDeviceSummary> — `bt list`
// ---------------------------------------------------------------------------

impl Render for Vec<BluetoothDeviceSummary> {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        if self.is_empty() {
            writeln!(w, "no bluetooth devices")?;
            return Ok(());
        }
        table_with(
            &[
                "ADAPTER", "ADDRESS", "NAME", "STATE", "PAIRED", "CONN", "RSSI",
            ],
            |t| {
                for r in self {
                    t.add_row(vec![
                        Cell::new(&r.adapter),
                        Cell::new(&r.address),
                        Cell::new(&r.name),
                        Cell::new(&r.state),
                        Cell::new(fmt_bool(r.paired)),
                        Cell::new(fmt_bool(r.connected)),
                        Cell::new(r.rssi.to_string()),
                    ]);
                }
            },
            w,
        )
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for r in self {
            let pairs: Vec<(&'static str, String)> = vec![
                ("adapter", r.adapter.clone()),
                ("address", r.address.clone()),
                ("name", r.name.clone()),
                ("state", r.state.clone()),
                ("paired", fmt_bool(r.paired)),
                ("connected", fmt_bool(r.connected)),
                ("trusted", fmt_bool(r.trusted)),
                ("rssi", r.rssi.to_string()),
                ("transport", r.transport.clone()),
            ];
            terse_pairs(&pairs, ctx, w)?;
        }
        Ok(())
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for (i, r) in self.iter().enumerate() {
            if i > 0 {
                writeln!(w)?;
            }
            let pairs = vec![
                ("Adapter", r.adapter.clone()),
                ("Address", r.address.clone()),
                ("Name", non_empty_or_dash(&r.name)),
                ("State", r.state.clone()),
                ("Paired", fmt_bool(r.paired)),
                ("Bonded", fmt_bool(r.bonded)),
                ("Trusted", fmt_bool(r.trusted)),
                ("Connected", fmt_bool(r.connected)),
                ("Transport", r.transport.clone()),
                ("RSSI", r.rssi.to_string()),
            ];
            vertical_block(&pairs, w)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BluetoothDeviceDetail — `bt show`
// ---------------------------------------------------------------------------

impl Render for BluetoothDeviceDetail {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = vec![
            ("Address", self.summary.address.clone()),
            ("Adapter", self.summary.adapter.clone()),
            ("Name", non_empty_or_dash(&self.summary.name)),
            ("Alias", non_empty_or_dash(&self.alias)),
            ("State", self.summary.state.clone()),
            ("Transport", self.summary.transport.clone()),
            ("Address type", self.address_type.clone()),
            ("Paired", fmt_bool(self.summary.paired)),
            ("Bonded", fmt_bool(self.summary.bonded)),
            ("Trusted", fmt_bool(self.summary.trusted)),
            ("Blocked", fmt_bool(self.blocked)),
            ("Connected", fmt_bool(self.summary.connected)),
            ("RSSI", self.summary.rssi.to_string()),
            ("TX power", self.tx_power.to_string()),
        ];
        if !self.uuids.is_empty() {
            pairs.push(("UUIDs", self.uuids.join(", ")));
        }
        if let Some(p) = &self.profile_path {
            pairs.push(("Profile", p.clone()));
        }
        vertical_block(&pairs, w)
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let pairs: Vec<(&'static str, String)> = vec![
            ("address", self.summary.address.clone()),
            ("adapter", self.summary.adapter.clone()),
            ("name", self.summary.name.clone()),
            ("alias", self.alias.clone()),
            ("state", self.summary.state.clone()),
            ("paired", fmt_bool(self.summary.paired)),
            ("connected", fmt_bool(self.summary.connected)),
            ("rssi", self.summary.rssi.to_string()),
        ];
        terse_pairs(&pairs, ctx, w)
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        self.render_human(ctx, w)
    }
}

// ---------------------------------------------------------------------------
// GnssSatellitesView — `gnss satellites`
// ---------------------------------------------------------------------------

impl Render for GnssSatellitesView {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        render_gnss_sats_inner(self, w)
    }
    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let pairs: Vec<(&'static str, String)> = vec![
            ("device", self.device.clone()),
            ("in_view", self.in_view.to_string()),
            ("used", self.used.to_string()),
        ];
        terse_pairs(&pairs, ctx, w)
    }
    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }
    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        self.render_human(ctx, w)
    }
}

fn render_gnss_sats_inner(view: &GnssSatellitesView, w: &mut dyn Write) -> io::Result<()> {
    let pairs = vec![
        ("Device", view.device.clone()),
        ("In view", view.in_view.to_string()),
        ("Used", view.used.to_string()),
    ];
    vertical_block(&pairs, w)?;
    writeln!(
        w,
        "# per-satellite detail is not yet surfaced on D-Bus; counts only"
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Vec<ProfileSummary> — `profile list`
// ---------------------------------------------------------------------------

impl Render for Vec<ProfileSummary> {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        if self.is_empty() {
            writeln!(w, "no profiles")?;
            return Ok(());
        }
        table_with(
            &["ID", "KIND", "LABEL", "CREDS"],
            |t| {
                for r in self {
                    let creds = if r.credentials_invalid {
                        "invalid"
                    } else {
                        "ok"
                    };
                    t.add_row(vec![
                        Cell::new(&r.id),
                        Cell::new(&r.kind),
                        Cell::new(&r.label),
                        Cell::new(creds),
                    ]);
                }
            },
            w,
        )
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for r in self {
            let pairs: Vec<(&'static str, String)> = vec![
                ("id", r.id.clone()),
                ("kind", r.kind.clone()),
                ("label", r.label.clone()),
                ("credentials_invalid", fmt_bool(r.credentials_invalid)),
                ("created_at", r.created_at.clone()),
                ("updated_at", r.updated_at.clone()),
            ];
            terse_pairs(&pairs, ctx, w)?;
        }
        Ok(())
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for (i, r) in self.iter().enumerate() {
            if i > 0 {
                writeln!(w)?;
            }
            let pairs = vec![
                ("ID", r.id.clone()),
                ("Kind", r.kind.clone()),
                ("Label", r.label.clone()),
                ("Created", r.created_at.clone()),
                ("Updated", r.updated_at.clone()),
                (
                    "Credentials",
                    if r.credentials_invalid {
                        "invalid".into()
                    } else {
                        "ok".into()
                    },
                ),
            ];
            vertical_block(&pairs, w)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Vec<WifiProfileSummary> — `wifi profiles`
// ---------------------------------------------------------------------------

impl Render for Vec<WifiProfileSummary> {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        if self.is_empty() {
            writeln!(w, "no wifi profiles")?;
            return Ok(());
        }
        table_with(
            &["SSID", "LABEL", "SECURITY", "PRIORITY", "AUTO", "CREDS", "ID"],
            |t| {
                for r in self {
                    let auto = if r.auto_connect { "yes" } else { "no" };
                    let creds = if r.credentials_invalid {
                        "invalid"
                    } else {
                        "ok"
                    };
                    let ssid = if r.hidden {
                        format!("{} (hidden)", r.ssid)
                    } else {
                        r.ssid.clone()
                    };
                    t.add_row(vec![
                        Cell::new(&ssid),
                        Cell::new(&r.label),
                        Cell::new(&r.security_type),
                        Cell::new(r.priority),
                        Cell::new(auto),
                        Cell::new(creds),
                        Cell::new(&r.id),
                    ]);
                }
            },
            w,
        )
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for r in self {
            let pairs: Vec<(&'static str, String)> = vec![
                ("id", r.id.clone()),
                ("ssid", r.ssid.clone()),
                ("label", r.label.clone()),
                ("security", r.security_type.clone()),
                ("priority", r.priority.to_string()),
                ("auto_connect", fmt_bool(r.auto_connect)),
                ("hidden", fmt_bool(r.hidden)),
                ("credentials_invalid", fmt_bool(r.credentials_invalid)),
            ];
            terse_pairs(&pairs, ctx, w)?;
        }
        Ok(())
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        if self.is_empty() {
            writeln!(w, "no wifi profiles")?;
            return Ok(());
        }
        for (i, r) in self.iter().enumerate() {
            if i > 0 {
                writeln!(w)?;
            }
            let pairs = vec![
                ("SSID", r.ssid.clone()),
                ("Label", r.label.clone()),
                ("Security", r.security_type.clone()),
                ("Priority", r.priority.to_string()),
                ("Auto-connect", fmt_bool(r.auto_connect)),
                ("Hidden", fmt_bool(r.hidden)),
                (
                    "Credentials",
                    if r.credentials_invalid {
                        "invalid".into()
                    } else {
                        "ok".into()
                    },
                ),
                ("ID", r.id.clone()),
            ];
            vertical_block(&pairs, w)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ProfileDetail — `profile show`
// ---------------------------------------------------------------------------

impl Render for ProfileDetail {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let mut pairs = vec![
            ("ID", self.summary.id.clone()),
            ("Kind", self.summary.kind.clone()),
            ("Label", self.summary.label.clone()),
            ("Created", self.summary.created_at.clone()),
            ("Updated", self.summary.updated_at.clone()),
            (
                "Credentials",
                if self.summary.credentials_invalid {
                    "invalid".into()
                } else {
                    "ok".into()
                },
            ),
        ];
        if let Some(w_p) = &self.wifi {
            pairs.extend_from_slice(&[
                ("SSID", w_p.ssid.clone()),
                ("Security", w_p.security_type.clone()),
                ("Priority", w_p.priority.to_string()),
                ("Auto-connect", fmt_bool(w_p.auto_connect)),
                ("Hidden", fmt_bool(w_p.hidden)),
                ("Fast transition", fmt_bool(w_p.fast_transition)),
            ]);
            if !w_p.has_credentials.is_empty() {
                pairs.push(("Stored creds", w_p.has_credentials.join(", ")));
            }
        }
        if let Some(e) = &self.ethernet {
            pairs.extend_from_slice(&[
                ("Ifname", e.ifname.clone()),
                ("Auto-connect", fmt_bool(e.auto_connect)),
                ("802.1X", fmt_bool(e.dot1x_enabled)),
                ("EAP method", non_empty_or_dash(&e.dot1x_eap)),
            ]);
            if !e.has_credentials.is_empty() {
                pairs.push(("Stored creds", e.has_credentials.join(", ")));
            }
        }
        vertical_block(&pairs, w)
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let pairs: Vec<(&'static str, String)> = vec![
            ("id", self.summary.id.clone()),
            ("kind", self.summary.kind.clone()),
            ("label", self.summary.label.clone()),
            ("created_at", self.summary.created_at.clone()),
            ("updated_at", self.summary.updated_at.clone()),
            (
                "credentials_invalid",
                fmt_bool(self.summary.credentials_invalid),
            ),
        ];
        terse_pairs(&pairs, ctx, w)
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        self.render_human(ctx, w)
    }
}

// Touch the unused profile-detail types so the import list matches
// declared types even when future phases extend this file.
#[allow(dead_code)]
fn _touch(_w: &WifiProfileDetail, _e: &EthernetProfileDetail) {}

// ---------------------------------------------------------------------------
// MasterKeyInfo — `admin master-key-info`
// ---------------------------------------------------------------------------

impl Render for MasterKeyInfo {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        vertical_block(&[("Master key source", self.source.clone())], w)
    }
    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        terse_pairs(&[("source", self.source.clone())], ctx, w)
    }
    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }
    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        self.render_human(ctx, w)
    }
}

// ---------------------------------------------------------------------------
// Vec<WifiScanResult> — `wifi scan`
// ---------------------------------------------------------------------------

impl Render for Vec<crate::proxy::WifiScanResult> {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        if self.is_empty() {
            writeln!(w, "no BSSes visible")?;
            return Ok(());
        }
        table_with(
            &["SSID", "BSSID", "FREQ", "SIGNAL", "SECURITY", "AGE"],
            |t| {
                for r in self {
                    let security = if r.security.is_empty() {
                        "open".into()
                    } else {
                        r.security.join(",")
                    };
                    let ssid = if r.ssid.is_empty() {
                        "<hidden>".into()
                    } else {
                        r.ssid.clone()
                    };
                    t.add_row(vec![
                        Cell::new(ssid),
                        Cell::new(&r.bssid),
                        Cell::new(format!("{} MHz", r.frequency_mhz)),
                        Cell::new(format!("{} dBm", r.signal_dbm)),
                        Cell::new(security),
                        Cell::new(format!("{} ms", r.age_ms)),
                    ]);
                }
            },
            w,
        )
    }

    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for r in self {
            let pairs: Vec<(&'static str, String)> = vec![
                ("ssid", r.ssid.clone()),
                ("bssid", r.bssid.clone()),
                ("frequency_mhz", r.frequency_mhz.to_string()),
                ("signal_dbm", r.signal_dbm.to_string()),
                ("security", r.security.join(",")),
                ("age_ms", r.age_ms.to_string()),
            ];
            terse_pairs(&pairs, ctx, w)?;
        }
        Ok(())
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }

    fn render_pretty(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        for (i, r) in self.iter().enumerate() {
            if i > 0 {
                writeln!(w)?;
            }
            let pairs = vec![
                (
                    "SSID",
                    if r.ssid.is_empty() {
                        "<hidden>".into()
                    } else {
                        r.ssid.clone()
                    },
                ),
                ("BSSID", r.bssid.clone()),
                ("Frequency", format!("{} MHz", r.frequency_mhz)),
                ("Signal", format!("{} dBm", r.signal_dbm)),
                (
                    "Security",
                    if r.security.is_empty() {
                        "open".into()
                    } else {
                        r.security.join(", ")
                    },
                ),
                ("Age", format!("{} ms", r.age_ms)),
            ];
            vertical_block(&pairs, w)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MutationOutcome — confirmation banner for mutating commands
// ---------------------------------------------------------------------------

impl Render for crate::proxy::MutationOutcome {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let mut line = format!("{}: {}", self.action, self.subject);
        if let Some(id) = &self.id {
            line.push_str(&format!(" (id: {id})"));
        }
        writeln!(w, "{line}")?;
        if let Some(n) = &self.note {
            writeln!(w, "  {n}")?;
        }
        Ok(())
    }
    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let pairs: Vec<(&'static str, String)> = vec![
            ("action", self.action.clone()),
            ("subject", self.subject.clone()),
            ("id", self.id.clone().unwrap_or_default()),
        ];
        terse_pairs(&pairs, ctx, w)
    }
    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }
    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        self.render_human(ctx, w)
    }
}

// ---------------------------------------------------------------------------
// ReloadConfigReport — `admin reload-config`
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// WatchEvent — one line per event, NDJSON-friendly
// ---------------------------------------------------------------------------

impl Render for crate::watch::WatchEvent {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        // One aligned line per event: `<time>  <kind>  k=v k=v …`.
        // Auto-alignment across events isn't strictly the spec
        // (DD-008 §7.4 mentions "columns auto-align across events")
        // but shell consumers prefer predictable per-line output;
        // a fancier multi-event aligner can land later.
        let body: Vec<String> = self
            .fields
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_display()))
            .collect();
        writeln!(w, "{}  {}  {}", self.time, self.kind, body.join(" "))
    }
    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        // Terse is each field stringified, separator-joined. The
        // `--fields` filter picks columns; default is time, kind,
        // then every flat field in insertion-order (alphabetical
        // since we use BTreeMap).
        let mut all: Vec<(&'static str, String)> = Vec::new();
        // We need 'static str keys; intern via Box::leak on each
        // distinct key for `--fields` parity. To avoid that per-
        // event leak, just stream with owned strings and bypass
        // `terse_pairs` for WatchEvent.
        all.push(("time", self.time.clone()));
        all.push(("kind", self.kind.clone()));
        let _ = ctx;
        // Flat fields follow. Emit values directly.
        let mut out = String::new();
        out.push_str(&self.time);
        out.push_str(&ctx.separator);
        out.push_str(&self.kind);
        for (k, v) in &self.fields {
            out.push_str(&ctx.separator);
            out.push_str(k);
            out.push('=');
            out.push_str(&v.as_display());
        }
        writeln!(w, "{out}")
    }
    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        // DD-008 §7.4 NDJSON: one compact JSON object per line
        // (not pretty-printed like other commands — streams are
        // the scripting path).
        serde_json::to_writer(&mut *w, self).map_err(io::Error::other)?;
        writeln!(w)
    }
    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        // DD-008 §7.4 says pretty is not supported for watch
        // (pretty is a single-record format). The command handler
        // logs a stderr warning once and re-routes here to human.
        self.render_human(ctx, w)
    }
}

impl Render for crate::proxy::ReloadConfigReport {
    fn render_human(&self, _ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        if self.applied.is_empty() && self.deferred.is_empty() && self.errors.is_empty() {
            writeln!(w, "config unchanged (no fields differed)")?;
            return Ok(());
        }
        if !self.applied.is_empty() {
            writeln!(w, "Applied:")?;
            for f in &self.applied {
                writeln!(w, "  {f}")?;
            }
        }
        if !self.deferred.is_empty() {
            writeln!(w, "Deferred (restart required):")?;
            for f in &self.deferred {
                writeln!(w, "  {f}")?;
            }
        }
        if !self.errors.is_empty() {
            writeln!(w, "Errors:")?;
            for (f, r) in &self.errors {
                writeln!(w, "  {f}: {r}")?;
            }
        }
        Ok(())
    }
    fn render_terse(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        let pairs: Vec<(&'static str, String)> = vec![
            ("applied", self.applied.join(",")),
            ("deferred", self.deferred.join(",")),
            (
                "errors",
                self.errors
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        ];
        terse_pairs(&pairs, ctx, w)
    }
    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        super::json::write(self, w)
    }
    fn render_pretty(&self, ctx: &RenderContext, w: &mut dyn Write) -> io::Result<()> {
        self.render_human(ctx, w)
    }
}
