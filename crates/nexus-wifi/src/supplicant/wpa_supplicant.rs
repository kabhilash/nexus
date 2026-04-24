//! wpa_supplicant-backed [`WifiSupplicantBackend`]. See DD-003 §9.
//!
//! What's wired today (runs against a live `fi.w1.wpa_supplicant1`):
//!
//! - Construction opens a system-bus connection and spawns a
//!   `NameOwnerChanged` watcher so daemon appearance / disappearance
//!   flows back as [`SupplicantEvent::DaemonUp`] / [`DaemonDown`].
//! - [`attach`] calls `CreateInterface` (or falls back to
//!   `GetInterface` when the interface is already owned), remembers
//!   the interface object path, and spawns two per-interface
//!   watchers: one for `PropertiesChanged` on `State`
//!   (→ [`SupplicantEvent::State`]) and one for the `ScanDone`
//!   signal (→ [`SupplicantEvent::ScanComplete`]).
//! - [`detach`] aborts both watchers and calls `RemoveInterface`.
//! - [`scan`] invokes `Interface1.Scan(a{sv})` with a `Type` field
//!   derived from `ScanParams::active`. Results arrive via the
//!   `ScanDone` watcher already spawned at attach time.
//! - [`get_scan_results`] reads `Interface1.BSSs` then each
//!   `BSS1.{SSID,BSSID,Frequency,Signal,WPA,RSN,Age}` and returns a
//!   [`BssInfo`] for every BSS whose SSID is non-empty (hidden APs
//!   report empty SSID in probe responses and are filtered out —
//!   connecting to them is a profile-driven flow, not a scan one).
//!
//! The per-network mutating methods (`connect`, `disconnect`,
//! `roam`, `signal_info`) still return [`WifiError::Supplicant`] —
//! they need the security-mode dict builder per DD-003 §9.4 and the
//! hwsim harness from §14.2 to cover the handshake states. They stay
//! stubs until that harness lands.

use std::collections::HashMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, proxy};

use super::{SupplicantEvent, WifiSupplicantBackend};
use crate::error::{Result, WifiError};
use crate::types::{BssInfo, NetworkConfig, NetworkHandle, RoamTarget, ScanParams, SignalInfo};

// ---- zbus proxies --------------------------------------------------------

/// Root `fi.w1.wpa_supplicant1` service. Path
/// `/fi/w1/wpa_supplicant1`.
#[proxy(
    interface = "fi.w1.wpa_supplicant1",
    default_service = "fi.w1.wpa_supplicant1",
    default_path = "/fi/w1/wpa_supplicant1"
)]
trait WpaSupplicant {
    fn create_interface(&self, args: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn get_interface(&self, ifname: &str) -> zbus::Result<OwnedObjectPath>;
    fn remove_interface(&self, iface: &OwnedObjectPath) -> zbus::Result<()>;
}

/// Per-interface `fi.w1.wpa_supplicant1.Interface`. Object path
/// varies per interface (e.g. `/fi/w1/wpa_supplicant1/Interfaces/0`).
/// We use `ProxyBuilder::path(...)` to bind one at `attach` time.
#[proxy(
    interface = "fi.w1.wpa_supplicant1.Interface",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait WpaInterface {
    fn scan(&self, args: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "BSSs")]
    fn bsss(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(signal)]
    fn scan_done(&self, success: bool) -> zbus::Result<()>;
}

/// A single BSS seen by the supplicant scan cache. Properties only —
/// there's no method call involved in reading a scan result.
#[proxy(
    interface = "fi.w1.wpa_supplicant1.BSS",
    default_service = "fi.w1.wpa_supplicant1"
)]
trait Bss {
    #[zbus(property, name = "SSID")]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property, name = "BSSID")]
    fn bssid(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property)]
    fn frequency(&self) -> zbus::Result<u16>;

    #[zbus(property)]
    fn signal(&self) -> zbus::Result<i16>;

    #[zbus(property)]
    fn age(&self) -> zbus::Result<u32>;

    #[zbus(property, name = "WPA")]
    fn wpa(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(property, name = "RSN")]
    fn rsn(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}

// ---- backend -------------------------------------------------------------

/// Bundle of things an attached interface's watcher tasks need to
/// stay alive until `detach`.
struct AttachedInterface {
    path: OwnedObjectPath,
    state_watcher: JoinHandle<()>,
    scan_watcher: JoinHandle<()>,
}

pub struct WpaSupplicantBackend {
    connection: Connection,
    event_tx: broadcast::Sender<SupplicantEvent>,
    interfaces: HashMap<u32, AttachedInterface>,
    /// NameOwnerChanged watcher — alive for the full backend
    /// lifetime. Aborted on drop.
    daemon_watcher: Option<JoinHandle<()>>,
}

impl WpaSupplicantBackend {
    pub async fn new(event_tx: broadcast::Sender<SupplicantEvent>) -> Result<Self> {
        let connection = Connection::system().await.map_err(zbus_err)?;
        let daemon_watcher = Some(spawn_daemon_watcher(connection.clone(), event_tx.clone()));
        Ok(Self {
            connection,
            event_tx,
            interfaces: HashMap::new(),
            daemon_watcher,
        })
    }
}

impl Drop for WpaSupplicantBackend {
    fn drop(&mut self) {
        if let Some(h) = self.daemon_watcher.take() {
            h.abort();
        }
        for (_, iface) in self.interfaces.drain() {
            iface.state_watcher.abort();
            iface.scan_watcher.abort();
        }
    }
}

#[async_trait]
impl WifiSupplicantBackend for WpaSupplicantBackend {
    async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()> {
        if self.interfaces.contains_key(&ifindex) {
            return Ok(());
        }
        let root = WpaSupplicantProxy::new(&self.connection)
            .await
            .map_err(zbus_err)?;
        // First try CreateInterface. wpa_supplicant uses fdo error
        // `fi.w1.wpa_supplicant1.InterfaceExists` when the ifname
        // is already registered (e.g. from a prior nexusd run that
        // was killed without RemoveInterface); we translate that to
        // a GetInterface call so attach is idempotent across
        // crashes.
        let mut args: HashMap<&str, Value<'_>> = HashMap::new();
        args.insert("Ifname", Value::from(ifname));
        args.insert("Driver", Value::from("nl80211"));
        let path = match root.create_interface(args).await {
            Ok(p) => p,
            Err(e) if is_interface_exists(&e) => {
                root.get_interface(ifname).await.map_err(zbus_err)?
            }
            Err(e) => return Err(zbus_err(e)),
        };

        let iface_proxy = WpaInterfaceProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(zbus_err)?
            .build()
            .await
            .map_err(zbus_err)?;

        // Snapshot the current State before the watcher starts so a
        // freshly-attached interface in e.g. `disconnected` emits
        // one event immediately.
        if let Ok(s) = iface_proxy.state().await {
            if let Some(state) = translate_wpa_state(&s) {
                let _ = self
                    .event_tx
                    .send(SupplicantEvent::State { ifindex, state });
            }
        }

        // Subscribe to PropertiesChanged on the interface object
        // rather than to the generated `receive_state_changed`
        // stream — the property stream's lifetime is tied to the
        // proxy reference and is awkward to ship into a
        // `'static` task without extra Arc hoops.
        let props_proxy = zbus::fdo::PropertiesProxy::builder(&self.connection)
            .destination("fi.w1.wpa_supplicant1")
            .map_err(zbus_err)?
            .path(path.clone())
            .map_err(zbus_err)?
            .build()
            .await
            .map_err(zbus_err)?;
        let state_watcher = spawn_state_watcher(props_proxy, ifindex, self.event_tx.clone());

        // Re-build the interface proxy as `'static` for the ScanDone
        // watcher task. Two separate tasks (state + scan) keeps each
        // one small, and abort() on detach is straightforward.
        let iface_for_scan: WpaInterfaceProxy<'static> =
            WpaInterfaceProxy::builder(&self.connection)
                .path(path.clone())
                .map_err(zbus_err)?
                .build()
                .await
                .map_err(zbus_err)?;
        let scan_watcher = spawn_scan_watcher(iface_for_scan, ifindex, self.event_tx.clone());

        self.interfaces.insert(
            ifindex,
            AttachedInterface {
                path,
                state_watcher,
                scan_watcher,
            },
        );
        Ok(())
    }

    async fn detach(&mut self, ifindex: u32) -> Result<()> {
        let Some(AttachedInterface {
            path,
            state_watcher,
            scan_watcher,
        }) = self.interfaces.remove(&ifindex)
        else {
            return Ok(());
        };
        state_watcher.abort();
        scan_watcher.abort();
        let root = WpaSupplicantProxy::new(&self.connection)
            .await
            .map_err(zbus_err)?;
        // Tolerate "not owned" / "UnknownInterface" — if the daemon
        // already dropped the interface (e.g. after its own crash
        // and restart), our job is done.
        if let Err(e) = root.remove_interface(&path).await {
            if !is_unknown_interface(&e) {
                return Err(zbus_err(e));
            }
        }
        Ok(())
    }

    async fn scan(&mut self, ifindex: u32, params: ScanParams) -> Result<()> {
        let path = self
            .interfaces
            .get(&ifindex)
            .map(|i| i.path.clone())
            .ok_or(WifiError::NotAttached { ifindex })?;
        let iface = WpaInterfaceProxy::builder(&self.connection)
            .path(path)
            .map_err(zbus_err)?
            .build()
            .await
            .map_err(zbus_err)?;
        // Required: `Type`. Optional: `SSIDs` (aay), `Channels` (a(uu)).
        // We pass only Type for now — broadcast active/passive scan.
        // Per-SSID and per-channel narrowing lands with the connect
        // flow where targeted probes pay off.
        let type_str = if params.active { "active" } else { "passive" };
        let mut args: HashMap<&str, Value<'_>> = HashMap::new();
        args.insert("Type", Value::from(type_str));
        iface.scan(args).await.map_err(zbus_err)?;
        Ok(())
    }

    async fn get_scan_results(&self, ifindex: u32) -> Result<Vec<BssInfo>> {
        let path = self
            .interfaces
            .get(&ifindex)
            .map(|i| i.path.clone())
            .ok_or(WifiError::NotAttached { ifindex })?;
        let iface = WpaInterfaceProxy::builder(&self.connection)
            .path(path)
            .map_err(zbus_err)?
            .build()
            .await
            .map_err(zbus_err)?;
        let bss_paths = iface.bsss().await.map_err(zbus_err)?;
        let mut out = Vec::with_capacity(bss_paths.len());
        for p in bss_paths {
            match read_bss(&self.connection, p).await {
                Ok(Some(info)) => out.push(info),
                // BSS had an empty SSID (hidden AP) or malformed data.
                // Skip rather than abort — one bad cache entry can't
                // invalidate the whole scan.
                Ok(None) => continue,
                Err(e) => {
                    tracing::debug!(error = %e, "wpa_supplicant: skipping unreadable BSS");
                    continue;
                }
            }
        }
        Ok(out)
    }

    async fn connect(&mut self, _ifindex: u32, _network: &NetworkConfig) -> Result<NetworkHandle> {
        // TODO DD-003 §9.4: network-dict builder per security mode
        //       then Interface1.AddNetwork + SelectNetwork.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "connect: awaiting DD-003 §9.4 wiring".into(),
        })
    }

    async fn disconnect(&mut self, _ifindex: u32) -> Result<()> {
        // TODO DD-003 §9.4: Interface1.Disconnect.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "disconnect: awaiting DD-003 §9.4 wiring".into(),
        })
    }

    async fn forget_network(&mut self, _ifindex: u32, _handle: NetworkHandle) -> Result<()> {
        // TODO DD-003 §9.4: Interface1.RemoveNetwork(handle).
        Ok(())
    }

    async fn roam(&mut self, _ifindex: u32, _target: RoamTarget) -> Result<()> {
        // TODO DD-003 §9.3: Interface1.Roam(bssid) / Reassociate.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "roam: awaiting DD-003 §9.3 wiring".into(),
        })
    }

    async fn signal_info(&self, _ifindex: u32) -> Result<SignalInfo> {
        // TODO DD-003 §9.2: Interface1.SignalPoll → a{sv} dict.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "signal_info: awaiting DD-003 §9.2 wiring".into(),
        })
    }

    fn name(&self) -> &'static str {
        "wpa_supplicant"
    }
}

// ---- watchers ------------------------------------------------------------

/// Spawn a task that watches `NameOwnerChanged` on the system bus
/// for `fi.w1.wpa_supplicant1` and forwards `DaemonUp` / `DaemonDown`
/// through `event_tx`. The task holds an owned `Connection` (cheap
/// Arc clone) so it's `'static`.
fn spawn_daemon_watcher(
    connection: Connection,
    event_tx: broadcast::Sender<SupplicantEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let dbus = match zbus::fdo::DBusProxy::new(&connection).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "wpa_supplicant: DBus proxy init failed");
                return;
            }
        };
        // Initial owner snapshot: if wpa_supplicant is already
        // running when nexus starts, raise DaemonUp once so
        // lifecycle code doesn't wait for a real transition.
        if let Ok(owner) = dbus
            .get_name_owner("fi.w1.wpa_supplicant1".try_into().unwrap())
            .await
        {
            if !owner.is_empty() {
                let _ = event_tx.send(SupplicantEvent::DaemonUp);
            }
        }
        let mut stream = match dbus.receive_name_owner_changed().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "wpa_supplicant: NameOwnerChanged subscribe failed");
                return;
            }
        };
        while let Some(sig) = stream.next().await {
            let Ok(args) = sig.args() else { continue };
            if args.name() != "fi.w1.wpa_supplicant1" {
                continue;
            }
            // Empty `new_owner` ⇒ the name was released.
            let went_down = args
                .new_owner()
                .as_ref()
                .map(|s| s.as_str().is_empty())
                .unwrap_or(true);
            let event = if went_down {
                SupplicantEvent::DaemonDown
            } else {
                SupplicantEvent::DaemonUp
            };
            let _ = event_tx.send(event);
        }
    })
}

/// Spawn a task that watches the interface's `ScanDone` signal and
/// converts each firing into [`SupplicantEvent::ScanComplete`].
/// `success=false` is still forwarded — the backend's scan-complete
/// path treats it as "results are authoritative now, whatever they
/// are" and a failed scan just means the BSS list didn't refresh.
fn spawn_scan_watcher(
    iface: WpaInterfaceProxy<'static>,
    ifindex: u32,
    event_tx: broadcast::Sender<SupplicantEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = match iface.receive_scan_done().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(ifindex, error = %e, "wpa_supplicant: ScanDone subscribe failed");
                return;
            }
        };
        while let Some(_sig) = stream.next().await {
            let _ = event_tx.send(SupplicantEvent::ScanComplete { ifindex });
        }
    })
}

/// Read the BSS at `path` and translate to [`BssInfo`]. Returns
/// `Ok(None)` when the BSS's SSID is empty (hidden AP in probe
/// response) — callers skip those entries rather than surfacing a
/// noise row. Returns `Err` only when the D-Bus read itself fails.
async fn read_bss(
    connection: &Connection,
    path: OwnedObjectPath,
) -> Result<Option<crate::types::BssInfo>> {
    use nexus_core::{MacAddr, Ssid};

    let bss = BssProxy::builder(connection)
        .path(path)
        .map_err(zbus_err)?
        .build()
        .await
        .map_err(zbus_err)?;

    let ssid_bytes = bss.ssid().await.map_err(zbus_err)?;
    if ssid_bytes.is_empty() {
        return Ok(None);
    }
    let Ok(ssid) = Ssid::new(ssid_bytes) else {
        // >32 bytes or otherwise invalid — treat as uninteresting.
        return Ok(None);
    };
    let bssid_bytes = bss.bssid().await.map_err(zbus_err)?;
    if bssid_bytes.len() != 6 {
        return Ok(None);
    }
    let bssid = MacAddr([
        bssid_bytes[0],
        bssid_bytes[1],
        bssid_bytes[2],
        bssid_bytes[3],
        bssid_bytes[4],
        bssid_bytes[5],
    ]);
    let frequency = bss.frequency().await.map_err(zbus_err)? as u32;
    let signal_dbm = bss.signal().await.map_err(zbus_err)? as i32;
    let age_s = bss.age().await.unwrap_or(0);
    let wpa = bss.wpa().await.unwrap_or_default();
    let rsn = bss.rsn().await.unwrap_or_default();
    let security = detect_security(&wpa, &rsn);

    Ok(Some(crate::types::BssInfo {
        bssid,
        ssid,
        frequency,
        signal_dbm,
        capabilities: crate::types::BssCapabilities::default(),
        security,
        age_ms: (age_s as u64).saturating_mul(1000),
    }))
}

/// Convert wpa_supplicant's `WPA` and `RSN` a{sv} dicts into the
/// set of security modes the AP advertises. Looks only at the
/// `KeyMgmt` entry — that's enough to distinguish the six
/// [`SecurityMode`] variants the rest of the stack cares about. See
/// DD-003 §9.2 for the translation table.
fn detect_security(
    wpa: &HashMap<String, OwnedValue>,
    rsn: &HashMap<String, OwnedValue>,
) -> Vec<nexus_core::SecurityMode> {
    use nexus_core::SecurityMode;
    let rsn_mgmt = extract_key_mgmt(rsn);
    let wpa_mgmt = extract_key_mgmt(wpa);
    let mut out = Vec::new();
    // Prefer the RSN (WPA2/3) advertisement. An AP running in
    // WPA2/WPA3 transition mode lists both `wpa-psk` and `sae` here.
    if !rsn_mgmt.is_empty() {
        let has_sae = rsn_mgmt.iter().any(|s| s == "sae");
        let has_psk = rsn_mgmt
            .iter()
            .any(|s| s == "wpa-psk" || s == "wpa-psk-sha256");
        let has_eap = rsn_mgmt.iter().any(|s| s.contains("wpa-eap"));
        let has_owe = rsn_mgmt.iter().any(|s| s == "owe");
        if has_sae && has_psk {
            out.push(SecurityMode::Wpa2Wpa3Transition);
        } else if has_sae {
            out.push(SecurityMode::Wpa3Sae);
        } else if has_psk {
            out.push(SecurityMode::Wpa2Psk);
        }
        if has_eap {
            out.push(SecurityMode::Wpa2Eap);
        }
        if has_owe {
            out.push(SecurityMode::Owe);
        }
    } else if wpa_mgmt.iter().any(|s| s == "wpa-psk") {
        // Legacy WPA1-PSK. We don't have a dedicated variant; round
        // it up to Wpa2Psk so the selector at least picks something
        // rather than treating it as Open.
        out.push(SecurityMode::Wpa2Psk);
    }
    if out.is_empty() {
        out.push(SecurityMode::Open);
    }
    out
}

/// Extract the `KeyMgmt: as` from a WPA/RSN property dict.
/// Missing or wrongly-typed entries yield an empty vec — caller
/// treats that as "no key management advertised."
fn extract_key_mgmt(dict: &HashMap<String, OwnedValue>) -> Vec<String> {
    dict.get("KeyMgmt")
        .and_then(|v| <Vec<String>>::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default()
}

/// Spawn a task that watches `PropertiesChanged` on the interface
/// object and forwards each State transition as
/// [`SupplicantEvent::State`]. The watcher exits when the signal
/// stream closes (daemon went away) — the daemon watcher then
/// emits `DaemonDown` and the backend reconnects on next DaemonUp.
fn spawn_state_watcher(
    props: zbus::fdo::PropertiesProxy<'static>,
    ifindex: u32,
    event_tx: broadcast::Sender<SupplicantEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = match props.receive_properties_changed().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(ifindex, error = %e, "wpa_supplicant: PropertiesChanged subscribe failed");
                return;
            }
        };
        while let Some(sig) = stream.next().await {
            let Ok(args) = sig.args() else { continue };
            // Only the `fi.w1.wpa_supplicant1.Interface` surface
            // matters to us — skip changes on sibling interfaces
            // hosted on the same object.
            if args.interface_name != "fi.w1.wpa_supplicant1.Interface" {
                continue;
            }
            let changed: &HashMap<&str, Value<'_>> = &args.changed_properties;
            if let Some(raw) = changed.get("State") {
                // State arrives as a `Value::Str`. Convert tolerantly
                // — a non-string value would be a daemon bug, but we
                // don't want a panic path there.
                if let Ok(owned) = OwnedValue::try_from(raw) {
                    if let Ok(s) = <String>::try_from(owned) {
                        if let Some(state) = translate_wpa_state(&s) {
                            let _ = event_tx.send(SupplicantEvent::State { ifindex, state });
                        }
                    }
                }
            }
        }
    })
}

// ---- error helpers -------------------------------------------------------

fn zbus_err(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> WifiError {
    WifiError::Supplicant {
        backend: "wpa_supplicant",
        source: e.into(),
    }
}

/// wpa_supplicant raises this error name when `CreateInterface` is
/// called for an ifname it already owns.
fn is_interface_exists(e: &zbus::Error) -> bool {
    matches!(
        e,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "fi.w1.wpa_supplicant1.InterfaceExists"
    )
}

/// wpa_supplicant raises this when the interface path passed to
/// `RemoveInterface` isn't one it owns — e.g. if the daemon crashed
/// between our attach and detach.
fn is_unknown_interface(e: &zbus::Error) -> bool {
    matches!(
        e,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "fi.w1.wpa_supplicant1.InterfaceUnknown"
    )
}

// ---- state translation (unchanged from scaffold) ------------------------

/// Translate wpa_supplicant's `State` property string into a
/// [`super::SupplicantState`] shape per DD-003 §9.5.
pub fn translate_wpa_state(state_str: &str) -> Option<super::SupplicantState> {
    use super::{DisconnectHint, SupplicantState};
    match state_str {
        "scanning" => Some(SupplicantState::Scanning),
        "associating" => Some(SupplicantState::Associating),
        "authenticating" => Some(SupplicantState::Authenticating),
        "4way_handshake" | "group_handshake" => Some(SupplicantState::FourWayHandshake),
        "disconnected" => Some(SupplicantState::Disconnected {
            reason: DisconnectHint::Unspecified,
        }),
        // `associated` / `completed` / transitional states that
        // don't map 1:1 here are resolved by the PropertiesChanged
        // watcher once the BSSID / SSID fields arrive alongside
        // `completed`.
        _ => None,
    }
}

/// Translate a numeric 802.11 disconnect reason (the wire-format
/// `Reason Code` from IEEE 802.11-2020 Table 9-49) into the coarse
/// [`super::DisconnectHint`] consumed by the backend. See
/// DD-003 §9.6.
pub fn translate_disconnect_reason(code: i32) -> super::DisconnectHint {
    use super::DisconnectHint;
    match code {
        // Locally initiated disconnects
        -3 | 1 => DisconnectHint::LocalRequest,
        // Auth-related
        2 | 13 => DisconnectHint::AuthFailure,
        // 4-way handshake failure
        15 => DisconnectHint::HandshakeTimeout,
        // Association / driver timeout
        3 | 4 | 23 => DisconnectHint::AssociationTimeout,
        // PSK failures surface as reason 15 or via EAPOL events; be
        // tolerant and accept either path.
        _ => DisconnectHint::Unspecified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an `OwnedValue` wrapping an `as` (array of strings) —
    /// matches wpa_supplicant's real `KeyMgmt` property type.
    fn keymgmt_value(items: &[&str]) -> OwnedValue {
        let vec: Vec<String> = items.iter().map(|s| (*s).to_owned()).collect();
        Value::new(vec).try_into().expect("OwnedValue from as")
    }

    fn keymgmt_dict(items: &[&str]) -> HashMap<String, OwnedValue> {
        let mut m = HashMap::new();
        m.insert("KeyMgmt".to_owned(), keymgmt_value(items));
        m
    }

    #[test]
    fn security_translator_picks_wpa2_psk_from_rsn() {
        use nexus_core::SecurityMode;
        let rsn = keymgmt_dict(&["wpa-psk"]);
        let wpa = HashMap::new();
        assert_eq!(detect_security(&wpa, &rsn), vec![SecurityMode::Wpa2Psk]);
    }

    #[test]
    fn security_translator_picks_wpa3_sae_only_when_no_psk() {
        use nexus_core::SecurityMode;
        let rsn = keymgmt_dict(&["sae"]);
        let wpa = HashMap::new();
        assert_eq!(detect_security(&wpa, &rsn), vec![SecurityMode::Wpa3Sae]);
    }

    #[test]
    fn security_translator_picks_transition_mode_for_psk_plus_sae() {
        use nexus_core::SecurityMode;
        let rsn = keymgmt_dict(&["wpa-psk", "sae"]);
        let wpa = HashMap::new();
        assert_eq!(
            detect_security(&wpa, &rsn),
            vec![SecurityMode::Wpa2Wpa3Transition]
        );
    }

    #[test]
    fn security_translator_adds_enterprise_when_eap_advertised() {
        use nexus_core::SecurityMode;
        let rsn = keymgmt_dict(&["wpa-eap"]);
        let wpa = HashMap::new();
        assert_eq!(detect_security(&wpa, &rsn), vec![SecurityMode::Wpa2Eap]);
    }

    #[test]
    fn security_translator_falls_back_to_open_on_empty_dicts() {
        use nexus_core::SecurityMode;
        let rsn = HashMap::new();
        let wpa = HashMap::new();
        assert_eq!(detect_security(&wpa, &rsn), vec![SecurityMode::Open]);
    }

    #[test]
    fn security_translator_rounds_legacy_wpa1_psk_up_to_wpa2() {
        use nexus_core::SecurityMode;
        // RSN absent, only legacy WPA present — our selector doesn't
        // have a WPA1 variant; round up to Wpa2Psk so profile matching
        // at least attempts the connect.
        let rsn = HashMap::new();
        let wpa = keymgmt_dict(&["wpa-psk"]);
        assert_eq!(detect_security(&wpa, &rsn), vec![SecurityMode::Wpa2Psk]);
    }

    #[test]
    fn security_translator_owe_is_reported() {
        use nexus_core::SecurityMode;
        let rsn = keymgmt_dict(&["owe"]);
        let wpa = HashMap::new();
        assert_eq!(detect_security(&wpa, &rsn), vec![SecurityMode::Owe]);
    }

    #[test]
    fn key_mgmt_extractor_handles_missing_and_malformed() {
        let empty = HashMap::new();
        assert!(extract_key_mgmt(&empty).is_empty());
        // Wrongly-typed entry — a single string where the property
        // shape promises `as`. Should silently yield empty rather
        // than panic.
        let mut bogus = HashMap::new();
        bogus.insert("KeyMgmt".into(), Value::from("wpa-psk").try_into().unwrap());
        assert!(extract_key_mgmt(&bogus).is_empty());
    }

    #[test]
    fn state_table_covers_dd003_section_9_5() {
        use super::super::SupplicantState;
        assert!(matches!(
            translate_wpa_state("scanning"),
            Some(SupplicantState::Scanning)
        ));
        assert!(matches!(
            translate_wpa_state("associating"),
            Some(SupplicantState::Associating)
        ));
        assert!(matches!(
            translate_wpa_state("authenticating"),
            Some(SupplicantState::Authenticating)
        ));
        assert!(matches!(
            translate_wpa_state("4way_handshake"),
            Some(SupplicantState::FourWayHandshake)
        ));
        assert!(matches!(
            translate_wpa_state("group_handshake"),
            Some(SupplicantState::FourWayHandshake)
        ));
        assert!(matches!(
            translate_wpa_state("disconnected"),
            Some(SupplicantState::Disconnected { .. })
        ));
        assert!(translate_wpa_state("completed").is_none());
        assert!(translate_wpa_state("unknown_future").is_none());
    }

    #[test]
    fn disconnect_reasons_split_into_retriable_and_fail_fast() {
        use super::super::DisconnectHint;
        assert!(matches!(
            translate_disconnect_reason(15),
            DisconnectHint::HandshakeTimeout
        ));
        assert!(matches!(
            translate_disconnect_reason(2),
            DisconnectHint::AuthFailure
        ));
        assert!(matches!(
            translate_disconnect_reason(3),
            DisconnectHint::AssociationTimeout
        ));
        assert!(matches!(
            translate_disconnect_reason(-3),
            DisconnectHint::LocalRequest
        ));
        assert!(matches!(
            translate_disconnect_reason(999),
            DisconnectHint::Unspecified
        ));
    }
}
