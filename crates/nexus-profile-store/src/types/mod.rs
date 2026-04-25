//! Profile types.
//!
//! Each technology has an in-memory form (with `SecretString`
//! credentials) and a matching on-disk form (with `EncryptedBlob`
//! credentials). The dual-struct pattern keeps `SecretString` out
//! of every `Serialize` impl (DD-007 §5.3). GNSS and Bluetooth
//! profiles carry no credential fields, so they use a single struct.
//!
//! Cross-technology types (`Dot1xEapConfig`, `SecurityConfig`) live
//! here rather than in their "home" backend crate because the
//! backend crates are currently placeholders — these definitions
//! will move once those crates are implemented.

pub mod bluetooth;
pub mod ethernet;
pub mod gnss;
pub mod wifi;

pub use nexus_core::ProfileMetadata;

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use crate::crypto::{Cipher, CipherError, EncryptedBlob, associated_data};
use crate::secret::SecretString;
use crate::trait_def::ProfileKind;

// ---------------------------------------------------------------------------
// Dot1x / EAP
// ---------------------------------------------------------------------------

/// EAP identity + credentials used by both wired (802.1X) and
/// enterprise Wi-Fi. Contains `SecretString` fields; NOT Serialize
/// or Deserialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dot1xEapConfig {
    pub eap: EapMethod,
    pub identity: String,
    pub anonymous_identity: Option<String>,
    pub ca_cert: Option<String>,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
    pub client_key_password: Option<SecretString>,
    pub phase2: Option<String>,
    pub domain_suffix_match: Option<String>,
    pub password: Option<SecretString>,
}

/// EAP method identifier. Expected values correspond to
/// wpa_supplicant's `eap` parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum EapMethod {
    Peap,
    Ttls,
    Tls,
    PwdMschapv2,
    Leap,
    Fast,
}

impl EapMethod {
    /// Stable wire label exposed via `fi.nexus.Profile.Ethernet.Dot1xEap`
    /// (DD-006 §7.3) and `fi.nexus.Interface.StateChanged` `details`
    /// (DD-006 §6.2 / §9). Source of truth for the string form;
    /// callers that need the label must go through this method to
    /// avoid drift between profile-side and lifecycle-side surfaces.
    pub fn as_str(self) -> &'static str {
        match self {
            EapMethod::Peap => "PEAP",
            EapMethod::Ttls => "TTLS",
            EapMethod::Tls => "TLS",
            EapMethod::PwdMschapv2 => "PWD_MSCHAPV2",
            EapMethod::Leap => "LEAP",
            EapMethod::Fast => "FAST",
        }
    }
}

#[cfg(test)]
mod eap_method_tests {
    use super::EapMethod;

    #[test]
    fn as_str_covers_every_variant() {
        // Every variant must produce a non-empty label so D-Bus
        // properties (DD-006 §6.2 / §7.3) never surface "" by
        // accident on a future enum extension.
        for m in [
            EapMethod::Peap,
            EapMethod::Ttls,
            EapMethod::Tls,
            EapMethod::PwdMschapv2,
            EapMethod::Leap,
            EapMethod::Fast,
        ] {
            assert!(!m.as_str().is_empty(), "{m:?} produced empty label");
        }
        assert_eq!(EapMethod::Peap.as_str(), "PEAP");
        assert_eq!(EapMethod::Tls.as_str(), "TLS");
        assert_eq!(EapMethod::PwdMschapv2.as_str(), "PWD_MSCHAPV2");
    }
}

/// On-disk form of [`Dot1xEapConfig`]. Credential fields are
/// `EncryptedBlob`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dot1xEapConfigOnDisk {
    pub eap: EapMethod,
    pub identity: String,
    #[serde(default)]
    pub anonymous_identity: Option<String>,
    #[serde(default)]
    pub ca_cert: Option<String>,
    #[serde(default)]
    pub client_cert: Option<String>,
    #[serde(default)]
    pub client_key: Option<String>,
    #[serde(default)]
    pub client_key_password: Option<EncryptedBlob>,
    #[serde(default)]
    pub phase2: Option<String>,
    #[serde(default)]
    pub domain_suffix_match: Option<String>,
    #[serde(default)]
    pub password: Option<EncryptedBlob>,
}

// ---------------------------------------------------------------------------
// Encrypt / decrypt helpers
// ---------------------------------------------------------------------------

/// Build AAD for one field of one profile (`kind:id:field`).
pub(crate) fn ad(kind: ProfileKind, id: &Ulid, field: &str) -> Vec<u8> {
    associated_data(kind, id, field)
}

/// Encrypt `plaintext` (as UTF-8 bytes from a `SecretString`) using
/// the per-file cipher + AD constructed from the profile context.
pub(crate) fn encrypt_field(
    cipher: &dyn Cipher,
    kind: ProfileKind,
    id: &Ulid,
    field: &str,
    plaintext: &str,
) -> Result<EncryptedBlob, CipherError> {
    cipher.encrypt(&ad(kind, id, field), plaintext.as_bytes())
}

/// Decrypt one field, expecting UTF-8.
pub(crate) fn decrypt_field(
    cipher: &dyn Cipher,
    kind: ProfileKind,
    id: &Ulid,
    field: &str,
    blob: &EncryptedBlob,
) -> Result<SecretString, CipherError> {
    let pt = cipher.decrypt(&ad(kind, id, field), blob)?;
    let s = String::from_utf8(pt).map_err(|_| CipherError::InvalidUtf8)?;
    Ok(SecretString::new(s))
}

pub(crate) fn encrypt_eap(
    config: &Dot1xEapConfig,
    cipher: &dyn Cipher,
    kind: ProfileKind,
    profile_id: &Ulid,
    field_prefix: &str,
) -> Result<Dot1xEapConfigOnDisk, CipherError> {
    let password = match &config.password {
        Some(s) => Some(encrypt_field(
            cipher,
            kind,
            profile_id,
            &format!("{field_prefix}.password"),
            s.expose_secret(),
        )?),
        None => None,
    };
    let client_key_password = match &config.client_key_password {
        Some(s) => Some(encrypt_field(
            cipher,
            kind,
            profile_id,
            &format!("{field_prefix}.client_key_password"),
            s.expose_secret(),
        )?),
        None => None,
    };
    Ok(Dot1xEapConfigOnDisk {
        eap: config.eap,
        identity: config.identity.clone(),
        anonymous_identity: config.anonymous_identity.clone(),
        ca_cert: config.ca_cert.clone(),
        client_cert: config.client_cert.clone(),
        client_key: config.client_key.clone(),
        client_key_password,
        phase2: config.phase2.clone(),
        domain_suffix_match: config.domain_suffix_match.clone(),
        password,
    })
}

pub(crate) fn decrypt_eap(
    on_disk: Dot1xEapConfigOnDisk,
    cipher: &dyn Cipher,
    kind: ProfileKind,
    profile_id: &Ulid,
    field_prefix: &str,
) -> Result<Dot1xEapConfig, CipherError> {
    let password = match on_disk.password {
        Some(blob) => Some(decrypt_field(
            cipher,
            kind,
            profile_id,
            &format!("{field_prefix}.password"),
            &blob,
        )?),
        None => None,
    };
    let client_key_password = match on_disk.client_key_password {
        Some(blob) => Some(decrypt_field(
            cipher,
            kind,
            profile_id,
            &format!("{field_prefix}.client_key_password"),
            &blob,
        )?),
        None => None,
    };
    Ok(Dot1xEapConfig {
        eap: on_disk.eap,
        identity: on_disk.identity,
        anonymous_identity: on_disk.anonymous_identity,
        ca_cert: on_disk.ca_cert,
        client_cert: on_disk.client_cert,
        client_key: on_disk.client_key,
        client_key_password,
        phase2: on_disk.phase2,
        domain_suffix_match: on_disk.domain_suffix_match,
        password,
    })
}
