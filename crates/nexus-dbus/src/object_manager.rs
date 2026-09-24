//! Manual implementation of
//! `org.freedesktop.DBus.ObjectManager`. See DD-006 §4.
//!
//! zbus doesn't provide this interface out of the box (it's a
//! server-side responsibility), so we assemble the reply from the
//! shared [`State`] snapshot. The returned dict always mirrors the
//! current object tree; there's no separate cache to keep in sync.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::interfaces::iface_names as iface;
use crate::paths::{
    MANAGER_PATH, bluetooth_device_path, ethernet_profile_path, interface_path, scan_result_path,
    wifi_profile_path,
};
use crate::profiles::iface_names as prof_iface;
use crate::state::{InterfaceKindData, State};

/// Inner dict entry: interface name → property map.
pub type IfaceMap = HashMap<String, HashMap<String, OwnedValue>>;
/// Outer dict returned by `GetManagedObjects`.
pub type ObjectsMap = HashMap<OwnedObjectPath, IfaceMap>;

pub struct ObjectManager {
    pub state: Arc<RwLock<State>>,
}

impl ObjectManager {
    pub fn new(state: Arc<RwLock<State>>) -> Self {
        Self { state }
    }
}

#[zbus::interface(name = "org.freedesktop.DBus.ObjectManager")]
impl ObjectManager {
    async fn get_managed_objects(&self) -> zbus::fdo::Result<ObjectsMap> {
        let guard = self.state.read().await;
        let mut out: ObjectsMap = HashMap::new();

        // Every interface object implements `fi.nexus.Interface` plus
        // one technology-specific interface.
        for (ifname, entry) in guard.interfaces.iter() {
            let mut ifaces: IfaceMap = HashMap::new();
            ifaces.insert(iface::COMMON.to_owned(), common_props(ifname, entry));
            match &entry.kind_data {
                InterfaceKindData::Ethernet(c) => {
                    ifaces.insert(iface::ETHERNET.to_owned(), ethernet_props(c));
                }
                InterfaceKindData::Wifi(c) => {
                    ifaces.insert(iface::WIFI.to_owned(), wifi_props(c));
                    // Per-BSS scan-result objects.
                    for bssid in c.scan_cache.keys() {
                        let spath = scan_result_path(ifname, bssid);
                        if let Ok(p) = ObjectPath::try_from(spath) {
                            let mut sr_ifaces: IfaceMap = HashMap::new();
                            let mut props: HashMap<String, OwnedValue> = HashMap::new();
                            if let Ok(v) = OwnedValue::try_from(Value::new(bssid.0.to_vec())) {
                                props.insert("Bssid".to_owned(), v);
                            }
                            sr_ifaces.insert("fi.nexus.ScanResult".to_owned(), props);
                            out.insert(OwnedObjectPath::from(p), sr_ifaces);
                        }
                    }
                }
                InterfaceKindData::Bluetooth(c) => {
                    ifaces.insert(iface::BLUETOOTH.to_owned(), bluetooth_props(ifname, c));
                    // Per-device objects hang off the adapter.
                    for (key, dev) in c.known_devices.iter() {
                        let dpath = bluetooth_device_path(ifname, &dev.info.address);
                        if let Ok(p) = ObjectPath::try_from(dpath) {
                            let mut dev_ifaces: IfaceMap = HashMap::new();
                            dev_ifaces.insert(
                                iface::BLUETOOTH_DEVICE.to_owned(),
                                bluetooth_device_props(ifname, key, dev),
                            );
                            out.insert(OwnedObjectPath::from(p), dev_ifaces);
                        }
                    }
                }
                InterfaceKindData::Gnss(c) => {
                    ifaces.insert(iface::GNSS.to_owned(), gnss_props(c));
                }
            }
            if let Ok(p) = ObjectPath::try_from(interface_path(ifname)) {
                out.insert(OwnedObjectPath::from(p), ifaces);
            }
        }

        // Wi-Fi profiles.
        for (id, profile) in guard.wifi_profiles.iter() {
            let mut ifaces: IfaceMap = HashMap::new();
            ifaces.insert(
                prof_iface::COMMON.to_owned(),
                profile_common_props_wifi(profile),
            );
            ifaces.insert(prof_iface::WIFI.to_owned(), profile_wifi_props(profile));
            if let Ok(p) = ObjectPath::try_from(wifi_profile_path(&profile.id)) {
                let _ = id;
                out.insert(OwnedObjectPath::from(p), ifaces);
            }
        }

        // Ethernet profiles.
        for (id, profile) in guard.ethernet_profiles.iter() {
            let mut ifaces: IfaceMap = HashMap::new();
            ifaces.insert(
                prof_iface::COMMON.to_owned(),
                profile_common_props_ethernet(profile),
            );
            ifaces.insert(
                prof_iface::ETHERNET.to_owned(),
                profile_ethernet_props(profile),
            );
            if let Ok(p) = ObjectPath::try_from(ethernet_profile_path(&profile.id)) {
                let _ = id;
                out.insert(OwnedObjectPath::from(p), ifaces);
            }
        }

        // Manager itself (at MANAGER_PATH) — include it so the
        // tree is complete.
        let mut mgr_ifaces: IfaceMap = HashMap::new();
        mgr_ifaces.insert("fi.nexus.Manager".to_owned(), manager_props(&guard));
        if let Ok(p) = ObjectPath::try_from(MANAGER_PATH) {
            out.insert(OwnedObjectPath::from(p), mgr_ifaces);
        }

        Ok(out)
    }

    #[zbus(signal)]
    pub async fn interfaces_added(
        emitter: &SignalEmitter<'_>,
        path: OwnedObjectPath,
        interfaces: IfaceMap,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn interfaces_removed(
        emitter: &SignalEmitter<'_>,
        path: OwnedObjectPath,
        interfaces: Vec<String>,
    ) -> zbus::Result<()>;
}

fn insert_value<V: Into<Value<'static>>>(out: &mut HashMap<String, OwnedValue>, key: &str, v: V) {
    if let Ok(ov) = OwnedValue::try_from(v.into()) {
        out.insert(key.to_owned(), ov);
    }
}

pub fn common_props(
    ifname: &str,
    entry: &crate::state::InterfaceState,
) -> HashMap<String, OwnedValue> {
    use crate::interfaces::common::oper_state_label;
    let mut out = HashMap::new();
    insert_value(&mut out, "Ifname", ifname.to_owned());
    insert_value(&mut out, "Ifindex", entry.info.ifindex);
    insert_value(&mut out, "Mac", entry.info.mac.to_vec());
    insert_value(&mut out, "Kind", entry.kind_label().to_owned());
    insert_value(
        &mut out,
        "OperState",
        oper_state_label(&entry.info.operstate).to_owned(),
    );
    insert_value(&mut out, "Carrier", entry.info.carrier);
    let managed = entry
        .managed_profile
        .clone()
        .unwrap_or_else(|| "/".to_owned());
    if let Ok(p) = ObjectPath::try_from(managed) {
        if let Ok(v) = OwnedValue::try_from(Value::ObjectPath(p)) {
            out.insert("ManagedProfile".to_owned(), v);
        }
    }
    out
}

pub fn ethernet_props(c: &crate::state::EthernetState) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    insert_value(&mut out, "State", c.state.clone());
    insert_value(&mut out, "AuthBackend", c.auth_backend.clone());
    insert_value(&mut out, "AuthFailureReason", c.auth_failure_reason.clone());
    insert_value(&mut out, "EapMethod", c.eap_method.clone());
    out
}

pub fn wifi_props(c: &crate::state::WifiInterfaceState) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    insert_value(&mut out, "State", c.state.clone());
    insert_value(&mut out, "SignalDbm", c.signal_dbm);
    insert_value(&mut out, "Frequency", c.frequency);
    insert_value(&mut out, "Supplicant", c.supplicant.clone());
    insert_value(&mut out, "RoamingMode", c.roaming_mode.clone());
    insert_value(&mut out, "Powered", c.powered);
    out
}

pub fn bluetooth_props(
    ifname: &str,
    c: &crate::state::BluetoothAdapterState,
) -> HashMap<String, OwnedValue> {
    let _ = ifname;
    let mut out = HashMap::new();
    insert_value(&mut out, "Address", c.address.clone());
    insert_value(&mut out, "Powered", c.powered);
    insert_value(&mut out, "Discoverable", c.discoverable);
    insert_value(&mut out, "Pairable", c.pairable);
    insert_value(&mut out, "Discovering", c.discovering);
    insert_value(&mut out, "NexusDiscovering", c.nexus_discovering);
    insert_value(&mut out, "State", c.state.clone());
    out
}

pub fn bluetooth_device_props(
    _ifname: &str,
    _key: &str,
    d: &crate::state::BtDeviceState,
) -> HashMap<String, OwnedValue> {
    use nexus_core::BluetoothAddrExt;
    let mut out = HashMap::new();
    insert_value(&mut out, "Address", d.info.address.to_bluez());
    insert_value(&mut out, "Name", d.info.name.clone().unwrap_or_default());
    insert_value(&mut out, "Alias", d.info.alias.clone().unwrap_or_default());
    insert_value(&mut out, "Paired", d.info.paired);
    insert_value(&mut out, "Connected", d.info.connected);
    insert_value(&mut out, "State", d.state.clone());
    out
}

pub fn gnss_props(c: &crate::state::GnssState) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    insert_value(&mut out, "State", c.state.clone());
    insert_value(&mut out, "DevicePath", c.device_path.clone());
    insert_value(&mut out, "VendorModel", c.vendor_model.clone());
    insert_value(&mut out, "SatellitesInView", c.satellites_in_view);
    insert_value(&mut out, "SatellitesUsed", c.satellites_used);
    insert_value(&mut out, "HorizontalErrorM", c.horizontal_error_m);
    insert_value(&mut out, "GpsdConnected", c.gpsd_connected);
    out
}

pub fn profile_common_props_wifi(
    p: &nexus_profile_store::WifiProfile,
) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    insert_value(&mut out, "Id", p.id.to_string());
    insert_value(&mut out, "Kind", "wifi".to_owned());
    insert_value(
        &mut out,
        "Label",
        p.metadata.label.clone().unwrap_or_default(),
    );
    insert_value(
        &mut out,
        "CredentialsInvalid",
        p.network.credentials_invalid,
    );
    out
}

pub fn profile_common_props_ethernet(
    p: &nexus_profile_store::EthernetProfile,
) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    insert_value(&mut out, "Id", p.id.to_string());
    insert_value(&mut out, "Kind", "ethernet".to_owned());
    insert_value(
        &mut out,
        "Label",
        p.metadata.label.clone().unwrap_or_default(),
    );
    insert_value(&mut out, "CredentialsInvalid", false);
    out
}

pub fn profile_wifi_props(p: &nexus_profile_store::WifiProfile) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    insert_value(&mut out, "Ssid", p.network.ssid.as_bytes().to_vec());
    insert_value(&mut out, "Hidden", p.network.hidden);
    insert_value(&mut out, "Priority", p.network.priority);
    insert_value(&mut out, "AutoConnect", p.network.auto_connect);
    insert_value(&mut out, "FastTransition", p.network.fast_transition);
    out
}

pub fn profile_ethernet_props(
    p: &nexus_profile_store::EthernetProfile,
) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    insert_value(&mut out, "Ifname", p.interface.name.clone());
    insert_value(&mut out, "AutoConnect", p.interface.auto_connect);
    insert_value(
        &mut out,
        "Dot1xEnabled",
        p.dot1x.as_ref().map(|d| d.enabled).unwrap_or(false),
    );
    out
}

fn manager_props(guard: &State) -> HashMap<String, OwnedValue> {
    let mut out = HashMap::new();
    insert_value(&mut out, "Version", guard.version.clone());
    insert_value(
        &mut out,
        "PowerState",
        guard.power_state.as_str().to_owned(),
    );
    insert_value(&mut out, "ApiCapabilities", guard.api_capabilities.clone());
    insert_value(&mut out, "MasterKeySource", guard.master_key_source.clone());
    out
}
