//! zbus proxies for the BlueZ D-Bus surface Nexus uses. See DD-004
//! §6.1, §6.2. We hand-write these rather than pulling in `bluer`
//! or similar: the subset we need is tiny and stable across BlueZ
//! 5.50-5.72.
//!
//! Not every BlueZ method is wrapped — the trait in
//! `crate::bluez::mod` defines the operational vocabulary, and the
//! proxies here only need to cover that subset plus the
//! ObjectManager signal surface.

use std::collections::HashMap;

use zbus::proxy;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

/// BlueZ's `org.bluez.Adapter1`. Documented in
/// `doc/adapter-api.txt` in the BlueZ source tree.
#[proxy(interface = "org.bluez.Adapter1", default_service = "org.bluez")]
pub trait Adapter1 {
    /// Kick off a discovery session. Idempotent at BlueZ's level:
    /// repeated calls from the same sender stack up but only the
    /// first has effect on the radio.
    fn start_discovery(&self) -> zbus::Result<()>;

    /// Stop this sender's discovery session. If others are still
    /// active, BlueZ keeps the adapter discovering.
    fn stop_discovery(&self) -> zbus::Result<()>;

    /// Apply a discovery filter dict before `StartDiscovery`. Keys:
    /// `Transport`, `RSSI`, `UUIDs`, `DuplicateData`, `Pathloss`,
    /// `DiscoverableTimeout`. See DD-004 §9.2 for the ones we use.
    fn set_discovery_filter(&self, filter: HashMap<String, Value<'_>>) -> zbus::Result<()>;

    /// Drop a device from BlueZ's registry (unpair).
    fn remove_device(&self, device: &ObjectPath<'_>) -> zbus::Result<()>;

    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn powered(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_powered(&self, on: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn discoverable(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_discoverable(&self, on: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn pairable(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_pairable(&self, on: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn discovering(&self) -> zbus::Result<bool>;
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> zbus::Result<Vec<String>>;
}

/// BlueZ's `org.bluez.Device1`. Documented in
/// `doc/device-api.txt`.
#[proxy(interface = "org.bluez.Device1", default_service = "org.bluez")]
pub trait Device1 {
    fn connect(&self) -> zbus::Result<()>;
    fn disconnect(&self) -> zbus::Result<()>;
    fn pair(&self) -> zbus::Result<()>;
    fn cancel_pairing(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn address(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn address_type(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn alias(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn paired(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn bonded(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn trusted(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn set_trusted(&self, on: bool) -> zbus::Result<()>;
    #[zbus(property)]
    fn blocked(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn connected(&self) -> zbus::Result<bool>;
    /// RSSI in dBm. Optional; BlueZ omits the property if the peer
    /// hasn't been heard from recently.
    #[zbus(property, name = "RSSI")]
    fn rssi(&self) -> zbus::Result<i16>;
    #[zbus(property, name = "TxPower")]
    fn tx_power(&self) -> zbus::Result<i16>;
    #[zbus(property, name = "UUIDs")]
    fn uuids(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn adapter(&self) -> zbus::Result<OwnedObjectPath>;
    /// ManufacturerData is `a{qay}` (uint16 keyed, byte-array value).
    #[zbus(property)]
    fn manufacturer_data(&self) -> zbus::Result<HashMap<u16, Vec<u8>>>;
}

/// BlueZ's `org.bluez.AgentManager1` — used at startup to register
/// the Nexus Agent (DD-004 §8.1).
#[proxy(
    interface = "org.bluez.AgentManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez"
)]
pub trait AgentManager1 {
    fn register_agent(&self, agent: &ObjectPath<'_>, capability: &str) -> zbus::Result<()>;
    fn unregister_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
    fn request_default_agent(&self, agent: &ObjectPath<'_>) -> zbus::Result<()>;
}

/// Standard `org.freedesktop.DBus.ObjectManager`. BlueZ exposes the
/// whole `/org/bluez` tree through this interface; Nexus calls
/// `GetManagedObjects` on first connect and subscribes to
/// `InterfacesAdded` / `InterfacesRemoved` afterwards.
///
/// `a{oa{sa{sv}}}` is the published shape: a map from object path
/// to a map from interface name to property-bag.
#[proxy(
    interface = "org.freedesktop.DBus.ObjectManager",
    default_service = "org.bluez",
    default_path = "/"
)]
pub trait ObjectManager {
    fn get_managed_objects(&self) -> zbus::Result<ManagedObjects>;

    #[zbus(signal)]
    fn interfaces_added(
        &self,
        object: OwnedObjectPath,
        interfaces: InterfacesMap,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    fn interfaces_removed(
        &self,
        object: OwnedObjectPath,
        interfaces: Vec<String>,
    ) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Shared type aliases used by the proxies above.
// ---------------------------------------------------------------------------

/// Map from interface name → property bag (`a{sa{sv}}`). Used by
/// `GetManagedObjects` and `InterfacesAdded`.
pub type InterfacesMap = HashMap<String, HashMap<String, OwnedValue>>;

/// Full `GetManagedObjects` return (`a{oa{sa{sv}}}`).
pub type ManagedObjects = HashMap<OwnedObjectPath, InterfacesMap>;

/// Well-known BlueZ interface names we inspect.
pub mod iface {
    pub const ADAPTER1: &str = "org.bluez.Adapter1";
    pub const DEVICE1: &str = "org.bluez.Device1";
    pub const AGENT_MANAGER1: &str = "org.bluez.AgentManager1";
}
