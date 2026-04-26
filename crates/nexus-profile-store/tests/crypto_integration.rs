//! Integration tests that exercise encryption end-to-end through
//! `ProfileFileStore`. Unit tests on [`Cipher`] live in the lib;
//! these cover user-prompt requirements that hit the full
//! write→read pipeline.

use nexus_core::{MacAddr, Ssid};
use nexus_profile_store::{
    DerivedKeySource, InMemoryKeySource, ProfileFileStore, ProfileMetadata, ProfileStore,
    SecretString, SecurityConfig, WifiNetworkSettings, WifiProfile, WpaPsk,
};
use tempfile::TempDir;
use ulid::Ulid;

fn sample_wifi() -> WifiProfile {
    WifiProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        network: WifiNetworkSettings {
            ssid: Ssid::new(b"corp-wifi".to_vec()).unwrap(),
            hidden: false,
            priority: 10,
            auto_connect: true,
            fast_transition: true,
            security: SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(SecretString::from("correct horse battery staple")),
            },
            bssid_preferred: Some(MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])),
            bssid_blacklist: vec![],
            scan_freqs: vec![],
            credentials_invalid: false,
            last_connected_at: None,
        },
    }
}

#[tokio::test]
async fn plaintext_passphrase_never_appears_on_disk() {
    let dir = TempDir::new().unwrap();
    let source = InMemoryKeySource::new([0xBE; 32]);
    let store = ProfileFileStore::open(dir.path(), &source).unwrap();
    let profile = sample_wifi();
    store.put_wifi(&profile).await.unwrap();

    // Scan every file under the profiles root for the plaintext.
    for entry in walk(dir.path()) {
        let bytes = std::fs::read(&entry).unwrap();
        let as_str = String::from_utf8_lossy(&bytes);
        assert!(
            !as_str.contains("correct horse battery staple"),
            "plaintext leaked into {}",
            entry.display(),
        );
    }
}

#[tokio::test]
async fn ciphertext_differs_between_writes_of_the_same_profile() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();
    let source = InMemoryKeySource::new([0xBE; 32]);

    let store_a = ProfileFileStore::open(dir_a.path(), &source).unwrap();
    let store_b = ProfileFileStore::open(dir_b.path(), &source).unwrap();

    let profile = sample_wifi();
    store_a.put_wifi(&profile).await.unwrap();
    store_b.put_wifi(&profile).await.unwrap();

    let a = find_wifi_toml(dir_a.path());
    let b = find_wifi_toml(dir_b.path());
    let a_bytes = std::fs::read(&a).unwrap();
    let b_bytes = std::fs::read(&b).unwrap();
    assert_ne!(
        a_bytes, b_bytes,
        "fresh-nonce invariant: two writes of the same profile must differ on disk",
    );
}

#[tokio::test]
async fn decryption_fails_under_a_different_master_key() {
    let dir = TempDir::new().unwrap();

    // Write with key A.
    {
        let store =
            ProfileFileStore::open(dir.path(), &InMemoryKeySource::new([0xAA; 32])).unwrap();
        store.put_wifi(&sample_wifi()).await.unwrap();
    }

    // Reopen with key B. load_wifi should warn-skip the profile
    // (AEAD rejection) and return an empty vec rather than the
    // plaintext.
    {
        let store =
            ProfileFileStore::open(dir.path(), &InMemoryKeySource::new([0xBB; 32])).unwrap();
        let loaded = store.load_wifi().await.unwrap();
        assert!(
            loaded.is_empty(),
            "profile must not decrypt under the wrong master key",
        );
    }
}

#[tokio::test]
async fn derived_source_roundtrip_with_matching_password() {
    let dir = TempDir::new().unwrap();
    let password = SecretString::from("it was the best of times");
    let salt = *b"0123456789abcdef";
    let params = nexus_profile_store::keys::derived_source::ScryptParams::testing();

    let source = DerivedKeySource::new(password.clone(), salt).with_params(params);
    let store = ProfileFileStore::open(dir.path(), &source).unwrap();
    let profile = sample_wifi();
    store.put_wifi(&profile).await.unwrap();

    // Reopen with the same password; the profile decrypts.
    let source2 = DerivedKeySource::new(password, salt).with_params(params);
    let store2 = ProfileFileStore::open(dir.path(), &source2).unwrap();
    let loaded = store2.load_wifi().await.unwrap();
    assert_eq!(loaded.len(), 1);
    match &loaded[0].network.security {
        SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(s),
        } => {
            assert_eq!(s.expose_secret(), "correct horse battery staple");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn derived_source_wrong_password_fails_to_decrypt() {
    let dir = TempDir::new().unwrap();
    let salt = *b"0123456789abcdef";
    let params = nexus_profile_store::keys::derived_source::ScryptParams::testing();

    let correct =
        DerivedKeySource::new(SecretString::from("the real password"), salt).with_params(params);
    let store = ProfileFileStore::open(dir.path(), &correct).unwrap();
    store.put_wifi(&sample_wifi()).await.unwrap();

    let wrong = DerivedKeySource::new(SecretString::from("guess"), salt).with_params(params);
    let store2 = ProfileFileStore::open(dir.path(), &wrong).unwrap();
    let loaded = store2.load_wifi().await.unwrap();
    assert!(
        loaded.is_empty(),
        "wrong password must not yield a decrypted profile",
    );
}

fn find_wifi_toml(root: &std::path::Path) -> std::path::PathBuf {
    for entry in std::fs::read_dir(root.join("wifi")).unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension().and_then(|s| s.to_str()) == Some("toml") {
            return entry.path();
        }
    }
    panic!("no wifi profile on disk under {}", root.display());
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out
}
