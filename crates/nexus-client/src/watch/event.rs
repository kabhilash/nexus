//! `WatchEvent` — a single synthesised row of `nexusctl watch`
//! output. DD-008 §7.4.
//!
//! Per §7.4's "flat dict" rule, `NotificationEvent.data` (and any
//! other nested `a{sv}`) gets hoisted into the top-level event
//! dict. `WatchEvent::fields` is a `BTreeMap` so the JSON output
//! order is deterministic across runs — scripts that grep the
//! emitted lines appreciate that.

use std::collections::BTreeMap;

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};

/// One synthesized event. `kind` mirrors the DD-008 §7.4 table
/// ("interface-added", "wifi-scan", "notification", …).
#[derive(Debug, Clone, PartialEq)]
pub struct WatchEvent {
    /// RFC 3339 timestamp (stamped at emission time, not the
    /// signal's original time). Zero-allocation compare-cheap
    /// strings are good enough here.
    pub time: String,
    /// Event-kind label. See DD-008 §7.4 table.
    pub kind: String,
    /// Flat top-level fields. Ordered so the JSON / human output
    /// stays deterministic. Values are strings (terse-mode
    /// emission is always stringified; JSON consumers can still
    /// parse numeric or boolean looking fields via jq).
    pub fields: BTreeMap<String, FieldValue>,
}

/// Value variants the wire can carry. Most fields come out of
/// D-Bus as strings or integers; we keep the JSON shape honest
/// rather than stringifying everything.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldValue {
    String(String),
    Int(i64),
    Uint(u64),
    Bool(bool),
    Float(f64),
}

impl From<String> for FieldValue {
    fn from(s: String) -> Self {
        FieldValue::String(s)
    }
}

impl From<&str> for FieldValue {
    fn from(s: &str) -> Self {
        FieldValue::String(s.to_owned())
    }
}

impl From<i32> for FieldValue {
    fn from(n: i32) -> Self {
        FieldValue::Int(n as i64)
    }
}

impl From<i16> for FieldValue {
    fn from(n: i16) -> Self {
        FieldValue::Int(n as i64)
    }
}

impl From<u32> for FieldValue {
    fn from(n: u32) -> Self {
        FieldValue::Uint(n as u64)
    }
}

impl From<u64> for FieldValue {
    fn from(n: u64) -> Self {
        FieldValue::Uint(n)
    }
}

impl From<bool> for FieldValue {
    fn from(b: bool) -> Self {
        FieldValue::Bool(b)
    }
}

impl From<f64> for FieldValue {
    fn from(f: f64) -> Self {
        FieldValue::Float(f)
    }
}

impl FieldValue {
    /// Stringified form used for `--filter` glob matching + terse
    /// / human rendering. Consistent with JSON's scalar printing.
    pub fn as_display(&self) -> String {
        match self {
            FieldValue::String(s) => s.clone(),
            FieldValue::Int(n) => n.to_string(),
            FieldValue::Uint(n) => n.to_string(),
            FieldValue::Bool(b) => b.to_string(),
            FieldValue::Float(f) => f.to_string(),
        }
    }
}

impl Serialize for FieldValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            FieldValue::String(v) => s.serialize_str(v),
            FieldValue::Int(v) => s.serialize_i64(*v),
            FieldValue::Uint(v) => s.serialize_u64(*v),
            FieldValue::Bool(v) => s.serialize_bool(*v),
            FieldValue::Float(v) => s.serialize_f64(*v),
        }
    }
}

/// Custom `Serialize` so `time` and `kind` lead, followed by the
/// flat field map — matches DD-008 §7.4's example output.
impl Serialize for WatchEvent {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(2 + self.fields.len()))?;
        map.serialize_entry("time", &self.time)?;
        map.serialize_entry("kind", &self.kind)?;
        for (k, v) in &self.fields {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl WatchEvent {
    /// Construct at `now_rfc3339`-stamped time. Test builders use
    /// [`WatchEvent::at`] so snapshots are deterministic.
    pub fn now(kind: &str) -> Self {
        Self {
            time: now_rfc3339(),
            kind: kind.into(),
            fields: BTreeMap::new(),
        }
    }

    pub fn at(time: &str, kind: &str) -> Self {
        Self {
            time: time.into(),
            kind: kind.into(),
            fields: BTreeMap::new(),
        }
    }

    pub fn with<V: Into<FieldValue>>(mut self, key: &str, value: V) -> Self {
        self.fields.insert(key.into(), value.into());
        self
    }

    /// Look up a field as a string for `--filter` matching.
    pub fn get(&self, key: &str) -> Option<String> {
        if key == "kind" {
            return Some(self.kind.clone());
        }
        if key == "time" {
            return Some(self.time.clone());
        }
        self.fields.get(key).map(FieldValue::as_display)
    }
}

/// Bare-minimum RFC 3339 UTC timestamp — we only need seconds
/// resolution for a watch log. Avoids the `chrono` dependency
/// in nexus-client.
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Converting to a calendar date without chrono is tedious;
    // emit Unix time as the RFC 3339 profile allows `YYYY-...` but
    // `@<secs>` isn't standard. Use a simple conversion here.
    unix_to_rfc3339(secs)
}

fn unix_to_rfc3339(mut secs: i64) -> String {
    // Days since 1970-01-01 (Thursday), then Zeller-ish date math.
    let sec_in_day = 86_400;
    let mut days = secs.div_euclid(sec_in_day);
    secs = secs.rem_euclid(sec_in_day);
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    // Date conversion using a well-known algorithm: Gregorian date
    // from days-since-epoch. Good through AD 99999.
    let (y, mo, d) = days_to_ymd(days as i32);
    let _ = &mut days;
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(days: i32) -> (i32, u32, u32) {
    // Algorithm from Howard Hinnant's "chrono-Compatible
    // Low-Level Date Algorithms". Accurate for -4712-03 .. +9999.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_order_keeps_time_and_kind_first() {
        let ev = WatchEvent::at("2026-03-01T00:00:00Z", "wifi-scan")
            .with("iface", "wlan0")
            .with("result_count", 5u32);
        let s = serde_json::to_string(&ev).unwrap();
        // Time and kind must lead.
        assert!(s.starts_with("{\"time\":\"2026-03-01T00:00:00Z\",\"kind\":\"wifi-scan\""));
        // Scalar fields round-trip with their native JSON type.
        assert!(s.contains("\"result_count\":5"));
    }

    #[test]
    fn fields_are_sorted_for_deterministic_output() {
        let ev = WatchEvent::at("t", "k")
            .with("z", "last")
            .with("a", "first")
            .with("m", "middle");
        let s = serde_json::to_string(&ev).unwrap();
        let a_pos = s.find("\"a\":").unwrap();
        let m_pos = s.find("\"m\":").unwrap();
        let z_pos = s.find("\"z\":").unwrap();
        assert!(a_pos < m_pos && m_pos < z_pos);
    }

    #[test]
    fn get_resolves_kind_time_and_fields() {
        let ev = WatchEvent::at("T", "k").with("iface", "eth0");
        assert_eq!(ev.get("kind"), Some("k".to_owned()));
        assert_eq!(ev.get("time"), Some("T".to_owned()));
        assert_eq!(ev.get("iface"), Some("eth0".to_owned()));
        assert_eq!(ev.get("missing"), None);
    }

    #[test]
    fn days_to_ymd_matches_known_dates() {
        // 1970-01-01 is day 0; 2020-01-01 is day 18262.
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        assert_eq!(days_to_ymd(18_262), (2020, 1, 1));
    }
}
