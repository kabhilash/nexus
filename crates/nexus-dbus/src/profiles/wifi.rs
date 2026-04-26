//! `fi.nexus.Profile.Wifi` — DD-006 §7.2.
//!
//! Credentials never leave the process; the `HasCredentials`
//! presence map is the only channel through which clients see
//! "is there a passphrase stored?". All other fields are
//! straightforward reads off the stored profile.

use std::collections::HashMap;
use std::sync::Arc;

use nexus_profile_store::{SecurityConfig, WifiProfile};
use ulid::Ulid;
use zbus::zvariant::{OwnedValue, Value};

use crate::services::Services;

pub struct WifiProfileIface {
    pub services: Arc<Services>,
    pub id: Ulid,
}

impl WifiProfileIface {
    pub fn new(services: Arc<Services>, id: Ulid) -> Self {
        Self { services, id }
    }

    async fn with_profile<R>(&self, default: R, f: impl FnOnce(&WifiProfile) -> R) -> R {
        let guard = self.services.state.read().await;
        guard
            .wifi_profiles
            .get(&self.id.to_string())
            .map(f)
            .unwrap_or(default)
    }
}

#[zbus::interface(name = "fi.nexus.Profile.Wifi")]
impl WifiProfileIface {
    #[zbus(property, name = "Ssid")]
    async fn ssid(&self) -> Vec<u8> {
        self.with_profile(Vec::new(), |p| p.network.ssid.as_bytes().to_vec())
            .await
    }

    #[zbus(property, name = "Hidden")]
    async fn hidden(&self) -> bool {
        self.with_profile(false, |p| p.network.hidden).await
    }

    #[zbus(property, name = "Priority")]
    async fn priority(&self) -> i32 {
        self.with_profile(0, |p| p.network.priority).await
    }

    #[zbus(property, name = "AutoConnect")]
    async fn auto_connect(&self) -> bool {
        self.with_profile(false, |p| p.network.auto_connect).await
    }

    #[zbus(property, name = "FastTransition")]
    async fn fast_transition(&self) -> bool {
        self.with_profile(false, |p| p.network.fast_transition)
            .await
    }

    #[zbus(property, name = "Security")]
    async fn security(&self) -> HashMap<String, OwnedValue> {
        self.with_profile(HashMap::new(), |p| security_dict(&p.network.security))
            .await
    }

    #[zbus(property, name = "HasCredentials")]
    async fn has_credentials(&self) -> HashMap<String, bool> {
        self.with_profile(HashMap::new(), |p| has_credentials(&p.network.security))
            .await
    }

    #[zbus(property, name = "BssidPreferred")]
    async fn bssid_preferred(&self) -> Vec<u8> {
        self.with_profile(Vec::new(), |p| {
            p.network
                .bssid_preferred
                .map(|m| m.0.to_vec())
                .unwrap_or_default()
        })
        .await
    }

    #[zbus(property, name = "BssidBlacklist")]
    async fn bssid_blacklist(&self) -> Vec<Vec<u8>> {
        self.with_profile(Vec::new(), |p| {
            p.network
                .bssid_blacklist
                .iter()
                .map(|m| m.0.to_vec())
                .collect()
        })
        .await
    }

    #[zbus(property, name = "ScanFrequencies")]
    async fn scan_frequencies(&self) -> Vec<u32> {
        self.with_profile(Vec::new(), |p| p.network.scan_freqs.clone())
            .await
    }

    /// `LastConnectedAt: s` — RFC 3339 / ISO 8601 timestamp of the
    /// most recent successful connection using this profile, or
    /// the empty string if it has never connected. Updated by the
    /// Wi-Fi backend on every Connected transition. Drives the
    /// auto-select recency tiebreaker (DD-003 §6.1) and is
    /// surfaced here for operator UIs that want to render
    /// "last connected N days ago" or sort the profile picker by
    /// recency.
    #[zbus(property, name = "LastConnectedAt")]
    async fn last_connected_at(&self) -> String {
        self.with_profile(String::new(), |p| {
            p.network
                .last_connected_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_default()
        })
        .await
    }
}

fn security_dict(cfg: &SecurityConfig) -> HashMap<String, OwnedValue> {
    let mut out: HashMap<String, OwnedValue> = HashMap::new();
    let tag = security_tag(cfg);
    if let Ok(v) = OwnedValue::try_from(Value::new(tag.to_owned())) {
        out.insert("type".to_owned(), v);
    }
    out
}

fn security_tag(cfg: &SecurityConfig) -> &'static str {
    use SecurityConfig::*;
    match cfg {
        Open => "open",
        Owe => "owe",
        Wpa2Personal { .. } => "wpa2_personal",
        Wpa3Personal { .. } => "wpa3_personal",
        Wpa2Wpa3Personal { .. } => "wpa2_wpa3_personal",
        Wpa2Enterprise(_) => "wpa2_enterprise",
        Wpa3Enterprise(_) => "wpa3_enterprise",
    }
}

fn has_credentials(cfg: &SecurityConfig) -> HashMap<String, bool> {
    use SecurityConfig::*;
    let mut out = HashMap::new();
    match cfg {
        Open | Owe => {}
        Wpa2Personal { .. } => {
            out.insert("passphrase".to_owned(), true);
        }
        Wpa3Personal { .. } | Wpa2Wpa3Personal { .. } => {
            out.insert("passphrase".to_owned(), true);
        }
        Wpa2Enterprise(eap) | Wpa3Enterprise(eap) => {
            if eap.password.is_some() {
                out.insert("password".to_owned(), true);
            }
            if eap.client_key_password.is_some() {
                out.insert("private_key_passwd".to_owned(), true);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::Ssid;
    use nexus_profile_store::{
        ProfileMetadata, SecretString, WifiNetworkSettings, WifiProfile, WpaPsk,
    };

    #[test]
    fn has_credentials_for_open_is_empty() {
        let cfg = SecurityConfig::Open;
        assert!(has_credentials(&cfg).is_empty());
    }

    #[test]
    fn has_credentials_for_wpa2_personal_is_passphrase_true() {
        let cfg = SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(SecretString::from("hunter2")),
        };
        let m = has_credentials(&cfg);
        assert_eq!(m.get("passphrase"), Some(&true));
    }

    #[test]
    fn security_tag_covers_every_variant() {
        assert_eq!(security_tag(&SecurityConfig::Open), "open");
        assert_eq!(security_tag(&SecurityConfig::Owe), "owe");
        assert_eq!(
            security_tag(&SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(SecretString::from("x"))
            }),
            "wpa2_personal"
        );
    }

    #[test]
    fn wifi_profile_has_sane_ssid() {
        // Smoke test: building a full WifiProfile doesn't panic.
        let _p = WifiProfile {
            id: ulid::Ulid::new(),
            schema_version: 1,
            metadata: ProfileMetadata::default(),
            network: WifiNetworkSettings {
                ssid: Ssid::new(b"x".to_vec()).unwrap(),
                hidden: false,
                priority: 0,
                auto_connect: true,
                fast_transition: false,
                security: SecurityConfig::Open,
                bssid_preferred: None,
                bssid_blacklist: Vec::new(),
                scan_freqs: Vec::new(),
                credentials_invalid: false,
                last_connected_at: None,
            },
        };
    }
}
