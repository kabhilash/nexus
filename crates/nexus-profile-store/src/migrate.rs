//! Store-version tracking and schema migration. See DD-007 §8.
//!
//! The store root carries a `version` file with a single integer —
//! the current on-disk store version. On open, the store compares
//! that against [`CURRENT_STORE_VERSION`]:
//!
//! - Equal → no migration needed.
//! - Less  → run migrators in order until the versions match.
//! - Greater → refuse to open (the binary is too old for this
//!   store; operators downgrade via backup/restore per DD-007 §9).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Result, StoreError};
use crate::fs_store::{self, DIR_BLUETOOTH, DIR_ETHERNET, DIR_GNSS, DIR_WIFI, ProfileFileStore};
use crate::metrics as m;

/// Filename under the store root that tracks the on-disk version.
pub const VERSION_FILENAME: &str = "version";

/// Current store version this build of the library writes and
/// understands. Bump when the directory layout or cross-file
/// contract changes, and add a new migrator keyed by the previous
/// version.
pub const CURRENT_STORE_VERSION: u32 = 2;

/// Outcome of one migrator's run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub from_version: u32,
    pub to_version: u32,
    pub profiles_migrated: u32,
}

/// Read the `version` file at the store root. Returns `None` if the
/// file doesn't exist (first-ever open of a fresh directory).
pub fn read_store_version(root: &Path) -> Result<Option<u32>> {
    let path = root.join(VERSION_FILENAME);
    match fs::read_to_string(&path) {
        Ok(s) => s
            .trim()
            .parse::<u32>()
            .map(Some)
            .map_err(|_| StoreError::malformed(&path, "version file is not a positive integer")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::io(&path, e)),
    }
}

/// Write the `version` file atomically.
pub fn write_store_version(root: &Path, version: u32) -> Result<()> {
    let path = root.join(VERSION_FILENAME);
    fs_store::write_atomic(&path, format!("{version}\n").as_bytes())
}

/// Check the on-disk version and run pending migrators. Called by
/// `ProfileFileStore::open`.
pub fn run_startup_migrations(store: &ProfileFileStore) -> Result<()> {
    let current = match read_store_version(store.root())? {
        Some(v) => v,
        None => {
            // Fresh directory: stamp the current version and exit.
            write_store_version(store.root(), CURRENT_STORE_VERSION)?;
            return Ok(());
        }
    };

    if current == CURRENT_STORE_VERSION {
        return Ok(());
    }
    if current > CURRENT_STORE_VERSION {
        return Err(StoreError::malformed(
            store.root().join(VERSION_FILENAME),
            format!(
                "store version {current} is newer than this binary's supported \
                 version {CURRENT_STORE_VERSION}",
            ),
        ));
    }

    // current < CURRENT_STORE_VERSION: walk migrators forward.
    let mut v = current;
    while v < CURRENT_STORE_VERSION {
        let next = v + 1;
        let outcome = run_migrator(store, v, next);
        match outcome {
            Ok(report) => {
                m::record_migration(report.from_version, report.to_version, m::outcome::SUCCESS);
                write_store_version(store.root(), next)?;
                tracing::info!(
                    from = report.from_version,
                    to = report.to_version,
                    profiles = report.profiles_migrated,
                    "store migration complete",
                );
            }
            Err(e) => {
                m::record_migration(v, next, m::outcome::IO_ERROR);
                return Err(e);
            }
        }
        v = next;
    }
    Ok(())
}

fn run_migrator(store: &ProfileFileStore, from: u32, to: u32) -> Result<MigrationReport> {
    match (from, to) {
        (1, 2) => migrate_v1_to_v2(store),
        _ => Err(StoreError::NotYetImplemented(
            "migrator not registered for this version pair",
        )),
    }
}

// ---------------------------------------------------------------------------
// v1 → v2
//
// Phase-9 placeholder: we don't actually change any field, but we
// bump every profile's `schema_version` so downstream consumers can
// observe the bump and so the test suite has a concrete migration
// to exercise. The real content changes land with future schema
// revisions.
// ---------------------------------------------------------------------------

fn migrate_v1_to_v2(store: &ProfileFileStore) -> Result<MigrationReport> {
    let mut migrated = 0u32;
    for sub in [DIR_WIFI, DIR_ETHERNET, DIR_GNSS, DIR_BLUETOOTH] {
        migrated += bump_schema_version_in(&store.dir(sub))?;
    }
    Ok(MigrationReport {
        from_version: 1,
        to_version: 2,
        profiles_migrated: migrated,
    })
}

fn bump_schema_version_in(dir: &Path) -> Result<u32> {
    let mut count = 0u32;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(StoreError::io(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::io(dir, e))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        bump_schema_version_in_file(&path)?;
        count += 1;
    }
    Ok(count)
}

fn bump_schema_version_in_file(path: &PathBuf) -> Result<()> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return Err(StoreError::io(path, e)),
    };
    // Parse to generic toml::Value, mutate, re-serialize. Using
    // generic TOML avoids having to round-trip through every profile
    // kind's struct (which would require the right Cipher on hand).
    let mut value: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "skipping malformed profile during migration");
            return Ok(());
        }
    };
    if let Some(table) = value.as_table_mut() {
        table.insert(
            "schema_version".to_owned(),
            toml::Value::Integer(CURRENT_STORE_VERSION as i64),
        );
    }
    let out = toml::to_string_pretty(&value).map_err(StoreError::from)?;
    fs_store::write_atomic(path, out.as_bytes())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn version_file_roundtrip() {
        let dir = TempDir::new().unwrap();
        assert_eq!(read_store_version(dir.path()).unwrap(), None);
        write_store_version(dir.path(), 7).unwrap();
        assert_eq!(read_store_version(dir.path()).unwrap(), Some(7));
    }

    #[test]
    fn malformed_version_file_is_classified() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join(VERSION_FILENAME), "not a number").unwrap();
        let err = read_store_version(dir.path()).unwrap_err();
        assert!(matches!(err, StoreError::Malformed { .. }));
    }
}
