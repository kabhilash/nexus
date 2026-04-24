//! D-Bus proxy layer (DD-008 §7.2).
//!
//! Two things live here:
//!
//! 1. [`ManagerOps`] — an async trait that captures every operation
//!    a read-only nexusctl command issues against `fi.nexus.*`.
//!    Command handlers depend on the trait, not on a concrete zbus
//!    proxy, so unit tests can pass a hand-rolled stub.
//!
//! 2. [`ZbusManagerOps`] — the production impl that actually talks
//!    to nexusd. It wraps the generated zbus proxies for every
//!    `fi.nexus.*` interface.
//!
//! Every trait method has a default impl that returns
//! `NexusctlError::Unsupported`, so per-command test stubs only
//! override the methods they exercise.

pub mod bluetooth;
pub mod bluetooth_device;
pub mod ethernet;
pub mod gnss;
pub mod interface;
pub mod manager;
pub mod profile;
pub mod scan_result;
pub mod wifi;
pub mod zbus_ops;

use async_trait::async_trait;
use serde::Serialize;

use crate::errors::NexusctlError;

pub use zbus_ops::ZbusManagerOps;

/// Snapshot of the daemon's overall state. Rendered by
/// `nexusctl status`. Mirrors the `Manager.GetManagerStatus()`
/// `a{sv}` dict from DD-006 §5.2, plus derived BlueZ / gpsd
/// availability flags (computed on the client side from the
/// interface list — the daemon doesn't yet surface them directly).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ManagerStatus {
    pub version: String,
    pub power_state: String,
    pub api_capabilities: Vec<String>,
    pub interface_count: u32,
    pub ethernet_count: u32,
    pub wifi_count: u32,
    pub bluetooth_count: u32,
    pub gnss_count: u32,
    pub wifi_profile_count: u32,
    pub ethernet_profile_count: u32,
    pub bluetooth_profile_count: u32,
    pub master_key_source: String,
    /// `true` when at least one bluetooth interface is reporting a
    /// non-`unavailable` state.
    pub bluez_available: bool,
    /// `true` when at least one GNSS interface's `GpsdConnected`
    /// property is true.
    pub gpsd_available: bool,
}

/// One row in `nexusctl iface list` (and the technology-scoped
/// list variants). Summarises the common `fi.nexus.Interface`
/// properties.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct InterfaceSummary {
    pub iface: String,
    pub kind: String,
    pub state: String,
    pub mac: Option<String>,
    pub carrier: bool,
    /// `"/"` means no profile attached. The state-prefix classifier
    /// (`state_prefix::classify`) uses this to decide whether to
    /// set the `A` (auto-configured) flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_profile: Option<String>,
}

/// Detailed record for `nexusctl iface show <iface>`. Carries the
/// common properties plus an optional per-kind detail blob.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct InterfaceDetail {
    #[serde(flatten)]
    pub summary: InterfaceSummary,
    pub mtu: Option<u32>,
    pub ifindex: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi: Option<WifiDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ethernet: Option<EthernetDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bluetooth: Option<BluetoothAdapterDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gnss: Option<GnssDetail>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WifiDetail {
    pub state: String,
    pub ssid: Option<String>,
    pub bssid: Option<String>,
    pub frequency_mhz: u32,
    pub signal_dbm: i32,
    pub security: String,
    pub supplicant: String,
    pub roaming_mode: String,
    pub powered: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EthernetDetail {
    pub state: String,
    pub auth_backend: String,
    pub auth_failure_reason: String,
    pub eap_method: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BluetoothAdapterDetail {
    pub address: String,
    pub powered: bool,
    pub discoverable: bool,
    pub pairable: bool,
    pub discovering: bool,
    pub nexus_discovering: bool,
    pub state: String,
    pub known_device_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GnssDetail {
    pub state: String,
    pub device_path: String,
    pub vendor_model: String,
    pub gpsd_connected: bool,
    pub satellites_in_view: u32,
    pub satellites_used: u32,
    pub horizontal_error_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fix: Option<GnssFix>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GnssFix {
    pub time_unix_ms: i64,
    pub mode: i32,
    pub latitude: f64,
    pub longitude: f64,
    pub altitude_m: f64,
    pub speed_mps: f64,
    pub track_deg: f64,
    pub horizontal_error_m: f64,
    pub vertical_error_m: f64,
    pub satellites_used: u32,
}

/// Placeholder row for `gnss satellites`. Nexusd doesn't yet expose
/// per-satellite detail on D-Bus (DD-005's `SatellitesChanged`
/// signal carries `in_view` + `used` scalars only), so this view
/// currently returns counts rather than per-satellite rows. The
/// structure is ready for the future richer surface.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GnssSatellitesView {
    pub device: String,
    pub in_view: u32,
    pub used: u32,
}

/// `nexusctl bt adapters` row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BluetoothAdapterSummary {
    pub ifname: String,
    pub address: String,
    pub state: String,
    pub powered: bool,
    pub discovering: bool,
    pub known_device_count: u32,
}

/// `nexusctl bt list` row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BluetoothDeviceSummary {
    pub adapter: String,
    pub address: String,
    pub name: String,
    pub state: String,
    pub paired: bool,
    pub bonded: bool,
    pub trusted: bool,
    pub connected: bool,
    pub rssi: i16,
    pub transport: String,
}

/// `nexusctl bt show` record.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct BluetoothDeviceDetail {
    #[serde(flatten)]
    pub summary: BluetoothDeviceSummary,
    pub address_type: String,
    pub alias: String,
    pub tx_power: i16,
    pub uuids: Vec<String>,
    pub blocked: bool,
    pub profile_path: Option<String>,
}

/// `nexusctl profile list` row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProfileSummary {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub credentials_invalid: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// `nexusctl profile show` record.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ProfileDetail {
    #[serde(flatten)]
    pub summary: ProfileSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wifi: Option<WifiProfileDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ethernet: Option<EthernetProfileDetail>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WifiProfileDetail {
    pub ssid: String,
    pub hidden: bool,
    pub priority: i32,
    pub auto_connect: bool,
    pub fast_transition: bool,
    pub security_type: String,
    pub has_credentials: Vec<String>,
    pub bssid_preferred: Option<String>,
    pub bssid_blacklist: Vec<String>,
    pub scan_frequencies: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EthernetProfileDetail {
    pub ifname: String,
    pub auto_connect: bool,
    pub dot1x_enabled: bool,
    pub dot1x_eap: String,
    pub has_credentials: Vec<String>,
}

/// `nexusctl admin master-key-info` record.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MasterKeyInfo {
    pub source: String,
}

/// One row of `nexusctl wifi scan` output.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WifiScanResult {
    pub ssid: String,
    pub bssid: String,
    pub frequency_mhz: u32,
    pub signal_dbm: i32,
    pub security: Vec<String>,
    pub age_ms: u64,
}

/// DD-008 §5 requires every mutating command to render confirmation
/// output. Most of them produce a tiny struct like this; the shared
/// view lets the output layer format consistently across commands.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct MutationOutcome {
    pub action: String,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `a{sv}` payload for `Manager.AddWifiProfile`. Only the handful of
/// fields nexusctl exposes today — DD-006 §16.2 has the full dict.
#[derive(Debug, Clone)]
pub struct WifiProfileSettings {
    pub ssid: Vec<u8>,
    pub security_type: String,
    pub passphrase: Option<String>,
    pub label: Option<String>,
    pub priority: Option<i32>,
    pub auto_connect: Option<bool>,
    pub hidden: Option<bool>,
    pub fast_transition: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct EthernetProfileSettings {
    pub ifname: String,
    pub label: Option<String>,
    pub auto_connect: Option<bool>,
}

/// Response from `Manager.ReloadConfig`. Same shape as DD-006 §5.2.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct ReloadConfigReport {
    pub applied: Vec<String>,
    pub deferred: Vec<String>,
    pub errors: Vec<(String, String)>,
}

/// Subset filter for `nexusctl bt list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BluetoothListFilter {
    All,
    Paired,
    Connected,
}

/// Every read-only operation a nexusctl command needs. Methods
/// default to `Unsupported` so per-command test stubs only override
/// what they exercise.
#[async_trait]
pub trait ManagerOps: Send + Sync {
    async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "get_manager_status".into(),
        })
    }
    async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "list_interfaces".into(),
        })
    }
    async fn show_interface(&self, _ifname: &str) -> Result<InterfaceDetail, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "show_interface".into(),
        })
    }
    async fn list_bluetooth_adapters(&self) -> Result<Vec<BluetoothAdapterSummary>, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "list_bluetooth_adapters".into(),
        })
    }
    async fn list_bluetooth_devices(
        &self,
        _filter: BluetoothListFilter,
    ) -> Result<Vec<BluetoothDeviceSummary>, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "list_bluetooth_devices".into(),
        })
    }
    async fn show_bluetooth_device(
        &self,
        _address: &str,
    ) -> Result<BluetoothDeviceDetail, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "show_bluetooth_device".into(),
        })
    }
    async fn gnss_satellites(
        &self,
        _device: Option<&str>,
    ) -> Result<GnssSatellitesView, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "gnss_satellites".into(),
        })
    }
    async fn list_profiles(
        &self,
        _kind: Option<&str>,
    ) -> Result<Vec<ProfileSummary>, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "list_profiles".into(),
        })
    }
    async fn show_profile(&self, _reference: &str) -> Result<ProfileDetail, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "show_profile".into(),
        })
    }
    async fn export_profile(&self, _reference: &str) -> Result<String, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "export_profile".into(),
        })
    }
    async fn master_key_info(&self) -> Result<MasterKeyInfo, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "master_key_info".into(),
        })
    }

    // ---- Mutating: wifi (DD-008 Phase 7.4 non-interactive subset) ----

    /// Issues `Wifi.Scan()` and returns the per-BSS rows now
    /// visible in `ScanResults`. The scan itself is asynchronous at
    /// the daemon level; phase-4 polls `ScanResults` for a short
    /// window after the call returns rather than subscribing to
    /// `ScanCompleted` — a future commit can tighten this once the
    /// signal wiring lands end-to-end.
    async fn wifi_scan(&self, _ifname: &str) -> Result<Vec<WifiScanResult>, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "wifi_scan".into(),
        })
    }

    /// Connect by stored profile object path.
    async fn wifi_connect_profile(
        &self,
        _ifname: &str,
        _profile_path: &str,
    ) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "wifi_connect_profile".into(),
        })
    }

    /// Disconnect the interface's current session.
    async fn wifi_disconnect(&self, _ifname: &str) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "wifi_disconnect".into(),
        })
    }

    /// Find a stored Wi-Fi profile by its SSID bytes. Returns the
    /// profile's object path, or `NotFound` when no profile matches.
    async fn find_wifi_profile(&self, _ssid: &[u8]) -> Result<String, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "find_wifi_profile".into(),
        })
    }

    // ---- Mutating: bluetooth ----

    async fn bt_set_powered(&self, _adapter: &str, _on: bool) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "bt_set_powered".into(),
        })
    }

    /// Runs a bounded-duration discovery session: `StartDiscovery`,
    /// sleep, `StopDiscovery`. Returns the known-device list after
    /// the session ends.
    async fn bt_scan(
        &self,
        _adapter: Option<&str>,
        _duration: std::time::Duration,
    ) -> Result<Vec<BluetoothDeviceSummary>, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "bt_scan".into(),
        })
    }

    async fn bt_connect_device(&self, _address: &str) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "bt_connect_device".into(),
        })
    }

    async fn bt_disconnect_device(&self, _address: &str) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "bt_disconnect_device".into(),
        })
    }

    async fn bt_forget_device(&self, _address: &str) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "bt_forget_device".into(),
        })
    }

    async fn bt_set_trusted(&self, _address: &str, _on: bool) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "bt_set_trusted".into(),
        })
    }

    // ---- Mutating: profiles ----

    /// Create a Wi-Fi profile from a settings dict. Returns the new
    /// profile's ULID as a string — the caller can use it for
    /// subsequent `connect-profile` / `update` calls.
    async fn add_wifi_profile(
        &self,
        _settings: WifiProfileSettings,
    ) -> Result<String, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "add_wifi_profile".into(),
        })
    }

    async fn add_ethernet_profile(
        &self,
        _settings: EthernetProfileSettings,
    ) -> Result<String, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "add_ethernet_profile".into(),
        })
    }

    /// Remove a profile by ULID or label. Same resolution rules as
    /// `show_profile`.
    async fn remove_profile(&self, _reference: &str) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "remove_profile".into(),
        })
    }

    /// Update a top-level field on a profile. `field` is the key as
    /// it appears in the profile's `a{sv}` settings dict (e.g.,
    /// `"auto_connect"`, `"label"`). `value` is the raw string the
    /// operator supplied — handlers parse it against the expected
    /// type.
    async fn update_profile_field(
        &self,
        _reference: &str,
        _field: &str,
        _value: &str,
    ) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "update_profile_field".into(),
        })
    }

    // ---- Manager-level admin operations ----

    async fn set_power_state(&self, _state: &str) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "set_power_state".into(),
        })
    }

    /// Fire-and-forget. Returns the job id; the outcome arrives via
    /// the `MasterKeyRotated` signal which operators watch with
    /// `nexusctl watch`.
    async fn rotate_master_key(&self) -> Result<String, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "rotate_master_key".into(),
        })
    }

    async fn freeze_for_backup(&self) -> Result<String, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "freeze_for_backup".into(),
        })
    }

    async fn release_backup_lease(&self, _lease: &str) -> Result<(), NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "release_backup_lease".into(),
        })
    }

    async fn reload_config(&self) -> Result<ReloadConfigReport, NexusctlError> {
        Err(NexusctlError::Unsupported {
            detail: "reload_config".into(),
        })
    }
}
