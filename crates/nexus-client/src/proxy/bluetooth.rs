//! `fi.nexus.Bluetooth` proxy (adapter). Read-only. DD-006 §6.4.

use zbus::zvariant::OwnedObjectPath;

#[zbus::proxy(interface = "fi.nexus.Bluetooth", default_service = "fi.nexus1")]
pub trait Bluetooth {
    #[zbus(property, name = "Address")]
    fn address(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Powered")]
    fn powered(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Discoverable")]
    fn discoverable(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Pairable")]
    fn pairable(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Discovering")]
    fn discovering(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "NexusDiscovering")]
    fn nexus_discovering(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "KnownDevices")]
    fn known_devices(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property, name = "State")]
    fn state(&self) -> zbus::Result<String>;
}
