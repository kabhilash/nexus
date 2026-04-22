//! nl80211 command and attribute identifiers. Full parsing of
//! NL80211 replies lives in a later prompt; this module exists so the
//! generic-netlink resolver and socket code can reference stable
//! constants today.
//!
//! Source: `<linux/nl80211.h>` from the Linux UAPI tree.

/// Canonical name for the nl80211 generic-netlink family.
pub const NL80211_FAMILY_NAME: &str = "nl80211";

/// Nexus sends `genlmsghdr.version = NL80211_GENL_VERSION`.
pub const NL80211_GENL_VERSION: u8 = 1;

// ---------------------------------------------------------------------------
// Multicast group names (DD-001 §4.2).
// ---------------------------------------------------------------------------

/// Interface add/remove, regulatory domain changes.
pub const NL80211_MCAST_GROUP_CONFIG: &str = "config";
/// Scan trigger, results, and abort.
pub const NL80211_MCAST_GROUP_SCAN: &str = "scan";
/// Association / authentication / disassociation events.
pub const NL80211_MCAST_GROUP_MLME: &str = "mlme";
/// Regulatory domain change notifications.
pub const NL80211_MCAST_GROUP_REG: &str = "regulatory";
/// Vendor-specific events.
pub const NL80211_MCAST_GROUP_VENDOR: &str = "vendor";

// ---------------------------------------------------------------------------
// Command IDs (carried in genlmsghdr.cmd). The enum is large; we
// mirror the subset Nexus issues or consumes. Add new commands here
// as they become needed.
// ---------------------------------------------------------------------------

pub const NL80211_CMD_UNSPEC: u8 = 0;
pub const NL80211_CMD_GET_WIPHY: u8 = 1;
pub const NL80211_CMD_SET_WIPHY: u8 = 2;
pub const NL80211_CMD_NEW_WIPHY: u8 = 3;
pub const NL80211_CMD_DEL_WIPHY: u8 = 4;
pub const NL80211_CMD_GET_INTERFACE: u8 = 5;
pub const NL80211_CMD_SET_INTERFACE: u8 = 6;
pub const NL80211_CMD_NEW_INTERFACE: u8 = 7;
pub const NL80211_CMD_DEL_INTERFACE: u8 = 8;
pub const NL80211_CMD_GET_KEY: u8 = 9;
pub const NL80211_CMD_GET_STATION: u8 = 17;
pub const NL80211_CMD_GET_SCAN: u8 = 32;
pub const NL80211_CMD_TRIGGER_SCAN: u8 = 33;
pub const NL80211_CMD_NEW_SCAN_RESULTS: u8 = 34;
pub const NL80211_CMD_SCAN_ABORTED: u8 = 35;
pub const NL80211_CMD_REG_CHANGE: u8 = 36;
pub const NL80211_CMD_AUTHENTICATE: u8 = 37;
pub const NL80211_CMD_ASSOCIATE: u8 = 38;
pub const NL80211_CMD_DEAUTHENTICATE: u8 = 39;
pub const NL80211_CMD_DISASSOCIATE: u8 = 40;
pub const NL80211_CMD_CONNECT: u8 = 46;
pub const NL80211_CMD_ROAM: u8 = 47;
pub const NL80211_CMD_DISCONNECT: u8 = 48;

// ---------------------------------------------------------------------------
// Attribute IDs (carried in NLA type on nl80211 payloads). Subset
// used by DD-001 §§5.2, 5.3 and by the forthcoming full parser.
// ---------------------------------------------------------------------------

pub const NL80211_ATTR_UNSPEC: u16 = 0;
pub const NL80211_ATTR_WIPHY: u16 = 1;
pub const NL80211_ATTR_WIPHY_NAME: u16 = 2;
pub const NL80211_ATTR_IFINDEX: u16 = 3;
pub const NL80211_ATTR_IFNAME: u16 = 4;
pub const NL80211_ATTR_IFTYPE: u16 = 5;
pub const NL80211_ATTR_MAC: u16 = 6;
pub const NL80211_ATTR_KEY_DATA: u16 = 7;
pub const NL80211_ATTR_KEY_IDX: u16 = 8;
pub const NL80211_ATTR_KEY_CIPHER: u16 = 9;
pub const NL80211_ATTR_SSID: u16 = 52;
pub const NL80211_ATTR_WDEV: u16 = 153;
pub const NL80211_ATTR_CHANNEL_WIDTH: u16 = 159;

pub const NL80211_ATTR_SUPPORTED_IFTYPES: u16 = 32;
pub const NL80211_ATTR_WIPHY_BANDS: u16 = 22;
pub const NL80211_ATTR_SUPPORTED_COMMANDS: u16 = 50;
pub const NL80211_ATTR_CIPHER_SUITES: u16 = 115;
pub const NL80211_ATTR_MAX_NUM_SCAN_SSIDS: u16 = 43;
pub const NL80211_ATTR_SUPPORT_AP_UAPSD: u16 = 120;
pub const NL80211_ATTR_ROAM_SUPPORT: u16 = 131;
pub const NL80211_ATTR_MAX_NUM_SCHED_SCAN_SSIDS: u16 = 148;
pub const NL80211_ATTR_MAX_SCHED_SCAN_IE_LEN: u16 = 149;
pub const NL80211_ATTR_FEATURE_FLAGS: u16 = 138;
pub const NL80211_ATTR_EXT_FEATURES: u16 = 217;
pub const NL80211_ATTR_SPLIT_WIPHY_DUMP: u16 = 174;

// ---------------------------------------------------------------------------
// NL80211_IFTYPE_* (carried as u32 in NL80211_ATTR_IFTYPE). These
// land in `InterfaceKind::Wireless { iftype: Nl80211IfType(..) }`.
// ---------------------------------------------------------------------------

pub const NL80211_IFTYPE_UNSPECIFIED: u32 = 0;
pub const NL80211_IFTYPE_ADHOC: u32 = 1;
pub const NL80211_IFTYPE_STATION: u32 = 2;
pub const NL80211_IFTYPE_AP: u32 = 3;
pub const NL80211_IFTYPE_AP_VLAN: u32 = 4;
pub const NL80211_IFTYPE_WDS: u32 = 5;
pub const NL80211_IFTYPE_MONITOR: u32 = 6;
pub const NL80211_IFTYPE_MESH_POINT: u32 = 7;
pub const NL80211_IFTYPE_P2P_CLIENT: u32 = 8;
pub const NL80211_IFTYPE_P2P_GO: u32 = 9;
pub const NL80211_IFTYPE_P2P_DEVICE: u32 = 10;
pub const NL80211_IFTYPE_OCB: u32 = 11;
pub const NL80211_IFTYPE_NAN: u32 = 12;
