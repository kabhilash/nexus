//! Wi-Fi profile types. See DD-007 §5.2/§5.3 and DD-003 §§4.3/6.1.
//!
//! Wi-Fi is the crate's motivating example of the dual-struct
//! pattern: `WifiProfile` is the in-memory form (with
//! `SecretString` credentials); `WifiProfileOnDisk` is the
//! serializable form with `EncryptedBlob` credentials. The
//! filesystem store converts between them at the (de)serialize
//! boundary via [`encrypt_wifi`] / [`decrypt_wifi`].

use nexus_core::{MacAddr, Ssid};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::{
    Dot1xEapConfig, Dot1xEapConfigOnDisk, ProfileMetadata, decrypt_eap, decrypt_field, encrypt_eap,
    encrypt_field,
};
use crate::crypto::{Cipher, CipherError, EncryptedBlob};
use crate::secret::SecretString;
use crate::trait_def::ProfileKind;

// ---------------------------------------------------------------------------
// In-memory form
// ---------------------------------------------------------------------------

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
    /// Timestamp of the most recent successful Wi-Fi connection
    /// using this profile (i.e., when the supplicant reported
    /// `Connected` and the backend emitted `WifiLinkReady`).
    /// Used by [`crate::select_network`](../../../nexus-wifi/src/select.rs)
    /// as a tiebreaker between profiles of equal `priority` and
    /// without a preferred-BSSID hit, so a daemon coming back up
    /// prefers the network the operator was most recently using.
    /// `None` for profiles that have never successfully connected.
    pub last_connected_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityConfig {
    Open,
    Owe,
    Wpa2Personal { psk: WpaPsk },
    Wpa3Personal { passphrase: SecretString },
    Wpa2Wpa3Personal { passphrase: SecretString },
    Wpa2Enterprise(Dot1xEapConfig),
    Wpa3Enterprise(Dot1xEapConfig),
}

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
    /// See [`WifiNetworkSettings::last_connected_at`]. `#[serde(default)]`
    /// so older on-disk profiles round-trip without bumping
    /// `schema_version`.
    #[serde(default)]
    pub last_connected_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SecurityConfigOnDisk {
    Open,
    Owe,
    Wpa2Personal { psk: WpaPskOnDisk },
    Wpa3Personal { passphrase: EncryptedBlob },
    Wpa2Wpa3Personal { passphrase: EncryptedBlob },
    Wpa2Enterprise { eap: Dot1xEapConfigOnDisk },
    Wpa3Enterprise { eap: Dot1xEapConfigOnDisk },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WpaPskOnDisk {
    Passphrase { passphrase: EncryptedBlob },
    Raw { psk: EncryptedBlob },
}

// ---------------------------------------------------------------------------
// Encrypt / decrypt
// ---------------------------------------------------------------------------

/// Encrypt every credential in `profile` under `cipher` and
/// construct the on-disk shape.
pub fn encrypt_wifi(
    profile: &WifiProfile,
    cipher: &dyn Cipher,
) -> Result<WifiProfileOnDisk, CipherError> {
    let security = encrypt_security(&profile.network.security, cipher, &profile.id)?;
    Ok(WifiProfileOnDisk {
        id: profile.id,
        schema_version: profile.schema_version,
        metadata: profile.metadata.clone(),
        network: WifiNetworkSettingsOnDisk {
            ssid: profile.network.ssid.clone(),
            hidden: profile.network.hidden,
            priority: profile.network.priority,
            auto_connect: profile.network.auto_connect,
            fast_transition: profile.network.fast_transition,
            security,
            bssid_preferred: profile.network.bssid_preferred,
            bssid_blacklist: profile.network.bssid_blacklist.clone(),
            scan_freqs: profile.network.scan_freqs.clone(),
            credentials_invalid: profile.network.credentials_invalid,
            last_connected_at: profile.network.last_connected_at,
        },
    })
}

/// Decrypt an on-disk Wi-Fi profile into the in-memory shape.
pub fn decrypt_wifi(
    on_disk: WifiProfileOnDisk,
    cipher: &dyn Cipher,
) -> Result<WifiProfile, CipherError> {
    let id = on_disk.id;
    let security = decrypt_security(on_disk.network.security, cipher, &id)?;
    Ok(WifiProfile {
        id,
        schema_version: on_disk.schema_version,
        metadata: on_disk.metadata,
        network: WifiNetworkSettings {
            ssid: on_disk.network.ssid,
            hidden: on_disk.network.hidden,
            priority: on_disk.network.priority,
            auto_connect: on_disk.network.auto_connect,
            fast_transition: on_disk.network.fast_transition,
            security,
            bssid_preferred: on_disk.network.bssid_preferred,
            bssid_blacklist: on_disk.network.bssid_blacklist,
            scan_freqs: on_disk.network.scan_freqs,
            credentials_invalid: on_disk.network.credentials_invalid,
            last_connected_at: on_disk.network.last_connected_at,
        },
    })
}

// Field-path prefixes. Each variant uses its own prefix so a
// ciphertext moved between `SecurityConfig` variants would fail the
// AD hash check.
const WPA2_PERSONAL_PSK_PASSPHRASE: &str = "network.security.wpa2_personal.psk.passphrase";
const WPA2_PERSONAL_PSK_RAW: &str = "network.security.wpa2_personal.psk.raw";
const WPA3_PERSONAL_PASSPHRASE: &str = "network.security.wpa3_personal.passphrase";
const WPA2_WPA3_PERSONAL_PASSPHRASE: &str = "network.security.wpa2_wpa3_personal.passphrase";
const WPA2_ENTERPRISE_EAP: &str = "network.security.wpa2_enterprise.eap";
const WPA3_ENTERPRISE_EAP: &str = "network.security.wpa3_enterprise.eap";

fn encrypt_security(
    security: &SecurityConfig,
    cipher: &dyn Cipher,
    id: &Ulid,
) -> Result<SecurityConfigOnDisk, CipherError> {
    Ok(match security {
        SecurityConfig::Open => SecurityConfigOnDisk::Open,
        SecurityConfig::Owe => SecurityConfigOnDisk::Owe,
        SecurityConfig::Wpa2Personal { psk } => SecurityConfigOnDisk::Wpa2Personal {
            psk: match psk {
                WpaPsk::Passphrase(s) => WpaPskOnDisk::Passphrase {
                    passphrase: encrypt_field(
                        cipher,
                        ProfileKind::Wifi,
                        id,
                        WPA2_PERSONAL_PSK_PASSPHRASE,
                        s.expose_secret(),
                    )?,
                },
                WpaPsk::RawPsk(bytes) => WpaPskOnDisk::Raw {
                    psk: encrypt_field(
                        cipher,
                        ProfileKind::Wifi,
                        id,
                        WPA2_PERSONAL_PSK_RAW,
                        &hex_encode(bytes),
                    )?,
                },
            },
        },
        SecurityConfig::Wpa3Personal { passphrase } => SecurityConfigOnDisk::Wpa3Personal {
            passphrase: encrypt_field(
                cipher,
                ProfileKind::Wifi,
                id,
                WPA3_PERSONAL_PASSPHRASE,
                passphrase.expose_secret(),
            )?,
        },
        SecurityConfig::Wpa2Wpa3Personal { passphrase } => SecurityConfigOnDisk::Wpa2Wpa3Personal {
            passphrase: encrypt_field(
                cipher,
                ProfileKind::Wifi,
                id,
                WPA2_WPA3_PERSONAL_PASSPHRASE,
                passphrase.expose_secret(),
            )?,
        },
        SecurityConfig::Wpa2Enterprise(eap) => SecurityConfigOnDisk::Wpa2Enterprise {
            eap: encrypt_eap(eap, cipher, ProfileKind::Wifi, id, WPA2_ENTERPRISE_EAP)?,
        },
        SecurityConfig::Wpa3Enterprise(eap) => SecurityConfigOnDisk::Wpa3Enterprise {
            eap: encrypt_eap(eap, cipher, ProfileKind::Wifi, id, WPA3_ENTERPRISE_EAP)?,
        },
    })
}

fn decrypt_security(
    on_disk: SecurityConfigOnDisk,
    cipher: &dyn Cipher,
    id: &Ulid,
) -> Result<SecurityConfig, CipherError> {
    Ok(match on_disk {
        SecurityConfigOnDisk::Open => SecurityConfig::Open,
        SecurityConfigOnDisk::Owe => SecurityConfig::Owe,
        SecurityConfigOnDisk::Wpa2Personal { psk } => SecurityConfig::Wpa2Personal {
            psk: match psk {
                WpaPskOnDisk::Passphrase { passphrase } => WpaPsk::Passphrase(decrypt_field(
                    cipher,
                    ProfileKind::Wifi,
                    id,
                    WPA2_PERSONAL_PSK_PASSPHRASE,
                    &passphrase,
                )?),
                WpaPskOnDisk::Raw { psk } => {
                    let hex =
                        decrypt_field(cipher, ProfileKind::Wifi, id, WPA2_PERSONAL_PSK_RAW, &psk)?;
                    WpaPsk::RawPsk(hex_decode_32(hex.expose_secret())?)
                }
            },
        },
        SecurityConfigOnDisk::Wpa3Personal { passphrase } => SecurityConfig::Wpa3Personal {
            passphrase: decrypt_field(
                cipher,
                ProfileKind::Wifi,
                id,
                WPA3_PERSONAL_PASSPHRASE,
                &passphrase,
            )?,
        },
        SecurityConfigOnDisk::Wpa2Wpa3Personal { passphrase } => SecurityConfig::Wpa2Wpa3Personal {
            passphrase: decrypt_field(
                cipher,
                ProfileKind::Wifi,
                id,
                WPA2_WPA3_PERSONAL_PASSPHRASE,
                &passphrase,
            )?,
        },
        SecurityConfigOnDisk::Wpa2Enterprise { eap } => SecurityConfig::Wpa2Enterprise(
            decrypt_eap(eap, cipher, ProfileKind::Wifi, id, WPA2_ENTERPRISE_EAP)?,
        ),
        SecurityConfigOnDisk::Wpa3Enterprise { eap } => SecurityConfig::Wpa3Enterprise(
            decrypt_eap(eap, cipher, ProfileKind::Wifi, id, WPA3_ENTERPRISE_EAP)?,
        ),
    })
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn hex_decode_32(s: &str) -> Result<[u8; 32], CipherError> {
    if s.len() != 64 {
        return Err(CipherError::InvalidUtf8); // misuse: 32 bytes = 64 hex chars
    }
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate().take(32) {
        let hi = from_hex(chunk[0]).ok_or(CipherError::InvalidUtf8)?;
        let lo = from_hex(chunk[1]).ok_or(CipherError::InvalidUtf8)?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn from_hex(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
