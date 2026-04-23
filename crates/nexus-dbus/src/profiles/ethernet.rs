//! `fi.nexus.Profile.Ethernet` — DD-006 §7.3.

use std::collections::HashMap;
use std::sync::Arc;

use nexus_profile_store::{EapMethod, EthernetProfile};
use ulid::Ulid;

use crate::services::Services;

pub struct EthernetProfileIface {
    pub services: Arc<Services>,
    pub id: Ulid,
}

impl EthernetProfileIface {
    pub fn new(services: Arc<Services>, id: Ulid) -> Self {
        Self { services, id }
    }

    async fn with_profile<R>(&self, default: R, f: impl FnOnce(&EthernetProfile) -> R) -> R {
        let guard = self.services.state.read().await;
        guard
            .ethernet_profiles
            .get(&self.id.to_string())
            .map(f)
            .unwrap_or(default)
    }
}

#[zbus::interface(name = "fi.nexus.Profile.Ethernet")]
impl EthernetProfileIface {
    #[zbus(property, name = "Ifname")]
    async fn ifname(&self) -> String {
        self.with_profile(String::new(), |p| p.interface.name.clone())
            .await
    }

    #[zbus(property, name = "AutoConnect")]
    async fn auto_connect(&self) -> bool {
        self.with_profile(false, |p| p.interface.auto_connect).await
    }

    #[zbus(property, name = "Dot1xEnabled")]
    async fn dot1x_enabled(&self) -> bool {
        self.with_profile(false, |p| {
            p.dot1x.as_ref().map(|d| d.enabled).unwrap_or(false)
        })
        .await
    }

    #[zbus(property, name = "Dot1xEap")]
    async fn dot1x_eap(&self) -> String {
        self.with_profile(String::new(), |p| {
            p.dot1x
                .as_ref()
                .map(|d| eap_method_label(d.eap.eap).to_owned())
                .unwrap_or_default()
        })
        .await
    }

    #[zbus(property, name = "HasCredentials")]
    async fn has_credentials(&self) -> HashMap<String, bool> {
        self.with_profile(HashMap::new(), |p| {
            let mut out = HashMap::new();
            if let Some(d) = &p.dot1x {
                if d.eap.password.is_some() {
                    out.insert("password".to_owned(), true);
                }
                if d.eap.client_key_password.is_some() {
                    out.insert("private_key_passwd".to_owned(), true);
                }
            }
            out
        })
        .await
    }
}

fn eap_method_label(m: EapMethod) -> &'static str {
    use EapMethod::*;
    match m {
        Peap => "PEAP",
        Ttls => "TTLS",
        Tls => "TLS",
        PwdMschapv2 => "PWD_MSCHAPV2",
        Leap => "LEAP",
        Fast => "FAST",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_profile_store::{Dot1xEapConfig, Dot1xSettings, EapMethod};

    #[test]
    fn eap_method_labels() {
        assert_eq!(eap_method_label(EapMethod::Tls), "TLS");
        assert_eq!(eap_method_label(EapMethod::Peap), "PEAP");
    }

    #[test]
    fn dot1x_settings_build() {
        let _d = Dot1xSettings {
            enabled: true,
            eap: Dot1xEapConfig {
                eap: EapMethod::Tls,
                identity: "alice".into(),
                anonymous_identity: None,
                ca_cert: None,
                client_cert: None,
                client_key: None,
                client_key_password: None,
                phase2: None,
                domain_suffix_match: None,
                password: None,
            },
        };
    }
}
