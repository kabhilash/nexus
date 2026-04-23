//! Per-technology interface objects. See DD-006 §6.
//!
//! Each interface is implemented as a thin wrapper around the
//! shared [`crate::state::State`]. Phase 1-3 is read-only; method
//! names are reserved (DD-006 §6.3/§6.4 etc.) but not yet
//! implemented.

pub mod bluetooth;
pub mod bluetooth_device;
pub mod common;
pub mod ethernet;
pub mod gnss;
pub mod wifi;

pub use bluetooth::BluetoothIface;
pub use bluetooth_device::BluetoothDeviceIface;
pub use common::InterfaceIface;
pub use ethernet::EthernetIface;
pub use gnss::GnssIface;
pub use wifi::WifiIface;

/// Interface names used when emitting ObjectManager entries.
pub mod iface_names {
    pub const COMMON: &str = "fi.nexus.Interface";
    pub const ETHERNET: &str = "fi.nexus.Ethernet";
    pub const WIFI: &str = "fi.nexus.Wifi";
    pub const BLUETOOTH: &str = "fi.nexus.Bluetooth";
    pub const GNSS: &str = "fi.nexus.Gnss";
    pub const BLUETOOTH_DEVICE: &str = "fi.nexus.BluetoothDevice";
}
