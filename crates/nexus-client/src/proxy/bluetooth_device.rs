//! `fi.nexus.BluetoothDevice` proxy. Read-only. DD-006 §6.6.

use std::collections::HashMap;

use zbus::zvariant::OwnedObjectPath;

#[zbus::proxy(interface = "fi.nexus.BluetoothDevice", default_service = "fi.nexus1")]
pub trait BluetoothDevice {
    #[zbus(property, name = "Address")]
    fn address(&self) -> zbus::Result<String>;

    #[zbus(property, name = "AddressType")]
    fn address_type(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Transport")]
    fn transport(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Name")]
    fn name(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Alias")]
    fn alias(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Rssi")]
    fn rssi(&self) -> zbus::Result<i16>;

    #[zbus(property, name = "TxPower")]
    fn tx_power(&self) -> zbus::Result<i16>;

    #[zbus(property, name = "Uuids")]
    fn uuids(&self) -> zbus::Result<Vec<String>>;

    #[zbus(property, name = "ManufacturerData")]
    fn manufacturer_data(&self) -> zbus::Result<HashMap<u16, Vec<u8>>>;

    #[zbus(property, name = "State")]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Paired")]
    fn paired(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Bonded")]
    fn bonded(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Trusted")]
    fn trusted(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Blocked")]
    fn blocked(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Connected")]
    fn connected(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Adapter")]
    fn adapter(&self) -> zbus::Result<OwnedObjectPath>;

    #[zbus(property, name = "Profile")]
    fn profile(&self) -> zbus::Result<OwnedObjectPath>;

    /// `Connect() -> ()` — shortcut for Bluetooth.Connect(this).
    #[zbus(name = "Connect")]
    fn connect(&self) -> zbus::Result<()>;

    /// `Disconnect() -> ()`.
    #[zbus(name = "Disconnect")]
    fn disconnect(&self) -> zbus::Result<()>;

    /// `Forget() -> ()`.
    #[zbus(name = "Forget")]
    fn forget(&self) -> zbus::Result<()>;

    /// `Trusted` is read/write — property setter.
    #[zbus(property, name = "Trusted")]
    fn set_trusted(&self, on: bool) -> zbus::Result<()>;
}
