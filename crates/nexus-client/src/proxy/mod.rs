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
}
