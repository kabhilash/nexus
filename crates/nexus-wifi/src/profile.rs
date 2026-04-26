//! Wi-Fi profile helpers. See DD-003 §6.1.
//!
//! The canonical on-disk profile lives in nexus-profile-store
//! (`WifiProfile`); this module wraps its credential-bearing form
//! as the backend's in-memory view and renders a
//! [`crate::types::NetworkConfig`] when it's time to hand one to
//! the supplicant.

use nexus_core::Ssid;
use nexus_profile_store::{WifiProfile, ssid_hash};

use crate::types::NetworkConfig;

/// Compute the on-disk filename stub for this profile's SSID.
/// Kept here so callers that already have a `WifiProfile` don't
/// need to reach into nexus-profile-store.
pub fn profile_key(profile: &WifiProfile) -> String {
    ssid_hash(&profile.network.ssid)
}

/// Filename-key version of [`profile_key`] that takes a raw SSID —
/// used by match paths that start from a `BssInfo`.
pub fn ssid_key(ssid: &Ssid) -> String {
    ssid_hash(ssid)
}

/// Translate a `WifiProfile` (in-memory, post-decrypt) into the
/// supplicant-facing shape per DD-003 §6.1.
pub fn to_network_config(profile: &WifiProfile) -> NetworkConfig {
    NetworkConfig {
        ssid: profile.network.ssid.clone(),
        hidden: profile.network.hidden,
        security: profile.network.security.clone(),
        priority: profile.network.priority,
        bssid_preferred: profile.network.bssid_preferred,
        bssid_blacklist: profile.network.bssid_blacklist.clone(),
        scan_freqs: profile.network.scan_freqs.clone(),
        fast_transition: profile.network.fast_transition,
    }
}

#[cfg(test)]
mod tests {
    use nexus_profile_store::{
        ProfileMetadata, SecurityConfig, WifiNetworkSettings, WifiProfile, WpaPsk,
    };
    use ulid::Ulid;

    use super::*;
    use crate::secretstring;

    fn profile(ssid: &[u8], priority: i32) -> WifiProfile {
        WifiProfile {
            id: Ulid::new(),
            schema_version: 1,
            metadata: ProfileMetadata::default(),
            network: WifiNetworkSettings {
                ssid: Ssid::new(ssid.to_vec()).unwrap(),
                hidden: false,
                priority,
                auto_connect: true,
                fast_transition: false,
                security: SecurityConfig::Wpa2Personal {
                    psk: WpaPsk::Passphrase(secretstring("correct horse")),
                },
                bssid_preferred: None,
                bssid_blacklist: vec![],
                scan_freqs: vec![2412, 5180],
                credentials_invalid: false,
                last_connected_at: None,
            },
        }
    }

    #[test]
    fn profile_key_matches_ssid_hash() {
        let p = profile(b"net-a", 10);
        let key = profile_key(&p);
        assert_eq!(key, ssid_key(&p.network.ssid));
        assert_eq!(key.len(), 16);
    }

    #[test]
    fn to_network_config_copies_every_field() {
        let p = profile(b"net-b", 5);
        let cfg = to_network_config(&p);
        assert_eq!(cfg.ssid, p.network.ssid);
        assert_eq!(cfg.priority, 5);
        assert_eq!(cfg.scan_freqs, vec![2412, 5180]);
        assert!(!cfg.hidden);
    }
}
