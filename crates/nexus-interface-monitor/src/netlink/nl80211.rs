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

// ---------------------------------------------------------------------------
// NL80211_CMD_GET_WIPHY dump request + parser (DD-001 §5.3).
//
// The wiphy dump is split: for large PHYs the kernel spreads the
// per-wiphy attributes across multiple messages, each carrying the
// same NL80211_ATTR_WIPHY index. `WiphyCache` merges the incremental
// updates; callers feed each parsed message in and read the final
// `PhyCapabilities` once the dump terminates with `NLMSG_DONE`.
// ---------------------------------------------------------------------------

use std::collections::HashMap;

use nexus_core::PhyCapabilities;

use super::ParseError;
use super::genl::GENL_HDRLEN;
use super::genl::GenlHeader;
use super::parser::{
    AttributeIter, NLM_F_DUMP, NLM_F_REQUEST, NLMSG_HDRLEN, NetlinkMessageHeader, encode_attribute,
    finalize_message_length,
};

/// Build an `NL80211_CMD_GET_WIPHY` dump request with the split-dump
/// flag set. Matches the pseudocode in DD-001 §5.3.
pub fn build_get_wiphy_dump_request(seq: u32, port_id: u32, family_id: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(NLMSG_HDRLEN + GENL_HDRLEN + 8);
    let hdr = NetlinkMessageHeader {
        length: 0,
        msg_type: family_id,
        flags: NLM_F_REQUEST | NLM_F_DUMP,
        seq,
        pid: port_id,
    };
    buf.extend_from_slice(&hdr.to_bytes());
    buf.extend_from_slice(
        &GenlHeader {
            cmd: NL80211_CMD_GET_WIPHY,
            version: NL80211_GENL_VERSION,
        }
        .to_bytes(),
    );
    // NLA_FLAG (NL80211_ATTR_SPLIT_WIPHY_DUMP): zero-length payload.
    encode_attribute(&mut buf, NL80211_ATTR_SPLIT_WIPHY_DUMP, &[]);
    finalize_message_length(&mut buf);
    buf
}

/// Accumulator for `NL80211_CMD_GET_WIPHY` split-dump responses.
/// Each wiphy's attributes may be spread across multiple messages;
/// the cache merges them keyed by `NL80211_ATTR_WIPHY`.
#[derive(Debug, Default)]
pub struct WiphyCache {
    wiphies: HashMap<u32, PhyCapabilities>,
}

impl WiphyCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest the NLA chain from one `NL80211_CMD_NEW_WIPHY` message
    /// (the reply kind emitted for GET_WIPHY). Returns the wiphy
    /// index touched, or `None` if the message lacked
    /// `NL80211_ATTR_WIPHY`.
    pub fn ingest_attrs(&mut self, attrs_buf: &[u8]) -> Result<Option<u32>, ParseError> {
        let mut attrs = Vec::new();
        for attr in AttributeIter::new(attrs_buf) {
            attrs.push(attr?);
        }

        let wiphy = match attrs.iter().find(|a| a.attr_type == NL80211_ATTR_WIPHY) {
            Some(a) => a.u32()?,
            None => return Ok(None),
        };

        let entry = self
            .wiphies
            .entry(wiphy)
            .or_insert_with(|| PhyCapabilities {
                wiphy,
                ..PhyCapabilities::default()
            });
        merge_wiphy_attrs(entry, &attrs);
        Ok(Some(wiphy))
    }

    /// Look up a wiphy's capabilities by index.
    pub fn get(&self, wiphy: u32) -> Option<&PhyCapabilities> {
        self.wiphies.get(&wiphy)
    }

    /// Consume the cache and return the populated map.
    pub fn into_map(self) -> HashMap<u32, PhyCapabilities> {
        self.wiphies
    }

    /// Number of wiphies seen.
    pub fn len(&self) -> usize {
        self.wiphies.len()
    }

    /// True when no wiphies have been ingested.
    pub fn is_empty(&self) -> bool {
        self.wiphies.is_empty()
    }
}

/// Merge one dump message's attributes into the per-wiphy entry.
/// Per-attribute parse errors are logged and skipped (DD-001 §9.2)
/// rather than failing the whole dump, since real-world kernels
/// occasionally ship attributes of unexpected widths.
fn merge_wiphy_attrs(out: &mut PhyCapabilities, attrs: &[super::parser::NetlinkAttribute<'_>]) {
    for attr in attrs {
        if let Err(e) = merge_one_wiphy_attr(out, attr) {
            tracing::debug!(
                attr_type = attr.attr_type,
                payload_len = attr.payload.len(),
                error = %e,
                "skipping malformed wiphy attribute",
            );
        }
    }
}

fn merge_one_wiphy_attr(
    out: &mut PhyCapabilities,
    attr: &super::parser::NetlinkAttribute<'_>,
) -> Result<(), ParseError> {
    match attr.attr_type {
        NL80211_ATTR_WIPHY_NAME => {
            out.wiphy_name = attr.cstr()?.to_owned();
        }
        NL80211_ATTR_SUPPORTED_IFTYPES => {
            for child in attr.nested() {
                let child = child?;
                let iftype = u32::from(child.attr_type);
                if !out.supported_iftypes.contains(&iftype) {
                    out.supported_iftypes.push(iftype);
                }
            }
        }
        NL80211_ATTR_SUPPORTED_COMMANDS => {
            for child in attr.nested() {
                let child = child?;
                let cmd = child.u32()?;
                if !out.supported_commands.contains(&cmd) {
                    out.supported_commands.push(cmd);
                }
            }
        }
        NL80211_ATTR_CIPHER_SUITES => {
            for chunk in attr.bytes().chunks_exact(4) {
                let suite = u32::from_ne_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                if !out.cipher_suites.contains(&suite) {
                    out.cipher_suites.push(suite);
                }
            }
        }
        NL80211_ATTR_MAX_NUM_SCAN_SSIDS => {
            out.max_num_scan_ssids = read_small_uint(attr)?;
        }
        NL80211_ATTR_MAX_NUM_SCHED_SCAN_SSIDS => {
            out.max_num_sched_scan_ssids = read_small_uint(attr)?;
        }
        NL80211_ATTR_MAX_SCHED_SCAN_IE_LEN => {
            out.max_sched_scan_ie_len = attr.u16()?;
        }
        NL80211_ATTR_FEATURE_FLAGS => {
            out.feature_flags = attr.u32()?;
        }
        NL80211_ATTR_EXT_FEATURES => {
            // Newer kernels emit the bitmap as a byte array; last
            // message wins since the kernel re-sends the full bitmap
            // on each dump message for this attribute.
            out.ext_features = attr.bytes().to_vec();
        }
        NL80211_ATTR_SUPPORT_AP_UAPSD => {
            out.supports_ap_uapsd = true;
        }
        NL80211_ATTR_ROAM_SUPPORT => {
            out.supports_roaming = true;
        }
        _ => {
            // Skipped per DD-001 §9.3 (forward compat).
        }
    }
    Ok(())
}

/// Some wiphy scalar attributes that nl80211.h declares as a single
/// u8 (`max_num_scan_ssids`, etc.) actually ship in a variety of
/// widths in the wild. Some kernels emit the byte only; others wrap
/// it in a u32; a handful bundle several related bytes into the same
/// attribute. We read the low byte either way — the semantic value
/// is always in the first byte on little-endian (Linux only supports
/// little-endian netlink byte ordering).
fn read_small_uint(attr: &super::parser::NetlinkAttribute<'_>) -> Result<u32, ParseError> {
    if attr.payload.is_empty() {
        return Err(ParseError::AttributeWrongSize {
            kind: "small-uint",
            need: 1,
            got: 0,
        });
    }
    Ok(u32::from(attr.payload[0]))
}

// Internal helper to keep the test readable.
#[cfg(test)]
trait CommandU32 {
    fn to_ne_bytes_u32(self) -> [u8; 4];
}
#[cfg(test)]
impl CommandU32 for u8 {
    fn to_ne_bytes_u32(self) -> [u8; 4] {
        (self as u32).to_ne_bytes()
    }
}

#[cfg(test)]
mod wiphy_tests {
    use super::super::parser::{NLMSG_HDRLEN, encode_attribute, parse_header};
    use super::*;

    #[test]
    fn get_wiphy_dump_request_has_split_flag() {
        let bytes = build_get_wiphy_dump_request(1, 99, 0x17);
        // 16 nlmsghdr + 4 genlmsghdr + 4 NLA header (payload 0 bytes)
        assert_eq!(bytes.len(), 24);

        let hdr = parse_header(&bytes[..NLMSG_HDRLEN]).unwrap();
        assert_eq!(hdr.msg_type, 0x17);
        assert_eq!(hdr.flags, NLM_F_REQUEST | NLM_F_DUMP);

        // genlmsghdr
        assert_eq!(
            &bytes[NLMSG_HDRLEN..NLMSG_HDRLEN + GENL_HDRLEN],
            &[NL80211_CMD_GET_WIPHY, NL80211_GENL_VERSION, 0, 0],
        );
        // NLA: len=4 (header only), type=NL80211_ATTR_SPLIT_WIPHY_DUMP
        let nla_start = NLMSG_HDRLEN + GENL_HDRLEN;
        assert_eq!(
            u16::from_ne_bytes([bytes[nla_start], bytes[nla_start + 1]]),
            4,
        );
        assert_eq!(
            u16::from_ne_bytes([bytes[nla_start + 2], bytes[nla_start + 3]]),
            NL80211_ATTR_SPLIT_WIPHY_DUMP,
        );
    }

    #[test]
    fn wiphy_cache_requires_wiphy_index() {
        // Without NL80211_ATTR_WIPHY, ingest_attrs returns None.
        let mut buf = Vec::new();
        encode_attribute(&mut buf, NL80211_ATTR_WIPHY_NAME, b"phy0\0");
        let mut cache = WiphyCache::new();
        assert_eq!(cache.ingest_attrs(&buf).unwrap(), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn wiphy_cache_merges_scalars_vectors_and_flags() {
        // Build a single dump message for wiphy 0 with representative
        // attributes of every parsed kind.
        let mut buf = Vec::new();
        encode_attribute(&mut buf, NL80211_ATTR_WIPHY, &0u32.to_ne_bytes());
        encode_attribute(&mut buf, NL80211_ATTR_WIPHY_NAME, b"phy0\0");

        // Supported iftypes: STATION (2) and MONITOR (6), nested.
        let mut iftypes_payload = Vec::new();
        encode_attribute(&mut iftypes_payload, NL80211_IFTYPE_STATION as u16, &[]);
        encode_attribute(&mut iftypes_payload, NL80211_IFTYPE_MONITOR as u16, &[]);
        encode_attribute(&mut buf, NL80211_ATTR_SUPPORTED_IFTYPES, &iftypes_payload);

        // Supported commands: two entries, each a u32 payload.
        let mut commands_payload = Vec::new();
        encode_attribute(
            &mut commands_payload,
            1,
            &NL80211_CMD_GET_INTERFACE.to_ne_bytes_u32(),
        );
        encode_attribute(
            &mut commands_payload,
            2,
            &NL80211_CMD_GET_WIPHY.to_ne_bytes_u32(),
        );
        encode_attribute(&mut buf, NL80211_ATTR_SUPPORTED_COMMANDS, &commands_payload);

        // Cipher suites: two packed u32 values.
        let mut ciphers = Vec::new();
        ciphers.extend_from_slice(&0x000F_AC04u32.to_ne_bytes()); // CCMP-128
        ciphers.extend_from_slice(&0x000F_AC02u32.to_ne_bytes()); // TKIP
        encode_attribute(&mut buf, NL80211_ATTR_CIPHER_SUITES, &ciphers);

        encode_attribute(&mut buf, NL80211_ATTR_MAX_NUM_SCAN_SSIDS, &[10u8]);
        encode_attribute(&mut buf, NL80211_ATTR_MAX_NUM_SCHED_SCAN_SSIDS, &[4u8]);
        encode_attribute(
            &mut buf,
            NL80211_ATTR_MAX_SCHED_SCAN_IE_LEN,
            &512u16.to_ne_bytes(),
        );
        encode_attribute(
            &mut buf,
            NL80211_ATTR_FEATURE_FLAGS,
            &0xDEAD_BEEFu32.to_ne_bytes(),
        );
        encode_attribute(&mut buf, NL80211_ATTR_EXT_FEATURES, &[0x01, 0x00, 0x42]);
        encode_attribute(&mut buf, NL80211_ATTR_SUPPORT_AP_UAPSD, &[]);
        encode_attribute(&mut buf, NL80211_ATTR_ROAM_SUPPORT, &[]);

        let mut cache = WiphyCache::new();
        assert_eq!(cache.ingest_attrs(&buf).unwrap(), Some(0));
        let caps = cache.get(0).unwrap();
        assert_eq!(caps.wiphy, 0);
        assert_eq!(caps.wiphy_name, "phy0");
        assert_eq!(
            caps.supported_iftypes,
            vec![NL80211_IFTYPE_STATION, NL80211_IFTYPE_MONITOR],
        );
        assert_eq!(
            caps.supported_commands,
            vec![
                NL80211_CMD_GET_INTERFACE as u32,
                NL80211_CMD_GET_WIPHY as u32
            ],
        );
        assert_eq!(caps.cipher_suites, vec![0x000F_AC04, 0x000F_AC02]);
        assert_eq!(caps.max_num_scan_ssids, 10);
        assert_eq!(caps.max_num_sched_scan_ssids, 4);
        assert_eq!(caps.max_sched_scan_ie_len, 512);
        assert_eq!(caps.feature_flags, 0xDEAD_BEEF);
        assert_eq!(caps.ext_features, vec![0x01, 0x00, 0x42]);
        assert!(caps.supports_ap_uapsd);
        assert!(caps.supports_roaming);
    }

    #[test]
    fn wiphy_cache_merges_split_messages_for_same_wiphy() {
        // Two messages for wiphy 7, each carrying different iftypes
        // plus a different flag. After ingest both the cache entry
        // should contain all of them.
        let mut msg1 = Vec::new();
        encode_attribute(&mut msg1, NL80211_ATTR_WIPHY, &7u32.to_ne_bytes());
        let mut iftypes1 = Vec::new();
        encode_attribute(&mut iftypes1, NL80211_IFTYPE_STATION as u16, &[]);
        encode_attribute(&mut msg1, NL80211_ATTR_SUPPORTED_IFTYPES, &iftypes1);
        encode_attribute(&mut msg1, NL80211_ATTR_SUPPORT_AP_UAPSD, &[]);

        let mut msg2 = Vec::new();
        encode_attribute(&mut msg2, NL80211_ATTR_WIPHY, &7u32.to_ne_bytes());
        let mut iftypes2 = Vec::new();
        encode_attribute(&mut iftypes2, NL80211_IFTYPE_AP as u16, &[]);
        encode_attribute(&mut msg2, NL80211_ATTR_SUPPORTED_IFTYPES, &iftypes2);
        encode_attribute(&mut msg2, NL80211_ATTR_ROAM_SUPPORT, &[]);

        let mut cache = WiphyCache::new();
        assert_eq!(cache.ingest_attrs(&msg1).unwrap(), Some(7));
        assert_eq!(cache.ingest_attrs(&msg2).unwrap(), Some(7));
        assert_eq!(cache.len(), 1);
        let caps = cache.get(7).unwrap();
        assert_eq!(
            caps.supported_iftypes,
            vec![NL80211_IFTYPE_STATION, NL80211_IFTYPE_AP],
        );
        assert!(caps.supports_ap_uapsd);
        assert!(caps.supports_roaming);
    }
}
