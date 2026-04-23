//! `fi.nexus.Profile` — DD-006 §7.1. Shared across every profile
//! object regardless of technology.

use std::sync::Arc;

use chrono::SecondsFormat;
use ulid::Ulid;
use zbus::fdo;
use zbus::message::Header;

use crate::authz::{AuthDecision, actions};
use crate::errors::DbusError;
use crate::services::Services;

use super::ProfileKind;

pub struct ProfileIface {
    pub services: Arc<Services>,
    pub registry: tokio::sync::mpsc::Sender<crate::service::ServiceCommand>,
    pub id: Ulid,
    pub kind: ProfileKind,
}

impl ProfileIface {
    pub fn new(
        services: Arc<Services>,
        registry: tokio::sync::mpsc::Sender<crate::service::ServiceCommand>,
        id: Ulid,
        kind: ProfileKind,
    ) -> Self {
        Self {
            services,
            registry,
            id,
            kind,
        }
    }

    async fn require_auth(&self, hdr: &Header<'_>, action: &str) -> fdo::Result<()> {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        match self.services.auth.check(action, &sender).await {
            AuthDecision::Authorized => Ok(()),
            AuthDecision::Denied => Err(fdo::Error::from(DbusError::AuthFailed(format!(
                "policykit denied '{action}' for sender '{sender}'"
            )))),
        }
    }
}

#[zbus::interface(name = "fi.nexus.Profile")]
impl ProfileIface {
    #[zbus(property, name = "Id")]
    async fn id(&self) -> String {
        self.id.to_string()
    }

    #[zbus(property, name = "Kind")]
    async fn kind(&self) -> String {
        self.kind.as_str().to_owned()
    }

    #[zbus(property, name = "Label")]
    async fn label(&self) -> String {
        let guard = self.services.state.read().await;
        let key = self.id.to_string();
        match self.kind {
            ProfileKind::Wifi => guard
                .wifi_profiles
                .get(&key)
                .and_then(|p| p.metadata.label.clone())
                .unwrap_or_default(),
            ProfileKind::Ethernet => guard
                .ethernet_profiles
                .get(&key)
                .and_then(|p| p.metadata.label.clone())
                .unwrap_or_default(),
        }
    }

    #[zbus(property, name = "CreatedAt")]
    async fn created_at(&self) -> String {
        let guard = self.services.state.read().await;
        let key = self.id.to_string();
        let ts = match self.kind {
            ProfileKind::Wifi => guard
                .wifi_profiles
                .get(&key)
                .and_then(|p| p.metadata.created_at),
            ProfileKind::Ethernet => guard
                .ethernet_profiles
                .get(&key)
                .and_then(|p| p.metadata.created_at),
        };
        ts.map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_default()
    }

    #[zbus(property, name = "UpdatedAt")]
    async fn updated_at(&self) -> String {
        let guard = self.services.state.read().await;
        let key = self.id.to_string();
        let ts = match self.kind {
            ProfileKind::Wifi => guard
                .wifi_profiles
                .get(&key)
                .and_then(|p| p.metadata.updated_at),
            ProfileKind::Ethernet => guard
                .ethernet_profiles
                .get(&key)
                .and_then(|p| p.metadata.updated_at),
        };
        ts.map(|t| t.to_rfc3339_opts(SecondsFormat::Millis, true))
            .unwrap_or_default()
    }

    #[zbus(property, name = "CredentialsInvalid")]
    async fn credentials_invalid(&self) -> bool {
        let guard = self.services.state.read().await;
        let key = self.id.to_string();
        match self.kind {
            ProfileKind::Wifi => guard
                .wifi_profiles
                .get(&key)
                .map(|p| p.network.credentials_invalid)
                .unwrap_or(false),
            // Ethernet profile in the store doesn't expose an
            // invalid flag; default to false.
            ProfileKind::Ethernet => false,
        }
    }

    // `CredentialsInvalidChanged` is the property-change signal
    // for the `CredentialsInvalid` property and is delivered via
    // the standard `PropertiesChanged` mechanism (DD-006 §12.1).
    // No custom signal is defined here to avoid colliding with
    // zbus's auto-generated property-change helper.

    /// `Update(settings: a{sv}) -> ()`. Requires
    /// `fi.nexus.profile.modify`. Phase 4-6 implements only the
    /// `label` field; full credential editing lands in a later
    /// phase alongside `fi.nexus.profile.read_credentials`.
    async fn update(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        settings: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> fdo::Result<()> {
        self.require_auth(&hdr, actions::PROFILE_MODIFY).await?;
        let key = self.id.to_string();
        let new_label = crate::manager::lookup_string(&settings, "label")
            .map_err(DbusError::InvalidArgument)?;

        // Persist via the store, then refresh the in-memory cache.
        match self.kind {
            ProfileKind::Wifi => {
                let mut state = self.services.state.write().await;
                let Some(profile) = state.wifi_profiles.get_mut(&key) else {
                    return Err(DbusError::NotFound(format!("wifi profile {}", self.id)).into());
                };
                if let Some(label) = &new_label {
                    profile.metadata.label = Some(label.clone());
                }
                profile.metadata.updated_at = Some(chrono::Utc::now());
                let snapshot = profile.clone();
                drop(state);
                self.services
                    .profile_store
                    .put_wifi(&snapshot)
                    .await
                    .map_err(DbusError::from)?;
            }
            ProfileKind::Ethernet => {
                let mut state = self.services.state.write().await;
                let Some(profile) = state.ethernet_profiles.get_mut(&key) else {
                    return Err(DbusError::NotFound(format!("ethernet profile {}", self.id)).into());
                };
                if let Some(label) = &new_label {
                    profile.metadata.label = Some(label.clone());
                }
                profile.metadata.updated_at = Some(chrono::Utc::now());
                let snapshot = profile.clone();
                drop(state);
                self.services
                    .profile_store
                    .put_ethernet(&snapshot)
                    .await
                    .map_err(DbusError::from)?;
            }
        }
        Ok(())
    }

    /// `Delete() -> ()`. Requires `fi.nexus.profile.modify`.
    /// Equivalent to `Manager.RemoveProfile(this)`.
    async fn delete(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        self.require_auth(&hdr, actions::PROFILE_MODIFY).await?;
        let key = self.id.to_string();
        match self.kind {
            ProfileKind::Wifi => {
                let mut state = self.services.state.write().await;
                let Some(profile) = state.wifi_profiles.remove(&key) else {
                    return Err(DbusError::NotFound(format!("wifi profile {}", self.id)).into());
                };
                let hash = nexus_profile_store::ssid_hash(&profile.network.ssid);
                drop(state);
                self.services
                    .profile_store
                    .remove_wifi(&hash)
                    .await
                    .map_err(DbusError::from)?;
                let _ = self
                    .registry
                    .send(crate::service::ServiceCommand::UnregisterWifiProfile(
                        self.id,
                    ))
                    .await;
            }
            ProfileKind::Ethernet => {
                let mut state = self.services.state.write().await;
                let Some(profile) = state.ethernet_profiles.remove(&key) else {
                    return Err(DbusError::NotFound(format!("ethernet profile {}", self.id)).into());
                };
                let ifname = profile.interface.name.clone();
                drop(state);
                self.services
                    .profile_store
                    .remove_ethernet(&ifname)
                    .await
                    .map_err(DbusError::from)?;
                let _ = self
                    .registry
                    .send(crate::service::ServiceCommand::UnregisterEthernetProfile(
                        self.id,
                    ))
                    .await;
            }
        }
        Ok(())
    }
}
