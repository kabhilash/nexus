//! Filesystem-backed implementation of [`ProfileStore`] with
//! encryption wired in. See DD-007 §§4-6.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use nexus_core::MacAddr;
use ulid::Ulid;
use zeroize::Zeroize;

use crate::crypto::{ChaChaCipher, derive_file_key};
use crate::error::{Result, StoreError};
use crate::keys::MasterKeySource;
use crate::trait_def::{ProfileRef, ProfileStore, RotateReport};
use crate::types::bluetooth::BluetoothProfile;
use crate::types::ethernet::{
    EthernetProfile, EthernetProfileOnDisk, decrypt_ethernet, encrypt_ethernet,
};
use crate::types::gnss::GnssDeviceProfile;
use crate::types::wifi::{WifiProfile, WifiProfileOnDisk, decrypt_wifi, encrypt_wifi};

/// File mode for every profile file (`0600`, owner rw).
const FILE_MODE: u32 = 0o600;
/// Directory mode for every profile directory (`0700`, owner rwx).
const DIR_MODE: u32 = 0o700;

const DIR_ETHERNET: &str = "ethernet";
const DIR_WIFI: &str = "wifi";
const DIR_GNSS: &str = "gnss";
const DIR_BLUETOOTH: &str = "bluetooth";

/// Filesystem-backed profile store with encrypted credentials.
pub struct ProfileFileStore {
    root: PathBuf,
    master_key: MasterKey,
    source_name: &'static str,
}

impl std::fmt::Debug for ProfileFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileFileStore")
            .field("root", &self.root)
            .field("master_key", &"<redacted>")
            .field("source", &self.source_name)
            .finish()
    }
}

impl ProfileFileStore {
    /// Open a store rooted at `root`, pulling the master key from
    /// `source`. Creates the per-kind subdirectories if absent.
    pub fn open(root: impl Into<PathBuf>, source: &dyn MasterKeySource) -> Result<Self> {
        let key = source.master_key().map_err(|e| StoreError::RootDir {
            path: PathBuf::new(),
            source: io::Error::other(format!("master key source '{}': {e}", source.name())),
        })?;
        Self::open_with_key(root, key, source.name())
    }

    /// Lower-level constructor for cases where the caller already
    /// holds an unwrapped key (tests, explicit in-memory sources).
    pub fn open_with_key(
        root: impl Into<PathBuf>,
        key: [u8; 32],
        source_name: &'static str,
    ) -> Result<Self> {
        let root = root.into();
        mkdir_p(&root)?;
        for sub in [DIR_ETHERNET, DIR_WIFI, DIR_GNSS, DIR_BLUETOOTH] {
            mkdir_p(&root.join(sub))?;
        }
        Ok(Self {
            root,
            master_key: MasterKey::new(key),
            source_name,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Name of the active master-key source (`"file"`, `"tpm"`,
    /// `"derived"`, `"memory"`). Used by observability code and
    /// log messages.
    pub fn source_name(&self) -> &'static str {
        self.source_name
    }

    fn dir(&self, sub: &str) -> PathBuf {
        self.root.join(sub)
    }

    fn ethernet_path(&self, ifname: &str) -> PathBuf {
        self.dir(DIR_ETHERNET).join(format!("{ifname}.toml"))
    }

    fn wifi_path(&self, ssid_hash: &str) -> PathBuf {
        self.dir(DIR_WIFI).join(format!("{ssid_hash}.toml"))
    }

    fn gnss_path(&self, id: &Ulid) -> PathBuf {
        self.dir(DIR_GNSS).join(format!("{id}.toml"))
    }

    fn bluetooth_path(&self, id: &Ulid) -> PathBuf {
        self.dir(DIR_BLUETOOTH).join(format!("{id}.toml"))
    }

    /// Construct the per-file cipher for a profile. The profile's
    /// ULID drives HKDF; every file thus has a distinct key.
    fn cipher_for(&self, id: &Ulid) -> ChaChaCipher {
        let id_bytes = id.to_bytes();
        let file_key = derive_file_key(&self.master_key.0, &id_bytes);
        ChaChaCipher::new(file_key)
    }
}

#[async_trait]
impl ProfileStore for ProfileFileStore {
    async fn load_ethernet(&self) -> Result<Vec<EthernetProfile>> {
        let mut out = Vec::new();
        for on_disk in load_all::<EthernetProfileOnDisk>(&self.dir(DIR_ETHERNET))? {
            let cipher = self.cipher_for(&on_disk.id);
            match decrypt_ethernet(on_disk, &cipher) {
                Ok(p) => out.push(p),
                Err(e) => {
                    tracing::warn!(error = %e, "ethernet profile failed to decrypt; skipping")
                }
            }
        }
        out.sort_by_key(|p| p.id);
        Ok(out)
    }

    async fn load_ethernet_profile(&self, ifname: &str) -> Result<Option<EthernetProfile>> {
        let path = self.ethernet_path(ifname);
        match load_one::<EthernetProfileOnDisk>(&path)? {
            Some(on_disk) => {
                let cipher = self.cipher_for(&on_disk.id);
                match decrypt_ethernet(on_disk, &cipher) {
                    Ok(p) => Ok(Some(p)),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "decrypt failed");
                        Ok(None)
                    }
                }
            }
            None => Ok(None),
        }
    }

    async fn put_ethernet(&self, profile: &EthernetProfile) -> Result<()> {
        let cipher = self.cipher_for(&profile.id);
        let on_disk = encrypt_ethernet(profile, &cipher).map_err(|e| {
            StoreError::malformed(self.ethernet_path(&profile.interface.name), e.to_string())
        })?;
        let path = self.ethernet_path(&profile.interface.name);
        write_atomic_toml(&path, &on_disk)
    }

    async fn remove_ethernet(&self, ifname: &str) -> Result<()> {
        remove_if_present(&self.ethernet_path(ifname))
    }

    async fn load_wifi(&self) -> Result<Vec<WifiProfile>> {
        let mut out = Vec::new();
        for on_disk in load_all::<WifiProfileOnDisk>(&self.dir(DIR_WIFI))? {
            let cipher = self.cipher_for(&on_disk.id);
            match decrypt_wifi(on_disk, &cipher) {
                Ok(p) => out.push(p),
                Err(e) => tracing::warn!(error = %e, "wifi profile failed to decrypt; skipping"),
            }
        }
        out.sort_by_key(|p| p.id);
        Ok(out)
    }

    async fn put_wifi(&self, profile: &WifiProfile) -> Result<()> {
        let cipher = self.cipher_for(&profile.id);
        let on_disk = encrypt_wifi(profile, &cipher)
            .map_err(|e| StoreError::malformed(self.wifi_path(""), e.to_string()))?;
        let hash = ssid_hash(&profile.network.ssid);
        let path = self.wifi_path(&hash);
        write_atomic_toml(&path, &on_disk)
    }

    async fn remove_wifi(&self, ssid_hash: &str) -> Result<()> {
        remove_if_present(&self.wifi_path(ssid_hash))
    }

    async fn load_gnss(&self) -> Result<Vec<GnssDeviceProfile>> {
        let mut out: Vec<GnssDeviceProfile> = load_all::<GnssDeviceProfile>(&self.dir(DIR_GNSS))?;
        out.sort_by_key(|p| p.id);
        Ok(out)
    }

    async fn load_gnss_profile_by_path(
        &self,
        device_path: &str,
    ) -> Result<Option<GnssDeviceProfile>> {
        let mut matches: Vec<GnssDeviceProfile> = self
            .load_gnss()
            .await?
            .into_iter()
            .filter(|p| p.device_path == device_path)
            .collect();
        matches.sort_by_key(|p| p.id);
        Ok(matches.into_iter().next())
    }

    async fn put_gnss(&self, profile: &GnssDeviceProfile) -> Result<()> {
        let path = self.gnss_path(&profile.id);
        write_atomic_toml(&path, profile)
    }

    async fn remove_gnss(&self, id: &Ulid) -> Result<()> {
        remove_if_present(&self.gnss_path(id))
    }

    async fn load_bluetooth(&self) -> Result<Vec<BluetoothProfile>> {
        let mut out: Vec<BluetoothProfile> =
            load_all::<BluetoothProfile>(&self.dir(DIR_BLUETOOTH))?;
        out.sort_by_key(|p| p.id);
        Ok(out)
    }

    async fn load_bluetooth_profile_by_address(
        &self,
        address: &MacAddr,
    ) -> Result<Option<BluetoothProfile>> {
        let mut matches: Vec<BluetoothProfile> = self
            .load_bluetooth()
            .await?
            .into_iter()
            .filter(|p| &p.device_address == address)
            .collect();
        matches.sort_by_key(|p| p.id);
        Ok(matches.into_iter().next())
    }

    async fn put_bluetooth(&self, profile: &BluetoothProfile) -> Result<()> {
        let path = self.bluetooth_path(&profile.id);
        write_atomic_toml(&path, profile)
    }

    async fn remove_bluetooth(&self, id: &Ulid) -> Result<()> {
        remove_if_present(&self.bluetooth_path(id))
    }

    async fn set_credentials_invalid(
        &self,
        reference: ProfileRef<'_>,
        invalid: bool,
    ) -> Result<()> {
        match reference {
            ProfileRef::Wifi { ssid_hash } => {
                let path = self.wifi_path(ssid_hash);
                let mut on_disk: WifiProfileOnDisk = match load_one(&path)? {
                    Some(p) => p,
                    None => return Ok(()),
                };
                on_disk.network.credentials_invalid = invalid;
                write_atomic_toml(&path, &on_disk)
            }
            ProfileRef::Ethernet { .. }
            | ProfileRef::Gnss { .. }
            | ProfileRef::Bluetooth { .. } => Ok(()),
        }
    }

    async fn rotate_master_key(&self) -> Result<RotateReport> {
        Err(StoreError::NotYetImplemented("rotate_master_key"))
    }
}

// ---------------------------------------------------------------------------
// Master key zeroization wrapper.
// ---------------------------------------------------------------------------

struct MasterKey([u8; 32]);

impl MasterKey {
    fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl Drop for MasterKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

// ---------------------------------------------------------------------------
// SSID hashing (DD-007 §3.2).
// ---------------------------------------------------------------------------

pub fn ssid_hash(ssid: &nexus_core::Ssid) -> String {
    let hash = blake3::hash(ssid.as_bytes());
    hex_encode_short(&hash.as_bytes()[..8])
}

fn hex_encode_short(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

// ---------------------------------------------------------------------------
// Filesystem helpers
// ---------------------------------------------------------------------------

fn mkdir_p(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    DirBuilder::new()
        .recursive(true)
        .mode(DIR_MODE)
        .create(path)
        .map_err(|e| StoreError::RootDir {
            path: path.to_owned(),
            source: e,
        })
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(StoreError::io(path, e)),
    }
}

fn load_all<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(StoreError::io(dir, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::io(dir, e))?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("toml") {
            continue;
        }
        if let Some(parsed) = load_one::<T>(&path)? {
            out.push(parsed);
        }
    }
    Ok(out)
}

fn load_one<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(StoreError::io(path, e)),
    };
    let text = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "profile not valid UTF-8; skipping");
            return Ok(None);
        }
    };
    match toml::from_str::<T>(text) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "profile fails to deserialize; skipping");
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic write (§6.2)
// ---------------------------------------------------------------------------

fn write_atomic_toml<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let contents = toml::to_string_pretty(value)?;
    write_atomic(path, contents.as_bytes())
}

pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| StoreError::io(path, io::Error::from(io::ErrorKind::InvalidInput)))?;
    mkdir_p(dir)?;

    let tmp = temp_path_near(path);

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_CLOEXEC)
        .open(&tmp)
        .map_err(|e| StoreError::io(&tmp, e))?;

    let io_result = (|| -> io::Result<()> {
        file.write_all(contents)?;
        file.flush()?;
        fsync_fd(file.as_raw_fd())?;
        Ok(())
    })();
    if let Err(e) = io_result {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(StoreError::io(&tmp, e));
    }
    drop(file);

    if test_hooks::crash_after_tmp() {
        panic!("simulated crash after tmp fsync, before rename");
    }

    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(StoreError::io(path, e));
    }

    fsync_dir(dir).map_err(|e| StoreError::io(dir, e))?;
    Ok(())
}

fn temp_path_near(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let original = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    let tmp_name = format!(".{original}.nexus.tmp.{pid}.{seq}.{nanos}");
    path.with_file_name(tmp_name)
}

fn fsync_fd(fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: fsync(2) takes a valid open fd owned by the caller.
    let rc = unsafe { libc::fsync(fd) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fsync_dir(dir: &Path) -> io::Result<()> {
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(dir)?;
    fsync_fd(f.as_raw_fd())
}

/// Inspect the current mode of a file. Exposed publicly so
/// integration tests can assert on `0600` / `0700`.
pub fn file_mode(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(path)?;
    Ok(meta.mode() & 0o777)
}

// ---------------------------------------------------------------------------
// Test hooks
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub mod test_hooks {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static CRASH_AFTER_TMP: AtomicBool = AtomicBool::new(false);

    fn lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    pub fn crash_after_tmp() -> bool {
        CRASH_AFTER_TMP.load(Ordering::SeqCst)
    }

    pub struct CrashAfterTmpGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl CrashAfterTmpGuard {
        pub fn new() -> Self {
            let guard = lock().lock().unwrap_or_else(|p| p.into_inner());
            CRASH_AFTER_TMP.store(true, Ordering::SeqCst);
            Self { _lock: guard }
        }
    }

    impl Default for CrashAfterTmpGuard {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for CrashAfterTmpGuard {
        fn drop(&mut self) {
            CRASH_AFTER_TMP.store(false, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod meta_tests {
    use super::*;

    #[test]
    fn ssid_hash_is_stable_lowercase_hex() {
        let a = ssid_hash(&nexus_core::Ssid::new(b"nexus-net".to_vec()).unwrap());
        let b = ssid_hash(&nexus_core::Ssid::new(b"nexus-net".to_vec()).unwrap());
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_ssids_hash_differently() {
        let a = ssid_hash(&nexus_core::Ssid::new(b"network-a".to_vec()).unwrap());
        let b = ssid_hash(&nexus_core::Ssid::new(b"network-b".to_vec()).unwrap());
        assert_ne!(a, b);
    }

    #[test]
    fn debug_impl_redacts_master_key() {
        let store = ProfileFileStore::open_with_key(
            tempfile::tempdir().unwrap().path(),
            [0x42; 32],
            "memory",
        )
        .unwrap();
        let rendered = format!("{store:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("42, 42, 42"));
    }
}
