//! `fi.nexus.Manager` zbus proxy. See DD-006 §5.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

#[zbus::proxy(
    interface = "fi.nexus.Manager",
    default_service = "fi.nexus1",
    default_path = "/fi/nexus1"
)]
pub trait Manager {
    /// `Interfaces` property: object paths of every interface
    /// nexusd has discovered.
    #[zbus(property)]
    fn interfaces(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    /// `Version` property — daemon version string.
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;

    /// `PowerState` property.
    #[zbus(property, name = "PowerState")]
    fn power_state(&self) -> zbus::Result<String>;

    /// `MasterKeySource` property.
    #[zbus(property, name = "MasterKeySource")]
    fn master_key_source(&self) -> zbus::Result<String>;

    /// `GetManagerStatus()` — convenience snapshot used by
    /// `nexusctl status`. Returns the variant dict described in
    /// DD-006 §5.2.
    #[zbus(name = "GetManagerStatus")]
    fn get_manager_status(&self) -> zbus::Result<HashMap<String, OwnedValue>>;
}
