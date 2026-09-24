//! Shared, in-process view of everything the D-Bus layer serves.
//!
//! Every interface implementation reads from this struct via the
//! `Arc<RwLock<State>>` given at construction time. The event
//! loop mutates state on NexusEvent arrivals; on mutations it
//! invokes the helper `properties_changed` on each live interface
//! object so D-Bus clients see `PropertiesChanged` signals in
//! real time.

use std::collections::{BTreeMap, HashMap};

use nexus_core::{
    BssInfo, BtDeviceInfo, ConnectivityState, FixMode, GnssFix, InterfaceInfo, InterfaceKind,
    MacAddr, PairingJobId, SatInfo, WifiState,
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
    /// Last observed internet-reachability state, fed by the
    /// daemon's connectivity probe via
    /// `NexusEvent::InternetConnectivityChanged`. `Unknown` until the
    /// first probe completes.
    pub internet_connectivity: ConnectivityState,

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

    /// Adapter ifname a pairing job belongs to, so `BtPairingComplete`
    /// (which carries no device/adapter field — see DD-006 §6.4's
    /// `PairingComplete(job_id, success, reason)`) knows which
    /// `fi.nexus.Bluetooth` object to fire the signal on. Populated
    /// when `BtPairingStarted` arrives (which does carry a device
    /// path to resolve the adapter from); removed once
    /// `BtPairingComplete` for that job has fired.
    pub pairing_jobs: HashMap<PairingJobId, String>,
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nexus_core::{
        BtAddressType, BtDeviceInfo, BtTransport, DisconnectReason, InterfaceInfo, InterfaceKind,
        MacAddr, Nl80211IfType, OperState, PhyCapabilities, SecurityMode, Ssid, WifiState,
    };

    use super::*;

    fn ssid() -> Ssid {
        Ssid::new(b"nexus-net".to_vec()).unwrap()
    }

    fn bssid() -> MacAddr {
        MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])
    }

    fn info(kind: InterfaceKind, ifname: &str, ifindex: u32) -> InterfaceInfo {
        InterfaceInfo {
            ifindex,
            ifname: ifname.into(),
            mac: [0x02, 0, 0, 0, 0, ifindex as u8],
            mtu: 1500,
            operstate: OperState::Up,
            carrier: true,
            kind,
            discovered_at: std::time::Instant::now(),
        }
    }

    fn wireless_kind() -> InterfaceKind {
        InterfaceKind::Wireless {
            wiphy: 0,
            wiphy_name: "phy0".into(),
            wdev: 1,
            iftype: Nl80211IfType(2),
            capabilities: Arc::new(PhyCapabilities::default()),
        }
    }

    fn bluetooth_kind() -> InterfaceKind {
        InterfaceKind::Bluetooth {
            hci_name: "hci0".into(),
            hci_index: 0,
            bt_address: MacAddr([0; 6]),
            bluez_path: "/org/bluez/hci0".into(),
        }
    }

    fn gnss_kind() -> InterfaceKind {
        InterfaceKind::Gnss {
            device_path: "/dev/ttyUSB0".into(),
            gpsd_device: "/dev/ttyUSB0".into(),
            vendor_model: Some("u-blox F9P".into()),
        }
    }

    fn dummy_bt_info() -> BtDeviceInfo {
        BtDeviceInfo {
            adapter: "/org/bluez/hci0".into(),
            device_path: "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF".into(),
            address: bssid(),
            address_type: BtAddressType::LePublic,
            name: Some("Pixel 8".into()),
            alias: None,
            rssi: Some(-55),
            tx_power: None,
            uuids: vec![],
            transport: BtTransport::Le,
            manufacturer_data: Default::default(),
            paired: false,
            bonded: false,
            trusted: false,
            blocked: false,
            connected: false,
        }
    }

    // --- PowerState ----------------------------------------------------

    #[test]
    fn power_state_as_str_and_parse_round_trip() {
        for v in [PowerState::Active, PowerState::Background, PowerState::Sleep] {
            assert_eq!(PowerState::parse(v.as_str()), Some(v));
        }
    }

    #[test]
    fn power_state_parse_unknown_returns_none() {
        assert!(PowerState::parse("unknown").is_none());
        assert!(PowerState::parse("").is_none());
        assert!(PowerState::parse("ACTIVE").is_none());
    }

    #[test]
    fn power_state_default_is_active() {
        assert_eq!(PowerState::default(), PowerState::Active);
    }

    // --- InterfaceState::new picks the right kind_data variant --------

    #[test]
    fn interface_state_new_ethernet_kind_data() {
        let s = InterfaceState::new(info(InterfaceKind::Ethernet, "eth0", 1));
        assert!(matches!(s.kind_data, InterfaceKindData::Ethernet(_)));
        assert!(s.managed_profile.is_none());
        assert_eq!(s.kind_label(), "ethernet");
    }

    #[test]
    fn interface_state_new_wireless_kind_data() {
        let s = InterfaceState::new(info(wireless_kind(), "wlan0", 2));
        assert!(matches!(s.kind_data, InterfaceKindData::Wifi(_)));
        assert_eq!(s.kind_label(), "wifi");
    }

    #[test]
    fn interface_state_new_bluetooth_kind_data() {
        let s = InterfaceState::new(info(bluetooth_kind(), "hci0", 3));
        assert!(matches!(s.kind_data, InterfaceKindData::Bluetooth(_)));
        assert_eq!(s.kind_label(), "bluetooth");
    }

    #[test]
    fn interface_state_new_gnss_kind_data() {
        let s = InterfaceState::new(info(gnss_kind(), "/dev/ttyUSB0", 4));
        assert!(matches!(s.kind_data, InterfaceKindData::Gnss(_)));
        assert_eq!(s.kind_label(), "gnss");
    }

    // --- WifiInterfaceState::apply_state ------------------------------

    #[test]
    fn apply_state_covers_every_non_connected_variant() {
        // For everything that isn't `Connected`, the cache must reset
        // its connected_bss / signal_dbm / frequency to defaults and
        // the state string must mirror the variant's lowercase name.
        let cases = [
            (WifiState::Idle, "idle"),
            (WifiState::Scanning, "scanning"),
            (
                WifiState::Connecting {
                    bssid: bssid(),
                    ssid: ssid(),
                },
                "connecting",
            ),
            (
                WifiState::Authenticating {
                    bssid: bssid(),
                    ssid: ssid(),
                },
                "authenticating",
            ),
            (
                WifiState::Handshaking {
                    bssid: bssid(),
                    ssid: ssid(),
                },
                "handshaking",
            ),
            (
                WifiState::Roaming {
                    from: bssid(),
                    to: MacAddr([0x11; 6]),
                    ssid: ssid(),
                },
                "roaming",
            ),
            (
                WifiState::Disconnected {
                    reason: DisconnectReason::CredentialsInvalid,
                },
                "disconnected",
            ),
            (WifiState::Gone, "gone"),
        ];

        for (variant, expected) in cases {
            let mut w = WifiInterfaceState {
                signal_dbm: -50,
                frequency: 2412,
                connected_bss: Some(WifiConnectedBss {
                    ssid: vec![],
                    bssid: bssid(),
                    frequency: 2412,
                    signal_dbm: -50,
                    security: "open".into(),
                }),
                ..Default::default()
            };
            w.apply_state(&variant);
            assert_eq!(w.state, expected, "variant {variant:?}");
            assert!(
                w.connected_bss.is_none(),
                "{variant:?} must clear connected_bss"
            );
            assert_eq!(w.signal_dbm, 0, "{variant:?} must reset signal_dbm");
            assert_eq!(w.frequency, 0, "{variant:?} must reset frequency");
        }
    }

    #[test]
    fn apply_state_connected_populates_bss_cache() {
        let mut w = WifiInterfaceState::default();
        w.apply_state(&WifiState::Connected {
            bssid: bssid(),
            ssid: ssid(),
            frequency: 5180,
            signal_dbm: -42,
            security: SecurityMode::Wpa2Psk,
        });
        assert_eq!(w.state, "connected");
        assert_eq!(w.signal_dbm, -42);
        assert_eq!(w.frequency, 5180);
        let bss = w.connected_bss.as_ref().expect("populated");
        assert_eq!(bss.bssid, bssid());
        assert_eq!(bss.ssid, b"nexus-net".to_vec());
        assert_eq!(bss.frequency, 5180);
        assert_eq!(bss.signal_dbm, -42);
        assert_eq!(bss.security, "wpa2_personal");
    }

    // --- security_label ------------------------------------------------

    #[test]
    fn security_label_covers_every_mode() {
        let cases = [
            (SecurityMode::Open, "open"),
            (SecurityMode::Owe, "owe"),
            (SecurityMode::Wep, "wep"),
            (SecurityMode::Wpa2Psk, "wpa2_personal"),
            (SecurityMode::Wpa3Sae, "wpa3_personal"),
            (SecurityMode::Wpa2Wpa3Transition, "wpa2_wpa3_personal"),
            (SecurityMode::Wpa2Eap, "wpa2_enterprise"),
            (SecurityMode::Wpa3Eap, "wpa3_enterprise"),
            (SecurityMode::Wpa3EapSuiteB192, "wpa3_enterprise_192"),
        ];
        for (mode, expected) in cases {
            assert_eq!(security_label(mode), expected, "mode {mode:?}");
        }
    }

    // --- fix_mode_to_i32 ----------------------------------------------

    #[test]
    fn fix_mode_to_i32_uses_dbus_wire_values() {
        // 0/2/3 mirrors the DD-006 Gnss.FixMode wire enum;
        // explicitly pin the gap (no value 1) so a future "Fix1D"
        // refactor doesn't silently flip semantics.
        assert_eq!(fix_mode_to_i32(FixMode::NoFix), 0);
        assert_eq!(fix_mode_to_i32(FixMode::Fix2D), 2);
        assert_eq!(fix_mode_to_i32(FixMode::Fix3D), 3);
    }

    // --- BtDeviceState ------------------------------------------------

    #[test]
    fn bt_device_state_from_info_starts_in_discovered() {
        let s = BtDeviceState::from_info(dummy_bt_info());
        assert_eq!(s.state, "discovered");
        assert!(s.profile_path.is_none());
        assert_eq!(s.info.address, bssid());
    }

    // --- State / capabilities ----------------------------------------

    #[test]
    fn default_capabilities_includes_every_documented_axis() {
        // The capability list is the API surface every UI consumes
        // to decide which interfaces to render. A drift here breaks
        // every operator app silently.
        let caps = default_capabilities();
        for required in [
            "wifi.wpa2",
            "wifi.wpa3",
            "wifi.owe",
            "eth.dot1x",
            "gnss",
            "bluetooth",
        ] {
            assert!(caps.iter().any(|c| c == required), "missing {required}");
        }
    }

    #[test]
    fn state_new_populates_version_and_defaults() {
        let s = State::new("0.1.0-test");
        assert_eq!(s.version, "0.1.0-test");
        assert_eq!(s.master_key_source, "file");
        assert_eq!(s.power_state, PowerState::Active);
        assert!(!s.bluez_connected);
        assert!(s.interfaces.is_empty());
        assert!(s.wifi_profiles.is_empty());
        assert!(s.ethernet_profiles.is_empty());
        assert!(s.bluetooth_profiles.is_empty());
        assert_eq!(s.api_capabilities, default_capabilities());
    }
}
