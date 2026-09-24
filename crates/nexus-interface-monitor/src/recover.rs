//! Error-handling and recovery machinery for the monitor. See DD-001
//! §§9.1 and 9.4.
//!
//! This module is purposely thin — it provides:
//!
//! 1. [`is_enobufs`] to classify a recv error on a netlink socket.
//! 2. [`compute_registry_diff`] to turn a pre/post-re-enumeration
//!    snapshot into `InterfaceDiscovered` / `InterfaceRemoved` /
//!    carrier/operstate deltas. The monitor emits those events
//!    against its broadcast bus.
//! 3. [`Nl80211RetryTimer`] — a 30 s backoff timer the monitor can
//!    sleep on while nl80211 is degraded.

use std::collections::HashMap;
use std::io;
use std::time::{Duration, Instant};

use nexus_core::{InterfaceInfo, NexusEvent};

/// Classify whether a netlink recv error is `ENOBUFS` — the primary
/// drop signal per DD-001 §9.1.
pub fn is_enobufs(err: &io::Error) -> bool {
    err.raw_os_error() == Some(libc::ENOBUFS)
}

// ---------------------------------------------------------------------------
// Registry diff
// ---------------------------------------------------------------------------

/// One event the caller should emit after a re-enumeration diff.
///
/// Kept as a separate type rather than `NexusEvent` directly so the
/// caller can decide whether to clone the InterfaceInfo into a
/// NexusEvent or use it for other bookkeeping first.
#[derive(Debug, Clone)]
pub enum DiffEvent {
    Added(InterfaceInfo),
    Removed {
        ifindex: u32,
    },
    CarrierChanged {
        ifindex: u32,
        up: bool,
    },
    OperstateChanged {
        ifindex: u32,
        state: nexus_core::OperState,
    },
}

impl DiffEvent {
    /// Convenience conversion to the equivalent NexusEvent the
    /// monitor emits on its broadcast channel.
    pub fn into_nexus_event(self) -> NexusEvent {
        match self {
            DiffEvent::Added(info) => NexusEvent::InterfaceDiscovered(info),
            DiffEvent::Removed { ifindex } => NexusEvent::InterfaceRemoved { ifindex },
            DiffEvent::CarrierChanged { ifindex, up } => NexusEvent::CarrierChanged { ifindex, up },
            DiffEvent::OperstateChanged { ifindex, state } => {
                NexusEvent::OperstateChanged { ifindex, state }
            }
        }
    }
}

/// Diff an "after" snapshot against a "before" map. The caller owns
/// both; this function allocates only the result vector.
///
/// Ordering is deterministic only within each category
/// (Removed → Added → CarrierChanged → OperstateChanged) — tests
/// shouldn't rely on order within a category since HashMap iteration
/// is unordered.
pub fn compute_registry_diff(
    before: &HashMap<u32, InterfaceInfo>,
    after: &HashMap<u32, InterfaceInfo>,
) -> Vec<DiffEvent> {
    let mut out = Vec::new();

    for ifindex in before.keys() {
        if !after.contains_key(ifindex) {
            out.push(DiffEvent::Removed { ifindex: *ifindex });
        }
    }
    for (ifindex, info) in after.iter() {
        match before.get(ifindex) {
            None => out.push(DiffEvent::Added(info.clone())),
            Some(existing) => {
                if existing.carrier != info.carrier {
                    out.push(DiffEvent::CarrierChanged {
                        ifindex: *ifindex,
                        up: info.carrier,
                    });
                }
                if existing.operstate != info.operstate {
                    out.push(DiffEvent::OperstateChanged {
                        ifindex: *ifindex,
                        state: info.operstate,
                    });
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// nl80211 retry timer
// ---------------------------------------------------------------------------

/// Default interval between nl80211 family-resolution retries per
/// DD-001 §9.4.
pub const NL80211_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Low-frequency retry timer for the degraded-nl80211 state. The
/// monitor holds one of these and calls [`Nl80211RetryTimer::poll`]
/// on each tick of its select loop to decide whether to attempt a
/// fresh resolve. When nl80211 comes back (or was never degraded),
/// the timer should be discarded via [`Nl80211RetryTimer::disable`].
#[derive(Debug, Clone)]
pub struct Nl80211RetryTimer {
    interval: Duration,
    /// When the next retry becomes due. `None` means "disabled".
    next: Option<Instant>,
}

impl Default for Nl80211RetryTimer {
    fn default() -> Self {
        Self::new(NL80211_RETRY_INTERVAL)
    }
}

impl Nl80211RetryTimer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next: None,
        }
    }

    /// Arm the timer starting from `now + interval`.
    pub fn arm(&mut self, now: Instant) {
        self.next = Some(now + self.interval);
    }

    /// Turn the timer off. Called when nl80211 has been (re)acquired.
    pub fn disable(&mut self) {
        self.next = None;
    }

    /// The next time this timer wants to fire, or `None` if disabled.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.next
    }

    /// If the timer is due, schedule the subsequent retry and return
    /// `true`. Otherwise return `false`.
    pub fn poll(&mut self, now: Instant) -> bool {
        match self.next {
            Some(deadline) if deadline <= now => {
                self.next = Some(now + self.interval);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use nexus_core::{InterfaceInfo, InterfaceKind, OperState, PhyCapabilities};

    use super::*;

    fn ethernet(ifindex: u32, carrier: bool, operstate: OperState) -> InterfaceInfo {
        InterfaceInfo {
            ifindex,
            ifname: format!("eth{ifindex}"),
            mac: [0; 6],
            mtu: 1500,
            operstate,
            carrier,
            kind: InterfaceKind::Ethernet,
            discovered_at: Instant::now(),
        }
    }

    fn wireless(ifindex: u32) -> InterfaceInfo {
        InterfaceInfo {
            ifindex,
            ifname: format!("wlan{ifindex}"),
            mac: [0; 6],
            mtu: 1500,
            operstate: OperState::Up,
            carrier: true,
            kind: InterfaceKind::Wireless {
                wiphy: 0,
                wiphy_name: "phy0".into(),
                wdev: 1,
                iftype: nexus_core::Nl80211IfType(2),
                capabilities: Arc::new(PhyCapabilities::default()),
            },
            discovered_at: Instant::now(),
        }
    }

    #[test]
    fn enobufs_is_classified_as_drop() {
        let err = io::Error::from_raw_os_error(libc::ENOBUFS);
        assert!(is_enobufs(&err));
    }

    #[test]
    fn non_enobufs_errors_are_ignored() {
        let err = io::Error::from_raw_os_error(libc::EAGAIN);
        assert!(!is_enobufs(&err));
        let err = io::Error::from_raw_os_error(libc::EINVAL);
        assert!(!is_enobufs(&err));
    }

    #[test]
    fn registry_diff_detects_added_removed_and_changed() {
        let before: HashMap<u32, InterfaceInfo> = [
            (2u32, ethernet(2, true, OperState::Up)),
            (3u32, ethernet(3, true, OperState::Up)),
        ]
        .into_iter()
        .collect();
        let after: HashMap<u32, InterfaceInfo> = [
            (2u32, ethernet(2, false, OperState::Up)), // carrier changed
            (4u32, wireless(4)),                       // added
        ]
        .into_iter()
        .collect();

        let diff = compute_registry_diff(&before, &after);
        assert!(
            diff.iter()
                .any(|e| matches!(e, DiffEvent::Removed { ifindex: 3 })),
            "expected Removed(3), got {diff:?}",
        );
        assert!(
            diff.iter()
                .any(|e| matches!(e, DiffEvent::Added(info) if info.ifindex == 4)),
            "expected Added(ifindex=4), got {diff:?}",
        );
        assert!(
            diff.iter().any(|e| matches!(
                e,
                DiffEvent::CarrierChanged {
                    ifindex: 2,
                    up: false,
                }
            )),
            "expected CarrierChanged(2, false), got {diff:?}",
        );
    }

    #[test]
    fn retry_timer_fires_after_interval() {
        let mut t = Nl80211RetryTimer::new(Duration::from_millis(10));
        let now = Instant::now();
        assert!(!t.poll(now)); // disabled by default

        t.arm(now);
        assert!(!t.poll(now));
        assert!(!t.poll(now + Duration::from_millis(5)));
        assert!(t.poll(now + Duration::from_millis(20)));

        // After firing, the timer rearms for the next interval.
        assert!(!t.poll(now + Duration::from_millis(20)));
        assert!(t.poll(now + Duration::from_millis(40)));
    }

    #[test]
    fn retry_timer_disable_stops_firing() {
        let mut t = Nl80211RetryTimer::new(Duration::from_millis(1));
        let now = Instant::now();
        t.arm(now);
        t.disable();
        assert!(!t.poll(now + Duration::from_secs(1)));
        assert!(t.next_deadline().is_none());
    }
}
