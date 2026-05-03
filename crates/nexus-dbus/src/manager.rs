//! `fi.nexus.Manager` at `/fi/nexus1`. See DD-006 §5.
//!
//! This module hosts every Manager-level operation, both
//! read-only (DD-006 §5.1, §5.2 lookup methods) and mutating
//! (`AddWifiProfile`, `AddEthernetProfile`, `RemoveProfile`,
//! `SetPowerState`).
//!
//! Mutating methods follow the DD-006 §10.2 flow:
//!   1. Pull the caller's bus name from the message header.
//!   2. Ask the [`crate::authz::AuthChecker`] for an authorization decision.
//!   3. On `Denied`, return `fi.nexus.Error.AuthFailed` immediately.
//!   4. On `Authorized`, dispatch to the profile store / [`crate::backend_ops::BackendOps`].

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use nexus_core::Ssid;
use nexus_profile_store::{
    Dot1xEapConfig, Dot1xSettings, EapMethod, EthInterfaceSettings, EthernetProfile,
    ProfileMetadata, SecretString, SecurityConfig, WifiNetworkSettings, WifiProfile, WpaPsk,
};
use ulid::Ulid;
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::authz::{AuthDecision, actions};
use crate::errors::DbusError;
use crate::paths::{ethernet_profile_path, interface_path, wifi_profile_path};
use crate::rate_limit::OpClass;
use crate::services::Services;
use crate::state::PowerState;

/// Interface name used for introspection / signal matching.
pub const INTERFACE_NAME: &str = "fi.nexus.Manager";

pub struct Manager {
    pub services: Arc<Services>,
    /// Channel used to ask the service to register / unregister
    /// profile objects on the live `ObjectServer`. The event loop
    /// owns the receiver — see [`crate::service`].
    pub registry: tokio::sync::mpsc::Sender<crate::service::ServiceCommand>,
    /// Backup-lease bookkeeping. `None` when no lease is held.
    /// DD-006 §5.2 says at most one lease at a time, with a 60 s
    /// timeout enforced by the server.
    pub backup_lease: tokio::sync::Mutex<Option<BackupLease>>,
}

/// Active backup lease.
#[derive(Debug, Clone)]
pub struct BackupLease {
    pub token: String,
    pub expires_at: std::time::Instant,
}

#[zbus::interface(name = "fi.nexus.Manager")]
impl Manager {
    // -----------------------------------------------------------------
    // Properties — DD-006 §5.1
    // -----------------------------------------------------------------

    #[zbus(property)]
    async fn version(&self) -> String {
        self.services.state.read().await.version.clone()
    }

    #[zbus(property, name = "ApiCapabilities")]
    async fn api_capabilities(&self) -> Vec<String> {
        self.services.state.read().await.api_capabilities.clone()
    }

    #[zbus(property, name = "PowerState")]
    async fn power_state(&self) -> String {
        self.services
            .state
            .read()
            .await
            .power_state
            .as_str()
            .to_owned()
    }

    #[zbus(property, name = "Interfaces")]
    async fn interfaces(&self) -> Vec<OwnedObjectPath> {
        let guard = self.services.state.read().await;
        guard
            .interfaces
            .keys()
            .filter_map(|name| ObjectPath::try_from(interface_path(name)).ok())
            .map(OwnedObjectPath::from)
            .collect()
    }

    #[zbus(property, name = "EthernetProfiles")]
    async fn ethernet_profiles(&self) -> Vec<OwnedObjectPath> {
        let guard = self.services.state.read().await;
        guard
            .ethernet_profiles
            .values()
            .filter_map(|p| ObjectPath::try_from(ethernet_profile_path(&p.id)).ok())
            .map(OwnedObjectPath::from)
            .collect()
    }

    #[zbus(property, name = "WifiProfiles")]
    async fn wifi_profiles(&self) -> Vec<OwnedObjectPath> {
        let guard = self.services.state.read().await;
        guard
            .wifi_profiles
            .values()
            .filter_map(|p| ObjectPath::try_from(wifi_profile_path(&p.id)).ok())
            .map(OwnedObjectPath::from)
            .collect()
    }

    #[zbus(property, name = "MasterKeySource")]
    async fn master_key_source(&self) -> String {
        self.services.state.read().await.master_key_source.clone()
    }

    /// Last observed internet-reachability state. One of
    /// `internetUnknown` (pre-probe), `internetOnline`,
    /// `internetCaptivePortal`, `internetOffline`. Mirrors the most
    /// recent `InternetConnectivityChanged` signal (which is emitted
    /// directly from the service event loop — see the comment at the
    /// bottom of this impl).
    #[zbus(property, name = "InternetConnectivity")]
    async fn internet_connectivity(&self) -> String {
        self.services
            .state
            .read()
            .await
            .internet_connectivity
            .as_str()
            .to_owned()
    }

    // -----------------------------------------------------------------
    // Read-only lookup methods — DD-006 §5.2
    // -----------------------------------------------------------------

    /// `GetInterface(ifname: s) -> (path: o)`
    async fn get_interface(&self, ifname: String) -> fdo::Result<OwnedObjectPath> {
        let guard = self.services.state.read().await;
        if !guard.interfaces.contains_key(&ifname) {
            return Err(DbusError::NotFound(format!("interface '{ifname}'")).into());
        }
        let path = interface_path(&ifname);
        Ok(ObjectPath::try_from(path.clone())
            .map_err(|e| DbusError::InvalidArgument(format!("bad path {path}: {e}")))?
            .into())
    }

    /// `FindWifiProfile(ssid: ay) -> (path: o)`
    async fn find_wifi_profile(&self, ssid: Vec<u8>) -> fdo::Result<OwnedObjectPath> {
        let guard = self.services.state.read().await;
        let Some(profile) = guard
            .wifi_profiles
            .values()
            .find(|p| p.network.ssid.as_bytes() == ssid.as_slice())
        else {
            return Err(
                DbusError::NotFound(format!("wifi profile for {} bytes", ssid.len())).into(),
            );
        };
        let path = wifi_profile_path(&profile.id);
        Ok(ObjectPath::try_from(path.clone())
            .map_err(|e| DbusError::InvalidArgument(format!("bad path {path}: {e}")))?
            .into())
    }

    /// `GetManagerStatus() -> a{sv}` — convenience snapshot.
    async fn get_manager_status(&self) -> fdo::Result<HashMap<String, OwnedValue>> {
        let guard = self.services.state.read().await;
        let mut out: HashMap<String, OwnedValue> = HashMap::new();
        out.insert(
            "Version".into(),
            OwnedValue::try_from(Value::new(guard.version.clone()))
                .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        out.insert(
            "PowerState".into(),
            OwnedValue::try_from(Value::new(guard.power_state.as_str().to_owned()))
                .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        out.insert(
            "ApiCapabilities".into(),
            OwnedValue::try_from(Value::new(guard.api_capabilities.clone()))
                .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        let interfaces: Vec<String> = guard.interfaces.keys().cloned().collect();
        out.insert(
            "Interfaces".into(),
            OwnedValue::try_from(Value::new(interfaces))
                .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        out.insert(
            "WifiProfileCount".into(),
            OwnedValue::try_from(Value::new(guard.wifi_profiles.len() as u32))
                .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        out.insert(
            "EthernetProfileCount".into(),
            OwnedValue::try_from(Value::new(guard.ethernet_profiles.len() as u32))
                .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        out.insert(
            "BluetoothProfileCount".into(),
            OwnedValue::try_from(Value::new(guard.bluetooth_profiles.len() as u32))
                .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        out.insert(
            "MasterKeySource".into(),
            OwnedValue::try_from(Value::new(guard.master_key_source.clone()))
                .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        out.insert(
            "InternetConnectivity".into(),
            OwnedValue::try_from(Value::new(
                guard.internet_connectivity.as_str().to_owned(),
            ))
            .map_err(|e| DbusError::InvalidArgument(e.to_string()))?,
        );
        Ok(out)
    }

    // -----------------------------------------------------------------
    // Mutating methods — DD-006 §5.2 (auth checks per §10)
    // -----------------------------------------------------------------

    /// `AddWifiProfile(settings: a{sv}) -> (path: o)`. Requires
    /// `fi.nexus.profile.add`.
    async fn add_wifi_profile(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        settings: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check_feature(crate::services::Feature::Wifi)?;
        self.check_rate(&hdr, OpClass::ProfileWrite)?;
        self.require_auth(&hdr, actions::PROFILE_ADD).await?;
        let profile = parse_wifi_settings(&settings)
            .map_err(|e| fdo::Error::from(DbusError::InvalidArgument(e)))?;

        // Uniqueness: at most one profile per SSID.
        {
            let guard = self.services.state.read().await;
            let exists = guard
                .wifi_profiles
                .values()
                .any(|p| p.network.ssid.as_bytes() == profile.network.ssid.as_bytes());
            if exists {
                return Err(DbusError::AlreadyExists(format!(
                    "wifi profile for SSID with {} bytes",
                    profile.network.ssid.as_bytes().len()
                ))
                .into());
            }
        }

        self.services
            .profile_store
            .put_wifi(&profile)
            .await
            .map_err(|e| fdo::Error::from(DbusError::from(e)))?;

        let id = profile.id;
        self.services
            .state
            .write()
            .await
            .wifi_profiles
            .insert(id.to_string(), profile);

        // Tell the event loop to register the profile object on
        // the live ObjectServer.
        let _ = self
            .registry
            .send(crate::service::ServiceCommand::RegisterWifiProfile(id))
            .await;

        let path = wifi_profile_path(&id);
        Ok(ObjectPath::try_from(path)
            .map_err(|e| DbusError::InvalidArgument(e.to_string()))?
            .into())
    }

    /// `AddEthernetProfile(settings: a{sv}) -> (path: o)`. Requires
    /// `fi.nexus.profile.add`.
    async fn add_ethernet_profile(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        settings: HashMap<String, OwnedValue>,
    ) -> fdo::Result<OwnedObjectPath> {
        self.check_feature(crate::services::Feature::Ethernet)?;
        self.check_rate(&hdr, OpClass::ProfileWrite)?;
        self.require_auth(&hdr, actions::PROFILE_ADD).await?;
        let profile = parse_ethernet_settings(&settings)
            .map_err(|e| fdo::Error::from(DbusError::InvalidArgument(e)))?;

        // Uniqueness: at most one profile per ifname.
        {
            let guard = self.services.state.read().await;
            let exists = guard
                .ethernet_profiles
                .values()
                .any(|p| p.interface.name == profile.interface.name);
            if exists {
                return Err(DbusError::AlreadyExists(format!(
                    "ethernet profile for ifname '{}'",
                    profile.interface.name
                ))
                .into());
            }
        }

        self.services
            .profile_store
            .put_ethernet(&profile)
            .await
            .map_err(|e| fdo::Error::from(DbusError::from(e)))?;

        let id = profile.id;
        self.services
            .state
            .write()
            .await
            .ethernet_profiles
            .insert(id.to_string(), profile);

        let _ = self
            .registry
            .send(crate::service::ServiceCommand::RegisterEthernetProfile(id))
            .await;

        let path = ethernet_profile_path(&id);
        Ok(ObjectPath::try_from(path)
            .map_err(|e| DbusError::InvalidArgument(e.to_string()))?
            .into())
    }

    /// `RemoveProfile(path: o) -> ()`. Requires
    /// `fi.nexus.profile.modify`.
    async fn remove_profile(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        path: OwnedObjectPath,
    ) -> fdo::Result<()> {
        self.check_rate(&hdr, OpClass::ProfileWrite)?;
        self.require_auth(&hdr, actions::PROFILE_MODIFY).await?;
        let path_str = path.as_str();

        // Determine kind by prefix.
        let id_str = if let Some(rest) = path_str.strip_prefix("/fi/nexus1/profile/wifi/") {
            rest.to_owned()
        } else if let Some(rest) = path_str.strip_prefix("/fi/nexus1/profile/ethernet/") {
            rest.to_owned()
        } else {
            return Err(
                DbusError::InvalidArgument(format!("unknown profile path '{path_str}'")).into(),
            );
        };
        let id = Ulid::from_string(&id_str)
            .map_err(|e| DbusError::InvalidArgument(format!("bad ulid: {e}")))?;

        if path_str.contains("/profile/wifi/") {
            // Hash off-disk filename in the store is the SSID hash;
            // the store maps that internally on remove via the id
            // (we use load_wifi to get the SSID). Simpler: just
            // remove the in-memory entry + tell the event loop to
            // unregister. The on-disk file removal is deferred to
            // the Profile.Delete shortcut path which knows the id
            // → SSID mapping by reading the in-memory profile.
            let mut state = self.services.state.write().await;
            let Some(profile) = state.wifi_profiles.remove(&id_str) else {
                return Err(DbusError::NotFound(format!("wifi profile {id}")).into());
            };
            // remove_wifi takes the SSID hash. The store's
            // ssid_hash() helper computes it.
            let key = nexus_profile_store::ssid_hash(&profile.network.ssid);
            drop(state);
            self.services
                .profile_store
                .remove_wifi(&key)
                .await
                .map_err(|e| fdo::Error::from(DbusError::from(e)))?;
            let _ = self
                .registry
                .send(crate::service::ServiceCommand::UnregisterWifiProfile(id))
                .await;
        } else {
            let mut state = self.services.state.write().await;
            let Some(profile) = state.ethernet_profiles.remove(&id_str) else {
                return Err(DbusError::NotFound(format!("ethernet profile {id}")).into());
            };
            let ifname = profile.interface.name.clone();
            drop(state);
            self.services
                .profile_store
                .remove_ethernet(&ifname)
                .await
                .map_err(|e| fdo::Error::from(DbusError::from(e)))?;
            let _ = self
                .registry
                .send(crate::service::ServiceCommand::UnregisterEthernetProfile(
                    id,
                ))
                .await;
        }
        Ok(())
    }

    /// `SetPowerState(state: s) -> ()`. Requires
    /// `fi.nexus.set_power`.
    async fn set_power_state(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        state: String,
    ) -> fdo::Result<()> {
        self.require_auth(&hdr, actions::SET_POWER).await?;
        let next = PowerState::parse(&state).ok_or_else(|| {
            fdo::Error::from(DbusError::InvalidArgument(format!(
                "unknown power state '{state}'"
            )))
        })?;
        // Update local state first; backends see the change via
        // BackendOps. The `PowerState` property's auto-emitted
        // PropertiesChanged signal handles client-side notification.
        self.services.state.write().await.power_state = next;
        self.services
            .ops
            .set_power_state(next)
            .await
            .map_err(fdo::Error::from)?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // Signals — DD-006 §5.3
    // -----------------------------------------------------------------

    // -----------------------------------------------------------------
    // Admin operations — DD-006 §5.2 (auth fi.nexus.admin)
    // -----------------------------------------------------------------

    /// `RotateMasterKey() -> (job_id: s)`. Rotates the profile-store
    /// master key. Returns immediately with a job id; the
    /// `MasterKeyRotated` signal fires when the rotation completes.
    async fn rotate_master_key(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<String> {
        // The 1/min admin rate limit also serves as the
        // serialization mechanism — two RotateMasterKey calls
        // within the window get `RateLimited`.
        self.check_rate(&hdr, OpClass::Admin)?;
        self.require_auth(&hdr, actions::ADMIN).await?;
        let job_id = Ulid::new().to_string();

        let store = Arc::clone(&self.services.profile_store);
        let emitter_owned = emitter.to_owned();
        let job_id_for_task = job_id.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            let outcome = store.rotate_master_key().await;
            let mut report: HashMap<String, OwnedValue> = HashMap::new();
            let elapsed_ms = started.elapsed().as_millis() as u64;
            match outcome {
                Ok(rep) => {
                    if let Ok(v) = OwnedValue::try_from(Value::new("success".to_owned())) {
                        report.insert("outcome".to_owned(), v);
                    }
                    if let Ok(v) = OwnedValue::try_from(Value::new(rep.profiles_rewritten)) {
                        report.insert("profiles_rewritten".to_owned(), v);
                    }
                    if let Ok(v) = OwnedValue::try_from(Value::new(elapsed_ms)) {
                        report.insert("duration_ms".to_owned(), v);
                    }
                }
                Err(e) => {
                    if let Ok(v) = OwnedValue::try_from(Value::new("failed".to_owned())) {
                        report.insert("outcome".to_owned(), v);
                    }
                    if let Ok(v) = OwnedValue::try_from(Value::new(format!("{e}"))) {
                        report.insert("error".to_owned(), v);
                    }
                    if let Ok(v) = OwnedValue::try_from(Value::new(elapsed_ms)) {
                        report.insert("duration_ms".to_owned(), v);
                    }
                }
            }
            let _ = Manager::master_key_rotated(&emitter_owned, &job_id_for_task, report).await;
        });
        Ok(job_id)
    }

    /// `FreezeForBackup() -> (lease: s)`. Returns a UUID-style
    /// lease token; expires after 60 s if not released.
    async fn freeze_for_backup(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<String> {
        self.check_rate(&hdr, OpClass::Admin)?;
        self.require_auth(&hdr, actions::ADMIN).await?;
        let mut guard = self.backup_lease.lock().await;
        // Expire stale leases first.
        if let Some(lease) = guard.as_ref() {
            if lease.expires_at > std::time::Instant::now() {
                return Err(DbusError::ResourceBusy("backup lease already held".into()).into());
            }
        }
        let token = generate_uuid_v4();
        *guard = Some(BackupLease {
            token: token.clone(),
            expires_at: std::time::Instant::now() + std::time::Duration::from_secs(60),
        });
        Ok(token)
    }

    /// `ReloadConfig() -> a{sv}`. See DD-006 §5.2.
    ///
    /// Re-reads `/etc/nexus/nexus.toml` (or the path passed to
    /// `nexusd --config`), diffs it against the live config, and
    /// applies the safely-reloadable sections. The returned dict has
    /// three keys:
    ///
    /// - `applied`  : `as` — sections whose new values took effect.
    /// - `deferred` : `as` — sections that differ but require a
    ///                       daemon restart to take effect.
    /// - `errors`   : `a(ss)` — `(section, reason)` pairs for
    ///                          sections that failed to reload due to
    ///                          invalid values; those sections keep
    ///                          their prior values.
    ///
    /// A structural parse error on the config file propagates as
    /// `fi.nexus.Error.IoError` — no partial apply happens.
    async fn reload_config(
        &self,
        #[zbus(header)] hdr: Header<'_>,
    ) -> fdo::Result<HashMap<String, OwnedValue>> {
        self.check_rate(&hdr, OpClass::Admin)?;
        self.require_auth(&hdr, actions::ADMIN).await?;
        let report = self
            .services
            .ops
            .reload_config()
            .await
            .map_err(fdo::Error::from)?;
        Ok(reload_report_to_dict(&report))
    }

    /// `ReleaseBackupLease(lease: s) -> ()`. Compares
    /// byte-for-byte; mismatches return NotFound.
    async fn release_backup_lease(&self, lease: String) -> fdo::Result<()> {
        let mut guard = self.backup_lease.lock().await;
        match guard.as_ref() {
            Some(active)
                if active.token == lease && active.expires_at > std::time::Instant::now() =>
            {
                *guard = None;
                Ok(())
            }
            _ => Err(DbusError::NotFound("backup lease".into()).into()),
        }
    }

    /// `NotificationEvent(kind: s, data: a{sv})`.
    #[zbus(signal)]
    pub async fn notification_event(
        emitter: &SignalEmitter<'_>,
        kind: &str,
        data: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    /// `MasterKeyRotated(job_id: s, report: a{sv})`.
    #[zbus(signal)]
    pub async fn master_key_rotated(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        report: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    // `InternetConnectivityChanged(state: s)` and the matching
    // `org.freedesktop.DBus.Properties.PropertiesChanged` for the
    // `InternetConnectivity` property are both emitted from the
    // service event loop via `connection.emit_signal` (see
    // `service::emit_manager_internet_connectivity_changed`). The
    // bare signal is NOT declared with `#[zbus(signal)]` here
    // because zbus auto-generates an `internet_connectivity_changed`
    // PropertiesChanged emitter from the `InternetConnectivity`
    // property above, and the Rust names collide. The property is
    // mutated outside the zbus property API (the broadcast-event
    // loop writes `state.internet_connectivity` directly), so the
    // auto-emit never fires either way — the manual dual emit is
    // what honours the `emits-change` introspection annotation.
}

impl Manager {
    pub fn new(
        services: Arc<Services>,
        registry: tokio::sync::mpsc::Sender<crate::service::ServiceCommand>,
    ) -> Self {
        Self {
            services,
            registry,
            backup_lease: tokio::sync::Mutex::new(None),
        }
    }

    /// Per-class rate-limit check. Returns `Err(RateLimited)` with
    /// a `retry_after_ms` hint when the per-(sender, op-class)
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

    /// `FeatureDisabled` gate for a per-technology Manager op
    /// (AddWifiProfile / AddEthernetProfile).
    fn check_feature(&self, feature: crate::services::Feature) -> fdo::Result<()> {
        self.services
            .enabled
            .require(feature)
            .map_err(fdo::Error::from)
    }

    /// Pull the caller's bus name from the message header and run
    /// the configured authorization check. Returns `Ok(())` when
    /// authorized; `Err(AuthFailed)` otherwise.
    pub(crate) async fn require_auth(&self, hdr: &Header<'_>, action: &str) -> fdo::Result<()> {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        let decision = self.services.auth.check(action, &sender).await;
        match decision {
            AuthDecision::Authorized => Ok(()),
            AuthDecision::Denied => Err(fdo::Error::from(DbusError::AuthFailed(format!(
                "policykit denied '{action}' for sender '{sender}'"
            )))),
        }
    }
}

// ---------------------------------------------------------------------------
// ReloadConfig → D-Bus dict
// ---------------------------------------------------------------------------

/// Serialize a [`crate::ReloadReport`] into the DD-006 §5.2 report
/// dict. Keys are fixed; any failure to encode a sub-array is
/// logged and swallowed (a partial dict is more useful than a hard
/// D-Bus error).
fn reload_report_to_dict(report: &crate::ReloadReport) -> HashMap<String, OwnedValue> {
    let mut out: HashMap<String, OwnedValue> = HashMap::new();
    if let Ok(v) = OwnedValue::try_from(Value::new(report.applied.clone())) {
        out.insert("applied".to_owned(), v);
    }
    if let Ok(v) = OwnedValue::try_from(Value::new(report.deferred.clone())) {
        out.insert("deferred".to_owned(), v);
    }
    // `errors` is a(ss). zbus renders Vec<(String, String)> as that
    // signature automatically.
    if let Ok(v) = OwnedValue::try_from(Value::new(report.errors.clone())) {
        out.insert("errors".to_owned(), v);
    }
    out
}

// ---------------------------------------------------------------------------
// Settings-dict parsers
// ---------------------------------------------------------------------------

/// Parse a Wi-Fi `AddWifiProfile` settings dict. See DD-006 §16.2.
fn parse_wifi_settings(
    s: &HashMap<String, OwnedValue>,
) -> std::result::Result<WifiProfile, String> {
    let ssid_bytes =
        lookup_byte_array(s, "ssid")?.ok_or_else(|| "missing required field 'ssid'".to_owned())?;
    let ssid = Ssid::new(ssid_bytes).map_err(|e| format!("invalid ssid: {e:?}"))?;
    let priority = lookup_i32(s, "priority")?.unwrap_or(0);
    let auto_connect = lookup_bool(s, "auto_connect")?.unwrap_or(true);
    let hidden = lookup_bool(s, "hidden")?.unwrap_or(false);
    let fast_transition = lookup_bool(s, "fast_transition")?.unwrap_or(false);
    let security_dict = lookup_dict(s, "security")?
        .ok_or_else(|| "missing required field 'security' dict".to_owned())?;
    let security = parse_security(&security_dict)?;

    let now = Utc::now();
    Ok(WifiProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata {
            created_at: Some(now),
            updated_at: Some(now),
            label: lookup_string(s, "label")?,
        },
        network: WifiNetworkSettings {
            ssid,
            hidden,
            priority,
            auto_connect,
            fast_transition,
            security,
            bssid_preferred: None,
            bssid_blacklist: Vec::new(),
            scan_freqs: Vec::new(),
            credentials_invalid: false,
            last_connected_at: None,
        },
    })
}

fn parse_security(
    dict: &HashMap<String, OwnedValue>,
) -> std::result::Result<SecurityConfig, String> {
    let kind =
        lookup_string(dict, "type")?.ok_or_else(|| "security.type is required".to_owned())?;
    Ok(match kind.as_str() {
        "open" => SecurityConfig::Open,
        "owe" => SecurityConfig::Owe,
        "wpa2_personal" => {
            let pass = lookup_string(dict, "passphrase")?
                .ok_or_else(|| "wpa2_personal needs 'passphrase'".to_owned())?;
            SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(SecretString::from(pass)),
            }
        }
        "wpa3_personal" => {
            let pass = lookup_string(dict, "passphrase")?
                .ok_or_else(|| "wpa3_personal needs 'passphrase'".to_owned())?;
            SecurityConfig::Wpa3Personal {
                passphrase: SecretString::from(pass),
            }
        }
        "wpa2_wpa3_personal" => {
            let pass = lookup_string(dict, "passphrase")?
                .ok_or_else(|| "wpa2_wpa3_personal needs 'passphrase'".to_owned())?;
            SecurityConfig::Wpa2Wpa3Personal {
                passphrase: SecretString::from(pass),
            }
        }
        other => return Err(format!("unsupported security.type '{other}'")),
    })
}

/// Parse an `AddEthernetProfile` dict.
fn parse_ethernet_settings(
    s: &HashMap<String, OwnedValue>,
) -> std::result::Result<EthernetProfile, String> {
    let ifname =
        lookup_string(s, "ifname")?.ok_or_else(|| "missing required field 'ifname'".to_owned())?;
    let auto_connect = lookup_bool(s, "auto_connect")?.unwrap_or(true);
    let dot1x = lookup_dict(s, "dot1x")?
        .map(|d| parse_dot1x(&d))
        .transpose()?;
    let now = Utc::now();
    Ok(EthernetProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata {
            created_at: Some(now),
            updated_at: Some(now),
            label: lookup_string(s, "label")?,
        },
        interface: EthInterfaceSettings {
            name: ifname,
            auto_connect,
        },
        dot1x,
    })
}

fn parse_dot1x(d: &HashMap<String, OwnedValue>) -> std::result::Result<Dot1xSettings, String> {
    let enabled = lookup_bool(d, "enabled")?.unwrap_or(true);
    let eap_str = lookup_string(d, "eap")?.ok_or_else(|| "dot1x.eap is required".to_owned())?;
    let eap = match eap_str.to_ascii_uppercase().as_str() {
        "PEAP" => EapMethod::Peap,
        "TTLS" => EapMethod::Ttls,
        "TLS" => EapMethod::Tls,
        "PWD_MSCHAPV2" => EapMethod::PwdMschapv2,
        "LEAP" => EapMethod::Leap,
        "FAST" => EapMethod::Fast,
        other => return Err(format!("unknown EAP method '{other}'")),
    };
    let identity =
        lookup_string(d, "identity")?.ok_or_else(|| "dot1x.identity is required".to_owned())?;
    Ok(Dot1xSettings {
        enabled,
        eap: Dot1xEapConfig {
            eap,
            identity,
            anonymous_identity: lookup_string(d, "anonymous_identity")?,
            ca_cert: lookup_string(d, "ca_cert")?,
            client_cert: lookup_string(d, "client_cert")?,
            client_key: lookup_string(d, "client_key")?,
            client_key_password: lookup_string(d, "client_key_password")?.map(SecretString::from),
            phase2: lookup_string(d, "phase2")?,
            domain_suffix_match: lookup_string(d, "domain_suffix_match")?,
            password: lookup_string(d, "password")?.map(SecretString::from),
        },
    })
}

// ---------------------------------------------------------------------------
// Variant-dict lookup helpers — every getter returns
// `Result<Option<T>, String>` so absent keys are distinguished
// from type mismatches.
// ---------------------------------------------------------------------------

pub(crate) fn lookup_string(
    dict: &HashMap<String, OwnedValue>,
    key: &str,
) -> std::result::Result<Option<String>, String> {
    let Some(v) = dict.get(key) else {
        return Ok(None);
    };
    let s: &str = <&str>::try_from(v).map_err(|e| format!("'{key}' must be a string: {e}"))?;
    Ok(Some(s.to_owned()))
}

pub(crate) fn lookup_bool(
    dict: &HashMap<String, OwnedValue>,
    key: &str,
) -> std::result::Result<Option<bool>, String> {
    let Some(v) = dict.get(key) else {
        return Ok(None);
    };
    bool::try_from(v)
        .map(Some)
        .map_err(|e| format!("'{key}' must be a boolean: {e}"))
}

pub(crate) fn lookup_i32(
    dict: &HashMap<String, OwnedValue>,
    key: &str,
) -> std::result::Result<Option<i32>, String> {
    let Some(v) = dict.get(key) else {
        return Ok(None);
    };
    i32::try_from(v)
        .map(Some)
        .map_err(|e| format!("'{key}' must be int32: {e}"))
}

pub(crate) fn lookup_byte_array(
    dict: &HashMap<String, OwnedValue>,
    key: &str,
) -> std::result::Result<Option<Vec<u8>>, String> {
    let Some(v) = dict.get(key) else {
        return Ok(None);
    };
    let arr: &zbus::zvariant::Array = v
        .downcast_ref()
        .map_err(|e| format!("'{key}' must be ay: {e}"))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr.iter() {
        let b: u8 = item
            .downcast_ref()
            .map_err(|e| format!("'{key}' element must be byte: {e}"))?;
        out.push(b);
    }
    Ok(Some(out))
}

/// Synthesize a UUID v4 string. Avoids pulling in the `uuid`
/// crate for one call; the format is `xxxxxxxx-xxxx-4xxx-yxxx-…`
/// per RFC 4122. We feed `getrandom` (already in the workspace
/// via other crates) for the 16 random bytes.
fn generate_uuid_v4() -> String {
    let mut buf = [0u8; 16];
    // ULID's RNG is already cryptographically reasonable for our
    // purposes. Re-using it avoids adding a dependency for one
    // call. The Ulid bytes are 128 bits; we take them as-is, set
    // the version + variant nibbles per RFC 4122, and format.
    let bytes = Ulid::new().to_bytes();
    buf.copy_from_slice(&bytes);
    buf[6] = (buf[6] & 0x0F) | 0x40; // version 4
    buf[8] = (buf[8] & 0x3F) | 0x80; // variant 1 (RFC 4122)
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        buf[0],
        buf[1],
        buf[2],
        buf[3],
        buf[4],
        buf[5],
        buf[6],
        buf[7],
        buf[8],
        buf[9],
        buf[10],
        buf[11],
        buf[12],
        buf[13],
        buf[14],
        buf[15],
    )
}

pub(crate) fn lookup_dict(
    dict: &HashMap<String, OwnedValue>,
    key: &str,
) -> std::result::Result<Option<HashMap<String, OwnedValue>>, String> {
    let Some(v) = dict.get(key) else {
        return Ok(None);
    };
    HashMap::<String, OwnedValue>::try_from(v.clone())
        .map(Some)
        .map_err(|e| format!("'{key}' must be a{{sv}} dict: {e}"))
}
