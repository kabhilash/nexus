//! `fi.nexus.Ethernet` proxy — read-only. DD-006 §6.2.

#[zbus::proxy(interface = "fi.nexus.Ethernet", default_service = "fi.nexus1")]
pub trait Ethernet {
    #[zbus(property, name = "State")]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "AuthBackend")]
    fn auth_backend(&self) -> zbus::Result<String>;

    #[zbus(property, name = "AuthFailureReason")]
    fn auth_failure_reason(&self) -> zbus::Result<String>;

    #[zbus(property, name = "EapMethod")]
    fn eap_method(&self) -> zbus::Result<String>;
}
