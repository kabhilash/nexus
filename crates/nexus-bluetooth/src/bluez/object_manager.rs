//! ObjectManager subscription. Translates BlueZ's signal stream
//! into [`nexus_core::NexusEvent`] variants. See DD-004 §7.1.
//!
//! The pump is a dedicated tokio task spawned at client connect
//! time. It drains:
//!
//! - The initial `GetManagedObjects` snapshot (republishes every
//!   existing adapter and device as a synthetic event sequence).
//! - `InterfacesAdded` → `BtAdapterChanged` or `BtDeviceDiscovered`.
//! - `InterfacesRemoved` → translated to `BtDeviceRemoved` for
//!   devices (adapter removal is handled by DD-001 and the
//!   kernel-side `InterfaceRemoved`).
//! - `PropertiesChanged` for adapter `Powered` / `Discovering` and
//!   device `Connected`.
//!
//! The pump reads raw `MessageStream` frames so its decode logic
//! has no zbus-proxy-macro dependency — that keeps the decode
//! helpers unit-testable from a mock.

use std::collections::HashMap;

use nexus_core::{BluetoothAddrExt, BtAddressType, BtDeviceInfo, BtTransport, MacAddr, NexusEvent};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zbus::zvariant::{OwnedValue, Value};

use super::proxies::{InterfacesMap, ObjectManagerProxy, iface};
use crate::errors::{BtError, Result};

/// Opaque handle returned by the pump. Dropping it doesn't stop
/// the task — the pump observes the cancellation token instead,
/// which is held by the [`super::zbus_client::ZbusBluezClient`].
pub struct PumpHandle {
    pub _join: JoinHandle<()>,
}

/// Spawn the pump. Runs the initial snapshot synthesis before
/// returning, so the first batch of `BtAdapterChanged` /
/// `BtDeviceDiscovered` events is already queued on the broadcast
/// by the time this returns.
pub async fn spawn_object_manager_pump(
    connection: zbus::Connection,
    om: ObjectManagerProxy<'static>,
    event_tx: broadcast::Sender<NexusEvent>,
    cancel: CancellationToken,
) -> Result<PumpHandle> {
    let initial = om
        .get_managed_objects()
        .await
        .map_err(|e| BtError::Bluez(format!("GetManagedObjects: {e}")))?;
    republish_snapshot(&initial, &event_tx);

    let stream = zbus::MessageStream::from(connection);
    let join = tokio::spawn(run_pump(stream, event_tx, cancel));
    Ok(PumpHandle { _join: join })
}

async fn run_pump(
    mut stream: zbus::MessageStream,
    event_tx: broadcast::Sender<NexusEvent>,
    cancel: CancellationToken,
) {
    use futures_util::StreamExt;
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            msg = stream.next() => {
                let Some(msg) = msg else { break };
                let Ok(msg) = msg else { continue };
                handle_signal(&msg, &event_tx);
            }
        }
    }
}

fn handle_signal(msg: &zbus::Message, event_tx: &broadcast::Sender<NexusEvent>) {
    let header = msg.header();
    if header.message_type() != zbus::message::Type::Signal {
        return;
    }
    let Some(iface_name) = header.interface() else {
        return;
    };
    let Some(member) = header.member() else {
        return;
    };

    match (iface_name.as_str(), member.as_str()) {
        ("org.freedesktop.DBus.ObjectManager", "InterfacesAdded") => {
            if let Ok((path, ifaces)) = msg
                .body()
                .deserialize::<(zbus::zvariant::OwnedObjectPath, InterfacesMap)>()
            {
                on_interfaces_added(path.as_str(), &ifaces, event_tx);
            }
        }
        ("org.freedesktop.DBus.ObjectManager", "InterfacesRemoved") => {
            if let Ok((path, ifaces)) = msg
                .body()
                .deserialize::<(zbus::zvariant::OwnedObjectPath, Vec<String>)>()
            {
                on_interfaces_removed(path.as_str(), &ifaces, event_tx);
            }
        }
        ("org.freedesktop.DBus.Properties", "PropertiesChanged") => {
            let path_owned = header.path().map(|p| p.to_string()).unwrap_or_default();
            if let Ok((iface, changed, _invalidated)) =
                msg.body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
            {
                on_properties_changed(&path_owned, &iface, &changed, event_tx);
            }
        }
        _ => {}
    }
}

fn republish_snapshot(
    snapshot: &super::proxies::ManagedObjects,
    event_tx: &broadcast::Sender<NexusEvent>,
) {
    for (path, ifaces) in snapshot {
        on_interfaces_added(path.as_str(), ifaces, event_tx);
    }
}

/// Translate a freshly-added BlueZ object into the appropriate
/// [`NexusEvent`]. Exposed for test coverage.
pub fn on_interfaces_added(
    path: &str,
    ifaces: &InterfacesMap,
    event_tx: &broadcast::Sender<NexusEvent>,
) {
    if let Some(props) = ifaces.get(iface::ADAPTER1) {
        let powered = read_bool(props, "Powered").unwrap_or(false);
        let discovering = read_bool(props, "Discovering").unwrap_or(false);
        let _ = event_tx.send(NexusEvent::BtAdapterChanged {
            adapter: path.to_owned(),
            powered,
            discovering,
        });
    }
    if let Some(props) = ifaces.get(iface::DEVICE1) {
        if let Some(info) = parse_device_info(path, props) {
            let _ = event_tx.send(NexusEvent::BtDeviceDiscovered(info));
        }
    }
}

/// Translate an `InterfacesRemoved` signal. Adapter removal is
/// left to DD-001's `InterfaceRemoved` path (kernel-authoritative);
/// a Device1 removal triggers `BtDeviceDisconnected` so the
/// backend can finalize per-device state.
pub fn on_interfaces_removed(
    path: &str,
    ifaces: &[String],
    event_tx: &broadcast::Sender<NexusEvent>,
) {
    if ifaces.iter().any(|i| i == iface::DEVICE1) {
        if let (Some(adapter), Some(address)) = (
            adapter_from_device_path(path),
            address_from_device_path(path),
        ) {
            let _ = event_tx.send(NexusEvent::BtDeviceDisconnected { adapter, address });
        }
    }
}

/// Translate a `PropertiesChanged` signal. `path` is the object
/// path the signal was sent from; `iface` is arg0; `changed` is
/// arg1.
pub fn on_properties_changed(
    path: &str,
    iface_name: &str,
    changed: &HashMap<String, OwnedValue>,
    event_tx: &broadcast::Sender<NexusEvent>,
) {
    match iface_name {
        iface::ADAPTER1 => {
            let powered = read_bool(changed, "Powered");
            let discovering = read_bool(changed, "Discovering");
            if powered.is_some() || discovering.is_some() {
                let _ = event_tx.send(NexusEvent::BtAdapterChanged {
                    adapter: path.to_owned(),
                    powered: powered.unwrap_or(false),
                    discovering: discovering.unwrap_or(false),
                });
            }
        }
        iface::DEVICE1 => {
            if let Some(connected) = read_bool(changed, "Connected") {
                let Some(address) = address_from_device_path(path) else {
                    tracing::debug!(path, "device PropertiesChanged with unparseable path");
                    return;
                };
                let Some(adapter) = adapter_from_device_path(path) else {
                    return;
                };
                let event = if connected {
                    NexusEvent::BtDeviceConnected { adapter, address }
                } else {
                    NexusEvent::BtDeviceDisconnected { adapter, address }
                };
                let _ = event_tx.send(event);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Helpers for pulling typed values out of the OwnedValue dict.
// ---------------------------------------------------------------------------

fn read_bool(dict: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    let v = dict.get(key)?;
    bool::try_from(v).ok()
}

fn read_string(dict: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let v = dict.get(key)?;
    let s: &str = <&str>::try_from(v).ok()?;
    Some(s.to_owned())
}

fn read_i16(dict: &HashMap<String, OwnedValue>, key: &str) -> Option<i16> {
    let v = dict.get(key)?;
    i16::try_from(v).ok()
}

fn read_string_vec(dict: &HashMap<String, OwnedValue>, key: &str) -> Option<Vec<String>> {
    let v = dict.get(key)?;
    let arr: &zbus::zvariant::Array = v.downcast_ref().ok()?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr.iter() {
        let s: &str = item.downcast_ref().ok()?;
        out.push(s.to_owned());
    }
    Some(out)
}

/// Parse `dev_AA_BB_...` suffix → [`MacAddr`].
pub fn address_from_device_path(path: &str) -> Option<MacAddr> {
    let last = path.rsplit('/').next()?;
    let stripped = last.strip_prefix("dev_")?;
    let mac_str: String = stripped.replace('_', ":");
    MacAddr::from_bluez(&mac_str).ok()
}

/// BlueZ device paths are `<adapter>/dev_XXXX`; peel off the
/// trailing device component to get the adapter path.
pub fn adapter_from_device_path(path: &str) -> Option<String> {
    let (head, _tail) = path.rsplit_once('/')?;
    Some(head.to_owned())
}

/// Derive a best-effort [`BtAddressType`] from BlueZ's string form.
fn parse_address_type(s: &str) -> BtAddressType {
    match s {
        "public" => BtAddressType::LePublic,
        "random" => BtAddressType::LeRandom,
        "bredr" => BtAddressType::Bredr,
        // BlueZ emits strictly one of the three; if it ever deviates,
        // err toward Classic — the transport classifier will still
        // catch Dual devices via the Classic-only UUID heuristic.
        _ => BtAddressType::Bredr,
    }
}

fn classify_transport(addr: BtAddressType, uuids: &[String]) -> BtTransport {
    match addr {
        BtAddressType::Bredr => BtTransport::Bredr,
        BtAddressType::LeRandom => BtTransport::Le,
        BtAddressType::LePublic => {
            const CLASSIC_HINTS: &[&str] = &[
                // HFP.
                "0000111e-0000-1000-8000-00805f9b34fb",
                // A2DP source / sink / AVRCP.
                "0000110a-0000-1000-8000-00805f9b34fb",
                "0000110b-0000-1000-8000-00805f9b34fb",
                "0000110e-0000-1000-8000-00805f9b34fb",
                // Serial Port.
                "00001101-0000-1000-8000-00805f9b34fb",
            ];
            if uuids
                .iter()
                .any(|u| CLASSIC_HINTS.iter().any(|h| u.eq_ignore_ascii_case(h)))
            {
                BtTransport::Dual
            } else {
                BtTransport::Le
            }
        }
    }
}

/// Parse a BlueZ `Device1` property bag into a [`BtDeviceInfo`].
/// `None` when required fields are missing (no `Address`).
pub fn parse_device_info(path: &str, props: &HashMap<String, OwnedValue>) -> Option<BtDeviceInfo> {
    let address_str = read_string(props, "Address")?;
    let address = MacAddr::from_bluez(&address_str).ok()?;
    let address_type_str = read_string(props, "AddressType").unwrap_or_else(|| "public".to_owned());
    let address_type = parse_address_type(&address_type_str);
    let uuids = read_string_vec(props, "UUIDs").unwrap_or_default();
    let transport = classify_transport(address_type, &uuids);
    let adapter = adapter_from_device_path(path).unwrap_or_default();
    Some(BtDeviceInfo {
        adapter,
        device_path: path.to_owned(),
        address,
        address_type,
        name: read_string(props, "Name"),
        alias: read_string(props, "Alias"),
        rssi: read_i16(props, "RSSI"),
        tx_power: read_i16(props, "TxPower"),
        uuids,
        transport,
        manufacturer_data: HashMap::new(),
        paired: read_bool(props, "Paired").unwrap_or(false),
        bonded: read_bool(props, "Bonded").unwrap_or(false),
        trusted: read_bool(props, "Trusted").unwrap_or(false),
        blocked: read_bool(props, "Blocked").unwrap_or(false),
        connected: read_bool(props, "Connected").unwrap_or(false),
    })
}

// ---------------------------------------------------------------------------
// Mock-only helper: build an [`InterfacesMap`] for a fake adapter /
// device. Lets tests drive `on_interfaces_added` without spinning
// up a real zbus connection.
// ---------------------------------------------------------------------------

/// Build an adapter property bag for the given powered/discovering
/// pair. Used by the mock client and unit tests.
pub fn adapter_props(powered: bool, discovering: bool) -> InterfacesMap {
    let mut inner: HashMap<String, OwnedValue> = HashMap::new();
    inner.insert(
        "Powered".to_owned(),
        OwnedValue::try_from(Value::new(powered)).unwrap(),
    );
    inner.insert(
        "Discovering".to_owned(),
        OwnedValue::try_from(Value::new(discovering)).unwrap(),
    );
    let mut map: InterfacesMap = HashMap::new();
    map.insert(iface::ADAPTER1.to_owned(), inner);
    map
}

/// Build a device property bag. Only the fields the backend's
/// parse path cares about are populated — enough to drive the
/// [`parse_device_info`] path.
pub fn device_props(
    address: MacAddr,
    address_type: &str,
    name: Option<&str>,
    paired: bool,
    connected: bool,
    uuids: &[&str],
) -> InterfacesMap {
    let mut inner: HashMap<String, OwnedValue> = HashMap::new();
    inner.insert(
        "Address".to_owned(),
        OwnedValue::try_from(Value::new(address.to_bluez())).unwrap(),
    );
    inner.insert(
        "AddressType".to_owned(),
        OwnedValue::try_from(Value::new(address_type.to_owned())).unwrap(),
    );
    if let Some(name) = name {
        inner.insert(
            "Name".to_owned(),
            OwnedValue::try_from(Value::new(name.to_owned())).unwrap(),
        );
    }
    inner.insert(
        "Paired".to_owned(),
        OwnedValue::try_from(Value::new(paired)).unwrap(),
    );
    inner.insert(
        "Connected".to_owned(),
        OwnedValue::try_from(Value::new(connected)).unwrap(),
    );
    let uuid_vec: Vec<String> = uuids.iter().map(|s| s.to_string()).collect();
    inner.insert(
        "UUIDs".to_owned(),
        OwnedValue::try_from(Value::new(uuid_vec)).unwrap(),
    );
    let mut map: InterfacesMap = HashMap::new();
    map.insert(iface::DEVICE1.to_owned(), inner);
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_parsed_from_device_path() {
        let got = address_from_device_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
        assert_eq!(got, Some(MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])));
    }

    #[test]
    fn adapter_extracted_from_device_path() {
        assert_eq!(
            adapter_from_device_path("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF"),
            Some("/org/bluez/hci0".to_owned()),
        );
    }

    #[test]
    fn transport_classifies_le_without_classic_uuids() {
        assert_eq!(
            classify_transport(BtAddressType::LePublic, &[]),
            BtTransport::Le,
        );
    }

    #[test]
    fn transport_classifies_dual_with_classic_uuid() {
        let uuids = vec!["0000110a-0000-1000-8000-00805f9b34fb".to_owned()];
        assert_eq!(
            classify_transport(BtAddressType::LePublic, &uuids),
            BtTransport::Dual,
        );
    }

    #[test]
    fn transport_random_is_always_le() {
        assert_eq!(
            classify_transport(BtAddressType::LeRandom, &[]),
            BtTransport::Le,
        );
    }

    #[test]
    fn on_interfaces_added_emits_adapter_event() {
        let (tx, mut rx) = broadcast::channel(8);
        on_interfaces_added("/org/bluez/hci0", &adapter_props(true, false), &tx);
        let event = rx.try_recv().unwrap();
        matches!(event, NexusEvent::BtAdapterChanged { .. });
    }

    #[test]
    fn on_interfaces_added_emits_device_event() {
        let (tx, mut rx) = broadcast::channel(8);
        let props = device_props(
            MacAddr([0x01, 0x02, 0x03, 0x04, 0x05, 0x06]),
            "public",
            Some("Peer"),
            false,
            false,
            &[],
        );
        on_interfaces_added("/org/bluez/hci0/dev_01_02_03_04_05_06", &props, &tx);
        let event = rx.try_recv().unwrap();
        matches!(event, NexusEvent::BtDeviceDiscovered(_));
    }

    #[test]
    fn properties_changed_emits_device_connected() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "Connected".to_owned(),
            OwnedValue::try_from(Value::new(true)).unwrap(),
        );
        on_properties_changed(
            "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
            iface::DEVICE1,
            &changed,
            &tx,
        );
        let event = rx.try_recv().unwrap();
        matches!(event, NexusEvent::BtDeviceConnected { .. });
    }
}
