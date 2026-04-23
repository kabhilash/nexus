//! 48-bit hardware addresses shared by Wi-Fi (BSSIDs) and Bluetooth
//! (adapter/device addresses). See DD-003 §4.2 and DD-004 §6.2.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// 48-bit MAC-family address. Used for Wi-Fi BSSIDs and Bluetooth
/// addresses alike (Bluetooth addresses share the MAC-48 format).
///
/// `Debug` prints the canonical lowercase, colon-separated form
/// (`"aa:bb:cc:dd:ee:ff"`) per DD-003 §4.2. BlueZ-flavored parsing
/// and formatting (uppercase, and the `dev_XX_XX…` object-path
/// component) live on the [`BluetoothAddrExt`] trait below.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacAddr(pub [u8; 6]);

impl fmt::Debug for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [a, b, c, d, e, g] = self.0;
        write!(f, "{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

impl fmt::Display for MacAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

// ---------------------------------------------------------------------------
// serde
//
// MacAddr serializes as its canonical lowercase colon form
// (`"aa:bb:cc:dd:ee:ff"`). This is the natural TOML/JSON
// representation for human-readable profile files; the raw bytes
// form would round-trip as a 6-element sequence which is less
// obvious and breaks TOML.
// ---------------------------------------------------------------------------

impl Serialize for MacAddr {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MacAddr {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let s = String::deserialize(deserializer)?;
        parse_any_mac(&s).map_err(D::Error::custom)
    }
}

/// Parse either `"AA:BB:CC:DD:EE:FF"` or `"aa:bb:cc:dd:ee:ff"`
/// (case-insensitive). Used by [`MacAddr`]'s serde impl; lives on
/// `MacAddr` via a free function rather than the [`BluetoothAddrExt`]
/// trait so callers don't have to import the BlueZ extension just
/// to deserialize.
fn parse_any_mac(s: &str) -> Result<MacAddr, ParseMacAddrError> {
    <MacAddr as BluetoothAddrExt>::from_bluez(s)
}

/// Error returned by [`BluetoothAddrExt::from_bluez`] and any other
/// MAC-address parser in `nexus-core`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseMacAddrError {
    /// The input was not exactly 17 ASCII characters (`XX:XX:XX:XX:XX:XX`).
    #[error("invalid length: expected 17 bytes, got {0}")]
    WrongLength(usize),
    /// A separator position did not hold a `:`.
    #[error("missing colon at position {0}")]
    MissingColon(usize),
    /// A hex byte contained a non-hex character.
    #[error("invalid hex byte at position {0}")]
    InvalidHex(usize),
}

/// BlueZ-flavored formatting for [`MacAddr`]. Kept as an extension
/// trait so `MacAddr` itself doesn't need to know about BlueZ
/// conventions (per DD-004 §6.2).
pub trait BluetoothAddrExt {
    /// Parse BlueZ's `"XX:XX:XX:XX:XX:XX"` format. Accepts both
    /// uppercase and lowercase hex; BlueZ itself emits uppercase.
    fn from_bluez(s: &str) -> Result<MacAddr, ParseMacAddrError>;

    /// Format as BlueZ's canonical uppercase `"XX:XX:XX:XX:XX:XX"`.
    fn to_bluez(&self) -> String;

    /// Format as BlueZ's device object-path component,
    /// e.g. `"dev_AA_BB_CC_DD_EE_FF"`.
    fn to_object_path_component(&self) -> String;
}

impl BluetoothAddrExt for MacAddr {
    fn from_bluez(s: &str) -> Result<MacAddr, ParseMacAddrError> {
        let bytes = s.as_bytes();
        if bytes.len() != 17 {
            return Err(ParseMacAddrError::WrongLength(bytes.len()));
        }
        let mut out = [0u8; 6];
        for i in 0..6 {
            let off = i * 3;
            if i < 5 && bytes[off + 2] != b':' {
                return Err(ParseMacAddrError::MissingColon(off + 2));
            }
            let hi = hex_nibble(bytes[off]).ok_or(ParseMacAddrError::InvalidHex(off))?;
            let lo = hex_nibble(bytes[off + 1]).ok_or(ParseMacAddrError::InvalidHex(off + 1))?;
            out[i] = (hi << 4) | lo;
        }
        Ok(MacAddr(out))
    }

    fn to_bluez(&self) -> String {
        let [a, b, c, d, e, g] = self.0;
        format!("{a:02X}:{b:02X}:{c:02X}:{d:02X}:{e:02X}:{g:02X}")
    }

    fn to_object_path_component(&self) -> String {
        let [a, b, c, d, e, g] = self.0;
        format!("dev_{a:02X}_{b:02X}_{c:02X}_{d:02X}_{e:02X}_{g:02X}")
    }
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_lowercase_colon_separated() {
        let m = MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(format!("{m:?}"), "aa:bb:cc:dd:ee:ff");
    }

    #[test]
    fn bluez_roundtrip() {
        let m = MacAddr::from_bluez("AA:BB:CC:DD:EE:FF").unwrap();
        assert_eq!(m, MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]));
        assert_eq!(m.to_bluez(), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn bluez_lowercase_input_is_accepted() {
        assert_eq!(
            MacAddr::from_bluez("aa:bb:cc:dd:ee:ff").unwrap(),
            MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
        );
    }

    #[test]
    fn object_path_component_uses_uppercase_underscores() {
        let m = MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(m.to_object_path_component(), "dev_AA_BB_CC_DD_EE_FF");
    }

    #[test]
    fn wrong_length_is_rejected() {
        assert_eq!(
            MacAddr::from_bluez(""),
            Err(ParseMacAddrError::WrongLength(0)),
        );
        assert_eq!(
            MacAddr::from_bluez("AA:BB:CC:DD:EE"),
            Err(ParseMacAddrError::WrongLength(14)),
        );
        assert_eq!(
            MacAddr::from_bluez("AA:BB:CC:DD:EE:FF:00"),
            Err(ParseMacAddrError::WrongLength(20)),
        );
    }

    #[test]
    fn invalid_hex_is_rejected() {
        assert_eq!(
            MacAddr::from_bluez("ZZ:BB:CC:DD:EE:FF"),
            Err(ParseMacAddrError::InvalidHex(0)),
        );
        assert_eq!(
            MacAddr::from_bluez("AA:BB:CC:DD:EE:FG"),
            Err(ParseMacAddrError::InvalidHex(16)),
        );
    }

    #[test]
    fn missing_colon_is_rejected() {
        // Same length (17) but a colon replaced with a dash.
        assert_eq!(
            MacAddr::from_bluez("AA-BB:CC:DD:EE:FF"),
            Err(ParseMacAddrError::MissingColon(2)),
        );
        assert_eq!(
            MacAddr::from_bluez("AA:BB:CC:DD:EE.FF"),
            Err(ParseMacAddrError::MissingColon(14)),
        );
    }

    #[test]
    fn serde_roundtrips_canonical_lowercase_form() {
        let m = MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, "\"aa:bb:cc:dd:ee:ff\"");
        let back: MacAddr = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn serde_accepts_uppercase_form() {
        let back: MacAddr = serde_json::from_str("\"AA:BB:CC:DD:EE:FF\"").unwrap();
        assert_eq!(back, MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]));
    }

    #[test]
    fn extra_characters_are_rejected() {
        // Trailing space pushes length to 18.
        assert_eq!(
            MacAddr::from_bluez("AA:BB:CC:DD:EE:FF "),
            Err(ParseMacAddrError::WrongLength(18)),
        );
        // Leading space pushes length to 18.
        assert_eq!(
            MacAddr::from_bluez(" AA:BB:CC:DD:EE:FF"),
            Err(ParseMacAddrError::WrongLength(18)),
        );
    }
}
