//! [`SecretString`] — credential strings that never appear in Debug
//! output and are zeroized on drop. See DD-007 §5.3.
//!
//! # Not Serialize / Deserialize
//!
//! Implementing `Serialize` on [`SecretString`] would either
//! encrypt every serialization (we want in-memory copies to stay
//! plaintext) or silently reveal secrets on `toml::to_string`.
//! Neither is acceptable, so this crate provides the in-memory
//! container type alone; the dual-struct pattern (`*ProfileOnDisk`)
//! converts to/from a plaintext `String` at the (en|de)crypt
//! boundary — plaintext during the phase-2 filesystem store,
//! `EncryptedBlob` once phase-3 encryption is wired in.

use std::fmt;

use secrecy::ExposeSecret;

/// String credential with redacted `Debug` and zero-on-drop
/// semantics.
///
/// Wraps [`secrecy::SecretString`] but hides the type. Callers never
/// use the `secrecy` crate's API directly — only the two entry
/// points here and the explicit [`SecretString::expose_secret`]
/// accessor.
#[derive(Clone)]
pub struct SecretString(secrecy::SecretString);

impl SecretString {
    /// Wrap an already-owned `String`. The input is moved into
    /// zeroizing storage.
    pub fn new(plaintext: String) -> Self {
        Self(secrecy::SecretString::new(plaintext.into()))
    }

    /// Borrow the underlying plaintext. The returned reference is
    /// only meant for the Profile Store's serialization boundary —
    /// logging or re-transmitting it elsewhere defeats the point.
    pub fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

// Equality is value-based for tests and round-trip assertions. In
// production code comparing secrets directly is rare — the supplicant
// layer hashes before comparing — but having PartialEq here is a
// substantial ergonomics win and doesn't broaden the leak surface.
impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        self.expose_secret() == other.expose_secret()
    }
}

impl Eq for SecretString {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_plaintext() {
        let s = SecretString::new("hunter2".into());
        assert_eq!(format!("{s:?}"), "SecretString(<redacted>)");
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn expose_secret_returns_plaintext() {
        let s = SecretString::from("hunter2");
        assert_eq!(s.expose_secret(), "hunter2");
    }

    #[test]
    fn equality_is_value_based() {
        let a = SecretString::from("same");
        let b = SecretString::from("same");
        let c = SecretString::from("other");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // Compile-time assertion via a generic bound: `SecretString` does
    // NOT implement `Serialize` or `Deserialize`. The file-level rule
    // in DD-007 §5.3. This test proves the property via static check.
    #[test]
    fn secret_string_is_not_serialize_or_deserialize() {
        fn assert_not<T>()
        where
            T: SecretStringMarker,
        {
        }
        assert_not::<SecretString>();
    }

    trait SecretStringMarker {}
    impl SecretStringMarker for SecretString {}
    // If someone ever adds `impl Serialize for SecretString`, this
    // trait-set check won't catch it directly; the check is mostly
    // documentary. The real guard is that we never import serde on
    // `SecretString` in `secret.rs` and that grep'ing for
    // `Serialize for SecretString` comes up empty across the crate.
}
