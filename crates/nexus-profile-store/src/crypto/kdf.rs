//! Per-file key derivation. See DD-007 §4.3.
//!
//! `file_key = HKDF-SHA256(ikm=master_key, salt="nexus-profile-store-v1", info=file_id_bytes)`
//!
//! Where `file_id_bytes` is the profile ULID as 16 little-endian bytes.
//! HKDF expands 32 bytes of output material — the per-file
//! ChaCha20-Poly1305 key.

use hkdf::Hkdf;
use sha2::Sha256;
use zeroize::Zeroize;

/// Fixed (non-secret) salt for HKDF. DD-007 §4.3.
pub const HKDF_SALT: &[u8] = b"nexus-profile-store-v1";

/// Derive the 32-byte file key from the master key and a file id.
/// The `file_id` input is the profile ULID's raw 16 bytes.
pub fn derive_file_key(master_key: &[u8; 32], file_id: &[u8]) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), master_key);
    let mut out = [0u8; 32];
    // HKDF::expand returns `Err` only when the requested length is
    // longer than 255 * HashLen; 32 bytes from SHA-256 is always fine.
    hk.expand(file_id, &mut out)
        .expect("32-byte HKDF-SHA256 expansion always fits");
    out
}

/// Erase a 32-byte key buffer. Used where the key lifetime isn't
/// bound by a zeroizing wrapper type.
pub fn zeroize_key(key: &mut [u8; 32]) {
    key.zeroize();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_file_key_is_deterministic() {
        let master = [0x42u8; 32];
        let a = derive_file_key(&master, b"hello");
        let b = derive_file_key(&master, b"hello");
        assert_eq!(a, b);
    }

    #[test]
    fn different_file_ids_produce_different_keys() {
        let master = [0x42u8; 32];
        let a = derive_file_key(&master, b"hello");
        let b = derive_file_key(&master, b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn different_masters_produce_different_keys() {
        let a = derive_file_key(&[0x42u8; 32], b"hello");
        let b = derive_file_key(&[0x43u8; 32], b"hello");
        assert_ne!(a, b);
    }
}
