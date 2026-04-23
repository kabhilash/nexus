//! Phase 6-9 tests: schema migration, master-key rotation,
//! crash-recovery through the key ring, and quarantine on decrypt
//! failure.

use std::fs;
use std::panic;
use std::path::Path;
use std::time::Duration;

use nexus_core::{NexusEvent, Ssid};
use nexus_profile_store::{
    CURRENT_STORE_VERSION, InMemoryKeySource, ProfileFileStore, ProfileMetadata, ProfileStore,
    SecretString, SecurityConfig, WifiNetworkSettings, WifiProfile, WpaPsk, rotate_to_key,
    ssid_hash,
};
use tempfile::TempDir;
use tokio::sync::broadcast;
use ulid::Ulid;

fn sample_wifi(ssid: &[u8]) -> WifiProfile {
    WifiProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        network: WifiNetworkSettings {
            ssid: Ssid::new(ssid.to_vec()).unwrap(),
            hidden: false,
            priority: 10,
            auto_connect: true,
            fast_transition: false,
            security: SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(SecretString::from("correct horse battery staple")),
            },
            bssid_preferred: None,
            bssid_blacklist: vec![],
            scan_freqs: vec![],
            credentials_invalid: false,
        },
    }
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn schema_migration_runs_on_open_when_store_is_behind() {
    let dir = TempDir::new().unwrap();
    let source = InMemoryKeySource::new([0x42; 32]);

    // Seed the store at version 1: write a v1 profile, then drop
    // the version file back to "1" to simulate a pre-migration
    // store shape.
    {
        let store = ProfileFileStore::open(dir.path(), &source).unwrap();
        let profile = sample_wifi(b"before-migration");
        store.put_wifi(&profile).await.unwrap();
    }
    fs::write(dir.path().join("version"), "1\n").unwrap();

    // Force the on-disk schema_version back to 1 too so the v1→v2
    // migrator actually has something to bump.
    let wifi_path = dir.path().join("wifi").join(format!(
        "{}.toml",
        ssid_hash(&Ssid::new(b"before-migration".to_vec()).unwrap())
    ));
    let body = fs::read_to_string(&wifi_path).unwrap();
    let body = body.replace("schema_version = 2", "schema_version = 1");
    fs::write(&wifi_path, body).unwrap();

    // Re-open: migration should run.
    let _store = ProfileFileStore::open(dir.path(), &source).unwrap();

    // Store version file is now CURRENT_STORE_VERSION.
    let version = fs::read_to_string(dir.path().join("version")).unwrap();
    assert_eq!(
        version.trim().parse::<u32>().unwrap(),
        CURRENT_STORE_VERSION
    );

    // Per-file schema_version is now 2.
    let body = fs::read_to_string(&wifi_path).unwrap();
    assert!(
        body.contains("schema_version = 2"),
        "migrated file should have schema_version = 2, got:\n{body}",
    );
}

// ---------------------------------------------------------------------------
// Rotation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rotation_rewrites_every_profile_under_new_key() {
    let dir = TempDir::new().unwrap();
    let source = InMemoryKeySource::new([0x01; 32]);
    let store = ProfileFileStore::open(dir.path(), &source).unwrap();

    // Seed 10 Wi-Fi profiles.
    for i in 0..10 {
        let ssid = format!("network-{i}");
        store.put_wifi(&sample_wifi(ssid.as_bytes())).await.unwrap();
    }

    // Rotate to a fresh key.
    let new_key = [0xAB; 32];
    let report = rotate_to_key(&store, new_key).await.unwrap();
    assert_eq!(report.profiles_rewritten, 10);

    // After rotation, the store loads them all under the new active.
    let loaded = store.load_wifi().await.unwrap();
    assert_eq!(loaded.len(), 10);

    // And a fresh store opened with the OLD key cannot decrypt any
    // of them — the on-disk files are now under the new key only.
    let old_source = InMemoryKeySource::new([0x01; 32]);
    let old_store = ProfileFileStore::open(dir.path(), &old_source).unwrap();
    let under_old = old_store.load_wifi().await.unwrap();
    assert!(
        under_old.is_empty(),
        "no profile should decrypt under the pre-rotation key",
    );
}

#[tokio::test]
async fn rotation_emits_progress_events() {
    let dir = TempDir::new().unwrap();
    let source = InMemoryKeySource::new([0x02; 32]);
    let (tx, mut rx) = broadcast::channel(64);
    let store = ProfileFileStore::open(dir.path(), &source)
        .unwrap()
        .with_event_tx(tx);

    for i in 0..25 {
        let ssid = format!("net-{i}");
        store.put_wifi(&sample_wifi(ssid.as_bytes())).await.unwrap();
    }

    let _ = rotate_to_key(&store, [0xFF; 32]).await.unwrap();

    let mut progress_events = 0;
    let mut max_completed = 0u32;
    let mut saw_total_25 = false;
    while let Ok(event) = rx.try_recv() {
        if let NexusEvent::ProfileStoreRotationProgress { completed, total } = event {
            progress_events += 1;
            max_completed = max_completed.max(completed);
            if total == 25 {
                saw_total_25 = true;
            }
        }
    }
    assert!(progress_events > 0, "at least one progress event expected");
    assert!(saw_total_25, "events should report total=25");
    assert_eq!(max_completed, 25, "final progress should reach total");
}

// ---------------------------------------------------------------------------
// Crash recovery — partial rotation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rotation_crash_leaves_previous_key_active_for_unrotated_profiles() {
    let dir = TempDir::new().unwrap();
    let source = InMemoryKeySource::new([0x03; 32]);
    let store = ProfileFileStore::open(dir.path(), &source).unwrap();

    // 10 profiles. Rotation crashes after 5 are re-encrypted.
    for i in 0..10 {
        let ssid = format!("crash-{i}");
        store.put_wifi(&sample_wifi(ssid.as_bytes())).await.unwrap();
    }

    let outcome = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let _guard = nexus_profile_store::fs_store::test_hooks::CrashAfterRotateGuard::new(5);
        futures_like_blocking(rotate_to_key(&store, [0xCD; 32]))
    }));
    assert!(outcome.is_err(), "rotation should have panicked");

    // The in-process store still has previous+active in its keyring,
    // so subsequent loads succeed via fallback for the 5 that weren't
    // rotated and directly for the 5 that were.
    let loaded = store.load_wifi().await.unwrap();
    assert_eq!(
        loaded.len(),
        10,
        "all profiles must still decrypt via the active+previous ring",
    );
}

/// Block a future to completion on the current thread. Used by the
/// panic-catching test since `catch_unwind` can't cross `.await`.
fn futures_like_blocking<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Handle::current().block_on(f)
}

// ---------------------------------------------------------------------------
// Quarantine
// ---------------------------------------------------------------------------

#[tokio::test]
async fn truncated_ciphertext_moves_to_quarantine_on_load() {
    let dir = TempDir::new().unwrap();
    let source = InMemoryKeySource::new([0x04; 32]);
    let (tx, mut rx) = broadcast::channel(16);
    let store = ProfileFileStore::open(dir.path(), &source)
        .unwrap()
        .with_event_tx(tx);

    // Write one valid profile and then corrupt its ciphertext.
    let profile = sample_wifi(b"to-be-corrupted");
    store.put_wifi(&profile).await.unwrap();

    let hash = ssid_hash(&profile.network.ssid);
    let path = dir.path().join("wifi").join(format!("{hash}.toml"));
    let body = fs::read_to_string(&path).unwrap();
    // Replace every `A` in the ct field with `B` — scrambles the
    // base64 payload enough to fail Poly1305 verification while
    // keeping TOML structure valid.
    let body = corrupt_ct_field(&body);
    fs::write(&path, body).unwrap();

    // Load: the broken profile should be moved to quarantine.
    let loaded = store.load_wifi().await.unwrap();
    assert!(
        loaded.is_empty(),
        "corrupted profile must not be returned; instead quarantined",
    );

    let quarantine_dir = dir.path().join(".quarantine");
    assert!(quarantine_dir.exists(), "quarantine dir should exist");
    let entries: Vec<_> = fs::read_dir(&quarantine_dir).unwrap().collect();
    assert!(
        entries.len() >= 2,
        "expected at least the moved file + its .note tombstone",
    );

    // Tombstone references the original filename.
    let tombstone = find_tombstone(&quarantine_dir);
    let body = fs::read_to_string(&tombstone).unwrap();
    assert!(body.contains(&format!("{hash}.toml")));
    assert!(body.contains("kind = \"wifi\""));

    // A ProfileCorrupt event + OperatorNotification fired.
    let mut saw_corrupt = false;
    let mut saw_notification = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            NexusEvent::ProfileCorrupt { .. } => saw_corrupt = true,
            NexusEvent::OperatorNotification { kind, .. } if kind == "profile_corrupt" => {
                saw_notification = true;
            }
            _ => {}
        }
    }
    assert!(saw_corrupt);
    assert!(saw_notification);
}

fn corrupt_ct_field(body: &str) -> String {
    // Flip the first character after `ct = "` so base64 content is
    // different. This preserves byte count (so deserialization of
    // the TOML still succeeds) while changing ciphertext enough that
    // ChaCha20-Poly1305 authentication fails.
    let marker = "ct = \"";
    if let Some(start) = body.find(marker) {
        let pos = start + marker.len();
        let mut bytes = body.as_bytes().to_vec();
        // Swap the byte at `pos` with a different valid base64url
        // character.
        let replacement = if bytes[pos] == b'A' { b'B' } else { b'A' };
        bytes[pos] = replacement;
        return String::from_utf8(bytes).unwrap();
    }
    body.to_owned()
}

fn find_tombstone(dir: &Path) -> std::path::PathBuf {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.ends_with(".note"))
            .unwrap_or(false)
        {
            return path;
        }
    }
    panic!("no tombstone found in {}", dir.display());
}
