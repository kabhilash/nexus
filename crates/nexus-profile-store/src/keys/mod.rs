//! Master-key sources. See DD-007 §4.2.
//!
//! Each implementation of [`MasterKeySource`] produces the 32-byte
//! master key used to derive per-file keys (see
//! [`crate::crypto::kdf`]). `FileKeySource` is always buildable;
//! `DerivedKeySource` only needs the `scrypt` dep (already in the
//! default feature set); `TpmKeySource` is behind the `tpm` feature.

pub mod derived_source;
pub mod file_source;

#[cfg(feature = "tpm")]
pub mod tpm_source;

use std::io;

use thiserror::Error;

pub use derived_source::DerivedKeySource;
pub use file_source::FileKeySource;

#[cfg(feature = "tpm")]
pub use tpm_source::TpmKeySource;

/// Errors produced by [`MasterKeySource`] implementations.
#[derive(Debug, Error)]
pub enum KeyError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("scrypt error: {0}")]
    Scrypt(String),

    /// The source exists but isn't usable on this host (e.g., TPM
    /// sealed key present but PCR policy no longer matches).
    #[error("master-key source unavailable: {0}")]
    Unavailable(String),

    /// Placeholder returned by sources that aren't fully implemented
    /// yet (see `TpmKeySource`). Should never be seen on a built-out
    /// deployment.
    #[error("{0} is not implemented in this phase")]
    NotImplemented(&'static str),
}

/// Produces the 32-byte master key. Implementations are expected
/// to cache internally — the store calls [`MasterKeySource::master_key`]
/// exactly once per `open()`.
pub trait MasterKeySource: Send + Sync {
    fn master_key(&self) -> Result<[u8; 32], KeyError>;

    /// Human-readable name for logs and metrics (`file`, `tpm`,
    /// `derived`, …).
    fn name(&self) -> &'static str;
}

// ---------------------------------------------------------------------------
// In-memory test source
// ---------------------------------------------------------------------------

/// Test-only source that returns a fixed master key held in memory.
/// Useful for unit tests, examples, and integration tests that
/// don't want to touch disk or scrypt.
///
/// This source intentionally has no feature gate — it's cheap,
/// dependency-free, and harmless in production (anyone who wants a
/// fixed master key can already construct one).
#[derive(Debug, Clone)]
pub struct InMemoryKeySource {
    key: [u8; 32],
}

impl InMemoryKeySource {
    pub fn new(key: [u8; 32]) -> Self {
        Self { key }
    }
}

impl MasterKeySource for InMemoryKeySource {
    fn master_key(&self) -> Result<[u8; 32], KeyError> {
        Ok(self.key)
    }

    fn name(&self) -> &'static str {
        "memory"
    }
}
