//! Top-level errors for the GNSS Backend. See DD-005 §4.1.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, GnssError>;

#[derive(Debug, Error)]
pub enum GnssError {
    /// gpsd is not reachable. The supervisor tick retries `connect()`
    /// with backoff (DD-005 §6.4).
    #[error("not connected to gpsd")]
    NotConnected,

    /// gpsd's `?VERSION` reply announced a protocol older than the
    /// 3.x series we support.
    #[error("gpsd protocol too old: got major={got}, need >= 3")]
    GpsdProtocolTooOld { got: u32 },

    /// Network I/O failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// Malformed or unexpected JSON from gpsd.
    #[error("gpsd json: {0}")]
    Json(#[from] serde_json::Error),

    /// Device path the operator asked about isn't in our registry.
    #[error("unknown device: {0}")]
    UnknownDevice(String),

    /// Profile store surfaced a problem.
    #[error("profile-store: {0}")]
    ProfileStore(String),
}

impl From<nexus_profile_store::StoreError> for GnssError {
    fn from(e: nexus_profile_store::StoreError) -> Self {
        GnssError::ProfileStore(e.to_string())
    }
}
