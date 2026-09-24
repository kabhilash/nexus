//! `nexusctl wifi …` mutating commands. DD-008 §4.1.

use std::io::Write;

use crate::errors::NexusctlError;
use crate::output::{OutputFormat, RenderContext, render};
use crate::proxy::{ManagerOps, MutationOutcome, WifiProfileSettings};
use crate::psk_warn::maybe_warn_psk;

/// Resolve the target Wi-Fi interface per DD-008 §4.1: `iface`
/// argument when given; otherwise auto-select if exactly one
/// Wi-Fi interface exists. Ambiguity returns `InvalidArgument`
/// listing the candidates.
async fn resolve_wifi_iface(
    ops: &dyn ManagerOps,
    iface: Option<&str>,
) -> Result<String, NexusctlError> {
    if let Some(name) = iface {
        return Ok(name.to_owned());
    }
    let rows = ops.list_interfaces().await?;
    let wifis: Vec<_> = rows
        .iter()
        .filter(|r| r.kind == "wifi" || r.kind == "wireless")
        .collect();
    match wifis.len() {
        0 => Err(NexusctlError::NotFound {
            reference: "<any wifi interface>".into(),
        }),
        1 => Ok(wifis[0].iface.clone()),
        _ => {
            let names: Vec<&str> = wifis.iter().map(|r| r.iface.as_str()).collect();
            Err(NexusctlError::InvalidArgument {
                message: format!(
                    "multiple Wi-Fi interfaces available; specify --iface: {}",
                    names.join(", ")
                ),
            })
        }
    }
}

pub async fn scan(
    ops: &dyn ManagerOps,
    iface: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let target = resolve_wifi_iface(ops, iface).await?;
    let results = ops.wifi_scan(&target).await?;
    render(&results, format, ctx, w).map_err(io_err)
}

/// Security-type tags `Manager.AddWifiProfile` can build without
/// extra Enterprise fields (EAP method, certs, identity — see
/// `parse_security` in nexus-dbus). `wifi connect`'s auto-detect path
/// only ever offers one of these.
const AUTO_DETECTABLE_SECURITY_TYPES: [&str; 5] =
    ["open", "owe", "wpa2_personal", "wpa3_personal", "wpa2_wpa3_personal"];

fn security_type_needs_passphrase(security_type: &str) -> bool {
    !matches!(security_type, "open" | "owe")
}

/// Scan for `ssid` on `target` and return the security type the AP
/// actually advertises (DD-008 §6.2 step 2). Without this, a new
/// profile can't tell WPA2-Personal from WPA3-Personal apart, and
/// building the wrong one means wpa_supplicant negotiates the wrong
/// key management (e.g. `WPA-PSK` against an SAE-only AP).
async fn detect_security_type(
    ops: &dyn ManagerOps,
    target: &str,
    ssid: &str,
) -> Result<String, NexusctlError> {
    let results = ops.wifi_scan(target).await?;
    let offered = results
        .iter()
        .find(|r| r.ssid == ssid)
        .map(|r| r.security.clone())
        .ok_or_else(|| NexusctlError::NotFound {
            reference: ssid.to_owned(),
        })?;
    offered
        .iter()
        .find(|s| AUTO_DETECTABLE_SECURITY_TYPES.contains(&s.as_str()))
        .cloned()
        .ok_or_else(|| NexusctlError::InvalidArgument {
            message: format!(
                "{ssid} advertises {offered:?}, which `wifi connect` can't configure \
                 automatically; use `nexusctl profile add-wifi` with explicit security fields"
            ),
        })
}

#[allow(clippy::too_many_arguments)]
pub async fn connect(
    ops: &dyn ManagerOps,
    ssid: &str,
    iface: Option<&str>,
    psk: Option<&str>,
    no_warn_psk: bool,
    stderr: &mut dyn Write,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    maybe_warn_psk(psk, no_warn_psk, stderr);
    let target = resolve_wifi_iface(ops, iface).await?;
    // Try to find an existing profile by SSID.
    let profile_path = match ops.find_wifi_profile(ssid.as_bytes()).await {
        Ok(p) => p,
        Err(NexusctlError::NotFound { .. }) => {
            // No stored profile — scan to identify the AP's actual
            // security type before building one (DD-008 §6.2 step 2).
            let security_type = detect_security_type(ops, &target, ssid).await?;
            // Open/OWE need no credentials; everything else resolves
            // a passphrase from `--psk`, then `NEXUSCTL_PSK`, then an
            // interactive TTY prompt. The interactive fallback lives
            // in `crate::interactive::passphrase`; it returns
            // `NotInteractive` (exit 5) when stdin isn't a TTY.
            let passphrase = if security_type_needs_passphrase(&security_type) {
                Some(crate::interactive::passphrase::resolve_psk(psk).await?)
            } else {
                None
            };
            let settings = WifiProfileSettings {
                ssid: ssid.as_bytes().to_vec(),
                security_type,
                passphrase,
                label: None,
                priority: None,
                auto_connect: Some(true),
                hidden: None,
                fast_transition: None,
            };
            let id = ops.add_wifi_profile(settings).await?;
            format!("/fi/nexus1/profile/wifi/{id}")
        }
        Err(e) => return Err(e),
    };
    ops.wifi_connect_profile(&target, &profile_path).await?;
    let outcome = MutationOutcome {
        action: "wifi connect".into(),
        subject: ssid.to_owned(),
        id: Some(profile_path),
        note: Some(format!("on {target}")),
    };
    render(&outcome, format, ctx, w).map_err(io_err)
}

pub async fn connect_profile(
    ops: &dyn ManagerOps,
    profile_ref: &str,
    iface: Option<&str>,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let target = resolve_wifi_iface(ops, iface).await?;
    let detail = ops.show_profile(profile_ref).await?;
    let path = format!(
        "/fi/nexus1/profile/{}/{}",
        detail.summary.kind, detail.summary.id
    );
    ops.wifi_connect_profile(&target, &path).await?;
    let outcome = MutationOutcome {
        action: "wifi connect".into(),
        subject: detail.summary.label.clone(),
        id: Some(detail.summary.id),
        note: Some(format!("on {target}")),
    };
    render(&outcome, format, ctx, w).map_err(io_err)
}

pub async fn power(
    ops: &dyn ManagerOps,
    iface: Option<&str>,
    on: bool,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let target = resolve_wifi_iface(ops, iface).await?;
    ops.wifi_set_powered(&target, on).await?;
    let outcome = MutationOutcome {
        action: "wifi power".into(),
        subject: target,
        id: None,
        note: Some(if on { "on".into() } else { "off".into() }),
    };
    render(&outcome, format, ctx, w).map_err(io_err)
}

pub async fn disconnect(
    ops: &dyn ManagerOps,
    iface: Option<&str>,
    pause_auto_connect: bool,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let target = resolve_wifi_iface(ops, iface).await?;
    ops.wifi_disconnect(&target, pause_auto_connect).await?;
    let action = if pause_auto_connect {
        "wifi disconnect (pause auto-connect)".to_owned()
    } else {
        "wifi disconnect".to_owned()
    };
    let outcome = MutationOutcome {
        action,
        subject: target,
        id: None,
        note: if pause_auto_connect {
            Some(
                "auto-connect paused for this session; profile's on-disk auto_connect unchanged"
                    .into(),
            )
        } else {
            None
        },
    };
    render(&outcome, format, ctx, w).map_err(io_err)
}

/// `wifi profiles` — lists saved Wi-Fi profiles with Wi-Fi-specific
/// columns. Differs from `profile list --kind wifi` (kind-agnostic
/// generic table) by surfacing SSID / security / priority /
/// auto-connect / hidden flags inline.
pub async fn profiles(
    ops: &dyn ManagerOps,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    let rows = ops.list_wifi_profiles().await?;
    render(&rows, format, ctx, w).map_err(io_err)
}

/// `wifi forget <ssid|ulid>` — removes a stored Wi-Fi profile.
pub async fn forget(
    ops: &dyn ManagerOps,
    reference: &str,
    format: OutputFormat,
    ctx: &RenderContext,
    w: &mut dyn Write,
) -> Result<(), NexusctlError> {
    // Try ULID/label first; if that fails, try SSID lookup.
    let path = match ops.show_profile(reference).await {
        Ok(detail) => format!(
            "/fi/nexus1/profile/{}/{}",
            detail.summary.kind, detail.summary.id
        ),
        Err(NexusctlError::NotFound { .. }) => ops.find_wifi_profile(reference.as_bytes()).await?,
        Err(e) => return Err(e),
    };
    ops.remove_profile(&path).await?;
    let outcome = MutationOutcome {
        action: "wifi forget".into(),
        subject: reference.to_owned(),
        id: Some(path),
        note: None,
    };
    render(&outcome, format, ctx, w).map_err(io_err)
}

fn io_err(e: std::io::Error) -> NexusctlError {
    if e.kind() == std::io::ErrorKind::BrokenPipe {
        return NexusctlError::Other { raw: String::new() };
    }
    NexusctlError::Other {
        raw: format!("write failed: {e}"),
    }
}
