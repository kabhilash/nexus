//! Quarantine machinery for corrupt profile files. See DD-007
//! §10.1.
//!
//! On load, a file that fails to deserialize, decrypts under no
//! key, or violates a structural invariant is moved to
//! `<root>/.quarantine/<original-name>.<ts>` with a tombstone
//! `.note` alongside describing the failure. The store emits an
//! `OperatorNotification` event so the D-Bus layer can surface the
//! corruption to an operator.

use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use nexus_core::{NexusEvent, NotificationData, ProfileKind};
use tokio::sync::broadcast;

use crate::error::{Result, StoreError};
use crate::metrics;

/// Quarantine directory name (relative to the store root). Leading
/// `.` keeps it out of default `ls` listings.
pub const QUARANTINE_DIR: &str = ".quarantine";

/// Tombstone file extension appended to the moved file's name.
pub const TOMBSTONE_EXT: &str = "note";

/// Outcome classification for the corrupt-reason metric label.
pub use crate::metrics::corrupt_reason;

/// Move the malformed file at `path` to the store's quarantine
/// directory, then write a tombstone note alongside. Emits
/// `NexusEvent::ProfileCorrupt` + `OperatorNotification` on
/// `event_tx` if it's set.
pub fn quarantine_file(
    root: &Path,
    kind: ProfileKind,
    path: &Path,
    reason: &str,
    reason_label: &str,
    event_tx: Option<&broadcast::Sender<NexusEvent>>,
) -> Result<PathBuf> {
    let quarantine_dir = root.join(QUARANTINE_DIR);
    create_quarantine_dir(&quarantine_dir)?;

    let original_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let target = quarantine_dir.join(format!("{original_name}.{ts}"));

    fs::rename(path, &target).map_err(|e| StoreError::io(path, e))?;
    write_tombstone(&target, original_name, kind, reason)?;

    metrics::record_corrupt(kind, reason_label);

    if let Some(tx) = event_tx {
        let key = original_name.trim_end_matches(".toml").to_owned();
        let _ = tx.send(NexusEvent::ProfileCorrupt {
            kind,
            key: key.clone(),
            reason: reason.to_owned(),
        });

        let mut data = NotificationData::new();
        data.insert("kind", kind_label(kind));
        data.insert("file", original_name);
        data.insert("reason", reason);
        data.insert("quarantined_to", target.to_string_lossy().into_owned());
        let _ = tx.send(NexusEvent::OperatorNotification {
            kind: "profile_corrupt".to_owned(),
            data,
        });
    }

    Ok(target)
}

fn kind_label(kind: ProfileKind) -> &'static str {
    match kind {
        ProfileKind::Ethernet => "ethernet",
        ProfileKind::Wifi => "wifi",
        ProfileKind::Gnss => "gnss",
        ProfileKind::Bluetooth => "bluetooth",
    }
}

fn create_quarantine_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        return Ok(());
    }
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| StoreError::RootDir {
            path: dir.to_owned(),
            source: e,
        })
}

fn write_tombstone(
    quarantined_path: &Path,
    original_name: &str,
    kind: ProfileKind,
    reason: &str,
) -> Result<()> {
    let note_path = quarantined_path.with_extension(format!(
        "{}.{}",
        quarantined_path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or(""),
        TOMBSTONE_EXT,
    ));
    let iso_timestamp = chrono::Utc::now().to_rfc3339();
    let body = format!(
        "# Nexus profile-store quarantine tombstone\n\
         kind = \"{kind_label}\"\n\
         original_name = \"{original_name}\"\n\
         quarantined_at = \"{iso_timestamp}\"\n\
         reason = \"\"\"\n{reason}\n\"\"\"\n",
        kind_label = kind_label(kind),
    );
    fs::write(&note_path, body).map_err(|e| StoreError::io(&note_path, e))?;
    Ok(())
}

/// Emit only an `OperatorNotification` without moving anything —
/// used by the rotation path to surface a `master_key_degraded`
/// state without quarantining the profile that failed.
pub fn notify_master_key_degraded(
    event_tx: Option<&broadcast::Sender<NexusEvent>>,
    reason: &str,
    completed: u32,
    total: u32,
) {
    if let Some(tx) = event_tx {
        let mut data = NotificationData::new();
        data.insert("reason", reason);
        data.insert("completed", completed);
        data.insert("total", total);
        let _ = tx.send(NexusEvent::OperatorNotification {
            kind: "master_key_degraded".to_owned(),
            data,
        });
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn quarantine_moves_file_and_writes_tombstone() {
        let dir = TempDir::new().unwrap();
        let wifi_dir = dir.path().join("wifi");
        fs::create_dir_all(&wifi_dir).unwrap();
        let victim = wifi_dir.join("abcdef0123456789.toml");
        fs::write(&victim, b"broken content").unwrap();

        let moved = quarantine_file(
            dir.path(),
            ProfileKind::Wifi,
            &victim,
            "simulated decrypt failure",
            corrupt_reason::DECRYPT_FAIL,
            None,
        )
        .unwrap();

        assert!(!victim.exists(), "original must move away");
        assert!(moved.exists(), "quarantined copy must be present");
        assert!(
            moved
                .file_name()
                .unwrap()
                .to_string_lossy()
                .contains("abcdef0123456789.toml."),
            "quarantined name should preserve original + timestamp",
        );

        // Tombstone is alongside with .note extension appended.
        let mut tombstone = moved.clone();
        tombstone.set_extension(format!(
            "{}.{}",
            moved.extension().unwrap().to_string_lossy(),
            TOMBSTONE_EXT,
        ));
        let body = fs::read_to_string(&tombstone).unwrap();
        assert!(body.contains("kind = \"wifi\""));
        assert!(body.contains("simulated decrypt failure"));
    }

    #[test]
    fn quarantine_emits_profile_corrupt_and_operator_notification() {
        let dir = TempDir::new().unwrap();
        let wifi_dir = dir.path().join("wifi");
        fs::create_dir_all(&wifi_dir).unwrap();
        let victim = wifi_dir.join("abc.toml");
        fs::write(&victim, b"x").unwrap();

        let (tx, mut rx) = broadcast::channel(8);
        let _ = quarantine_file(
            dir.path(),
            ProfileKind::Wifi,
            &victim,
            "bad",
            corrupt_reason::DECRYPT_FAIL,
            Some(&tx),
        )
        .unwrap();

        let mut saw_corrupt = false;
        let mut saw_notification = false;
        while let Ok(event) = rx.try_recv() {
            match event {
                NexusEvent::ProfileCorrupt { kind, key, .. } => {
                    assert_eq!(kind, ProfileKind::Wifi);
                    assert_eq!(key, "abc");
                    saw_corrupt = true;
                }
                NexusEvent::OperatorNotification { kind, data } => {
                    assert_eq!(kind, "profile_corrupt");
                    assert!(data.get("file").is_some());
                    saw_notification = true;
                }
                _ => {}
            }
        }
        assert!(saw_corrupt, "ProfileCorrupt must be emitted");
        assert!(
            saw_notification,
            "OperatorNotification{{kind:profile_corrupt}} must be emitted",
        );
    }

    #[test]
    fn quarantine_file_writes_correct_kind_label_for_each_profile_kind() {
        // Ethernet/Gnss/Bluetooth tombstones must carry the right
        // `kind = "..."` line; these are the strings every operator
        // tool greps for, and they are the kind_label arms not
        // exercised by the happy-path Wifi test.
        for (kind, expected) in [
            (ProfileKind::Ethernet, "ethernet"),
            (ProfileKind::Gnss, "gnss"),
            (ProfileKind::Bluetooth, "bluetooth"),
        ] {
            let dir = TempDir::new().unwrap();
            let sub = dir.path().join(expected);
            fs::create_dir_all(&sub).unwrap();
            let victim = sub.join("file.toml");
            fs::write(&victim, b"x").unwrap();

            let moved = quarantine_file(
                dir.path(),
                kind,
                &victim,
                "bad",
                corrupt_reason::DECRYPT_FAIL,
                None,
            )
            .unwrap();

            let mut tombstone = moved.clone();
            tombstone.set_extension(format!(
                "{}.{}",
                moved.extension().unwrap().to_string_lossy(),
                TOMBSTONE_EXT,
            ));
            let body = fs::read_to_string(&tombstone).unwrap();
            assert!(
                body.contains(&format!("kind = \"{expected}\"")),
                "{kind:?} tombstone missing kind label, got:\n{body}"
            );
        }
    }

    #[test]
    fn quarantine_dir_creation_is_idempotent_across_calls() {
        // Two consecutive quarantines: first creates the dir, the
        // second hits the `if dir.exists() { return Ok }` early
        // return in `create_quarantine_dir`.
        let dir = TempDir::new().unwrap();
        let wifi_dir = dir.path().join("wifi");
        fs::create_dir_all(&wifi_dir).unwrap();

        for n in 0..2 {
            let victim = wifi_dir.join(format!("v{n}.toml"));
            fs::write(&victim, b"x").unwrap();
            quarantine_file(
                dir.path(),
                ProfileKind::Wifi,
                &victim,
                "bad",
                corrupt_reason::DECRYPT_FAIL,
                None,
            )
            .unwrap();
        }

        let entries: Vec<_> = fs::read_dir(dir.path().join(QUARANTINE_DIR))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        // Two payloads + two tombstones.
        assert_eq!(entries.len(), 4, "got entries: {entries:?}");
    }

    #[test]
    fn notify_master_key_degraded_emits_operator_notification_with_progress() {
        let (tx, mut rx) = broadcast::channel(4);
        notify_master_key_degraded(Some(&tx), "scrypt mismatch", 3, 7);

        let event = rx.try_recv().expect("event");
        match event {
            NexusEvent::OperatorNotification { kind, data } => {
                assert_eq!(kind, "master_key_degraded");
                assert!(data.get("reason").is_some());
                assert!(data.get("completed").is_some());
                assert!(data.get("total").is_some());
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn notify_master_key_degraded_without_tx_is_silent() {
        // Just exercises the `event_tx = None` branch; no panic
        // means the early return path works.
        notify_master_key_degraded(None, "no listener", 0, 0);
    }
}
