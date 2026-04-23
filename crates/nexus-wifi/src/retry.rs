//! BSSID blacklist + per-profile credentials-invalid tracking.
//! See DD-003 §§6.3, 12.3.
//!
//! This module is pure — no I/O, no async. The backend holds a
//! [`RetryBook`] per crate, mutates it from connection-flow event
//! handlers, and reads it during selection.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nexus_core::MacAddr;
use ulid::Ulid;

/// DD-003 §§6.3, 12.3 defaults.
pub const DEFAULT_BSSID_BACKOFF: Duration = Duration::from_secs(60);
pub const DEFAULT_MAX_BSSID_FAILURES: u32 = 3;
pub const DEFAULT_CONNECT_RATE_LIMIT: Duration = Duration::from_secs(2);

/// Per-BSSID retry / blacklist bookkeeping plus a per-profile
/// credentials-invalid set.
#[derive(Debug, Clone, Default)]
pub struct RetryBook {
    /// `(ifindex, bssid)` → (failure count, blacklisted-until).
    bssid: HashMap<(u32, MacAddr), BssidRecord>,
    /// Profile ULIDs the operator needs to refresh before Nexus
    /// retries.
    credentials_invalid: HashMap<Ulid, &'static str>,
    /// Last connect attempt per ifindex (for the 2 s rate limit).
    last_attempt: HashMap<u32, Instant>,
    pub max_failures: u32,
    pub backoff: Duration,
    pub rate_limit: Duration,
}

#[derive(Debug, Clone, Copy)]
struct BssidRecord {
    failures: u32,
    blacklisted_until: Option<Instant>,
}

impl RetryBook {
    pub fn new() -> Self {
        Self {
            max_failures: DEFAULT_MAX_BSSID_FAILURES,
            backoff: DEFAULT_BSSID_BACKOFF,
            rate_limit: DEFAULT_CONNECT_RATE_LIMIT,
            ..Self::default()
        }
    }

    /// Record a failed connection against `(ifindex, bssid)`. If
    /// the failure count hits `max_failures`, start the blacklist
    /// window. Returns `true` if the BSSID is now blacklisted.
    pub fn record_failure(&mut self, ifindex: u32, bssid: MacAddr, now: Instant) -> bool {
        let entry = self.bssid.entry((ifindex, bssid)).or_insert(BssidRecord {
            failures: 0,
            blacklisted_until: None,
        });
        entry.failures = entry.failures.saturating_add(1);
        if entry.failures >= self.max_failures {
            entry.blacklisted_until = Some(now + self.backoff);
            true
        } else {
            false
        }
    }

    /// Drop any failure history for the BSSID (called on
    /// successful connect per §6.3's intent).
    pub fn record_success(&mut self, ifindex: u32, bssid: MacAddr) {
        self.bssid.remove(&(ifindex, bssid));
    }

    /// True if the BSSID is currently in blacklist cooldown.
    pub fn is_blacklisted(&self, ifindex: u32, bssid: MacAddr, now: Instant) -> bool {
        match self.bssid.get(&(ifindex, bssid)) {
            Some(r) => match r.blacklisted_until {
                Some(until) => until > now,
                None => false,
            },
            None => false,
        }
    }

    /// Snapshot of currently-blacklisted BSSIDs for a given
    /// interface. Used by the metric gauge.
    pub fn blacklisted_count(&self, ifindex: u32, now: Instant) -> u32 {
        self.bssid
            .iter()
            .filter(|((ifi, _), r)| *ifi == ifindex && r.blacklisted_until.is_some_and(|u| u > now))
            .count() as u32
    }

    /// Mark a profile as having invalid credentials. Idempotent.
    pub fn mark_credentials_invalid(&mut self, profile: Ulid, reason: &'static str) {
        self.credentials_invalid.insert(profile, reason);
    }

    /// Clear the credentials-invalid flag. Called after a profile
    /// put through D-Bus (the operator updated the password).
    pub fn clear_credentials_invalid(&mut self, profile: Ulid) {
        self.credentials_invalid.remove(&profile);
    }

    pub fn credentials_invalid(&self, profile: Ulid) -> bool {
        self.credentials_invalid.contains_key(&profile)
    }

    pub fn credentials_invalid_count(&self) -> u32 {
        self.credentials_invalid.len() as u32
    }

    /// Enforce the per-interface connect rate limit: returns true
    /// when a new `connect` is allowed.
    pub fn rate_limit_allows(&mut self, ifindex: u32, now: Instant) -> bool {
        match self.last_attempt.get(&ifindex) {
            Some(t) if now.duration_since(*t) < self.rate_limit => false,
            _ => {
                self.last_attempt.insert(ifindex, now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mac(b: u8) -> MacAddr {
        MacAddr([b; 6])
    }

    #[test]
    fn bssid_blacklists_after_three_failures() {
        let mut r = RetryBook::new();
        let now = Instant::now();
        assert!(!r.record_failure(2, mac(1), now));
        assert!(!r.record_failure(2, mac(1), now));
        assert!(r.record_failure(2, mac(1), now));
        assert!(r.is_blacklisted(2, mac(1), now));
        assert!(!r.is_blacklisted(2, mac(2), now));
    }

    #[test]
    fn blacklist_expires_after_backoff_window() {
        let mut r = RetryBook::new();
        r.backoff = Duration::from_millis(50);
        r.max_failures = 1;
        let now = Instant::now();
        r.record_failure(2, mac(1), now);
        assert!(r.is_blacklisted(2, mac(1), now));
        assert!(!r.is_blacklisted(2, mac(1), now + Duration::from_millis(100)));
    }

    #[test]
    fn successful_connect_clears_failure_count() {
        let mut r = RetryBook::new();
        let now = Instant::now();
        r.record_failure(2, mac(1), now);
        r.record_failure(2, mac(1), now);
        r.record_success(2, mac(1));
        assert!(!r.is_blacklisted(2, mac(1), now));
    }

    #[test]
    fn credentials_invalid_is_idempotent() {
        let mut r = RetryBook::new();
        let p = Ulid::new();
        r.mark_credentials_invalid(p, "bad psk");
        r.mark_credentials_invalid(p, "still bad");
        assert!(r.credentials_invalid(p));
        assert_eq!(r.credentials_invalid_count(), 1);
        r.clear_credentials_invalid(p);
        assert!(!r.credentials_invalid(p));
    }

    #[test]
    fn rate_limit_blocks_second_connect_within_two_seconds() {
        let mut r = RetryBook::new();
        let now = Instant::now();
        assert!(r.rate_limit_allows(2, now));
        assert!(!r.rate_limit_allows(2, now + Duration::from_millis(500)));
        assert!(r.rate_limit_allows(2, now + Duration::from_secs(3)));
    }

    #[test]
    fn blacklisted_count_counts_active_entries() {
        let mut r = RetryBook::new();
        let now = Instant::now();
        for _ in 0..r.max_failures {
            r.record_failure(2, mac(1), now);
            r.record_failure(2, mac(2), now);
        }
        assert_eq!(r.blacklisted_count(2, now), 2);
    }
}
