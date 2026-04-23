//! `fi.nexus.Interface` — shared on every per-technology object.
//! See DD-006 §6.1.

use std::sync::Arc;

use nexus_core::OperState;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath};

use crate::services::Services;

pub struct InterfaceIface {
    pub services: Arc<Services>,
    pub ifname: String,
}

impl InterfaceIface {
    pub fn new(services: Arc<Services>, ifname: impl Into<String>) -> Self {
        Self {
            services,
            ifname: ifname.into(),
        }
    }
}

#[zbus::interface(name = "fi.nexus.Interface")]
impl InterfaceIface {
    #[zbus(property, name = "Ifname")]
    async fn ifname(&self) -> String {
        self.ifname.clone()
    }

    #[zbus(property, name = "Ifindex")]
    async fn ifindex(&self) -> u32 {
        self.services
            .state
            .read()
            .await
            .interfaces
            .get(&self.ifname)
            .map(|e| e.info.ifindex)
            .unwrap_or(0)
    }

    #[zbus(property, name = "Mac")]
    async fn mac(&self) -> Vec<u8> {
        let guard = self.services.state.read().await;
        guard
            .interfaces
            .get(&self.ifname)
            .map(|e| e.info.mac.to_vec())
            .unwrap_or_default()
    }

    #[zbus(property, name = "Kind")]
    async fn kind(&self) -> String {
        self.services
            .state
            .read()
            .await
            .interfaces
            .get(&self.ifname)
            .map(|e| e.kind_label().to_owned())
            .unwrap_or_default()
    }

    #[zbus(property, name = "OperState")]
    async fn oper_state(&self) -> String {
        self.services
            .state
            .read()
            .await
            .interfaces
            .get(&self.ifname)
            .map(|e| oper_state_label(&e.info.operstate).to_owned())
            .unwrap_or_default()
    }

    #[zbus(property, name = "Carrier")]
    async fn carrier(&self) -> bool {
        self.services
            .state
            .read()
            .await
            .interfaces
            .get(&self.ifname)
            .map(|e| e.info.carrier)
            .unwrap_or(false)
    }

    #[zbus(property, name = "ManagedProfile")]
    async fn managed_profile(&self) -> OwnedObjectPath {
        let guard = self.services.state.read().await;
        let path = guard
            .interfaces
            .get(&self.ifname)
            .and_then(|e| e.managed_profile.clone())
            .unwrap_or_else(|| "/".to_owned());
        ObjectPath::try_from(path)
            .unwrap_or_else(|_| ObjectPath::try_from("/").unwrap())
            .into()
    }

    /// `StateChanged(new_state: s, details: a{sv})` (DD-006 §9).
    /// The event loop fires this via `SignalEmitter`; no caller
    /// ever invokes this directly as a method.
    #[zbus(signal)]
    pub async fn state_changed(
        emitter: &SignalEmitter<'_>,
        new_state: &str,
        details: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::Result<()>;
}

pub fn oper_state_label(s: &OperState) -> &'static str {
    match s {
        OperState::Unknown => "unknown",
        OperState::NotPresent => "notpresent",
        OperState::Down => "down",
        OperState::LowerLayerDown => "lowerlayerdown",
        OperState::Testing => "testing",
        OperState::Dormant => "dormant",
        OperState::Up => "up",
    }
}
