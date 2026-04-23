//! Profile types.
//!
//! Each technology has an in-memory form that the backends work
//! with, and (for credential-bearing kinds) a matching on-disk
//! form that is Serialize/Deserialize. The dual-struct pattern
//! keeps [`crate::secret::SecretString`] out of every
//! `Serialize` impl (DD-007 §5.3). GNSS and Bluetooth profiles do
//! not currently carry credential fields and use a single struct.
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

use crate::secret::SecretString;

// ---------------------------------------------------------------------------
// Dot1x / EAP
//
// Shared between Ethernet's `Dot1xSettings` and Wi-Fi's
// `SecurityConfig::Wpa{2,3}Enterprise`. Full field set is from the
// DD-007 §3.3 example TOML plus DD-003 §4.3. Per DD-002's repo
// layout this will eventually live in `nexus-auth-eap` — today it
// lives here so `nexus-profile-store` compiles standalone.
// ---------------------------------------------------------------------------

/// EAP identity + credentials used by both wired (802.1X) and
/// enterprise Wi-Fi. In-memory form with redacted passwords.
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

/// On-disk form of [`Dot1xEapConfig`]. Credential fields are plain
/// strings during phase 2; phase 3 replaces them with
/// `EncryptedBlob`. See DD-007 §5.3.
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
    pub client_key_password: Option<String>,
    #[serde(default)]
    pub phase2: Option<String>,
    #[serde(default)]
    pub domain_suffix_match: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl From<&Dot1xEapConfig> for Dot1xEapConfigOnDisk {
    fn from(value: &Dot1xEapConfig) -> Self {
        Self {
            eap: value.eap,
            identity: value.identity.clone(),
            anonymous_identity: value.anonymous_identity.clone(),
            ca_cert: value.ca_cert.clone(),
            client_cert: value.client_cert.clone(),
            client_key: value.client_key.clone(),
            client_key_password: value
                .client_key_password
                .as_ref()
                .map(|s| s.expose_secret().to_owned()),
            phase2: value.phase2.clone(),
            domain_suffix_match: value.domain_suffix_match.clone(),
            password: value
                .password
                .as_ref()
                .map(|s| s.expose_secret().to_owned()),
        }
    }
}

impl From<Dot1xEapConfigOnDisk> for Dot1xEapConfig {
    fn from(value: Dot1xEapConfigOnDisk) -> Self {
        Self {
            eap: value.eap,
            identity: value.identity,
            anonymous_identity: value.anonymous_identity,
            ca_cert: value.ca_cert,
            client_cert: value.client_cert,
            client_key: value.client_key,
            client_key_password: value.client_key_password.map(SecretString::new),
            phase2: value.phase2,
            domain_suffix_match: value.domain_suffix_match,
            password: value.password.map(SecretString::new),
        }
    }
}
