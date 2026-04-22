//! Interface-registry types produced by the Interface Monitor and
//! consumed by every backend. See DD-001 §5.5.

use std::sync::Arc;
use std::time::Instant;

use crate::address::MacAddr;

/// Kernel operational state (`IF_OPER_*` in `<linux/if.h>`). Values
/// are the set observed on the rtnetlink wire; DD-001 §5.5 is the
/// source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperState {
    /// `IF_OPER_UNKNOWN` — driver hasn't reported yet.
    Unknown,
    /// `IF_OPER_NOTPRESENT` — hardware missing.
    NotPresent,
    /// `IF_OPER_DOWN`.
    Down,
    /// `IF_OPER_LOWERLAYERDOWN` — e.g., a VLAN whose parent is down.
    LowerLayerDown,
    /// `IF_OPER_TESTING`.
    Testing,
    /// `IF_OPER_DORMANT` — waiting on external event (common for Wi-Fi
    /// until association completes).
    Dormant,
    /// `IF_OPER_UP`.
    Up,
}

/// Technology-specific identity and per-kind metadata for an
/// interface. See DD-001 §5.5.
#[derive(Debug, Clone)]
pub enum InterfaceKind {
    /// A wired Ethernet interface.
    Ethernet,
    /// A Wi-Fi interface backed by an nl80211 wiphy.
    Wireless {
        /// nl80211 wiphy index.
        wiphy: u32,
        /// Human-readable wiphy name, e.g. `"phy0"`.
        wiphy_name: String,
        /// nl80211 wireless-device identifier (`wdev`).
        wdev: u64,
        /// Current iftype (station, AP, monitor, etc.).
        iftype: Nl80211IfType,
        /// Shared per-wiphy capability snapshot.
        capabilities: Arc<PhyCapabilities>,
    },
    /// A Bluetooth HCI adapter surfaced via BlueZ.
    Bluetooth {
        /// Kernel HCI name, e.g. `"hci0"`.
        hci_name: String,
        /// HCI index.
        hci_index: u32,
        /// Bluetooth adapter address (48-bit, MAC-family).
        bt_address: MacAddr,
        /// BlueZ adapter object path, e.g. `"/org/bluez/hci0"`.
        bluez_path: String,
    },
    /// A GNSS receiver surfaced via udev and gpsd.
    Gnss {
        /// Serial/tty device path, e.g. `"/dev/ttyUSB0"`.
        device_path: String,
        /// gpsd device identifier.
        gpsd_device: String,
        /// Vendor/model string from udev, if available.
        vendor_model: Option<String>,
    },
}

/// Registry record for every discovered interface. See DD-001 §5.5.
#[derive(Debug, Clone)]
pub struct InterfaceInfo {
    /// Unique key across the system. Real kernel ifindex for
    /// Ethernet/Wireless; synthesized for Bluetooth (`0x8000_0000 |
    /// hci_index`) and GNSS (`0x9000_0000 | seq`).
    pub ifindex: u32,
    /// Human-readable name: `"eth0"`, `"wlp2s0"`, `"hci0"`,
    /// `"/dev/ttyUSB0"`.
    pub ifname: String,
    /// Hardware address.
    pub mac: [u8; 6],
    /// MTU as reported by the kernel (or 0 where not applicable).
    pub mtu: u32,
    /// Kernel operstate.
    pub operstate: OperState,
    /// Kernel carrier bit (`IFLA_CARRIER`).
    pub carrier: bool,
    /// Technology-specific payload.
    pub kind: InterfaceKind,
    /// Monotonic timestamp for when the registry first observed the
    /// interface. Used for cold-boot budgeting and diagnostics.
    pub discovered_at: Instant,
}

/// nl80211 interface type (`NL80211_IFTYPE_*`). The full decode table
/// lives in the Interface Monitor; this newtype keeps the raw kernel
/// value so upstream callers can compare without a churn in every
/// crate whenever a new iftype appears. See DD-001 §5.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Nl80211IfType(pub u32);

/// Per-wiphy capability snapshot, merged from split-dump
/// `NL80211_CMD_GET_WIPHY` responses. Fields correspond to the
/// attributes-of-interest table in DD-001 §5.3.
///
/// Attributes with no value are defaulted per the nl80211 semantics:
/// scalar maxima default to 0, flag attributes default to `false`,
/// vector attributes default to empty.
#[derive(Debug, Clone, Default)]
pub struct PhyCapabilities {
    /// nl80211 wiphy index (NL80211_ATTR_WIPHY). The registry key.
    pub wiphy: u32,
    /// Human-readable wiphy name (NL80211_ATTR_WIPHY_NAME), e.g.
    /// `"phy0"`.
    pub wiphy_name: String,
    /// Supported interface types (NL80211_ATTR_SUPPORTED_IFTYPES).
    /// Values are `NL80211_IFTYPE_*`.
    pub supported_iftypes: Vec<u32>,
    /// nl80211 commands this driver supports
    /// (NL80211_ATTR_SUPPORTED_COMMANDS). Values are `NL80211_CMD_*`.
    pub supported_commands: Vec<u32>,
    /// Supported encryption cipher suites
    /// (NL80211_ATTR_CIPHER_SUITES).
    pub cipher_suites: Vec<u32>,
    /// Maximum SSIDs per scan request
    /// (NL80211_ATTR_MAX_NUM_SCAN_SSIDS).
    pub max_num_scan_ssids: u32,
    /// Maximum SSIDs per scheduled scan
    /// (NL80211_ATTR_MAX_NUM_SCHED_SCAN_SSIDS).
    pub max_num_sched_scan_ssids: u32,
    /// Maximum IE length for scheduled scans
    /// (NL80211_ATTR_MAX_SCHED_SCAN_IE_LEN).
    pub max_sched_scan_ie_len: u16,
    /// Bit flags for driver features (NL80211_ATTR_FEATURE_FLAGS).
    pub feature_flags: u32,
    /// Extended feature bitmap (NL80211_ATTR_EXT_FEATURES), raw bytes.
    pub ext_features: Vec<u8>,
    /// AP UAPSD supported (NL80211_ATTR_SUPPORT_AP_UAPSD flag).
    pub supports_ap_uapsd: bool,
    /// Driver-level roaming supported (NL80211_ATTR_ROAM_SUPPORT flag).
    pub supports_roaming: bool,
}
