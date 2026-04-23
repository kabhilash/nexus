//! Top-level error type for the Profile Store.

use std::io;
use std::path::PathBuf;

use thiserror::Error;

/// Shared `Result` alias.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Errors surfaced by [`crate::trait_def::ProfileStore`]
/// implementations and the atomic-write helpers.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("I/O error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("TOML serialization failed: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("TOML deserialization failed: {0}")]
    TomlDeserialize(#[from] toml::de::Error),

    /// A profile file at `path` is present but fails to deserialize
    /// / validate. The load path logs these at `warn` and skips the
    /// file so the remaining profiles still load (DD-007 §10.1).
    #[error("profile at {path} is malformed: {reason}")]
    Malformed { path: PathBuf, reason: String },

    /// The store's root directory could not be created.
    #[error("failed to create profile store root at {path}: {source}")]
    RootDir {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    /// A function that isn't expected to be called during phase 2
    /// (e.g., `rotate_master_key`) was invoked.
    #[error("{0} is not implemented until the encryption phase")]
    NotYetImplemented(&'static str),
}

impl StoreError {
    /// Construct an [`StoreError::Io`] for a specific path.
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// Construct a [`StoreError::Malformed`] classification.
    pub fn malformed(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        Self::Malformed {
            path: path.into(),
            reason: reason.into(),
        }
    }
}
