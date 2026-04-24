//! Profile proxies — `fi.nexus.Profile` common interface plus the
//! Wi-Fi and Ethernet per-kind proxies. Read-only surface for
//! Phase 3; `Update` and `Delete` land in Phase 7.4.

use std::collections::HashMap;

/// `fi.nexus.Profile` — common fields on every profile object.
/// DD-006 §7.1.
#[zbus::proxy(interface = "fi.nexus.Profile", default_service = "fi.nexus1")]
pub trait Profile {
    #[zbus(property, name = "Id")]
    fn id(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Kind")]
    fn kind(&self) -> zbus::Result<String>;

    #[zbus(property, name = "Label")]
    fn label(&self) -> zbus::Result<String>;

    #[zbus(property, name = "CreatedAt")]
    fn created_at(&self) -> zbus::Result<String>;

    #[zbus(property, name = "UpdatedAt")]
    fn updated_at(&self) -> zbus::Result<String>;

    #[zbus(property, name = "CredentialsInvalid")]
    fn credentials_invalid(&self) -> zbus::Result<bool>;

    /// `Update(settings: a{sv}) -> ()` — partial update.
    #[zbus(name = "Update")]
    fn update(&self, settings: HashMap<String, zbus::zvariant::OwnedValue>) -> zbus::Result<()>;

    /// `Delete() -> ()`.
    #[zbus(name = "Delete")]
    fn delete(&self) -> zbus::Result<()>;
}

/// `fi.nexus.Profile.Wifi`. DD-006 §7.2.
#[zbus::proxy(interface = "fi.nexus.Profile.Wifi", default_service = "fi.nexus1")]
pub trait WifiProfile {
    #[zbus(property, name = "Ssid")]
    fn ssid(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property, name = "Hidden")]
    fn hidden(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Priority")]
    fn priority(&self) -> zbus::Result<i32>;

    #[zbus(property, name = "AutoConnect")]
    fn auto_connect(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "FastTransition")]
    fn fast_transition(&self) -> zbus::Result<bool>;

    /// `Security` is an `a{sv}` with a `"type"` field (string) and
    /// non-credential fields. Clients decode as needed.
    #[zbus(property, name = "Security")]
    fn security(&self) -> zbus::Result<HashMap<String, zbus::zvariant::OwnedValue>>;

    #[zbus(property, name = "HasCredentials")]
    fn has_credentials(&self) -> zbus::Result<HashMap<String, bool>>;

    #[zbus(property, name = "BssidPreferred")]
    fn bssid_preferred(&self) -> zbus::Result<Vec<u8>>;

    #[zbus(property, name = "BssidBlacklist")]
    fn bssid_blacklist(&self) -> zbus::Result<Vec<Vec<u8>>>;

    #[zbus(property, name = "ScanFrequencies")]
    fn scan_frequencies(&self) -> zbus::Result<Vec<u32>>;
}

/// `fi.nexus.Profile.Ethernet`. DD-006 §7.3.
#[zbus::proxy(interface = "fi.nexus.Profile.Ethernet", default_service = "fi.nexus1")]
pub trait EthernetProfile {
    #[zbus(property, name = "Ifname")]
    fn ifname(&self) -> zbus::Result<String>;

    #[zbus(property, name = "AutoConnect")]
    fn auto_connect(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Dot1xEnabled")]
    fn dot1x_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property, name = "Dot1xEap")]
    fn dot1x_eap(&self) -> zbus::Result<String>;

    #[zbus(property, name = "HasCredentials")]
    fn has_credentials(&self) -> zbus::Result<HashMap<String, bool>>;
}
