//! Demonstrates encrypting a Wi-Fi profile to disk and reading it
//! back. Run with:
//!
//!     cargo run -p nexus-profile-store --example roundtrip
//!
//! The example uses a file-backed master key under a temp dir so
//! successive runs are hermetic.

use std::error::Error;

use nexus_core::{MacAddr, Ssid};
use nexus_profile_store::{
    FileKeySource, ProfileFileStore, ProfileMetadata, ProfileStore, SecretString, SecurityConfig,
    WifiNetworkSettings, WifiProfile, WpaPsk,
};
use tempfile::TempDir;
use ulid::Ulid;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let tmp = TempDir::new()?;
    let root = tmp.path().join("profiles");
    let key_path = tmp.path().join("keys").join("master.key");

    println!("root:    {}", root.display());
    println!("key:     {}", key_path.display());

    let key_source = FileKeySource::new(&key_path);
    let store = ProfileFileStore::open(&root, &key_source)?;

    let profile = WifiProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        network: WifiNetworkSettings {
            ssid: Ssid::new(b"nexus-demo".to_vec())?,
            hidden: false,
            priority: 10,
            auto_connect: true,
            fast_transition: false,
            security: SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(SecretString::from("correct horse battery staple")),
            },
            bssid_preferred: Some(MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])),
            bssid_blacklist: vec![],
            scan_freqs: vec![2412],
            credentials_invalid: false,
            last_connected_at: None,
        },
    };
    println!("writing profile id={} ssid=\"nexus-demo\"", profile.id);

    store.put_wifi(&profile).await?;

    // Inspect the raw on-disk file — the passphrase should not
    // appear in plaintext anywhere.
    for entry in std::fs::read_dir(root.join("wifi"))? {
        let entry = entry?;
        let body = std::fs::read_to_string(entry.path())?;
        println!("--- {} ---", entry.file_name().to_string_lossy());
        println!("{body}");
        assert!(
            !body.contains("correct horse battery staple"),
            "plaintext passphrase leaked to disk!",
        );
    }

    // Reload and confirm the passphrase comes back intact.
    let loaded = store.load_wifi().await?;
    assert_eq!(loaded.len(), 1);
    let loaded = &loaded[0];
    println!(
        "reloaded id={} ssid={:?} priority={}",
        loaded.id, loaded.network.ssid, loaded.network.priority,
    );
    match &loaded.network.security {
        SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(s),
        } => {
            println!("decrypted passphrase: {}", s.expose_secret());
            assert_eq!(s.expose_secret(), "correct horse battery staple");
        }
        other => panic!("unexpected security config: {other:?}"),
    }

    println!("\nroundtrip ok");
    Ok(())
}
