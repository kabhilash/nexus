//! `fi.nexus.Wifi` — DD-006 §6.3.
//!
//! Read-only properties + mutating methods. Every mutating method
//! goes through `services.auth.check(action, sender)` before
//! dispatching to `services.ops.wifi_*`.

use std::collections::HashMap;
use std::sync::Arc;

use nexus_core::{MacAddr, Ssid};
use ulid::Ulid;
use zbus::fdo;
use zbus::message::Header;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

use crate::authz::{AuthDecision, actions};
use crate::backend_ops::{RoamingMode, ScanParams};
use crate::errors::DbusError;
use crate::manager::lookup_bool;
use crate::rate_limit::OpClass;
use crate::services::Services;
use crate::state::{InterfaceKindData, WifiConnectedBss};

/// D-Bus type for `ConnectedBss`: `(sayayuis)`.
pub type ConnectedBssTuple = (String, Vec<u8>, Vec<u8>, u32, i32, String);

pub struct WifiIface {
    pub services: Arc<Services>,
    pub ifname: String,
}

impl WifiIface {
    pub fn new(services: Arc<Services>, ifname: impl Into<String>) -> Self {
        Self {
            services,
            ifname: ifname.into(),
        }
    }

    async fn with_cache<R>(
        &self,
        default: R,
        f: impl FnOnce(&crate::state::WifiInterfaceState) -> R,
    ) -> R {
        let guard = self.services.state.read().await;
        match guard.interfaces.get(&self.ifname).map(|e| &e.kind_data) {
            Some(InterfaceKindData::Wifi(c)) => f(c),
            _ => default,
        }
    }

    async fn require_auth(&self, hdr: &Header<'_>, action: &str) -> fdo::Result<()> {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        match self.services.auth.check(action, &sender).await {
            AuthDecision::Authorized => Ok(()),
            AuthDecision::Denied => Err(fdo::Error::from(DbusError::AuthFailed(format!(
                "policykit denied '{action}' for sender '{sender}'"
            )))),
        }
    }

    /// Rate-limit check. Returns `Err(RateLimited)` with a
    /// `retry_after_ms` hint when the per-(sender, op-class)
    /// window is full.
    fn check_rate(&self, hdr: &Header<'_>, op: OpClass) -> fdo::Result<()> {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        match self.services.rate_limiter.check(&sender, op) {
            Ok(()) => Ok(()),
            Err(retry) => Err(fdo::Error::from(DbusError::RateLimited {
                op: op.as_str(),
                retry_after_ms: retry.as_millis() as u64,
            })),
        }
    }

    /// `FeatureDisabled` gate for Wi-Fi mutating methods. Property
    /// reads bypass this so clients can still see
    /// `Powered = false` etc. on a disabled backend.
    fn check_feature(&self) -> fdo::Result<()> {
        self.services
            .enabled
            .require(crate::services::Feature::Wifi)
            .map_err(fdo::Error::from)
    }
}

fn tuple_from_bss(b: &WifiConnectedBss) -> ConnectedBssTuple {
    (
        String::from_utf8_lossy(&b.ssid).into_owned(),
        b.ssid.clone(),
        b.bssid.0.to_vec(),
        b.frequency,
        b.signal_dbm,
        b.security.clone(),
    )
}

fn empty_bss_tuple() -> ConnectedBssTuple {
    (String::new(), Vec::new(), Vec::new(), 0, 0, String::new())
}

#[zbus::interface(name = "fi.nexus.Wifi")]
impl WifiIface {
    // ---- Properties ----

    #[zbus(property, name = "State")]
    async fn state(&self) -> String {
        self.with_cache(String::new(), |c| c.state.clone()).await
    }

    #[zbus(property, name = "ConnectedBss")]
    async fn connected_bss(&self) -> ConnectedBssTuple {
        self.with_cache(empty_bss_tuple(), |c| {
            c.connected_bss
                .as_ref()
                .map(tuple_from_bss)
                .unwrap_or_else(empty_bss_tuple)
        })
        .await
    }

    #[zbus(property, name = "SignalDbm")]
    async fn signal_dbm(&self) -> i32 {
        self.with_cache(0, |c| c.signal_dbm).await
    }

    #[zbus(property, name = "Frequency")]
    async fn frequency(&self) -> u32 {
        self.with_cache(0, |c| c.frequency).await
    }

    #[zbus(property, name = "ScanResults")]
    async fn scan_results(&self) -> Vec<OwnedObjectPath> {
        self.with_cache(Vec::new(), |c| c.scan_results.clone())
            .await
            .into_iter()
            .filter_map(|p| ObjectPath::try_from(p).ok())
            .map(OwnedObjectPath::from)
            .collect()
    }

    #[zbus(property, name = "Supplicant")]
    async fn supplicant(&self) -> String {
        self.with_cache(String::new(), |c| c.supplicant.clone())
            .await
    }

    #[zbus(property, name = "RoamingMode")]
    async fn roaming_mode(&self) -> String {
        self.with_cache(String::new(), |c| c.roaming_mode.clone())
            .await
    }

    #[zbus(property, name = "Powered")]
    async fn powered(&self) -> bool {
        // When the Wi-Fi feature is disabled, the daemon isn't
        // managing the interface; report `false` regardless of
        // kernel state so clients see a consistent "we don't
        // treat this interface as powered" signal (matches the
        // `check_feature` docstring above).
        if !self
            .services
            .enabled
            .is_enabled(crate::services::Feature::Wifi)
        {
            return false;
        }
        self.with_cache(false, |c| c.powered).await
    }

    // ---- Mutating methods (DD-006 §6.3 + §10) ----

    /// `Scan(params: a{sv}) -> ()` — `fi.nexus.scan`.
    async fn scan(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        params: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.check_feature()?;
        self.check_rate(&hdr, OpClass::Scan)?;
        self.require_auth(&hdr, actions::SCAN).await?;
        let parsed = parse_scan_params(&params)
            .map_err(|e| fdo::Error::from(DbusError::InvalidArgument(e)))?;
        self.services
            .ops
            .wifi_scan(&self.ifname, parsed)
            .await
            .map_err(fdo::Error::from)
    }

    /// `Connect(profile: o) -> ()` — `fi.nexus.connect`.
    async fn connect(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        profile: OwnedObjectPath,
    ) -> fdo::Result<()> {
        self.check_feature()?;
        self.check_rate(&hdr, OpClass::ConnectDisconnect)?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let path_str = profile.as_str();
        let id_str = path_str
            .strip_prefix("/fi/nexus1/profile/wifi/")
            .ok_or_else(|| {
                fdo::Error::from(DbusError::InvalidArgument(format!(
                    "expected wifi profile path, got '{path_str}'"
                )))
            })?;
        let id = Ulid::from_string(id_str)
            .map_err(|e| fdo::Error::from(DbusError::InvalidArgument(format!("bad ulid: {e}"))))?;
        // Verify the profile actually exists locally — sending
        // `Connect` for an unknown profile is `NotFound`.
        {
            let guard = self.services.state.read().await;
            if !guard.wifi_profiles.contains_key(&id.to_string()) {
                return Err(fdo::Error::from(DbusError::NotFound(format!(
                    "wifi profile {id}"
                ))));
            }
        }
        self.services
            .ops
            .wifi_connect(&self.ifname, id)
            .await
            .map_err(fdo::Error::from)
    }

    /// `Disconnect(params: a{sv}) -> ()` — `fi.nexus.connect`.
    /// DD-006 §6.3. Recognised params:
    ///   - `pause_auto_connect` (b, default false): if true, the
    ///     active profile is added to the backend's runtime
    ///     paused-set so the automatic selector skips it until the
    ///     operator either explicitly `Connect`s, edits the profile,
    ///     or restarts the daemon. The on-disk profile's
    ///     `auto_connect` field is *not* modified.
    /// Unknown keys are ignored — clients can probe for
    /// future-added knobs without server-side validation churn.
    async fn disconnect(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        params: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.check_feature()?;
        self.check_rate(&hdr, OpClass::ConnectDisconnect)?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let pause_auto_connect = lookup_bool(&params, "pause_auto_connect")
            .map_err(|e| fdo::Error::from(DbusError::InvalidArgument(e)))?
            .unwrap_or(false);
        self.services
            .ops
            .wifi_disconnect(&self.ifname, pause_auto_connect)
            .await
            .map_err(fdo::Error::from)
    }

    /// `Roam(bssid: ay) -> ()` — `fi.nexus.connect`. Only valid in
    /// `RoamingMode == "nexus"`.
    async fn roam(&self, #[zbus(header)] hdr: Header<'_>, bssid: Vec<u8>) -> fdo::Result<()> {
        self.check_feature()?;
        self.check_rate(&hdr, OpClass::ConnectDisconnect)?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        if bssid.len() != 6 {
            return Err(fdo::Error::from(DbusError::InvalidArgument(format!(
                "BSSID must be 6 bytes, got {}",
                bssid.len()
            ))));
        }
        let mut arr = [0u8; 6];
        arr.copy_from_slice(&bssid);
        let mac = MacAddr(arr);
        self.services
            .ops
            .wifi_roam(&self.ifname, mac)
            .await
            .map_err(fdo::Error::from)
    }

    /// `ProvideCredential(network: o, field: s, value: s) -> ()` —
    /// DD-006 §6.3 / DD-003 §9.2. Echoes the operator's reply back
    /// into wpa_supplicant's `NetworkReply`.
    async fn provide_credential(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        network: OwnedObjectPath,
        field: String,
        value: String,
    ) -> fdo::Result<()> {
        self.check_feature()?;
        // Same op-class as Connect — credential replies are
        // operator-driven and infrequent. Reusing the bucket keeps
        // an interactive flow (Connect → NetworkRequest → reply →
        // Connect) under a single rate-limit budget.
        self.check_rate(&hdr, OpClass::ConnectDisconnect)?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        if field.is_empty() {
            return Err(fdo::Error::from(DbusError::InvalidArgument(
                "credential field must be non-empty".into(),
            )));
        }
        self.services
            .ops
            .wifi_provide_credential(&self.ifname, network.as_str(), &field, &value)
            .await
            .map_err(fdo::Error::from)
    }

    /// `Powered` writeable property — `fi.nexus.set_power`.
    /// zbus threads the request `Header` through property setters
    /// as `#[zbus(header)]`, so the PolicyKit check gets the real
    /// sender's unique bus name (matching every mutating-method
    /// path on this interface). The setter returns `zbus::Result<()>`
    /// per zbus's property contract — we map `DbusError::AuthFailed`
    /// onto the closest `zbus::Error` variant.
    #[zbus(property)]
    async fn set_powered(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
        on: bool,
    ) -> zbus::Result<()> {
        if !self
            .services
            .enabled
            .is_enabled(crate::services::Feature::Wifi)
        {
            return Err(zbus::Error::from(zbus::fdo::Error::from(
                DbusError::FeatureDisabled("wifi".to_owned()),
            )));
        }
        let sender = hdr
            .as_ref()
            .and_then(|h| h.sender().map(|s| s.to_string()))
            .unwrap_or_default();
        if !self
            .services
            .auth
            .check(actions::SET_POWER, &sender)
            .await
            .is_authorized()
        {
            return Err(zbus::Error::from(zbus::fdo::Error::AuthFailed(
                "Powered set denied".into(),
            )));
        }
        self.services
            .ops
            .wifi_set_powered(&self.ifname, on)
            .await
            .map_err(|e| zbus::Error::from(zbus::fdo::Error::from(e)))
    }

    /// `RoamingMode` writeable property — `fi.nexus.connect`.
    #[zbus(property)]
    async fn set_roaming_mode(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
        mode: String,
    ) -> zbus::Result<()> {
        if !self
            .services
            .enabled
            .is_enabled(crate::services::Feature::Wifi)
        {
            return Err(zbus::Error::from(zbus::fdo::Error::from(
                DbusError::FeatureDisabled("wifi".to_owned()),
            )));
        }
        let parsed = RoamingMode::parse(&mode).ok_or_else(|| {
            zbus::Error::from(zbus::fdo::Error::InvalidArgs(format!(
                "unknown roaming mode '{mode}'"
            )))
        })?;
        let sender = hdr
            .as_ref()
            .and_then(|h| h.sender().map(|s| s.to_string()))
            .unwrap_or_default();
        if !self
            .services
            .auth
            .check(actions::CONNECT, &sender)
            .await
            .is_authorized()
        {
            return Err(zbus::Error::from(zbus::fdo::Error::AuthFailed(
                "RoamingMode set denied".into(),
            )));
        }
        self.services
            .ops
            .wifi_set_roaming_mode(&self.ifname, parsed)
            .await
            .map_err(|e| zbus::Error::from(zbus::fdo::Error::from(e)))
    }
}

fn parse_scan_params(
    dict: &HashMap<String, OwnedValue>,
) -> std::result::Result<ScanParams, String> {
    let active = lookup_bool(dict, "active")?.unwrap_or(true);
    let allow_roam = lookup_bool(dict, "allow_roam")?.unwrap_or(false);
    let mut ssids = Vec::new();
    if let Some(v) = dict.get("ssids") {
        if let Ok(arr) = <&zbus::zvariant::Array>::try_from(v) {
            for item in arr.iter() {
                if let Ok(inner) = <&zbus::zvariant::Array>::try_from(item) {
                    let mut bytes = Vec::with_capacity(inner.len());
                    for byte in inner.iter() {
                        if let Ok(b) = u8::try_from(byte) {
                            bytes.push(b);
                        }
                    }
                    ssids.push(bytes);
                }
            }
        }
    }
    let mut frequencies = Vec::new();
    if let Some(v) = dict.get("frequencies") {
        if let Ok(arr) = <&zbus::zvariant::Array>::try_from(v) {
            for item in arr.iter() {
                if let Ok(f) = u32::try_from(item) {
                    frequencies.push(f);
                }
            }
        }
    }
    Ok(ScanParams {
        active,
        ssids,
        frequencies,
        allow_roam,
    })
}

// Silence unused-import warnings for types referenced only in
// option-typed parsing branches.
#[allow(dead_code)]
fn _touch(_s: Ssid) {}
