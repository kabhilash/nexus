//! `nexus.toml` — the daemon's single configuration file.
//!
//! Every subsystem has its own section with an `enabled` flag; the
//! daemon honours it to skip spawning. Keys that correspond to
//! backend-specific tunables (retry policies, gpsd endpoint, …) are
//! mirrored from the per-DD config structs but kept simple here —
//! the daemon applies them at spawn time.
//!
//! This module intentionally uses plain `serde` rather than an
//! Figment-style layered loader: the architecture doc calls for a
//! single config path with sensible defaults, nothing more.

use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use thiserror::Error;

/// Errors surfaced during config loading / validation.
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("parsing {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("config validation failed: {0}")]
    Invalid(String),
}

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Broadcast channel capacity for [`nexus_core::NexusEvent`].
    pub bus_capacity: usize,
    /// Tracing log level (one of `"error"`, `"warn"`, `"info"`,
    /// `"debug"`, `"trace"`).
    pub log_level: String,
    #[serde(rename = "supervision")]
    pub supervision: SupervisionSection,
    #[serde(rename = "interface_monitor")]
    pub interface_monitor: InterfaceMonitorSection,
    #[serde(rename = "profile_store")]
    pub profile_store: ProfileStoreSection,
    #[serde(rename = "dbus")]
    pub dbus: DbusSection,
    #[serde(rename = "ethernet")]
    pub ethernet: EthernetSection,
    #[serde(rename = "wifi")]
    pub wifi: WifiSection,
    #[serde(rename = "bluetooth")]
    pub bluetooth: BluetoothSection,
    #[serde(rename = "gnss")]
    pub gnss: GnssSection,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bus_capacity: 256,
            log_level: "info".to_owned(),
            supervision: SupervisionSection::default(),
            interface_monitor: InterfaceMonitorSection::default(),
            profile_store: ProfileStoreSection::default(),
            dbus: DbusSection::default(),
            ethernet: EthernetSection::default(),
            wifi: WifiSection::default(),
            bluetooth: BluetoothSection::default(),
            gnss: GnssSection::default(),
        }
    }
}

impl Config {
    /// Load + validate a config file. Missing sections fall back to
    /// their `Default`. Unknown keys are rejected.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_owned(),
            source: e,
        })?;
        let cfg: Config = toml::from_str(&text).map_err(|e| ConfigError::Parse {
            path: path.to_owned(),
            source: e,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse + validate from a string slice. Useful in tests.
    pub fn parse_str(text: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(text).map_err(|e| ConfigError::Parse {
            path: PathBuf::from("<inline>"),
            source: e,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Subsystems whose `enabled = true`. Order matches the
    /// architecture doc's spawn sequence (discovery first, D-Bus
    /// last so it can hydrate from interfaces already published).
    /// Used by the daemon's supervisor loop and exercised by the
    /// "disabled subsystem emits no events" integration test.
    pub fn enabled_subsystems(&self) -> Vec<crate::supervision::SubsystemName> {
        use crate::supervision::SubsystemName as S;
        let mut out = Vec::new();
        if self.interface_monitor.enabled {
            out.push(S::InterfaceMonitor);
        }
        if self.ethernet.enabled {
            out.push(S::Ethernet);
        }
        if self.wifi.enabled {
            out.push(S::Wifi);
        }
        if self.bluetooth.enabled {
            out.push(S::Bluetooth);
        }
        if self.gnss.enabled {
            out.push(S::Gnss);
        }
        if self.dbus.enabled {
            out.push(S::Dbus);
        }
        out
    }

    /// Validate invariants that TOML's type system can't express
    /// (non-zero capacities, known log level, …).
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.bus_capacity == 0 {
            return Err(ConfigError::Invalid("bus_capacity must be > 0".to_owned()));
        }
        match self.log_level.as_str() {
            "error" | "warn" | "info" | "debug" | "trace" => {}
            other => {
                return Err(ConfigError::Invalid(format!(
                    "log_level must be one of error/warn/info/debug/trace, got {other:?}"
                )));
            }
        }
        match self.wifi.backend.as_str() {
            "wpa_supplicant" | "iwd" | "mock" => {}
            other => {
                return Err(ConfigError::Invalid(format!(
                    "wifi.backend must be wpa_supplicant / iwd / mock, got {other:?}"
                )));
            }
        }
        match self.ethernet.auth_backend.as_str() {
            "wpa_supplicant" | "ead" | "none" => {}
            other => {
                return Err(ConfigError::Invalid(format!(
                    "ethernet.auth_backend must be wpa_supplicant / ead / none, got {other:?}"
                )));
            }
        }
        if self.dbus.bus_name.is_empty() {
            return Err(ConfigError::Invalid(
                "dbus.bus_name must not be empty".into(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Section: supervision (§5)
// ---------------------------------------------------------------------------

/// Supervisor behaviour for subsystem crashes.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SupervisionSection {
    /// If true, a crashed subsystem is restarted with exponential
    /// backoff. If false, a crash is logged and the subsystem stays
    /// down until the daemon restarts.
    pub restart: bool,
    /// Starting backoff for the first restart attempt.
    #[serde(with = "duration_secs_f")]
    pub restart_initial_backoff: Duration,
    /// Cap on the exponential backoff.
    #[serde(with = "duration_secs_f")]
    pub restart_max_backoff: Duration,
    /// Multiplier between successive backoffs.
    pub restart_multiplier: f64,
}

impl Default for SupervisionSection {
    fn default() -> Self {
        Self {
            restart: true,
            restart_initial_backoff: Duration::from_millis(500),
            restart_max_backoff: Duration::from_secs(30),
            restart_multiplier: 2.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Section: interface_monitor (DD-001)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InterfaceMonitorSection {
    pub enabled: bool,
}

impl Default for InterfaceMonitorSection {
    fn default() -> Self {
        Self { enabled: true }
    }
}

// ---------------------------------------------------------------------------
// Section: profile_store (DD-007)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileStoreSection {
    pub root: PathBuf,
    /// Master-key source. One of `"file"` (`/var/lib/nexus/master.key`),
    /// `"in_memory"` (derived from `in_memory_seed`; dev only), or
    /// `"tpm"` (build with `--features tpm`).
    pub key_source: String,
    /// 32-byte lowercase hex seed for the `in_memory` key source.
    /// Ignored otherwise.
    pub in_memory_seed: Option<String>,
}

impl Default for ProfileStoreSection {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/var/lib/nexus"),
            key_source: "file".to_owned(),
            in_memory_seed: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Section: dbus (DD-006)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DbusSection {
    pub enabled: bool,
    pub bus_name: String,
    /// `true` → session bus (for development). `false` → system bus.
    pub use_session_bus: bool,
    /// When set, overrides `use_session_bus`. Used by tests with a
    /// private `dbus-daemon`.
    pub address: Option<String>,
    /// Scan / Connect / ProfileWrite / Admin limits per minute per
    /// D-Bus sender. Defaults mirror DD-006 §15.
    pub rate_limit_scan_per_min: u32,
    pub rate_limit_connect_per_min: u32,
    pub rate_limit_profile_write_per_min: u32,
    pub rate_limit_admin_per_min: u32,
    pub rate_limit_property_read_per_min: u32,
    /// When true, always allow every method (bypasses PolicyKit).
    /// Intended only for development and in-process integration
    /// tests. Production always sets this to false so the daemon
    /// talks to org.freedesktop.PolicyKit1.
    pub allow_all_authz: bool,
}

impl Default for DbusSection {
    fn default() -> Self {
        Self {
            enabled: true,
            bus_name: "fi.nexus1".to_owned(),
            use_session_bus: false,
            address: None,
            rate_limit_scan_per_min: 10,
            rate_limit_connect_per_min: 30,
            rate_limit_profile_write_per_min: 30,
            rate_limit_admin_per_min: 1,
            rate_limit_property_read_per_min: 1000,
            allow_all_authz: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Section: ethernet (DD-002)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EthernetSection {
    pub enabled: bool,
    /// Which 802.1X backend to use (`"wpa_supplicant"`, `"ead"`, or
    /// `"none"` to disable wired auth).
    pub auth_backend: String,
    #[serde(with = "duration_secs_f")]
    pub retry_initial: Duration,
    #[serde(with = "duration_secs_f")]
    pub retry_max: Duration,
    pub retry_multiplier: f64,
    pub retry_max_attempts: u32,
}

impl Default for EthernetSection {
    fn default() -> Self {
        Self {
            enabled: true,
            auth_backend: "wpa_supplicant".to_owned(),
            retry_initial: Duration::from_millis(500),
            retry_max: Duration::from_secs(30),
            retry_multiplier: 2.0,
            retry_max_attempts: 8,
        }
    }
}

// ---------------------------------------------------------------------------
// Section: wifi (DD-003)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WifiSection {
    pub enabled: bool,
    /// `"wpa_supplicant"` (default), `"iwd"`, or `"mock"` (test-only).
    pub backend: String,
    /// One of `"off"`, `"supplicant"`, `"nexus"`.
    pub roam_mode: String,
    #[serde(with = "duration_secs_f")]
    pub signal_poll_interval: Duration,
    #[serde(with = "duration_secs_f")]
    pub disconnect_cool_down: Duration,
    /// Supplicant-event-channel capacity. Separate from the shared
    /// NexusEvent bus because the supplicant driver needs its own
    /// multiplexing.
    pub supplicant_event_capacity: usize,
}

impl Default for WifiSection {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: "wpa_supplicant".to_owned(),
            roam_mode: "supplicant".to_owned(),
            signal_poll_interval: Duration::from_secs(5),
            disconnect_cool_down: Duration::from_secs(2),
            supplicant_event_capacity: 128,
        }
    }
}

// ---------------------------------------------------------------------------
// Section: bluetooth (DD-004)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BluetoothSection {
    pub enabled: bool,
    /// Use the `mock` BlueZ client instead of real zbus.
    pub mock: bool,
    pub pairing_timeout_s: u32,
    pub agent_response_timeout_s: u32,
    pub discovery_timeout_s: u32,
    pub discovery_device_ttl_s: u32,
    pub bluez_outage_notify_s: u32,
    pub auto_power_on_startup: bool,
    pub register_agent: bool,
}

impl Default for BluetoothSection {
    fn default() -> Self {
        Self {
            enabled: true,
            mock: false,
            pairing_timeout_s: 60,
            agent_response_timeout_s: 45,
            discovery_timeout_s: 30,
            discovery_device_ttl_s: 300,
            bluez_outage_notify_s: 60,
            auto_power_on_startup: true,
            register_agent: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Section: gnss (DD-005)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GnssSection {
    pub enabled: bool,
    /// Use the mock gpsd client instead of connecting to a real
    /// gpsd.
    pub mock: bool,
    pub gpsd_endpoint: SocketAddr,
    pub acquisition_timeout_s: u32,
    pub tpv_stall_timeout_s: u32,
    pub gpsd_outage_notify_s: u32,
}

impl Default for GnssSection {
    fn default() -> Self {
        Self {
            enabled: false,
            mock: false,
            gpsd_endpoint: ([127, 0, 0, 1], 2947).into(),
            acquisition_timeout_s: 300,
            tpv_stall_timeout_s: 30,
            gpsd_outage_notify_s: 60,
        }
    }
}

// ---------------------------------------------------------------------------
// Serde helpers
// ---------------------------------------------------------------------------

/// Deserialize a `Duration` from fractional seconds. Only the
/// deserialization half is exercised today; the config is not
/// round-tripped back to TOML.
mod duration_secs_f {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D>(d: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let secs = f64::deserialize(d)?;
        if secs < 0.0 {
            return Err(serde::de::Error::custom("duration must be non-negative"));
        }
        Ok(Duration::from_secs_f64(secs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let cfg = Config::parse_str("").unwrap();
        assert_eq!(cfg.bus_capacity, 256);
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.supervision.restart);
        assert!(cfg.interface_monitor.enabled);
        assert!(cfg.dbus.enabled);
        assert!(cfg.ethernet.enabled);
        assert!(cfg.wifi.enabled);
        assert!(cfg.bluetooth.enabled);
        assert!(!cfg.gnss.enabled);
    }

    #[test]
    fn every_section_round_trips() {
        let text = r#"
            bus_capacity = 512
            log_level = "debug"

            [supervision]
            restart = false
            restart_initial_backoff = 1.0
            restart_max_backoff = 60.0
            restart_multiplier = 3.0

            [interface_monitor]
            enabled = false

            [profile_store]
            root = "/tmp/nexus"
            key_source = "in_memory"
            in_memory_seed = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"

            [dbus]
            enabled = true
            bus_name = "fi.nexus1"
            use_session_bus = true
            rate_limit_scan_per_min = 5
            rate_limit_connect_per_min = 10
            rate_limit_profile_write_per_min = 15
            rate_limit_admin_per_min = 2
            rate_limit_property_read_per_min = 500
            allow_all_authz = true

            [ethernet]
            enabled = true
            auth_backend = "none"
            retry_initial = 0.25
            retry_max = 10.0
            retry_multiplier = 1.5
            retry_max_attempts = 16

            [wifi]
            enabled = true
            backend = "mock"
            roam_mode = "nexus"
            signal_poll_interval = 2.0
            disconnect_cool_down = 1.5
            supplicant_event_capacity = 64

            [bluetooth]
            enabled = false
            mock = true
            pairing_timeout_s = 90
            agent_response_timeout_s = 60
            discovery_timeout_s = 45
            discovery_device_ttl_s = 600
            bluez_outage_notify_s = 120
            auto_power_on_startup = false
            register_agent = false

            [gnss]
            enabled = true
            mock = true
            gpsd_endpoint = "127.0.0.1:3947"
            acquisition_timeout_s = 600
            tpv_stall_timeout_s = 60
            gpsd_outage_notify_s = 120
        "#;
        let cfg = Config::parse_str(text).unwrap();
        assert_eq!(cfg.bus_capacity, 512);
        assert_eq!(cfg.log_level, "debug");
        assert!(!cfg.supervision.restart);
        assert_eq!(
            cfg.supervision.restart_initial_backoff,
            Duration::from_secs(1)
        );
        assert_eq!(cfg.supervision.restart_max_backoff, Duration::from_secs(60));
        assert!((cfg.supervision.restart_multiplier - 3.0).abs() < f64::EPSILON);
        assert!(!cfg.interface_monitor.enabled);
        assert_eq!(cfg.profile_store.root, PathBuf::from("/tmp/nexus"));
        assert_eq!(cfg.profile_store.key_source, "in_memory");
        assert!(cfg.dbus.use_session_bus);
        assert!(cfg.dbus.allow_all_authz);
        assert_eq!(cfg.dbus.rate_limit_scan_per_min, 5);
        assert_eq!(cfg.ethernet.auth_backend, "none");
        assert_eq!(cfg.ethernet.retry_initial, Duration::from_millis(250));
        assert_eq!(cfg.ethernet.retry_max_attempts, 16);
        assert_eq!(cfg.wifi.backend, "mock");
        assert_eq!(cfg.wifi.roam_mode, "nexus");
        assert_eq!(cfg.wifi.signal_poll_interval, Duration::from_secs(2));
        assert_eq!(cfg.wifi.supplicant_event_capacity, 64);
        assert!(!cfg.bluetooth.enabled);
        assert!(cfg.bluetooth.mock);
        assert!(cfg.gnss.enabled);
        assert!(cfg.gnss.mock);
        assert_eq!(
            cfg.gnss.gpsd_endpoint,
            "127.0.0.1:3947".parse::<SocketAddr>().unwrap()
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = Config::parse_str("mystery = 42").unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn invalid_log_level_rejected() {
        let text = r#"log_level = "spam""#;
        let err = Config::parse_str(text).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn zero_bus_capacity_rejected() {
        let text = "bus_capacity = 0";
        let err = Config::parse_str(text).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn invalid_wifi_backend_rejected() {
        let text = r#"
            [wifi]
            backend = "quantum"
        "#;
        let err = Config::parse_str(text).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn invalid_ethernet_auth_rejected() {
        let text = r#"
            [ethernet]
            auth_backend = "radius"
        "#;
        let err = Config::parse_str(text).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)));
    }

    #[test]
    fn load_from_path_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nexus.toml");
        std::fs::write(&path, "bus_capacity = 17\n").unwrap();
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.bus_capacity, 17);
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let err = Config::load_from_path("/nonexistent/nexus.toml").unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }
}
