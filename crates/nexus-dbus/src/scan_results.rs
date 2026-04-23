//! `fi.nexus.ScanResult` — DD-006 §8.
//!
//! One D-Bus object per cached BSS, at
//! `/fi/nexus1/interface/<ifname>/scan_result/<bssid>`. Objects
//! appear on `WifiScanComplete` and disappear when evicted from
//! the per-interface cache.

use std::collections::HashMap;
use std::sync::Arc;

use nexus_core::{BssInfo, MacAddr, SecurityMode};
use zbus::zvariant::{OwnedValue, Value};

use crate::services::Services;

pub struct ScanResultIface {
    pub services: Arc<Services>,
    pub ifname: String,
    pub bssid: MacAddr,
}

impl ScanResultIface {
    pub fn new(services: Arc<Services>, ifname: impl Into<String>, bssid: MacAddr) -> Self {
        Self {
            services,
            ifname: ifname.into(),
            bssid,
        }
    }

    async fn with_bss<R>(&self, default: R, f: impl FnOnce(&BssInfo) -> R) -> R {
        let guard = self.services.state.read().await;
        let Some(entry) = guard.interfaces.get(&self.ifname) else {
            return default;
        };
        if let crate::state::InterfaceKindData::Wifi(c) = &entry.kind_data {
            if let Some(bss) = c.scan_cache.get(&self.bssid) {
                return f(bss);
            }
        }
        default
    }
}

#[zbus::interface(name = "fi.nexus.ScanResult")]
impl ScanResultIface {
    #[zbus(property, name = "Bssid")]
    async fn bssid(&self) -> Vec<u8> {
        self.bssid.0.to_vec()
    }

    #[zbus(property, name = "Ssid")]
    async fn ssid(&self) -> Vec<u8> {
        self.with_bss(Vec::new(), |b| b.ssid.as_bytes().to_vec())
            .await
    }

    #[zbus(property, name = "Frequency")]
    async fn frequency(&self) -> u32 {
        self.with_bss(0, |b| b.frequency).await
    }

    #[zbus(property, name = "SignalDbm")]
    async fn signal_dbm(&self) -> i32 {
        self.with_bss(0, |b| b.signal_dbm).await
    }

    #[zbus(property, name = "SecurityOffered")]
    async fn security_offered(&self) -> Vec<String> {
        self.with_bss(Vec::new(), |b| {
            b.security.iter().map(|m| security_label(*m)).collect()
        })
        .await
    }

    #[zbus(property, name = "AgeMs")]
    async fn age_ms(&self) -> u64 {
        self.with_bss(0, |b| b.age_ms).await
    }

    #[zbus(property, name = "Capabilities")]
    async fn capabilities(&self) -> HashMap<String, OwnedValue> {
        self.with_bss(HashMap::new(), |b| {
            let mut out: HashMap<String, OwnedValue> = HashMap::new();
            for (key, val) in [
                ("ht", b.capabilities.ht),
                ("vht", b.capabilities.vht),
                ("he", b.capabilities.he),
                ("eht", b.capabilities.eht),
                ("ft", b.capabilities.ft),
                ("pmf_required", b.capabilities.pmf_required),
                ("pmf_capable", b.capabilities.pmf_capable),
                ("wps", b.capabilities.wps),
            ] {
                if let Ok(v) = OwnedValue::try_from(Value::new(val)) {
                    out.insert(key.to_owned(), v);
                }
            }
            out
        })
        .await
    }
}

fn security_label(mode: SecurityMode) -> String {
    crate::state::security_label(mode)
}
