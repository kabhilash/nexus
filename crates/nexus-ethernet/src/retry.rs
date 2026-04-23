//! Exponential-backoff retry policy for auth failures. See DD-002
//! §§6 (config surface), 9.3 (fail-fast vs retry classification).
//!
//! This module is pure — no I/O, no async. The backend's event loop
//! calls into it when an `AuthState::Failed` arrives, gets back
//! either `None` (fail-fast; give up until operator intervention) or
//! `Some(next_at)` (sleep-until deadline the loop can wait on).

use std::time::{Duration, Instant};

use nexus_core::AuthFailureReason;

/// Retry knobs. Mirrors the `[ethernet]` TOML block in DD-002 §8.1.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub initial: Duration,
    pub max: Duration,
    pub multiplier: f64,
    /// `0` means unlimited retries.
    pub max_attempts: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(1000),
            max: Duration::from_secs(60),
            multiplier: 2.0,
            max_attempts: 0,
        }
    }
}

impl RetryPolicy {
    /// Compute the next retry deadline given the previous failure
    /// reason and how many attempts have already happened. Returns
    /// `None` to mean "don't retry" — either the reason is
    /// fail-fast per DD-002 §9.3 or `max_attempts` is exhausted.
    pub fn next_attempt(
        &self,
        reason: &AuthFailureReason,
        attempts: u32,
        now: Instant,
    ) -> Option<Instant> {
        if !is_retriable(reason) {
            return None;
        }
        if self.max_attempts != 0 && attempts >= self.max_attempts {
            return None;
        }
        Some(now + self.backoff_for(attempts))
    }

    /// Raw backoff duration before the `attempts`-th retry (0-based:
    /// `attempts == 0` is the first retry). Exposed so tests can
    /// assert the curve directly.
    pub fn backoff_for(&self, attempts: u32) -> Duration {
        let base = self.initial.as_secs_f64();
        let scaled = base * self.multiplier.powi(attempts as i32);
        let max = self.max.as_secs_f64();
        let capped = scaled.min(max).max(base);
        Duration::from_secs_f64(capped)
    }
}

/// DD-002 §9.3 classification. `BadCredentials` and
/// `CertificateRejected` are fail-fast — retrying won't help until
/// the operator fixes the profile. Everything else is retriable.
pub fn is_retriable(reason: &AuthFailureReason) -> bool {
    !matches!(
        reason,
        AuthFailureReason::BadCredentials | AuthFailureReason::CertificateRejected,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let policy = RetryPolicy {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(8),
            multiplier: 2.0,
            max_attempts: 0,
        };
        assert_eq!(policy.backoff_for(0), Duration::from_secs(1));
        assert_eq!(policy.backoff_for(1), Duration::from_secs(2));
        assert_eq!(policy.backoff_for(2), Duration::from_secs(4));
        assert_eq!(policy.backoff_for(3), Duration::from_secs(8));
        // Past the cap — stays at the cap.
        assert_eq!(policy.backoff_for(4), Duration::from_secs(8));
        assert_eq!(policy.backoff_for(10), Duration::from_secs(8));
    }

    #[test]
    fn bad_credentials_is_fail_fast() {
        assert!(!is_retriable(&AuthFailureReason::BadCredentials));
        assert!(!is_retriable(&AuthFailureReason::CertificateRejected));
        assert!(is_retriable(&AuthFailureReason::ServerUnreachable));
        assert!(is_retriable(&AuthFailureReason::Timeout));
        assert!(is_retriable(&AuthFailureReason::Other("x".into())));
    }

    #[test]
    fn next_attempt_returns_none_for_fail_fast() {
        let policy = RetryPolicy::default();
        let now = Instant::now();
        assert!(
            policy
                .next_attempt(&AuthFailureReason::BadCredentials, 0, now)
                .is_none(),
        );
        assert!(
            policy
                .next_attempt(&AuthFailureReason::CertificateRejected, 0, now)
                .is_none(),
        );
    }

    #[test]
    fn next_attempt_honors_max_attempts() {
        let policy = RetryPolicy {
            max_attempts: 3,
            ..RetryPolicy::default()
        };
        let now = Instant::now();
        assert!(
            policy
                .next_attempt(&AuthFailureReason::Timeout, 0, now)
                .is_some(),
        );
        assert!(
            policy
                .next_attempt(&AuthFailureReason::Timeout, 2, now)
                .is_some(),
        );
        assert!(
            policy
                .next_attempt(&AuthFailureReason::Timeout, 3, now)
                .is_none(),
            "attempts >= max_attempts must stop retrying",
        );
    }

    #[test]
    fn max_attempts_zero_means_unlimited() {
        let policy = RetryPolicy {
            max_attempts: 0,
            ..RetryPolicy::default()
        };
        let now = Instant::now();
        assert!(
            policy
                .next_attempt(&AuthFailureReason::Timeout, 1_000_000, now)
                .is_some(),
        );
    }
}
