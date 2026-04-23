//! Property-change coalescing. See DD-006 §12.
//!
//! Hot properties (`SignalDbm`, `Frequency`, GNSS sat-counts,
//! scan-result `AgeMs`) can update many times per second. zbus's
//! standard `PropertiesChanged` flow would emit one signal per
//! change — saturating the bus on a degraded link.
//!
//! [`PropertyBatcher`] accumulates dirty property names into a
//! per-(object, interface) bucket and flushes at most twice per
//! second (DD-006 §12.2). Cold properties (state transitions,
//! profile changes) bypass the batcher and emit immediately.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Default coalescing window. The DD ceiling is "≤ 2 Hz per
/// property", i.e. one signal per 500 ms per object/interface.
pub const COALESCE_WINDOW: Duration = Duration::from_millis(500);

/// Per-(object path, interface name) batcher for `PropertiesChanged`
/// emission. The map records dirty property names; `pop_due`
/// returns the buckets ready to flush.
#[derive(Debug, Default)]
pub struct PropertyBatcher {
    inner: Mutex<HashMap<(String, String), BucketState>>,
}

#[derive(Debug)]
struct BucketState {
    /// Property names accumulated since the last flush.
    dirty: Vec<String>,
    /// When the bucket was first marked dirty in this window.
    armed_at: Instant,
}

impl PropertyBatcher {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Mark `prop` dirty under `(path, iface)`. Returns the
    /// timestamp the bucket was first armed at — callers schedule
    /// a flush relative to that.
    pub fn mark(&self, path: &str, iface: &str, prop: &str) -> Instant {
        let now = Instant::now();
        let mut state = self.inner.lock().unwrap();
        let entry = state
            .entry((path.to_owned(), iface.to_owned()))
            .or_insert_with(|| BucketState {
                dirty: Vec::new(),
                armed_at: now,
            });
        if !entry.dirty.iter().any(|s| s == prop) {
            entry.dirty.push(prop.to_owned());
        }
        entry.armed_at
    }

    /// Drain every bucket whose `armed_at` is older than `window`
    /// from `now`. Returns `(path, iface, props)` for each one;
    /// callers hand them to the connection's `PropertiesChanged`
    /// emitter.
    pub fn pop_due(&self, now: Instant, window: Duration) -> Vec<(String, String, Vec<String>)> {
        let mut state = self.inner.lock().unwrap();
        let mut out = Vec::new();
        let mut to_remove = Vec::new();
        for (key, bucket) in state.iter() {
            if now.duration_since(bucket.armed_at) >= window {
                out.push((key.0.clone(), key.1.clone(), bucket.dirty.clone()));
                to_remove.push(key.clone());
            }
        }
        for k in to_remove {
            state.remove(&k);
        }
        out
    }

    /// Emergency flush: drain every bucket regardless of age.
    /// Used at shutdown so clients see the last batch of updates.
    pub fn drain_all(&self) -> Vec<(String, String, Vec<String>)> {
        let mut state = self.inner.lock().unwrap();
        let out: Vec<_> = state
            .drain()
            .map(|(key, bucket)| (key.0, key.1, bucket.dirty))
            .collect();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_records_unique_properties() {
        let b = PropertyBatcher::new();
        b.mark("/p", "fi.nexus.Wifi", "SignalDbm");
        b.mark("/p", "fi.nexus.Wifi", "SignalDbm"); // dedup
        b.mark("/p", "fi.nexus.Wifi", "Frequency");
        let due = b.drain_all();
        assert_eq!(due.len(), 1);
        let (_, _, props) = &due[0];
        assert_eq!(props.len(), 2);
        assert!(props.contains(&"SignalDbm".to_owned()));
        assert!(props.contains(&"Frequency".to_owned()));
    }

    #[test]
    fn pop_due_respects_window() {
        let b = PropertyBatcher::new();
        b.mark("/p", "fi.nexus.Wifi", "SignalDbm");
        // Right after marking, nothing is due under a 500ms window.
        let now = Instant::now();
        let due = b.pop_due(now, COALESCE_WINDOW);
        assert!(due.is_empty());
        // After the window elapses, the bucket flushes.
        let later = now + COALESCE_WINDOW + Duration::from_millis(10);
        let due = b.pop_due(later, COALESCE_WINDOW);
        assert_eq!(due.len(), 1);
    }

    #[test]
    fn separate_objects_are_independent() {
        let b = PropertyBatcher::new();
        b.mark("/a", "fi.nexus.Wifi", "SignalDbm");
        b.mark("/b", "fi.nexus.Wifi", "SignalDbm");
        let all = b.drain_all();
        assert_eq!(all.len(), 2);
    }
}
