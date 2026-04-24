//! Terse renderer. DD-008 §5.2.
//!
//! Rules:
//! - No headers, no colours.
//! - One record per line.
//! - `--fields` picks columns; default order is the view's
//!   declared `FIELDS` slice.
//! - Single-field selection emits just the value (no separator).
//! - Embedded separator chars in values are backslash-escaped.
//!   Backslashes themselves are doubled.

use std::io::{self, Write};

use crate::output::{RenderContext, escape_terse};
use crate::proxy::{InterfaceSummary, ManagerStatus};

/// Fields supported by `nexusctl iface list` in terse / JSON /
/// pretty modes. The order here is the default column order when
/// `--fields` isn't supplied.
pub const IFACE_FIELDS: &[&str] = &["iface", "kind", "state", "mac", "carrier"];

pub const STATUS_FIELDS: &[&str] = &[
    "version",
    "power_state",
    "interfaces",
    "ethernet",
    "wifi",
    "bluetooth",
    "gnss",
    "wifi_profiles",
    "ethernet_profiles",
    "bluetooth_profiles",
    "bluez",
    "gpsd",
    "master_key",
];

pub fn render_iface_list_terse(
    rows: &[InterfaceSummary],
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> io::Result<()> {
    let fields = resolve_fields(&ctx.fields, IFACE_FIELDS)?;
    for row in rows {
        emit_record(&fields, &ctx.separator, w, |field| {
            iface_field_value(row, field)
        })?;
    }
    Ok(())
}

pub fn render_status_terse(
    status: &ManagerStatus,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> io::Result<()> {
    let fields = resolve_fields(&ctx.fields, STATUS_FIELDS)?;
    emit_record(&fields, &ctx.separator, w, |field| {
        status_field_value(status, field)
    })
}

fn emit_record<F>(fields: &[&str], sep: &str, w: &mut dyn Write, mut value_of: F) -> io::Result<()>
where
    F: FnMut(&str) -> String,
{
    let escaped: Vec<String> = fields
        .iter()
        .map(|f| escape_terse(&value_of(f), sep))
        .collect();
    // DD-008 §5.2: "When only one field is requested, no separator
    // is emitted — just the value." No trailing newline either?
    // The same section calls for a newline record separator, so
    // one-field output is `<value>\n`.
    if escaped.len() == 1 {
        writeln!(w, "{}", escaped[0])?;
    } else {
        writeln!(w, "{}", escaped.join(sep))?;
    }
    Ok(())
}

fn iface_field_value(row: &InterfaceSummary, field: &str) -> String {
    match field {
        "iface" => row.iface.clone(),
        "kind" => row.kind.clone(),
        "state" => row.state.clone(),
        "mac" => row.mac.clone().unwrap_or_default(),
        "carrier" => if row.carrier { "true" } else { "false" }.into(),
        _ => String::new(),
    }
}

fn status_field_value(status: &ManagerStatus, field: &str) -> String {
    match field {
        "version" => status.version.clone(),
        "power_state" => status.power_state.clone(),
        "interfaces" => status.interface_count.to_string(),
        "ethernet" => status.ethernet_count.to_string(),
        "wifi" => status.wifi_count.to_string(),
        "bluetooth" => status.bluetooth_count.to_string(),
        "gnss" => status.gnss_count.to_string(),
        "wifi_profiles" => status.wifi_profile_count.to_string(),
        "ethernet_profiles" => status.ethernet_profile_count.to_string(),
        "bluetooth_profiles" => status.bluetooth_profile_count.to_string(),
        "bluez" => if status.bluez_available {
            "true"
        } else {
            "false"
        }
        .into(),
        "gpsd" => if status.gpsd_available {
            "true"
        } else {
            "false"
        }
        .into(),
        "master_key" => status.master_key_source.clone(),
        _ => String::new(),
    }
}

/// Resolve the caller's `--fields` selection against a view's
/// declared field list. Unknown field names are an error — terse
/// mode is shell-facing, so a typo should fail loudly rather than
/// silently emit empty columns.
fn resolve_fields<'a>(
    requested: &'a Option<Vec<String>>,
    supported: &'static [&'static str],
) -> io::Result<Vec<&'a str>> {
    match requested {
        None => Ok(supported.iter().copied().collect()),
        Some(list) => {
            let mut out: Vec<&str> = Vec::with_capacity(list.len());
            for name in list {
                let s = name.as_str();
                if supported.contains(&s) {
                    out.push(s);
                } else {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "unknown field `{name}`; supported: {}",
                            supported.join(", ")
                        ),
                    ));
                }
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(iface: &str, kind: &str, state: &str, mac: Option<&str>) -> InterfaceSummary {
        InterfaceSummary {
            iface: iface.into(),
            kind: kind.into(),
            state: state.into(),
            mac: mac.map(str::to_owned),
            carrier: false,
            managed_profile: None,
        }
    }

    #[test]
    fn terse_default_emits_every_field_colon_separated() {
        let rows = vec![row("eth0", "ethernet", "up", Some("aa:bb:cc:dd:ee:01"))];
        let ctx = RenderContext::default();
        let mut buf = Vec::new();
        render_iface_list_terse(&rows, &ctx, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        // Note the escaped colons inside the MAC — they use the
        // same separator so they must be escaped.
        assert_eq!(s, "eth0:ethernet:up:aa\\:bb\\:cc\\:dd\\:ee\\:01:false\n");
    }

    #[test]
    fn terse_with_fields_subset_in_requested_order() {
        let rows = vec![row("eth0", "ethernet", "up", Some("aa:bb"))];
        let ctx = RenderContext {
            fields: Some(vec!["state".into(), "iface".into()]),
            ..RenderContext::default()
        };
        let mut buf = Vec::new();
        render_iface_list_terse(&rows, &ctx, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "up:eth0\n");
    }

    #[test]
    fn terse_single_field_omits_separator() {
        let rows = vec![
            row("eth0", "ethernet", "up", None),
            row("wlan0", "wifi", "connected", None),
        ];
        let ctx = RenderContext {
            fields: Some(vec!["iface".into()]),
            ..RenderContext::default()
        };
        let mut buf = Vec::new();
        render_iface_list_terse(&rows, &ctx, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "eth0\nwlan0\n");
    }

    #[test]
    fn terse_custom_separator_respected() {
        let rows = vec![row("eth0", "ethernet", "up", None)];
        let ctx = RenderContext {
            separator: "\t".into(),
            fields: Some(vec!["iface".into(), "kind".into()]),
            ..RenderContext::default()
        };
        let mut buf = Vec::new();
        render_iface_list_terse(&rows, &ctx, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "eth0\tethernet\n");
    }

    #[test]
    fn terse_unknown_field_errors_out() {
        let rows = vec![row("eth0", "ethernet", "up", None)];
        let ctx = RenderContext {
            fields: Some(vec!["bogus".into()]),
            ..RenderContext::default()
        };
        let err = render_iface_list_terse(&rows, &ctx, &mut Vec::new()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn terse_escapes_embedded_separator_in_value() {
        // Wi-Fi SSID containing ":" — DD-008 §5.2 explicitly cites
        // this case.
        let rows = vec![InterfaceSummary {
            iface: "net:work".into(),
            kind: "wifi".into(),
            state: "up".into(),
            mac: None,
            carrier: false,
            managed_profile: None,
        }];
        let ctx = RenderContext {
            fields: Some(vec!["iface".into(), "state".into()]),
            ..RenderContext::default()
        };
        let mut buf = Vec::new();
        render_iface_list_terse(&rows, &ctx, &mut buf).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "net\\:work:up\n");
    }
}
