//! `fi.nexus.ScanResult` proxy. DD-006 §8.

#[zbus::proxy(interface = "fi.nexus.ScanResult", default_service = "fi.nexus1")]
pub trait ScanResult {
    #[zbus(property, name = "Bssid")]
    fn bssid(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property, name = "Ssid")]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property, name = "Frequency")]
    fn frequency(&self) -> zbus::Result<u32>;

    #[zbus(property, name = "SignalDbm")]
    fn signal_dbm(&self) -> zbus::Result<i32>;

    #[zbus(property, name = "SecurityOffered")]
    fn security_offered(&self) -> zbus::Result<Vec<String>>;

    #[zbus(property, name = "AgeMs")]
    fn age_ms(&self) -> zbus::Result<u64>;
}
