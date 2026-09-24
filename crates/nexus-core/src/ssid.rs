//! Raw 802.11 SSID bytes. See DD-003 §4.2.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 802.11 SSID. 1..=32 bytes per the spec; empty and over-length
/// buffers are rejected at construction. Not required to be UTF-8 —
/// some networks use vendor encodings.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "Vec<u8>", into = "Vec<u8>")]
pub struct Ssid(Vec<u8>);

/// Maximum 802.11 SSID length in bytes.
pub const SSID_MAX_LEN: usize = 32;

/// Error returned by [`Ssid::new`] and the serde `TryFrom` impl.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidSsid {
    #[error("SSID is empty")]
    Empty,
    #[error("SSID is too long: {len} bytes (max {SSID_MAX_LEN})")]
    TooLong { len: usize },
}

impl Ssid {
    /// Construct an `Ssid`, validating the length.
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, InvalidSsid> {
        let bytes = bytes.into();
        match bytes.len() {
            0 => Err(InvalidSsid::Empty),
            1..=SSID_MAX_LEN => Ok(Self(bytes)),
            len => Err(InvalidSsid::TooLong { len }),
        }
    }

    /// Raw SSID bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Length in bytes (guaranteed `1..=32`).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always false — an `Ssid` cannot be constructed empty. Kept
    /// because clippy flags types with `len` but no `is_empty`.
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl fmt::Debug for Ssid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match std::str::from_utf8(&self.0) {
            Ok(s) => write!(f, "Ssid({s:?})"),
            Err(_) => write!(f, "Ssid({:?})", self.0),
        }
    }
}

impl TryFrom<Vec<u8>> for Ssid {
    type Error = InvalidSsid;
    fn try_from(v: Vec<u8>) -> Result<Self, Self::Error> {
        Ssid::new(v)
    }
}

impl From<Ssid> for Vec<u8> {
    fn from(s: Ssid) -> Vec<u8> {
        s.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_one_byte_through_thirty_two_bytes() {
        assert!(Ssid::new(vec![b'x']).is_ok());
        assert!(Ssid::new(vec![b'a'; 32]).is_ok());
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(Ssid::new(Vec::<u8>::new()), Err(InvalidSsid::Empty));
    }

    #[test]
    fn rejects_over_thirty_two_bytes() {
        assert_eq!(
            Ssid::new(vec![b'a'; 33]),
            Err(InvalidSsid::TooLong { len: 33 }),
        );
    }

    #[test]
    fn debug_prints_utf8_when_possible() {
        let s = Ssid::new(b"guest".to_vec()).unwrap();
        assert_eq!(format!("{s:?}"), "Ssid(\"guest\")");
    }

    #[test]
    fn debug_falls_back_to_bytes_on_non_utf8() {
        // 0xFF 0xFE is invalid UTF-8.
        let s = Ssid::new(vec![0xFF, 0xFE]).unwrap();
        assert_eq!(format!("{s:?}"), "Ssid([255, 254])");
    }
}
