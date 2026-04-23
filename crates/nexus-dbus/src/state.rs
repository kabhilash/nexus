//! Shared, in-process view of everything the D-Bus layer serves.
//!
//! Every interface implementation reads from this struct via the
//! `Arc<RwLock<State>>` given at construction time. The event
//! loop mutates state on NexusEvent arrivals; on mutations it
//! invokes the helper `properties_changed` on each live interface
//! object so D-Bus clients see `PropertiesChanged` signals in
//! real time.

use std::collections::BTreeMap;

use nexus_core::{
    BssInfo, BtDeviceInfo, FixMode, GnssFix, InterfaceInfo, InterfaceKind, MacAddr, SatInfo,
    WifiState,
};
use nexus_profile_store::{BluetoothProfile, EthernetProfile, WifiProfile};

// ---------------------------------------------------------------------------
// Globals
// ---------------------------------------------------------------------------

/// Coarse power state mirrored from DD-006 §5.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PowerState {
    #[default]
    Active,
    Background,
    Sleep,
}

impl PowerState {
    pub fn as_str(self) -> &'static str {
        match self {
            PowerState::Active => "active",
            PowerState::Background => "background",
            PowerState::Sleep => "sleep",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "active" => PowerState::Active,
            "background" => PowerState::Background,
            "sleep" => PowerState::Sleep,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Per-interface state, specialized per technology
// ---------------------------------------------------------------------------

/// What the D-Bus layer knows about one interface. The `kind_data`
/// variant is populated according to the Interface Monitor's
/// classification; only the matching D-Bus interfaces are served
/// for that object.
#[derive(Debug, Clone)]
pub struct InterfaceState {
    pub info: InterfaceInfo,
    pub managed_profile: Option<String>,
    pub kind_data: InterfaceKindData,
}

#[derive(Debug, Clone)]
pub enum InterfaceKindData {
    Ethernet(EthernetState),
    Wifi(WifiInterfaceState),
    Bluetooth(BluetoothAdapterState),
    Gnss(GnssState),
}

impl InterfaceState {
    pub fn new(info: InterfaceInfo) -> Self {
        let kind_data = match &info.kind {
            InterfaceKind::Ethernet => InterfaceKindData::Ethernet(EthernetState::default()),
            InterfaceKind::Wireless { .. } => {
                InterfaceKindData::Wifi(WifiInterfaceState::default())
            }
            InterfaceKind::Bluetooth { .. } => {
                InterfaceKindData::Bluetooth(BluetoothAdapterState::default())
            }
            InterfaceKind::Gnss { .. } => InterfaceKindData::Gnss(GnssState::default()),
        };
        Self {
            info,
            managed_profile: None,
            kind_data,
        }
    }

    pub fn kind_label(&self) -> &'static str {
        match &self.info.kind {
            InterfaceKind::Ethernet => "ethernet",
            InterfaceKind::Wireless { .. } => "wifi",
            InterfaceKind::Bluetooth { .. } => "bluetooth",
            InterfaceKind::Gnss { .. } => "gnss",
        }
    }
}

/// DD-006 §6.2 `fi.nexus.Ethernet` cache.
#[derive(Debug, Clone, Default)]
pub struct EthernetState {
    pub state: String,
    pub auth_backend: String,
    pub auth_failure_reason: String,
    pub eap_method: String,
}

/// DD-006 §6.3 `fi.nexus.Wifi` cache.
#[derive(Debug, Clone, Default)]
pub struct WifiInterfaceState {
    pub state: String,
    pub connected_bss: Option<WifiConnectedBss>,
    pub signal_dbm: i32,
    pub frequency: u32,
    /// Object paths the Wi-Fi interface's `ScanResults` property
    /// returns. Updated on every `WifiScanComplete` to mirror
    /// `scan_cache`.
    pub scan_results: Vec<String>,
    /// Per-BSSID cache of the last scan result. The
    /// `fi.nexus.ScanResult` per-object interface reads from this
    /// map; the event loop diffs the new and prior cache to
    /// register/unregister D-Bus objects. Uses a `HashMap` because
    /// `MacAddr` doesn't implement `Ord`.
    pub scan_cache: std::collections::HashMap<MacAddr, BssInfo>,
    pub supplicant: String,
    pub roaming_mode: String,
    pub powered: bool,
}

#[derive(Debug, Clone)]
pub struct WifiConnectedBss {
    pub ssid: Vec<u8>,
    pub bssid: MacAddr,
    pub frequency: u32,
    pub signal_dbm: i32,
    pub security: String,
}

impl WifiInterfaceState {
    /// Update the cache from a [`WifiState`] transition.
    pub fn apply_state(&mut self, s: &WifiState) {
        self.state = match s {
            WifiState::Idle => "idle",
            WifiState::Scanning => "scanning",
            WifiState::Connecting { .. } => "connecting",
            WifiState::Authenticating { .. } => "authenticating",
            WifiState::Handshaking { .. } => "handshaking",
            WifiState::Connected { .. } => "connected",
            WifiState::Roaming { .. } => "roaming",
            WifiState::Disconnected { .. } => "disconnected",
            WifiState::Gone => "gone",
        }
        .to_owned();
        if let WifiState::Connected {
            bssid,
            ssid,
            frequency,
            signal_dbm,
            security,
        } = s
        {
            self.connected_bss = Some(WifiConnectedBss {
                ssid: ssid.as_bytes().to_vec(),
                bssid: *bssid,
                frequency: *frequency,
                signal_dbm: *signal_dbm,
                security: security_label(*security),
            });
            self.signal_dbm = *signal_dbm;
            self.frequency = *frequency;
        } else {
            self.connected_bss = None;
            self.signal_dbm = 0;
            self.frequency = 0;
        }
    }
}

pub fn security_label(mode: nexus_core::SecurityMode) -> String {
    use nexus_core::SecurityMode as M;
    match mode {
        M::Open => "open",
        M::Owe => "owe",
        M::Wep => "wep",
        M::Wpa2Psk => "wpa2_personal",
        M::Wpa3Sae => "wpa3_personal",
        M::Wpa2Wpa3Transition => "wpa2_wpa3_personal",
        M::Wpa2Eap => "wpa2_enterprise",
        M::Wpa3Eap => "wpa3_enterprise",
        M::Wpa3EapSuiteB192 => "wpa3_enterprise_192",
    }
    .to_owned()
}

/// DD-006 §6.4 `fi.nexus.Bluetooth` cache.
#[derive(Debug, Clone, Default)]
pub struct BluetoothAdapterState {
    pub address: String,
    pub powered: bool,
    pub discoverable: bool,
    pub pairable: bool,
    pub discovering: bool,
    pub nexus_discovering: bool,
    /// Sub-path components (`<adapter>/device/<XX_...>`) for known
    /// devices under this adapter.
    pub known_devices: BTreeMap<String, BtDeviceState>,
    pub state: String,
}

/// DD-006 §6.6 `fi.nexus.BluetoothDevice` cache.
#[derive(Debug, Clone)]
pub struct BtDeviceState {
    pub info: BtDeviceInfo,
    pub state: String,
    pub profile_path: Option<String>,
}

impl BtDeviceState {
    pub fn from_info(info: BtDeviceInfo) -> Self {
        Self {
            info,
            state: "discovered".to_owned(),
            profile_path: None,
        }
    }
}

/// DD-006 §6.5 `fi.nexus.Gnss` cache.
#[derive(Debug, Clone, Default)]
pub struct GnssState {
    pub state: String,
    pub device_path: String,
    pub vendor_model: String,
    pub last_fix: Option<GnssFix>,
    pub satellites_in_view: u32,
    pub satellites_used: u32,
    pub horizontal_error_m: f64,
    pub gpsd_connected: bool,
    pub last_satellites: Vec<SatInfo>,
}

pub fn fix_mode_to_i32(mode: FixMode) -> i32 {
    match mode {
        FixMode::NoFix => 0,
        FixMode::Fix2D => 2,
        FixMode::Fix3D => 3,
    }
}

// ---------------------------------------------------------------------------
// Central state registry
// ---------------------------------------------------------------------------

/// Everything the D-Bus layer serves. Held behind an
/// `Arc<RwLock<…>>` inside `DbusServiceHandle`; every
/// interface implementation borrows `&State` through the service
/// reference.
#[derive(Debug, Default)]
pub struct State {
    pub version: String,
    pub api_capabilities: Vec<String>,
    pub power_state: PowerState,
    pub master_key_source: String,
    pub bluez_connected: bool,

    /// Keyed by interface ifname — the escaped form is derived via
    /// [`crate::paths::escape_component`].
    pub interfaces: BTreeMap<String, InterfaceState>,

    /// Wi-Fi profiles keyed by ULID string. Stored as the
    /// on-disk [`WifiProfile`] with credentials stripped at
    /// the D-Bus boundary.
    pub wifi_profiles: BTreeMap<String, WifiProfile>,
    /// Ethernet profiles keyed by ULID string.
    pub ethernet_profiles: BTreeMap<String, EthernetProfile>,
    /// Bluetooth profiles keyed by ULID string.
    pub bluetooth_profiles: BTreeMap<String, BluetoothProfile>,
}

impl State {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            api_capabilities: default_capabilities(),
            master_key_source: "file".to_owned(),
            ..Self::default()
        }
    }
}

pub fn default_capabilities() -> Vec<String> {
    vec![
        "wifi.wpa2".to_owned(),
        "wifi.wpa3".to_owned(),
        "wifi.owe".to_owned(),
        "eth.dot1x".to_owned(),
        "gnss".to_owned(),
        "bluetooth".to_owned(),
    ]
}
