//! `fi.nexus.Manager` zbus proxy. See DD-006 §5.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

#[zbus::proxy(
    interface = "fi.nexus.Manager",
    default_service = "fi.nexus1",
    default_path = "/fi/nexus1"
)]
pub trait Manager {
    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;

    #[zbus(property, name = "PowerState")]
    fn power_state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "ApiCapabilities")]
    fn api_capabilities(&self) -> zbus::Result<Vec<String>>;

    #[zbus(property, name = "WifiProfiles")]
    fn wifi_profiles(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property, name = "EthernetProfiles")]
    fn ethernet_profiles(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property, name = "MasterKeySource")]
    fn master_key_source(&self) -> zbus::Result<String>;

    /// Last observed internet-reachability state from the daemon's
    /// connectivity probe. One of `internetUnknown`, `internetOnline`,
    /// `internetCaptivePortal`, `internetOffline`.
    #[zbus(property, name = "InternetConnectivity")]
    fn internet_connectivity(&self) -> zbus::Result<String>;

    /// `GetInterface(ifname: s) -> (path: o)`. Returns
    /// `fi.nexus.Error.NotFound` on an unknown name.
    #[zbus(name = "GetInterface")]
    fn get_interface(&self, ifname: &str) -> zbus::Result<OwnedObjectPath>;

    /// `GetManagerStatus() -> a{sv}` — DD-006 §5.2 snapshot.
    #[zbus(name = "GetManagerStatus")]
    fn get_manager_status(&self) -> zbus::Result<HashMap<String, OwnedValue>>;

    /// `FindWifiProfile(ssid: ay) -> (path: o)`.
    #[zbus(name = "FindWifiProfile")]
    fn find_wifi_profile(&self, ssid: &[u8]) -> zbus::Result<OwnedObjectPath>;

    /// `AddWifiProfile(settings: a{sv}) -> (path: o)`.
    #[zbus(name = "AddWifiProfile")]
    fn add_wifi_profile(
        &self,
        settings: HashMap<String, OwnedValue>,
    ) -> zbus::Result<OwnedObjectPath>;

    /// `AddEthernetProfile(settings: a{sv}) -> (path: o)`.
    #[zbus(name = "AddEthernetProfile")]
    fn add_ethernet_profile(
        &self,
        settings: HashMap<String, OwnedValue>,
    ) -> zbus::Result<OwnedObjectPath>;

    /// `RemoveProfile(path: o) -> ()`.
    #[zbus(name = "RemoveProfile")]
    fn remove_profile(&self, path: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

    /// `SetPowerState(state: s) -> ()`.
    #[zbus(name = "SetPowerState")]
    fn set_power_state(&self, state: &str) -> zbus::Result<()>;

    /// `RotateMasterKey() -> (job_id: s)`.
    #[zbus(name = "RotateMasterKey")]
    fn rotate_master_key(&self) -> zbus::Result<String>;

    /// `FreezeForBackup() -> (lease: s)`.
    #[zbus(name = "FreezeForBackup")]
    fn freeze_for_backup(&self) -> zbus::Result<String>;

    /// `ReleaseBackupLease(lease: s) -> ()`.
    #[zbus(name = "ReleaseBackupLease")]
    fn release_backup_lease(&self, lease: &str) -> zbus::Result<()>;

    /// `ReloadConfig() -> a{sv}`.
    #[zbus(name = "ReloadConfig")]
    fn reload_config(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}
