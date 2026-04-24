//! Subscription abstraction for `nexusctl watch`.
//!
//! [`WatchSubset`] maps the operator's `nexusctl watch <subset>`
//! argument to the set of [`crate::watch::event`] kinds that
//! subscription should emit. [`WatchStream`] is the async trait
//! the command handler pulls events from; production impls wrap
//! zbus signal streams, tests yield scripted events via
//! [`MockWatchStream`].

use async_trait::async_trait;

use crate::errors::NexusctlError;
use crate::watch::event::WatchEvent;

/// Which event kinds `watch` should surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchSubset {
    /// Every row in the DD-008 §7.4 synthesis table.
    Events,
    /// `interface-*` + `link-state`.
    Iface,
    /// `wifi-*`.
    Wifi,
    /// `bt-*`.
    Bt,
    /// `gnss-*`.
    Gnss,
}

impl WatchSubset {
    /// `true` when the given event kind belongs in this subset.
    /// `WatchSubset::Events` passes everything.
    pub fn includes(self, event_kind: &str) -> bool {
        match self {
            WatchSubset::Events => true,
            WatchSubset::Iface => {
                event_kind == "link-state"
                    || event_kind.starts_with("interface-")
                    || event_kind.starts_with("iface-")
            }
            WatchSubset::Wifi => event_kind.starts_with("wifi-"),
            WatchSubset::Bt => event_kind.starts_with("bt-"),
            WatchSubset::Gnss => event_kind.starts_with("gnss-"),
        }
    }
}

/// Async stream of already-synthesised [`WatchEvent`]s. Implementers
/// are responsible for translating raw D-Bus signals into events
/// via the helpers in [`crate::watch::synthesize`].
#[async_trait]
pub trait WatchStream: Send {
    /// Yield the next event. `Ok(None)` means the stream closed
    /// cleanly (daemon disconnected, subscription cancelled) — the
    /// command handler treats that as DD-008 §3 "signal
    /// subscription dropped" and exits 0. `Err(_)` is a genuine
    /// subscription-level failure.
    async fn next(&mut self) -> Result<Option<WatchEvent>, NexusctlError>;
}

// ---------------------------------------------------------------------------
// Tests / mocks
// ---------------------------------------------------------------------------

/// Scripted [`WatchStream`] for tests. Gated behind
/// `interactive-flows-testing` so integration tests can use it.
#[cfg(any(test, feature = "interactive-flows-testing"))]
pub struct MockWatchStream {
    queue: std::sync::Mutex<std::collections::VecDeque<WatchEvent>>,
    /// When true, `next()` returns `Ok(None)` after the queue
    /// drains. When false, the stream parks forever (simulates a
    /// live subscription with no traffic).
    close_on_drain: bool,
}

#[cfg(any(test, feature = "interactive-flows-testing"))]
impl MockWatchStream {
    pub fn new(events: Vec<WatchEvent>) -> Self {
        Self {
            queue: std::sync::Mutex::new(events.into()),
            close_on_drain: true,
        }
    }

    pub fn new_open(events: Vec<WatchEvent>) -> Self {
        Self {
            queue: std::sync::Mutex::new(events.into()),
            close_on_drain: false,
        }
    }
}

#[cfg(any(test, feature = "interactive-flows-testing"))]
#[async_trait]
impl WatchStream for MockWatchStream {
    async fn next(&mut self) -> Result<Option<WatchEvent>, NexusctlError> {
        if let Some(ev) = self.queue.lock().unwrap().pop_front() {
            return Ok(Some(ev));
        }
        if self.close_on_drain {
            Ok(None)
        } else {
            std::future::pending::<Result<Option<WatchEvent>, NexusctlError>>().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_events_includes_everything() {
        let all = [
            "interface-added",
            "interface-removed",
            "link-state",
            "eth-auth-state",
            "wifi-state",
            "wifi-scan",
            "wifi-signal",
            "bt-adapter-state",
            "bt-device-state",
            "bt-pairing-started",
            "bt-pairing-prompt",
            "bt-pairing-complete",
            "gnss-fix",
            "profile-changed",
            "notification",
            "master-key-rotated",
            "power-state",
        ];
        for k in all {
            assert!(WatchSubset::Events.includes(k), "events should include {k}");
        }
    }

    #[test]
    fn subset_iface_covers_interface_arrivals_and_link_state() {
        let s = WatchSubset::Iface;
        assert!(s.includes("interface-added"));
        assert!(s.includes("interface-removed"));
        assert!(s.includes("link-state"));
        assert!(!s.includes("wifi-state"));
        assert!(!s.includes("bt-adapter-state"));
    }

    #[test]
    fn subset_wifi_only_wifi_kinds() {
        assert!(WatchSubset::Wifi.includes("wifi-state"));
        assert!(WatchSubset::Wifi.includes("wifi-scan"));
        assert!(WatchSubset::Wifi.includes("wifi-signal"));
        assert!(!WatchSubset::Wifi.includes("bt-adapter-state"));
        assert!(!WatchSubset::Wifi.includes("link-state"));
    }

    #[test]
    fn subset_bt_only_bt_kinds() {
        assert!(WatchSubset::Bt.includes("bt-adapter-state"));
        assert!(WatchSubset::Bt.includes("bt-device-state"));
        assert!(WatchSubset::Bt.includes("bt-pairing-started"));
        assert!(WatchSubset::Bt.includes("bt-pairing-prompt"));
        assert!(WatchSubset::Bt.includes("bt-pairing-complete"));
        assert!(!WatchSubset::Bt.includes("wifi-state"));
    }

    #[test]
    fn subset_gnss_only_gnss_kinds() {
        assert!(WatchSubset::Gnss.includes("gnss-fix"));
        assert!(!WatchSubset::Gnss.includes("bt-adapter-state"));
    }
}
