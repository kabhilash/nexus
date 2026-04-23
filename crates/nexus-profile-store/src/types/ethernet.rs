//! Ethernet profile types. See DD-007 §5.2 and DD-002 §8.2.

use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::{Dot1xEapConfig, Dot1xEapConfigOnDisk, ProfileMetadata};

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
    pub auto_connect: bool,
}

/// 802.1X wired-auth settings. Contains [`Dot1xEapConfig`] which
/// in turn holds [`crate::secret::SecretString`] credential fields,
/// so this type isn't directly serializable — see
/// [`EthernetProfileOnDisk`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dot1xSettings {
    pub enabled: bool,
    pub eap: Dot1xEapConfig,
}

// ---------------------------------------------------------------------------
// On-disk form.
// ---------------------------------------------------------------------------

/// TOML-shaped representation of [`EthernetProfile`]. Credential
/// fields (`eap.password`, `eap.client_key_password`) are plaintext
/// strings during phase 2; phase 3 replaces them with
/// `EncryptedBlob`.
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

impl From<&EthernetProfile> for EthernetProfileOnDisk {
    fn from(value: &EthernetProfile) -> Self {
        Self {
            id: value.id,
            schema_version: value.schema_version,
            metadata: value.metadata.clone(),
            interface: value.interface.clone(),
            dot1x: value.dot1x.as_ref().map(|d| Dot1xSettingsOnDisk {
                enabled: d.enabled,
                eap: Dot1xEapConfigOnDisk::from(&d.eap),
            }),
        }
    }
}

impl From<EthernetProfileOnDisk> for EthernetProfile {
    fn from(value: EthernetProfileOnDisk) -> Self {
        Self {
            id: value.id,
            schema_version: value.schema_version,
            metadata: value.metadata,
            interface: value.interface,
            dot1x: value.dot1x.map(|d| Dot1xSettings {
                enabled: d.enabled,
                eap: Dot1xEapConfig::from(d.eap),
            }),
        }
    }
}
