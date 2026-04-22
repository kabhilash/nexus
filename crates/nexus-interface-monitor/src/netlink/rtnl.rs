//! rtnetlink (`NETLINK_ROUTE`) message parsing and request building.
//! See DD-001 §§5.1, 6.1 and Appendix A.2.

use super::ParseError;
use super::parser::{
    self, AttributeIter, NLM_F_DUMP, NLM_F_REQUEST, NLMSG_HDRLEN, NetlinkAttribute, NetlinkMessage,
    NetlinkMessageHeader,
};

// ---------------------------------------------------------------------------
// rtnetlink message types (subset relevant to Nexus).
// See <linux/rtnetlink.h>.
// ---------------------------------------------------------------------------

pub const RTM_NEWLINK: u16 = 16;
pub const RTM_DELLINK: u16 = 17;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_SETLINK: u16 = 19;

// ---------------------------------------------------------------------------
// Multicast groups. Bit index = RTNLGRP_* value minus 1 (classic
// netlink group bitmask).
// ---------------------------------------------------------------------------

pub const RTNLGRP_LINK: u32 = 1;
pub const RTMGRP_LINK: u32 = 1 << (RTNLGRP_LINK - 1);

// ---------------------------------------------------------------------------
// ifinfomsg header (Appendix A.2). 16 bytes on every Linux ABI.
// ---------------------------------------------------------------------------

pub const IFINFOMSG_SIZE: usize = 16;

/// `struct ifinfomsg` as described in Appendix A.2. `ifi_family` is
/// almost always `AF_UNSPEC` for Nexus's traffic; `ifi_type` is the
/// `ARPHRD_*` constant that DD-001 §5.1's filter checks against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IfInfoHeader {
    pub family: u8,
    pub ifi_type: u16,
    pub index: i32,
    pub flags: u32,
    pub change: u32,
}

impl IfInfoHeader {
    pub fn to_bytes(self) -> [u8; IFINFOMSG_SIZE] {
        let mut out = [0u8; IFINFOMSG_SIZE];
        out[0] = self.family;
        // out[1] is a pad byte, always zero.
        out[2..4].copy_from_slice(&self.ifi_type.to_ne_bytes());
        out[4..8].copy_from_slice(&self.index.to_ne_bytes());
        out[8..12].copy_from_slice(&self.flags.to_ne_bytes());
        out[12..16].copy_from_slice(&self.change.to_ne_bytes());
        out
    }
}

/// Parse `ifinfomsg` off the front of a buffer and return (header,
/// remainder). The remainder holds the NLA chain.
pub fn parse_ifinfomsg(buf: &[u8]) -> Result<(IfInfoHeader, &[u8]), ParseError> {
    if buf.len() < IFINFOMSG_SIZE {
        return Err(ParseError::Truncated {
            need: IFINFOMSG_SIZE,
            got: buf.len(),
        });
    }
    let header = IfInfoHeader {
        family: buf[0],
        // buf[1] is pad
        ifi_type: u16::from_ne_bytes([buf[2], buf[3]]),
        index: i32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]),
        flags: u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]),
        change: u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]),
    };
    Ok((header, &buf[IFINFOMSG_SIZE..]))
}

// ---------------------------------------------------------------------------
// ARPHRD_* interface hardware types (subset used by DD-001 §5.1).
// ---------------------------------------------------------------------------

pub const ARPHRD_ETHER: u16 = 1;
pub const ARPHRD_LOOPBACK: u16 = 772;
pub const ARPHRD_NONE: u16 = 0xFFFE;

// ---------------------------------------------------------------------------
// IFF_* interface flags (subset used by Nexus).
// ---------------------------------------------------------------------------

pub const IFF_UP: u32 = 1 << 0;
pub const IFF_BROADCAST: u32 = 1 << 1;
pub const IFF_DEBUG: u32 = 1 << 2;
pub const IFF_LOOPBACK: u32 = 1 << 3;
pub const IFF_POINTOPOINT: u32 = 1 << 4;
pub const IFF_NOTRAILERS: u32 = 1 << 5;
pub const IFF_RUNNING: u32 = 1 << 6;
pub const IFF_NOARP: u32 = 1 << 7;
pub const IFF_PROMISC: u32 = 1 << 8;
pub const IFF_ALLMULTI: u32 = 1 << 9;
pub const IFF_MASTER: u32 = 1 << 10;
pub const IFF_SLAVE: u32 = 1 << 11;
pub const IFF_MULTICAST: u32 = 1 << 12;
pub const IFF_DORMANT: u32 = 1 << 17;
pub const IFF_LOWER_UP: u32 = 1 << 16;

// ---------------------------------------------------------------------------
// IF_OPER_* operstate values (as carried in `IFLA_OPERSTATE`, u8).
// ---------------------------------------------------------------------------

pub const IF_OPER_UNKNOWN: u8 = 0;
pub const IF_OPER_NOTPRESENT: u8 = 1;
pub const IF_OPER_DOWN: u8 = 2;
pub const IF_OPER_LOWERLAYERDOWN: u8 = 3;
pub const IF_OPER_TESTING: u8 = 4;
pub const IF_OPER_DORMANT: u8 = 5;
pub const IF_OPER_UP: u8 = 6;

// ---------------------------------------------------------------------------
// IFLA_* attribute IDs (subset in DD-001 §5.1). Values match
// `<linux/if_link.h>`.
// ---------------------------------------------------------------------------

pub const IFLA_UNSPEC: u16 = 0;
pub const IFLA_ADDRESS: u16 = 1;
pub const IFLA_BROADCAST: u16 = 2;
pub const IFLA_IFNAME: u16 = 3;
pub const IFLA_MTU: u16 = 4;
pub const IFLA_LINK: u16 = 5;
pub const IFLA_QDISC: u16 = 6;
pub const IFLA_STATS: u16 = 7;
pub const IFLA_MASTER: u16 = 10;
pub const IFLA_TXQLEN: u16 = 13;
pub const IFLA_MAP: u16 = 14;
pub const IFLA_WEIGHT: u16 = 15;
pub const IFLA_OPERSTATE: u16 = 16;
pub const IFLA_LINKMODE: u16 = 17;
pub const IFLA_LINKINFO: u16 = 18;
pub const IFLA_IFALIAS: u16 = 20;
pub const IFLA_STATS64: u16 = 23;
pub const IFLA_GROUP: u16 = 27;
pub const IFLA_CARRIER: u16 = 33;
pub const IFLA_PHYS_PORT_ID: u16 = 34;
pub const IFLA_CARRIER_CHANGES: u16 = 35;
pub const IFLA_PHYS_SWITCH_ID: u16 = 36;
pub const IFLA_PHYS_PORT_NAME: u16 = 38;

// IFLA_LINKINFO's nested attribute IDs.
pub const IFLA_INFO_UNSPEC: u16 = 0;
pub const IFLA_INFO_KIND: u16 = 1;
pub const IFLA_INFO_DATA: u16 = 2;

// ---------------------------------------------------------------------------
// Parsed representation of one RTM_NEWLINK / RTM_DELLINK message.
// ---------------------------------------------------------------------------

/// Extracted fields from an `RTM_NEWLINK` / `RTM_DELLINK` message.
/// Only the attributes listed in DD-001 §5.1 are decoded explicitly;
/// others are silently skipped per §9.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkMessage {
    pub header: IfInfoHeader,
    pub ifname: Option<String>,
    pub mac: Option<[u8; 6]>,
    pub mtu: Option<u32>,
    pub operstate: Option<u8>,
    /// `IFLA_CARRIER` is u32; Nexus consumers only care about
    /// truthiness.
    pub carrier: Option<bool>,
    /// `IFLA_LINKINFO` → `IFLA_INFO_KIND`. Present only for virtual
    /// devices (veth, bridge, vlan, …).
    pub info_kind: Option<String>,
    /// `IFLA_LINK` — underlying device ifindex for stacked devices.
    pub link: Option<i32>,
    /// `IFLA_PHYS_PORT_NAME` — physical port name for multi-port NICs.
    pub phys_port_name: Option<String>,
}

impl LinkMessage {
    /// True if DD-001 §5.1's "skip virtual/stacked" filter matches
    /// `info_kind`.
    pub fn is_virtual_kind(&self) -> bool {
        matches!(
            self.info_kind.as_deref(),
            Some("veth")
                | Some("bridge")
                | Some("bond")
                | Some("vlan")
                | Some("macvlan")
                | Some("tun")
                | Some("tap")
                | Some("gre")
                | Some("ipip")
                | Some("sit")
                | Some("ip6tnl")
        )
    }
}

/// Parse a full `RTM_NEWLINK` / `RTM_DELLINK` message payload
/// (everything after the outer `nlmsghdr`).
pub fn parse_link_message(payload: &[u8]) -> Result<LinkMessage, ParseError> {
    let (header, attrs_buf) = parse_ifinfomsg(payload)?;
    let mut out = LinkMessage {
        header,
        ifname: None,
        mac: None,
        mtu: None,
        operstate: None,
        carrier: None,
        info_kind: None,
        link: None,
        phys_port_name: None,
    };

    for attr in AttributeIter::new(attrs_buf) {
        let attr = attr?;
        match attr.attr_type {
            IFLA_IFNAME => out.ifname = Some(attr.cstr()?.to_owned()),
            IFLA_ADDRESS => {
                let bytes = attr.bytes();
                if bytes.len() == 6 {
                    let mut mac = [0u8; 6];
                    mac.copy_from_slice(bytes);
                    out.mac = Some(mac);
                }
                // ARPHRD types other than ARPHRD_ETHER have different
                // address widths; skip silently rather than erroring.
            }
            IFLA_MTU => out.mtu = Some(attr.u32()?),
            IFLA_OPERSTATE => out.operstate = Some(attr.u8()?),
            IFLA_CARRIER => out.carrier = Some(attr.u32()? != 0),
            IFLA_LINK => out.link = Some(attr.i32()?),
            IFLA_PHYS_PORT_NAME => out.phys_port_name = Some(attr.cstr()?.to_owned()),
            IFLA_LINKINFO => {
                if let Some(kind) = extract_info_kind(&attr)? {
                    out.info_kind = Some(kind);
                }
            }
            _ => {
                // Unknown attribute types are preserved for forward
                // compatibility per DD-001 §9.3; we simply skip.
            }
        }
    }

    Ok(out)
}

fn extract_info_kind<'a>(linkinfo: &NetlinkAttribute<'a>) -> Result<Option<String>, ParseError> {
    for nested in linkinfo.nested() {
        let nested = nested?;
        if nested.attr_type == IFLA_INFO_KIND {
            return Ok(Some(nested.cstr()?.to_owned()));
        }
    }
    Ok(None)
}

/// Convenience for consumers that already have a parsed outer
/// message. Equivalent to `parse_link_message(msg.payload)`.
pub fn link_message_from(msg: &NetlinkMessage<'_>) -> Result<LinkMessage, ParseError> {
    parse_link_message(msg.payload)
}

// ---------------------------------------------------------------------------
// Request builders.
// ---------------------------------------------------------------------------

/// Build an `RTM_GETLINK` dump request covering every interface.
/// Matches the pseudocode in DD-001 §5.1.
pub fn build_rtm_getlink_dump_request(seq: u32, port_id: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(NLMSG_HDRLEN + IFINFOMSG_SIZE);

    let placeholder_header = NetlinkMessageHeader {
        length: 0, // patched by finalize_message_length
        msg_type: RTM_GETLINK,
        flags: NLM_F_REQUEST | NLM_F_DUMP,
        seq,
        pid: port_id,
    };
    buf.extend_from_slice(&placeholder_header.to_bytes());

    let ifi = IfInfoHeader {
        family: 0, // AF_UNSPEC — dump all families
        ifi_type: 0,
        index: 0,
        flags: 0,
        change: 0,
    };
    buf.extend_from_slice(&ifi.to_bytes());

    // No attributes on a dump request.
    parser::finalize_message_length(&mut buf);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::parser::{
        MessageIter, NLM_F_MULTI, NLMSG_DONE, encode_attribute, parse_message,
    };

    /// Hand-crafted RTM_NEWLINK message. Constructed from the
    /// rtnetlink(7) man-page example layout so the parser can
    /// prove it decodes every field.
    ///
    /// | Section | Offset | Bytes |
    /// |---|---|---|
    /// | `nlmsghdr`             | 0..16   | 16 |
    /// | `ifinfomsg`            | 16..32  | 16 |
    /// | `IFLA_IFNAME "eth0\0"` | 32..44  | 12 (9 + 3 pad) |
    /// | `IFLA_ADDRESS` 6 B MAC | 44..56  | 12 (10 + 2 pad) |
    /// | `IFLA_MTU` 1500        | 56..64  | 8 |
    /// | `IFLA_OPERSTATE` Up    | 64..72  | 8 (5 + 3 pad) |
    /// | `IFLA_CARRIER` 1       | 72..80  | 8 |
    /// | `IFLA_LINKINFO` nested | 80..96  | 16 |
    /// | total                  | 96      |
    fn captured_newlink_message() -> Vec<u8> {
        let mut buf = Vec::new();

        // Outer header: length patched in at the end, type =
        // RTM_NEWLINK, flags = NLM_F_MULTI, seq = 1, pid = 0 (kernel).
        let hdr = NetlinkMessageHeader {
            length: 0,
            msg_type: RTM_NEWLINK,
            flags: NLM_F_MULTI,
            seq: 1,
            pid: 0,
        };
        buf.extend_from_slice(&hdr.to_bytes());

        // ifinfomsg: AF_UNSPEC family, ARPHRD_ETHER type, ifindex 2,
        // flags UP|BROADCAST|RUNNING|LOWER_UP, change mask 0xFFFFFFFF.
        let ifi = IfInfoHeader {
            family: 0,
            ifi_type: ARPHRD_ETHER,
            index: 2,
            flags: IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_LOWER_UP,
            change: 0xFFFF_FFFF,
        };
        buf.extend_from_slice(&ifi.to_bytes());

        encode_attribute(&mut buf, IFLA_IFNAME, b"eth0\0");
        encode_attribute(
            &mut buf,
            IFLA_ADDRESS,
            &[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
        );
        encode_attribute(&mut buf, IFLA_MTU, &1500u32.to_ne_bytes());
        encode_attribute(&mut buf, IFLA_OPERSTATE, &[IF_OPER_UP]);
        encode_attribute(&mut buf, IFLA_CARRIER, &1u32.to_ne_bytes());

        // IFLA_LINKINFO containing IFLA_INFO_KIND="veth\0"
        let mut linkinfo_payload = Vec::new();
        encode_attribute(&mut linkinfo_payload, IFLA_INFO_KIND, b"veth\0");
        encode_attribute(&mut buf, IFLA_LINKINFO, &linkinfo_payload);

        parser::finalize_message_length(&mut buf);
        buf
    }

    #[test]
    fn captured_newlink_decodes_every_field() {
        let raw = captured_newlink_message();
        assert_eq!(raw.len(), 96, "golden layout is 96 bytes");

        let (msg, rest) = parse_message(&raw).unwrap();
        assert!(rest.is_empty(), "single-message buffer");
        assert_eq!(msg.header.msg_type, RTM_NEWLINK);
        assert_eq!(msg.header.flags, NLM_F_MULTI);
        assert_eq!(msg.header.seq, 1);
        assert_eq!(msg.header.pid, 0);
        assert_eq!(msg.header.length as usize, raw.len());

        let link = parse_link_message(msg.payload).unwrap();
        assert_eq!(link.header.family, 0);
        assert_eq!(link.header.ifi_type, ARPHRD_ETHER);
        assert_eq!(link.header.index, 2);
        assert_eq!(
            link.header.flags,
            IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_LOWER_UP,
        );
        assert_eq!(link.header.change, 0xFFFF_FFFF);

        assert_eq!(link.ifname.as_deref(), Some("eth0"));
        assert_eq!(link.mac, Some([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]));
        assert_eq!(link.mtu, Some(1500));
        assert_eq!(link.operstate, Some(IF_OPER_UP));
        assert_eq!(link.carrier, Some(true));
        assert_eq!(link.info_kind.as_deref(), Some("veth"));
        assert!(link.is_virtual_kind());
    }

    #[test]
    fn multipart_dump_terminates_at_nlmsg_done() {
        // Two RTM_NEWLINK messages followed by a terminating
        // NLMSG_DONE. MessageIter should surface all three.
        let mut buf = Vec::new();
        buf.extend_from_slice(&captured_newlink_message());
        buf.extend_from_slice(&captured_newlink_message());

        let done = NetlinkMessageHeader {
            length: NLMSG_HDRLEN as u32,
            msg_type: NLMSG_DONE,
            flags: NLM_F_MULTI,
            seq: 1,
            pid: 0,
        };
        buf.extend_from_slice(&done.to_bytes());

        let types: Vec<u16> = MessageIter::new(&buf)
            .map(|r| r.unwrap().header.msg_type)
            .collect();
        assert_eq!(types, vec![RTM_NEWLINK, RTM_NEWLINK, NLMSG_DONE]);
    }

    #[test]
    fn build_rtm_getlink_dump_layout_is_stable() {
        let bytes = build_rtm_getlink_dump_request(42, 99);

        // Total length: 16 (nlmsghdr) + 16 (ifinfomsg) = 32 bytes.
        assert_eq!(bytes.len(), 32);

        // Parse the header back and verify every field.
        let header = parser::parse_header(&bytes[..NLMSG_HDRLEN]).unwrap();
        assert_eq!(header.length, 32);
        assert_eq!(header.msg_type, RTM_GETLINK);
        assert_eq!(header.flags, NLM_F_REQUEST | NLM_F_DUMP);
        assert_eq!(header.seq, 42);
        assert_eq!(header.pid, 99);

        // The ifinfomsg is all zeros (AF_UNSPEC dump).
        assert!(bytes[NLMSG_HDRLEN..].iter().all(|&b| b == 0));

        // Round-trip: re-parse the ifinfomsg via parse_ifinfomsg.
        let (_, msg_rest) = parse_message(&bytes).unwrap();
        assert!(msg_rest.is_empty());
    }

    #[test]
    fn truncated_link_message_errors_rather_than_panics() {
        // Outer header says length=40 but we only supply 30 bytes.
        let hdr = NetlinkMessageHeader {
            length: 40,
            msg_type: RTM_NEWLINK,
            flags: 0,
            seq: 1,
            pid: 0,
        };
        let mut buf = hdr.to_bytes().to_vec();
        buf.resize(30, 0u8);
        let err = parse_message(&buf).unwrap_err();
        assert_eq!(
            err,
            ParseError::MessageLengthExceedsBuffer {
                declared: 40,
                buf_len: 30,
            },
        );
    }

    #[test]
    fn invalid_tlv_length_in_attrs_errors() {
        // Build an ifinfomsg followed by a TLV whose nla_len = 2
        // (below the 4-byte header minimum).
        let ifi = IfInfoHeader {
            family: 0,
            ifi_type: ARPHRD_ETHER,
            index: 1,
            flags: 0,
            change: 0,
        };
        let mut payload = ifi.to_bytes().to_vec();
        // malformed attribute header: len=2, type=IFLA_IFNAME
        payload.extend_from_slice(&[0x02, 0x00, 0x03, 0x00]);
        let err = parse_link_message(&payload).unwrap_err();
        assert_eq!(err, ParseError::AttributeTooShort { nla_len: 2 });
    }
}
