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
    ///
    /// WORKAROUND: declared return type is `OwnedValue` rather than
    /// `HashMap<String, OwnedValue>` because some wpa_supplicant
    /// builds (observed on Raspberry Pi OS / wpa_supplicant 2.10
    /// and the Yocto kirkstone packaging) marshal the dict wrapped
    /// in a variant — body signature `v(a{sv})` instead of the
    /// documented `a{sv}`. zbus rejects the mismatch with
    /// `Signature mismatch: got 'v', expected 'a{sv}'`. By accepting
    /// the most permissive type here, the call site in
    /// [`signal_info`] handles either layout (direct dict, or
    /// variant-wrapped dict) at runtime.
    fn signal_poll(&self) -> zbus::Result<OwnedValue>;

    /// Reply to a `NetworkRequest` signal. `path` is the network
    /// object path from the request; `field` is echoed so the
    /// supplicant can match the reply to the outstanding prompt;
    /// `value` is the credential. DD-003 §9.2.
    fn network_reply(
        &self,
        path: &OwnedObjectPath,
        field: &str,
        value: &str,
    ) -> zbus::Result<()>;

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

    /// `NetworkRequest(network: o, field: s, text: s)` — emitted by
    /// wpa_supplicant when it needs an out-of-band credential such
    /// as an OTP or a password for `ext_password=1` fields.
    #[zbus(signal)]
    fn network_request(
        &self,
        network: OwnedObjectPath,
        field: String,
        text: String,
    ) -> zbus::Result<()>;

    /// `BSSAdded(path: o, properties: a{sv})` — fires whenever the
    /// supplicant adds an entry to its BSS cache outside of a
    /// driven `Scan()` call (e.g. a passive Beacon update). S5 /
    /// DD-003 §9.2.
    #[zbus(signal)]
    fn bss_added(
        &self,
        path: OwnedObjectPath,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    /// `BSSRemoved(path: o)` — fires when an aged-out BSS leaves
    /// the supplicant's cache. S5 / DD-003 §9.2.
    #[zbus(signal)]
    fn bss_removed(&self, path: OwnedObjectPath) -> zbus::Result<()>;
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

    /// Returned by wpa_supplicant as `q` (uint16) today; declared
    /// here as [`OwnedValue`] so a future kernel/supplicant change
    /// to `u` (uint32) — necessary if 6 GHz extensions push past
    /// 65535 MHz — doesn't break the BSS read path. The K7
    /// `bss_frequency_mhz` helper widens whichever wire type
    /// shows up.
    #[zbus(property)]
    fn frequency(&self) -> zbus::Result<OwnedValue>;

    #[zbus(property)]
    fn signal(&self) -> zbus::Result<i16>;

    #[zbus(property)]
    fn age(&self) -> zbus::Result<u32>;

    #[zbus(property, name = "WPA")]
    fn wpa(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    #[zbus(property, name = "RSN")]
    fn rsn(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    /// Raw 802.11 Information Elements concatenation as it
    /// appeared in the most recent Beacon / Probe Response. K2:
    /// parsed for HT (45) / VHT (191) / HE (255+ext 35) /
    /// EHT (255+ext 108) / WPS (221 vendor-specific) presence,
    /// and the RSN element (48) for the PMF capability byte.
    #[zbus(property, name = "IEs")]
    fn ies(&self) -> zbus::Result<Vec<u8>>;
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
    request_watcher: JoinHandle<()>,
    bss_watcher: JoinHandle<()>,
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
            iface.request_watcher.abort();
            iface.bss_watcher.abort();
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
        // immediately. The snapshot goes through the same resolver
        // the watcher uses so `disconnected` carries a real reason
        // (read from `DisconnectReason`) rather than a stock
        // `Unspecified`.
        if let Some(state) = resolve_state(&self.connection, &iface_proxy).await {
            let _ = self
                .event_tx
                .send(SupplicantEvent::State { ifindex, state });
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

        // `NetworkRequest` gets its own watcher — DD-003 §9.2. The
        // request stream is low-volume (fires only on OTP /
        // ext-password prompts) but its latency matters when it
        // does; bundling with the state watcher would delay
        // credential prompts behind property-read syscalls.
        let iface_for_request: WpaInterfaceProxy<'static> =
            WpaInterfaceProxy::builder(&self.connection)
                .path(path.clone())
                .map_err(zbus_err)?
                .build()
                .await
                .map_err(zbus_err)?;
        let request_watcher =
            spawn_request_watcher(iface_for_request, ifindex, self.event_tx.clone());

        // `BSSAdded` / `BSSRemoved` watcher (S5). Both signals
        // funnel into `SupplicantEvent::BssCacheStale` so the
        // backend re-reads the BSS list without emitting a public
        // `WifiScanComplete` event for every Beacon-driven update.
        let iface_for_bss: WpaInterfaceProxy<'static> =
            WpaInterfaceProxy::builder(&self.connection)
                .path(path.clone())
                .map_err(zbus_err)?
                .build()
                .await
                .map_err(zbus_err)?;
        let bss_watcher = spawn_bss_watcher(iface_for_bss, ifindex, self.event_tx.clone());

        self.interfaces.insert(
            ifindex,
            AttachedInterface {
                path,
                state_watcher,
                scan_watcher,
                request_watcher,
                bss_watcher,
            },
        );
        Ok(())
    }

    async fn detach(&mut self, ifindex: u32) -> Result<()> {
        let Some(AttachedInterface {
            path,
            state_watcher,
            scan_watcher,
            request_watcher,
            bss_watcher,
        }) = self.interfaces.remove(&ifindex)
        else {
            return Ok(());
        };
        state_watcher.abort();
        scan_watcher.abort();
        request_watcher.abort();
        bss_watcher.abort();
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
        // `AllowRoam` controls whether wpa_supplicant may use this
        // scan's results to autonomously switch BSSes. Nexus is the
        // authority in `off` / `nexus` modes; only hand the decision
        // to the supplicant when `allow_roam` is set. DD-003 §4.1 /
        // §9.3.
        args.insert("AllowRoam", Value::from(params.allow_roam));
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
            // `Reassociate` is the closest single-call wpa_supplicant
            // primitive; it does NOT pick a different BSS unless
            // `bg_scan` has stashed a fresher candidate. See the
            // RoamTarget::Auto rustdoc for the K3 caveats — the
            // backend nudges callers toward `Bss(...)` for
            // `roaming_mode = "nexus"`.
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
        let raw = iface.signal_poll().await.map_err(zbus_err)?;
        let dict = unwrap_signal_poll_dict(raw).map_err(|e| WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: format!("decoding SignalPoll reply: {e}").into(),
        })?;
        // Fields are all nominally optional — different drivers
        // populate different subsets. Missing → 0 / None so the
        // caller at least gets the rssi snapshot.
        let rssi_dbm = signal_i32(&dict, "rssi").unwrap_or(0);
        let noise_dbm = signal_i32(&dict, "noise");
        let snr_db = noise_dbm.map(|n| rssi_dbm - n);
        let frequency = signal_u32(&dict, "frequency").unwrap_or(0);
        let linkspeed = signal_i32(&dict, "linkspeed").unwrap_or(0);
        let bitrate_mbps = linkspeed.max(0) as f32;
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
            bitrate_mbps,
            frequency,
        })
    }

    async fn provide_network_credential(
        &mut self,
        ifindex: u32,
        network: &str,
        field: &str,
        value: &str,
    ) -> Result<()> {
        let iface = self.iface_proxy(ifindex).await?;
        let path = OwnedObjectPath::try_from(network.to_owned()).map_err(|e| {
            WifiError::Supplicant {
                backend: "wpa_supplicant",
                source: format!("bad network path: {e}").into(),
            }
        })?;
        iface.network_reply(&path, field, value).await.map_err(zbus_err)?;
        Ok(())
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
            // the scheduler's post-scan hook runs; the success bit
            // is forwarded so the backend can label the metric
            // outcome (K5).
            let success = sig.args().map(|a| a.success).unwrap_or(true);
            if !success {
                tracing::debug!(ifindex, "wpa_supplicant: ScanDone success=false");
            }
            let _ = event_tx.send(SupplicantEvent::ScanComplete { ifindex, success });
        }
    })
}

/// Spawn a task that watches the interface's `NetworkRequest`
/// signal and converts each into [`SupplicantEvent::NetworkRequest`].
/// See DD-003 §9.2.
fn spawn_request_watcher(
    iface: WpaInterfaceProxy<'static>,
    ifindex: u32,
    event_tx: broadcast::Sender<SupplicantEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut stream = match iface.receive_network_request().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    ifindex,
                    error = %e,
                    "wpa_supplicant: NetworkRequest subscribe failed"
                );
                return;
            }
        };
        while let Some(sig) = stream.next().await {
            let Ok(args) = sig.args() else { continue };
            let _ = event_tx.send(SupplicantEvent::NetworkRequest {
                ifindex,
                network: args.network.as_str().to_owned(),
                field: args.field.clone(),
                text: args.text.clone(),
            });
        }
    })
}

/// Spawn a task that watches `BSSAdded` and `BSSRemoved` and
/// folds both into [`SupplicantEvent::BssCacheStale`]. The
/// backend re-reads the BSS list on each — cheap, and
/// continuous-freshness updates shouldn't masquerade as
/// `WifiScanComplete` to operator clients. S5.
fn spawn_bss_watcher(
    iface: WpaInterfaceProxy<'static>,
    ifindex: u32,
    event_tx: broadcast::Sender<SupplicantEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let added = match iface.receive_bss_added().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    ifindex,
                    error = %e,
                    "wpa_supplicant: BSSAdded subscribe failed"
                );
                return;
            }
        };
        let removed = match iface.receive_bss_removed().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    ifindex,
                    error = %e,
                    "wpa_supplicant: BSSRemoved subscribe failed"
                );
                return;
            }
        };
        let mut added = added;
        let mut removed = removed;
        loop {
            tokio::select! {
                a = added.next() => {
                    if a.is_none() { return; }
                    let _ = event_tx.send(SupplicantEvent::BssCacheStale { ifindex });
                }
                r = removed.next() => {
                    if r.is_none() { return; }
                    let _ = event_tx.send(SupplicantEvent::BssCacheStale { ifindex });
                }
            }
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
    let frequency = bss
        .frequency()
        .await
        .map_err(zbus_err)
        .ok()
        .and_then(|v| bss_frequency_mhz(&v))
        .unwrap_or(0);
    let signal_dbm = bss.signal().await.map_err(zbus_err)? as i32;
    let age_s = bss.age().await.unwrap_or(0);
    let wpa = bss.wpa().await.unwrap_or_default();
    let rsn = bss.rsn().await.unwrap_or_default();
    let security = detect_security(&wpa, &rsn);
    // K2: parse the raw IE blob for HT/VHT/HE/EHT/WPS presence and
    // the RSN cap byte. Skipping the call (or a parse failure)
    // leaves capabilities at default — the security match still
    // works off `KeyMgmt`.
    let ies = bss.ies().await.unwrap_or_default();
    let capabilities = parse_bss_capabilities(&ies, &rsn);

    Ok(Some(crate::types::BssInfo {
        bssid,
        ssid,
        frequency,
        signal_dbm,
        capabilities,
        security,
        age_ms: (age_s as u64).saturating_mul(1000),
    }))
}

/// Walk the raw 802.11 IE chain in `ies` and the parsed RSN dict
/// to populate every `BssCapabilities` flag the rest of Nexus
/// cares about. K2 / DD-003 §4.2.
fn parse_bss_capabilities(
    ies: &[u8],
    rsn: &HashMap<String, OwnedValue>,
) -> crate::types::BssCapabilities {
    let mut caps = crate::types::BssCapabilities::default();
    let mut rsn_payload: Option<&[u8]> = None;
    let mut i = 0;
    while i + 2 <= ies.len() {
        let id = ies[i];
        let len = ies[i + 1] as usize;
        let body_start = i + 2;
        let body_end = body_start.saturating_add(len);
        if body_end > ies.len() {
            break;
        }
        let body = &ies[body_start..body_end];
        match id {
            45 => caps.ht = true,         // HT Capabilities
            191 => caps.vht = true,       // VHT Capabilities
            48 => rsn_payload = Some(body), // RSN
            // Vendor-Specific: WPS uses Microsoft OUI
            // 00:50:F2 + type 04. The first 4 body bytes are
            // OUI (3) + type (1).
            221 if matches!(body, [0x00, 0x50, 0xF2, 0x04, ..]) => {
                caps.wps = true;
            }
            255 => {
                // Element-extension: the first body byte is the
                // ext-tag.
                if let Some(ext_tag) = body.first() {
                    match *ext_tag {
                        35 => caps.he = true,  // HE Capabilities
                        108 => caps.eht = true, // EHT Capabilities
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        i = body_end;
    }

    // RSN gives us PMF (capable + required) directly via the
    // capabilities byte and FT via the AKM suite list. Walk the
    // RSN payload up to the cap bytes.
    if let Some(payload) = rsn_payload {
        if let Some((cap_lo, cap_hi)) = rsn_capability_bytes(payload) {
            // 802.11-2020 9.4.2.24.4: bit 6 = MFPC, bit 7 = MFPR
            // in the low byte of the RSN Capabilities field.
            caps.pmf_capable = (cap_lo & 0b0100_0000) != 0;
            caps.pmf_required = (cap_lo & 0b1000_0000) != 0;
            // The high byte carries PTKSA / GTKSA replay-counter
            // counts and SPP-A-MSDU bits; nothing we surface.
            let _ = cap_hi;
        }
    }
    // FT: any FT-* AKM in RSN.KeyMgmt → caps.ft. extract_key_mgmt
    // already lowercases.
    let mgmt = extract_key_mgmt(rsn);
    if mgmt.iter().any(|s| s.starts_with("ft-")) {
        caps.ft = true;
    }
    // Fallback PMF detection from the RSN dict's `MgmtGroup`
    // field — wpa_supplicant publishes a non-empty group cipher
    // name (e.g. "ccmp", "bip-cmac-128") iff PMF is supported.
    // Useful when the IEs blob isn't available (very rare on
    // current wpa_supplicant, but cheap belt-and-braces).
    if !caps.pmf_capable {
        if let Some(v) = rsn.get("MgmtGroup") {
            if let Ok(cloned) = v.try_clone() {
                if let Ok(s) = String::try_from(cloned) {
                    if !s.is_empty() {
                        caps.pmf_capable = true;
                    }
                }
            }
        }
    }
    caps
}

/// Walk the RSN element body to the (optional) two-byte
/// Capabilities field. Returns `(low, high)` if present, `None`
/// when the AP omits it (legacy WPA2-only deployments often do).
fn rsn_capability_bytes(rsn: &[u8]) -> Option<(u8, u8)> {
    // Layout per 802.11-2020 9.4.2.24:
    //   2 bytes Version
    //   4 bytes Group Data Cipher Suite
    //   2 bytes Pairwise Cipher Suite Count (n)
    //   4*n bytes Pairwise Cipher Suite List
    //   2 bytes AKM Suite Count (m)
    //   4*m bytes AKM Suite List
    //   2 bytes RSN Capabilities  ← what we want
    if rsn.len() < 8 {
        return None;
    }
    let mut idx = 6; // skip Version + Group cipher
    if rsn.len() < idx + 2 {
        return None;
    }
    let pairwise_count = u16::from_le_bytes([rsn[idx], rsn[idx + 1]]) as usize;
    idx += 2 + 4 * pairwise_count;
    if rsn.len() < idx + 2 {
        return None;
    }
    let akm_count = u16::from_le_bytes([rsn[idx], rsn[idx + 1]]) as usize;
    idx += 2 + 4 * akm_count;
    if rsn.len() < idx + 2 {
        return None;
    }
    Some((rsn[idx], rsn[idx + 1]))
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

/// Read the interface's current State (and the supporting
/// properties for `completed` / `disconnected`) and broadcast the
/// resulting [`SupplicantEvent::State`]. Shared by the signal path
/// and the reconciliation tick in [`spawn_state_watcher`].
async fn evaluate_and_emit(
    connection: &Connection,
    iface: &WpaInterfaceProxy<'_>,
    ifindex: u32,
    event_tx: &broadcast::Sender<SupplicantEvent>,
) {
    if let Some(state) = resolve_state(connection, iface).await {
        let _ = event_tx.send(SupplicantEvent::State { ifindex, state });
    }
}

/// Read `State` from the supplicant and, when needed, the
/// supporting properties (`CurrentBSS` for `completed`,
/// `DisconnectReason` for `disconnected`) to build a fully-populated
/// [`super::SupplicantState`]. Returns `None` when the read itself
/// fails or when `State` is a value the pure translator can't
/// classify yet (e.g. a completed→associating flicker where
/// `CurrentBSS == "/"`).
async fn resolve_state(
    connection: &Connection,
    iface: &WpaInterfaceProxy<'_>,
) -> Option<super::SupplicantState> {
    let state_str = match iface.state().await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(error = %e, "wpa_supplicant: State read failed");
            return None;
        }
    };
    if state_str == "completed" {
        return resolve_completed(connection, iface).await;
    }
    if state_str == "disconnected" {
        // Read `DisconnectReason` so the D-Bus `StateChanged`
        // payload carries the 802.11 reason code rather than a
        // stock `Unspecified` (DD-003 §9.6). Property read errors
        // fall through to `Unspecified` — losing fidelity on an
        // already-degenerate path is better than dropping the
        // Disconnected event entirely.
        let code = iface.disconnect_reason().await.unwrap_or(0);
        return Some(super::SupplicantState::Disconnected {
            reason: translate_disconnect_reason(code),
        });
    }
    translate_wpa_state(&state_str)
}

// ---- error helpers -------------------------------------------------------

fn zbus_err(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> WifiError {
    WifiError::Supplicant {
        backend: "wpa_supplicant",
        source: e.into(),
    }
}

/// Convert the `OwnedValue` returned by `SignalPoll` into the
/// `a{sv}` dict the rest of `signal_info` expects. Handles both
/// shapes seen in the wild: the documented `a{sv}` (the proxy
/// reads it as an `OwnedValue` whose inner Value is a dict) and
/// the variant-wrapped `v(a{sv})` some wpa_supplicant builds emit
/// instead. See the `signal_poll` proxy comment for the upstream
/// quirk this works around.
fn unwrap_signal_poll_dict(raw: OwnedValue) -> std::result::Result<HashMap<String, OwnedValue>, String> {
    use zbus::zvariant::Value;
    // Direct `a{sv}`: try the cheap conversion first.
    if let Ok(d) = HashMap::<String, OwnedValue>::try_from(raw.clone()) {
        return Ok(d);
    }
    // `v(a{sv})`: peel one layer of variant wrapping. `OwnedValue`
    // derefs to `Value`; for a body of signature `v` the inner
    // value is itself a `Value::Value(Box<Value>)`.
    let inner: &Value<'_> = &raw;
    if let Value::Value(boxed) = inner {
        let inner_owned = OwnedValue::try_from(
            boxed
                .try_clone()
                .map_err(|e| format!("variant clone failed: {e}"))?,
        )
        .map_err(|e| format!("inner variant convert: {e}"))?;
        return HashMap::<String, OwnedValue>::try_from(inner_owned)
            .map_err(|e| format!("inner dict convert: {e}"));
    }
    Err(format!(
        "unexpected SignalPoll body shape: {:?}",
        inner.value_signature()
    ))
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

/// Decode `BSS.Frequency` regardless of whether the supplicant
/// publishes it as `q` (uint16, current behaviour) or `u` (uint32,
/// the shape needed once 6 GHz channels exceed 65535 MHz). Both
/// widen cleanly to `u32`. K7.
fn bss_frequency_mhz(value: &OwnedValue) -> Option<u32> {
    if let Ok(n) = u32::try_from(value) {
        return Some(n);
    }
    if let Ok(n) = u16::try_from(value) {
        return Some(n as u32);
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
/// Pure translator for the states that don't need an extra property
/// read. `completed` needs `CurrentBSS`; `disconnected` needs
/// `DisconnectReason`. Both are resolved by [`resolve_state`].
pub fn translate_wpa_state(state_str: &str) -> Option<super::SupplicantState> {
    use super::{DisconnectHint, SupplicantState};
    match state_str {
        "inactive" => Some(SupplicantState::Disconnected {
            reason: DisconnectHint::LocalRequest,
        }),
        "scanning" => Some(SupplicantState::Scanning),
        "authenticating" => Some(SupplicantState::Authenticating),
        "associating" => Some(SupplicantState::Associating),
        "associated" => Some(SupplicantState::Associated),
        "4way_handshake" | "group_handshake" => Some(SupplicantState::FourWayHandshake),
        // `completed` and `disconnected` are resolved by
        // `resolve_state` with a supporting property read.
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
    let frequency = bss_frequency_mhz(&bss.frequency().await.ok()?)?;
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
        // reachable but no longer willing to talk.
        1 => DisconnectHint::ApInitiated,
        // "Previous authentication no longer valid" (reason 2) and
        // "Invalid IE" (reason 13) — the pre-4way auth exchange
        // itself went sideways. Distinct from the 4-way handshake
        // timeout path below.
        2 | 13 => DisconnectHint::AuthFailure,
        4 => DisconnectHint::Inactivity,
        6 | 7 => DisconnectHint::ProtocolError,
        15 => DisconnectHint::HandshakeTimeout,
        17 => DisconnectHint::AssociationTimeout,
        // 802.1X EAP failure has its own bucket so Enterprise auth
        // failures don't look like stale-key deauths.
        23 => DisconnectHint::EapFailure,
        _ => DisconnectHint::Unspecified,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- unwrap_signal_poll_dict ----------------------------------------

    fn dict_with_rssi(rssi: i32) -> HashMap<String, OwnedValue> {
        use zbus::zvariant::Value;
        let mut d = HashMap::new();
        d.insert(
            "rssi".to_owned(),
            OwnedValue::try_from(Value::new(rssi)).unwrap(),
        );
        d
    }

    #[test]
    fn unwrap_signal_poll_handles_direct_dict() {
        // Documented `a{sv}` shape: the proxy reads the body as
        // `OwnedValue` whose inner Value is a Dict.
        use zbus::zvariant::Value;
        let dict = dict_with_rssi(-42);
        let raw = OwnedValue::try_from(Value::new(dict.clone())).unwrap();
        let out = unwrap_signal_poll_dict(raw).expect("direct dict decodes");
        assert_eq!(signal_i32(&out, "rssi"), Some(-42));
    }

    #[test]
    fn unwrap_signal_poll_peels_variant_wrapper() {
        // Quirk shape `v(a{sv})`: the Value is a variant whose inner
        // Value is the dict.
        use zbus::zvariant::Value;
        let dict = dict_with_rssi(-55);
        let inner = Value::new(dict);
        let wrapped = Value::Value(Box::new(inner));
        let raw = OwnedValue::try_from(wrapped).unwrap();
        let out = unwrap_signal_poll_dict(raw).expect("variant-wrapped dict decodes");
        assert_eq!(signal_i32(&out, "rssi"), Some(-55));
    }

    #[test]
    fn unwrap_signal_poll_rejects_unexpected_shape() {
        // Anything that's neither a dict nor a variant-wrapped dict
        // should error rather than silently return an empty map.
        use zbus::zvariant::Value;
        let raw = OwnedValue::try_from(Value::new(42i32)).unwrap();
        assert!(unwrap_signal_poll_dict(raw).is_err());
    }

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

    // ---- K2 BssCapabilities parser --------------------------------------

    /// Build a single-IE blob: id, length, payload.
    fn ie(id: u8, body: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(2 + body.len());
        v.push(id);
        v.push(body.len() as u8);
        v.extend_from_slice(body);
        v
    }

    /// Glue several IEs together.
    fn ies(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.iter().flatten().copied().collect()
    }

    #[test]
    fn caps_parser_detects_ht_vht_he_eht_wps() {
        let ht = ie(45, &[0; 26]);
        let vht = ie(191, &[0; 12]);
        let he = ie(255, &[35]); // ext-tag 35 = HE
        let eht = ie(255, &[108]); // ext-tag 108 = EHT
        let wps = ie(221, &[0x00, 0x50, 0xF2, 0x04]);
        let blob = ies(&[ht, vht, he, eht, wps]);
        let caps = parse_bss_capabilities(&blob, &HashMap::new());
        assert!(caps.ht);
        assert!(caps.vht);
        assert!(caps.he);
        assert!(caps.eht);
        assert!(caps.wps);
    }

    #[test]
    fn caps_parser_skips_truncated_tail_without_panic() {
        // Last IE claims length 5 but only 2 bytes follow — must
        // bail cleanly.
        let mut blob = ie(45, &[0; 26]);
        blob.extend_from_slice(&[191, 5, 0, 0]);
        let caps = parse_bss_capabilities(&blob, &HashMap::new());
        assert!(caps.ht);
        assert!(!caps.vht); // truncated, ignored
    }

    #[test]
    fn rsn_parser_extracts_pmf_required_bit() {
        // RSN element body for WPA2-PSK with PMF required + capable:
        //   version 0x0001, group ccmp, pairwise count 1 + ccmp,
        //   akm count 1 + psk, capabilities 0xC0 0x00.
        let rsn_body = vec![
            0x01, 0x00, // version
            0x00, 0x0F, 0xAC, 0x04, // group cipher CCMP
            0x01, 0x00, // pairwise count 1
            0x00, 0x0F, 0xAC, 0x04, // pairwise CCMP
            0x01, 0x00, // akm count 1
            0x00, 0x0F, 0xAC, 0x02, // PSK
            0xC0, 0x00, // RSN caps: MFPR + MFPC
        ];
        let blob = ie(48, &rsn_body);
        let caps = parse_bss_capabilities(&blob, &HashMap::new());
        assert!(caps.pmf_required);
        assert!(caps.pmf_capable);
    }

    #[test]
    fn rsn_parser_handles_missing_capabilities_byte() {
        // Legacy RSN element ending right after AKM list — no
        // capabilities. Should stay default (false).
        let rsn_body = vec![
            0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00, 0x00, 0x0F, 0xAC, 0x04, 0x01, 0x00,
            0x00, 0x0F, 0xAC, 0x02,
        ];
        let blob = ie(48, &rsn_body);
        let caps = parse_bss_capabilities(&blob, &HashMap::new());
        assert!(!caps.pmf_required);
        assert!(!caps.pmf_capable);
    }

    #[test]
    fn caps_parser_picks_up_ft_from_rsn_keymgmt() {
        let rsn = keymgmt_dict(&["ft-psk", "wpa-psk"]);
        let caps = parse_bss_capabilities(&[], &rsn);
        assert!(caps.ft);
    }

    #[test]
    fn caps_parser_falls_back_to_mgmtgroup_for_pmf_capable() {
        // No RSN IE in the blob; rely on the `MgmtGroup` fallback.
        let mut rsn = HashMap::new();
        rsn.insert(
            "MgmtGroup".to_owned(),
            Value::from("bip-cmac-128").try_into().unwrap(),
        );
        let caps = parse_bss_capabilities(&[], &rsn);
        assert!(caps.pmf_capable);
        assert!(!caps.pmf_required);
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
        // K1: `associated` is its own variant — the BSS is chosen
        // but the 4-way hasn't begun. Backend folds both into
        // `WifiState::Connecting` per DD-003 §9.5.
        assert!(matches!(
            translate_wpa_state("associated"),
            Some(SupplicantState::Associated)
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
        // `completed` and `disconnected` both resolve via a
        // property read (IO-bound); the pure translator returns
        // None for them.
        assert!(translate_wpa_state("disconnected").is_none());
        assert!(translate_wpa_state("completed").is_none());
        assert!(translate_wpa_state("unknown_future").is_none());
    }

    #[test]
    fn disconnect_reasons_cover_dd003_section_9_6() {
        use super::super::DisconnectHint;
        assert_eq!(
            translate_disconnect_reason(-3),
            DisconnectHint::LocalRequest
        );
        assert_eq!(translate_disconnect_reason(3), DisconnectHint::LocalRequest);
        assert_eq!(translate_disconnect_reason(1), DisconnectHint::ApInitiated);
        assert_eq!(translate_disconnect_reason(2), DisconnectHint::AuthFailure);
        assert_eq!(translate_disconnect_reason(13), DisconnectHint::AuthFailure);
        assert_eq!(translate_disconnect_reason(4), DisconnectHint::Inactivity);
        assert_eq!(translate_disconnect_reason(6), DisconnectHint::ProtocolError);
        assert_eq!(translate_disconnect_reason(7), DisconnectHint::ProtocolError);
        assert_eq!(
            translate_disconnect_reason(15),
            DisconnectHint::HandshakeTimeout
        );
        assert_eq!(
            translate_disconnect_reason(17),
            DisconnectHint::AssociationTimeout
        );
        assert_eq!(translate_disconnect_reason(23), DisconnectHint::EapFailure);
        assert_eq!(
            translate_disconnect_reason(999),
            DisconnectHint::Unspecified
        );
        assert_eq!(translate_disconnect_reason(0), DisconnectHint::Unspecified);
    }
}
