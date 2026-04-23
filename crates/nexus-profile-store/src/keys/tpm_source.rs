//! TPM-sealed master key. See DD-007 §4.2 "TPM-sealed key".
//!
//! This module is only compiled when the `tpm` Cargo feature is
//! enabled (which pulls in `tss-esapi`). The type here is a
//! structural placeholder — the actual sealing/unsealing flow
//! requires live TPM hardware and PCR-policy configuration that
//! lands with phase-4 TPM integration proper.
//!
//! Behavior today:
//! - Construction succeeds and records the sealed-key path.
//! - [`MasterKeySource::master_key`] returns
//!   [`KeyError::NotImplemented`] until the full flow is wired in.
//!
//! The test suite gates actual TPM-hardware tests behind the
//! `tpm-integration` feature so CI on TPM-less machines skips them.

use std::path::PathBuf;

use super::{KeyError, MasterKeySource};

/// TPM-backed master-key source.
#[derive(Debug, Clone)]
pub struct TpmKeySource {
    sealed_key_path: PathBuf,
    pcrs: Vec<u32>,
}

impl TpmKeySource {
    /// Path is typically `/var/lib/nexus/keys/master.key.sealed`.
    /// `pcrs` is the bind list (DD-007 §4.2 default is `[0, 2, 4,
    /// 7, 11]`).
    pub fn new(sealed_key_path: impl Into<PathBuf>, pcrs: Vec<u32>) -> Self {
        Self {
            sealed_key_path: sealed_key_path.into(),
            pcrs,
        }
    }

    pub fn sealed_key_path(&self) -> &std::path::Path {
        &self.sealed_key_path
    }

    pub fn pcrs(&self) -> &[u32] {
        &self.pcrs
    }
}

impl MasterKeySource for TpmKeySource {
    fn master_key(&self) -> Result<[u8; 32], KeyError> {
        Err(KeyError::NotImplemented("TPM unseal"))
    }

    fn name(&self) -> &'static str {
        "tpm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_and_metadata() {
        let src = TpmKeySource::new(
            "/var/lib/nexus/keys/master.key.sealed",
            vec![0, 2, 4, 7, 11],
        );
        assert_eq!(src.name(), "tpm");
        assert_eq!(src.pcrs(), &[0, 2, 4, 7, 11]);
        assert_eq!(
            src.sealed_key_path(),
            std::path::Path::new("/var/lib/nexus/keys/master.key.sealed"),
        );
    }

    #[test]
    fn master_key_returns_not_implemented_until_phase_4_completes() {
        let src = TpmKeySource::new("/tmp/ignored.sealed", vec![]);
        let err = src.master_key().unwrap_err();
        match err {
            KeyError::NotImplemented(name) => assert!(name.contains("TPM")),
            other => panic!("expected NotImplemented, got {other:?}"),
        }
    }

    /// Hardware-dependent integration test. Requires a live TPM 2.0
    /// accessible via `/dev/tpm0` and `tss-esapi` configured. Skip
    /// by default; run explicitly with
    /// `cargo test -p nexus-profile-store --features tpm-integration`.
    #[cfg(feature = "tpm-integration")]
    #[test]
    #[ignore = "requires a live TPM 2.0 on /dev/tpm0"]
    fn tpm_integration_seal_unseal_roundtrip() {
        // Placeholder for the real seal/unseal test; implemented
        // when phase-4 TPM integration lands.
        let src = TpmKeySource::new("/tmp/seal.bin", vec![0, 7]);
        let _ = src.master_key();
    }
}
