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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::ChaChaCipher;
    use crate::secret::SecretString;
    use crate::types::{Dot1xEapConfig, EapMethod, ProfileMetadata};
    use nexus_core::{MacAddr, Ssid};

    fn cipher() -> ChaChaCipher {
        ChaChaCipher::new([0x42; 32])
    }

    fn profile_with(security: SecurityConfig) -> WifiProfile {
        WifiProfile {
            id: Ulid::from_parts(0x0123_4567_89AB, 0xCDEF_0123_4567_89AB_CDEF_0123),
            schema_version: 1,
            metadata: ProfileMetadata::default(),
            network: WifiNetworkSettings {
                ssid: Ssid::new(b"nexus-net".to_vec()).unwrap(),
                hidden: false,
                priority: 10,
                auto_connect: true,
                fast_transition: false,
                security,
                bssid_preferred: None,
                bssid_blacklist: Vec::new(),
                scan_freqs: Vec::new(),
                credentials_invalid: false,
                last_connected_at: None,
            },
        }
    }

    fn round_trip(profile: WifiProfile) -> WifiProfile {
        let on_disk = encrypt_wifi(&profile, &cipher()).expect("encrypt");
        decrypt_wifi(on_disk, &cipher()).expect("decrypt")
    }

    fn sample_eap() -> Dot1xEapConfig {
        Dot1xEapConfig {
            eap: EapMethod::Peap,
            identity: "user@corp.example.com".into(),
            anonymous_identity: Some("anon@corp.example.com".into()),
            ca_cert: Some("/etc/nexus/ca.pem".into()),
            client_cert: None,
            client_key: None,
            client_key_password: None,
            phase2: Some("auth=MSCHAPV2".into()),
            domain_suffix_match: Some("corp.example.com".into()),
            password: Some(SecretString::from("hunter2")),
        }
    }

    #[test]
    fn roundtrip_open() {
        let p = profile_with(SecurityConfig::Open);
        assert_eq!(round_trip(p.clone()), p);
    }

    #[test]
    fn roundtrip_owe() {
        let p = profile_with(SecurityConfig::Owe);
        assert_eq!(round_trip(p.clone()), p);
    }

    #[test]
    fn roundtrip_wpa2_personal_passphrase() {
        let p = profile_with(SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(SecretString::from("correct horse battery staple")),
        });
        let back = round_trip(p.clone());
        match (&p.network.security, &back.network.security) {
            (
                SecurityConfig::Wpa2Personal {
                    psk: WpaPsk::Passphrase(a),
                },
                SecurityConfig::Wpa2Personal {
                    psk: WpaPsk::Passphrase(b),
                },
            ) => assert_eq!(a.expose_secret(), b.expose_secret()),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_wpa2_personal_raw_psk() {
        let raw: [u8; 32] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD,
            0xEE, 0xFF, 0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xA0, 0xB0, 0xC0,
            0xD0, 0xE0, 0xF0, 0x01,
        ];
        let p = profile_with(SecurityConfig::Wpa2Personal {
            psk: WpaPsk::RawPsk(raw),
        });
        let back = round_trip(p.clone());
        match back.network.security {
            SecurityConfig::Wpa2Personal {
                psk: WpaPsk::RawPsk(bytes),
            } => assert_eq!(bytes, raw),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_wpa3_personal() {
        let p = profile_with(SecurityConfig::Wpa3Personal {
            passphrase: SecretString::from("sae-pass"),
        });
        let back = round_trip(p);
        match back.network.security {
            SecurityConfig::Wpa3Personal { passphrase } => {
                assert_eq!(passphrase.expose_secret(), "sae-pass");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_wpa2_wpa3_personal() {
        let p = profile_with(SecurityConfig::Wpa2Wpa3Personal {
            passphrase: SecretString::from("mixed-mode"),
        });
        let back = round_trip(p);
        match back.network.security {
            SecurityConfig::Wpa2Wpa3Personal { passphrase } => {
                assert_eq!(passphrase.expose_secret(), "mixed-mode");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_wpa2_enterprise_with_password() {
        let p = profile_with(SecurityConfig::Wpa2Enterprise(sample_eap()));
        let back = round_trip(p);
        match back.network.security {
            SecurityConfig::Wpa2Enterprise(eap) => {
                assert_eq!(eap.identity, "user@corp.example.com");
                assert_eq!(eap.password.unwrap().expose_secret(), "hunter2");
                assert!(eap.client_key_password.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roundtrip_wpa3_enterprise_tls_with_client_key_password() {
        // EAP-TLS path: cert paths set, password=None, client_key_password=Some.
        // Exercises the *other* Some/None split in encrypt_eap/decrypt_eap.
        let eap = Dot1xEapConfig {
            eap: EapMethod::Tls,
            identity: "device-cert".into(),
            anonymous_identity: None,
            ca_cert: Some("/etc/nexus/ca.pem".into()),
            client_cert: Some("/etc/nexus/client.pem".into()),
            client_key: Some("/etc/nexus/client.key".into()),
            client_key_password: Some(SecretString::from("keypass")),
            phase2: None,
            domain_suffix_match: None,
            password: None,
        };
        let p = profile_with(SecurityConfig::Wpa3Enterprise(eap));
        let back = round_trip(p);
        match back.network.security {
            SecurityConfig::Wpa3Enterprise(eap) => {
                assert!(eap.password.is_none());
                assert_eq!(eap.client_key_password.unwrap().expose_secret(), "keypass");
                assert_eq!(eap.client_cert.as_deref(), Some("/etc/nexus/client.pem"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // --- AAD / field-prefix invariants ----------------------------------

    #[test]
    fn aad_field_prefix_distinguishes_wpa2_from_wpa3_personal() {
        // The comment at the top of the prefix block calls out this
        // exact property: a ciphertext moved between SecurityConfig
        // variants must fail the AD-hash check.
        let p = profile_with(SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(SecretString::from("pw")),
        });
        let on_disk = encrypt_wifi(&p, &cipher()).unwrap();
        let blob = match on_disk.network.security {
            SecurityConfigOnDisk::Wpa2Personal {
                psk: WpaPskOnDisk::Passphrase { passphrase },
            } => passphrase,
            _ => unreachable!(),
        };

        let mut tampered = encrypt_wifi(&p, &cipher()).unwrap();
        tampered.network.security = SecurityConfigOnDisk::Wpa3Personal { passphrase: blob };

        let err = decrypt_wifi(tampered, &cipher()).unwrap_err();
        assert!(matches!(err, CipherError::AdHashMismatch), "got {err:?}");
    }

    #[test]
    fn aad_field_prefix_distinguishes_passphrase_from_raw_psk() {
        let p = profile_with(SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(SecretString::from("pw")),
        });
        let on_disk = encrypt_wifi(&p, &cipher()).unwrap();
        let blob = match on_disk.network.security {
            SecurityConfigOnDisk::Wpa2Personal {
                psk: WpaPskOnDisk::Passphrase { passphrase },
            } => passphrase,
            _ => unreachable!(),
        };

        let mut tampered = encrypt_wifi(&p, &cipher()).unwrap();
        tampered.network.security = SecurityConfigOnDisk::Wpa2Personal {
            psk: WpaPskOnDisk::Raw { psk: blob },
        };

        let err = decrypt_wifi(tampered, &cipher()).unwrap_err();
        assert!(matches!(err, CipherError::AdHashMismatch), "got {err:?}");
    }

    #[test]
    fn aad_includes_profile_id() {
        // Encrypt under id A, mutate the on-disk struct's id to B
        // before decrypt. The AAD recomputed during decrypt embeds
        // the new id, which won't match the stored ad_hash.
        let p = profile_with(SecurityConfig::Wpa3Personal {
            passphrase: SecretString::from("pw"),
        });
        let mut on_disk = encrypt_wifi(&p, &cipher()).unwrap();
        on_disk.id = Ulid::from_parts(0xDEAD_BEEF, 0xFEED_FACE_CAFE_BABE_F00D_DEAD);

        let err = decrypt_wifi(on_disk, &cipher()).unwrap_err();
        assert!(matches!(err, CipherError::AdHashMismatch), "got {err:?}");
    }

    // --- hex helpers ----------------------------------------------------

    #[test]
    fn hex_decode_32_rejects_short_input() {
        assert!(matches!(
            hex_decode_32("abcd"),
            Err(CipherError::InvalidUtf8)
        ));
    }

    #[test]
    fn hex_decode_32_rejects_non_hex_chars() {
        // 63 valid hex chars + one 'z' = 64 total chars, fails inside from_hex.
        let mut s = "a".repeat(63);
        s.push('z');
        assert!(matches!(hex_decode_32(&s), Err(CipherError::InvalidUtf8)));
    }

    #[test]
    fn hex_decode_32_accepts_uppercase() {
        let bytes = hex_decode_32(&"FF".repeat(32)).unwrap();
        assert_eq!(bytes, [0xFFu8; 32]);
    }

    // --- serde TOML round-trip -----------------------------------------

    #[test]
    fn wifi_on_disk_toml_round_trip_preserves_every_field() {
        let p = profile_with(SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(SecretString::from("pw")),
        });
        let mut on_disk = encrypt_wifi(&p, &cipher()).unwrap();
        on_disk.network.last_connected_at =
            Some(chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap());
        on_disk.network.bssid_preferred = Some(MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]));
        on_disk.network.bssid_blacklist = vec![MacAddr([0x11; 6])];
        on_disk.network.scan_freqs = vec![2412, 5180];

        let text = toml::to_string(&on_disk).unwrap();
        assert!(text.contains("type = \"wpa2_personal\""), "got:\n{text}");
        let back: WifiProfileOnDisk = toml::from_str(&text).unwrap();

        // Decrypt both sides and compare the in-memory shape, since
        // EncryptedBlob nonces/ct match by-value.
        let original = decrypt_wifi(on_disk, &cipher()).unwrap();
        let reloaded = decrypt_wifi(back, &cipher()).unwrap();
        assert_eq!(original, reloaded);
    }

    #[test]
    fn legacy_on_disk_without_last_connected_at_loads() {
        // Build a fresh on-disk profile, then strip the
        // last_connected_at line from the rendered TOML to simulate
        // an older profile predating that field.
        let p = profile_with(SecurityConfig::Open);
        let on_disk = encrypt_wifi(&p, &cipher()).unwrap();
        let text = toml::to_string(&on_disk).unwrap();
        // Open profiles don't have credentials, so the only optional
        // field present should be last_connected_at = ""... but it's
        // absent here because the original profile had None. The
        // serde(default) attribute is what we're really pinning:
        // a literal TOML missing the key must deserialize cleanly.
        let stripped: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("last_connected_at"))
            .collect::<Vec<_>>()
            .join("\n");
        let back: WifiProfileOnDisk = toml::from_str(&stripped).unwrap();
        assert!(back.network.last_connected_at.is_none());
    }
}
