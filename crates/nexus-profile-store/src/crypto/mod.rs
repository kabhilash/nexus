//! Credential encryption primitives. See DD-007 §4.
//!
//! The public surface is three types:
//! - [`EncryptedBlob`] — on-disk shape
//! - [`Cipher`] + [`ChaChaCipher`] — the AEAD
//! - [`kdf::derive_file_key`] — HKDF-SHA256 per-file keys
//! - [`associated_data`] — the AAD construction from DD-007 §4.3

pub mod blob;
pub mod cipher;
pub mod kdf;

pub use blob::{ENC_TAG_V1, EncryptedBlob};
pub use cipher::{ChaChaCipher, Cipher, CipherError};
pub use kdf::{HKDF_SALT, derive_file_key};

use crate::trait_def::ProfileKind;
use ulid::Ulid;

/// Build the AAD bound to one ciphertext. Binds the ciphertext to
/// (a) its profile kind, (b) its profile ULID, and (c) its field
/// path within the profile. Reshuffling or cross-file copy of a
/// ciphertext won't decrypt.
pub fn associated_data(kind: ProfileKind, id: &Ulid, field_path: &str) -> Vec<u8> {
    format!("{}:{}:{}", kind_tag(kind), id, field_path).into_bytes()
}

/// Stable string used in [`associated_data`] for each kind. Kept
/// distinct from any future metric-label renaming so we never have
/// to rotate ciphertexts just because observability code changed.
pub fn kind_tag(kind: ProfileKind) -> &'static str {
    match kind {
        ProfileKind::Ethernet => "ethernet",
        ProfileKind::Wifi => "wifi",
        ProfileKind::Gnss => "gnss",
        ProfileKind::Bluetooth => "bluetooth",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associated_data_includes_kind_id_and_field() {
        let id = Ulid::from_string("01ARZ3NDEKTSV4RRFFQ69G5FAV").unwrap();
        let ad = associated_data(ProfileKind::Wifi, &id, "network.security.psk");
        let s = std::str::from_utf8(&ad).unwrap();
        assert!(s.starts_with("wifi:01ARZ3NDEKTSV4RRFFQ69G5FAV:"));
        assert!(s.ends_with(":network.security.psk"));
    }

    #[test]
    fn different_kinds_or_fields_produce_different_ads() {
        let id = Ulid::new();
        let a = associated_data(ProfileKind::Wifi, &id, "x");
        let b = associated_data(ProfileKind::Ethernet, &id, "x");
        let c = associated_data(ProfileKind::Wifi, &id, "y");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
