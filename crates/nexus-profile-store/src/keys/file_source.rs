//! File-backed master key. See DD-007 §4.2 "File-based key".
//!
//! The master key lives in `/var/lib/nexus/keys/master.key` (or a
//! configurable path), mode `0400`, owned by `nexus`. This source
//! is the default on hosts without a TPM or keyring — encryption at
//! rest is protected only by filesystem permissions.
//!
//! Nexus logs a WARN on every startup when this source is active
//! so production operators notice they're on the weaker option.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::OsRng;
use chacha20poly1305::aead::rand_core::RngCore;

use super::{KeyError, MasterKeySource};

/// File mode for `master.key`: owner read-only.
pub const KEY_FILE_MODE: u32 = 0o400;

/// File-backed master key source.
#[derive(Debug, Clone)]
pub struct FileKeySource {
    path: PathBuf,
}

impl FileKeySource {
    /// Path on disk. Typical production value is
    /// `/var/lib/nexus/keys/master.key`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Load the key if the file exists, or generate a fresh random
    /// 32-byte key and persist it with mode `0400`. Logs a WARN on
    /// the "just generated" path.
    pub fn load_or_generate(&self) -> Result<[u8; 32], KeyError> {
        match self.try_load()? {
            Some(k) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "profile-store master key source is file-based (no hardware binding); \
                     consider TPM or keyring backends for production",
                );
                Ok(k)
            }
            None => {
                if let Some(parent) = self.path.parent() {
                    create_dir_0700(parent)?;
                }
                let key = random_key();
                write_mode(&self.path, &key, KEY_FILE_MODE)?;
                tracing::warn!(
                    path = %self.path.display(),
                    "generated fresh file-based master key (no hardware binding)",
                );
                Ok(key)
            }
        }
    }

    fn try_load(&self) -> Result<Option<[u8; 32]>, KeyError> {
        match fs::metadata(&self.path) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let mut f = OpenOptions::new().read(true).open(&self.path)?;
        let mut buf = [0u8; 32];
        f.read_exact(&mut buf).map_err(|e| {
            KeyError::Unavailable(format!(
                "{}: expected 32-byte key, got fewer: {e}",
                self.path.display(),
            ))
        })?;
        // Soft-warn (not fail) on surprising extra bytes.
        let mut tail = [0u8; 1];
        if f.read(&mut tail).unwrap_or(0) > 0 {
            tracing::warn!(
                path = %self.path.display(),
                "master.key has extra bytes beyond the first 32; ignoring the remainder",
            );
        }
        Ok(Some(buf))
    }
}

impl MasterKeySource for FileKeySource {
    fn master_key(&self) -> Result<[u8; 32], KeyError> {
        self.load_or_generate()
    }

    fn name(&self) -> &'static str {
        "file"
    }
}

fn create_dir_0700(path: &Path) -> Result<(), KeyError> {
    use std::os::unix::fs::DirBuilderExt;
    if path.exists() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(Into::into)
}

fn write_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<(), KeyError> {
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_CLOEXEC)
        .open(path)?;
    f.write_all(bytes)?;
    // Defensive chmod in case umask lowered the mode.
    fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn random_key() -> [u8; 32] {
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    k
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn generates_new_key_on_first_run() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("master.key");
        let source = FileKeySource::new(&path);
        let k1 = source.master_key().unwrap();
        assert_eq!(k1.len(), 32);

        // File should exist with 0400 perms and contain exactly 32
        // bytes.
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 32);
        assert_eq!(meta.mode() & 0o777, 0o400);
    }

    #[test]
    fn second_open_returns_the_same_key() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("master.key");
        let source = FileKeySource::new(&path);
        let a = source.master_key().unwrap();
        let b = source.master_key().unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn two_fresh_sources_produce_distinct_keys() {
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let a = FileKeySource::new(dir_a.path().join("master.key"))
            .master_key()
            .unwrap();
        let b = FileKeySource::new(dir_b.path().join("master.key"))
            .master_key()
            .unwrap();
        assert_ne!(a, b);
    }
}
