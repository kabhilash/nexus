//! Top-level error type for the Wi-Fi Backend.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, WifiError>;

#[derive(Debug, Error)]
pub enum WifiError {
    /// A supplicant operation was issued against an ifindex that
    /// wasn't attached first.
    #[error("ifindex {ifindex} is not attached to the supplicant")]
    NotAttached { ifindex: u32 },

    /// No visible BSS matched any loaded profile.
    #[error("no profile matches the scan results on ifindex {ifindex}")]
    NoProfileMatch { ifindex: u32 },

    /// The supplicant daemon returned an error.
    #[error("supplicant '{backend}' error: {source}")]
    Supplicant {
        backend: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    /// Profile store surfaced a problem.
    #[error("profile-store: {0}")]
    ProfileStore(#[from] nexus_profile_store::StoreError),
}
