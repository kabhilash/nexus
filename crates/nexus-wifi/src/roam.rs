//! Roaming policy. See DD-003 §7.
//!
//! This module is pure — it evaluates whether a roam should fire
//! given the current RSSI and a candidate BSS. The actual
//! `supplicant.roam()` call lives in the backend's event loop.

use nexus_core::MacAddr;

use crate::types::BssInfo;

/// DD-003 §7.3 defaults.
pub const DEFAULT_ROAM_TRIGGER_DBM: i32 = -75;
pub const DEFAULT_ROAM_HYSTERESIS_DB: i32 = 8;

/// Roaming thresholds.
#[derive(Debug, Clone, Copy)]
pub struct RoamPolicy {
    /// If the current RSSI is above this, don't evaluate a roam.
    pub trigger_dbm: i32,
    /// A candidate must beat the current RSSI by at least this
    /// many dB before we actually roam. Avoids oscillation between
    /// two BSSes on marginal signal.
    pub hysteresis_db: i32,
}

impl Default for RoamPolicy {
    fn default() -> Self {
        Self {
            trigger_dbm: DEFAULT_ROAM_TRIGGER_DBM,
            hysteresis_db: DEFAULT_ROAM_HYSTERESIS_DB,
        }
    }
}

/// Pick the best roam target among visible BSSes for a given
/// SSID. Returns `None` if none exceeds the hysteresis threshold
/// over the current BSS's RSSI.
pub fn pick_roam_target(
    policy: RoamPolicy,
    current_bssid: MacAddr,
    current_rssi: i32,
    candidates: &[BssInfo],
) -> Option<MacAddr> {
    if current_rssi > policy.trigger_dbm {
        return None; // signal is fine, don't roam
    }
    candidates
        .iter()
        .filter(|b| b.bssid != current_bssid)
        .filter(|b| b.signal_dbm >= current_rssi + policy.hysteresis_db)
        .max_by_key(|b| b.signal_dbm)
        .map(|b| b.bssid)
}

#[cfg(test)]
mod tests {
    use nexus_core::Ssid;

    use super::*;
    use crate::types::BssCapabilities;

    fn bss(bssid: [u8; 6], signal_dbm: i32) -> BssInfo {
        BssInfo {
            bssid: MacAddr(bssid),
            ssid: Ssid::new(b"x".to_vec()).unwrap(),
            frequency: 2412,
            signal_dbm,
            capabilities: BssCapabilities::default(),
            security: vec![],
            age_ms: 0,
        }
    }

    #[test]
    fn no_roam_when_signal_is_above_trigger() {
        let p = RoamPolicy::default();
        let current = MacAddr([0x01; 6]);
        let bsses = vec![bss([0x02; 6], -40)];
        assert!(pick_roam_target(p, current, -60, &bsses).is_none());
    }

    #[test]
    fn no_roam_when_candidate_does_not_beat_hysteresis() {
        let p = RoamPolicy::default();
        let current = MacAddr([0x01; 6]);
        // current -80, candidate -77: only 3 dB better, below the
        // 8 dB hysteresis.
        let bsses = vec![bss([0x02; 6], -77)];
        assert!(pick_roam_target(p, current, -80, &bsses).is_none());
    }

    #[test]
    fn roams_to_candidate_that_clears_hysteresis() {
        let p = RoamPolicy::default();
        let current = MacAddr([0x01; 6]);
        let bsses = vec![bss([0x02; 6], -65)];
        assert_eq!(
            pick_roam_target(p, current, -80, &bsses),
            Some(MacAddr([0x02; 6])),
        );
    }

    #[test]
    fn picks_strongest_candidate_above_threshold() {
        let p = RoamPolicy::default();
        let current = MacAddr([0x01; 6]);
        let bsses = vec![
            bss([0x02; 6], -65),
            bss([0x03; 6], -50),
            bss([0x04; 6], -70),
        ];
        assert_eq!(
            pick_roam_target(p, current, -80, &bsses),
            Some(MacAddr([0x03; 6])),
        );
    }

    #[test]
    fn excludes_the_current_bssid_from_candidates() {
        let p = RoamPolicy::default();
        let current = MacAddr([0x02; 6]);
        let bsses = vec![bss([0x02; 6], -40)];
        assert!(pick_roam_target(p, current, -80, &bsses).is_none());
    }
}
