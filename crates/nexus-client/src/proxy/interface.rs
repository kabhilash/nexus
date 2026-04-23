//! `fi.nexus.Interface` (the common base interface from DD-006 §6.1).
//!
//! `default_service` is fixed to `fi.nexus1`; `path` varies per
//! object and is set at proxy construction time.

#[zbus::proxy(interface = "fi.nexus.Interface", default_service = "fi.nexus1")]
pub trait Interface {
    #[zbus(property, name = "Ifname")]
    fn ifname(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Kind")]
    fn kind(&self) -> zbus::Result<String>;

    #[zbus(property, name = "OperState")]
    fn oper_state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Carrier")]
    fn carrier(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Mac")]
    fn mac(&self) -> zbus::Result<Vec<u8>>;
}
