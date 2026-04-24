//! `--filter 'field=glob'` matching for `nexusctl watch`.
//! DD-008 §7.4.
//!
//! Multiple filters AND together. Missing fields on a given event
//! never match (so `--filter 'iface=eth0'` naturally excludes
//! events with no `iface` key — profile-changed, master-key-rotated,
//! etc.).

use globset::{Glob, GlobMatcher};

use crate::errors::NexusctlError;
use crate::watch::event::WatchEvent;

/// One parsed `--filter` argument.
#[derive(Debug)]
pub struct Filter {
    field: String,
    matcher: GlobMatcher,
    original: String,
}

impl Filter {
    /// Parse `field=glob`. Spaces around `=` are not tolerated; this
    /// is a shell-scriptable interface, not a free-form DSL.
    pub fn parse(raw: &str) -> Result<Self, NexusctlError> {
        let (field, glob) = raw
            .split_once('=')
            .ok_or_else(|| NexusctlError::InvalidArgument {
                message: format!("filter `{raw}` must be `field=glob`"),
            })?;
        if field.is_empty() {
            return Err(NexusctlError::InvalidArgument {
                message: format!("filter `{raw}` is missing a field name"),
            });
        }
        let matcher = Glob::new(glob)
            .map_err(|e| NexusctlError::InvalidArgument {
                message: format!("bad glob in filter `{raw}`: {e}"),
            })?
            .compile_matcher();
        Ok(Self {
            field: field.to_owned(),
            matcher,
            original: raw.to_owned(),
        })
    }

    pub fn field(&self) -> &str {
        &self.field
    }

    pub fn as_str(&self) -> &str {
        &self.original
    }

    pub fn matches(&self, event: &WatchEvent) -> bool {
        match event.get(&self.field) {
            Some(v) => self.matcher.is_match(&v),
            None => false,
        }
    }
}

/// Apply every filter (AND). Empty filter list always matches.
pub fn passes(event: &WatchEvent, filters: &[Filter]) -> bool {
    filters.iter().all(|f| f.matches(event))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev() -> WatchEvent {
        WatchEvent::at("t", "link-state")
            .with("iface", "eth0")
            .with("state", "up")
    }

    #[test]
    fn parse_accepts_field_equals_glob() {
        let f = Filter::parse("iface=eth0").unwrap();
        assert_eq!(f.field(), "iface");
    }

    #[test]
    fn parse_rejects_missing_equals() {
        let err = Filter::parse("iface").unwrap_err();
        assert!(matches!(err, NexusctlError::InvalidArgument { .. }));
    }

    #[test]
    fn parse_rejects_empty_field() {
        assert!(Filter::parse("=eth0").is_err());
    }

    #[test]
    fn parse_rejects_invalid_glob() {
        // '[' without matching ']' is malformed.
        assert!(Filter::parse("iface=[").is_err());
    }

    #[test]
    fn exact_match_passes() {
        let f = Filter::parse("iface=eth0").unwrap();
        assert!(f.matches(&ev()));
    }

    #[test]
    fn wildcard_star_matches_prefix() {
        let f = Filter::parse("kind=link-*").unwrap();
        assert!(f.matches(&ev()));
    }

    #[test]
    fn wildcard_star_matches_suffix() {
        let f = Filter::parse("iface=eth?").unwrap();
        assert!(f.matches(&ev()));
    }

    #[test]
    fn missing_field_never_matches() {
        let f = Filter::parse("adapter=hci0").unwrap();
        assert!(!f.matches(&ev()));
    }

    #[test]
    fn multiple_filters_and_together() {
        let fs = vec![
            Filter::parse("iface=eth0").unwrap(),
            Filter::parse("state=up").unwrap(),
        ];
        assert!(passes(&ev(), &fs));
    }

    #[test]
    fn multiple_filters_fail_on_any_mismatch() {
        let fs = vec![
            Filter::parse("iface=eth0").unwrap(),
            Filter::parse("state=down").unwrap(),
        ];
        assert!(!passes(&ev(), &fs));
    }

    #[test]
    fn empty_filters_always_match() {
        assert!(passes(&ev(), &[]));
    }

    #[test]
    fn char_class_glob_works() {
        let f = Filter::parse("iface=eth[012]").unwrap();
        assert!(f.matches(&ev()));
        let f = Filter::parse("iface=wlan[012]").unwrap();
        assert!(!f.matches(&ev()));
    }
}
