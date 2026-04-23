//! Profile objects. See DD-006 §7.

pub mod common;
pub mod ethernet;
pub mod wifi;

pub use common::ProfileIface;
pub use ethernet::EthernetProfileIface;
pub use wifi::WifiProfileIface;

/// Interface names for ObjectManager surfacing.
pub mod iface_names {
    pub const COMMON: &str = "fi.nexus.Profile";
    pub const WIFI: &str = "fi.nexus.Profile.Wifi";
    pub const ETHERNET: &str = "fi.nexus.Profile.Ethernet";
}

/// Kind enum used by the common profile to report its flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileKind {
    Wifi,
    Ethernet,
}

impl ProfileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileKind::Wifi => "wifi",
            ProfileKind::Ethernet => "ethernet",
        }
    }
}
