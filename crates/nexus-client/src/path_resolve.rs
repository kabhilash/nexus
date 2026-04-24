//! Object-path resolution (DD-008 §7.2).
//!
//! Most per-interface D-Bus objects live at
//! `/fi/nexus1/interface/<ifname>` or
//! `/fi/nexus1/profile/<kind>/<ulid>`. nexusctl receives
//! human-friendly names (`wlan0`, `AA:BB:...`, a profile label),
//! so each handler starts with a resolution step.
//!
//! The helpers here are stateless — they issue exactly the D-Bus
//! round-trips described in §7.2:
//!
//! - [`resolve_interface_by_ifname`] uses `Manager.GetInterface`.
//! - [`resolve_interfaces_of_kind`] walks `Manager.Interfaces` and
//!   filters by the common `Kind` property.
//! - [`resolve_bluetooth_device_by_address`] walks each adapter's
//!   `KnownDevices` and matches on `Address`.
//! - [`resolve_gnss_by_device`] walks the GNSS interfaces and
//!   matches on `DevicePath`.
//! - [`resolve_profile_by_ref`] tries the reference as a ULID path
//!   first, then as a `Label` match across `WifiProfiles` +
//!   `EthernetProfiles`.
//!
//! A resolution that matches multiple objects (e.g. `wifi show` with
//! no arg and two Wi-Fi interfaces) returns
//! [`ResolveError::Ambiguous`] carrying the candidate list so the
//! caller can render a usage hint.

use zbus::Connection;
use zbus::zvariant::OwnedObjectPath;

use crate::errors::NexusctlError;
use crate::errors_map::from_zbus_error;
use crate::proxy::bluetooth::BluetoothProxy;
use crate::proxy::bluetooth_device::BluetoothDeviceProxy;
use crate::proxy::gnss::GnssProxy;
use crate::proxy::interface::InterfaceProxy;
use crate::proxy::manager::ManagerProxy;
use crate::proxy::profile::ProfileProxy;

/// A resolution error shapes into one of two [`NexusctlError`]
/// variants depending on the cause. Kept as its own enum so the
/// caller can distinguish "absent" (the usage hint prints
/// candidates) from "ambiguous" (the message lists them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    NotFound {
        reference: String,
    },
    Ambiguous {
        reference: String,
        candidates: Vec<String>,
    },
}

impl From<ResolveError> for NexusctlError {
    fn from(e: ResolveError) -> Self {
        match e {
            ResolveError::NotFound { reference } => NexusctlError::NotFound { reference },
            ResolveError::Ambiguous {
                reference,
                candidates,
            } => NexusctlError::InvalidArgument {
                message: format!(
                    "ambiguous reference `{reference}`; candidates: {}",
                    candidates.join(", ")
                ),
            },
        }
    }
}

pub async fn resolve_interface_by_ifname(
    conn: &Connection,
    ifname: &str,
) -> Result<OwnedObjectPath, NexusctlError> {
    let mgr = ManagerProxy::new(conn).await.map_err(from_zbus_error)?;
    // `Manager.GetInterface(ifname)` returns `fi.nexus.Error.NotFound`
    // on an unknown name; our error translator already maps that to
    // `NexusctlError::NotFound`.
    mgr.get_interface(ifname).await.map_err(from_zbus_error)
}

pub async fn resolve_interfaces_of_kind(
    conn: &Connection,
    kind: &str,
) -> Result<Vec<(String, OwnedObjectPath)>, NexusctlError> {
    let mgr = ManagerProxy::new(conn).await.map_err(from_zbus_error)?;
    let paths = mgr.interfaces().await.map_err(from_zbus_error)?;
    let mut out = Vec::new();
    for path in paths {
        let iface = InterfaceProxy::builder(conn)
            .path(path.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let actual = iface.kind().await.map_err(from_zbus_error)?;
        if actual == kind {
            let name = iface.ifname().await.map_err(from_zbus_error)?;
            out.push((name, path));
        }
    }
    Ok(out)
}

/// Resolve a Bluetooth device by colon-formatted address. Walks
/// every Bluetooth adapter's `KnownDevices` list.
pub async fn resolve_bluetooth_device_by_address(
    conn: &Connection,
    address: &str,
) -> Result<OwnedObjectPath, NexusctlError> {
    let target = address.to_ascii_uppercase();
    for (_ifname, adapter_path) in resolve_interfaces_of_kind(conn, "bluetooth").await? {
        let adapter = BluetoothProxy::builder(conn)
            .path(adapter_path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let device_paths = adapter.known_devices().await.map_err(from_zbus_error)?;
        for dpath in device_paths {
            let dev = BluetoothDeviceProxy::builder(conn)
                .path(dpath.clone())
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            let addr = dev.address().await.map_err(from_zbus_error)?;
            if addr.to_ascii_uppercase() == target {
                return Ok(dpath);
            }
        }
    }
    Err(ResolveError::NotFound {
        reference: address.to_owned(),
    }
    .into())
}

/// Resolve a GNSS interface. `device` may be an ifname (`/dev/gps0`
/// or `gpsd-0`) or the kernel device path reported in `DevicePath`.
/// When `device` is `None`, returns the single Gnss interface if
/// exactly one is registered — otherwise raises `InvalidArgument`.
pub async fn resolve_gnss_by_device(
    conn: &Connection,
    device: Option<&str>,
) -> Result<OwnedObjectPath, NexusctlError> {
    let gnss_list = resolve_interfaces_of_kind(conn, "gnss").await?;
    if gnss_list.is_empty() {
        return Err(ResolveError::NotFound {
            reference: device.unwrap_or("<any gnss>").to_owned(),
        }
        .into());
    }
    if device.is_none() {
        if gnss_list.len() == 1 {
            return Ok(gnss_list.into_iter().next().unwrap().1);
        }
        let candidates = gnss_list.iter().map(|(n, _)| n.clone()).collect();
        return Err(ResolveError::Ambiguous {
            reference: "<any gnss>".to_owned(),
            candidates,
        }
        .into());
    }
    let target = device.unwrap();
    for (ifname, path) in &gnss_list {
        if ifname == target {
            return Ok(path.clone());
        }
        let proxy = GnssProxy::builder(conn)
            .path(path.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let dpath = proxy.device_path().await.map_err(from_zbus_error)?;
        if dpath == target {
            return Ok(path.clone());
        }
    }
    Err(ResolveError::NotFound {
        reference: target.to_owned(),
    }
    .into())
}

/// Resolve a profile by `<ulid>` or by `Label`. Tries the ULID path
/// first (`/fi/nexus1/profile/{wifi,ethernet}/<ref>`); on miss,
/// walks `Manager.WifiProfiles` + `Manager.EthernetProfiles` and
/// matches `Label`.
pub async fn resolve_profile_by_ref(
    conn: &Connection,
    reference: &str,
) -> Result<OwnedObjectPath, NexusctlError> {
    let mgr = ManagerProxy::new(conn).await.map_err(from_zbus_error)?;
    let wifi_paths = mgr.wifi_profiles().await.map_err(from_zbus_error)?;
    let eth_paths = mgr.ethernet_profiles().await.map_err(from_zbus_error)?;

    // Try as ULID against existing profile paths.
    for p in wifi_paths.iter().chain(eth_paths.iter()) {
        if let Some(id) = p.as_str().rsplit('/').next() {
            if id.eq_ignore_ascii_case(reference) {
                return Ok(p.clone());
            }
        }
    }

    // Try label match.
    let mut matches: Vec<OwnedObjectPath> = Vec::new();
    for p in wifi_paths.iter().chain(eth_paths.iter()) {
        let proxy = ProfileProxy::builder(conn)
            .path(p.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let label = proxy.label().await.map_err(from_zbus_error)?;
        if label == reference {
            matches.push(p.clone());
        }
    }
    match matches.len() {
        0 => Err(ResolveError::NotFound {
            reference: reference.to_owned(),
        }
        .into()),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => {
            let names = matches.into_iter().map(|p| p.as_str().to_owned()).collect();
            Err(ResolveError::Ambiguous {
                reference: reference.to_owned(),
                candidates: names,
            }
            .into())
        }
    }
}
