//! wpa_supplicant-backed [`WifiSupplicantBackend`]. See DD-003 §9.
//!
//! What's wired today (runs against a live `fi.w1.wpa_supplicant1`):
//!
//! - Construction opens a system-bus connection and spawns a
//!   `NameOwnerChanged` watcher so daemon appearance / disappearance
//!   flows back as [`SupplicantEvent::DaemonUp`] / [`DaemonDown`].
//! - [`attach`] calls `CreateInterface` (or falls back to
//!   `GetInterface` when the interface is already owned), remembers
//!   the interface object path, and spawns a per-interface watcher
//!   that relays `State` property changes as
//!   [`SupplicantEvent::State`].
//! - [`detach`] aborts the watcher and calls `RemoveInterface`.
//!
//! The per-network mutating methods (`connect`, `disconnect`,
//! `scan`, `signal_info`, `roam`, `forget_network`) still return
//! [`WifiError::Supplicant`] — they need security-mode dict builders
//! per DD-003 §9.4 and the hwsim harness from §14.2 to cover the
//! handshake states. They stay stubs until that harness lands.

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
    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;
}

// ---- backend -------------------------------------------------------------

/// Bundle of things an attached interface's watcher task needs to
/// stay alive until `detach`.
struct AttachedInterface {
    path: OwnedObjectPath,
    watcher: JoinHandle<()>,
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
            iface.watcher.abort();
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
        let watcher = spawn_interface_watcher(props_proxy, ifindex, self.event_tx.clone());
        self.interfaces
            .insert(ifindex, AttachedInterface { path, watcher });
        Ok(())
    }

    async fn detach(&mut self, ifindex: u32) -> Result<()> {
        let Some(AttachedInterface { path, watcher }) = self.interfaces.remove(&ifindex) else {
            return Ok(());
        };
        watcher.abort();
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

    async fn scan(&mut self, _ifindex: u32, _params: ScanParams) -> Result<()> {
        // TODO DD-003 §9.2: Interface1.Scan(a{sv}{Type, SSIDs, Channels}).
        //       ScanDone signal → SupplicantEvent::ScanComplete.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "scan: awaiting DD-003 §9.2 wiring".into(),
        })
    }

    async fn get_scan_results(&self, _ifindex: u32) -> Result<Vec<BssInfo>> {
        // TODO DD-003 §9.2: Interface1.BSSs → read BSS1 properties.
        Ok(Vec::new())
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

/// Spawn a task that watches `PropertiesChanged` on the interface
/// object and forwards each State transition as
/// [`SupplicantEvent::State`]. The watcher exits when the signal
/// stream closes (daemon went away) — the daemon watcher then
/// emits `DaemonDown` and the backend reconnects on next DaemonUp.
fn spawn_interface_watcher(
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
