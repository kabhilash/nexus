//! Wi-Fi profile types. See DD-007 §5.2/§5.3 and DD-003 §§4.3/6.1.
//!
//! Wi-Fi is the crate's motivating example of the dual-struct
//! pattern: `WifiProfile` is the in-memory form (with
//! `SecretString` credentials, not `Serialize`); `WifiProfileOnDisk`
//! is the serializable form. The filesystem store converts between
//! them at the (de)serialize boundary.

use nexus_core::{MacAddr, Ssid};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::{Dot1xEapConfig, Dot1xEapConfigOnDisk, ProfileMetadata};
use crate::secret::SecretString;

// ---------------------------------------------------------------------------
// In-memory form
// ---------------------------------------------------------------------------

/// Wi-Fi profile as the backend sees it. Contains `SecretString`
/// fields; NOT `Serialize`/`Deserialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiProfile {
    pub id: Ulid,
    pub schema_version: u32,
    pub metadata: ProfileMetadata,
    pub network: WifiNetworkSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiNetworkSettings {
    pub ssid: Ssid,
    pub hidden: bool,
    pub priority: i32,
    pub auto_connect: bool,
    pub fast_transition: bool,
    pub security: SecurityConfig,
    pub bssid_preferred: Option<MacAddr>,
    pub bssid_blacklist: Vec<MacAddr>,
    pub scan_freqs: Vec<u32>,
    pub credentials_invalid: bool,
}

/// Per-BSS/per-profile security configuration. Variants carry
/// `SecretString` credentials where present. DD-003 §4.3 is the
/// source of truth for the variant list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityConfig {
    Open,
    /// Opportunistic Wireless Encryption; encrypted open network.
    Owe,
    Wpa2Personal {
        psk: WpaPsk,
    },
    Wpa3Personal {
        passphrase: SecretString,
    },
    Wpa2Wpa3Personal {
        passphrase: SecretString,
    },
    Wpa2Enterprise(Dot1xEapConfig),
    Wpa3Enterprise(Dot1xEapConfig),
}

/// WPA2/WPA3-Personal pre-shared-key form. Passphrases are
/// `SecretString`; raw PSKs are 32 bytes of already-derived key
/// material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WpaPsk {
    Passphrase(SecretString),
    RawPsk([u8; 32]),
}

// ---------------------------------------------------------------------------
// On-disk form
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiProfileOnDisk {
    pub id: Ulid,
    pub schema_version: u32,
    #[serde(default)]
    pub metadata: ProfileMetadata,
    pub network: WifiNetworkSettingsOnDisk,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WifiNetworkSettingsOnDisk {
    pub ssid: Ssid,
    #[serde(default)]
    pub hidden: bool,
    pub priority: i32,
    pub auto_connect: bool,
    #[serde(default)]
    pub fast_transition: bool,
    pub security: SecurityConfigOnDisk,
    #[serde(default)]
    pub bssid_preferred: Option<MacAddr>,
    #[serde(default)]
    pub bssid_blacklist: Vec<MacAddr>,
    #[serde(default)]
    pub scan_freqs: Vec<u32>,
    #[serde(default)]
    pub credentials_invalid: bool,
}

/// On-disk encoding of [`SecurityConfig`]. The `type` tag
/// discriminates between variants and keeps the TOML shape
/// compatible with the example in DD-007 §3.3.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SecurityConfigOnDisk {
    Open,
    Owe,
    Wpa2Personal { psk: WpaPskOnDisk },
    Wpa3Personal { passphrase: String },
    Wpa2Wpa3Personal { passphrase: String },
    Wpa2Enterprise { eap: Dot1xEapConfigOnDisk },
    Wpa3Enterprise { eap: Dot1xEapConfigOnDisk },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WpaPskOnDisk {
    Passphrase { passphrase: String },
    Raw { psk_hex: String },
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

impl From<&WifiProfile> for WifiProfileOnDisk {
    fn from(value: &WifiProfile) -> Self {
        Self {
            id: value.id,
            schema_version: value.schema_version,
            metadata: value.metadata.clone(),
            network: WifiNetworkSettingsOnDisk::from(&value.network),
        }
    }
}

impl From<WifiProfileOnDisk> for WifiProfile {
    fn from(value: WifiProfileOnDisk) -> Self {
        Self {
            id: value.id,
            schema_version: value.schema_version,
            metadata: value.metadata,
            network: WifiNetworkSettings::from(value.network),
        }
    }
}

impl From<&WifiNetworkSettings> for WifiNetworkSettingsOnDisk {
    fn from(value: &WifiNetworkSettings) -> Self {
        Self {
            ssid: value.ssid.clone(),
            hidden: value.hidden,
            priority: value.priority,
            auto_connect: value.auto_connect,
            fast_transition: value.fast_transition,
            security: SecurityConfigOnDisk::from(&value.security),
            bssid_preferred: value.bssid_preferred,
            bssid_blacklist: value.bssid_blacklist.clone(),
            scan_freqs: value.scan_freqs.clone(),
            credentials_invalid: value.credentials_invalid,
        }
    }
}

impl From<WifiNetworkSettingsOnDisk> for WifiNetworkSettings {
    fn from(value: WifiNetworkSettingsOnDisk) -> Self {
        Self {
            ssid: value.ssid,
            hidden: value.hidden,
            priority: value.priority,
            auto_connect: value.auto_connect,
            fast_transition: value.fast_transition,
            security: SecurityConfig::from(value.security),
            bssid_preferred: value.bssid_preferred,
            bssid_blacklist: value.bssid_blacklist,
            scan_freqs: value.scan_freqs,
            credentials_invalid: value.credentials_invalid,
        }
    }
}

impl From<&SecurityConfig> for SecurityConfigOnDisk {
    fn from(value: &SecurityConfig) -> Self {
        match value {
            SecurityConfig::Open => SecurityConfigOnDisk::Open,
            SecurityConfig::Owe => SecurityConfigOnDisk::Owe,
            SecurityConfig::Wpa2Personal { psk } => SecurityConfigOnDisk::Wpa2Personal {
                psk: WpaPskOnDisk::from(psk),
            },
            SecurityConfig::Wpa3Personal { passphrase } => SecurityConfigOnDisk::Wpa3Personal {
                passphrase: passphrase.expose_secret().to_owned(),
            },
            SecurityConfig::Wpa2Wpa3Personal { passphrase } => {
                SecurityConfigOnDisk::Wpa2Wpa3Personal {
                    passphrase: passphrase.expose_secret().to_owned(),
                }
            }
            SecurityConfig::Wpa2Enterprise(eap) => SecurityConfigOnDisk::Wpa2Enterprise {
                eap: Dot1xEapConfigOnDisk::from(eap),
            },
            SecurityConfig::Wpa3Enterprise(eap) => SecurityConfigOnDisk::Wpa3Enterprise {
                eap: Dot1xEapConfigOnDisk::from(eap),
            },
        }
    }
}

impl From<SecurityConfigOnDisk> for SecurityConfig {
    fn from(value: SecurityConfigOnDisk) -> Self {
        match value {
            SecurityConfigOnDisk::Open => SecurityConfig::Open,
            SecurityConfigOnDisk::Owe => SecurityConfig::Owe,
            SecurityConfigOnDisk::Wpa2Personal { psk } => SecurityConfig::Wpa2Personal {
                psk: WpaPsk::from(psk),
            },
            SecurityConfigOnDisk::Wpa3Personal { passphrase } => SecurityConfig::Wpa3Personal {
                passphrase: SecretString::new(passphrase),
            },
            SecurityConfigOnDisk::Wpa2Wpa3Personal { passphrase } => {
                SecurityConfig::Wpa2Wpa3Personal {
                    passphrase: SecretString::new(passphrase),
                }
            }
            SecurityConfigOnDisk::Wpa2Enterprise { eap } => {
                SecurityConfig::Wpa2Enterprise(Dot1xEapConfig::from(eap))
            }
            SecurityConfigOnDisk::Wpa3Enterprise { eap } => {
                SecurityConfig::Wpa3Enterprise(Dot1xEapConfig::from(eap))
            }
        }
    }
}

impl From<&WpaPsk> for WpaPskOnDisk {
    fn from(value: &WpaPsk) -> Self {
        match value {
            WpaPsk::Passphrase(s) => WpaPskOnDisk::Passphrase {
                passphrase: s.expose_secret().to_owned(),
            },
            WpaPsk::RawPsk(bytes) => WpaPskOnDisk::Raw {
                psk_hex: hex_encode(bytes),
            },
        }
    }
}

impl From<WpaPskOnDisk> for WpaPsk {
    fn from(value: WpaPskOnDisk) -> Self {
        match value {
            WpaPskOnDisk::Passphrase { passphrase } => {
                WpaPsk::Passphrase(SecretString::new(passphrase))
            }
            WpaPskOnDisk::Raw { psk_hex } => WpaPsk::RawPsk(hex_decode_fixed(&psk_hex)),
        }
    }
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hex_decode_fixed(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate().take(32) {
        if let (Some(hi), Some(lo)) = (
            chunk.first().and_then(|b| from_hex(*b)),
            chunk.get(1).and_then(|b| from_hex(*b)),
        ) {
            out[i] = (hi << 4) | lo;
        }
    }
    out
}

fn from_hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
