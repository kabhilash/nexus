//! Generic Netlink (`NETLINK_GENERIC`) controller. Resolves a family
//! name to its runtime-assigned family ID and multicast-group IDs.
//! See DD-001 §4.2 and §9.4.

use std::collections::BTreeMap;

use thiserror::Error;

use super::ParseError;
use super::parser::{
    self, AttributeIter, NLM_F_REQUEST, NLMSG_ERROR, NLMSG_HDRLEN, NetlinkMessage,
    NetlinkMessageHeader, encode_attribute, parse_message, parse_nlmsgerr,
};

// ---------------------------------------------------------------------------
// genlmsghdr (Appendix A.2). 4 bytes: u8 cmd + u8 version + u16 pad.
// ---------------------------------------------------------------------------

pub const GENL_HDRLEN: usize = 4;

/// Built-in generic-netlink controller family. Every other family's
/// ID is allocated dynamically and resolved through this one.
pub const GENL_ID_CTRL: u16 = 0x10;

/// `struct genlmsghdr`. The 2-byte `reserved` field is always zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenlHeader {
    pub cmd: u8,
    pub version: u8,
}

impl GenlHeader {
    pub fn to_bytes(self) -> [u8; GENL_HDRLEN] {
        [self.cmd, self.version, 0, 0]
    }
}

/// Parse the 4-byte `genlmsghdr` off the front of a buffer.
pub fn parse_genl_header(buf: &[u8]) -> Result<(GenlHeader, &[u8]), ParseError> {
    if buf.len() < GENL_HDRLEN {
        return Err(ParseError::Truncated {
            need: GENL_HDRLEN,
            got: buf.len(),
        });
    }
    Ok((
        GenlHeader {
            cmd: buf[0],
            version: buf[1],
        },
        &buf[GENL_HDRLEN..],
    ))
}

// ---------------------------------------------------------------------------
// CTRL_CMD_* and CTRL_ATTR_* from <linux/genetlink.h>.
// ---------------------------------------------------------------------------

pub const CTRL_CMD_UNSPEC: u8 = 0;
pub const CTRL_CMD_NEWFAMILY: u8 = 1;
pub const CTRL_CMD_DELFAMILY: u8 = 2;
pub const CTRL_CMD_GETFAMILY: u8 = 3;
pub const CTRL_CMD_NEWOPS: u8 = 4;
pub const CTRL_CMD_DELOPS: u8 = 5;
pub const CTRL_CMD_GETOPS: u8 = 6;
pub const CTRL_CMD_NEWMCAST_GRP: u8 = 7;
pub const CTRL_CMD_DELMCAST_GRP: u8 = 8;

pub const CTRL_ATTR_UNSPEC: u16 = 0;
pub const CTRL_ATTR_FAMILY_ID: u16 = 1;
pub const CTRL_ATTR_FAMILY_NAME: u16 = 2;
pub const CTRL_ATTR_VERSION: u16 = 3;
pub const CTRL_ATTR_HDRSIZE: u16 = 4;
pub const CTRL_ATTR_MAXATTR: u16 = 5;
pub const CTRL_ATTR_OPS: u16 = 6;
pub const CTRL_ATTR_MCAST_GROUPS: u16 = 7;

pub const CTRL_ATTR_MCAST_GRP_UNSPEC: u16 = 0;
pub const CTRL_ATTR_MCAST_GRP_NAME: u16 = 1;
pub const CTRL_ATTR_MCAST_GRP_ID: u16 = 2;

pub const GENL_VERSION_CTRL: u8 = 1;

// ---------------------------------------------------------------------------
// Family resolution: request, response, result type.
// ---------------------------------------------------------------------------

/// Resolved metadata for a generic-netlink family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyInfo {
    /// Runtime-assigned family ID. Used as `nlmsg_type` in subsequent
    /// requests against this family.
    pub id: u16,
    /// Canonical family name (`"nl80211"`, …).
    pub name: String,
    /// Family protocol version. `0` when the kernel omitted
    /// `CTRL_ATTR_VERSION`.
    pub version: u32,
    /// Multicast groups this family declares, keyed by name.
    pub mcast_groups: BTreeMap<String, u32>,
}

impl FamilyInfo {
    /// Look up a multicast group by name. Returns `None` if the
    /// family does not publish one.
    pub fn mcast_group(&self, name: &str) -> Option<u32> {
        self.mcast_groups.get(name).copied()
    }
}

/// Errors surfaced by the generic-netlink resolver. A missing-family
/// response from the kernel (`ENOENT`) maps to
/// [`ResolveError::FamilyUnavailable`] so the caller can distinguish
/// "Wi-Fi not supported on this kernel" from "netlink I/O failed".
#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("netlink I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("netlink parse error: {0}")]
    Parse(#[from] ParseError),

    #[error("generic-netlink family '{name}' unavailable (kernel errno = {errno})")]
    FamilyUnavailable { name: String, errno: i32 },

    #[error("expected CTRL reply, got netlink message type {msg_type}")]
    UnexpectedMessageType { msg_type: u16 },

    #[error("CTRL reply missing required attribute '{name}'")]
    MissingAttribute { name: &'static str },
}

/// Build a `CTRL_CMD_GETFAMILY` request that asks the controller to
/// resolve `family_name`. The returned buffer is ready to send to
/// the generic-netlink socket.
pub fn build_getfamily_request(seq: u32, port_id: u32, family_name: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(NLMSG_HDRLEN + GENL_HDRLEN + 16 + family_name.len());

    let placeholder = NetlinkMessageHeader {
        length: 0,
        msg_type: GENL_ID_CTRL,
        flags: NLM_F_REQUEST,
        seq,
        pid: port_id,
    };
    buf.extend_from_slice(&placeholder.to_bytes());
    buf.extend_from_slice(
        &GenlHeader {
            cmd: CTRL_CMD_GETFAMILY,
            version: GENL_VERSION_CTRL,
        }
        .to_bytes(),
    );

    // CTRL_ATTR_FAMILY_NAME is a NUL-terminated string.
    let mut name_payload = Vec::with_capacity(family_name.len() + 1);
    name_payload.extend_from_slice(family_name.as_bytes());
    name_payload.push(0);
    encode_attribute(&mut buf, CTRL_ATTR_FAMILY_NAME, &name_payload);

    parser::finalize_message_length(&mut buf);
    buf
}

/// Parse a `CTRL_CMD_NEWFAMILY` response produced in reply to
/// [`build_getfamily_request`]. If the outer message is an
/// `NLMSG_ERROR`, the errno is surfaced through
/// [`ResolveError::FamilyUnavailable`].
pub fn parse_family_response(
    msg: &NetlinkMessage<'_>,
    requested_name: &str,
) -> Result<FamilyInfo, ResolveError> {
    if msg.header.msg_type == NLMSG_ERROR {
        let err = parse_nlmsgerr(msg.payload)?;
        return Err(ResolveError::FamilyUnavailable {
            name: requested_name.to_owned(),
            errno: err.error,
        });
    }
    if msg.header.msg_type != GENL_ID_CTRL {
        return Err(ResolveError::UnexpectedMessageType {
            msg_type: msg.header.msg_type,
        });
    }

    let (_genl, attrs_buf) = parse_genl_header(msg.payload)?;

    let mut id: Option<u16> = None;
    let mut name: Option<String> = None;
    let mut version: u32 = 0;
    let mut mcast_groups: BTreeMap<String, u32> = BTreeMap::new();

    for attr in AttributeIter::new(attrs_buf) {
        let attr = attr?;
        match attr.attr_type {
            CTRL_ATTR_FAMILY_ID => id = Some(attr.u16()?),
            CTRL_ATTR_FAMILY_NAME => name = Some(attr.cstr()?.to_owned()),
            CTRL_ATTR_VERSION => version = attr.u32()?,
            CTRL_ATTR_MCAST_GROUPS => {
                // Outer NLA whose payload is a list of nested entries.
                // Each entry is itself a nested NLA whose payload
                // holds CTRL_ATTR_MCAST_GRP_{NAME,ID}. The wrapping
                // entry's type ID is an index (1-based), not one of
                // the CTRL_ATTR_MCAST_GRP_* constants.
                for entry in attr.nested() {
                    let entry = entry?;
                    let mut grp_name: Option<String> = None;
                    let mut grp_id: Option<u32> = None;
                    for inner in entry.nested() {
                        let inner = inner?;
                        match inner.attr_type {
                            CTRL_ATTR_MCAST_GRP_NAME => grp_name = Some(inner.cstr()?.to_owned()),
                            CTRL_ATTR_MCAST_GRP_ID => grp_id = Some(inner.u32()?),
                            _ => {}
                        }
                    }
                    if let (Some(n), Some(i)) = (grp_name, grp_id) {
                        mcast_groups.insert(n, i);
                    }
                }
            }
            _ => {} // forward-compat: skip unknown CTRL attrs
        }
    }

    let id = id.ok_or(ResolveError::MissingAttribute {
        name: "CTRL_ATTR_FAMILY_ID",
    })?;
    let name = name.unwrap_or_else(|| requested_name.to_owned());

    Ok(FamilyInfo {
        id,
        name,
        version,
        mcast_groups,
    })
}

/// Send a `CTRL_CMD_GETFAMILY` request on `socket` and parse the
/// response. Allocates a 4 KiB receive buffer (the CTRL reply is
/// always small). See DD-001 §4.2.
pub async fn resolve_family(
    socket: &super::socket::NetlinkSocket,
    family_name: &str,
) -> Result<FamilyInfo, ResolveError> {
    let seq = socket.next_seq();
    let request = build_getfamily_request(seq, socket.port_id(), family_name);
    socket.send(&request).await?;

    let mut buf = vec![0u8; 4096];
    let n = socket.recv(&mut buf).await?;
    let (msg, _rest) = parse_message(&buf[..n])?;
    parse_family_response(&msg, family_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::parser::{NLM_F_REQUEST, NLMSG_HDRLEN, parse_header};

    #[test]
    fn getfamily_request_layout() {
        let bytes = build_getfamily_request(1, 99, "nl80211");
        // 16 nlmsghdr + 4 genlmsghdr + (4 nla header + 8 payload "nl80211\0") = 32.
        assert_eq!(bytes.len(), 32);

        let hdr = parse_header(&bytes[..NLMSG_HDRLEN]).unwrap();
        assert_eq!(hdr.msg_type, GENL_ID_CTRL);
        assert_eq!(hdr.flags, NLM_F_REQUEST);
        assert_eq!(hdr.seq, 1);
        assert_eq!(hdr.pid, 99);
        assert_eq!(hdr.length as usize, bytes.len());

        // genlmsghdr
        let genl_hdr = &bytes[NLMSG_HDRLEN..NLMSG_HDRLEN + GENL_HDRLEN];
        assert_eq!(genl_hdr, &[CTRL_CMD_GETFAMILY, GENL_VERSION_CTRL, 0, 0]);

        // NLA: len=12 (4 header + 8 payload), type=CTRL_ATTR_FAMILY_NAME
        let nla = &bytes[NLMSG_HDRLEN + GENL_HDRLEN..];
        assert_eq!(
            u16::from_ne_bytes([nla[0], nla[1]]) as usize,
            4 + b"nl80211\0".len()
        );
        assert_eq!(u16::from_ne_bytes([nla[2], nla[3]]), CTRL_ATTR_FAMILY_NAME);
        assert_eq!(&nla[4..12], b"nl80211\0");
    }

    /// Build a plausible CTRL reply for "nl80211": family id 0x17,
    /// version 1, mcast groups "config"=3 and "scan"=4.
    fn synthetic_nl80211_ctrl_reply() -> Vec<u8> {
        // Start with a placeholder outer header.
        let mut buf = Vec::new();
        let outer = NetlinkMessageHeader {
            length: 0,
            msg_type: GENL_ID_CTRL,
            flags: 0,
            seq: 1,
            pid: 0,
        };
        buf.extend_from_slice(&outer.to_bytes());

        // genlmsghdr for CTRL_CMD_NEWFAMILY (the reply kind).
        buf.extend_from_slice(
            &GenlHeader {
                cmd: CTRL_CMD_NEWFAMILY,
                version: GENL_VERSION_CTRL,
            }
            .to_bytes(),
        );

        encode_attribute(&mut buf, CTRL_ATTR_FAMILY_ID, &0x17u16.to_ne_bytes());
        encode_attribute(&mut buf, CTRL_ATTR_FAMILY_NAME, b"nl80211\0");
        encode_attribute(&mut buf, CTRL_ATTR_VERSION, &1u32.to_ne_bytes());

        // Build CTRL_ATTR_MCAST_GROUPS payload: two nested entries
        // each containing NAME + ID.
        let mut groups_payload = Vec::new();
        for (idx, (name, id)) in [(1u16, ("config", 3u32)), (2u16, ("scan", 4u32))] {
            let mut entry = Vec::new();
            let mut name_bytes = Vec::with_capacity(name.len() + 1);
            name_bytes.extend_from_slice(name.as_bytes());
            name_bytes.push(0);
            encode_attribute(&mut entry, CTRL_ATTR_MCAST_GRP_NAME, &name_bytes);
            encode_attribute(&mut entry, CTRL_ATTR_MCAST_GRP_ID, &id.to_ne_bytes());
            // The wrapping entry's type ID is the entry index; the
            // kernel sets this to a 1-based counter. Nexus's parser
            // ignores the wrapper type and walks the nested payload.
            encode_attribute(&mut groups_payload, idx, &entry);
        }
        encode_attribute(&mut buf, CTRL_ATTR_MCAST_GROUPS, &groups_payload);

        parser::finalize_message_length(&mut buf);
        buf
    }

    #[test]
    fn family_response_parses_id_version_and_groups() {
        let bytes = synthetic_nl80211_ctrl_reply();
        let (msg, rest) = parse_message(&bytes).unwrap();
        assert!(rest.is_empty());

        let info = parse_family_response(&msg, "nl80211").unwrap();
        assert_eq!(info.id, 0x17);
        assert_eq!(info.name, "nl80211");
        assert_eq!(info.version, 1);
        assert_eq!(info.mcast_group("config"), Some(3));
        assert_eq!(info.mcast_group("scan"), Some(4));
        assert_eq!(info.mcast_group("missing"), None);
    }

    #[test]
    fn unknown_family_surfaces_as_family_unavailable() {
        // Build an NLMSG_ERROR reply with errno = -ENOENT.
        const ENOENT: i32 = -2;
        let echoed = NetlinkMessageHeader {
            length: NLMSG_HDRLEN as u32,
            msg_type: GENL_ID_CTRL,
            flags: NLM_F_REQUEST,
            seq: 1,
            pid: 99,
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&ENOENT.to_ne_bytes());
        payload.extend_from_slice(&echoed.to_bytes());

        let outer_len = NLMSG_HDRLEN + payload.len();
        let outer = NetlinkMessageHeader {
            length: outer_len as u32,
            msg_type: NLMSG_ERROR,
            flags: 0,
            seq: 1,
            pid: 0,
        };

        let mut bytes = Vec::with_capacity(outer_len);
        bytes.extend_from_slice(&outer.to_bytes());
        bytes.extend_from_slice(&payload);

        let (msg, _rest) = parse_message(&bytes).unwrap();
        let err = parse_family_response(&msg, "nl80211").unwrap_err();
        match err {
            ResolveError::FamilyUnavailable { name, errno } => {
                assert_eq!(name, "nl80211");
                assert_eq!(errno, ENOENT);
            }
            other => panic!("expected FamilyUnavailable, got {other:?}"),
        }
    }

    #[test]
    fn missing_family_id_returns_missing_attribute() {
        // Reply with a CTRL message body but no CTRL_ATTR_FAMILY_ID.
        let mut buf = Vec::new();
        let outer = NetlinkMessageHeader {
            length: 0,
            msg_type: GENL_ID_CTRL,
            flags: 0,
            seq: 1,
            pid: 0,
        };
        buf.extend_from_slice(&outer.to_bytes());
        buf.extend_from_slice(
            &GenlHeader {
                cmd: CTRL_CMD_NEWFAMILY,
                version: GENL_VERSION_CTRL,
            }
            .to_bytes(),
        );
        encode_attribute(&mut buf, CTRL_ATTR_FAMILY_NAME, b"nl80211\0");
        parser::finalize_message_length(&mut buf);

        let (msg, _) = parse_message(&buf).unwrap();
        let err = parse_family_response(&msg, "nl80211").unwrap_err();
        match err {
            ResolveError::MissingAttribute { name } => {
                assert_eq!(name, "CTRL_ATTR_FAMILY_ID");
            }
            other => panic!("expected MissingAttribute, got {other:?}"),
        }
    }
}
