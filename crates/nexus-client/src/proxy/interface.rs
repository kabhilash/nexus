//! `fi.nexus.Interface` — the common base interface. DD-006 §6.1.

use zbus::zvariant::OwnedObjectPath;

#[zbus::proxy(interface = "fi.nexus.Interface", default_service = "fi.nexus1")]
pub trait Interface {
    #[zbus(property, name = "Ifname")]
    fn ifname(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Ifindex")]
    fn ifindex(&self) -> zbus::Result<u32>;

    #[zbus(property, name = "Kind")]
    fn kind(&self) -> zbus::Result<String>;

    #[zbus(property, name = "OperState")]
    fn oper_state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Carrier")]
    fn carrier(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Mac")]
    fn mac(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property, name = "ManagedProfile")]
    fn managed_profile(&self) -> zbus::Result<OwnedObjectPath>;
}
