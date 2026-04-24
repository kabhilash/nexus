//! `nexusctl profile …` mutating commands. DD-008 §4.1.

use std::io::{Read, Write};

use crate::cli::ProfileKind;
use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::{EthernetProfileSettings, ManagerOps, MutationOutcome, WifiProfileSettings};
use crate::psk_warn::maybe_warn_psk;

#[allow(clippy::too_many_arguments)]
pub async fn add_wifi(
    ops: &dyn ManagerOps,
    ssid: Option<&str>,
    psk: Option<&str>,
    file: Option<&str>,
    label: Option<&str>,
    priority: Option<i32>,
    auto_connect: Option<bool>,
    hidden: Option<bool>,
    fast_transition: Option<bool>,
    security: &str,
    no_warn_psk: bool,
    stderr: &mut dyn Write,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    maybe_warn_psk(psk, no_warn_psk, stderr);
    if file.is_some() {
        // File-based creation routes through `import` so the TOML
        // parsing + kind-inference logic lives in one place.
        return import_from_file(ops, Some("wifi"), file, format, ctx, w).await;
    }
    let ssid_val = ssid.ok_or_else(|| NexusctlError::InvalidArgument {
        message: "profile add-wifi needs <ssid> or --file".into(),
    })?;
    // Decide up front whether the operator is creating an
    // open-network profile (`--security open` explicitly) or a
    // secured one. Secured profiles need a PSK; when neither
    // `--psk` nor `NEXUSCTL_PSK` is set, fall back to the
    // interactive prompt. On a non-TTY, `resolve_psk` surfaces
    // `NotInteractive` (exit 5) per DD-008 §6.2.
    let want_credentials = security != "open"
        && !(security == "auto" && psk.is_none() && !interactive_psk_available());
    let passphrase = if want_credentials {
        Some(crate::interactive::passphrase::resolve_psk(psk).await?)
    } else {
        None
    };
    let security_type = if security == "auto" {
        if passphrase.is_some() {
            "wpa2_personal".into()
        } else {
            "open".into()
        }
    } else {
        security.to_owned()
    };
    let settings = WifiProfileSettings {
        ssid: ssid_val.as_bytes().to_vec(),
        security_type,
        passphrase,
        label: label.map(str::to_owned),
        priority,
        auto_connect,
        hidden,
        fast_transition,
    };
    let id = ops.add_wifi_profile(settings).await?;
    render(
        &MutationOutcome {
            action: "profile add-wifi".into(),
            subject: ssid_val.to_owned(),
            id: Some(id),
            note: None,
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

pub async fn add_ethernet(
    ops: &dyn ManagerOps,
    ifname: Option<&str>,
    file: Option<&str>,
    label: Option<&str>,
    auto_connect: Option<bool>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    if file.is_some() {
        return import_from_file(ops, Some("ethernet"), file, format, ctx, w).await;
    }
    let ifname_val = ifname.ok_or_else(|| NexusctlError::InvalidArgument {
        message: "profile add-ethernet needs <ifname> or --file".into(),
    })?;
    let settings = EthernetProfileSettings {
        ifname: ifname_val.to_owned(),
        label: label.map(str::to_owned),
        auto_connect,
    };
    let id = ops.add_ethernet_profile(settings).await?;
    render(
        &MutationOutcome {
            action: "profile add-ethernet".into(),
            subject: ifname_val.to_owned(),
            id: Some(id),
            note: None,
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

/// `profile import`: reads TOML from stdin or `--file` and applies
/// it. Kind inference comes from the TOML's `kind` field first, then
/// from the `--kind` override. Phase 4 extracts the fields into
/// neutral settings structs for the corresponding add method —
/// richer TOML shapes (full security dicts, dot1x configs) land in
/// later phases as the parser grows.
pub async fn import(
    ops: &dyn ManagerOps,
    kind: Option<ProfileKind>,
    file: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let kind_wire = kind.map(|k| k.as_wire());
    import_from_file(ops, kind_wire, file, format, ctx, w).await
}

async fn import_from_file(
    ops: &dyn ManagerOps,
    kind_hint: Option<&str>,
    file: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let toml = match file {
        Some("-") | None => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .map_err(|e| NexusctlError::IoError {
                    detail: format!("stdin: {e}"),
                })?;
            buf
        }
        Some(path) => std::fs::read_to_string(path).map_err(|e| NexusctlError::IoError {
            detail: format!("{path}: {e}"),
        })?,
    };
    let parsed: toml::Value =
        toml::from_str(&toml).map_err(|e| NexusctlError::InvalidArgument {
            message: format!("profile TOML parse: {e}"),
        })?;
    let kind = parsed
        .get("kind")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .or_else(|| kind_hint.map(str::to_owned))
        .ok_or_else(|| NexusctlError::InvalidArgument {
            message: "profile TOML missing `kind`; pass --kind".into(),
        })?;
    match kind.as_str() {
        "wifi" => {
            let wifi = parsed
                .get("wifi")
                .ok_or_else(|| NexusctlError::InvalidArgument {
                    message: "wifi profile TOML needs a [wifi] section".into(),
                })?;
            let ssid = wifi.get("ssid").and_then(|v| v.as_str()).ok_or_else(|| {
                NexusctlError::InvalidArgument {
                    message: "wifi profile TOML needs wifi.ssid".into(),
                }
            })?;
            let security = wifi
                .get("security_type")
                .and_then(|v| v.as_str())
                .unwrap_or("open")
                .to_owned();
            let settings = WifiProfileSettings {
                ssid: ssid.as_bytes().to_vec(),
                security_type: security,
                passphrase: wifi
                    .get("passphrase")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                label: parsed
                    .get("label")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                priority: wifi
                    .get("priority")
                    .and_then(|v| v.as_integer())
                    .map(|n| n as i32),
                auto_connect: wifi.get("auto_connect").and_then(|v| v.as_bool()),
                hidden: wifi.get("hidden").and_then(|v| v.as_bool()),
                fast_transition: wifi.get("fast_transition").and_then(|v| v.as_bool()),
            };
            let id = ops.add_wifi_profile(settings).await?;
            render(
                &MutationOutcome {
                    action: "profile import (wifi)".into(),
                    subject: ssid.to_owned(),
                    id: Some(id),
                    note: None,
                },
                format,
                ctx,
                w,
            )
            .map_err(io_err)
        }
        "ethernet" => {
            let eth = parsed
                .get("ethernet")
                .ok_or_else(|| NexusctlError::InvalidArgument {
                    message: "ethernet profile TOML needs an [ethernet] section".into(),
                })?;
            let ifname = eth.get("ifname").and_then(|v| v.as_str()).ok_or_else(|| {
                NexusctlError::InvalidArgument {
                    message: "ethernet profile TOML needs ethernet.ifname".into(),
                }
            })?;
            let settings = EthernetProfileSettings {
                ifname: ifname.to_owned(),
                label: parsed
                    .get("label")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned),
                auto_connect: eth.get("auto_connect").and_then(|v| v.as_bool()),
            };
            let id = ops.add_ethernet_profile(settings).await?;
            render(
                &MutationOutcome {
                    action: "profile import (ethernet)".into(),
                    subject: ifname.to_owned(),
                    id: Some(id),
                    note: None,
                },
                format,
                ctx,
                w,
            )
            .map_err(io_err)
        }
        other => Err(NexusctlError::InvalidArgument {
            message: format!("unknown profile kind `{other}`"),
        }),
    }
}

pub async fn remove(
    ops: &dyn ManagerOps,
    reference: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    ops.remove_profile(reference).await?;
    render(
        &MutationOutcome {
            action: "profile remove".into(),
            subject: reference.to_owned(),
            id: None,
            note: None,
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

pub async fn update(
    ops: &dyn ManagerOps,
    reference: &str,
    field: &str,
    value: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    ops.update_profile_field(reference, field, value).await?;
    render(
        &MutationOutcome {
            action: format!("profile update ({field})"),
            subject: reference.to_owned(),
            id: None,
            note: Some(format!("value: {value}")),
        },
        format,
        ctx,
        w,
    )
    .map_err(io_err)
}

/// Probe: are we sitting at an interactive TTY *or* is
/// `NEXUSCTL_PSK` set? If yes, we can safely prompt. If no, an
/// `--security auto` / no-`--psk` invocation on a non-TTY is open
/// by default (same defensive choice dialoguer makes for password
/// prompts — scripts asking for `auto` with no PSK clearly don't
/// want to block).
fn interactive_psk_available() -> bool {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return true;
    }
    std::env::var("NEXUSCTL_PSK")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
