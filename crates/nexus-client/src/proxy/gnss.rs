//! `fi.nexus.Gnss` proxy. Read-only. DD-006 §6.5.

/// `LastFix` tuple: `(time_unix_ms: x, mode: i, latitude: d,
/// longitude: d, altitude_m: d, speed_mps: d, track_deg: d,
/// horizontal_error_m: d, vertical_error_m: d, satellites_used: u)`.
pub type LastFixTuple = (i64, i32, f64, f64, f64, f64, f64, f64, f64, u32);

#[zbus::proxy(interface = "fi.nexus.Gnss", default_service = "fi.nexus1")]
pub trait Gnss {
    #[zbus(property, name = "State")]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "DevicePath")]
    fn device_path(&self) -> zbus::Result<String>;

    #[zbus(property, name = "VendorModel")]
    fn vendor_model(&self) -> zbus::Result<String>;

    #[zbus(property, name = "LastFix")]
    fn last_fix(&self) -> zbus::Result<LastFixTuple>;

    #[zbus(property, name = "SatellitesInView")]
    fn satellites_in_view(&self) -> zbus::Result<u32>;

    #[zbus(property, name = "SatellitesUsed")]
    fn satellites_used(&self) -> zbus::Result<u32>;

    #[zbus(property, name = "HorizontalErrorM")]
    fn horizontal_error_m(&self) -> zbus::Result<f64>;

    #[zbus(property, name = "GpsdConnected")]
    fn gpsd_connected(&self) -> zbus::Result<bool>;
}
