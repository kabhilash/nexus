//! `fi.nexus.Gnss` — DD-006 §6.5.

use std::sync::Arc;

use crate::services::Services;
use crate::state::{InterfaceKindData, fix_mode_to_i32};

/// `LastFix` tuple: `(xidddddddu)` — 10 fields.
pub type LastFixTuple = (i64, i32, f64, f64, f64, f64, f64, f64, f64, u32);

pub struct GnssIface {
    pub services: Arc<Services>,
    pub ifname: String,
}

impl GnssIface {
    pub fn new(services: Arc<Services>, ifname: impl Into<String>) -> Self {
        Self {
            services,
            ifname: ifname.into(),
        }
    }

    async fn with_cache<R>(&self, default: R, f: impl FnOnce(&crate::state::GnssState) -> R) -> R {
        let guard = self.services.state.read().await;
        match guard.interfaces.get(&self.ifname).map(|e| &e.kind_data) {
            Some(InterfaceKindData::Gnss(c)) => f(c),
            _ => default,
        }
    }
}

fn empty_fix() -> LastFixTuple {
    (0, 0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0)
}

#[zbus::interface(name = "fi.nexus.Gnss")]
impl GnssIface {
    #[zbus(property, name = "State")]
    async fn state(&self) -> String {
        self.with_cache(String::new(), |c| c.state.clone()).await
    }

    #[zbus(property, name = "DevicePath")]
    async fn device_path(&self) -> String {
        self.with_cache(String::new(), |c| c.device_path.clone())
            .await
    }

    #[zbus(property, name = "VendorModel")]
    async fn vendor_model(&self) -> String {
        self.with_cache(String::new(), |c| c.vendor_model.clone())
            .await
    }

    #[zbus(property, name = "LastFix")]
    async fn last_fix(&self) -> LastFixTuple {
        self.with_cache(empty_fix(), |c| match &c.last_fix {
            Some(fix) => (
                fix.time.timestamp_millis(),
                fix_mode_to_i32(fix.mode),
                fix.latitude.unwrap_or(0.0),
                fix.longitude.unwrap_or(0.0),
                fix.altitude_m.unwrap_or(0.0),
                fix.speed_mps.unwrap_or(0.0),
                fix.track_deg.unwrap_or(0.0),
                fix.horizontal_error_m.unwrap_or(0.0),
                fix.vertical_error_m.unwrap_or(0.0),
                fix.satellites_used,
            ),
            None => empty_fix(),
        })
        .await
    }

    #[zbus(property, name = "SatellitesInView")]
    async fn satellites_in_view(&self) -> u32 {
        self.with_cache(0, |c| c.satellites_in_view).await
    }

    #[zbus(property, name = "SatellitesUsed")]
    async fn satellites_used(&self) -> u32 {
        self.with_cache(0, |c| c.satellites_used).await
    }

    #[zbus(property, name = "HorizontalErrorM")]
    async fn horizontal_error_m(&self) -> f64 {
        self.with_cache(0.0, |c| c.horizontal_error_m).await
    }

    #[zbus(property, name = "GpsdConnected")]
    async fn gpsd_connected(&self) -> bool {
        self.with_cache(false, |c| c.gpsd_connected).await
    }
}
