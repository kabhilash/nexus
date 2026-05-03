//! Network selection: pick the best `(WifiProfile, BssInfo)` given
//! what's visible in the scan cache. See DD-003 §6.1.

use std::collections::HashSet;

use nexus_core::SecurityMode;
use nexus_profile_store::{SecurityConfig, WifiProfile};
use ulid::Ulid;

use crate::types::BssInfo;

/// Pick the best candidate for auto-connect. `None` means either
/// no profile opts in, or every compatible profile is either
/// blacklisted on the visible BSS, marked `credentials_invalid`,
/// or runtime-paused via
/// `Wifi.Disconnect(pause_auto_connect=true)`.
pub fn select_network(
    profiles: &[WifiProfile],
    visible_bsses: &[BssInfo],
    paused: &HashSet<Ulid>,
) -> Option<(WifiProfile, BssInfo)> {
    let mut candidates: Vec<(WifiProfile, BssInfo)> = Vec::new();

    for profile in profiles {
        if !profile.network.auto_connect {
            continue;
        }
        if profile.network.credentials_invalid {
            continue;
        }
        if paused.contains(&profile.id) {
            continue;
        }
        for bss in visible_bsses {
            if bss.ssid != profile.network.ssid {
                continue;
            }
            if !security_compatible(&profile.network.security, &bss.security) {
                continue;
            }
            if profile.network.bssid_blacklist.contains(&bss.bssid) {
                continue;
            }
            candidates.push((profile.clone(), bss.clone()));
        }
    }

    candidates.sort_by(|(p1, b1), (p2, b2)| {
        use std::cmp::Ordering;

        let pref_match = |p: &WifiProfile, b: &BssInfo| -> bool {
            p.network
                .bssid_preferred
                .as_ref()
                .is_some_and(|pref| pref == &b.bssid)
        };
        // Higher priority first.
        p2.network
            .priority
            .cmp(&p1.network.priority)
            // Preferred-BSSID match first (true > false in our scheme).
            .then_with(|| pref_match(p2, b2).cmp(&pref_match(p1, b1)))
            // Most recent successful connection first; profiles
            // never connected (None) sort last so a known-good
            // network beats a stranger even if the stranger's RSSI
            // is briefly stronger. Two candidates tied here fall
            // through to signal.
            .then_with(|| match (
                p1.network.last_connected_at,
                p2.network.last_connected_at,
            ) {
                (Some(t1), Some(t2)) => t2.cmp(&t1),
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => Ordering::Equal,
            })
            // Stronger signal first.
            .then_with(|| b2.signal_dbm.cmp(&b1.signal_dbm))
    });

    candidates.into_iter().next()
}

/// True if a profile's required security matches something the BSS
/// advertises. See DD-003 §8.3.
///
/// The rule: a profile matches a BSS if any advertised
/// [`SecurityMode`] on the BSS is compatible with the profile's
/// [`SecurityConfig`]. Transition modes (WPA2/WPA3 and
/// Wpa2Wpa3Personal) accept either side.
pub fn security_compatible(profile: &SecurityConfig, bss_modes: &[SecurityMode]) -> bool {
    bss_modes.iter().any(|mode| pair_compatible(profile, mode))
}

// Tabular match is more readable than `matches!` here since the
// compatibility matrix is long; suppress the idiom lint.
#[allow(clippy::match_like_matches_macro)]
fn pair_compatible(profile: &SecurityConfig, bss: &SecurityMode) -> bool {
    match (profile, bss) {
        (SecurityConfig::Open, SecurityMode::Open) => true,
        (SecurityConfig::Owe, SecurityMode::Owe) => true,
        (SecurityConfig::Wpa2Personal { .. }, SecurityMode::Wpa2Psk) => true,
        (SecurityConfig::Wpa2Personal { .. }, SecurityMode::Wpa2Wpa3Transition) => true,
        (SecurityConfig::Wpa3Personal { .. }, SecurityMode::Wpa3Sae) => true,
        (SecurityConfig::Wpa3Personal { .. }, SecurityMode::Wpa2Wpa3Transition) => true,
        (SecurityConfig::Wpa2Wpa3Personal { .. }, SecurityMode::Wpa2Psk) => true,
        (SecurityConfig::Wpa2Wpa3Personal { .. }, SecurityMode::Wpa3Sae) => true,
        (SecurityConfig::Wpa2Wpa3Personal { .. }, SecurityMode::Wpa2Wpa3Transition) => true,
        (SecurityConfig::Wpa2Enterprise(_), SecurityMode::Wpa2Eap) => true,
        (SecurityConfig::Wpa3Enterprise(_), SecurityMode::Wpa3Eap) => true,
        (SecurityConfig::Wpa3Enterprise(_), SecurityMode::Wpa3EapSuiteB192) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use nexus_core::{MacAddr, Ssid};
    use nexus_profile_store::{ProfileMetadata, WifiNetworkSettings, WifiProfile, WpaPsk};
    use ulid::Ulid;

    use super::*;
    use crate::secretstring;
    use crate::types::BssCapabilities;

    fn bss(bssid: [u8; 6], ssid: &[u8], signal_dbm: i32, mode: SecurityMode) -> BssInfo {
        BssInfo {
            bssid: MacAddr(bssid),
            ssid: Ssid::new(ssid.to_vec()).unwrap(),
            frequency: 2412,
            signal_dbm,
            capabilities: BssCapabilities::default(),
            security: vec![mode],
            age_ms: 0,
        }
    }

    fn profile(
        ssid: &[u8],
        security: SecurityConfig,
        priority: i32,
        auto: bool,
        invalid: bool,
    ) -> WifiProfile {
        WifiProfile {
            id: Ulid::new(),
            schema_version: 1,
            metadata: ProfileMetadata::default(),
            network: WifiNetworkSettings {
                ssid: Ssid::new(ssid.to_vec()).unwrap(),
                hidden: false,
                priority,
                auto_connect: auto,
                fast_transition: false,
                security,
                bssid_preferred: None,
                bssid_blacklist: vec![],
                scan_freqs: vec![],
                credentials_invalid: invalid,
                last_connected_at: None,
            },
        }
    }

    fn psk(pw: &str) -> SecurityConfig {
        SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(secretstring(pw)),
        }
    }

    #[test]
    fn picks_highest_priority_when_multiple_match() {
        let a = profile(b"corp", psk("x"), 10, true, false);
        let b = profile(b"corp", psk("y"), 20, true, false);
        let bsses = vec![bss([0xAA; 6], b"corp", -55, SecurityMode::Wpa2Psk)];
        let (picked, _) = select_network(&[a, b.clone()], &bsses, &HashSet::new()).unwrap();
        assert_eq!(picked.id, b.id);
    }

    #[test]
    fn prefers_better_signal_at_equal_priority() {
        let p = profile(b"corp", psk("x"), 10, true, false);
        let bsses = vec![
            bss([0x01; 6], b"corp", -70, SecurityMode::Wpa2Psk),
            bss([0x02; 6], b"corp", -55, SecurityMode::Wpa2Psk),
            bss([0x03; 6], b"corp", -80, SecurityMode::Wpa2Psk),
        ];
        let (_, b) = select_network(&[p], &bsses, &HashSet::new()).unwrap();
        assert_eq!(b.bssid, MacAddr([0x02; 6]));
    }

    #[test]
    fn preferred_bssid_wins_over_signal() {
        let mut p = profile(b"corp", psk("x"), 10, true, false);
        p.network.bssid_preferred = Some(MacAddr([0x01; 6]));
        let bsses = vec![
            bss([0x01; 6], b"corp", -70, SecurityMode::Wpa2Psk),
            bss([0x02; 6], b"corp", -55, SecurityMode::Wpa2Psk),
        ];
        let (_, b) = select_network(&[p], &bsses, &HashSet::new()).unwrap();
        assert_eq!(b.bssid, MacAddr([0x01; 6]));
    }

    #[test]
    fn blacklisted_bssid_is_skipped() {
        let mut p = profile(b"corp", psk("x"), 10, true, false);
        p.network.bssid_blacklist = vec![MacAddr([0x01; 6])];
        let bsses = vec![
            bss([0x01; 6], b"corp", -40, SecurityMode::Wpa2Psk),
            bss([0x02; 6], b"corp", -70, SecurityMode::Wpa2Psk),
        ];
        let (_, b) = select_network(&[p], &bsses, &HashSet::new()).unwrap();
        assert_eq!(b.bssid, MacAddr([0x02; 6]));
    }

    #[test]
    fn auto_connect_off_profile_is_ignored() {
        let p = profile(b"corp", psk("x"), 10, false, false);
        let bsses = vec![bss([0x01; 6], b"corp", -40, SecurityMode::Wpa2Psk)];
        assert!(select_network(&[p], &bsses, &HashSet::new()).is_none());
    }

    #[test]
    fn credentials_invalid_profile_is_ignored() {
        let p = profile(b"corp", psk("x"), 10, true, true);
        let bsses = vec![bss([0x01; 6], b"corp", -40, SecurityMode::Wpa2Psk)];
        assert!(select_network(&[p], &bsses, &HashSet::new()).is_none());
    }

    #[test]
    fn recency_breaks_ties_before_signal() {
        // Two profiles, same priority, neither has a preferred-BSSID
        // hit, both visible. The more-recently-connected one wins
        // even when its RSSI is weaker.
        use chrono::{TimeZone, Utc};
        let mut older = profile(b"home", psk("x"), 10, true, false);
        older.network.last_connected_at = Some(Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
        let mut newer = profile(b"office", psk("y"), 10, true, false);
        newer.network.last_connected_at =
            Some(Utc.with_ymd_and_hms(2026, 4, 25, 12, 0, 0).unwrap());
        let bsses = vec![
            // Older profile's BSS has *stronger* signal — without
            // recency, it would win on the previous sort key.
            bss([0x01; 6], b"home", -45, SecurityMode::Wpa2Psk),
            bss([0x02; 6], b"office", -65, SecurityMode::Wpa2Psk),
        ];
        let (picked, _) = select_network(&[older, newer.clone()], &bsses, &HashSet::new()).unwrap();
        assert_eq!(picked.id, newer.id, "more-recent profile should win the tiebreaker");
    }

    #[test]
    fn never_connected_profile_loses_recency_tiebreaker() {
        // A profile with last_connected_at=Some(t) beats a profile
        // with last_connected_at=None even if the latter has a
        // stronger signal.
        use chrono::{TimeZone, Utc};
        let stranger = profile(b"home", psk("x"), 10, true, false);
        // last_connected_at: None
        let mut known = profile(b"office", psk("y"), 10, true, false);
        known.network.last_connected_at =
            Some(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap());
        let bsses = vec![
            bss([0x01; 6], b"home", -40, SecurityMode::Wpa2Psk),
            bss([0x02; 6], b"office", -75, SecurityMode::Wpa2Psk),
        ];
        let (picked, _) =
            select_network(&[stranger, known.clone()], &bsses, &HashSet::new()).unwrap();
        assert_eq!(picked.id, known.id);
    }

    #[test]
    fn signal_breaks_ties_when_neither_was_ever_connected() {
        // Both profiles have last_connected_at=None — fall through
        // to RSSI.
        let p1 = profile(b"home", psk("x"), 10, true, false);
        let p2 = profile(b"office", psk("y"), 10, true, false);
        let bsses = vec![
            bss([0x01; 6], b"home", -75, SecurityMode::Wpa2Psk),
            bss([0x02; 6], b"office", -40, SecurityMode::Wpa2Psk),
        ];
        let (picked, _) = select_network(&[p1, p2.clone()], &bsses, &HashSet::new()).unwrap();
        assert_eq!(picked.id, p2.id);
    }

    #[test]
    fn paused_profile_is_skipped_in_auto_select() {
        // DD-006 §6.3 Wifi.Disconnect(pause_auto_connect=true):
        // the runtime pause must hide a profile from auto-connect
        // even though `auto_connect = true` in the on-disk shape.
        let p = profile(b"corp", psk("x"), 10, true, false);
        let bsses = vec![bss([0x01; 6], b"corp", -40, SecurityMode::Wpa2Psk)];
        let mut paused = HashSet::new();
        paused.insert(p.id);
        assert!(select_network(std::slice::from_ref(&p), &bsses, &paused).is_none());
        // And without the pause, the same profile is selected.
        assert!(select_network(&[p], &bsses, &HashSet::new()).is_some());
    }

    #[test]
    fn security_mismatch_skips_candidate() {
        // Profile wants WPA2-PSK; only an Open BSS is visible.
        let p = profile(b"corp", psk("x"), 10, true, false);
        let bsses = vec![bss([0x01; 6], b"corp", -40, SecurityMode::Open)];
        assert!(select_network(&[p], &bsses, &HashSet::new()).is_none());
    }

    #[test]
    fn transition_mode_bss_matches_wpa2_or_wpa3_profile() {
        let p2 = profile(b"corp", psk("x"), 10, true, false);
        let p3 = profile(
            b"corp",
            SecurityConfig::Wpa3Personal {
                passphrase: secretstring("x"),
            },
            11,
            true,
            false,
        );
        let bsses = vec![bss(
            [0x01; 6],
            b"corp",
            -40,
            SecurityMode::Wpa2Wpa3Transition,
        )];
        let (picked2, _) = select_network(std::slice::from_ref(&p2), &bsses, &HashSet::new()).unwrap();
        assert_eq!(picked2.id, p2.id);
        let (picked3, _) = select_network(std::slice::from_ref(&p3), &bsses, &HashSet::new()).unwrap();
        assert_eq!(picked3.id, p3.id);
    }
}
