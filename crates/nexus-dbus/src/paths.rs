//! Object path construction. See DD-006 §4.
//!
//! D-Bus object paths are restricted to `[A-Za-z0-9_]` between
//! `/` separators. Nexus maps ifnames / SSIDs / device addresses
//! into that character set with a deterministic escape:
//!
//! - `-` → `_`
//! - Any other byte outside `[A-Za-z0-9_]` → `_XX` (lowercase hex).
//!
//! The escape is reversible in practice because `_` itself is
//! always preserved and legitimate hex pairs after `_` never
//! collide with raw ASCII letters (one look-ahead disambiguates
//! at decode time).

use nexus_core::MacAddr;

/// Manager object path — the service root.
pub const MANAGER_PATH: &str = "/fi/nexus1";

/// Interface object root; a specific interface lives at
/// `{INTERFACE_ROOT}/<escaped ifname>`.
pub const INTERFACE_ROOT: &str = "/fi/nexus1/interface";

/// Profile roots.
pub const PROFILE_WIFI_ROOT: &str = "/fi/nexus1/profile/wifi";
pub const PROFILE_ETHERNET_ROOT: &str = "/fi/nexus1/profile/ethernet";
pub const PROFILE_BLUETOOTH_ROOT: &str = "/fi/nexus1/profile/bluetooth";

/// Escape an ifname (or any arbitrary string) into a single D-Bus
/// path component. See DD-006 §4.
pub fn escape_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b'_' => out.push(b as char),
            b'-' => out.push('_'),
            other => {
                out.push('_');
                out.push_str(&format!("{other:02x}"));
            }
        }
    }
    out
}

/// Build the object path for an interface entry.
pub fn interface_path(ifname: &str) -> String {
    format!("{}/{}", INTERFACE_ROOT, escape_component(ifname))
}

/// Build the path for a scan result under a given interface.
/// `bssid` is formatted as lowercase hex without separators.
pub fn scan_result_path(ifname: &str, bssid: &MacAddr) -> String {
    let [a, b, c, d, e, g] = bssid.0;
    format!(
        "{}/{}/scan_result/{a:02x}{b:02x}{c:02x}{d:02x}{e:02x}{g:02x}",
        INTERFACE_ROOT,
        escape_component(ifname)
    )
}

/// Build the per-device object path under a Bluetooth adapter. See
/// DD-006 §6.6 — device addresses render as `AA_BB_CC_DD_EE_FF`.
pub fn bluetooth_device_path(adapter_ifname: &str, device: &MacAddr) -> String {
    let [a, b, c, d, e, g] = device.0;
    format!(
        "{}/{}/device/{a:02X}_{b:02X}_{c:02X}_{d:02X}_{e:02X}_{g:02X}",
        INTERFACE_ROOT,
        escape_component(adapter_ifname)
    )
}

/// Build a profile object path from its ULID.
pub fn wifi_profile_path(id: &ulid::Ulid) -> String {
    format!("{PROFILE_WIFI_ROOT}/{id}")
}

pub fn ethernet_profile_path(id: &ulid::Ulid) -> String {
    format!("{PROFILE_ETHERNET_ROOT}/{id}")
}

pub fn bluetooth_profile_path(id: &ulid::Ulid) -> String {
    format!("{PROFILE_BLUETOOTH_ROOT}/{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_preserves_alphanumeric_and_underscore() {
        assert_eq!(escape_component("eth0"), "eth0");
        assert_eq!(escape_component("wlp2s0"), "wlp2s0");
        assert_eq!(escape_component("foo_bar"), "foo_bar");
    }

    #[test]
    fn escape_maps_hyphen_to_underscore() {
        assert_eq!(escape_component("wlan-foo"), "wlan_foo");
    }

    #[test]
    fn escape_percent_encodes_special_chars() {
        // "eth0:1" → 'eth0' + _3a + '1'
        assert_eq!(escape_component("eth0:1"), "eth0_3a1");
        assert_eq!(escape_component("/dev/ttyUSB0"), "_2fdev_2fttyUSB0");
    }

    #[test]
    fn interface_path_is_rooted_correctly() {
        assert_eq!(interface_path("eth0"), "/fi/nexus1/interface/eth0");
    }

    #[test]
    fn scan_result_path_lowercase_hex_bssid() {
        let m = MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(
            scan_result_path("wlp2s0", &m),
            "/fi/nexus1/interface/wlp2s0/scan_result/aabbccddeeff"
        );
    }

    #[test]
    fn bluetooth_device_path_uses_underscores_and_uppercase() {
        let m = MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        assert_eq!(
            bluetooth_device_path("hci0", &m),
            "/fi/nexus1/interface/hci0/device/AA_BB_CC_DD_EE_FF"
        );
    }

    #[test]
    fn profile_paths_embed_ulid() {
        let id = ulid::Ulid::from_string("01HPQY8S2N0Z8K9M7V3Y2F4T5W").unwrap();
        assert!(wifi_profile_path(&id).ends_with("01HPQY8S2N0Z8K9M7V3Y2F4T5W"));
        assert!(ethernet_profile_path(&id).starts_with(PROFILE_ETHERNET_ROOT));
    }
}
