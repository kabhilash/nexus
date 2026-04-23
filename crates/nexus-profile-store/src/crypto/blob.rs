//! On-disk representation of an encrypted field. See DD-007 §4.4.
//!
//! `EncryptedBlob` is the serializable form a `SecretString` takes
//! inside a `*OnDisk` profile struct. On disk it's a TOML inline
//! table with `enc`/`nonce`/`ct`/`ad_hash` keys; the three variable
//! fields are base64url-unpadded.
//!
//! `ad_hash` is the SHA-256 of the associated data passed at encrypt
//! time. It's a defense-in-depth tripwire: AEAD decryption would
//! already fail on an AAD mismatch, but a stored hash lets the
//! decrypt path bail out with a descriptive error before invoking
//! the cipher, and lets forensic tooling reason about a profile
//! file without access to the master key.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Current on-disk encryption version tag.
pub const ENC_TAG_V1: &str = "v1";

/// Encrypted field on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncryptedBlob {
    /// Version tag: `"v1"` is ChaCha20-Poly1305 per DD-007 §4.3.
    pub enc: String,
    /// 12-byte nonce (base64url-unpadded on wire).
    pub nonce: [u8; 12],
    /// Ciphertext + 16-byte Poly1305 tag (base64url on wire).
    pub ct: Vec<u8>,
    /// SHA-256 of the AAD used at encrypt time (base64url on wire).
    pub ad_hash: [u8; 32],
}

impl EncryptedBlob {
    /// Construct a `v1` blob.
    pub fn v1(nonce: [u8; 12], ct: Vec<u8>, ad_hash: [u8; 32]) -> Self {
        Self {
            enc: ENC_TAG_V1.to_owned(),
            nonce,
            ct,
            ad_hash,
        }
    }
}

// ---------------------------------------------------------------------------
// Wire format: a TOML table with base64url-encoded byte fields.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Wire {
    enc: String,
    nonce: String,
    ct: String,
    ad_hash: String,
}

impl Serialize for EncryptedBlob {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = Wire {
            enc: self.enc.clone(),
            nonce: URL_SAFE_NO_PAD.encode(self.nonce),
            ct: URL_SAFE_NO_PAD.encode(&self.ct),
            ad_hash: URL_SAFE_NO_PAD.encode(self.ad_hash),
        };
        wire.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EncryptedBlob {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let wire = Wire::deserialize(deserializer)?;

        let nonce_bytes = URL_SAFE_NO_PAD
            .decode(&wire.nonce)
            .map_err(D::Error::custom)?;
        if nonce_bytes.len() != 12 {
            return Err(D::Error::custom(format!(
                "nonce must be 12 bytes, got {}",
                nonce_bytes.len(),
            )));
        }
        let mut nonce = [0u8; 12];
        nonce.copy_from_slice(&nonce_bytes);

        let ad_hash_bytes = URL_SAFE_NO_PAD
            .decode(&wire.ad_hash)
            .map_err(D::Error::custom)?;
        if ad_hash_bytes.len() != 32 {
            return Err(D::Error::custom(format!(
                "ad_hash must be 32 bytes, got {}",
                ad_hash_bytes.len(),
            )));
        }
        let mut ad_hash = [0u8; 32];
        ad_hash.copy_from_slice(&ad_hash_bytes);

        let ct = URL_SAFE_NO_PAD.decode(&wire.ct).map_err(D::Error::custom)?;

        Ok(Self {
            enc: wire.enc,
            nonce,
            ct,
            ad_hash,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_roundtrip_preserves_bytes() {
        let blob = EncryptedBlob::v1([0xAA; 12], vec![0x01, 0x02, 0x03, 0xFF], [0xBB; 32]);

        // Wrap in a struct so serde's TOML target is a table.
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            secret: EncryptedBlob,
        }

        let text = toml::to_string(&Wrap {
            secret: blob.clone(),
        })
        .unwrap();
        // `enc` is always "v1"; other fields are base64url.
        assert!(text.contains("enc = \"v1\""));
        assert!(!text.contains("aa:aa"), "raw bytes must not appear");

        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(back.secret, blob);
    }

    #[test]
    fn bad_nonce_length_is_rejected() {
        let text = r#"
enc = "v1"
nonce = "AAAA"
ct = "AAAA"
ad_hash = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
"#;
        let err = toml::from_str::<EncryptedBlob>(text).unwrap_err();
        assert!(err.to_string().contains("nonce"), "got: {err}");
    }

    #[test]
    fn bad_ad_hash_length_is_rejected() {
        let text = r#"
enc = "v1"
nonce = "AAAAAAAAAAAAAAAA"
ct = "AAAA"
ad_hash = "AA"
"#;
        let err = toml::from_str::<EncryptedBlob>(text).unwrap_err();
        assert!(err.to_string().contains("ad_hash"), "got: {err}");
    }
}
