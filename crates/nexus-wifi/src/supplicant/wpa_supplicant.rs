//! wpa_supplicant-backed [`WifiSupplicantBackend`]. See DD-003 §9.
//!
//! Every [`WifiSupplicantBackend`] method runs against a live
//! `fi.w1.wpa_supplicant1` today — no method returns a `not wired`
//! stub any more:
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
//! - [`connect`] translates the profile's `SecurityConfig` into
//!   wpa_supplicant network-dict arguments (see
//!   [`build_wpa_network_args`] and DD-003 §9.4), calls
//!   `AddNetwork` + `SelectNetwork`, and returns the new network's
//!   object path as the opaque [`NetworkHandle`]. Covers every
//!   variant from DD-003 §8.1 — Open, OWE, WPA2-Personal
//!   (passphrase + raw PMK), WPA3-Personal, WPA2/WPA3 transition,
//!   WPA2-Enterprise, WPA3-Enterprise — plus PMF gating per §8.2.
//! - [`disconnect`] / [`forget_network`] / [`roam`] / [`signal_info`]
//!   all talk to the matching `Interface1` method. `roam` accepts
//!   both auto (→ `Reassociate`) and targeted-BSSID (→ `Roam`).
//! - The state watcher resolves `completed` via `CurrentBSS` reads
//!   into [`SupplicantState::Connected { bssid, ssid, frequency }`].
//! - Disconnect reason codes map to the coarse
//!   [`DisconnectHint`](super::DisconnectHint) per DD-003 §9.6.

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
    fn add_network(&self, args: HashMap<&str, Value<'_>>) -> zbus::Result<OwnedObjectPath>;
    fn select_network(&self, network: &OwnedObjectPath) -> zbus::Result<()>;
    fn remove_network(&self, network: &OwnedObjectPath) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    fn reassociate(&self) -> zbus::Result<()>;
    /// `Roam(address: s)`. Targeted roam; the argument is a BSSID
    /// formatted as `aa:bb:cc:dd:ee:ff`. Only honored when
    /// wpa_supplicant's config sets `p2p_no_group_iface` off or
    /// when the device supports directed roaming — failures come
    /// back as a `MethodError`.
    fn roam(&self, address: &str) -> zbus::Result<()>;
    /// `SignalPoll() -> a{sv}`. Returns a dict with keys
    /// `rssi` (i32), `linkspeed` (i32, Mbps), `noise` (i32, dBm),
    /// `frequency` (u32, MHz). Not every driver populates every
    /// field.
    fn signal_poll(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(property)]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "BSSs")]
    fn bsss(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    /// The BSS the interface is currently associated with. When
    /// `State` is anything other than `completed` / `associated` /
    /// `4way_handshake` / `group_handshake`, this returns `/`
    /// (the root path) — callers should treat that as "no BSS."
    #[zbus(property, name = "CurrentBSS")]
    fn current_bss(&self) -> zbus::Result<OwnedObjectPath>;

    /// Path of the network the interface most recently attempted
    /// to associate with. Used by the state watcher to resolve the
    /// `NetworkHandle` when `State` reaches `completed`.
    #[zbus(property)]
    fn current_network(&self) -> zbus::Result<OwnedObjectPath>;

    /// The 802.11 reason code from the last disconnect. Positive
    /// values are AP-initiated, negative are supplicant-initiated.
    #[zbus(property)]
    fn disconnect_reason(&self) -> zbus::Result<i32>;

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

// ---- network-dict builder (DD-003 §9.4) ---------------------------------

/// Concrete owned values that go into the wpa_supplicant network
/// dict. We build these ahead of time so the `Value<'_>` fed into
/// `AddNetwork` can borrow from owned storage that outlives the call
/// — zbus's `Value` is a short-lived borrow over bytes, so the
/// stable storage has to sit somewhere.
#[derive(Debug, Default)]
pub(crate) struct OwnedNetworkArgs {
    pub ssid: Vec<u8>,
    pub scan_ssid: Option<u32>,
    /// Space-separated list of key-management strings. Starts life
    /// as one of the DD-003 §8.1 base modes (`WPA-PSK`, `SAE`, …);
    /// [`apply_fast_transition`] prepends the matching `FT-*`
    /// variants when the profile has `fast_transition = true`.
    pub key_mgmt: String,
    pub psk: Option<String>,
    pub sae_password: Option<String>,
    pub eap: Option<&'static str>,
    pub identity: Option<String>,
    pub anonymous_identity: Option<String>,
    pub ca_cert: Option<String>,
    pub client_cert: Option<String>,
    pub private_key: Option<String>,
    pub private_key_passwd: Option<String>,
    pub password: Option<String>,
    pub phase2: Option<String>,
    pub domain_suffix_match: Option<String>,
    pub ieee80211w: u32,
    pub bssid: Option<String>,
    pub bssid_blacklist: Option<String>,
    pub priority: i32,
}

/// Translate a [`NetworkConfig`] into wpa_supplicant network-block
/// arguments per DD-003 §9.4 / §8.1 / §8.2. Pure — every IO call
/// sits in [`wpa_network_dict`], which builds the Value map used
/// by [`AddNetwork`]. Split out so tests can exercise every
/// security variant without a D-Bus connection.
pub(crate) fn build_wpa_network_args(
    config: &crate::types::NetworkConfig,
) -> Result<OwnedNetworkArgs> {
    use nexus_profile_store::{SecurityConfig, WpaPsk};
    let mut out = OwnedNetworkArgs {
        ssid: config.ssid.as_bytes().to_vec(),
        scan_ssid: if config.hidden { Some(1) } else { None },
        key_mgmt: "NONE".to_owned(),
        priority: config.priority,
        ..Default::default()
    };
    match &config.security {
        SecurityConfig::Open => {
            out.key_mgmt = "NONE".to_owned();
            out.ieee80211w = 0;
        }
        SecurityConfig::Owe => {
            out.key_mgmt = "OWE".to_owned();
            out.ieee80211w = 2;
        }
        SecurityConfig::Wpa2Personal { psk } => {
            out.key_mgmt = "WPA-PSK".to_owned();
            out.psk = Some(match psk {
                WpaPsk::Passphrase(p) => p.expose_secret().to_owned(),
                // wpa_supplicant accepts a 64-char hex string as a
                // pre-computed PMK, in lieu of the passphrase.
                WpaPsk::RawPsk(bytes) => hex_encode_psk(bytes),
            });
            out.ieee80211w = 1;
        }
        SecurityConfig::Wpa3Personal { passphrase } => {
            out.key_mgmt = "SAE".to_owned();
            out.sae_password = Some(passphrase.expose_secret().to_owned());
            out.ieee80211w = 2;
        }
        SecurityConfig::Wpa2Wpa3Personal { passphrase } => {
            // Transition mode: offer both key-managements. A single
            // passphrase covers both flavours.
            let p = passphrase.expose_secret().to_owned();
            out.key_mgmt = "WPA-PSK SAE".to_owned();
            out.psk = Some(p.clone());
            out.sae_password = Some(p);
            out.ieee80211w = 2;
        }
        SecurityConfig::Wpa2Enterprise(eap) => {
            out.key_mgmt = "WPA-EAP".to_owned();
            apply_eap(&mut out, eap);
            out.ieee80211w = 1;
        }
        SecurityConfig::Wpa3Enterprise(eap) => {
            out.key_mgmt = "WPA-EAP-SHA256".to_owned();
            apply_eap(&mut out, eap);
            out.ieee80211w = 2;
        }
    }

    if config.fast_transition {
        apply_fast_transition(&mut out);
    }

    if let Some(bssid) = &config.bssid_preferred {
        out.bssid = Some(format!("{bssid}"));
    }
    if !config.bssid_blacklist.is_empty() {
        out.bssid_blacklist = Some(
            config
                .bssid_blacklist
                .iter()
                .map(|m| format!("{m}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    Ok(out)
}

/// Prepend 802.11r Fast Transition variants to the key_mgmt list
/// based on the base mode. wpa_supplicant parses the space-
/// separated string and picks whichever variant matches the AP's
/// advertised RSN caps at association time — so "FT-PSK WPA-PSK"
/// gives us fast-roam when the AP supports it and plain PSK when
/// it doesn't. DD-003 §7.4.
///
/// Open / OWE have no FT flavour and are left alone; a profile
/// setting `fast_transition = true` on those modes is a no-op.
fn apply_fast_transition(out: &mut OwnedNetworkArgs) {
    let ft_prefix = match out.key_mgmt.as_str() {
        // Transition-mode PSK+SAE gets both FT variants.
        "WPA-PSK SAE" => "FT-PSK FT-SAE",
        "WPA-PSK" => "FT-PSK",
        "SAE" => "FT-SAE",
        "WPA-EAP" | "WPA-EAP-SHA256" => "FT-EAP",
        // Open, OWE, or any future mode without a defined FT
        // counterpart — leave the list untouched.
        _ => return,
    };
    out.key_mgmt = format!("{ft_prefix} {}", out.key_mgmt);
}

/// Flatten EAP credentials into the owned-args struct. Every field
/// is `Option` because the profile-store type itself makes them
/// all optional (a TLS config without `password` is legal, a PEAP
/// config without a CA cert isn't recommended but isn't rejected
/// at the profile layer).
fn apply_eap(out: &mut OwnedNetworkArgs, eap: &nexus_profile_store::Dot1xEapConfig) {
    out.eap = Some(eap_method_wpa_name(eap.eap));
    out.identity = Some(eap.identity.clone());
    out.anonymous_identity = eap.anonymous_identity.clone();
    out.ca_cert = eap.ca_cert.clone();
    out.client_cert = eap.client_cert.clone();
    out.private_key = eap.client_key.clone();
    out.private_key_passwd = eap
        .client_key_password
        .as_ref()
        .map(|p| p.expose_secret().to_owned());
    out.password = eap.password.as_ref().map(|p| p.expose_secret().to_owned());
    out.phase2 = eap.phase2.clone();
    out.domain_suffix_match = eap.domain_suffix_match.clone();
}

/// Uppercase name wpa_supplicant expects for each
/// [`nexus_profile_store::EapMethod`]. The `eap` network-block key
/// accepts any of these.
fn eap_method_wpa_name(m: nexus_profile_store::EapMethod) -> &'static str {
    use nexus_profile_store::EapMethod;
    match m {
        EapMethod::Peap => "PEAP",
        EapMethod::Ttls => "TTLS",
        EapMethod::Tls => "TLS",
        EapMethod::PwdMschapv2 => "PWD",
        EapMethod::Leap => "LEAP",
        EapMethod::Fast => "FAST",
    }
}

/// Hex-encode a 32-byte raw PMK for the `psk` field.
/// `hex::encode` would pull in another dep just for this one call;
/// open-coded is cheaper.
fn hex_encode_psk(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Build the `HashMap<&str, Value<'_>>` that wpa_supplicant's
/// `AddNetwork` expects from an [`OwnedNetworkArgs`]. The `Value`
/// entries borrow from `args`, so the returned map must not
/// outlive it.
fn wpa_network_dict<'a>(args: &'a OwnedNetworkArgs) -> HashMap<&'a str, Value<'a>> {
    let mut m: HashMap<&str, Value<'_>> = HashMap::new();
    m.insert("ssid", Value::from(args.ssid.as_slice()));
    if let Some(s) = args.scan_ssid {
        m.insert("scan_ssid", Value::from(s));
    }
    m.insert("key_mgmt", Value::from(args.key_mgmt.as_str()));
    if let Some(p) = &args.psk {
        m.insert("psk", Value::from(p.as_str()));
    }
    if let Some(p) = &args.sae_password {
        m.insert("sae_password", Value::from(p.as_str()));
    }
    if let Some(e) = args.eap {
        m.insert("eap", Value::from(e));
    }
    if let Some(s) = &args.identity {
        m.insert("identity", Value::from(s.as_str()));
    }
    if let Some(s) = &args.anonymous_identity {
        m.insert("anonymous_identity", Value::from(s.as_str()));
    }
    if let Some(s) = &args.ca_cert {
        m.insert("ca_cert", Value::from(s.as_str()));
    }
    if let Some(s) = &args.client_cert {
        m.insert("client_cert", Value::from(s.as_str()));
    }
    if let Some(s) = &args.private_key {
        m.insert("private_key", Value::from(s.as_str()));
    }
    if let Some(s) = &args.private_key_passwd {
        m.insert("private_key_passwd", Value::from(s.as_str()));
    }
    if let Some(s) = &args.password {
        m.insert("password", Value::from(s.as_str()));
    }
    if let Some(s) = &args.phase2 {
        m.insert("phase2", Value::from(s.as_str()));
    }
    if let Some(s) = &args.domain_suffix_match {
        m.insert("domain_suffix_match", Value::from(s.as_str()));
    }
    m.insert("ieee80211w", Value::from(args.ieee80211w));
    if let Some(s) = &args.bssid {
        m.insert("bssid", Value::from(s.as_str()));
    }
    if let Some(s) = &args.bssid_blacklist {
        m.insert("bssid_blacklist", Value::from(s.as_str()));
    }
    m.insert("priority", Value::from(args.priority));
    m
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

impl WpaSupplicantBackend {
    /// Build a per-interface `WpaInterfaceProxy` bound to the
    /// object path the backend remembered at `attach` time. Returns
    /// [`WifiError::NotAttached`] for unknown ifindices — the trait's
    /// contract says callers must `attach` first.
    async fn iface_proxy(&self, ifindex: u32) -> Result<WpaInterfaceProxy<'static>> {
        let path = self
            .interfaces
            .get(&ifindex)
            .map(|i| i.path.clone())
            .ok_or(WifiError::NotAttached { ifindex })?;
        WpaInterfaceProxy::builder(&self.connection)
            .path(path)
            .map_err(zbus_err)?
            .build()
            .await
            .map_err(zbus_err)
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
        // freshly-attached interface in e.g. `disconnected` — or in
        // `completed`, if we re-attached to an already-associated
        // interface across a nexusd restart — emits one event
        // immediately.
        if let Ok(s) = iface_proxy.state().await {
            let state_opt = if s == "completed" {
                resolve_completed(&self.connection, &iface_proxy).await
            } else {
                translate_wpa_state(&s)
            };
            if let Some(state) = state_opt {
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
        // The state watcher additionally needs a live interface proxy
        // so it can resolve `State = completed` → `Connected { bssid,
        // ssid, frequency }` via CurrentBSS reads.
        let iface_for_state: WpaInterfaceProxy<'static> =
            WpaInterfaceProxy::builder(&self.connection)
                .path(path.clone())
                .map_err(zbus_err)?
                .build()
                .await
                .map_err(zbus_err)?;
        let state_watcher = spawn_state_watcher(
            props_proxy,
            iface_for_state,
            self.connection.clone(),
            ifindex,
            self.event_tx.clone(),
        );

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
        // Required: `Type`. Optional: `SSIDs` (aay), `Channels`
        // (a(uu), pairs of `(frequency_hz, width_mhz)` — we send the
        // width as 0 to mean "let the driver pick", which matches
        // how nmcli's directed scans behave).
        let type_str = if params.active { "active" } else { "passive" };
        // Hold ownership of everything Value<'_> borrows from for
        // the duration of the call.
        let ssids_owned: Vec<Vec<u8>> =
            params.ssids.iter().map(|s| s.as_bytes().to_vec()).collect();
        let channels_owned: Vec<(u32, u32)> =
            params.frequencies.iter().map(|f| (*f, 0u32)).collect();
        let mut args: HashMap<&str, Value<'_>> = HashMap::new();
        args.insert("Type", Value::from(type_str));
        if !ssids_owned.is_empty() {
            // aay — each SSID is itself an array of bytes.
            let arr = zbus::zvariant::Array::from(&ssids_owned[..]);
            args.insert("SSIDs", Value::Array(arr));
        }
        if !channels_owned.is_empty() {
            // a(uu) — one (freq, width) tuple per requested channel.
            let arr = zbus::zvariant::Array::from(&channels_owned[..]);
            args.insert("Channels", Value::Array(arr));
        }
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

    async fn connect(&mut self, ifindex: u32, network: &NetworkConfig) -> Result<NetworkHandle> {
        let iface = self.iface_proxy(ifindex).await?;
        let owned = build_wpa_network_args(network)?;
        let args = wpa_network_dict(&owned);
        let net_path = iface.add_network(args).await.map_err(zbus_err)?;
        iface.select_network(&net_path).await.map_err(zbus_err)?;
        // Opaque handle. The raw string form is cheap to pass
        // around; the supplicant ObjectPath can be reconstructed
        // in `forget_network` via `OwnedObjectPath::try_from`.
        Ok(NetworkHandle(net_path.as_str().to_owned()))
    }

    async fn disconnect(&mut self, ifindex: u32) -> Result<()> {
        let iface = self.iface_proxy(ifindex).await?;
        iface.disconnect().await.map_err(zbus_err)?;
        Ok(())
    }

    async fn forget_network(&mut self, ifindex: u32, handle: NetworkHandle) -> Result<()> {
        let iface = self.iface_proxy(ifindex).await?;
        let path = OwnedObjectPath::try_from(handle.0).map_err(|e| WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: format!("bad network handle: {e}").into(),
        })?;
        // Tolerate `NetworkUnknown` — the supplicant may already have
        // dropped the network via its own housekeeping (e.g. our
        // earlier `connect` for a different profile removed it).
        if let Err(e) = iface.remove_network(&path).await {
            if !is_network_unknown(&e) {
                return Err(zbus_err(e));
            }
        }
        Ok(())
    }

    async fn roam(&mut self, ifindex: u32, target: RoamTarget) -> Result<()> {
        let iface = self.iface_proxy(ifindex).await?;
        match target {
            RoamTarget::Auto => iface.reassociate().await.map_err(zbus_err)?,
            RoamTarget::Bss(mac) => {
                // wpa_supplicant wants the BSSID as a formatted
                // string, not raw bytes.
                iface.roam(&format!("{mac}")).await.map_err(zbus_err)?;
            }
        }
        Ok(())
    }

    async fn signal_info(&self, ifindex: u32) -> Result<SignalInfo> {
        let iface = self.iface_proxy(ifindex).await?;
        let dict = iface.signal_poll().await.map_err(zbus_err)?;
        // Fields are all nominally optional — different drivers
        // populate different subsets. Missing → 0 / None so the
        // caller at least gets the rssi snapshot.
        let rssi_dbm = signal_i32(&dict, "rssi").unwrap_or(0);
        let noise_dbm = signal_i32(&dict, "noise");
        let snr_db = noise_dbm.map(|n| rssi_dbm - n);
        let frequency = signal_u32(&dict, "frequency").unwrap_or(0);
        let linkspeed = signal_i32(&dict, "linkspeed").unwrap_or(0);
        // wpa_supplicant only publishes one rate value; we expose
        // the same number as both tx and rx until the supplicant
        // grows separate counters.
        let rate = linkspeed.max(0) as f32;
        // `SignalInfo::bssid` is required. Pull it from the live
        // association; fall back to zero MAC when we're not
        // currently associated (the caller treats that as stale).
        let current_bss_path = iface.current_bss().await.map_err(zbus_err)?;
        let bssid = read_bssid(&self.connection, current_bss_path)
            .await
            .unwrap_or(nexus_core::MacAddr([0; 6]));
        Ok(SignalInfo {
            bssid,
            rssi_dbm,
            noise_dbm,
            snr_db,
            tx_bitrate_mbps: rate,
            rx_bitrate_mbps: rate,
            frequency,
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
        while let Some(sig) = stream.next().await {
            // `ScanDone(success=false)` means the scan was started
            // but no new results were written to the BSS cache —
            // driver busy, radar-induced NOP, concurrent scan
            // conflict, etc. We still forward `ScanComplete` so
            // the scheduler's post-scan hook runs, but log the
            // failure at debug so repeated driver problems are
            // visible in steady-state traces.
            if let Ok(args) = sig.args() {
                if !args.success {
                    tracing::debug!(ifindex, "wpa_supplicant: ScanDone success=false");
                }
            }
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
/// How often the state watcher polls as a fallback against missed
/// `PropertiesChanged` signals. Two zbus property reads per tick
/// per attached interface — cheap — and idempotent at the backend's
/// dispatch layer (same state = no-op).
const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

fn spawn_state_watcher(
    props: zbus::fdo::PropertiesProxy<'static>,
    iface: WpaInterfaceProxy<'static>,
    connection: Connection,
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
        // Reconciliation tick. `PropertiesChanged` is the primary
        // path; this is a safety net for two known failure modes
        // observed on live hardware:
        //
        //   (a) Under heavy state churn, wpa_supplicant sometimes
        //       advances `State` (e.g. `4way_handshake → completed`)
        //       without firing a corresponding PropertiesChanged
        //       for the new value. Reproduced on the Pi: the
        //       interface was associated with a valid IP while our
        //       cached state stayed at `handshaking` indefinitely.
        //   (b) A rapid burst of signals can exceed the zbus
        //       message stream's buffer; older entries get dropped.
        //
        // The tick re-reads `State` (and `CurrentBSS` for
        // `completed`) and emits whatever the authoritative value
        // is. `on_supplicant_state` in the backend is idempotent
        // for steady-state — duplicates cost the two property
        // reads and a broadcast send, nothing more.
        let mut tick = tokio::time::interval(RECONCILE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                maybe_sig = stream.next() => {
                    let Some(sig) = maybe_sig else { return };
                    let Ok(args) = sig.args() else { continue };
                    if args.interface_name != "fi.w1.wpa_supplicant1.Interface" {
                        continue;
                    }
                    let changed: &HashMap<&str, Value<'_>> = &args.changed_properties;
                    if !(changed.contains_key("State") || changed.contains_key("CurrentBSS")) {
                        continue;
                    }
                    evaluate_and_emit(&connection, &iface, ifindex, &event_tx).await;
                }
                _ = tick.tick() => {
                    evaluate_and_emit(&connection, &iface, ifindex, &event_tx).await;
                }
            }
        }
    })
}

/// Read the interface's current State (and for `completed`,
/// `CurrentBSS` via [`resolve_completed`]) and broadcast the
/// resulting [`SupplicantEvent::State`]. Shared by the signal path
/// and the reconciliation tick in [`spawn_state_watcher`].
async fn evaluate_and_emit(
    connection: &Connection,
    iface: &WpaInterfaceProxy<'_>,
    ifindex: u32,
    event_tx: &broadcast::Sender<SupplicantEvent>,
) {
    let state_str = match iface.state().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(ifindex, error = %e, "wpa_supplicant: State read failed");
            return;
        }
    };
    let state_opt = if state_str == "completed" {
        resolve_completed(connection, iface).await
    } else {
        translate_wpa_state(&state_str)
    };
    if let Some(state) = state_opt {
        let _ = event_tx.send(SupplicantEvent::State { ifindex, state });
    }
}

// ---- error helpers -------------------------------------------------------

fn zbus_err(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> WifiError {
    WifiError::Supplicant {
        backend: "wpa_supplicant",
        source: e.into(),
    }
}

/// Decode an i32 out of wpa_supplicant's `SignalPoll` a{sv}. The
/// supplicant picks the smallest integer type that fits, so we
/// accept i16/i32/u16/u32 and widen.
fn signal_i32(dict: &HashMap<String, OwnedValue>, key: &str) -> Option<i32> {
    let v = dict.get(key)?;
    if let Ok(n) = i32::try_from(v) {
        return Some(n);
    }
    if let Ok(n) = i16::try_from(v) {
        return Some(n as i32);
    }
    if let Ok(n) = u32::try_from(v) {
        return Some(n as i32);
    }
    if let Ok(n) = u16::try_from(v) {
        return Some(n as i32);
    }
    None
}

/// Same as [`signal_i32`] but for unsigned fields like `frequency`.
fn signal_u32(dict: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    let v = dict.get(key)?;
    if let Ok(n) = u32::try_from(v) {
        return Some(n);
    }
    if let Ok(n) = u16::try_from(v) {
        return Some(n as u32);
    }
    if let Ok(n) = i32::try_from(v) {
        if n >= 0 {
            return Some(n as u32);
        }
    }
    None
}

/// Pull the BSSID from a BSS object at `path`. Returns `None` when
/// `path` is the root (`/`, reported while disassociated) or the
/// read fails — callers substitute the zero MAC.
async fn read_bssid(conn: &Connection, path: OwnedObjectPath) -> Option<nexus_core::MacAddr> {
    if path.as_str() == "/" {
        return None;
    }
    let bss = BssProxy::builder(conn)
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;
    let bytes = bss.bssid().await.ok()?;
    if bytes.len() != 6 {
        return None;
    }
    Some(nexus_core::MacAddr([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5],
    ]))
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

/// Similarly tolerant for `RemoveNetwork`: the network may already
/// be gone (e.g. wpa_supplicant auto-purged it after a prior
/// failure, or we're replaying `forget` on a stale handle).
fn is_network_unknown(e: &zbus::Error) -> bool {
    matches!(
        e,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "fi.w1.wpa_supplicant1.NetworkUnknown"
    )
}

// ---- state translation (unchanged from scaffold) ------------------------

/// Translate wpa_supplicant's `State` property string into a
/// [`super::SupplicantState`] shape per DD-003 §9.5.
///
/// Every state except `completed` maps cleanly to a single variant
/// here. `completed` carries BSSID / SSID / frequency fields that
/// can only be populated by reading `CurrentBSS`, so it returns
/// `None` from the pure translator — see [`resolve_completed`] for
/// the IO-bound path the watcher uses.
pub fn translate_wpa_state(state_str: &str) -> Option<super::SupplicantState> {
    use super::{DisconnectHint, SupplicantState};
    match state_str {
        "inactive" => Some(SupplicantState::Disconnected {
            reason: DisconnectHint::LocalRequest,
        }),
        "scanning" => Some(SupplicantState::Scanning),
        "authenticating" => Some(SupplicantState::Authenticating),
        "associating" | "associated" => Some(SupplicantState::Associating),
        "4way_handshake" | "group_handshake" => Some(SupplicantState::FourWayHandshake),
        "disconnected" => Some(SupplicantState::Disconnected {
            reason: DisconnectHint::Unspecified,
        }),
        // `completed` is resolved by `resolve_completed` — we
        // can't build the `Connected` variant without a D-Bus read
        // of `CurrentBSS`.
        _ => None,
    }
}

/// Build a [`SupplicantState::Connected`] by reading the given
/// interface's `CurrentBSS` and then that BSS's `SSID` / `BSSID` /
/// `Frequency` properties. Returns `None` when `CurrentBSS` is `/`
/// (the daemon briefly reports `completed` before the association
/// fully settles, during which the BSS path is unset).
async fn resolve_completed(
    connection: &Connection,
    iface: &WpaInterfaceProxy<'_>,
) -> Option<super::SupplicantState> {
    use super::SupplicantState;
    let path = iface.current_bss().await.ok()?;
    if path.as_str() == "/" {
        return None;
    }
    let bss = BssProxy::builder(connection)
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;
    let ssid_bytes = bss.ssid().await.ok()?;
    let ssid = nexus_core::Ssid::new(ssid_bytes).ok()?;
    let bssid_bytes = bss.bssid().await.ok()?;
    if bssid_bytes.len() != 6 {
        return None;
    }
    let bssid = nexus_core::MacAddr([
        bssid_bytes[0],
        bssid_bytes[1],
        bssid_bytes[2],
        bssid_bytes[3],
        bssid_bytes[4],
        bssid_bytes[5],
    ]);
    let frequency = bss.frequency().await.ok()? as u32;
    Some(SupplicantState::Connected {
        bssid,
        ssid,
        frequency,
    })
}

/// Translate a numeric 802.11 disconnect reason (the wire-format
/// `Reason Code` from IEEE 802.11-2020 Table 9-49) into the coarse
/// [`super::DisconnectHint`] consumed by the backend. See
/// DD-003 §9.6.
///
/// wpa_supplicant re-uses the same `i32` for locally-initiated
/// disconnects: negative values are its own codes, positive values
/// are the wire reason codes. Zero means "unspecified / absent."
pub fn translate_disconnect_reason(code: i32) -> super::DisconnectHint {
    use super::DisconnectHint;
    match code {
        // Locally-initiated: Nexus-side disconnect (-3) or the
        // station voluntarily leaving (802.11 reason 3).
        -3 | 3 => DisconnectHint::LocalRequest,
        // AP-initiated deauth with "unspecified reason" — AP still
        // reachable but no longer willing to talk. Treated like a
        // plain disconnect with hope of reassoc.
        1 => DisconnectHint::Unspecified,
        // Previous authentication no longer valid (802.11 reason 2)
        // and 802.1X EAP failure (reason 23): both indicate the
        // credentials the supplicant presented are stale / wrong.
        2 | 13 | 23 => DisconnectHint::AuthFailure,
        // 802.1X-wrapped handshake timeout.
        15 => DisconnectHint::HandshakeTimeout,
        // Association / driver timeout: reason 17 is
        // "association timeout from AP".
        17 => DisconnectHint::AssociationTimeout,
        // Inactivity deauth (reason 4) and class-2-frame
        // protocol glitches (6, 7): AP still reachable; no
        // credential change needed; typically retriable.
        4 | 6 | 7 => DisconnectHint::Unspecified,
        // 0 / unmapped positive codes → unspecified.
        _ => DisconnectHint::Unspecified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- build_wpa_network_args (DD-003 §9.4) ---------------------------

    fn mk_network_config(
        ssid: &[u8],
        security: nexus_profile_store::SecurityConfig,
    ) -> crate::types::NetworkConfig {
        crate::types::NetworkConfig {
            ssid: nexus_core::Ssid::new(ssid.to_vec()).unwrap(),
            hidden: false,
            security,
            priority: 0,
            bssid_preferred: None,
            bssid_blacklist: Vec::new(),
            scan_freqs: Vec::new(),
            fast_transition: false,
        }
    }

    #[test]
    fn builder_open_uses_none_keymgmt_and_disables_pmf() {
        use nexus_profile_store::SecurityConfig;
        let c = mk_network_config(b"open-net", SecurityConfig::Open);
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "NONE");
        assert_eq!(a.ieee80211w, 0);
        assert!(a.psk.is_none());
        assert!(a.sae_password.is_none());
        assert_eq!(a.ssid, b"open-net".to_vec());
    }

    #[test]
    fn builder_owe_requires_pmf() {
        use nexus_profile_store::SecurityConfig;
        let c = mk_network_config(b"owe-net", SecurityConfig::Owe);
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "OWE");
        assert_eq!(a.ieee80211w, 2);
    }

    #[test]
    fn builder_wpa2_personal_passphrase_populates_psk_and_pmf_capable() {
        use nexus_profile_store::{SecurityConfig, WpaPsk};
        let c = mk_network_config(
            b"home",
            SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(crate::secretstring("correct horse battery staple")),
            },
        );
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "WPA-PSK");
        assert_eq!(a.psk.as_deref(), Some("correct horse battery staple"));
        assert!(a.sae_password.is_none());
        assert_eq!(a.ieee80211w, 1);
    }

    #[test]
    fn builder_wpa2_personal_rawpsk_hex_encodes_the_pmk() {
        use nexus_profile_store::{SecurityConfig, WpaPsk};
        let mut pmk = [0u8; 32];
        pmk[0] = 0xDE;
        pmk[1] = 0xAD;
        pmk[2] = 0xBE;
        pmk[3] = 0xEF;
        let c = mk_network_config(
            b"home",
            SecurityConfig::Wpa2Personal {
                psk: WpaPsk::RawPsk(pmk),
            },
        );
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "WPA-PSK");
        // Must be 64 hex chars.
        assert_eq!(a.psk.as_ref().unwrap().len(), 64);
        assert!(a.psk.as_ref().unwrap().starts_with("deadbeef"));
    }

    #[test]
    fn builder_wpa3_personal_uses_sae_password_and_required_pmf() {
        use nexus_profile_store::SecurityConfig;
        let c = mk_network_config(
            b"home3",
            SecurityConfig::Wpa3Personal {
                passphrase: crate::secretstring("sae-pass"),
            },
        );
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "SAE");
        assert!(a.psk.is_none());
        assert_eq!(a.sae_password.as_deref(), Some("sae-pass"));
        assert_eq!(a.ieee80211w, 2);
    }

    #[test]
    fn builder_transition_mode_sets_both_psk_and_sae() {
        use nexus_profile_store::SecurityConfig;
        let c = mk_network_config(
            b"home-mixed",
            SecurityConfig::Wpa2Wpa3Personal {
                passphrase: crate::secretstring("shared-pass"),
            },
        );
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "WPA-PSK SAE");
        assert_eq!(a.psk.as_deref(), Some("shared-pass"));
        assert_eq!(a.sae_password.as_deref(), Some("shared-pass"));
        assert_eq!(a.ieee80211w, 2);
    }

    fn mk_eap() -> nexus_profile_store::Dot1xEapConfig {
        use nexus_profile_store::{Dot1xEapConfig, EapMethod};
        Dot1xEapConfig {
            eap: EapMethod::Peap,
            identity: "alice".into(),
            anonymous_identity: Some("anon@example".into()),
            ca_cert: Some("/etc/nexus/ca.pem".into()),
            client_cert: None,
            client_key: None,
            client_key_password: None,
            phase2: Some("auth=MSCHAPV2".into()),
            domain_suffix_match: Some("example.com".into()),
            password: Some(crate::secretstring("hunter2")),
        }
    }

    #[test]
    fn builder_wpa2_enterprise_sets_eap_fields_and_capable_pmf() {
        use nexus_profile_store::SecurityConfig;
        let c = mk_network_config(b"corp2", SecurityConfig::Wpa2Enterprise(mk_eap()));
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "WPA-EAP");
        assert_eq!(a.eap, Some("PEAP"));
        assert_eq!(a.identity.as_deref(), Some("alice"));
        assert_eq!(a.anonymous_identity.as_deref(), Some("anon@example"));
        assert_eq!(a.ca_cert.as_deref(), Some("/etc/nexus/ca.pem"));
        assert_eq!(a.phase2.as_deref(), Some("auth=MSCHAPV2"));
        assert_eq!(a.domain_suffix_match.as_deref(), Some("example.com"));
        assert_eq!(a.password.as_deref(), Some("hunter2"));
        assert_eq!(a.ieee80211w, 1);
    }

    #[test]
    fn builder_wpa3_enterprise_uses_sha256_keymgmt_and_required_pmf() {
        use nexus_profile_store::SecurityConfig;
        let c = mk_network_config(b"corp3", SecurityConfig::Wpa3Enterprise(mk_eap()));
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "WPA-EAP-SHA256");
        assert_eq!(a.ieee80211w, 2);
    }

    #[test]
    fn builder_fast_transition_prepends_ft_psk() {
        use nexus_profile_store::{SecurityConfig, WpaPsk};
        let mut c = mk_network_config(
            b"home",
            SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(crate::secretstring("pw")),
            },
        );
        c.fast_transition = true;
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "FT-PSK WPA-PSK");
    }

    #[test]
    fn builder_fast_transition_prepends_ft_sae() {
        use nexus_profile_store::SecurityConfig;
        let mut c = mk_network_config(
            b"home3",
            SecurityConfig::Wpa3Personal {
                passphrase: crate::secretstring("pw"),
            },
        );
        c.fast_transition = true;
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "FT-SAE SAE");
    }

    #[test]
    fn builder_fast_transition_prepends_ft_eap_for_both_enterprise_flavours() {
        use nexus_profile_store::SecurityConfig;
        let mut c = mk_network_config(b"corp2", SecurityConfig::Wpa2Enterprise(mk_eap()));
        c.fast_transition = true;
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "FT-EAP WPA-EAP");

        let mut c3 = mk_network_config(b"corp3", SecurityConfig::Wpa3Enterprise(mk_eap()));
        c3.fast_transition = true;
        let a3 = build_wpa_network_args(&c3).unwrap();
        assert_eq!(a3.key_mgmt, "FT-EAP WPA-EAP-SHA256");
    }

    #[test]
    fn builder_fast_transition_on_open_and_owe_is_noop() {
        use nexus_profile_store::SecurityConfig;
        let mut open = mk_network_config(b"open", SecurityConfig::Open);
        open.fast_transition = true;
        assert_eq!(build_wpa_network_args(&open).unwrap().key_mgmt, "NONE");

        let mut owe = mk_network_config(b"owe", SecurityConfig::Owe);
        owe.fast_transition = true;
        assert_eq!(build_wpa_network_args(&owe).unwrap().key_mgmt, "OWE");
    }

    #[test]
    fn builder_transition_mode_with_ft_adds_both_ft_variants() {
        use nexus_profile_store::SecurityConfig;
        let mut c = mk_network_config(
            b"mixed",
            SecurityConfig::Wpa2Wpa3Personal {
                passphrase: crate::secretstring("shared"),
            },
        );
        c.fast_transition = true;
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.key_mgmt, "FT-PSK FT-SAE WPA-PSK SAE");
    }

    #[test]
    fn builder_hidden_sets_scan_ssid() {
        use nexus_profile_store::SecurityConfig;
        let mut c = mk_network_config(b"stealth", SecurityConfig::Open);
        c.hidden = true;
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.scan_ssid, Some(1));
    }

    #[test]
    fn builder_bssid_and_blacklist_are_formatted() {
        use nexus_core::MacAddr;
        use nexus_profile_store::SecurityConfig;
        let mut c = mk_network_config(b"pinned", SecurityConfig::Open);
        c.bssid_preferred = Some(MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01]));
        c.bssid_blacklist = vec![
            MacAddr([0x11; 6]),
            MacAddr([0x22, 0x33, 0x44, 0x55, 0x66, 0x77]),
        ];
        let a = build_wpa_network_args(&c).unwrap();
        assert_eq!(a.bssid.as_deref(), Some("aa:bb:cc:dd:ee:01"));
        // Space-separated list per wpa_supplicant's accepted form.
        assert_eq!(
            a.bssid_blacklist.as_deref(),
            Some("11:11:11:11:11:11 22:33:44:55:66:77")
        );
    }

    #[test]
    fn network_dict_round_trips_every_owned_field() {
        // Builds the `HashMap<&str, Value<'_>>` and spot-checks that
        // the entries we set end up in the dict. The Value types
        // don't round-trip cleanly through equality (they're not
        // PartialEq); we just assert presence for the non-trivial
        // fields.
        use nexus_profile_store::{SecurityConfig, WpaPsk};
        let c = mk_network_config(
            b"ssid",
            SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(crate::secretstring("pw")),
            },
        );
        let owned = build_wpa_network_args(&c).unwrap();
        let dict = wpa_network_dict(&owned);
        assert!(dict.contains_key("ssid"));
        assert!(dict.contains_key("key_mgmt"));
        assert!(dict.contains_key("psk"));
        assert!(dict.contains_key("ieee80211w"));
        assert!(dict.contains_key("priority"));
        // `sae_password` is only present on SAE/transition configs.
        assert!(!dict.contains_key("sae_password"));
    }

    // ---- signal-poll helpers --------------------------------------------

    #[test]
    fn signal_i32_accepts_multiple_integer_widths() {
        let mut d = HashMap::new();
        d.insert("rssi".into(), Value::from(-65i32).try_into().unwrap());
        d.insert("noise".into(), Value::from(-95i16).try_into().unwrap());
        d.insert("linkspeed".into(), Value::from(150u32).try_into().unwrap());
        assert_eq!(signal_i32(&d, "rssi"), Some(-65));
        assert_eq!(signal_i32(&d, "noise"), Some(-95));
        assert_eq!(signal_i32(&d, "linkspeed"), Some(150));
        assert_eq!(signal_i32(&d, "missing"), None);
    }

    #[test]
    fn signal_u32_rejects_negative_i32() {
        let mut d = HashMap::new();
        d.insert("freq".into(), Value::from(-1i32).try_into().unwrap());
        assert_eq!(signal_u32(&d, "freq"), None);
    }

    // ---- existing tests -------------------------------------------------

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
            translate_wpa_state("inactive"),
            Some(SupplicantState::Disconnected { .. })
        ));
        assert!(matches!(
            translate_wpa_state("scanning"),
            Some(SupplicantState::Scanning)
        ));
        assert!(matches!(
            translate_wpa_state("associating"),
            Some(SupplicantState::Associating)
        ));
        // `associated` rolls up into Associating — the BSS is
        // chosen but the 4-way hasn't begun.
        assert!(matches!(
            translate_wpa_state("associated"),
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
        // `completed` resolves via `resolve_completed` (IO-bound);
        // the pure translator returns None.
        assert!(translate_wpa_state("completed").is_none());
        assert!(translate_wpa_state("unknown_future").is_none());
    }

    #[test]
    fn disconnect_reasons_cover_dd003_section_9_6() {
        use super::super::DisconnectHint;
        // Locally-initiated
        assert_eq!(
            translate_disconnect_reason(-3),
            DisconnectHint::LocalRequest
        );
        // STA leaving (reason 3)
        assert_eq!(translate_disconnect_reason(3), DisconnectHint::LocalRequest);
        // AP-initiated unspecified deauth
        assert_eq!(translate_disconnect_reason(1), DisconnectHint::Unspecified);
        // Auth-related
        assert_eq!(translate_disconnect_reason(2), DisconnectHint::AuthFailure);
        // 802.1X EAP failure
        assert_eq!(translate_disconnect_reason(23), DisconnectHint::AuthFailure);
        // 4-way handshake timeout
        assert_eq!(
            translate_disconnect_reason(15),
            DisconnectHint::HandshakeTimeout
        );
        // Association timeout
        assert_eq!(
            translate_disconnect_reason(17),
            DisconnectHint::AssociationTimeout
        );
        // Inactivity deauth — retriable, coarsely unspecified
        assert_eq!(translate_disconnect_reason(4), DisconnectHint::Unspecified);
        // Protocol glitches
        assert_eq!(translate_disconnect_reason(6), DisconnectHint::Unspecified);
        // Unknown codes
        assert_eq!(
            translate_disconnect_reason(999),
            DisconnectHint::Unspecified
        );
    }
}
