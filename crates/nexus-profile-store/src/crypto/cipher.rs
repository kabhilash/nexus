//! ChaCha20-Poly1305 AEAD. See DD-007 §4.3.
//!
//! The [`Cipher`] trait is abstract so tests can substitute a fake;
//! [`ChaChaCipher`] is the real implementation.

use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, KeyInit, OsRng, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::blob::{ENC_TAG_V1, EncryptedBlob};

/// Errors surfaced by the [`Cipher`] trait.
#[derive(Debug, Error)]
pub enum CipherError {
    /// The blob's `enc` tag is not recognized by this cipher version.
    #[error("unknown encryption version tag: {0}")]
    UnknownVersion(String),

    /// The blob's `ad_hash` doesn't match the AAD supplied at
    /// decrypt time. Fails fast before invoking the AEAD.
    #[error("associated-data hash mismatch")]
    AdHashMismatch,

    /// AEAD rejected the ciphertext — wrong key, wrong AAD (that
    /// matched the hash by collision), or tampering.
    #[error("ciphertext failed authentication")]
    DecryptFailed,

    /// Encryption failed at the AEAD layer. Very rare — indicates a
    /// library invariant violation.
    #[error("encryption failed: {0}")]
    EncryptFailed(String),

    /// A decrypted plaintext didn't parse as UTF-8 when a string
    /// field was expected.
    #[error("decrypted plaintext is not valid UTF-8")]
    InvalidUtf8,
}

/// Cipher abstraction.  Implementations hold a per-file key
/// internally; the caller just supplies AAD + plaintext.
pub trait Cipher: Send + Sync {
    fn encrypt(&self, ad: &[u8], plaintext: &[u8]) -> Result<EncryptedBlob, CipherError>;
    fn decrypt(&self, ad: &[u8], blob: &EncryptedBlob) -> Result<Vec<u8>, CipherError>;
}

/// Concrete ChaCha20-Poly1305 cipher. Holds a zeroizing copy of the
/// per-file key.
pub struct ChaChaCipher {
    key: ChaChaKey,
}

impl ChaChaCipher {
    /// Wrap a 32-byte key. The input is copied into zeroizing
    /// storage; the caller may zeroize its own copy afterward.
    pub fn new(key: [u8; 32]) -> Self {
        Self {
            key: ChaChaKey(key),
        }
    }
}

impl Cipher for ChaChaCipher {
    fn encrypt(&self, ad: &[u8], plaintext: &[u8]) -> Result<EncryptedBlob, CipherError> {
        let aead = ChaCha20Poly1305::new(Key::from_slice(&self.key.0));
        let mut nonce_bytes = [0u8; 12];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ct = aead
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: ad,
                },
            )
            .map_err(|e| CipherError::EncryptFailed(e.to_string()))?;
        let ad_hash = sha256(ad);
        Ok(EncryptedBlob::v1(nonce_bytes, ct, ad_hash))
    }

    fn decrypt(&self, ad: &[u8], blob: &EncryptedBlob) -> Result<Vec<u8>, CipherError> {
        if blob.enc != ENC_TAG_V1 {
            return Err(CipherError::UnknownVersion(blob.enc.clone()));
        }
        if sha256(ad) != blob.ad_hash {
            return Err(CipherError::AdHashMismatch);
        }
        let aead = ChaCha20Poly1305::new(Key::from_slice(&self.key.0));
        let nonce = Nonce::from_slice(&blob.nonce);
        aead.decrypt(
            nonce,
            Payload {
                msg: &blob.ct,
                aad: ad,
            },
        )
        .map_err(|_| CipherError::DecryptFailed)
    }
}

impl std::fmt::Debug for ChaChaCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChaChaCipher")
            .field("key", &"<redacted>")
            .finish()
    }
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// 32-byte key held in zeroizing storage.
struct ChaChaKey([u8; 32]);

impl Drop for ChaChaKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

// Mark the type explicitly: the Drop above already zeroizes, but
// this derive also protects against accidental exposure via tooling
// that looks for the ZeroizeOnDrop bound.
impl ZeroizeOnDrop for ChaChaKey {}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let c = ChaChaCipher::new(fixed_key());
        let ad = b"wifi:ULID:network.psk";
        let pt = b"correct horse battery staple";
        let blob = c.encrypt(ad, pt).unwrap();
        let back = c.decrypt(ad, &blob).unwrap();
        assert_eq!(back, pt);
    }

    #[test]
    fn each_write_uses_a_fresh_nonce() {
        let c = ChaChaCipher::new(fixed_key());
        let ad = b"wifi:ULID:network.psk";
        let a = c.encrypt(ad, b"pt").unwrap();
        let b = c.encrypt(ad, b"pt").unwrap();
        assert_ne!(a.nonce, b.nonce);
        assert_ne!(a.ct, b.ct);
    }

    #[test]
    fn wrong_ad_fails_on_hash_check() {
        let c = ChaChaCipher::new(fixed_key());
        let blob = c.encrypt(b"wifi:A:network.psk", b"secret").unwrap();
        // Different AD — hash check fires before AEAD.
        let err = c.decrypt(b"wifi:B:network.psk", &blob).unwrap_err();
        assert!(matches!(err, CipherError::AdHashMismatch));
    }

    #[test]
    fn tampered_ct_fails_authentication() {
        let c = ChaChaCipher::new(fixed_key());
        let mut blob = c.encrypt(b"ad", b"secret").unwrap();
        // Flip one bit in the ciphertext. Do it at the last byte so
        // the Poly1305 tag (which lives at the end) is guaranteed
        // affected — either the tag itself or the final payload
        // byte will diverge.
        let last = blob.ct.len() - 1;
        blob.ct[last] ^= 0x01;
        let err = c.decrypt(b"ad", &blob).unwrap_err();
        assert!(matches!(err, CipherError::DecryptFailed));
    }

    #[test]
    fn unknown_version_tag_is_rejected() {
        let c = ChaChaCipher::new(fixed_key());
        let mut blob = c.encrypt(b"ad", b"secret").unwrap();
        blob.enc = "v99".into();
        let err = c.decrypt(b"ad", &blob).unwrap_err();
        assert!(matches!(err, CipherError::UnknownVersion(_)));
    }

    #[test]
    fn debug_impl_redacts_the_key() {
        let c = ChaChaCipher::new(fixed_key());
        let rendered = format!("{c:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("0, 1, 2, 3"));
    }
}
