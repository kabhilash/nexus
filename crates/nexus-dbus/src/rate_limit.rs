//! Per-sender rate limiting. See DD-006 §15.
//!
//! The limiter is a sliding-window counter per
//! `(unique-bus-name, op-class)` key. Every mutating call asks
//! [`RateLimiter::check`] before doing real work; the limiter
//! returns the remaining headroom (`Ok`) or a
//! `Duration` hint (`Err(retry_after)`).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Operation classes from DD-006 §15. Each class has its own
/// limit; the strictest applicable class wins for a given call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpClass {
    PropertyRead,
    Scan,
    ConnectDisconnect,
    ProfileWrite,
    Admin,
}

impl OpClass {
    pub fn as_str(self) -> &'static str {
        match self {
            OpClass::PropertyRead => "property_read",
            OpClass::Scan => "scan",
            OpClass::ConnectDisconnect => "connect_disconnect",
            OpClass::ProfileWrite => "profile_write",
            OpClass::Admin => "admin",
        }
    }
}

/// Configurable per-class limits. Defaults match DD-006 §15.
#[derive(Debug, Clone, Copy)]
pub struct RateLimits {
    /// Property reads per minute, per sender.
    pub property_read_per_min: u32,
    /// Wi-Fi scans per minute per sender.
    pub scan_per_min: u32,
    /// Connect / disconnect calls per minute per sender.
    pub connect_disconnect_per_min: u32,
    /// Profile add / modify / remove per minute per sender.
    pub profile_write_per_min: u32,
    /// Admin operations per minute per sender (rotate / freeze /
    /// diagnostics — DD-006 §5.2 says ~1 per 60 seconds, i.e. 1/min).
    pub admin_per_min: u32,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            property_read_per_min: 1000,
            scan_per_min: 10,
            connect_disconnect_per_min: 30,
            profile_write_per_min: 30,
            admin_per_min: 1,
        }
    }
}

impl RateLimits {
    pub fn allowed_per_min(&self, op: OpClass) -> u32 {
        match op {
            OpClass::PropertyRead => self.property_read_per_min,
            OpClass::Scan => self.scan_per_min,
            OpClass::ConnectDisconnect => self.connect_disconnect_per_min,
            OpClass::ProfileWrite => self.profile_write_per_min,
            OpClass::Admin => self.admin_per_min,
        }
    }
}

/// Per-(sender, class) sliding-window counter. The window is
/// always 60 s; `allowed` is per-class.
#[derive(Debug, Default)]
pub struct RateLimiter {
    pub limits: RateLimits,
    state: Mutex<HashMap<(String, OpClass), Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new(limits: RateLimits) -> Self {
        Self {
            limits,
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Check + consume in one shot. On `Ok`, the call is recorded
    /// and counts against the sender's window. On `Err(retry_after)`,
    /// the call is *not* counted (DD-006 §15 "rate-limit rejection
    /// does NOT consume a slot"). Callers translate `retry_after`
    /// into `fi.nexus.Error.RateLimited { retry_after_ms }`.
    pub fn check(&self, sender: &str, op: OpClass) -> Result<(), Duration> {
        let now = Instant::now();
        let allowed = self.limits.allowed_per_min(op);
        let mut state = self.state.lock().unwrap();
        let entry = state.entry((sender.to_owned(), op)).or_default();
        // Drop entries older than the 60-s window.
        let window = Duration::from_secs(60);
        entry.retain(|t| now.duration_since(*t) < window);

        if entry.len() as u32 >= allowed {
            // Compute the delay until the oldest entry exits the
            // window, plus a 1 ms cushion so the next attempt is
            // safely past it.
            let oldest = entry.first().copied().unwrap_or(now);
            let wait = window.saturating_sub(now.duration_since(oldest)) + Duration::from_millis(1);
            return Err(wait);
        }
        entry.push(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_under_cap_passes() {
        let limiter = RateLimiter::new(RateLimits::default());
        for _ in 0..5 {
            assert!(limiter.check(":1.42", OpClass::Scan).is_ok());
        }
    }

    #[test]
    fn limiter_over_cap_returns_retry_after() {
        let limiter = RateLimiter::new(RateLimits {
            scan_per_min: 3,
            ..RateLimits::default()
        });
        for _ in 0..3 {
            assert!(limiter.check(":1.7", OpClass::Scan).is_ok());
        }
        let err = limiter.check(":1.7", OpClass::Scan).unwrap_err();
        assert!(err <= Duration::from_secs(60) + Duration::from_millis(1));
        // Subsequent over-limit checks don't consume slots.
        let _ = limiter.check(":1.7", OpClass::Scan);
        let _ = limiter.check(":1.7", OpClass::Scan);
        let again = limiter.check(":1.7", OpClass::Scan);
        assert!(again.is_err());
    }

    #[test]
    fn limiter_separates_senders() {
        let limiter = RateLimiter::new(RateLimits {
            scan_per_min: 1,
            ..RateLimits::default()
        });
        assert!(limiter.check(":1.10", OpClass::Scan).is_ok());
        assert!(limiter.check(":1.11", OpClass::Scan).is_ok());
        assert!(limiter.check(":1.10", OpClass::Scan).is_err());
    }

    #[test]
    fn limiter_separates_op_classes() {
        let limiter = RateLimiter::new(RateLimits {
            scan_per_min: 1,
            connect_disconnect_per_min: 1,
            ..RateLimits::default()
        });
        assert!(limiter.check(":1.4", OpClass::Scan).is_ok());
        assert!(limiter.check(":1.4", OpClass::ConnectDisconnect).is_ok());
        assert!(limiter.check(":1.4", OpClass::Scan).is_err());
        assert!(limiter.check(":1.4", OpClass::ConnectDisconnect).is_err());
    }
}
