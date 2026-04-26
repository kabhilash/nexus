//! `Manager.ReloadConfig` coordinator. See DD-006 §5.2.
//!
//! # What reload does
//!
//! 1. Re-read the TOML file from the `--config` path fresh — no cache.
//! 2. Run full validation (`Config::load_from_path`); on structural
//!    or semantic errors the whole call fails with [`ReloadError`]
//!    and the live config is unchanged.
//! 3. Diff the new config against the live one field-by-field.
//! 4. Classify every differing field as:
//!    - `applied`   — the change took effect at runtime.
//!    - `deferred`  — the field is startup-only; the change is kept
//!                    in the live config so subsequent restarts pick
//!                    it up, but runtime behaviour is unchanged.
//!    - `errors`    — applying the change failed; the field keeps
//!                    its prior value.
//! 5. Swap the live config atomically.
//!
//! # What's actually reloadable today
//!
//! `log_level` is the one field wired end-to-end: the daemon's
//! tracing subscriber exposes an [`tracing_subscriber::reload::Handle`]
//! which the coordinator drives via the [`LogLevelSetter`] callback.
//!
//! Every other field that differs currently goes into `deferred`.
//! Future commits are expected to grow per-backend reload hooks —
//! the classification table in [`diff_config`] is the seam where
//! fields graduate from `deferred` to `applied`.
//!
//! # Design notes
//!
//! - [`diff_config`] is pure (no IO, no async). It takes the old and
//!   new configs plus a [`LogLevelSetter`] closure. Tests exercise it
//!   directly.
//! - [`ReloadCoordinator`] wraps [`diff_config`] with the file-read
//!   + live-config-swap concerns.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nexus_dbus::{BackendOps, DbusError, ReloadReport, RoamingMode, ScanParams};
use tokio::sync::RwLock;

use crate::config::{Config, ConfigError};

/// A callback that applies a new `log_level` value to the running
/// tracing subscriber. Returns a human-readable error on failure
/// (e.g., malformed directive). Wired by `main.rs` to
/// `tracing_subscriber::reload::Handle::modify`.
///
/// Tests swap this for a recording/failing mock.
pub type LogLevelSetter = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Top-level errors returned by [`ReloadCoordinator::reload`]. These
/// map onto D-Bus errors via [`ReloadError::to_dbus_error`]: parse
/// and IO failures are `IoError`; validation failures are
/// `InvalidArgument`.
#[derive(Debug)]
pub enum ReloadError {
    /// Reading the file failed (file gone, EACCES, …).
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// TOML syntax error.
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// A field failed the [`Config::validate`] invariant check.
    Invalid(String),
}

impl std::fmt::Display for ReloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReloadError::Io { path, source } => {
                write!(f, "reading {}: {source}", path.display())
            }
            ReloadError::Parse { path, source } => {
                write!(f, "parsing {}: {source}", path.display())
            }
            ReloadError::Invalid(m) => write!(f, "config validation failed: {m}"),
        }
    }
}

impl std::error::Error for ReloadError {}

impl From<ConfigError> for ReloadError {
    fn from(e: ConfigError) -> Self {
        match e {
            ConfigError::Io { path, source } => ReloadError::Io { path, source },
            ConfigError::Parse { path, source } => ReloadError::Parse { path, source },
            ConfigError::Invalid(m) => ReloadError::Invalid(m),
        }
    }
}

impl ReloadError {
    /// Shape the error as a [`DbusError`] for the D-Bus layer. File /
    /// parse failures are [`DbusError::Io`]; semantic-validation
    /// failures are [`DbusError::InvalidArgument`].
    pub fn to_dbus_error(&self) -> DbusError {
        match self {
            ReloadError::Io { .. } | ReloadError::Parse { .. } => {
                DbusError::Io(std::io::Error::other(self.to_string()))
            }
            ReloadError::Invalid(m) => DbusError::InvalidArgument(m.clone()),
        }
    }
}

/// Coordinator for `Manager.ReloadConfig`. Holds the config-file
/// path, the live in-memory config, and the log-reload callback.
pub struct ReloadCoordinator {
    config_path: PathBuf,
    live: Arc<RwLock<Config>>,
    log_setter: LogLevelSetter,
}

impl ReloadCoordinator {
    pub fn new(
        config_path: PathBuf,
        live: Arc<RwLock<Config>>,
        log_setter: LogLevelSetter,
    ) -> Self {
        Self {
            config_path,
            live,
            log_setter,
        }
    }

    /// Run the reload flow. Called from the daemon-local
    /// [`BackendOps::reload_config`] impl.
    pub async fn reload(&self) -> Result<ReloadReport, ReloadError> {
        // Re-read fresh — no cache. `Config::load_from_path` also
        // runs `validate()` so a semantically-invalid file fails
        // here without partial apply.
        let new = Config::load_from_path(&self.config_path)?;
        let old = self.live.read().await.clone();
        let report = diff_config(&old, &new, &self.log_setter);
        // Swap even when the report is empty — cheap, and keeps the
        // live config byte-identical to what we just validated.
        *self.live.write().await = new;
        Ok(report)
    }
}

/// `BackendOps` adapter that routes `reload_config` into a
/// [`ReloadCoordinator`]. The daemon wires one of these as the
/// process-wide `BackendOps` impl (layered over whatever per-
/// technology routing already exists — today that's just
/// `NoopOps`-equivalent).
pub struct ReloadOps {
    coordinator: Arc<ReloadCoordinator>,
    inner: Arc<dyn BackendOps>,
}

impl ReloadOps {
    pub fn new(coordinator: Arc<ReloadCoordinator>, inner: Arc<dyn BackendOps>) -> Arc<Self> {
        Arc::new(Self { coordinator, inner })
    }
}

#[async_trait]
impl BackendOps for ReloadOps {
    async fn wifi_scan(&self, ifname: &str, params: ScanParams) -> nexus_dbus::Result<()> {
        self.inner.wifi_scan(ifname, params).await
    }
    async fn wifi_connect(&self, ifname: &str, profile_id: ulid::Ulid) -> nexus_dbus::Result<()> {
        self.inner.wifi_connect(ifname, profile_id).await
    }
    async fn wifi_disconnect(
        &self,
        ifname: &str,
        pause_auto_connect: bool,
    ) -> nexus_dbus::Result<()> {
        self.inner.wifi_disconnect(ifname, pause_auto_connect).await
    }
    async fn wifi_roam(&self, ifname: &str, bssid: nexus_core::MacAddr) -> nexus_dbus::Result<()> {
        self.inner.wifi_roam(ifname, bssid).await
    }
    async fn wifi_set_powered(&self, ifname: &str, on: bool) -> nexus_dbus::Result<()> {
        self.inner.wifi_set_powered(ifname, on).await
    }
    async fn wifi_set_roaming_mode(
        &self,
        ifname: &str,
        mode: RoamingMode,
    ) -> nexus_dbus::Result<()> {
        self.inner.wifi_set_roaming_mode(ifname, mode).await
    }
    async fn set_power_state(&self, state: nexus_dbus::PowerState) -> nexus_dbus::Result<()> {
        self.inner.set_power_state(state).await
    }
    async fn reload_config(&self) -> nexus_dbus::Result<ReloadReport> {
        self.coordinator
            .reload()
            .await
            .map_err(|e| e.to_dbus_error())
    }
}

/// Pure diff: compare `old` vs `new` field by field, populate a
/// [`ReloadReport`]. The only side effect is the [`LogLevelSetter`]
/// callback, which the coordinator injects — tests pass a
/// recording/failing stub to exercise the applied/errors paths
/// without touching the global tracing subscriber.
///
/// # Classification table
///
/// | Field                               | Category   |
/// | ----------------------------------- | ---------- |
/// | `log_level`                         | applied    |
/// | everything else that changes        | deferred   |
///
/// Growing the "applied" column is the future-work surface: add a
/// match arm + the backend hook it needs.
pub fn diff_config(old: &Config, new: &Config, log_setter: &LogLevelSetter) -> ReloadReport {
    let mut report = ReloadReport::default();

    // Top-level -------------------------------------------------------
    if old.log_level != new.log_level {
        match (log_setter)(&new.log_level) {
            Ok(()) => report.applied.push("log_level".into()),
            Err(reason) => report.errors.push(("log_level".into(), reason)),
        }
    }
    if old.bus_capacity != new.bus_capacity {
        report.deferred.push("bus_capacity".into());
    }

    // [supervision] ---------------------------------------------------
    if old.supervision.restart != new.supervision.restart {
        report.deferred.push("supervision.restart".into());
    }
    if old.supervision.restart_initial_backoff != new.supervision.restart_initial_backoff {
        report
            .deferred
            .push("supervision.restart_initial_backoff".into());
    }
    if old.supervision.restart_max_backoff != new.supervision.restart_max_backoff {
        report
            .deferred
            .push("supervision.restart_max_backoff".into());
    }
    if old.supervision.restart_multiplier != new.supervision.restart_multiplier {
        report
            .deferred
            .push("supervision.restart_multiplier".into());
    }

    // [interface_monitor] --------------------------------------------
    if old.interface_monitor.enabled != new.interface_monitor.enabled {
        report.deferred.push("interface_monitor.enabled".into());
    }

    // [profile_store] ------------------------------------------------
    // Every field here is load-bearing at startup; a live change
    // would need a re-encryption / remigration dance that DD-007
    // explicitly gates behind RotateMasterKey.
    if old.profile_store.root != new.profile_store.root {
        report.deferred.push("profile_store.root".into());
    }
    if old.profile_store.key_source != new.profile_store.key_source {
        report.deferred.push("profile_store.key_source".into());
    }
    if old.profile_store.in_memory_seed != new.profile_store.in_memory_seed {
        report.deferred.push("profile_store.in_memory_seed".into());
    }

    // [dbus] ---------------------------------------------------------
    if old.dbus.enabled != new.dbus.enabled {
        report.deferred.push("dbus.enabled".into());
    }
    if old.dbus.bus_name != new.dbus.bus_name {
        report.deferred.push("dbus.bus_name".into());
    }
    if old.dbus.use_session_bus != new.dbus.use_session_bus {
        report.deferred.push("dbus.use_session_bus".into());
    }
    if old.dbus.address != new.dbus.address {
        report.deferred.push("dbus.address".into());
    }
    if old.dbus.allow_all_authz != new.dbus.allow_all_authz {
        report.deferred.push("dbus.allow_all_authz".into());
    }
    // Rate limits are technically reloadable (RateLimiter holds its
    // limits in a field) but the RateLimiter struct doesn't yet
    // expose an `update_limits` method and adding one is scoped to a
    // follow-on commit — see `RateLimiter::new` in nexus-dbus.
    if old.dbus.rate_limit_property_read_per_min != new.dbus.rate_limit_property_read_per_min {
        report
            .deferred
            .push("dbus.rate_limit_property_read_per_min".into());
    }
    if old.dbus.rate_limit_scan_per_min != new.dbus.rate_limit_scan_per_min {
        report.deferred.push("dbus.rate_limit_scan_per_min".into());
    }
    if old.dbus.rate_limit_connect_per_min != new.dbus.rate_limit_connect_per_min {
        report
            .deferred
            .push("dbus.rate_limit_connect_per_min".into());
    }
    if old.dbus.rate_limit_profile_write_per_min != new.dbus.rate_limit_profile_write_per_min {
        report
            .deferred
            .push("dbus.rate_limit_profile_write_per_min".into());
    }
    if old.dbus.rate_limit_admin_per_min != new.dbus.rate_limit_admin_per_min {
        report.deferred.push("dbus.rate_limit_admin_per_min".into());
    }

    // [ethernet] -----------------------------------------------------
    if old.ethernet.enabled != new.ethernet.enabled {
        report.deferred.push("ethernet.enabled".into());
    }
    if old.ethernet.auth_backend != new.ethernet.auth_backend {
        report.deferred.push("ethernet.auth_backend".into());
    }
    if old.ethernet.retry_initial != new.ethernet.retry_initial {
        report.deferred.push("ethernet.retry_initial".into());
    }
    if old.ethernet.retry_max != new.ethernet.retry_max {
        report.deferred.push("ethernet.retry_max".into());
    }
    if old.ethernet.retry_multiplier != new.ethernet.retry_multiplier {
        report.deferred.push("ethernet.retry_multiplier".into());
    }
    if old.ethernet.retry_max_attempts != new.ethernet.retry_max_attempts {
        report.deferred.push("ethernet.retry_max_attempts".into());
    }

    // [wifi] ---------------------------------------------------------
    if old.wifi.enabled != new.wifi.enabled {
        report.deferred.push("wifi.enabled".into());
    }
    if old.wifi.backend != new.wifi.backend {
        report.deferred.push("wifi.backend".into());
    }
    if old.wifi.roam_mode != new.wifi.roam_mode {
        report.deferred.push("wifi.roam_mode".into());
    }
    if old.wifi.signal_poll_interval != new.wifi.signal_poll_interval {
        report.deferred.push("wifi.signal_poll_interval".into());
    }
    if old.wifi.disconnect_cool_down != new.wifi.disconnect_cool_down {
        report.deferred.push("wifi.disconnect_cool_down".into());
    }
    if old.wifi.supplicant_event_capacity != new.wifi.supplicant_event_capacity {
        report
            .deferred
            .push("wifi.supplicant_event_capacity".into());
    }

    // [bluetooth] ----------------------------------------------------
    if old.bluetooth.enabled != new.bluetooth.enabled {
        report.deferred.push("bluetooth.enabled".into());
    }
    if old.bluetooth.mock != new.bluetooth.mock {
        report.deferred.push("bluetooth.mock".into());
    }
    if old.bluetooth.pairing_timeout_s != new.bluetooth.pairing_timeout_s {
        report.deferred.push("bluetooth.pairing_timeout_s".into());
    }
    if old.bluetooth.agent_response_timeout_s != new.bluetooth.agent_response_timeout_s {
        report
            .deferred
            .push("bluetooth.agent_response_timeout_s".into());
    }
    if old.bluetooth.discovery_timeout_s != new.bluetooth.discovery_timeout_s {
        report.deferred.push("bluetooth.discovery_timeout_s".into());
    }
    if old.bluetooth.discovery_device_ttl_s != new.bluetooth.discovery_device_ttl_s {
        report
            .deferred
            .push("bluetooth.discovery_device_ttl_s".into());
    }
    if old.bluetooth.bluez_outage_notify_s != new.bluetooth.bluez_outage_notify_s {
        report
            .deferred
            .push("bluetooth.bluez_outage_notify_s".into());
    }
    if old.bluetooth.auto_power_on_startup != new.bluetooth.auto_power_on_startup {
        report
            .deferred
            .push("bluetooth.auto_power_on_startup".into());
    }
    if old.bluetooth.register_agent != new.bluetooth.register_agent {
        report.deferred.push("bluetooth.register_agent".into());
    }

    // [gnss] ---------------------------------------------------------
    if old.gnss.enabled != new.gnss.enabled {
        report.deferred.push("gnss.enabled".into());
    }
    if old.gnss.mock != new.gnss.mock {
        report.deferred.push("gnss.mock".into());
    }
    if old.gnss.gpsd_endpoint != new.gnss.gpsd_endpoint {
        report.deferred.push("gnss.gpsd_endpoint".into());
    }
    if old.gnss.acquisition_timeout_s != new.gnss.acquisition_timeout_s {
        report.deferred.push("gnss.acquisition_timeout_s".into());
    }
    if old.gnss.tpv_stall_timeout_s != new.gnss.tpv_stall_timeout_s {
        report.deferred.push("gnss.tpv_stall_timeout_s".into());
    }
    if old.gnss.gpsd_outage_notify_s != new.gnss.gpsd_outage_notify_s {
        report.deferred.push("gnss.gpsd_outage_notify_s".into());
    }
    let (od, nd) = (&old.gnss.defaults, &new.gnss.defaults);
    if od.min_fix_mode != nd.min_fix_mode {
        report.deferred.push("gnss.defaults.min_fix_mode".into());
    }
    if od.min_satellites != nd.min_satellites {
        report.deferred.push("gnss.defaults.min_satellites".into());
    }
    if od.max_horizontal_error_m != nd.max_horizontal_error_m {
        report
            .deferred
            .push("gnss.defaults.max_horizontal_error_m".into());
    }
    if od.strict_quality != nd.strict_quality {
        report.deferred.push("gnss.defaults.strict_quality".into());
    }
    if od.max_update_hz != nd.max_update_hz {
        report.deferred.push("gnss.defaults.max_update_hz".into());
    }
    if od.report_movement_only != nd.report_movement_only {
        report
            .deferred
            .push("gnss.defaults.report_movement_only".into());
    }
    if od.movement_threshold_m != nd.movement_threshold_m {
        report
            .deferred
            .push("gnss.defaults.movement_threshold_m".into());
    }
    if od.heartbeat_interval_s != nd.heartbeat_interval_s {
        report
            .deferred
            .push("gnss.defaults.heartbeat_interval_s".into());
    }

    report
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn noop_setter() -> LogLevelSetter {
        Arc::new(|_level: &str| Ok(()))
    }

    fn recording_setter() -> (LogLevelSetter, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let log_c = Arc::clone(&log);
        let setter: LogLevelSetter = Arc::new(move |level: &str| {
            log_c.lock().unwrap().push(level.to_owned());
            Ok(())
        });
        (setter, log)
    }

    fn failing_setter() -> LogLevelSetter {
        Arc::new(|_: &str| Err("tracing reload handle gone".to_owned()))
    }

    #[test]
    fn no_changes_returns_empty_report() {
        let cfg = Config::default();
        let report = diff_config(&cfg, &cfg, &noop_setter());
        assert!(report.is_empty(), "got {report:?}");
    }

    #[test]
    fn log_level_change_is_applied() {
        let mut old = Config::default();
        old.log_level = "info".into();
        let mut new = Config::default();
        new.log_level = "debug".into();
        let (setter, log) = recording_setter();
        let report = diff_config(&old, &new, &setter);
        assert_eq!(report.applied, vec!["log_level".to_string()]);
        assert!(report.deferred.is_empty());
        assert!(report.errors.is_empty());
        assert_eq!(&*log.lock().unwrap(), &vec!["debug".to_string()]);
    }

    #[test]
    fn log_level_setter_failure_is_error() {
        let mut old = Config::default();
        old.log_level = "info".into();
        let mut new = Config::default();
        new.log_level = "debug".into();
        let report = diff_config(&old, &new, &failing_setter());
        assert!(report.applied.is_empty());
        assert_eq!(
            report.errors,
            vec![(
                "log_level".to_string(),
                "tracing reload handle gone".to_string()
            )]
        );
    }

    #[test]
    fn bus_name_change_is_deferred() {
        let mut old = Config::default();
        old.dbus.bus_name = "fi.nexus1".into();
        let mut new = Config::default();
        new.dbus.bus_name = "fi.nexus1.dev".into();
        let report = diff_config(&old, &new, &noop_setter());
        assert_eq!(report.deferred, vec!["dbus.bus_name".to_string()]);
        assert!(report.applied.is_empty());
        assert!(report.errors.is_empty());
    }

    #[test]
    fn multiple_changes_across_sections_are_classified() {
        let mut old = Config::default();
        old.log_level = "info".into();
        old.dbus.bus_name = "fi.nexus1".into();
        old.wifi.signal_poll_interval = std::time::Duration::from_secs(5);

        let mut new = Config::default();
        new.log_level = "warn".into();
        new.dbus.bus_name = "fi.nexus1.dev".into();
        new.wifi.signal_poll_interval = std::time::Duration::from_secs(2);

        let report = diff_config(&old, &new, &noop_setter());
        assert_eq!(report.applied, vec!["log_level".to_string()]);
        assert!(report.deferred.contains(&"dbus.bus_name".to_string()));
        assert!(
            report
                .deferred
                .contains(&"wifi.signal_poll_interval".to_string())
        );
        assert!(report.errors.is_empty());
    }

    #[tokio::test]
    async fn coordinator_no_change_returns_empty_report() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nexus.toml");
        std::fs::write(&path, "log_level = \"info\"\n").unwrap();
        let live = Arc::new(RwLock::new(Config::load_from_path(&path).unwrap()));
        let coord = ReloadCoordinator::new(path.clone(), live, noop_setter());
        let report = coord.reload().await.unwrap();
        assert!(report.is_empty());
    }

    #[tokio::test]
    async fn coordinator_applies_log_level_change_and_swaps_live() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nexus.toml");
        std::fs::write(&path, "log_level = \"info\"\n").unwrap();
        let live = Arc::new(RwLock::new(Config::load_from_path(&path).unwrap()));
        let (setter, log) = recording_setter();
        let coord = ReloadCoordinator::new(path.clone(), Arc::clone(&live), setter);

        // Edit the file and reload.
        std::fs::write(&path, "log_level = \"debug\"\n").unwrap();
        let report = coord.reload().await.unwrap();
        assert_eq!(report.applied, vec!["log_level".to_string()]);
        assert_eq!(&*log.lock().unwrap(), &vec!["debug".to_string()]);
        assert_eq!(live.read().await.log_level, "debug");
    }

    #[tokio::test]
    async fn coordinator_syntax_error_surfaces_parse_error_leaves_live_untouched() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nexus.toml");
        std::fs::write(&path, "log_level = \"info\"\n").unwrap();
        let live = Arc::new(RwLock::new(Config::load_from_path(&path).unwrap()));
        let coord = ReloadCoordinator::new(path.clone(), Arc::clone(&live), noop_setter());

        // Break the file — unquoted value.
        std::fs::write(&path, "log_level = info\n").unwrap();
        let err = coord.reload().await.unwrap_err();
        assert!(
            matches!(err, ReloadError::Parse { .. }),
            "expected Parse, got {err:?}"
        );
        // Live config unchanged.
        assert_eq!(live.read().await.log_level, "info");
    }

    #[tokio::test]
    async fn coordinator_missing_file_surfaces_io_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nexus.toml");
        std::fs::write(&path, "log_level = \"info\"\n").unwrap();
        let live = Arc::new(RwLock::new(Config::load_from_path(&path).unwrap()));
        let coord = ReloadCoordinator::new(path.clone(), Arc::clone(&live), noop_setter());
        std::fs::remove_file(&path).unwrap();
        let err = coord.reload().await.unwrap_err();
        assert!(matches!(err, ReloadError::Io { .. }), "got {err:?}");
    }

    #[tokio::test]
    async fn coordinator_semantic_invalid_surfaces_invalid_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nexus.toml");
        std::fs::write(&path, "log_level = \"info\"\n").unwrap();
        let live = Arc::new(RwLock::new(Config::load_from_path(&path).unwrap()));
        let coord = ReloadCoordinator::new(path.clone(), Arc::clone(&live), noop_setter());
        // bus_capacity = 0 is rejected by `Config::validate`.
        std::fs::write(&path, "bus_capacity = 0\n").unwrap();
        let err = coord.reload().await.unwrap_err();
        assert!(matches!(err, ReloadError::Invalid(_)), "got {err:?}");
        // Dbus maps this to InvalidArgument.
        assert!(matches!(
            err.to_dbus_error(),
            nexus_dbus::DbusError::InvalidArgument(_)
        ));
    }

    #[tokio::test]
    async fn reload_ops_forwards_unrelated_methods() {
        // When `ReloadOps` wraps an inner `BackendOps`, non-reload
        // methods should delegate straight through to the inner impl.
        use nexus_dbus::{NoopOps, PowerState};
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nexus.toml");
        std::fs::write(&path, "").unwrap();
        let live = Arc::new(RwLock::new(Config::load_from_path(&path).unwrap()));
        let coord = Arc::new(ReloadCoordinator::new(path, live, noop_setter()));
        let ops = ReloadOps::new(coord, NoopOps::arc());
        // NoopOps returns Unsupported for set_power_state, which
        // proves the call reached the inner impl.
        let err = ops.set_power_state(PowerState::Active).await.unwrap_err();
        assert!(matches!(err, nexus_dbus::DbusError::Unsupported(_)));
    }
}
