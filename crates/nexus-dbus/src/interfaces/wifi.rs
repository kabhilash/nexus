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
use zbus::object_server::SignalEmitter;
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

    /// `Connect(profile: o) -> (job_id: s)` — `fi.nexus.connect`.
    /// DD-006 §6.3. Returns a ULID `job_id` that correlates the
    /// subsequent `ConnectComplete` signal. The terminal edge fires
    /// from the service event loop on the next Wi-Fi state
    /// transition (Connected → success; Disconnected{reason} →
    /// failure with mapped `reason`); an explicit `Disconnect`
    /// before that resolves the job with `reason="cancelled"`.
    async fn connect(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        profile: OwnedObjectPath,
    ) -> fdo::Result<String> {
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
        let job_id = Ulid::new().to_string();
        self.services.wifi_jobs.register_connect(&self.ifname, &job_id);
        if let Err(e) = self.services.ops.wifi_connect(&self.ifname, id).await {
            // Backend rejected the call before any state-machine
            // work — clear the registration so a stale job_id
            // doesn't sit waiting for a state transition that will
            // never come.
            self.services.wifi_jobs.take_connect(&self.ifname);
            return Err(fdo::Error::from(e));
        }
        Ok(job_id)
    }

    /// `Disconnect(params: a{sv}) -> (job_id: s)` — `fi.nexus.connect`.
    /// DD-006 §6.3. Recognised params:
    ///   - `pause_auto_connect` (b, default false): if true, the
    ///     active profile is added to the backend's runtime
    ///     paused-set so the automatic selector skips it until the
    ///     operator either explicitly `Connect`s, edits the profile,
    ///     or restarts the daemon. The on-disk profile's
    ///     `auto_connect` field is *not* modified.
    /// Unknown keys are ignored — clients can probe for
    /// future-added knobs without server-side validation churn.
    ///
    /// Returns a ULID `job_id` for the corresponding
    /// `DisconnectComplete` signal. Cancels any in-flight
    /// `Connect` job by emitting `ConnectComplete(success=false,
    /// reason="cancelled")` for it before driving the teardown.
    async fn disconnect(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        params: HashMap<String, OwnedValue>,
    ) -> fdo::Result<String> {
        self.check_feature()?;
        self.check_rate(&hdr, OpClass::ConnectDisconnect)?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let pause_auto_connect = lookup_bool(&params, "pause_auto_connect")
            .map_err(|e| fdo::Error::from(DbusError::InvalidArgument(e)))?
            .unwrap_or(false);
        let job_id = Ulid::new().to_string();
        self.services
            .wifi_jobs
            .register_disconnect(&self.ifname, &job_id);
        let services = Arc::clone(&self.services);
        let ifname = self.ifname.clone();
        let emitter_owned = emitter.to_owned();
        let job_for_task = job_id.clone();
        tokio::spawn(async move {
            // Cancel any in-flight Connect: emit ConnectComplete
            // with reason="cancelled" so the operator sees a
            // terminal edge for the prior Connect attempt before
            // the disconnect's terminal edge.
            if let Some(connect_job) = services.wifi_jobs.take_connect(&ifname) {
                let _ = WifiIface::connect_complete(
                    &emitter_owned,
                    &connect_job,
                    false,
                    "cancelled",
                )
                .await;
            }
            let result = services.ops.wifi_disconnect(&ifname, pause_auto_connect).await;
            services.wifi_jobs.take_disconnect(&ifname);
            let (success, reason) = match &result {
                Ok(()) => (true, ""),
                Err(_) => (false, "other"),
            };
            let _ = WifiIface::disconnect_complete(
                &emitter_owned,
                &job_for_task,
                success,
                reason,
            )
            .await;
        });
        Ok(job_id)
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

    // ---- Signals (DD-006 §9) ----

    // `StateChanged(new_state: s, details: a{sv})` is declared on
    // `fi.nexus.Wifi` for clients whose typed proxy stack
    // resolves per-technology signals more reliably than the
    // common `fi.nexus.Interface` channel. The signal is emitted
    // raw from the service event loop alongside
    // `Interface.StateChanged` (see `emit_wifi_state_changed`); a
    // typed `#[zbus(signal)]` helper here would clash with the
    // namesake on `InterfaceIface`, both of which are registered
    // on the same object path.

    /// `ConnectComplete(job_id: s, success: b, reason: s)`.
    ///
    /// Terminal signal for an operator-initiated `Connect`. See
    /// DD-006 §9 for the documented `reason` value set.
    #[zbus(signal)]
    pub async fn connect_complete(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        success: bool,
        reason: &str,
    ) -> zbus::Result<()>;

    /// `DisconnectComplete(job_id: s, success: b, reason: s)`.
    ///
    /// Terminal signal for an operator-initiated `Disconnect`.
    /// `reason` is `""` on success, `"other"` on backend failure
    /// mid-teardown.
    #[zbus(signal)]
    pub async fn disconnect_complete(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        success: bool,
        reason: &str,
    ) -> zbus::Result<()>;

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

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

    fn owned(v: Value<'_>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    // --- tuple_from_bss / empty_bss_tuple ----------------------------

    #[test]
    fn tuple_from_bss_packs_every_field_in_order() {
        let bss = WifiConnectedBss {
            ssid: b"nexus-net".to_vec(),
            bssid: MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
            frequency: 5180,
            signal_dbm: -42,
            security: "wpa2_personal".into(),
        };
        let (ssid_str, ssid_bytes, bssid_bytes, frequency, signal_dbm, security) =
            tuple_from_bss(&bss);
        assert_eq!(ssid_str, "nexus-net");
        assert_eq!(ssid_bytes, b"nexus-net".to_vec());
        assert_eq!(bssid_bytes, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(frequency, 5180);
        assert_eq!(signal_dbm, -42);
        assert_eq!(security, "wpa2_personal");
    }

    #[test]
    fn tuple_from_bss_preserves_non_utf8_ssid_bytes_alongside_lossy_string() {
        // SSIDs aren't required to be UTF-8 — the lossy string must
        // not corrupt the raw byte field, which is what consumers
        // doing exact comparisons rely on.
        let bss = WifiConnectedBss {
            ssid: vec![0xFF, 0xFE, 0x00, 0x41],
            bssid: MacAddr([0; 6]),
            frequency: 0,
            signal_dbm: 0,
            security: String::new(),
        };
        let (s, bytes, ..) = tuple_from_bss(&bss);
        assert_eq!(bytes, vec![0xFF, 0xFE, 0x00, 0x41]);
        assert!(
            s.contains('\u{FFFD}'),
            "lossy decode should mark invalid bytes, got {s:?}"
        );
    }

    #[test]
    fn empty_bss_tuple_has_zero_default_for_every_slot() {
        let (s, ssid, bssid, freq, dbm, sec) = empty_bss_tuple();
        assert!(s.is_empty());
        assert!(ssid.is_empty());
        assert!(bssid.is_empty());
        assert_eq!(freq, 0);
        assert_eq!(dbm, 0);
        assert!(sec.is_empty());
    }

    // --- parse_scan_params -------------------------------------------

    #[test]
    fn parse_scan_params_empty_dict_uses_documented_defaults() {
        let p = parse_scan_params(&HashMap::new()).unwrap();
        // active defaults to true; allow_roam to false. Both are
        // operator-visible defaults documented in DD-006 §6.3.
        assert!(p.active);
        assert!(!p.allow_roam);
        assert!(p.ssids.is_empty());
        assert!(p.frequencies.is_empty());
    }

    #[test]
    fn parse_scan_params_honors_explicit_active_and_allow_roam() {
        let mut d = HashMap::new();
        d.insert("active".into(), owned(Value::new(false)));
        d.insert("allow_roam".into(), owned(Value::new(true)));
        let p = parse_scan_params(&d).unwrap();
        assert!(!p.active);
        assert!(p.allow_roam);
    }

    #[test]
    fn parse_scan_params_extracts_single_ssid() {
        let mut d = HashMap::new();
        let ssids: Vec<Vec<u8>> = vec![b"nexus-net".to_vec()];
        d.insert("ssids".into(), owned(Value::new(ssids)));
        let p = parse_scan_params(&d).unwrap();
        assert_eq!(p.ssids, vec![b"nexus-net".to_vec()]);
    }

    #[test]
    fn parse_scan_params_extracts_multiple_ssids_in_order() {
        let mut d = HashMap::new();
        let ssids: Vec<Vec<u8>> =
            vec![b"a".to_vec(), b"bb".to_vec(), vec![0xFF, 0xFE, 0x00, 0x41]];
        d.insert("ssids".into(), owned(Value::new(ssids.clone())));
        let p = parse_scan_params(&d).unwrap();
        assert_eq!(p.ssids, ssids);
    }

    #[test]
    fn parse_scan_params_silently_drops_non_array_ssids() {
        // A misuse — caller passed a string under the "ssids" key.
        // The parser silently treats it as no SSIDs (DD-006 §6.3
        // "Unknown keys are ignored" tolerance extends to malformed
        // values — clients can't trip a hard error this way).
        let mut d = HashMap::new();
        d.insert("ssids".into(), owned(Value::new("not-an-array".to_owned())));
        let p = parse_scan_params(&d).unwrap();
        assert!(p.ssids.is_empty());
    }

    #[test]
    fn parse_scan_params_extracts_frequencies() {
        let mut d = HashMap::new();
        d.insert(
            "frequencies".into(),
            owned(Value::new(vec![2412u32, 5180u32, 5825u32])),
        );
        let p = parse_scan_params(&d).unwrap();
        assert_eq!(p.frequencies, vec![2412, 5180, 5825]);
    }

    #[test]
    fn parse_scan_params_silently_drops_non_array_frequencies() {
        let mut d = HashMap::new();
        d.insert("frequencies".into(), owned(Value::new(true)));
        let p = parse_scan_params(&d).unwrap();
        assert!(p.frequencies.is_empty());
    }

    #[test]
    fn parse_scan_params_rejects_bad_active_type() {
        // active=42 (i32) is not a bool — `lookup_bool` returns Err,
        // which `parse_scan_params` propagates as a String. A
        // mistyped argument should fail loudly.
        let mut d = HashMap::new();
        d.insert("active".into(), owned(Value::new(42i32)));
        let err = parse_scan_params(&d).unwrap_err();
        assert!(err.to_lowercase().contains("active"), "got {err}");
    }

    #[test]
    fn parse_scan_params_ignores_unknown_keys() {
        let mut d = HashMap::new();
        d.insert("active".into(), owned(Value::new(true)));
        d.insert("future_knob".into(), owned(Value::new("ignored".to_owned())));
        // No error; the future_knob is silently skipped, matching
        // the docstring's forward-compat guarantee.
        let p = parse_scan_params(&d).unwrap();
        assert!(p.active);
    }
}
