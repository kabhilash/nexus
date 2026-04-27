//! Production `ManagerOps` impl. Wraps the generated zbus proxies
//! and decodes them into the command-facing view types.
//!
//! Every method goes through the same shape:
//! 1. Open the Manager proxy + resolve the needed object path(s).
//! 2. Read each property the view needs.
//! 3. Assemble and return the view struct.
//!
//! Per-property reads are a round-trip each — Phase 3 prefers
//! "simple but obviously correct" over batched `Properties.GetAll`
//! calls; a future perf pass can tighten that once measurements
//! call for it.

use async_trait::async_trait;
use zbus::Connection;

use crate::errors::NexusctlError;
use crate::errors_map::from_zbus_error;
use crate::path_resolve::{
    resolve_bluetooth_device_by_address, resolve_gnss_by_device, resolve_interface_by_ifname,
    resolve_interfaces_of_kind, resolve_profile_by_ref,
};
use crate::proxy::bluetooth::BluetoothProxy;
use crate::proxy::bluetooth_device::BluetoothDeviceProxy;
use crate::proxy::ethernet::EthernetProxy;
use crate::proxy::gnss::GnssProxy;
use crate::proxy::interface::InterfaceProxy;
use crate::proxy::manager::ManagerProxy;
use crate::proxy::networkd::NetworkdManagerProxy;
use crate::proxy::profile::{EthernetProfileProxy, ProfileProxy, WifiProfileProxy};
use crate::proxy::resolve1::ResolveLinkProxy;
use crate::proxy::scan_result::ScanResultProxy;
use crate::proxy::wifi::WifiProxy;
use crate::proxy::{
    AltBss, BluetoothAdapterDetail, BluetoothAdapterSummary, BluetoothDeviceDetail,
    BluetoothDeviceSummary, BluetoothListFilter, EthernetDetail, EthernetProfileDetail, GnssDetail,
    GnssFix, GnssSatellitesView, InterfaceDetail, InterfaceSummary, IpDetail, ManagerOps,
    ManagerStatus, MasterKeyInfo, ProfileDetail, ProfileSummary, WifiDetail, WifiProfileDetail,
    WifiProfileSummary,
};

pub struct ZbusManagerOps {
    connection: Connection,
}

impl ZbusManagerOps {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

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

    /// Internal helper: build an `InterfaceSummary` for an already-
    /// resolved interface object path.
    async fn read_interface_summary(
        &self,
        iface: &InterfaceProxy<'_>,
    ) -> Result<InterfaceSummary, NexusctlError> {
        let ifname = iface.ifname().await.map_err(from_zbus_error)?;
        let kind = iface.kind().await.map_err(from_zbus_error)?;
        let state = iface.oper_state().await.map_err(from_zbus_error)?;
        let carrier = iface.carrier().await.map_err(from_zbus_error)?;
        let mac_bytes = iface.mac().await.map_err(from_zbus_error)?;
        let managed = iface.managed_profile().await.map_err(from_zbus_error)?;
        Ok(InterfaceSummary {
            iface: ifname,
            kind,
            state,
            mac: format_mac(&mac_bytes),
            carrier,
            managed_profile: normalise_profile_path(managed.as_str()),
        })
    }
}

#[async_trait]
impl ManagerOps for ZbusManagerOps {
    async fn get_manager_status(&self) -> Result<ManagerStatus, NexusctlError> {
        let proxy = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let dict = proxy.get_manager_status().await.map_err(from_zbus_error)?;
        let mut status = decode_manager_status(&dict);

        // BlueZ / gpsd availability is derived client-side from the
        // interface list. An adapter/GNSS interface in a
        // "past-unavailable" state implies the underlying daemon is
        // up.
        let rows = self.list_interfaces().await?;
        let mut eth = 0u32;
        let mut wifi = 0u32;
        let mut bt = 0u32;
        let mut gnss = 0u32;
        let mut bluez = false;
        for r in &rows {
            match r.kind.as_str() {
                "ethernet" => eth += 1,
                "wifi" | "wireless" => wifi += 1,
                "bluetooth" => {
                    bt += 1;
                    if r.state != "unavailable" && !r.state.is_empty() {
                        bluez = true;
                    }
                }
                "gnss" => gnss += 1,
                _ => {}
            }
        }
        // `gpsd_available` needs a property read per GNSS interface.
        let mut gpsd = false;
        for (_ifname, path) in resolve_interfaces_of_kind(&self.connection, "gnss").await? {
            let g = GnssProxy::builder(&self.connection)
                .path(path)
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            if g.gpsd_connected().await.unwrap_or(false) {
                gpsd = true;
                break;
            }
        }
        status.ethernet_count = eth;
        status.wifi_count = wifi;
        status.bluetooth_count = bt;
        status.gnss_count = gnss;
        status.bluez_available = bluez;
        status.gpsd_available = gpsd;
        Ok(status)
    }

    async fn list_interfaces(&self) -> Result<Vec<InterfaceSummary>, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let paths = mgr.interfaces().await.map_err(from_zbus_error)?;
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let proxy = InterfaceProxy::builder(&self.connection)
                .path(path)
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            out.push(self.read_interface_summary(&proxy).await?);
        }
        Ok(out)
    }

    async fn show_interface(&self, ifname: &str) -> Result<InterfaceDetail, NexusctlError> {
        let path = resolve_interface_by_ifname(&self.connection, ifname).await?;
        let iface = InterfaceProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let summary = self.read_interface_summary(&iface).await?;
        let ifindex = iface.ifindex().await.ok();
        let mut detail = InterfaceDetail {
            summary: summary.clone(),
            mtu: None, // MTU isn't exposed on fi.nexus.Interface today.
            ifindex,
            wifi: None,
            ethernet: None,
            bluetooth: None,
            gnss: None,
            ip: None,
        };
        match summary.kind.as_str() {
            "wifi" | "wireless" => {
                let w = WifiProxy::builder(&self.connection)
                    .path(path)
                    .map_err(from_zbus_error)?
                    .build()
                    .await
                    .map_err(from_zbus_error)?;
                let bss = w.connected_bss().await.map_err(from_zbus_error)?;
                let (ssid_str, ssid_bytes, bssid_bytes, freq, rssi, sec) = bss;
                let state = w.state().await.map_err(from_zbus_error)?;
                let alt_bsses = if state == "connected" {
                    read_alt_bsses(&self.connection, &w, &ssid_bytes, &bssid_bytes).await
                } else {
                    Vec::new()
                };
                detail.wifi = Some(WifiDetail {
                    state: state.clone(),
                    ssid: non_empty(ssid_str),
                    bssid: format_mac(&bssid_bytes),
                    frequency_mhz: freq,
                    signal_dbm: rssi,
                    security: sec,
                    supplicant: w.supplicant().await.map_err(from_zbus_error)?,
                    roaming_mode: w.roaming_mode().await.map_err(from_zbus_error)?,
                    powered: w.powered().await.map_err(from_zbus_error)?,
                    alt_bsses,
                });
                if state == "connected" {
                    if let Some(idx) = ifindex {
                        detail.ip = Some(read_ip_detail(&self.connection, idx).await);
                    }
                }
            }
            "ethernet" => {
                let e = EthernetProxy::builder(&self.connection)
                    .path(path)
                    .map_err(from_zbus_error)?
                    .build()
                    .await
                    .map_err(from_zbus_error)?;
                let state = e.state().await.map_err(from_zbus_error)?;
                detail.ethernet = Some(EthernetDetail {
                    state: state.clone(),
                    auth_backend: e.auth_backend().await.map_err(from_zbus_error)?,
                    auth_failure_reason: e.auth_failure_reason().await.map_err(from_zbus_error)?,
                    eap_method: e.eap_method().await.map_err(from_zbus_error)?,
                });
                if state == "link_ready" || state == "authenticated" {
                    if let Some(idx) = ifindex {
                        detail.ip = Some(read_ip_detail(&self.connection, idx).await);
                    }
                }
            }
            "bluetooth" => {
                let b = BluetoothProxy::builder(&self.connection)
                    .path(path)
                    .map_err(from_zbus_error)?
                    .build()
                    .await
                    .map_err(from_zbus_error)?;
                let known = b.known_devices().await.map_err(from_zbus_error)?;
                detail.bluetooth = Some(BluetoothAdapterDetail {
                    address: b.address().await.map_err(from_zbus_error)?,
                    powered: b.powered().await.map_err(from_zbus_error)?,
                    discoverable: b.discoverable().await.map_err(from_zbus_error)?,
                    pairable: b.pairable().await.map_err(from_zbus_error)?,
                    discovering: b.discovering().await.map_err(from_zbus_error)?,
                    nexus_discovering: b.nexus_discovering().await.map_err(from_zbus_error)?,
                    state: b.state().await.map_err(from_zbus_error)?,
                    known_device_paths: known.into_iter().map(|p| p.as_str().to_owned()).collect(),
                });
            }
            "gnss" => {
                detail.gnss = Some(read_gnss_detail(&self.connection, path).await?);
            }
            _ => {}
        }
        Ok(detail)
    }

    async fn list_bluetooth_adapters(&self) -> Result<Vec<BluetoothAdapterSummary>, NexusctlError> {
        let mut out = Vec::new();
        for (ifname, path) in resolve_interfaces_of_kind(&self.connection, "bluetooth").await? {
            let adapter = BluetoothProxy::builder(&self.connection)
                .path(path)
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            let devices = adapter.known_devices().await.map_err(from_zbus_error)?;
            out.push(BluetoothAdapterSummary {
                ifname,
                address: adapter.address().await.map_err(from_zbus_error)?,
                state: adapter.state().await.map_err(from_zbus_error)?,
                powered: adapter.powered().await.map_err(from_zbus_error)?,
                discovering: adapter.discovering().await.map_err(from_zbus_error)?,
                known_device_count: devices.len() as u32,
            });
        }
        Ok(out)
    }

    async fn list_bluetooth_devices(
        &self,
        filter: BluetoothListFilter,
    ) -> Result<Vec<BluetoothDeviceSummary>, NexusctlError> {
        let mut out = Vec::new();
        for (ifname, adapter_path) in
            resolve_interfaces_of_kind(&self.connection, "bluetooth").await?
        {
            let adapter = BluetoothProxy::builder(&self.connection)
                .path(adapter_path)
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            let devices = adapter.known_devices().await.map_err(from_zbus_error)?;
            for dpath in devices {
                let summary =
                    read_bluetooth_device_summary(&self.connection, &ifname, dpath).await?;
                let keep = match filter {
                    BluetoothListFilter::All => true,
                    BluetoothListFilter::Paired => summary.paired,
                    BluetoothListFilter::Connected => summary.connected,
                };
                if keep {
                    out.push(summary);
                }
            }
        }
        Ok(out)
    }

    async fn show_bluetooth_device(
        &self,
        address: &str,
    ) -> Result<BluetoothDeviceDetail, NexusctlError> {
        let path = resolve_bluetooth_device_by_address(&self.connection, address).await?;
        let dev = BluetoothDeviceProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let adapter_path = dev.adapter().await.map_err(from_zbus_error)?;
        let adapter_ifname = last_path_component(adapter_path.as_str());
        let summary =
            read_bluetooth_device_summary(&self.connection, &adapter_ifname, path.clone()).await?;
        Ok(BluetoothDeviceDetail {
            summary,
            address_type: dev.address_type().await.map_err(from_zbus_error)?,
            alias: dev.alias().await.map_err(from_zbus_error)?,
            tx_power: dev.tx_power().await.map_err(from_zbus_error)?,
            uuids: dev.uuids().await.map_err(from_zbus_error)?,
            blocked: dev.blocked().await.map_err(from_zbus_error)?,
            profile_path: normalise_profile_path(
                dev.profile().await.map_err(from_zbus_error)?.as_str(),
            ),
        })
    }

    async fn gnss_satellites(
        &self,
        device: Option<&str>,
    ) -> Result<GnssSatellitesView, NexusctlError> {
        let path = resolve_gnss_by_device(&self.connection, device).await?;
        let proxy = GnssProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        Ok(GnssSatellitesView {
            device: proxy.device_path().await.map_err(from_zbus_error)?,
            in_view: proxy.satellites_in_view().await.map_err(from_zbus_error)?,
            used: proxy.satellites_used().await.map_err(from_zbus_error)?,
        })
    }

    async fn list_profiles(
        &self,
        kind_filter: Option<&str>,
    ) -> Result<Vec<ProfileSummary>, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let mut paths = Vec::new();
        if kind_filter.map(|k| k == "wifi").unwrap_or(true) {
            paths.extend(mgr.wifi_profiles().await.map_err(from_zbus_error)?);
        }
        if kind_filter.map(|k| k == "ethernet").unwrap_or(true) {
            paths.extend(mgr.ethernet_profiles().await.map_err(from_zbus_error)?);
        }
        let mut out = Vec::new();
        for path in paths {
            let p = ProfileProxy::builder(&self.connection)
                .path(path)
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            out.push(ProfileSummary {
                id: p.id().await.map_err(from_zbus_error)?,
                kind: p.kind().await.map_err(from_zbus_error)?,
                label: p.label().await.map_err(from_zbus_error)?,
                credentials_invalid: p.credentials_invalid().await.map_err(from_zbus_error)?,
                created_at: p.created_at().await.map_err(from_zbus_error)?,
                updated_at: p.updated_at().await.map_err(from_zbus_error)?,
            });
        }
        Ok(out)
    }

    async fn list_wifi_profiles(&self) -> Result<Vec<WifiProfileSummary>, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let paths = mgr.wifi_profiles().await.map_err(from_zbus_error)?;
        let mut out = Vec::with_capacity(paths.len());
        for path in paths {
            let common = ProfileProxy::builder(&self.connection)
                .path(path.clone())
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            let wifi = WifiProfileProxy::builder(&self.connection)
                .path(path)
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            let security = wifi.security().await.map_err(from_zbus_error)?;
            let security_type = security
                .get("type")
                .and_then(|v| <&str>::try_from(v).ok())
                .map(str::to_owned)
                .unwrap_or_default();
            let ssid_bytes = wifi.ssid().await.map_err(from_zbus_error)?;
            out.push(WifiProfileSummary {
                id: common.id().await.map_err(from_zbus_error)?,
                ssid: String::from_utf8_lossy(&ssid_bytes).into_owned(),
                label: common.label().await.map_err(from_zbus_error)?,
                security_type,
                priority: wifi.priority().await.map_err(from_zbus_error)?,
                auto_connect: wifi.auto_connect().await.map_err(from_zbus_error)?,
                hidden: wifi.hidden().await.map_err(from_zbus_error)?,
                credentials_invalid: common.credentials_invalid().await.map_err(from_zbus_error)?,
            });
        }
        Ok(out)
    }

    async fn show_profile(&self, reference: &str) -> Result<ProfileDetail, NexusctlError> {
        let path = resolve_profile_by_ref(&self.connection, reference).await?;
        let base = ProfileProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let summary = ProfileSummary {
            id: base.id().await.map_err(from_zbus_error)?,
            kind: base.kind().await.map_err(from_zbus_error)?,
            label: base.label().await.map_err(from_zbus_error)?,
            credentials_invalid: base.credentials_invalid().await.map_err(from_zbus_error)?,
            created_at: base.created_at().await.map_err(from_zbus_error)?,
            updated_at: base.updated_at().await.map_err(from_zbus_error)?,
        };
        let mut detail = ProfileDetail {
            summary: summary.clone(),
            wifi: None,
            ethernet: None,
        };
        match summary.kind.as_str() {
            "wifi" => {
                let w = WifiProfileProxy::builder(&self.connection)
                    .path(path)
                    .map_err(from_zbus_error)?
                    .build()
                    .await
                    .map_err(from_zbus_error)?;
                let ssid_bytes = w.ssid().await.map_err(from_zbus_error)?;
                let security = w.security().await.map_err(from_zbus_error)?;
                let security_type = security
                    .get("type")
                    .and_then(|v| <&str>::try_from(v).ok())
                    .map(str::to_owned)
                    .unwrap_or_default();
                let has_creds: Vec<String> = w
                    .has_credentials()
                    .await
                    .map_err(from_zbus_error)?
                    .into_iter()
                    .filter(|(_, v)| *v)
                    .map(|(k, _)| k)
                    .collect();
                detail.wifi = Some(WifiProfileDetail {
                    ssid: String::from_utf8_lossy(&ssid_bytes).into_owned(),
                    hidden: w.hidden().await.map_err(from_zbus_error)?,
                    priority: w.priority().await.map_err(from_zbus_error)?,
                    auto_connect: w.auto_connect().await.map_err(from_zbus_error)?,
                    fast_transition: w.fast_transition().await.map_err(from_zbus_error)?,
                    security_type,
                    has_credentials: has_creds,
                    bssid_preferred: format_mac(
                        &w.bssid_preferred().await.map_err(from_zbus_error)?,
                    ),
                    bssid_blacklist: w
                        .bssid_blacklist()
                        .await
                        .map_err(from_zbus_error)?
                        .into_iter()
                        .filter_map(|b| format_mac(&b))
                        .collect(),
                    scan_frequencies: w.scan_frequencies().await.map_err(from_zbus_error)?,
                });
            }
            "ethernet" => {
                let e = EthernetProfileProxy::builder(&self.connection)
                    .path(path)
                    .map_err(from_zbus_error)?
                    .build()
                    .await
                    .map_err(from_zbus_error)?;
                let has_creds: Vec<String> = e
                    .has_credentials()
                    .await
                    .map_err(from_zbus_error)?
                    .into_iter()
                    .filter(|(_, v)| *v)
                    .map(|(k, _)| k)
                    .collect();
                detail.ethernet = Some(EthernetProfileDetail {
                    ifname: e.ifname().await.map_err(from_zbus_error)?,
                    auto_connect: e.auto_connect().await.map_err(from_zbus_error)?,
                    dot1x_enabled: e.dot1x_enabled().await.map_err(from_zbus_error)?,
                    dot1x_eap: e.dot1x_eap().await.map_err(from_zbus_error)?,
                    has_credentials: has_creds,
                });
            }
            _ => {}
        }
        Ok(detail)
    }

    async fn export_profile(&self, reference: &str) -> Result<String, NexusctlError> {
        // The daemon doesn't expose a `Profile.Export()` method, so
        // we assemble the TOML from the properties. Credentials
        // aren't available over D-Bus; exported profiles are
        // skeletons the operator fills in on import.
        let detail = self.show_profile(reference).await?;
        Ok(profile_to_toml(&detail))
    }

    async fn master_key_info(&self) -> Result<MasterKeyInfo, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        Ok(MasterKeyInfo {
            source: mgr.master_key_source().await.map_err(from_zbus_error)?,
        })
    }

    // ---- Mutating impls ----

    async fn wifi_scan(
        &self,
        ifname: &str,
    ) -> Result<Vec<crate::proxy::WifiScanResult>, NexusctlError> {
        let path = resolve_interface_by_ifname(&self.connection, ifname).await?;
        let wifi = WifiProxy::builder(&self.connection)
            .path(path.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let empty: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
            std::collections::HashMap::new();
        // The new wire shape returns a `(job_id: s)` tuple per
        // DD-006 §6.3; the CLI doesn't subscribe to ScanComplete
        // yet (Phase 7.6 work) so we discard the id and keep the
        // poll-on-ScanResults loop below.
        wifi.scan(empty)
            .await
            .map(|_job_id| ())
            .map_err(from_zbus_error)?;
        // Phase 4 polls ScanResults for a short window rather than
        // subscribing to `ScanComplete` — the signal flow lands in
        // Phase 7.6 alongside the `watch` machinery. Two second
        // window with 200 ms poll matches wpa_supplicant's typical
        // scan latency.
        let mut last_len = usize::MAX;
        let mut stable_ticks = 0u8;
        let mut result_paths = Vec::new();
        for _ in 0..10 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            result_paths = wifi.scan_results().await.map_err(from_zbus_error)?;
            if result_paths.len() == last_len {
                stable_ticks += 1;
                if stable_ticks >= 2 {
                    break;
                }
            } else {
                stable_ticks = 0;
            }
            last_len = result_paths.len();
        }
        // Decode each scan-result object.
        let mut out = Vec::with_capacity(result_paths.len());
        for p in result_paths {
            let proxy = crate::proxy::scan_result::ScanResultProxy::builder(&self.connection)
                .path(p)
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            let bssid = proxy.bssid().await.map_err(from_zbus_error)?;
            let ssid = proxy.ssid().await.map_err(from_zbus_error)?;
            out.push(crate::proxy::WifiScanResult {
                ssid: String::from_utf8_lossy(&ssid).into_owned(),
                bssid: format_mac(&bssid).unwrap_or_default(),
                frequency_mhz: proxy.frequency().await.map_err(from_zbus_error)?,
                signal_dbm: proxy.signal_dbm().await.map_err(from_zbus_error)?,
                security: proxy.security_offered().await.map_err(from_zbus_error)?,
                age_ms: proxy.age_ms().await.map_err(from_zbus_error)?,
            });
        }
        Ok(out)
    }

    async fn wifi_connect_profile(
        &self,
        ifname: &str,
        profile_path: &str,
    ) -> Result<(), NexusctlError> {
        let path = resolve_interface_by_ifname(&self.connection, ifname).await?;
        let wifi = WifiProxy::builder(&self.connection)
            .path(path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let p = zbus::zvariant::ObjectPath::try_from(profile_path).map_err(|e| {
            NexusctlError::InvalidArgument {
                message: format!("bad profile path `{profile_path}`: {e}"),
            }
        })?;
        // The new wire shape returns a `(job_id: s)` tuple per
        // DD-006 §6.3; the CLI doesn't surface the id today (no
        // wait-on-ConnectComplete loop yet), so discard it.
        wifi.connect(p)
            .await
            .map(|_job_id| ())
            .map_err(from_zbus_error)
    }

    async fn wifi_disconnect(
        &self,
        ifname: &str,
        pause_auto_connect: bool,
    ) -> Result<(), NexusctlError> {
        let path = resolve_interface_by_ifname(&self.connection, ifname).await?;
        let wifi = WifiProxy::builder(&self.connection)
            .path(path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let mut params: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
            std::collections::HashMap::new();
        if pause_auto_connect {
            if let Ok(v) =
                zbus::zvariant::OwnedValue::try_from(zbus::zvariant::Value::new(true))
            {
                params.insert("pause_auto_connect".to_owned(), v);
            }
        }
        // `Disconnect` likewise returns `(job_id: s)`. Same
        // forward-compat treatment as `wifi_connect_profile` above.
        wifi.disconnect(params)
            .await
            .map(|_job_id| ())
            .map_err(from_zbus_error)
    }

    async fn find_wifi_profile(&self, ssid: &[u8]) -> Result<String, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let path = mgr.find_wifi_profile(ssid).await.map_err(from_zbus_error)?;
        Ok(path.as_str().to_owned())
    }

    async fn bt_set_powered(&self, adapter: &str, on: bool) -> Result<(), NexusctlError> {
        let path = resolve_interface_by_ifname(&self.connection, adapter).await?;
        let bt = BluetoothProxy::builder(&self.connection)
            .path(path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        bt.set_powered(on).await.map_err(from_zbus_error)
    }

    async fn bt_scan(
        &self,
        adapter: Option<&str>,
        duration: std::time::Duration,
    ) -> Result<Vec<BluetoothDeviceSummary>, NexusctlError> {
        let adapter_path = match adapter {
            Some(name) => resolve_interface_by_ifname(&self.connection, name).await?,
            None => {
                let list = resolve_interfaces_of_kind(&self.connection, "bluetooth").await?;
                if list.is_empty() {
                    return Err(NexusctlError::NotFound {
                        reference: "<any bluetooth adapter>".into(),
                    });
                }
                if list.len() > 1 {
                    let names: Vec<String> = list.into_iter().map(|(n, _)| n).collect();
                    return Err(NexusctlError::InvalidArgument {
                        message: format!(
                            "multiple bluetooth adapters; specify one: {}",
                            names.join(", ")
                        ),
                    });
                }
                list.into_iter().next().unwrap().1
            }
        };
        let bt = BluetoothProxy::builder(&self.connection)
            .path(adapter_path.clone())
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let empty: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
            std::collections::HashMap::new();
        bt.start_discovery(empty).await.map_err(from_zbus_error)?;
        tokio::time::sleep(duration).await;
        let stop_result = bt.stop_discovery().await;
        // List known devices regardless of stop_discovery outcome —
        // an error from StopDiscovery after a successful session is
        // usually "nothing to stop" which we can ignore.
        let ifname = {
            let iface = InterfaceProxy::builder(&self.connection)
                .path(adapter_path.clone())
                .map_err(from_zbus_error)?
                .build()
                .await
                .map_err(from_zbus_error)?;
            iface.ifname().await.map_err(from_zbus_error)?
        };
        let mut out = Vec::new();
        let devices = bt.known_devices().await.map_err(from_zbus_error)?;
        for dpath in devices {
            out.push(read_bluetooth_device_summary(&self.connection, &ifname, dpath).await?);
        }
        // Only surface the stop error if we couldn't recover device
        // data — otherwise report success with the (possibly stale)
        // list.
        if out.is_empty() && stop_result.is_err() {
            let _ = stop_result.map_err(from_zbus_error)?;
        }
        Ok(out)
    }

    async fn bt_connect_device(&self, address: &str) -> Result<(), NexusctlError> {
        let path = resolve_bluetooth_device_by_address(&self.connection, address).await?;
        let dev = BluetoothDeviceProxy::builder(&self.connection)
            .path(path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        dev.connect().await.map_err(from_zbus_error)
    }

    async fn bt_disconnect_device(&self, address: &str) -> Result<(), NexusctlError> {
        let path = resolve_bluetooth_device_by_address(&self.connection, address).await?;
        let dev = BluetoothDeviceProxy::builder(&self.connection)
            .path(path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        dev.disconnect().await.map_err(from_zbus_error)
    }

    async fn bt_forget_device(&self, address: &str) -> Result<(), NexusctlError> {
        let path = resolve_bluetooth_device_by_address(&self.connection, address).await?;
        let dev = BluetoothDeviceProxy::builder(&self.connection)
            .path(path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        dev.forget().await.map_err(from_zbus_error)
    }

    async fn bt_set_trusted(&self, address: &str, on: bool) -> Result<(), NexusctlError> {
        let path = resolve_bluetooth_device_by_address(&self.connection, address).await?;
        let dev = BluetoothDeviceProxy::builder(&self.connection)
            .path(path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        dev.set_trusted(on).await.map_err(from_zbus_error)
    }

    async fn add_wifi_profile(
        &self,
        settings: crate::proxy::WifiProfileSettings,
    ) -> Result<String, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let dict = build_wifi_settings_dict(&settings)?;
        let path = mgr.add_wifi_profile(dict).await.map_err(from_zbus_error)?;
        Ok(last_path_component(path.as_str()))
    }

    async fn add_ethernet_profile(
        &self,
        settings: crate::proxy::EthernetProfileSettings,
    ) -> Result<String, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let dict = build_ethernet_settings_dict(&settings)?;
        let path = mgr
            .add_ethernet_profile(dict)
            .await
            .map_err(from_zbus_error)?;
        Ok(last_path_component(path.as_str()))
    }

    async fn remove_profile(&self, reference: &str) -> Result<(), NexusctlError> {
        // Accept a ULID, label, or raw object path.
        let path = if reference.starts_with("/fi/nexus1/profile/") {
            zbus::zvariant::OwnedObjectPath::try_from(reference).map_err(|e| {
                NexusctlError::InvalidArgument {
                    message: format!("bad path `{reference}`: {e}"),
                }
            })?
        } else {
            resolve_profile_by_ref(&self.connection, reference).await?
        };
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        mgr.remove_profile(path.as_ref())
            .await
            .map_err(from_zbus_error)
    }

    async fn update_profile_field(
        &self,
        reference: &str,
        field: &str,
        value: &str,
    ) -> Result<(), NexusctlError> {
        let path = resolve_profile_by_ref(&self.connection, reference).await?;
        let proxy = ProfileProxy::builder(&self.connection)
            .path(path)
            .map_err(from_zbus_error)?
            .build()
            .await
            .map_err(from_zbus_error)?;
        let mut settings: std::collections::HashMap<String, zbus::zvariant::OwnedValue> =
            std::collections::HashMap::new();
        let v = parse_scalar_value(field, value)?;
        settings.insert(field.to_owned(), v);
        proxy.update(settings).await.map_err(from_zbus_error)
    }

    async fn set_power_state(&self, state: &str) -> Result<(), NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        mgr.set_power_state(state).await.map_err(from_zbus_error)
    }

    async fn rotate_master_key(&self) -> Result<String, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        mgr.rotate_master_key().await.map_err(from_zbus_error)
    }

    async fn freeze_for_backup(&self) -> Result<String, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        mgr.freeze_for_backup().await.map_err(from_zbus_error)
    }

    async fn release_backup_lease(&self, lease: &str) -> Result<(), NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        mgr.release_backup_lease(lease)
            .await
            .map_err(from_zbus_error)
    }

    async fn reload_config(&self) -> Result<crate::proxy::ReloadConfigReport, NexusctlError> {
        let mgr = ManagerProxy::new(&self.connection)
            .await
            .map_err(from_zbus_error)?;
        let dict = mgr.reload_config().await.map_err(from_zbus_error)?;
        Ok(decode_reload_report(&dict))
    }
}

async fn read_bluetooth_device_summary(
    conn: &Connection,
    adapter_ifname: &str,
    dpath: zbus::zvariant::OwnedObjectPath,
) -> Result<BluetoothDeviceSummary, NexusctlError> {
    let dev = BluetoothDeviceProxy::builder(conn)
        .path(dpath)
        .map_err(from_zbus_error)?
        .build()
        .await
        .map_err(from_zbus_error)?;
    Ok(BluetoothDeviceSummary {
        adapter: adapter_ifname.to_owned(),
        address: dev.address().await.map_err(from_zbus_error)?,
        name: dev.name().await.map_err(from_zbus_error)?,
        state: dev.state().await.map_err(from_zbus_error)?,
        paired: dev.paired().await.map_err(from_zbus_error)?,
        bonded: dev.bonded().await.map_err(from_zbus_error)?,
        trusted: dev.trusted().await.map_err(from_zbus_error)?,
        connected: dev.connected().await.map_err(from_zbus_error)?,
        rssi: dev.rssi().await.map_err(from_zbus_error)?,
        transport: dev.transport().await.map_err(from_zbus_error)?,
    })
}

async fn read_gnss_detail(
    conn: &Connection,
    path: zbus::zvariant::OwnedObjectPath,
) -> Result<GnssDetail, NexusctlError> {
    let g = GnssProxy::builder(conn)
        .path(path)
        .map_err(from_zbus_error)?
        .build()
        .await
        .map_err(from_zbus_error)?;
    let fix_tuple = g.last_fix().await.map_err(from_zbus_error)?;
    let last_fix = if fix_tuple.1 >= 2 {
        Some(GnssFix {
            time_unix_ms: fix_tuple.0,
            mode: fix_tuple.1,
            latitude: fix_tuple.2,
            longitude: fix_tuple.3,
            altitude_m: fix_tuple.4,
            speed_mps: fix_tuple.5,
            track_deg: fix_tuple.6,
            horizontal_error_m: fix_tuple.7,
            vertical_error_m: fix_tuple.8,
            satellites_used: fix_tuple.9,
        })
    } else {
        None
    };
    Ok(GnssDetail {
        state: g.state().await.map_err(from_zbus_error)?,
        device_path: g.device_path().await.map_err(from_zbus_error)?,
        vendor_model: g.vendor_model().await.map_err(from_zbus_error)?,
        gpsd_connected: g.gpsd_connected().await.map_err(from_zbus_error)?,
        satellites_in_view: g.satellites_in_view().await.map_err(from_zbus_error)?,
        satellites_used: g.satellites_used().await.map_err(from_zbus_error)?,
        horizontal_error_m: g.horizontal_error_m().await.map_err(from_zbus_error)?,
        last_fix,
    })
}

// ---------------------------------------------------------------------------
// Decoders + utilities
// ---------------------------------------------------------------------------

pub(crate) fn format_mac(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    Some(
        bytes
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

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
        ethernet_count: 0,
        wifi_count: 0,
        bluetooth_count: 0,
        gnss_count: 0,
        wifi_profile_count: u("WifiProfileCount"),
        ethernet_profile_count: u("EthernetProfileCount"),
        bluetooth_profile_count: u("BluetoothProfileCount"),
        master_key_source: s("MasterKeySource"),
        bluez_available: false,
        gpsd_available: false,
        internet_connectivity: s("InternetConnectivity"),
    }
}

/// Empty SSIDs come across as empty strings; return `None` so
/// downstream renderers display a dash rather than an empty cell.
fn non_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

/// Filter the sentinel "no profile attached" object path (`/`). Any
/// other value is returned wrapped in `Some`.
fn normalise_profile_path(p: &str) -> Option<String> {
    if p.is_empty() || p == "/" {
        None
    } else {
        Some(p.to_owned())
    }
}

fn last_path_component(p: &str) -> String {
    p.rsplit('/').next().unwrap_or("").to_owned()
}

/// Build an `a{sv}` settings dict for `AddWifiProfile`. Matches the
/// shape documented in DD-006 §16.2.
fn build_wifi_settings_dict(
    settings: &crate::proxy::WifiProfileSettings,
) -> Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>, NexusctlError> {
    use zbus::zvariant::{OwnedValue, Value};
    let mut out: std::collections::HashMap<String, OwnedValue> = std::collections::HashMap::new();
    let ssid: OwnedValue =
        OwnedValue::try_from(Value::new(settings.ssid.clone())).map_err(|e| {
            NexusctlError::InvalidArgument {
                message: format!("encoding ssid: {e}"),
            }
        })?;
    out.insert("ssid".into(), ssid);
    if let Some(l) = &settings.label {
        if let Ok(v) = OwnedValue::try_from(Value::new(l.clone())) {
            out.insert("label".into(), v);
        }
    }
    if let Some(p) = settings.priority {
        if let Ok(v) = OwnedValue::try_from(Value::new(p)) {
            out.insert("priority".into(), v);
        }
    }
    if let Some(b) = settings.auto_connect {
        if let Ok(v) = OwnedValue::try_from(Value::new(b)) {
            out.insert("auto_connect".into(), v);
        }
    }
    if let Some(b) = settings.hidden {
        if let Ok(v) = OwnedValue::try_from(Value::new(b)) {
            out.insert("hidden".into(), v);
        }
    }
    if let Some(b) = settings.fast_transition {
        if let Ok(v) = OwnedValue::try_from(Value::new(b)) {
            out.insert("fast_transition".into(), v);
        }
    }
    // Security sub-dict.
    let mut security: std::collections::HashMap<String, OwnedValue> =
        std::collections::HashMap::new();
    if let Ok(v) = OwnedValue::try_from(Value::new(settings.security_type.clone())) {
        security.insert("type".into(), v);
    }
    if let Some(p) = &settings.passphrase {
        if let Ok(v) = OwnedValue::try_from(Value::new(p.clone())) {
            security.insert("passphrase".into(), v);
        }
    }
    if let Ok(sec) = OwnedValue::try_from(Value::new(security)) {
        out.insert("security".into(), sec);
    }
    Ok(out)
}

fn build_ethernet_settings_dict(
    settings: &crate::proxy::EthernetProfileSettings,
) -> Result<std::collections::HashMap<String, zbus::zvariant::OwnedValue>, NexusctlError> {
    use zbus::zvariant::{OwnedValue, Value};
    let mut out: std::collections::HashMap<String, OwnedValue> = std::collections::HashMap::new();
    if let Ok(v) = OwnedValue::try_from(Value::new(settings.ifname.clone())) {
        out.insert("ifname".into(), v);
    }
    if let Some(l) = &settings.label {
        if let Ok(v) = OwnedValue::try_from(Value::new(l.clone())) {
            out.insert("label".into(), v);
        }
    }
    if let Some(b) = settings.auto_connect {
        if let Ok(v) = OwnedValue::try_from(Value::new(b)) {
            out.insert("auto_connect".into(), v);
        }
    }
    Ok(out)
}

/// Decode a typed scalar out of a `<field>, <value>` CLI pair.
/// Bool fields round-trip "true"/"false"; integers try i32; the
/// fallback is a plain string. This is the minimum nexusctl needs
/// for `profile update --field label --value "home"` style edits.
fn parse_scalar_value(field: &str, raw: &str) -> Result<zbus::zvariant::OwnedValue, NexusctlError> {
    use zbus::zvariant::{OwnedValue, Value};
    if raw == "true" || raw == "false" {
        return OwnedValue::try_from(Value::new(raw == "true")).map_err(|e| {
            NexusctlError::InvalidArgument {
                message: format!("encoding {field}: {e}"),
            }
        });
    }
    if let Ok(n) = raw.parse::<i32>() {
        return OwnedValue::try_from(Value::new(n)).map_err(|e| NexusctlError::InvalidArgument {
            message: format!("encoding {field}: {e}"),
        });
    }
    OwnedValue::try_from(Value::new(raw.to_owned())).map_err(|e| NexusctlError::InvalidArgument {
        message: format!("encoding {field}: {e}"),
    })
}

/// Decode the `a{sv}` report from `Manager.ReloadConfig`. Same
/// keys as DD-006 §5.2.
fn decode_reload_report(
    dict: &std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
) -> crate::proxy::ReloadConfigReport {
    let str_array = |key: &str| -> Vec<String> {
        dict.get(key)
            .and_then(|v| <&zbus::zvariant::Array>::try_from(v).ok())
            .map(|a| {
                a.iter()
                    .filter_map(|item| <&str>::try_from(item).ok().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let errors: Vec<(String, String)> = dict
        .get("errors")
        .and_then(|v| <&zbus::zvariant::Array>::try_from(v).ok())
        .map(|a| {
            a.iter()
                .filter_map(|item| {
                    let s: &zbus::zvariant::Structure = item.downcast_ref().ok()?;
                    let fields = s.fields();
                    let a = <&str>::try_from(&fields[0]).ok()?.to_owned();
                    let b = <&str>::try_from(&fields[1]).ok()?.to_owned();
                    Some((a, b))
                })
                .collect()
        })
        .unwrap_or_default();
    crate::proxy::ReloadConfigReport {
        applied: str_array("applied"),
        deferred: str_array("deferred"),
        errors,
    }
}

/// Assemble a minimal TOML document from a `ProfileDetail`.
/// Credentials are *not* included — they never leave the daemon
/// via D-Bus. Operators edit the exported file and re-import it,
/// supplying fresh credentials at import time.
fn profile_to_toml(detail: &ProfileDetail) -> String {
    let mut out = String::new();
    out.push_str(&format!("# id = {}\n", detail.summary.id));
    out.push_str(&format!("kind = {:?}\n", detail.summary.kind));
    out.push_str(&format!("label = {:?}\n", detail.summary.label));
    if let Some(w) = &detail.wifi {
        out.push_str("\n[wifi]\n");
        out.push_str(&format!("ssid = {:?}\n", w.ssid));
        out.push_str(&format!("hidden = {}\n", w.hidden));
        out.push_str(&format!("priority = {}\n", w.priority));
        out.push_str(&format!("auto_connect = {}\n", w.auto_connect));
        out.push_str(&format!("fast_transition = {}\n", w.fast_transition));
        out.push_str(&format!("security_type = {:?}\n", w.security_type));
        out.push_str("# credentials are not exported; provide them at import time\n");
    }
    if let Some(e) = &detail.ethernet {
        out.push_str("\n[ethernet]\n");
        out.push_str(&format!("ifname = {:?}\n", e.ifname));
        out.push_str(&format!("auto_connect = {}\n", e.auto_connect));
        out.push_str(&format!("dot1x_enabled = {}\n", e.dot1x_enabled));
        out.push_str(&format!("dot1x_eap = {:?}\n", e.dot1x_eap));
        out.push_str("# credentials are not exported; provide them at import time\n");
    }
    out
}

// ---------------------------------------------------------------------------
// IP-layer + alt-BSS readers (off-Nexus / scan-cache walks)
// ---------------------------------------------------------------------------

/// Walk the connected interface's `Wifi.ScanResults`, keep entries
/// whose `Ssid` (ay) is byte-equal to the connected `ssid_bytes`
/// and whose `Bssid` is NOT the connected BSSID, sort by signal
/// desc, return one [`AltBss`] row each. Errors at any layer
/// degrade to "no alternatives" rather than failing the show
/// command — the recipe is best-effort UI enrichment.
async fn read_alt_bsses(
    conn: &Connection,
    wifi: &WifiProxy<'_>,
    ssid_bytes: &[u8],
    connected_bssid: &[u8],
) -> Vec<AltBss> {
    let paths = match wifi.scan_results().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<AltBss> = Vec::with_capacity(paths.len());
    for p in paths {
        let proxy = match ScanResultProxy::builder(conn).path(p) {
            Ok(b) => match b.build().await {
                Ok(p) => p,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        let ssid = match proxy.ssid().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if ssid != ssid_bytes {
            continue;
        }
        let bssid = match proxy.bssid().await {
            Ok(v) => v,
            Err(_) => continue,
        };
        if bssid == connected_bssid {
            continue;
        }
        let bssid_str = match format_mac(&bssid) {
            Some(s) => s,
            None => continue,
        };
        let frequency_mhz = proxy.frequency().await.unwrap_or(0);
        let signal_dbm = proxy.signal_dbm().await.unwrap_or(0);
        let security = proxy.security_offered().await.unwrap_or_default();
        out.push(AltBss {
            bssid: bssid_str,
            frequency_mhz,
            signal_dbm,
            security,
        });
    }
    out.sort_by_key(|r| std::cmp::Reverse(r.signal_dbm));
    out
}

/// Read networkd `Manager.DescribeLink(ifindex)` + resolved
/// `Link.DNS` and decode them into an [`IpDetail`]. Best-effort:
/// a missing daemon turns into the appropriate `*_unavailable`
/// marker rather than a hard failure, per the
/// connected-card-recipe contract.
async fn read_ip_detail(conn: &Connection, ifindex: u32) -> IpDetail {
    let mut detail = IpDetail::default();
    match NetworkdManagerProxy::new(conn).await {
        Ok(mgr) => match read_networkd_link_json(&mgr, ifindex).await {
            Ok(v) => decode_networkd_describe(&v, &mut detail),
            Err(reason) => detail.networkd_unavailable = Some(reason),
        },
        Err(_) => {
            detail.networkd_unavailable = Some("networkd not on bus".into());
        }
    }
    // Try resolved second. When it answers we OVERRIDE the
    // networkd-derived DNS list (resolved aggregates per-link
    // configuration with global / fallback / system entries). When
    // resolved is missing, leave the networkd-derived DNS in place
    // and only emit the "resolved not on bus" marker if networkd
    // didn't surface DNS either — otherwise the row would falsely
    // report no DNS on a working device.
    let resolve_path = format!("/org/freedesktop/resolve1/link/_{ifindex}");
    let resolved_dns = read_resolved_dns(conn, &resolve_path).await;
    match resolved_dns {
        Ok(list) => detail.dns = list,
        Err(_) if !detail.dns.is_empty() => { /* networkd DNS suffices */ }
        Err(_) => {
            detail.resolved_unavailable = Some("resolved not on bus".into());
        }
    }
    detail
}

async fn read_resolved_dns(conn: &Connection, path: &str) -> Result<Vec<String>, ()> {
    let object_path = zbus::zvariant::ObjectPath::try_from(path).map_err(|_| ())?;
    let proxy = ResolveLinkProxy::builder(conn)
        .path(object_path)
        .map_err(|_| ())?
        .build()
        .await
        .map_err(|_| ())?;
    let entries = proxy.dns().await.map_err(|_| ())?;
    Ok(entries
        .into_iter()
        .filter_map(|(family, bytes)| format_ip_address(family, &bytes))
        .collect())
}

/// Fetch the per-link JSON blob, preferring `DescribeLink(ifindex)`
/// (systemd 250+) and falling back to the no-arg `Describe()`
/// manager dump on older systemds — the latter returns a manager
/// state with `Interfaces[]`, from which we pluck the entry whose
/// `Index` matches our ifindex.
async fn read_networkd_link_json(
    mgr: &NetworkdManagerProxy<'_>,
    ifindex: u32,
) -> Result<serde_json::Value, String> {
    match mgr.describe_link(ifindex as i32).await {
        Ok(json) => serde_json::from_str(&json)
            .map_err(|e| format!("DescribeLink JSON parse failed: {e}")),
        Err(link_err) => match mgr.describe().await {
            Ok(json) => {
                let v: serde_json::Value = serde_json::from_str(&json)
                    .map_err(|e| format!("Describe JSON parse failed: {e}"))?;
                v.get("Interfaces")
                    .and_then(|a| a.as_array())
                    .and_then(|arr| {
                        arr.iter().find(|item| {
                            item.get("Index")
                                .and_then(|i| i.as_u64())
                                .map(|i| i as u32)
                                == Some(ifindex)
                        })
                    })
                    .cloned()
                    .ok_or_else(|| {
                        format!("ifindex {ifindex} not found in networkd Describe()")
                    })
            }
            Err(_) => Err(format!("DescribeLink failed: {link_err}")),
        },
    }
}

/// Pull IPv4/IPv6 address + default-gateway + DNS rows out of the
/// JSON blob returned by
/// `org.freedesktop.network1.Manager.DescribeLink` (or extracted
/// from the manager-wide `Describe()` fallback). Field rules per
/// integration-knowledge-graph
/// `flow:read-ip-info-for-connected-iface`:
///   - IPv4 address: `Family==2 && Scope==0`, decode 4 bytes.
///   - IPv6 address: `Family==10`, prefer `Scope==0` (global)
///     over `Scope==253` (link-local).
///   - Default route: `DestinationPrefixLength==0`.
///   - DNS: per-link `DNS[]` array (each entry `{Family, Address}`).
///     Used as the DNS-row fallback when systemd-resolved isn't on
///     the bus (stripped-down rootfs); when resolved is reachable
///     its `Link.DNS` overrides this list.
fn decode_networkd_describe(v: &serde_json::Value, out: &mut IpDetail) {
    if let Some(addrs) = v.get("Addresses").and_then(|a| a.as_array()) {
        let mut ipv6_global: Option<String> = None;
        let mut ipv6_link: Option<String> = None;
        for a in addrs {
            let family = a.get("Family").and_then(|x| x.as_i64()).unwrap_or(0);
            let scope = a.get("Scope").and_then(|x| x.as_i64()).unwrap_or(0);
            let prefix = a
                .get("PrefixLength")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u8;
            let bytes = parse_addr_bytes(a.get("Address"));
            if family == 2 && scope == 0 && bytes.len() == 4 && out.ipv4_address.is_none() {
                out.ipv4_address = Some(format_ipv4(&bytes));
                out.ipv4_prefix_length = Some(prefix);
            } else if family == 10 && bytes.len() == 16 {
                let s = format_ipv6(&bytes);
                if scope == 0 && ipv6_global.is_none() {
                    ipv6_global = Some(s);
                } else if scope == 253 && ipv6_link.is_none() {
                    ipv6_link = Some(s);
                }
            }
        }
        out.ipv6_address = ipv6_global.or(ipv6_link);
    }
    if let Some(routes) = v.get("Routes").and_then(|a| a.as_array()) {
        for r in routes {
            let family = r.get("Family").and_then(|x| x.as_i64()).unwrap_or(0);
            let dpl = r
                .get("DestinationPrefixLength")
                .and_then(|x| x.as_u64())
                .unwrap_or(u64::MAX);
            if dpl != 0 {
                continue;
            }
            let gw = parse_addr_bytes(r.get("Gateway"));
            if family == 2 && gw.len() == 4 && out.ipv4_gateway.is_none() {
                out.ipv4_gateway = Some(format_ipv4(&gw));
            } else if family == 10 && gw.len() == 16 && out.ipv6_gateway.is_none() {
                out.ipv6_gateway = Some(format_ipv6(&gw));
            }
        }
    }
    if let Some(dns) = v.get("DNS").and_then(|a| a.as_array()) {
        for d in dns {
            let family = d.get("Family").and_then(|x| x.as_i64()).unwrap_or(0) as i32;
            let bytes = parse_addr_bytes(d.get("Address"));
            if let Some(s) = format_ip_address(family, &bytes) {
                out.dns.push(s);
            }
        }
    }
}

fn parse_addr_bytes(v: Option<&serde_json::Value>) -> Vec<u8> {
    v.and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_u64().map(|n| n as u8))
                .collect()
        })
        .unwrap_or_default()
}

fn format_ipv4(bytes: &[u8]) -> String {
    std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]).to_string()
}

fn format_ipv6(bytes: &[u8]) -> String {
    let mut a = [0u8; 16];
    a.copy_from_slice(bytes);
    std::net::Ipv6Addr::from(a).to_string()
}

fn format_ip_address(family: i32, bytes: &[u8]) -> Option<String> {
    match family {
        2 if bytes.len() == 4 => Some(format_ipv4(bytes)),
        10 if bytes.len() == 16 => Some(format_ipv6(bytes)),
        _ => None,
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
        assert_eq!(s.master_key_source, "file");
    }

    #[test]
    fn normalise_profile_path_maps_root_to_none() {
        assert_eq!(normalise_profile_path("/"), None);
        assert_eq!(normalise_profile_path(""), None);
        assert_eq!(
            normalise_profile_path("/fi/nexus1/profile/wifi/X"),
            Some("/fi/nexus1/profile/wifi/X".to_owned())
        );
    }

    #[test]
    fn decode_networkd_describe_extracts_v4_v6_and_default_route() {
        let json = serde_json::json!({
            "Addresses": [
                {
                    "Family": 2, "Scope": 0, "PrefixLength": 24,
                    "Address": [192, 0, 2, 42]
                },
                {
                    "Family": 10, "Scope": 253, "PrefixLength": 64,
                    "Address": [
                        0xfe, 0x80, 0, 0, 0, 0, 0, 0,
                        0xda, 0x3a, 0xdd, 0xff, 0xfe, 0xbb, 0x34, 0x74
                    ]
                },
                {
                    "Family": 10, "Scope": 0, "PrefixLength": 64,
                    "Address": [
                        0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 0,
                        0, 0, 0, 0, 0, 0, 0, 0x01
                    ]
                }
            ],
            "Routes": [
                {
                    "Family": 2, "DestinationPrefixLength": 24,
                    "Destination": [192, 0, 2, 0],
                    "Gateway": [192, 0, 2, 1]
                },
                {
                    "Family": 2, "DestinationPrefixLength": 0,
                    "Destination": [0, 0, 0, 0],
                    "Gateway": [192, 0, 2, 1]
                },
                {
                    "Family": 10, "DestinationPrefixLength": 0,
                    "Destination": [
                        0, 0, 0, 0, 0, 0, 0, 0,
                        0, 0, 0, 0, 0, 0, 0, 0
                    ],
                    "Gateway": [
                        0xfe, 0x80, 0, 0, 0, 0, 0, 0,
                        0, 0, 0, 0, 0, 0, 0, 0x01
                    ]
                }
            ]
        });
        let mut d = IpDetail::default();
        decode_networkd_describe(&json, &mut d);
        assert_eq!(d.ipv4_address.as_deref(), Some("192.0.2.42"));
        assert_eq!(d.ipv4_prefix_length, Some(24));
        assert_eq!(d.ipv4_gateway.as_deref(), Some("192.0.2.1"));
        // Global IPv6 wins over link-local when both are present.
        assert_eq!(d.ipv6_address.as_deref(), Some("2001:db8::1"));
        assert_eq!(d.ipv6_gateway.as_deref(), Some("fe80::1"));
    }

    #[test]
    fn decode_networkd_describe_pulls_dns_when_present() {
        let json = serde_json::json!({
            "DNS": [
                { "Family": 2, "Address": [10, 11, 12, 1] },
                { "Family": 10, "Address": [
                    0xfd, 0x00, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0x01
                ] }
            ]
        });
        let mut d = IpDetail::default();
        decode_networkd_describe(&json, &mut d);
        assert_eq!(d.dns, vec!["10.11.12.1".to_owned(), "fd00::1".to_owned()]);
    }

    #[test]
    fn decode_networkd_describe_falls_back_to_link_local_v6() {
        let json = serde_json::json!({
            "Addresses": [
                {
                    "Family": 10, "Scope": 253, "PrefixLength": 64,
                    "Address": [
                        0xfe, 0x80, 0, 0, 0, 0, 0, 0,
                        0, 0, 0, 0, 0, 0, 0, 0x02
                    ]
                }
            ]
        });
        let mut d = IpDetail::default();
        decode_networkd_describe(&json, &mut d);
        assert_eq!(d.ipv6_address.as_deref(), Some("fe80::2"));
    }

    #[test]
    fn format_ip_address_rejects_wrong_lengths() {
        assert!(format_ip_address(2, &[1, 2, 3]).is_none());
        assert!(format_ip_address(10, &[0; 8]).is_none());
        assert_eq!(
            format_ip_address(2, &[1, 1, 1, 1]).as_deref(),
            Some("1.1.1.1")
        );
    }
}
