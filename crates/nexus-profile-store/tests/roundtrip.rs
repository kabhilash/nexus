//! End-to-end profile roundtrip tests against `ProfileFileStore`.
//!
//! These tests cover the user-prompt test list: every profile kind
//! roundtrips through serialize → write → read → deserialize; malformed
//! files are logged and skipped; file permissions land at `0600`.
//! Atomic-write crash simulation lives alongside in
//! `tests/atomic_write.rs`.

use std::collections::BTreeMap;
use std::time::Duration;

use nexus_core::{MacAddr, Ssid};
use nexus_profile_store::{
    BluetoothProfile, Dot1xEapConfig, Dot1xSettings, EapMethod, EthInterfaceSettings,
    EthernetProfile, GnssDeviceProfile, InMemoryKeySource, ProfileFileStore, ProfileMetadata,
    ProfileStore, SecretString, SecurityConfig, WifiNetworkSettings, WifiProfile, WpaPsk,
    ssid_hash,
};
use tempfile::TempDir;
use ulid::Ulid;

fn sample_ethernet(id: Ulid, ifname: &str) -> EthernetProfile {
    EthernetProfile {
        id,
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        interface: EthInterfaceSettings {
            name: ifname.to_owned(),
            auto_connect: true,
        },
        dot1x: Some(Dot1xSettings {
            enabled: true,
            eap: Dot1xEapConfig {
                eap: EapMethod::Peap,
                identity: "user@corp.example.com".into(),
                anonymous_identity: Some("anonymous@corp.example.com".into()),
                ca_cert: Some("/etc/nexus/certs/corp-ca.pem".into()),
                client_cert: None,
                client_key: None,
                client_key_password: None,
                phase2: Some("auth=MSCHAPV2".into()),
                domain_suffix_match: Some("corp.example.com".into()),
                password: Some(SecretString::from("hunter2")),
            },
        }),
    }
}

fn sample_wifi(id: Ulid, ssid_bytes: &[u8]) -> WifiProfile {
    WifiProfile {
        id,
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        network: WifiNetworkSettings {
            ssid: Ssid::new(ssid_bytes.to_vec()).unwrap(),
            hidden: false,
            priority: 10,
            auto_connect: true,
            fast_transition: true,
            security: SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(SecretString::from("correct horse battery staple")),
            },
            bssid_preferred: Some(MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF])),
            bssid_blacklist: vec![MacAddr([0x11; 6])],
            scan_freqs: vec![2412, 5180],
            credentials_invalid: false,
        },
    }
}

fn sample_gnss(id: Ulid) -> GnssDeviceProfile {
    GnssDeviceProfile {
        id,
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        device_path: "/dev/ttyUSB0".into(),
        label: Some("external u-blox F9P".into()),
        max_rate_hz: Some(10),
        min_horizontal_error_m: Some(1.5),
        auto_attach: true,
    }
}

fn sample_bluetooth(id: Ulid) -> BluetoothProfile {
    BluetoothProfile {
        id,
        schema_version: 1,
        metadata: ProfileMetadata::default(),
        adapter_path: "/org/bluez/hci0".into(),
        device_address: MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]),
        device_name: Some("Pixel 8".into()),
        auto_connect: true,
        auto_accept_incoming: false,
        preferences: BTreeMap::from([("profile_priority".into(), "a2dp".into())]),
    }
}

fn fresh_store() -> (TempDir, ProfileFileStore) {
    let dir = TempDir::new().expect("tempdir");
    let source = InMemoryKeySource::new([0x42; 32]);
    let store = ProfileFileStore::open(dir.path(), &source).expect("open store");
    (dir, store)
}

#[tokio::test]
async fn ethernet_roundtrip_preserves_every_field() {
    let (_dir, store) = fresh_store();
    let original = sample_ethernet(Ulid::new(), "eth0");
    store.put_ethernet(&original).await.unwrap();

    let reloaded = store
        .load_ethernet_profile("eth0")
        .await
        .unwrap()
        .expect("profile should be present");
    assert_eq!(reloaded, original);

    let list = store.load_ethernet().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0], original);
}

#[tokio::test]
async fn wifi_roundtrip_preserves_every_field_including_secrets() {
    let (_dir, store) = fresh_store();
    let original = sample_wifi(Ulid::new(), b"nexus-net");
    store.put_wifi(&original).await.unwrap();

    let list = store.load_wifi().await.unwrap();
    assert_eq!(list.len(), 1);
    let reloaded = &list[0];
    assert_eq!(reloaded, &original);

    match &reloaded.network.security {
        SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(s),
        } => {
            assert_eq!(s.expose_secret(), "correct horse battery staple");
        }
        other => panic!("unexpected security config: {other:?}"),
    }
}

#[tokio::test]
async fn gnss_roundtrip() {
    let (_dir, store) = fresh_store();
    let original = sample_gnss(Ulid::new());
    store.put_gnss(&original).await.unwrap();

    let reloaded = store
        .load_gnss_profile_by_path("/dev/ttyUSB0")
        .await
        .unwrap()
        .expect("present");
    assert_eq!(reloaded, original);
}

#[tokio::test]
async fn bluetooth_roundtrip_and_lookup_by_address() {
    let (_dir, store) = fresh_store();
    let original = sample_bluetooth(Ulid::new());
    store.put_bluetooth(&original).await.unwrap();

    let reloaded = store
        .load_bluetooth_profile_by_address(&MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]))
        .await
        .unwrap()
        .expect("present");
    assert_eq!(reloaded, original);
}

#[tokio::test]
async fn put_uses_0600_permissions() {
    let (dir, store) = fresh_store();
    let profile = sample_ethernet(Ulid::new(), "eth0");
    store.put_ethernet(&profile).await.unwrap();

    let path = dir.path().join("ethernet").join("eth0.toml");
    let mode = nexus_profile_store::fs_store::file_mode(&path).unwrap();
    assert_eq!(mode, 0o600, "profile file should be 0600, got {mode:o}");
}

#[tokio::test]
async fn put_uses_0700_directory_mode() {
    let (dir, _store) = fresh_store();
    let subdir = dir.path().join("ethernet");
    let mode = nexus_profile_store::fs_store::file_mode(&subdir).unwrap();
    assert_eq!(mode, 0o700, "profile dir should be 0700, got {mode:o}");
}

#[tokio::test]
async fn malformed_file_is_skipped_on_load() {
    let (dir, store) = fresh_store();
    store
        .put_ethernet(&sample_ethernet(Ulid::new(), "eth0"))
        .await
        .unwrap();

    // Inject a bogus file in the same directory.
    let bad = dir.path().join("ethernet").join("nonsense.toml");
    std::fs::write(&bad, b"this is not valid TOML at all ===\x00").unwrap();

    let all = store.load_ethernet().await.unwrap();
    assert_eq!(all.len(), 1, "valid profile should still load");
    assert_eq!(all[0].interface.name, "eth0");
}

#[tokio::test]
async fn remove_is_noop_when_missing() {
    let (_dir, store) = fresh_store();
    store.remove_ethernet("does-not-exist").await.unwrap();
}

#[tokio::test]
async fn ssid_hash_filename_is_stable_on_disk() {
    let (dir, store) = fresh_store();
    let profile = sample_wifi(Ulid::new(), b"nexus-net");
    store.put_wifi(&profile).await.unwrap();

    let expected = ssid_hash(&profile.network.ssid);
    let path = dir.path().join("wifi").join(format!("{expected}.toml"));
    assert!(
        path.exists(),
        "wifi file should be keyed by ssid hash at {}",
        path.display(),
    );
}

#[tokio::test]
async fn rotate_master_key_returns_not_yet_implemented() {
    let (_dir, store) = fresh_store();
    let result = store.rotate_master_key().await;
    assert!(matches!(
        result,
        Err(nexus_profile_store::StoreError::NotYetImplemented(_)),
    ));
    let _ = Duration::from_secs(0); // silence "unused import"
}

/// Dummy implementation proving the trait compiles against its use
/// sites. Lives in the test binary so it never ships to production.
#[allow(dead_code)]
struct DummyStore;

#[async_trait::async_trait]
impl ProfileStore for DummyStore {
    async fn load_ethernet(&self) -> nexus_profile_store::Result<Vec<EthernetProfile>> {
        Ok(Vec::new())
    }
    async fn load_ethernet_profile(
        &self,
        _ifname: &str,
    ) -> nexus_profile_store::Result<Option<EthernetProfile>> {
        Ok(None)
    }
    async fn put_ethernet(&self, _profile: &EthernetProfile) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn remove_ethernet(&self, _ifname: &str) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn load_wifi(&self) -> nexus_profile_store::Result<Vec<WifiProfile>> {
        Ok(Vec::new())
    }
    async fn put_wifi(&self, _profile: &WifiProfile) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn remove_wifi(&self, _ssid_hash: &str) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn load_gnss(&self) -> nexus_profile_store::Result<Vec<GnssDeviceProfile>> {
        Ok(Vec::new())
    }
    async fn load_gnss_profile_by_path(
        &self,
        _device_path: &str,
    ) -> nexus_profile_store::Result<Option<GnssDeviceProfile>> {
        Ok(None)
    }
    async fn put_gnss(&self, _profile: &GnssDeviceProfile) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn remove_gnss(&self, _id: &Ulid) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn load_bluetooth(&self) -> nexus_profile_store::Result<Vec<BluetoothProfile>> {
        Ok(Vec::new())
    }
    async fn load_bluetooth_profile_by_address(
        &self,
        _address: &MacAddr,
    ) -> nexus_profile_store::Result<Option<BluetoothProfile>> {
        Ok(None)
    }
    async fn put_bluetooth(&self, _profile: &BluetoothProfile) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn remove_bluetooth(&self, _id: &Ulid) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn set_credentials_invalid(
        &self,
        _reference: nexus_profile_store::ProfileRef<'_>,
        _invalid: bool,
    ) -> nexus_profile_store::Result<()> {
        Ok(())
    }
    async fn rotate_master_key(
        &self,
    ) -> nexus_profile_store::Result<nexus_profile_store::RotateReport> {
        Err(nexus_profile_store::StoreError::NotYetImplemented("dummy"))
    }
}

#[tokio::test]
async fn dummy_store_implementing_trait_compiles_and_returns_empty() {
    let d = DummyStore;
    assert!(d.load_ethernet().await.unwrap().is_empty());
    assert!(d.load_wifi().await.unwrap().is_empty());
    assert!(d.load_gnss().await.unwrap().is_empty());
    assert!(d.load_bluetooth().await.unwrap().is_empty());
}
