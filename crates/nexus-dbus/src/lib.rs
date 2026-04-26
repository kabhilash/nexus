//! Nexus D-Bus surface. See `dd-006-dbus-api.md`.
//!
//! Phase 1-6 scope: service skeleton, Manager + per-technology +
//! profile objects (read-only), PolicyKit-gated mutating methods
//! for Wi-Fi (Scan / Connect / Disconnect / Roam / SetPowered /
//! SetRoamingMode), Manager mutations (AddWifiProfile,
//! AddEthernetProfile, RemoveProfile, SetPowerState), and
//! Profile.Update / Profile.Delete.

pub mod authz;
pub mod backend_ops;
pub mod errors;
pub mod interfaces;
pub mod manager;
pub mod object_manager;
pub mod paths;
pub mod profiles;
pub mod properties;
pub mod rate_limit;
pub mod scan_results;
pub mod service;
pub mod services;
pub mod state;
pub mod wifi_jobs;

pub use authz::{
    AlwaysAllowChecker, AlwaysDenyChecker, AuthChecker, AuthDecision, PolicyKitChecker,
    PolicyMapChecker, actions, always_allow, always_deny,
};
pub use backend_ops::{
    BackendOps, NoopOps, RecordedCall, RecordingOps, ReloadReport, RoamingMode, ScanParams,
};
pub use errors::{DbusError, Result};
pub use interfaces::decode_pairing_answer;
pub use manager::Manager;
pub use object_manager::ObjectManager;
pub use paths::{
    MANAGER_PATH, bluetooth_device_path, bluetooth_profile_path, escape_component,
    ethernet_profile_path, interface_path, scan_result_path, wifi_profile_path,
};
pub use properties::{COALESCE_WINDOW, PropertyBatcher};
pub use rate_limit::{OpClass, RateLimiter, RateLimits};
pub use scan_results::ScanResultIface;
pub use service::{DbusConfig, DbusServiceHandle, ServiceCommand, spawn_dbus_service};
pub use services::{EnabledFeatures, Feature, Services};
pub use state::{
    BluetoothAdapterState, BtDeviceState, EthernetState, GnssState, InterfaceKindData,
    InterfaceState, PowerState, State, WifiInterfaceState,
};
