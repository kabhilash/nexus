//! `fi.nexus.Bluetooth` — DD-006 §6.4. Read-only for phase 2.

use std::sync::Arc;

use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::paths::bluetooth_device_path;
use crate::services::Services;
use crate::state::InterfaceKindData;

pub struct BluetoothIface {
    pub services: Arc<Services>,
    pub ifname: String,
}

impl BluetoothIface {
    pub fn new(services: Arc<Services>, ifname: impl Into<String>) -> Self {
        Self {
            services,
            ifname: ifname.into(),
        }
    }

    async fn with_cache<R>(
        &self,
        default: R,
        f: impl FnOnce(&crate::state::BluetoothAdapterState) -> R,
    ) -> R {
        let guard = self.services.state.read().await;
        match guard.interfaces.get(&self.ifname).map(|e| &e.kind_data) {
            Some(InterfaceKindData::Bluetooth(c)) => f(c),
            _ => default,
        }
    }
}

#[zbus::interface(name = "fi.nexus.Bluetooth")]
impl BluetoothIface {
    #[zbus(property, name = "Address")]
    async fn address(&self) -> String {
        self.with_cache(String::new(), |c| c.address.clone()).await
    }

    #[zbus(property, name = "Powered")]
    async fn powered(&self) -> bool {
        self.with_cache(false, |c| c.powered).await
    }

    #[zbus(property, name = "Discoverable")]
    async fn discoverable(&self) -> bool {
        self.with_cache(false, |c| c.discoverable).await
    }

    #[zbus(property, name = "Pairable")]
    async fn pairable(&self) -> bool {
        self.with_cache(false, |c| c.pairable).await
    }

    #[zbus(property, name = "Discovering")]
    async fn discovering(&self) -> bool {
        self.with_cache(false, |c| c.discovering).await
    }

    #[zbus(property, name = "NexusDiscovering")]
    async fn nexus_discovering(&self) -> bool {
        self.with_cache(false, |c| c.nexus_discovering).await
    }

    #[zbus(property, name = "KnownDevices")]
    async fn known_devices(&self) -> Vec<OwnedObjectPath> {
        let adapter = self.ifname.clone();
        self.with_cache(Vec::<OwnedObjectPath>::new(), |c| {
            c.known_devices
                .values()
                .filter_map(|d| {
                    let path = bluetooth_device_path(&adapter, &d.info.address);
                    ObjectPath::try_from(path).ok().map(OwnedObjectPath::from)
                })
                .collect()
        })
        .await
    }

    #[zbus(property, name = "State")]
    async fn state(&self) -> String {
        self.with_cache(String::new(), |c| c.state.clone()).await
    }
}
