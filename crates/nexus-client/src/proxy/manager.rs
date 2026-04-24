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

    /// `GetInterface(ifname: s) -> (path: o)`. Returns
    /// `fi.nexus.Error.NotFound` on an unknown name.
    #[zbus(name = "GetInterface")]
    fn get_interface(&self, ifname: &str) -> zbus::Result<OwnedObjectPath>;

    /// `GetManagerStatus() -> a{sv}` — DD-006 §5.2 snapshot.
    #[zbus(name = "GetManagerStatus")]
    fn get_manager_status(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}
