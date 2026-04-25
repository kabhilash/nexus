//! Scan scheduling + BSS cache. See DD-003 §§5.3, 5.4.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nexus_core::MacAddr;

use crate::power::PowerState;
use crate::types::BssInfo;

/// Adaptive scan scheduler. Driven by the backend's main loop
/// through `next_scan_at` for the select! sleep arm and
/// `on_scan_complete` to advance after a scan finishes.
#[derive(Debug, Clone)]
pub struct ScanScheduler {
    pub base_interval: Duration,
    pub max_interval: Duration,
    pub current_interval: Duration,
    pub next_scan: Instant,
    pub consecutive_empty: u32,
}

impl ScanScheduler {
    pub fn new(base_interval: Duration, max_interval: Duration) -> Self {
        Self {
            base_interval,
            max_interval,
            current_interval: base_interval,
            next_scan: Instant::now() + base_interval,
            consecutive_empty: 0,
        }
    }

    /// DD-003 §5.4 defaults: 60 s base, 10 min max.
    pub fn with_defaults() -> Self {
        Self::new(Duration::from_secs(60), Duration::from_secs(600))
    }

    /// Call after a scan completes. `matched_profile` is true when
    /// the scan results led to a selected profile (connection
    /// attempt or steady-state roam evaluation).
    pub fn on_scan_complete(&mut self, matched_profile: bool, now: Instant) {
        if matched_profile {
            self.current_interval = self.base_interval;
            self.consecutive_empty = 0;
        } else {
            self.consecutive_empty = self.consecutive_empty.saturating_add(1);
            self.current_interval = (self.current_interval * 2).min(self.max_interval);
        }
        self.next_scan = now + self.current_interval;
    }

    /// Force the scheduler to fire ASAP. Used after a wake-from-sleep
    /// (DD-003 §13.3) and when a fresh scan is requested out-of-band.
    pub fn fire_now(&mut self, now: Instant) {
        self.next_scan = now;
    }

    /// Effective interval under the given power state. `None` means
    /// "no scheduled scans" (Sleep).
    pub fn effective_interval(&self, power_state: PowerState) -> Option<Duration> {
        match power_state {
            PowerState::Active => Some(self.current_interval),
            PowerState::Background => Some(self.current_interval * 2),
            PowerState::Sleep => None,
        }
    }

    /// Earliest `Instant` at which the next scheduled scan is due.
    /// `None` means scheduled scanning is paused.
    pub fn next_scan_at(&self, power_state: PowerState) -> Option<Instant> {
        match power_state {
            PowerState::Sleep => None,
            PowerState::Active => Some(self.next_scan),
            // Background effectively doubles the interval, so
            // report a deadline one extra interval past the base.
            PowerState::Background => Some(self.next_scan + self.current_interval),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-interface BSS cache, keyed by BSSID. Mirrors what the
// supplicant holds between scans (DD-003 §5.3).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct BssCache {
    per_interface: HashMap<u32, HashMap<MacAddr, BssInfo>>,
}

impl BssCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn replace(&mut self, ifindex: u32, bsses: Vec<BssInfo>) {
        let mut map = HashMap::new();
        for bss in bsses {
            map.insert(bss.bssid, bss);
        }
        self.per_interface.insert(ifindex, map);
    }

    pub fn clear(&mut self, ifindex: u32) {
        self.per_interface.remove(&ifindex);
    }

    pub fn list(&self, ifindex: u32) -> Vec<BssInfo> {
        self.per_interface
            .get(&ifindex)
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    /// Look up a single BSS by its `(ifindex, bssid)` key. Used by
    /// the state machine to enrich `WifiState::Connected` with the
    /// BSS's advertised security modes (DD-003 §3.1).
    pub fn lookup(&self, ifindex: u32, bssid: MacAddr) -> Option<BssInfo> {
        self.per_interface
            .get(&ifindex)
            .and_then(|m| m.get(&bssid))
            .cloned()
    }

    pub fn len(&self, ifindex: u32) -> usize {
        self.per_interface
            .get(&ifindex)
            .map(|m| m.len())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use nexus_core::Ssid;

    use super::*;
    use crate::types::BssCapabilities;

    fn sample_bss(bssid: [u8; 6], ssid: &[u8]) -> BssInfo {
        BssInfo {
            bssid: MacAddr(bssid),
            ssid: Ssid::new(ssid.to_vec()).unwrap(),
            frequency: 2412,
            signal_dbm: -50,
            capabilities: BssCapabilities::default(),
            security: vec![],
            age_ms: 0,
        }
    }

    #[test]
    fn interval_doubles_on_empty_scan_and_resets_on_match() {
        let mut s = ScanScheduler::new(Duration::from_secs(10), Duration::from_secs(80));
        let now = Instant::now();
        s.on_scan_complete(false, now);
        assert_eq!(s.current_interval, Duration::from_secs(20));
        s.on_scan_complete(false, now);
        assert_eq!(s.current_interval, Duration::from_secs(40));
        s.on_scan_complete(false, now);
        assert_eq!(s.current_interval, Duration::from_secs(80));
        // Capped.
        s.on_scan_complete(false, now);
        assert_eq!(s.current_interval, Duration::from_secs(80));
        // Match resets.
        s.on_scan_complete(true, now);
        assert_eq!(s.current_interval, Duration::from_secs(10));
        assert_eq!(s.consecutive_empty, 0);
    }

    #[test]
    fn sleep_pauses_scheduled_scans() {
        let s = ScanScheduler::with_defaults();
        assert!(s.next_scan_at(PowerState::Sleep).is_none());
        assert!(s.effective_interval(PowerState::Sleep).is_none());
        assert!(s.next_scan_at(PowerState::Active).is_some());
        assert!(s.effective_interval(PowerState::Active).is_some());
    }

    #[test]
    fn background_state_doubles_effective_interval() {
        let s = ScanScheduler::new(Duration::from_secs(10), Duration::from_secs(80));
        assert_eq!(
            s.effective_interval(PowerState::Active),
            Some(Duration::from_secs(10)),
        );
        assert_eq!(
            s.effective_interval(PowerState::Background),
            Some(Duration::from_secs(20)),
        );
    }

    #[test]
    fn bss_cache_replace_overwrites_per_interface() {
        let mut cache = BssCache::new();
        cache.replace(
            2,
            vec![sample_bss([0x01; 6], b"a"), sample_bss([0x02; 6], b"b")],
        );
        assert_eq!(cache.len(2), 2);
        cache.replace(2, vec![sample_bss([0x03; 6], b"c")]);
        assert_eq!(cache.len(2), 1);
        assert_eq!(cache.list(2)[0].bssid, MacAddr([0x03; 6]));
    }

    #[test]
    fn bss_cache_clear_removes_interface_entries() {
        let mut cache = BssCache::new();
        cache.replace(2, vec![sample_bss([0x01; 6], b"a")]);
        cache.clear(2);
        assert!(cache.list(2).is_empty());
    }

    #[test]
    fn fire_now_resets_deadline_to_now() {
        let mut s = ScanScheduler::with_defaults();
        let t = Instant::now() - Duration::from_secs(5);
        s.fire_now(t);
        assert_eq!(s.next_scan, t);
    }
}
