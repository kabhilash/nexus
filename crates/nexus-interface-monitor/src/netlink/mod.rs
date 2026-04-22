//! Netlink foundation for the Interface Monitor: hand-rolled parsers
//! (no `netlink-packet-*` crates), async socket wrappers, and the
//! generic-netlink control-family resolver. See DD-001 §§4–5 and
//! Appendix A.

use thiserror::Error;

pub mod genl;
pub mod nl80211;
pub mod parser;
pub mod rtnl;
pub mod socket;

pub use parser::{
    AttributeIter, MessageIter, NLA_F_NESTED, NLA_F_NET_BYTEORDER, NLA_HDRLEN, NLA_TYPE_MASK,
    NLM_F_ACK, NLM_F_DUMP, NLM_F_DUMP_INTR, NLM_F_ECHO, NLM_F_MATCH, NLM_F_MULTI, NLM_F_REQUEST,
    NLM_F_ROOT, NLMSG_DONE, NLMSG_ERROR, NLMSG_HDRLEN, NLMSG_NOOP, NLMSG_OVERRUN, NetlinkAttribute,
    NetlinkError, NetlinkMessage, NetlinkMessageHeader, encode_attribute, finalize_message_length,
    nla_align, nlmsg_align, parse_attribute, parse_header, parse_message, parse_nlmsgerr,
};

/// Errors raised by the parsers. Every parser refuses to panic on
/// malformed input — it returns one of these variants instead.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    /// Not enough bytes remain to read a required fixed-size field.
    #[error("truncated buffer: need {need} bytes, got {got}")]
    Truncated { need: usize, got: usize },

    /// The outer `nlmsg_len` is below the minimum header size.
    #[error("netlink message length {declared} is below minimum {min}")]
    MessageLengthTooSmall { declared: usize, min: usize },

    /// The outer `nlmsg_len` exceeds the bytes actually available.
    #[error("netlink message length {declared} exceeds buffer of {buf_len}")]
    MessageLengthExceedsBuffer { declared: usize, buf_len: usize },

    /// A TLV's `nla_len` is smaller than the 4-byte TLV header.
    #[error("netlink attribute length {nla_len} is below TLV header size")]
    AttributeTooShort { nla_len: usize },

    /// A TLV's `nla_len` runs past the end of its containing buffer.
    #[error("netlink attribute claims {nla_len} bytes, only {remaining} remain")]
    AttributeTruncated { nla_len: usize, remaining: usize },

    /// The payload is the wrong size for the typed accessor called.
    #[error("expected {need} bytes for a {kind} attribute, got {got}")]
    AttributeWrongSize {
        kind: &'static str,
        need: usize,
        got: usize,
    },

    /// `cstr()` was called on a payload with no NUL terminator.
    #[error("expected NUL-terminated string attribute, no NUL found")]
    AttributeStringMissingNul,

    /// A string attribute's bytes did not decode as UTF-8.
    #[error("string attribute was not valid UTF-8")]
    AttributeNotUtf8,
}
