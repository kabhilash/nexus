//! Ethernet profile types. See DD-007 §5.2 and DD-002 §8.2.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::{Dot1xEapConfig, Dot1xEapConfigOnDisk, ProfileMetadata, decrypt_eap, encrypt_eap};
use crate::crypto::{Cipher, CipherError};
use crate::trait_def::ProfileKind;

/// Per-interface Ethernet settings. Single file per interface on
/// disk; the filename is the interface name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EthernetProfile {
    pub id: Ulid,
    pub schema_version: u32,
    pub metadata: ProfileMetadata,
    pub interface: EthInterfaceSettings,
    pub dot1x: Option<Dot1xSettings>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EthInterfaceSettings {
    pub name: String,
    /// Surfaced via `fi.nexus.Profile.Ethernet.AutoConnect`
    /// (DD-006 §7.3). **Currently a no-op for the Ethernet Backend
    /// — DD-002 §3 has no `Connect()` method, so the carrier-up
    /// path always proceeds to LinkReady regardless of this flag.**
    /// Wi-Fi consults the analogous flag during scan-result
    /// selection (`nexus-wifi/src/select.rs`); ethernet has no
    /// equivalent gating point today. Callers that need
    /// "registered but quiescent" semantics should set
    /// `dot1x.enabled = true` with an unreachable RADIUS server, or
    /// remove the profile entirely.
    pub auto_connect: bool,
}

/// 802.1X wired-auth settings. Contains [`Dot1xEapConfig`] which
/// in turn holds `SecretString` credentials; not directly Serialize.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dot1xSettings {
    pub enabled: bool,
    pub eap: Dot1xEapConfig,
}

// ---------------------------------------------------------------------------
// On-disk form
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EthernetProfileOnDisk {
    pub id: Ulid,
    pub schema_version: u32,
    #[serde(default)]
    pub metadata: ProfileMetadata,
    pub interface: EthInterfaceSettings,
    #[serde(default)]
    pub dot1x: Option<Dot1xSettingsOnDisk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dot1xSettingsOnDisk {
    pub enabled: bool,
    pub eap: Dot1xEapConfigOnDisk,
}

// ---------------------------------------------------------------------------
// Encrypt / decrypt
// ---------------------------------------------------------------------------

/// Encrypt every credential in `profile` under `cipher` and
/// construct the on-disk shape.
pub fn encrypt_ethernet(
    profile: &EthernetProfile,
    cipher: &dyn Cipher,
) -> Result<EthernetProfileOnDisk, CipherError> {
    let dot1x = match &profile.dot1x {
        Some(d) => Some(Dot1xSettingsOnDisk {
            enabled: d.enabled,
            eap: encrypt_eap(
                &d.eap,
                cipher,
                ProfileKind::Ethernet,
                &profile.id,
                "dot1x.eap",
            )?,
        }),
        None => None,
    };
    Ok(EthernetProfileOnDisk {
        id: profile.id,
        schema_version: profile.schema_version,
        metadata: profile.metadata.clone(),
        interface: profile.interface.clone(),
        dot1x,
    })
}

/// Decrypt an on-disk Ethernet profile into the in-memory shape.
pub fn decrypt_ethernet(
    on_disk: EthernetProfileOnDisk,
    cipher: &dyn Cipher,
) -> Result<EthernetProfile, CipherError> {
    let dot1x = match on_disk.dot1x {
        Some(d) => Some(Dot1xSettings {
            enabled: d.enabled,
            eap: decrypt_eap(
                d.eap,
                cipher,
                ProfileKind::Ethernet,
                &on_disk.id,
                "dot1x.eap",
            )?,
        }),
        None => None,
    };
    Ok(EthernetProfile {
        id: on_disk.id,
        schema_version: on_disk.schema_version,
        metadata: on_disk.metadata,
        interface: on_disk.interface,
        dot1x,
    })
}
