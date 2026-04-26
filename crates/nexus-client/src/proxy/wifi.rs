//! `fi.nexus.Wifi` proxy — read-only surface. DD-006 §6.3.
//!
//! `Scan`/`Connect`/`Disconnect`/`Roam` methods are deliberately
//! not declared here; nexusctl's Phase 3 is read-only and mutating
//! calls land in Phase 7.4 once the PSK-entry interactive flow is
//! ready.

use zbus::zvariant::OwnedObjectPath;

/// `fi.nexus.Wifi.ConnectedBss` — DD-006 §6.3. Tuple of
/// `(ssid_utf8_lossy: s, ssid_bytes: ay, bssid: ay, frequency: u,
///   signal_dbm: i, security: s)`.
pub type ConnectedBssTuple = (String, Vec<u8>, Vec<u8>, u32, i32, String);

#[zbus::proxy(interface = "fi.nexus.Wifi", default_service = "fi.nexus1")]
pub trait Wifi {
    #[zbus(property, name = "State")]
    fn state(&self) -> zbus::Result<String>;

    #[zbus(property, name = "ConnectedBss")]
    fn connected_bss(&self) -> zbus::Result<ConnectedBssTuple>;

    #[zbus(property, name = "SignalDbm")]
    fn signal_dbm(&self) -> zbus::Result<i32>;

    #[zbus(property, name = "Frequency")]
    fn frequency(&self) -> zbus::Result<u32>;

    #[zbus(property, name = "ScanResults")]
    fn scan_results(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    #[zbus(property, name = "Supplicant")]
    fn supplicant(&self) -> zbus::Result<String>;

    #[zbus(property, name = "RoamingMode")]
    fn roaming_mode(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Powered")]
    fn powered(&self) -> zbus::Result<bool>;

    /// `Scan(params: a{sv}) -> ()`. Pass an empty dict for the
    /// default "active probe every frequency" behaviour.
    #[zbus(name = "Scan")]
    fn scan(
        &self,
        params: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;

    /// `Connect(profile: o) -> ()`.
    #[zbus(name = "Connect")]
    fn connect(&self, profile: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;

    /// `Disconnect(params: a{sv}) -> ()`. Pass an empty dict for
    /// the historical no-arg behaviour. Recognised params:
    ///   - `pause_auto_connect` (b): also block the active profile
    ///     from auto-connect for the rest of this daemon session.
    #[zbus(name = "Disconnect")]
    fn disconnect(
        &self,
        params: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
}
