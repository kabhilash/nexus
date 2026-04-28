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

    /// Property setter for `Powered`. zbus emits a
    /// `Properties.Set` call under the hood. The daemon-side
    /// implementation enforces the `fi.nexus.set_power` polkit
    /// action and routes through the wifi backend's rfkill writer.
    #[zbus(property, name = "Powered")]
    fn set_powered(&self, on: bool) -> zbus::Result<()>;

    /// `Scan(params: a{sv}) -> (job_id: s)`. Pass an empty dict for
    /// the default "active probe every frequency" behaviour. The
    /// returned ULID correlates the subsequent
    /// `fi.nexus.Wifi.ScanComplete(job_id, success, results_count,
    /// reason)` signal — DD-006 §6.3 / §9.
    #[zbus(name = "Scan")]
    fn scan(
        &self,
        params: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<String>;

    /// `Connect(profile: o) -> (job_id: s)`. Returns a ULID job id
    /// that correlates the subsequent
    /// `fi.nexus.Wifi.ConnectComplete(job_id, success, reason)`
    /// signal — DD-006 §6.3 / §9.
    #[zbus(name = "Connect")]
    fn connect(&self, profile: zbus::zvariant::ObjectPath<'_>) -> zbus::Result<String>;

    /// `Disconnect(params: a{sv}) -> (job_id: s)`. Pass an empty
    /// dict for the historical no-arg behaviour. Recognised params:
    ///   - `pause_auto_connect` (b): also block the active profile
    ///     from auto-connect for the rest of this daemon session.
    /// Returns a ULID job id that correlates the subsequent
    /// `fi.nexus.Wifi.DisconnectComplete(job_id, success, reason)`
    /// signal.
    #[zbus(name = "Disconnect")]
    fn disconnect(
        &self,
        params: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<String>;

    /// `ConnectComplete(job_id: s, success: b, reason: s)` — DD-006
    /// §9. Terminal signal for an operator-initiated `Connect`.
    #[zbus(signal, name = "ConnectComplete")]
    fn connect_complete(
        &self,
        job_id: String,
        success: bool,
        reason: String,
    ) -> zbus::Result<()>;

    /// `DisconnectComplete(job_id: s, success: b, reason: s)` —
    /// DD-006 §9. Terminal signal for an operator-initiated
    /// `Disconnect`.
    #[zbus(signal, name = "DisconnectComplete")]
    fn disconnect_complete(
        &self,
        job_id: String,
        success: bool,
        reason: String,
    ) -> zbus::Result<()>;

    /// `ScanComplete(job_id: s, success: b, results_count: u, reason: s)`
    /// — DD-006 §9. Terminal signal for an operator-initiated
    /// `Scan`. Fires exactly once per accepted call.
    #[zbus(signal, name = "ScanComplete")]
    fn scan_complete(
        &self,
        job_id: String,
        success: bool,
        results_count: u32,
        reason: String,
    ) -> zbus::Result<()>;

    /// `StateChanged(new_state: s, details: a{sv})` — DD-006 §9.
    /// Typed mirror of `fi.nexus.Interface.StateChanged` emitted on
    /// every Wi-Fi state transition.
    ///
    /// Named `wifi_state_changed` in Rust (rather than
    /// `state_changed`) to avoid clashing with zbus's
    /// auto-generated `receive_state_changed` for the `State`
    /// property's `PropertiesChanged` stream — both helpers would
    /// otherwise occupy the same `WifiProxy::receive_state_changed`
    /// slot. The wire name is unchanged.
    #[zbus(signal, name = "StateChanged")]
    fn wifi_state_changed(
        &self,
        new_state: String,
        details: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
}
