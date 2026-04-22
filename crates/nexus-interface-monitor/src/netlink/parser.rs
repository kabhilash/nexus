//! Generic netlink message and attribute (TLV) parsing. Protocol-
//! agnostic; see `rtnl.rs`, `genl.rs`, and `nl80211.rs` for the
//! protocol-specific wrappers. See DD-001 Appendix A.1-A.5 and §9.3.

use std::str;

use super::ParseError;

// ---------------------------------------------------------------------------
// Message header constants (Appendix §A.1)
// ---------------------------------------------------------------------------

/// Size of the outer `nlmsghdr` in bytes.
pub const NLMSG_HDRLEN: usize = 16;

/// Alignment for both message lengths and attribute lengths.
pub const NLMSG_ALIGNTO: usize = 4;

/// No-op, padding message. Ignore.
pub const NLMSG_NOOP: u16 = 1;
/// Error or ACK. Payload is `nlmsgerr`.
pub const NLMSG_ERROR: u16 = 2;
/// Terminator for a multi-part dump.
pub const NLMSG_DONE: u16 = 3;
/// Kernel buffer overrun; treat as message drop.
pub const NLMSG_OVERRUN: u16 = 4;

// Flags defined in <linux/netlink.h>.

pub const NLM_F_REQUEST: u16 = 0x01;
pub const NLM_F_MULTI: u16 = 0x02;
pub const NLM_F_ACK: u16 = 0x04;
pub const NLM_F_ECHO: u16 = 0x08;
pub const NLM_F_DUMP_INTR: u16 = 0x10;
pub const NLM_F_DUMP_FILTERED: u16 = 0x20;

// GET-request modifiers.
pub const NLM_F_ROOT: u16 = 0x100;
pub const NLM_F_MATCH: u16 = 0x200;
pub const NLM_F_ATOMIC: u16 = 0x400;
pub const NLM_F_DUMP: u16 = NLM_F_ROOT | NLM_F_MATCH;

// Attribute-type flags (Appendix §A.3).
pub const NLA_F_NESTED: u16 = 0x8000;
pub const NLA_F_NET_BYTEORDER: u16 = 0x4000;
pub const NLA_TYPE_MASK: u16 = !(NLA_F_NESTED | NLA_F_NET_BYTEORDER);

/// Size of the NLA header in bytes (`u16 nla_len` + `u16 nla_type`).
pub const NLA_HDRLEN: usize = 4;

/// Round `len` up to the netlink alignment (4 bytes).
pub const fn nlmsg_align(len: usize) -> usize {
    (len + NLMSG_ALIGNTO - 1) & !(NLMSG_ALIGNTO - 1)
}

/// Alias for `nlmsg_align` — NLAs share the same alignment.
pub const fn nla_align(len: usize) -> usize {
    nlmsg_align(len)
}

// ---------------------------------------------------------------------------
// Outer message header
// ---------------------------------------------------------------------------

/// Parsed outer `nlmsghdr`. See Appendix §A.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetlinkMessageHeader {
    /// Total message length in bytes (header + payload, unpadded).
    pub length: u32,
    /// `RTM_NEWLINK`, `NLMSG_DONE`, or a generic-netlink family ID.
    pub msg_type: u16,
    /// `NLM_F_*`.
    pub flags: u16,
    /// Sender-chosen correlator.
    pub seq: u32,
    /// Port ID. Kernel-originated messages carry `0`.
    pub pid: u32,
}

impl NetlinkMessageHeader {
    /// Serialize into a 16-byte buffer in host byte order.
    pub fn to_bytes(self) -> [u8; NLMSG_HDRLEN] {
        let mut out = [0u8; NLMSG_HDRLEN];
        out[0..4].copy_from_slice(&self.length.to_ne_bytes());
        out[4..6].copy_from_slice(&self.msg_type.to_ne_bytes());
        out[6..8].copy_from_slice(&self.flags.to_ne_bytes());
        out[8..12].copy_from_slice(&self.seq.to_ne_bytes());
        out[12..16].copy_from_slice(&self.pid.to_ne_bytes());
        out
    }
}

/// Parse only the outer `nlmsghdr`. Does not consume the payload.
pub fn parse_header(buf: &[u8]) -> Result<NetlinkMessageHeader, ParseError> {
    if buf.len() < NLMSG_HDRLEN {
        return Err(ParseError::Truncated {
            need: NLMSG_HDRLEN,
            got: buf.len(),
        });
    }
    Ok(NetlinkMessageHeader {
        length: u32::from_ne_bytes([buf[0], buf[1], buf[2], buf[3]]),
        msg_type: u16::from_ne_bytes([buf[4], buf[5]]),
        flags: u16::from_ne_bytes([buf[6], buf[7]]),
        seq: u32::from_ne_bytes([buf[8], buf[9], buf[10], buf[11]]),
        pid: u32::from_ne_bytes([buf[12], buf[13], buf[14], buf[15]]),
    })
}

/// A single netlink message carved out of a larger buffer.
#[derive(Debug, Clone, Copy)]
pub struct NetlinkMessage<'a> {
    pub header: NetlinkMessageHeader,
    /// Protocol-specific payload: `ifinfomsg` + NLA chain for
    /// rtnetlink, `genlmsghdr` + NLA chain for generic netlink, or
    /// `nlmsgerr` for `NLMSG_ERROR`.
    pub payload: &'a [u8],
}

/// Parse one message from the buffer, returning the message and the
/// remainder (starting at the next 4-byte-aligned boundary).
pub fn parse_message(buf: &[u8]) -> Result<(NetlinkMessage<'_>, &[u8]), ParseError> {
    let header = parse_header(buf)?;
    let len = header.length as usize;
    if len < NLMSG_HDRLEN {
        return Err(ParseError::MessageLengthTooSmall {
            declared: len,
            min: NLMSG_HDRLEN,
        });
    }
    if len > buf.len() {
        return Err(ParseError::MessageLengthExceedsBuffer {
            declared: len,
            buf_len: buf.len(),
        });
    }
    let payload = &buf[NLMSG_HDRLEN..len];
    let aligned = nlmsg_align(len).min(buf.len());
    let rest = &buf[aligned..];
    Ok((NetlinkMessage { header, payload }, rest))
}

/// Iterator that walks successive messages in a datagram.
#[derive(Debug, Clone, Copy)]
pub struct MessageIter<'a> {
    buf: &'a [u8],
}

impl<'a> MessageIter<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl<'a> Iterator for MessageIter<'a> {
    type Item = Result<NetlinkMessage<'a>, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        match parse_message(self.buf) {
            Ok((msg, rest)) => {
                self.buf = rest;
                Some(Ok(msg))
            }
            Err(e) => {
                // Don't loop forever on a broken datagram.
                self.buf = &[];
                Some(Err(e))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// NLMSG_ERROR payload (Appendix §A.5)
// ---------------------------------------------------------------------------

/// Payload of an `NLMSG_ERROR` message. `error == 0` is the ACK form
/// used when `NLM_F_ACK` was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetlinkError {
    /// Negative errno, or 0 for ACK.
    pub error: i32,
    /// The echoed `nlmsghdr` of the request that generated the error.
    pub original_header: NetlinkMessageHeader,
}

/// Parse an `NLMSG_ERROR` payload. The caller is responsible for
/// checking that the outer `msg_type == NLMSG_ERROR`.
pub fn parse_nlmsgerr(payload: &[u8]) -> Result<NetlinkError, ParseError> {
    const NEED: usize = 4 + NLMSG_HDRLEN;
    if payload.len() < NEED {
        return Err(ParseError::Truncated {
            need: NEED,
            got: payload.len(),
        });
    }
    let error = i32::from_ne_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let original_header = parse_header(&payload[4..4 + NLMSG_HDRLEN])?;
    Ok(NetlinkError {
        error,
        original_header,
    })
}

// ---------------------------------------------------------------------------
// Netlink attributes (TLVs) — Appendix §A.3, forward-compat rules §9.3
// ---------------------------------------------------------------------------

/// One parsed netlink attribute. `attr_type` has the `NLA_F_NESTED`
/// and `NLA_F_NET_BYTEORDER` bits masked off; the raw flags are kept
/// in `flags` for protocols that rely on the nested bit.
#[derive(Debug, Clone, Copy)]
pub struct NetlinkAttribute<'a> {
    pub attr_type: u16,
    pub flags: u16,
    pub payload: &'a [u8],
}

impl<'a> NetlinkAttribute<'a> {
    /// True if the raw nla_type carried `NLA_F_NESTED`.
    pub fn is_nested(&self) -> bool {
        self.flags & NLA_F_NESTED != 0
    }

    /// Interpret payload as `u8`.
    pub fn u8(&self) -> Result<u8, ParseError> {
        self.expect_len("u8", 1)?;
        Ok(self.payload[0])
    }

    /// Interpret payload as `u16` (host byte order).
    pub fn u16(&self) -> Result<u16, ParseError> {
        self.expect_len("u16", 2)?;
        Ok(u16::from_ne_bytes([self.payload[0], self.payload[1]]))
    }

    /// Interpret payload as `u32` (host byte order).
    pub fn u32(&self) -> Result<u32, ParseError> {
        self.expect_len("u32", 4)?;
        Ok(u32::from_ne_bytes([
            self.payload[0],
            self.payload[1],
            self.payload[2],
            self.payload[3],
        ]))
    }

    /// Interpret payload as `i32` (host byte order).
    pub fn i32(&self) -> Result<i32, ParseError> {
        Ok(self.u32()? as i32)
    }

    /// Interpret payload as `u64` (host byte order).
    pub fn u64(&self) -> Result<u64, ParseError> {
        self.expect_len("u64", 8)?;
        let mut b = [0u8; 8];
        b.copy_from_slice(&self.payload[..8]);
        Ok(u64::from_ne_bytes(b))
    }

    /// Interpret payload as a NUL-terminated ASCII/UTF-8 string
    /// (the `IFLA_IFNAME` / `CTRL_ATTR_FAMILY_NAME` convention).
    pub fn cstr(&self) -> Result<&'a str, ParseError> {
        let end = self
            .payload
            .iter()
            .position(|&b| b == 0)
            .ok_or(ParseError::AttributeStringMissingNul)?;
        str::from_utf8(&self.payload[..end]).map_err(|_| ParseError::AttributeNotUtf8)
    }

    /// Raw payload bytes.
    pub fn bytes(&self) -> &'a [u8] {
        self.payload
    }

    /// Iterate over child attributes. Use for nested NLAs.
    pub fn nested(&self) -> AttributeIter<'a> {
        AttributeIter::new(self.payload)
    }

    fn expect_len(&self, kind: &'static str, need: usize) -> Result<(), ParseError> {
        if self.payload.len() < need {
            Err(ParseError::AttributeWrongSize {
                kind,
                need,
                got: self.payload.len(),
            })
        } else {
            Ok(())
        }
    }
}

/// Parse one NLA. Returns the attribute and the remaining buffer
/// advanced past the 4-byte-aligned end of this attribute.
pub fn parse_attribute(buf: &[u8]) -> Result<(NetlinkAttribute<'_>, &[u8]), ParseError> {
    if buf.len() < NLA_HDRLEN {
        return Err(ParseError::Truncated {
            need: NLA_HDRLEN,
            got: buf.len(),
        });
    }
    let nla_len = u16::from_ne_bytes([buf[0], buf[1]]) as usize;
    let raw_type = u16::from_ne_bytes([buf[2], buf[3]]);
    if nla_len < NLA_HDRLEN {
        return Err(ParseError::AttributeTooShort { nla_len });
    }
    if nla_len > buf.len() {
        return Err(ParseError::AttributeTruncated {
            nla_len,
            remaining: buf.len(),
        });
    }
    let attr = NetlinkAttribute {
        attr_type: raw_type & NLA_TYPE_MASK,
        flags: raw_type & !NLA_TYPE_MASK,
        payload: &buf[NLA_HDRLEN..nla_len],
    };
    // Advance past the aligned end of this attribute. The kernel
    // always pads, but a truncated trailing attribute may be the
    // final one — in that case aligned > buf.len() and we clamp.
    let aligned = nla_align(nla_len).min(buf.len());
    Ok((attr, &buf[aligned..]))
}

/// Iterator over a chain of NLAs. Unknown attribute types are
/// surfaced with their raw type ID; the caller decides whether to
/// look them up. See §9.3: the parser preserves forward compat by
/// never erroring on unknown types.
#[derive(Debug, Clone, Copy)]
pub struct AttributeIter<'a> {
    buf: &'a [u8],
}

impl<'a> AttributeIter<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }

    /// Collect attributes into a `Vec`, stopping at the first parse
    /// error. Convenient for protocols that look up attributes by
    /// type ID rather than walking in order.
    pub fn collect_ok(self) -> (Vec<NetlinkAttribute<'a>>, Option<ParseError>) {
        let mut out = Vec::new();
        for attr in self {
            match attr {
                Ok(a) => out.push(a),
                Err(e) => return (out, Some(e)),
            }
        }
        (out, None)
    }
}

impl<'a> Iterator for AttributeIter<'a> {
    type Item = Result<NetlinkAttribute<'a>, ParseError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buf.is_empty() {
            return None;
        }
        match parse_attribute(self.buf) {
            Ok((attr, rest)) => {
                self.buf = rest;
                Some(Ok(attr))
            }
            Err(e) => {
                self.buf = &[];
                Some(Err(e))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Encoder helpers for building outbound messages.
// ---------------------------------------------------------------------------

/// Append `nla_len`, `nla_type`, `payload`, and alignment padding to
/// `out`. Returns the total bytes appended.
pub fn encode_attribute(out: &mut Vec<u8>, attr_type: u16, payload: &[u8]) -> usize {
    let nla_len = NLA_HDRLEN + payload.len();
    out.extend_from_slice(&(nla_len as u16).to_ne_bytes());
    out.extend_from_slice(&attr_type.to_ne_bytes());
    out.extend_from_slice(payload);
    let pad = nla_align(nla_len) - nla_len;
    out.extend(std::iter::repeat_n(0u8, pad));
    NLA_HDRLEN + payload.len() + pad
}

/// Write the final `nlmsg_len` into a request buffer whose first
/// 4 bytes are a placeholder. Used after all attributes have been
/// appended.
pub fn finalize_message_length(buf: &mut [u8]) {
    let len = buf.len() as u32;
    buf[0..4].copy_from_slice(&len.to_ne_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nlmsg_align_rounds_up_to_four() {
        assert_eq!(nlmsg_align(0), 0);
        assert_eq!(nlmsg_align(1), 4);
        assert_eq!(nlmsg_align(4), 4);
        assert_eq!(nlmsg_align(5), 8);
        assert_eq!(nlmsg_align(17), 20);
    }

    #[test]
    fn header_parse_rejects_short_buffer() {
        let err = parse_header(&[0u8; 15]).unwrap_err();
        assert_eq!(
            err,
            ParseError::Truncated {
                need: NLMSG_HDRLEN,
                got: 15,
            },
        );
    }

    #[test]
    fn header_roundtrip_via_to_bytes() {
        let h = NetlinkMessageHeader {
            length: 0x20,
            msg_type: NLMSG_DONE,
            flags: NLM_F_MULTI,
            seq: 0xDEAD_BEEF,
            pid: 0x4242,
        };
        let bytes = h.to_bytes();
        assert_eq!(parse_header(&bytes).unwrap(), h);
    }

    #[test]
    fn message_parse_returns_remainder() {
        // Two minimal NLMSG_NOOP messages back-to-back.
        let mut buf = Vec::new();
        for seq in [1u32, 2u32] {
            let h = NetlinkMessageHeader {
                length: NLMSG_HDRLEN as u32,
                msg_type: NLMSG_NOOP,
                flags: 0,
                seq,
                pid: 0,
            };
            buf.extend_from_slice(&h.to_bytes());
        }

        let iter = MessageIter::new(&buf);
        let seqs: Vec<u32> = iter.map(|r| r.unwrap().header.seq).collect();
        assert_eq!(seqs, vec![1, 2]);
    }

    #[test]
    fn message_length_too_small_errors() {
        let h = NetlinkMessageHeader {
            length: 8, // < NLMSG_HDRLEN
            msg_type: NLMSG_NOOP,
            flags: 0,
            seq: 0,
            pid: 0,
        };
        let err = parse_message(&h.to_bytes()).unwrap_err();
        assert_eq!(
            err,
            ParseError::MessageLengthTooSmall {
                declared: 8,
                min: NLMSG_HDRLEN,
            },
        );
    }

    #[test]
    fn message_length_exceeds_buffer_errors() {
        let h = NetlinkMessageHeader {
            length: 64,
            msg_type: NLMSG_NOOP,
            flags: 0,
            seq: 0,
            pid: 0,
        };
        // Only 16 bytes in the buffer, but header says 64.
        let err = parse_message(&h.to_bytes()).unwrap_err();
        assert_eq!(
            err,
            ParseError::MessageLengthExceedsBuffer {
                declared: 64,
                buf_len: NLMSG_HDRLEN,
            },
        );
    }

    #[test]
    fn nlmsgerr_parses_negative_errno() {
        const ENOENT: i32 = -2;
        let echoed = NetlinkMessageHeader {
            length: NLMSG_HDRLEN as u32,
            msg_type: 16, // e.g. RTM_NEWLINK
            flags: NLM_F_REQUEST,
            seq: 7,
            pid: 99,
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&ENOENT.to_ne_bytes());
        payload.extend_from_slice(&echoed.to_bytes());

        let parsed = parse_nlmsgerr(&payload).unwrap();
        assert_eq!(parsed.error, ENOENT);
        assert_eq!(parsed.original_header, echoed);
    }

    #[test]
    fn attribute_roundtrip_u32() {
        let mut buf = Vec::new();
        encode_attribute(&mut buf, 4, &1500u32.to_ne_bytes());
        let (attr, rest) = parse_attribute(&buf).unwrap();
        assert_eq!(attr.attr_type, 4);
        assert_eq!(attr.u32().unwrap(), 1500);
        assert!(rest.is_empty());
    }

    #[test]
    fn attribute_cstr_stops_at_nul() {
        let mut buf = Vec::new();
        encode_attribute(&mut buf, 3, b"eth0\0");
        let (attr, _) = parse_attribute(&buf).unwrap();
        assert_eq!(attr.cstr().unwrap(), "eth0");
    }

    #[test]
    fn attribute_missing_nul_errors() {
        let mut buf = Vec::new();
        encode_attribute(&mut buf, 3, b"eth0"); // no trailing NUL
        let (attr, _) = parse_attribute(&buf).unwrap();
        assert_eq!(
            attr.cstr().unwrap_err(),
            ParseError::AttributeStringMissingNul
        );
    }

    #[test]
    fn attribute_too_short_errors() {
        // nla_len = 2, which is less than NLA_HDRLEN.
        let buf = [0x02, 0x00, 0x00, 0x00];
        assert_eq!(
            parse_attribute(&buf).unwrap_err(),
            ParseError::AttributeTooShort { nla_len: 2 },
        );
    }

    #[test]
    fn attribute_claims_more_than_remaining_errors() {
        // nla_len = 12 but only 6 bytes in the buffer.
        let buf = [0x0c, 0x00, 0x01, 0x00, 0xaa, 0xbb];
        assert_eq!(
            parse_attribute(&buf).unwrap_err(),
            ParseError::AttributeTruncated {
                nla_len: 12,
                remaining: 6,
            },
        );
    }

    #[test]
    fn nested_flag_is_exposed_and_masked_from_type() {
        // type = NLA_F_NESTED | 7
        let mut buf = Vec::new();
        buf.extend_from_slice(&(NLA_HDRLEN as u16).to_ne_bytes());
        buf.extend_from_slice(&(NLA_F_NESTED | 7).to_ne_bytes());
        let (attr, _) = parse_attribute(&buf).unwrap();
        assert_eq!(attr.attr_type, 7);
        assert!(attr.is_nested());
    }

    #[test]
    fn attribute_iter_skips_unknown_types_without_erroring() {
        let mut buf = Vec::new();
        encode_attribute(&mut buf, 3, b"eth0\0");
        encode_attribute(&mut buf, 0x3FFE, b"ignore me"); // fictitious type, no flag bits
        encode_attribute(&mut buf, 4, &1500u32.to_ne_bytes());

        let (attrs, err) = AttributeIter::new(&buf).collect_ok();
        assert!(err.is_none());
        let types: Vec<u16> = attrs.iter().map(|a| a.attr_type).collect();
        assert_eq!(types, vec![3, 0x3FFE, 4]);
    }
}
