//! `fi.nexus.BluetoothDevice` — DD-006 §6.6.
//!
//! Lives at `/fi/nexus1/interface/<adapter>/device/<AA_BB_…>`.

use std::collections::HashMap;
use std::sync::Arc;

use nexus_core::BluetoothAddrExt;
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::authz::actions;
use crate::errors::DbusError;
use crate::paths::interface_path;
use crate::services::{Feature, Services};
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

    /// Resolve the parent adapter's BlueZ object path, mirroring
    /// [`BluetoothIface::bluez_path`]. Used to scope `Forget`'s
    /// adapter-aware cleanup.
    async fn adapter_bluez_path(&self) -> String {
        let guard = self.services.state.read().await;
        guard
            .interfaces
            .get(&self.adapter_ifname)
            .and_then(|e| match &e.info.kind {
                nexus_core::InterfaceKind::Bluetooth { bluez_path, .. } => {
                    Some(bluez_path.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| format!("/org/bluez/{}", self.adapter_ifname))
    }

    /// Build this device's BlueZ object path
    /// (`<adapter>/dev_AA_BB_…`). Internal nexus-bluetooth callers
    /// expect the BlueZ-spelled path; the kernel name `hci0` plus
    /// the underscore-form address is enough to reconstruct it.
    async fn device_bluez_path(&self) -> String {
        format!("{}/dev_{}", self.adapter_bluez_path().await, self.device_key)
    }

    fn check_feature(&self) -> fdo::Result<()> {
        self.services
            .enabled
            .require(Feature::Bluetooth)
            .map_err(fdo::Error::from)
    }

    async fn require_auth(&self, hdr: &Header<'_>, action: &str) -> fdo::Result<()> {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        if self
            .services
            .auth
            .check(action, &sender)
            .await
            .is_authorized()
        {
            Ok(())
        } else {
            Err(fdo::Error::from(DbusError::AuthFailed(format!(
                "policykit denied '{action}' for sender '{sender}'"
            ))))
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

    /// `Trusted` writable — `fi.nexus.profile.modify` (it persists
    /// to the device's profile, conceptually a profile attribute).
    /// Routes to [`crate::BackendOps::bt_set_trusted`].
    #[zbus(property)]
    async fn set_trusted(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
        on: bool,
    ) -> zbus::Result<()> {
        if !self.services.enabled.is_enabled(Feature::Bluetooth) {
            return Err(zbus::Error::from(zbus::fdo::Error::from(
                DbusError::FeatureDisabled("bluetooth".to_owned()),
            )));
        }
        let sender = hdr
            .as_ref()
            .and_then(|h| h.sender().map(|s| s.to_string()))
            .unwrap_or_default();
        if !self
            .services
            .auth
            .check(actions::PROFILE_MODIFY, &sender)
            .await
            .is_authorized()
        {
            return Err(zbus::Error::from(zbus::fdo::Error::AuthFailed(format!(
                "policykit denied '{}' for sender '{sender}'",
                actions::PROFILE_MODIFY
            ))));
        }
        let path = self.device_bluez_path().await;
        self.services
            .ops
            .bt_set_trusted(&path, on)
            .await
            .map_err(|e| zbus::Error::from(zbus::fdo::Error::from(e)))
    }

    /// `Pair() -> (job_id: s)` — `fi.nexus.connect`. DD-006 §6.6.
    /// Shortcut for `fi.nexus.Bluetooth.Pair(this)`; the returned job
    /// id correlates `PairingPrompt`/`PairingComplete` signals fired
    /// on the *adapter* object (§6.4), not this one.
    async fn pair(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<String> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let path = self.device_bluez_path().await;
        let job_id = self
            .services
            .ops
            .bt_pair(&path)
            .await
            .map_err(fdo::Error::from)?;
        Ok(job_id.0.to_string())
    }

    /// `CancelPairing() -> ()` — `fi.nexus.connect`. DD-006 §6.6.
    /// Shortcut for `fi.nexus.Bluetooth.CancelPairing(this)`.
    async fn cancel_pairing(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let path = self.device_bluez_path().await;
        self.services
            .ops
            .bt_cancel_pairing(&path)
            .await
            .map_err(fdo::Error::from)
    }

    /// `Connect() -> ()` — `fi.nexus.connect`. DD-006 §6.6.
    /// Forwards to `nexus_bluetooth::BtCommand::Connect` which
    /// drives BlueZ's `org.bluez.Device1.Connect`.
    async fn connect(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let path = self.device_bluez_path().await;
        self.services
            .ops
            .bt_connect_device(&path)
            .await
            .map_err(fdo::Error::from)
    }

    /// `Disconnect() -> ()` — `fi.nexus.connect`. DD-006 §6.6.
    async fn disconnect(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let path = self.device_bluez_path().await;
        self.services
            .ops
            .bt_disconnect_device(&path)
            .await
            .map_err(fdo::Error::from)
    }

    /// `Forget() -> ()` — `fi.nexus.profile.modify`. DD-006 §6.6.
    /// Drops the device from BlueZ's registry AND erases the
    /// matching nexus profile, if any. `nexus_bluetooth::BtCommand::Forget`
    /// needs both the parent adapter path and the device path.
    async fn forget(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::PROFILE_MODIFY).await?;
        let adapter = self.adapter_bluez_path().await;
        let device = format!("{adapter}/dev_{}", self.device_key);
        self.services
            .ops
            .bt_forget_device(&adapter, &device)
            .await
            .map_err(fdo::Error::from)
    }

    /// `StateChanged(state: s)` — DD-006 §6.6. Mirrors `State`
    /// property transitions so clients don't have to poll. Emitted
    /// from the service event loop
    /// (`service::emit_bt_device_state_changed`) via raw
    /// `connection.emit_signal`, same reasoning as
    /// `fi.nexus.Bluetooth`'s pairing signals — this declaration
    /// exists for introspection only. Named `bt_state_changed` in
    /// Rust (rather than `state_changed`) because zbus's
    /// `#[zbus(property)]` macro already reserves that identifier as
    /// the `State` property's own generic-PropertiesChanged notifier
    /// — `name = "StateChanged"` keeps the wire signal name correct.
    #[zbus(signal, name = "StateChanged")]
    pub async fn bt_state_changed(emitter: &SignalEmitter<'_>, state: &str) -> zbus::Result<()>;

    /// `ConnectionChanged(connected: b)` — DD-006 §6.6. Emitted from
    /// the service event loop
    /// (`service::emit_bt_device_connection_changed`) via raw
    /// `connection.emit_signal`.
    #[zbus(signal)]
    pub async fn connection_changed(
        emitter: &SignalEmitter<'_>,
        connected: bool,
    ) -> zbus::Result<()>;
}
