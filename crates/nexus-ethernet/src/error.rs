//! Top-level error type for the Ethernet Backend.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, EthernetError>;

#[derive(Debug, Error)]
pub enum EthernetError {
    /// `authenticate` / `detach` / `state` called on an ifindex that
    /// hasn't been registered via `attach` yet.
    #[error("ifindex {ifindex} is not attached to the auth backend")]
    NotAttached { ifindex: u32 },

    /// The auth backend's external daemon returned an error.
    #[error("auth daemon '{backend}' error: {source}")]
    AuthDaemon {
        backend: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Profile-store error surfaced during profile load / update.
    #[error("profile-store error: {0}")]
    ProfileStore(#[from] nexus_profile_store::StoreError),

    /// An 802.1X-configured interface came up but no auth backend
    /// is available.
    #[error("802.1X profile on {ifname} requires an auth backend, but none is configured")]
    AuthBackendUnavailable { ifname: String },
}
