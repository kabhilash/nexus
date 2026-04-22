//! Operator-facing notification payloads. See nexus-architecture §6.

use std::collections::BTreeMap;
use std::collections::btree_map;

/// Key-value dict carried by
/// [`NexusEvent::OperatorNotification`](crate::event::NexusEvent::OperatorNotification).
/// The D-Bus layer marshals this to an `a{sv}` variant dict when
/// emitting `fi.nexus.Manager.NotificationEvent` (DD-006 §5.3).
#[derive(Debug, Clone, Default)]
pub struct NotificationData(pub BTreeMap<String, NotificationValue>);

/// Union of D-Bus-mappable scalar types carried by
/// [`NotificationData`]. `ObjectPath` serializes with the `o` D-Bus
/// signature; `String` uses `s`.
#[derive(Debug, Clone)]
pub enum NotificationValue {
    String(String),
    U32(u32),
    U64(u64),
    Bool(bool),
    /// Serialized as a D-Bus object path (`"o"`).
    ObjectPath(String),
}

impl NotificationData {
    /// Construct an empty payload.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Insert a key-value pair. Replaces any previous value for `k`.
    pub fn insert(&mut self, k: impl Into<String>, v: impl Into<NotificationValue>) {
        self.0.insert(k.into(), v.into());
    }

    /// Iterate over key-value pairs in key order.
    pub fn iter(&self) -> btree_map::Iter<'_, String, NotificationValue> {
        self.0.iter()
    }

    /// Look up a value by key.
    pub fn get(&self, k: &str) -> Option<&NotificationValue> {
        self.0.get(k)
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when the payload carries no entries.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'a> IntoIterator for &'a NotificationData {
    type Item = (&'a String, &'a NotificationValue);
    type IntoIter = btree_map::Iter<'a, String, NotificationValue>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl From<String> for NotificationValue {
    fn from(v: String) -> Self {
        NotificationValue::String(v)
    }
}

impl From<&str> for NotificationValue {
    fn from(v: &str) -> Self {
        NotificationValue::String(v.to_owned())
    }
}

impl From<u32> for NotificationValue {
    fn from(v: u32) -> Self {
        NotificationValue::U32(v)
    }
}

impl From<u64> for NotificationValue {
    fn from(v: u64) -> Self {
        NotificationValue::U64(v)
    }
}

impl From<bool> for NotificationValue {
    fn from(v: bool) -> Self {
        NotificationValue::Bool(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_iter_visits_entries_in_key_order() {
        let mut d = NotificationData::new();
        d.insert("zulu", "last");
        d.insert("alpha", 7u32);
        d.insert("mike", true);

        let keys: Vec<&str> = d.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, vec!["alpha", "mike", "zulu"]);
        assert_eq!(d.len(), 3);

        match d.get("alpha") {
            Some(NotificationValue::U32(7)) => {}
            other => panic!("unexpected value for alpha: {other:?}"),
        }
        match d.get("mike") {
            Some(NotificationValue::Bool(true)) => {}
            other => panic!("unexpected value for mike: {other:?}"),
        }
        match d.get("zulu") {
            Some(NotificationValue::String(s)) if s == "last" => {}
            other => panic!("unexpected value for zulu: {other:?}"),
        }
    }

    #[test]
    fn insert_replaces_existing_key() {
        let mut d = NotificationData::new();
        d.insert("k", 1u32);
        d.insert("k", "replaced");
        assert_eq!(d.len(), 1);
        match d.get("k") {
            Some(NotificationValue::String(s)) if s == "replaced" => {}
            other => panic!("expected replacement, got {other:?}"),
        }
    }

    #[test]
    fn empty_payload_has_no_entries() {
        let d = NotificationData::new();
        assert!(d.is_empty());
        assert_eq!(d.iter().count(), 0);
    }
}
