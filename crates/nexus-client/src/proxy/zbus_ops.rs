//! Production `ManagerOps` impl. Wraps the generated zbus proxies
//! and decodes the `a{sv}` snapshot into [`ManagerStatus`].

use async_trait::async_trait;
use zbus::Connection;

use crate::errors::NexusctlError;
use crate::errors_map::from_zbus_error;
use crate::proxy::interface::InterfaceProxy;
use crate::proxy::manager::ManagerProxy;
use crate::proxy::{InterfaceSummary, ManagerOps, ManagerStatus};

pub struct ZbusManagerOps {
    connection: Connection,
}

impl ZbusManagerOps {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    /// Open a connection to the system bus (or a custom address)
    /// and wrap it. Used by the binary entry point; tests inject a
    /// pre-built connection directly via [`Self::new`].
    pub async fn connect(bus_address: Option<&str>) -> Result<Self, NexusctlError> {
        let conn = match bus_address {
            Some(addr) => zbus::connection::Builder::address(addr)
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?,
            None => Connection::system().await.map_err(from_zbus_error)?,
        };
        Ok(Self::new(conn))
    }
}

#[async_trait]
impl ManagerOps for ZbusManagerOps {
    async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError> {
        let proxy = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let dict = proxy.get_manager_status().await.map_err(from_zbus_error)?;
        Ok(decode_manager_status(&dict))
    }

    async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let paths = mgr.interfaces().await.map_err(from_zbus_error)?;
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let proxy = InterfaceProxy::builder(&self.connection)
                .path(path.clone())
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            // Each property read is a separate D-Bus call; that's
            // fine for Phase 1 (small interface count, no caching).
            // Later phases batch via `Properties.GetAll`.
            let ifname = proxy.ifname().await.map_err(from_zbus_error)?;
            let kind = proxy.kind().await.map_err(from_zbus_error)?;
            let state = proxy.oper_state().await.map_err(from_zbus_error)?;
            let carrier = proxy.carrier().await.map_err(from_zbus_error)?;
            let mac_bytes = proxy.mac().await.map_err(from_zbus_error)?;
            out.push(InterfaceSummary {
                iface: ifname,
                kind,
                state,
                mac: format_mac(&mac_bytes),
                carrier,
            });
        }
        Ok(out)
    }
}

/// `MAC` from `fi.nexus.Interface` is `ay`; canonicalise to a
/// colon-separated lower-hex string (`aa:bb:cc:…`). Empty bytes
/// (e.g., GNSS) collapse to `None` so the JSON renderer emits
/// `null` and the human renderer prints an em-dash.
pub(crate) fn format_mac(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    let parts: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some(parts.join(":"))
}

/// Decode the `a{sv}` returned by `Manager.GetManagerStatus()` into
/// [`ManagerStatus`]. Missing keys default to empty / zero — the
/// daemon is the schema authority but a client at the wrong version
/// shouldn't crash on a missing field.
pub(crate) fn decode_manager_status(
    dict: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
) -> ManagerStatus {
    let s = |k: &str| {
        dict.get(k)
            .and_then(|v| <&str>::try_from(v).ok().map(str::to_owned))
            .unwrap_or_default()
    };
    let u = |k: &str| dict.get(k).and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
    let arr = |k: &str| {
        dict.get(k)
            .and_then(|v| <&zbus::zvariant::Array>::try_from(v).ok())
            .map(|a| {
                a.iter()
                    .filter_map(|item| <&str>::try_from(item).ok().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let interfaces: Vec<String> = arr("Interfaces");
    ManagerStatus {
        version: s("Version"),
        power_state: s("PowerState"),
        api_capabilities: arr("ApiCapabilities"),
        interface_count: interfaces.len() as u32,
        wifi_profile_count: u("WifiProfileCount"),
        ethernet_profile_count: u("EthernetProfileCount"),
        bluetooth_profile_count: u("BluetoothProfileCount"),
        master_key_source: s("MasterKeySource"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use zbus::zvariant::{OwnedValue, Value};

    #[test]
    fn format_mac_empty_is_none() {
        assert_eq!(format_mac(&[]), None);
    }

    #[test]
    fn format_mac_six_bytes_renders_lower_hex() {
        let m = format_mac(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01]).unwrap();
        assert_eq!(m, "aa:bb:cc:dd:ee:01");
    }

    #[test]
    fn decode_status_pulls_every_field() {
        let mut dict: HashMap<String, OwnedValue> = HashMap::new();
        dict.insert(
            "Version".into(),
            OwnedValue::try_from(Value::new("0.1.0".to_owned())).unwrap(),
        );
        dict.insert(
            "PowerState".into(),
            OwnedValue::try_from(Value::new("active".to_owned())).unwrap(),
        );
        dict.insert(
            "Interfaces".into(),
            OwnedValue::try_from(Value::new(vec!["eth0".to_owned(), "wlan0".to_owned()])).unwrap(),
        );
        dict.insert(
            "WifiProfileCount".into(),
            OwnedValue::try_from(Value::new(2u32)).unwrap(),
        );
        dict.insert(
            "EthernetProfileCount".into(),
            OwnedValue::try_from(Value::new(1u32)).unwrap(),
        );
        dict.insert(
            "BluetoothProfileCount".into(),
            OwnedValue::try_from(Value::new(0u32)).unwrap(),
        );
        dict.insert(
            "MasterKeySource".into(),
            OwnedValue::try_from(Value::new("file".to_owned())).unwrap(),
        );
        let s = decode_manager_status(&dict);
        assert_eq!(s.version, "0.1.0");
        assert_eq!(s.power_state, "active");
        assert_eq!(s.interface_count, 2);
        assert_eq!(s.wifi_profile_count, 2);
        assert_eq!(s.ethernet_profile_count, 1);
        assert_eq!(s.bluetooth_profile_count, 0);
        assert_eq!(s.master_key_source, "file");
    }
}
