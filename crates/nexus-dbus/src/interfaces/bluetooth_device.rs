//! `fi.nexus.BluetoothDevice` — DD-006 §6.6.
//!
//! Lives at `/fi/nexus1/interface/<adapter>/device/<AA_BB_…>`.

use std::collections::HashMap;
use std::sync::Arc;

use nexus_core::BluetoothAddrExt;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::paths::interface_path;
use crate::services::Services;
use crate::state::{BtDeviceState, InterfaceKindData};

pub struct BluetoothDeviceIface {
    pub services: Arc<Services>,
    /// Adapter ifname (e.g. `"hci0"`).
    pub adapter_ifname: String,
    /// Device key matching the `BluetoothAdapterState.known_devices`
    /// map — stored as the underscore form the object path uses.
    pub device_key: String,
}

impl BluetoothDeviceIface {
    pub fn new(
        services: Arc<Services>,
        adapter_ifname: impl Into<String>,
        device_key: impl Into<String>,
    ) -> Self {
        Self {
            services,
            adapter_ifname: adapter_ifname.into(),
            device_key: device_key.into(),
        }
    }

    async fn with_cache<R>(&self, default: R, f: impl FnOnce(&BtDeviceState) -> R) -> R {
        let guard = self.services.state.read().await;
        match guard
            .interfaces
            .get(&self.adapter_ifname)
            .map(|e| &e.kind_data)
        {
            Some(InterfaceKindData::Bluetooth(adapter)) => adapter
                .known_devices
                .get(&self.device_key)
                .map(f)
                .unwrap_or(default),
            _ => default,
        }
    }
}

#[zbus::interface(name = "fi.nexus.BluetoothDevice")]
impl BluetoothDeviceIface {
    #[zbus(property, name = "Address")]
    async fn address(&self) -> String {
        self.with_cache(String::new(), |d| d.info.address.to_bluez())
            .await
    }

    #[zbus(property, name = "AddressType")]
    async fn address_type(&self) -> String {
        self.with_cache(String::new(), |d| {
            use nexus_core::BtAddressType::*;
            match d.info.address_type {
                Bredr => "bredr",
                LePublic => "le_public",
                LeRandom => "le_random",
            }
            .to_owned()
        })
        .await
    }

    #[zbus(property, name = "Transport")]
    async fn transport(&self) -> String {
        self.with_cache(String::new(), |d| {
            use nexus_core::BtTransport::*;
            match d.info.transport {
                Bredr => "bredr",
                Le => "le",
                Dual => "dual",
            }
            .to_owned()
        })
        .await
    }

    #[zbus(property, name = "Name")]
    async fn name(&self) -> String {
        self.with_cache(String::new(), |d| d.info.name.clone().unwrap_or_default())
            .await
    }

    #[zbus(property, name = "Alias")]
    async fn alias(&self) -> String {
        self.with_cache(String::new(), |d| d.info.alias.clone().unwrap_or_default())
            .await
    }

    #[zbus(property, name = "Rssi")]
    async fn rssi(&self) -> i16 {
        self.with_cache(0i16, |d| d.info.rssi.unwrap_or(0)).await
    }

    #[zbus(property, name = "TxPower")]
    async fn tx_power(&self) -> i16 {
        self.with_cache(0i16, |d| d.info.tx_power.unwrap_or(0))
            .await
    }

    #[zbus(property, name = "Uuids")]
    async fn uuids(&self) -> Vec<String> {
        self.with_cache(Vec::new(), |d| d.info.uuids.clone()).await
    }

    #[zbus(property, name = "ManufacturerData")]
    async fn manufacturer_data(&self) -> HashMap<u16, Vec<u8>> {
        self.with_cache(HashMap::new(), |d| d.info.manufacturer_data.clone())
            .await
    }

    #[zbus(property, name = "State")]
    async fn state(&self) -> String {
        self.with_cache(String::new(), |d| d.state.clone()).await
    }

    #[zbus(property, name = "Paired")]
    async fn paired(&self) -> bool {
        self.with_cache(false, |d| d.info.paired).await
    }

    #[zbus(property, name = "Bonded")]
    async fn bonded(&self) -> bool {
        self.with_cache(false, |d| d.info.bonded).await
    }

    #[zbus(property, name = "Trusted")]
    async fn trusted(&self) -> bool {
        self.with_cache(false, |d| d.info.trusted).await
    }

    #[zbus(property, name = "Blocked")]
    async fn blocked(&self) -> bool {
        self.with_cache(false, |d| d.info.blocked).await
    }

    #[zbus(property, name = "Connected")]
    async fn connected(&self) -> bool {
        self.with_cache(false, |d| d.info.connected).await
    }

    #[zbus(property, name = "Adapter")]
    async fn adapter(&self) -> OwnedObjectPath {
        let path = interface_path(&self.adapter_ifname);
        ObjectPath::try_from(path)
            .unwrap_or_else(|_| ObjectPath::try_from("/").unwrap())
            .into()
    }

    #[zbus(property, name = "Profile")]
    async fn profile(&self) -> OwnedObjectPath {
        let raw = self
            .with_cache(None, |d| d.profile_path.clone())
            .await
            .unwrap_or_else(|| "/".to_owned());
        ObjectPath::try_from(raw)
            .unwrap_or_else(|_| ObjectPath::try_from("/").unwrap())
            .into()
    }
}
