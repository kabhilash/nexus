//! Classification state machine for network interfaces. See
//! DD-001 §7.
//!
//! This module is pure — it holds no sockets and performs no I/O.
//! The monitor drives it by:
//!
//! 1. Calling [`ClassifyTracker::start`] when an unknown ifindex
//!    shows up on rtnetlink (+ issuing a unicast
//!    `NL80211_CMD_GET_INTERFACE` probe with the returned seq).
//! 2. Feeding probe responses into
//!    [`ClassifyTracker::resolve_probe_success`] or
//!    [`ClassifyTracker::resolve_probe_error`].
//! 3. Feeding `NL80211_CMD_NEW_INTERFACE` multicast messages into
//!    [`ClassifyTracker::resolve_via_multicast`].
//! 4. Advancing time with [`ClassifyTracker::sweep_timeouts`] and
//!    pulling the next wake-up time with
//!    [`ClassifyTracker::next_deadline`].

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::enumerate::Nl80211InterfaceInfo;
use crate::netlink::rtnl::LinkMessage;

/// Default classify timeout from DD-001 §7 step 3.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// Outcome the tracker hands back when a classification resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassifyOutcome {
    /// Classified as Ethernet (nl80211 returned `ENODEV` / `ENOENT`
    /// / `EOPNOTSUPP`, or the timeout fired).
    Ethernet(LinkMessage),
    /// Classified as Wireless with the nl80211 interface payload.
    Wireless {
        link: LinkMessage,
        nl80211: Nl80211InterfaceInfo,
    },
}

impl ClassifyOutcome {
    pub fn link(&self) -> &LinkMessage {
        match self {
            ClassifyOutcome::Ethernet(l) => l,
            ClassifyOutcome::Wireless { link, .. } => link,
        }
    }

    pub fn is_wireless(&self) -> bool {
        matches!(self, ClassifyOutcome::Wireless { .. })
    }
}

/// One pending classification. The monitor builds a `ClassifyTracker`
/// entry for every new ifindex that enters the `Classifying` state.
#[derive(Debug, Clone)]
pub struct Pending {
    pub link: LinkMessage,
    pub started_at: Instant,
    pub deadline: Instant,
    /// The `nlmsg_seq` used for the unicast
    /// `NL80211_CMD_GET_INTERFACE` probe. `None` when nl80211 is
    /// unavailable — in that case the tracker defaults to
    /// `Ethernet` immediately rather than ever expecting a response.
    pub probe_seq: Option<u32>,
}

/// Pending-classification tracker.
#[derive(Debug)]
pub struct ClassifyTracker {
    timeout: Duration,
    pending: HashMap<u32, Pending>,
    by_probe_seq: HashMap<u32, u32>,
}

impl Default for ClassifyTracker {
    fn default() -> Self {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }
}

impl ClassifyTracker {
    /// Construct with the default 2 s classify timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct with a custom timeout (used by tests).
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout,
            pending: HashMap::new(),
            by_probe_seq: HashMap::new(),
        }
    }

    /// True when no interfaces are currently classifying.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Begin a new classification. Replaces any previous entry for
    /// the same ifindex (shouldn't happen — the monitor only enters
    /// `Classifying` for unknown ifindices — but we're defensive).
    pub fn start(&mut self, link: LinkMessage, probe_seq: Option<u32>, now: Instant) {
        let ifindex = link.header.index as u32;
        // Clear any stale index mapping so the new entry wins.
        self.pending.remove(&ifindex);
        self.by_probe_seq.retain(|_, ifi| *ifi != ifindex);

        let pending = Pending {
            link,
            started_at: now,
            deadline: now + self.timeout,
            probe_seq,
        };
        if let Some(seq) = probe_seq {
            self.by_probe_seq.insert(seq, ifindex);
        }
        self.pending.insert(ifindex, pending);
    }

    /// The monitor's nl80211 unicast probe returned a parsed
    /// `NL80211_CMD_NEW_INTERFACE` payload. Resolve as Wireless.
    pub fn resolve_probe_success(
        &mut self,
        seq: u32,
        info: Nl80211InterfaceInfo,
    ) -> Option<ClassifyOutcome> {
        let ifindex = self.by_probe_seq.remove(&seq)?;
        let pending = self.pending.remove(&ifindex)?;
        Some(ClassifyOutcome::Wireless {
            link: pending.link,
            nl80211: info,
        })
    }

    /// The monitor's nl80211 unicast probe returned an
    /// `NLMSG_ERROR`. Per DD-001 §7, any error (ENODEV, ENOENT,
    /// EOPNOTSUPP, …) resolves the interface as Ethernet.
    pub fn resolve_probe_error(&mut self, seq: u32, _errno: i32) -> Option<ClassifyOutcome> {
        let ifindex = self.by_probe_seq.remove(&seq)?;
        let pending = self.pending.remove(&ifindex)?;
        Some(ClassifyOutcome::Ethernet(pending.link))
    }

    /// A multicast `NL80211_CMD_NEW_INTERFACE` arrived. If its
    /// `NL80211_ATTR_IFINDEX` matches a pending classification,
    /// resolve as Wireless and cancel the unicast probe's handler.
    pub fn resolve_via_multicast(&mut self, info: Nl80211InterfaceInfo) -> Option<ClassifyOutcome> {
        let ifindex = info.ifindex;
        let pending = self.pending.remove(&ifindex)?;
        if let Some(seq) = pending.probe_seq {
            self.by_probe_seq.remove(&seq);
        }
        Some(ClassifyOutcome::Wireless {
            link: pending.link,
            nl80211: info,
        })
    }

    /// Return (and drop) every pending entry whose deadline has
    /// passed. These default to `Ethernet` per DD-001 §7 step 3.
    pub fn sweep_timeouts(&mut self, now: Instant) -> Vec<ClassifyOutcome> {
        let expired: Vec<u32> = self
            .pending
            .iter()
            .filter_map(|(ifi, p)| if p.deadline <= now { Some(*ifi) } else { None })
            .collect();
        let mut out = Vec::with_capacity(expired.len());
        for ifi in expired {
            if let Some(p) = self.pending.remove(&ifi) {
                if let Some(seq) = p.probe_seq {
                    self.by_probe_seq.remove(&seq);
                }
                out.push(ClassifyOutcome::Ethernet(p.link));
            }
        }
        out
    }

    /// Next deadline the monitor should sleep until, if any. Returns
    /// `None` when there are no pending classifications.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.pending.values().map(|p| p.deadline).min()
    }

    /// Force-drop a pending entry (e.g., the interface was removed
    /// before classification resolved). Returns `true` if an entry
    /// was present.
    pub fn cancel(&mut self, ifindex: u32) -> bool {
        if let Some(p) = self.pending.remove(&ifindex) {
            if let Some(seq) = p.probe_seq {
                self.by_probe_seq.remove(&seq);
            }
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::rtnl::{ARPHRD_ETHER, IfInfoHeader};

    fn link(ifindex: i32, name: &str) -> LinkMessage {
        LinkMessage {
            header: IfInfoHeader {
                family: 0,
                ifi_type: ARPHRD_ETHER,
                index: ifindex,
                flags: 0,
                change: 0xFFFF_FFFF,
            },
            ifname: Some(name.to_owned()),
            mac: Some([0; 6]),
            mtu: Some(1500),
            operstate: Some(6),
            carrier: Some(true),
            info_kind: None,
            link: None,
            phys_port_name: None,
        }
    }

    fn nl80211(ifindex: u32) -> Nl80211InterfaceInfo {
        Nl80211InterfaceInfo {
            ifindex,
            ifname: None,
            wiphy: 0,
            wdev: 1,
            iftype: 2,
        }
    }

    #[test]
    fn probe_success_resolves_as_wireless() {
        let mut t = ClassifyTracker::new();
        let now = Instant::now();
        t.start(link(5, "wlan0"), Some(42), now);
        let outcome = t.resolve_probe_success(42, nl80211(5)).unwrap();
        assert!(outcome.is_wireless());
        assert!(t.is_empty());
    }

    #[test]
    fn probe_error_resolves_as_ethernet() {
        let mut t = ClassifyTracker::new();
        let now = Instant::now();
        t.start(link(2, "eth0"), Some(100), now);
        let outcome = t.resolve_probe_error(100, -2 /* ENOENT */).unwrap();
        assert!(matches!(outcome, ClassifyOutcome::Ethernet(_)));
        assert!(t.is_empty());
    }

    #[test]
    fn multicast_beats_probe_and_cancels_seq_mapping() {
        let mut t = ClassifyTracker::new();
        let now = Instant::now();
        t.start(link(7, "wlan1"), Some(77), now);
        let outcome = t.resolve_via_multicast(nl80211(7)).unwrap();
        assert!(outcome.is_wireless());
        assert!(t.resolve_probe_success(77, nl80211(7)).is_none());
    }

    #[test]
    fn sweep_timeouts_defaults_to_ethernet() {
        let timeout = Duration::from_millis(10);
        let mut t = ClassifyTracker::with_timeout(timeout);
        let now = Instant::now();
        t.start(link(3, "eth0"), Some(1), now);
        assert!(t.sweep_timeouts(now).is_empty());
        let outcomes = t.sweep_timeouts(now + timeout + Duration::from_millis(1));
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], ClassifyOutcome::Ethernet(_)));
        assert!(t.is_empty());
    }

    #[test]
    fn next_deadline_tracks_the_earliest() {
        let mut t = ClassifyTracker::with_timeout(Duration::from_secs(2));
        let now = Instant::now();
        t.start(link(2, "a"), Some(1), now);
        t.start(link(3, "b"), Some(2), now + Duration::from_millis(500));
        let deadline = t.next_deadline().unwrap();
        assert!(deadline <= now + Duration::from_secs(2) + Duration::from_millis(1));
    }

    #[test]
    fn cancel_clears_probe_mapping() {
        let mut t = ClassifyTracker::new();
        let now = Instant::now();
        t.start(link(9, "if9"), Some(900), now);
        assert!(t.cancel(9));
        assert!(t.resolve_probe_success(900, nl80211(9)).is_none());
    }

    #[test]
    fn no_probe_seq_still_timeout_expires_to_ethernet() {
        let timeout = Duration::from_millis(5);
        let mut t = ClassifyTracker::with_timeout(timeout);
        let now = Instant::now();
        t.start(link(10, "if10"), None, now);
        let outcomes = t.sweep_timeouts(now + timeout + Duration::from_millis(1));
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], ClassifyOutcome::Ethernet(_)));
    }
}
