//! `fi.nexus.Ethernet` — DD-006 §6.2.

use std::sync::Arc;

use crate::services::Services;
use crate::state::InterfaceKindData;

pub struct EthernetIface {
    pub services: Arc<Services>,
    pub ifname: String,
}

impl EthernetIface {
    pub fn new(services: Arc<Services>, ifname: impl Into<String>) -> Self {
        Self {
            services,
            ifname: ifname.into(),
        }
    }

    async fn with_cache<R>(
        &self,
        default: R,
        f: impl FnOnce(&crate::state::EthernetState) -> R,
    ) -> R {
        let guard = self.services.state.read().await;
        match guard.interfaces.get(&self.ifname).map(|e| &e.kind_data) {
            Some(InterfaceKindData::Ethernet(c)) => f(c),
            _ => default,
        }
    }
}

#[zbus::interface(name = "fi.nexus.Ethernet")]
impl EthernetIface {
    #[zbus(property, name = "State")]
    async fn state(&self) -> String {
        self.with_cache(String::new(), |c| c.state.clone()).await
    }

    #[zbus(property, name = "AuthBackend")]
    async fn auth_backend(&self) -> String {
        self.with_cache(String::new(), |c| c.auth_backend.clone())
            .await
    }

    #[zbus(property, name = "AuthFailureReason")]
    async fn auth_failure_reason(&self) -> String {
        self.with_cache(String::new(), |c| c.auth_failure_reason.clone())
            .await
    }

    #[zbus(property, name = "EapMethod")]
    async fn eap_method(&self) -> String {
        self.with_cache(String::new(), |c| c.eap_method.clone())
            .await
    }
}
