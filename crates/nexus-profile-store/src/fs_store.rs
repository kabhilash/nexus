//! Filesystem-backed implementation of [`ProfileStore`].
//!
//! Phase 2: plaintext credentials. Every write is atomic (write tmp
//! → fsync → rename → fsync parent dir) per DD-007 §6.2. File mode
//! is `0600`, directory mode `0700`.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use nexus_core::MacAddr;
use ulid::Ulid;

use crate::error::{Result, StoreError};
use crate::trait_def::{ProfileRef, ProfileStore, RotateReport};
use crate::types::bluetooth::BluetoothProfile;
use crate::types::ethernet::{EthernetProfile, EthernetProfileOnDisk};
use crate::types::gnss::GnssDeviceProfile;
use crate::types::wifi::{WifiProfile, WifiProfileOnDisk};

/// File mode for every profile file (`0600`, owner rw).
const FILE_MODE: u32 = 0o600;
/// Directory mode for every profile directory (`0700`, owner rwx).
const DIR_MODE: u32 = 0o700;

/// Subdirectory names for each kind. Kept as constants so the
/// ssid-hashing and ULID-encoding callers don't drift.
const DIR_ETHERNET: &str = "ethernet";
const DIR_WIFI: &str = "wifi";
const DIR_GNSS: &str = "gnss";
const DIR_BLUETOOTH: &str = "bluetooth";

/// Filesystem-backed profile store.
#[derive(Debug, Clone)]
pub struct ProfileFileStore {
    root: PathBuf,
}

impl ProfileFileStore {
    /// Open (or create) a store rooted at `root`. Creates the
    /// per-kind subdirectories with mode `0700` if they don't
    /// already exist.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        mkdir_p(&root)?;
        for sub in [DIR_ETHERNET, DIR_WIFI, DIR_GNSS, DIR_BLUETOOTH] {
            mkdir_p(&root.join(sub))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
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
}

#[async_trait]
impl ProfileStore for ProfileFileStore {
    async fn load_ethernet(&self) -> Result<Vec<EthernetProfile>> {
        let mut out: Vec<EthernetProfile> =
            load_all::<EthernetProfileOnDisk>(&self.dir(DIR_ETHERNET))?
                .into_iter()
                .map(EthernetProfile::from)
                .collect();
        out.sort_by_key(|p| p.id);
        Ok(out)
    }

    async fn load_ethernet_profile(&self, ifname: &str) -> Result<Option<EthernetProfile>> {
        let path = self.ethernet_path(ifname);
        match load_one::<EthernetProfileOnDisk>(&path)? {
            Some(on_disk) => Ok(Some(EthernetProfile::from(on_disk))),
            None => Ok(None),
        }
    }

    async fn put_ethernet(&self, profile: &EthernetProfile) -> Result<()> {
        let on_disk = EthernetProfileOnDisk::from(profile);
        let path = self.ethernet_path(&profile.interface.name);
        write_atomic_toml(&path, &on_disk)
    }

    async fn remove_ethernet(&self, ifname: &str) -> Result<()> {
        remove_if_present(&self.ethernet_path(ifname))
    }

    async fn load_wifi(&self) -> Result<Vec<WifiProfile>> {
        let mut out: Vec<WifiProfile> = load_all::<WifiProfileOnDisk>(&self.dir(DIR_WIFI))?
            .into_iter()
            .map(WifiProfile::from)
            .collect();
        out.sort_by_key(|p| p.id);
        Ok(out)
    }

    async fn put_wifi(&self, profile: &WifiProfile) -> Result<()> {
        let on_disk = WifiProfileOnDisk::from(profile);
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
            | ProfileRef::Bluetooth { .. } => {
                // Only Wi-Fi profiles carry a `credentials_invalid`
                // flag in the DD-007 schema. Other kinds accept the
                // call as a no-op rather than erroring so a single
                // caller can operate on a ProfileRef without
                // per-kind branching.
                Ok(())
            }
        }
    }

    async fn rotate_master_key(&self) -> Result<RotateReport> {
        Err(StoreError::NotYetImplemented("rotate_master_key"))
    }
}

// ---------------------------------------------------------------------------
// SSID hashing (DD-007 §3.2: blake3, 16 hex chars)
// ---------------------------------------------------------------------------

/// Hash an SSID to the 16-hex-character filename stub used under
/// `profiles/wifi/`. Exposed so tests and callers upstream can
/// compute the same hash without loading the file first.
pub fn ssid_hash(ssid: &nexus_core::Ssid) -> String {
    let hash = blake3::hash(ssid.as_bytes());
    hex::encode_short(&hash.as_bytes()[..8])
}

/// Local hex module — we only need lowercase encode of a few
/// bytes, not a full crate.
mod hex {
    pub fn encode_short(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }
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

/// Load every `*.toml` file in `dir` whose contents deserialize as
/// `T`. Malformed files are logged at `warn` and skipped per §10.1.
fn load_all<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        // A missing directory means "no profiles yet" — treat as
        // empty rather than error.
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

/// Read a single file and deserialize as `T`. Missing file → `None`.
/// Malformed file → log + `None` (§10.1 soft-quarantine behavior for
/// phase 2; full quarantine lands with the rotation implementation).
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

/// Serialize `value` as TOML and write it atomically to `path`.
fn write_atomic_toml<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
    let contents = toml::to_string_pretty(value)?;
    write_atomic(path, contents.as_bytes())
}

/// Write `contents` atomically to `path`. Temp file in the same
/// directory, fsync'd, rename'd, then the parent directory is
/// fsync'd to make the rename durable.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| StoreError::io(path, io::Error::from(io::ErrorKind::InvalidInput)))?;
    mkdir_p(dir)?;

    let tmp = temp_path_near(path);

    // Create the tmp file with exclusive create + 0600.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_CLOEXEC)
        .open(&tmp)
        .map_err(|e| StoreError::io(&tmp, e))?;

    // Write, flush, fsync. On any I/O error along the way, clean up
    // the tmp file before returning.
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

    // Rename is atomic on POSIX within one directory.
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(StoreError::io(path, e));
    }

    // Flush the directory entry so the rename is durable.
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
    // SAFETY: `fsync(2)` just takes a valid open fd, which the
    // caller still owns through the passed `file` reference.
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
//
// Always compiled in. Production code never flips the flag; the
// guard type is `#[doc(hidden)]` so it doesn't appear in the rustdoc
// surface, and the read on every atomic write is a single
// `AtomicBool` load — negligible cost for the rare write path.
// ---------------------------------------------------------------------------

#[doc(hidden)]
pub mod test_hooks {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, MutexGuard, OnceLock};

    static CRASH_AFTER_TMP: AtomicBool = AtomicBool::new(false);

    /// Serializes test use of `CrashAfterTmpGuard` so parallel
    /// tests never observe the flag as `true` unexpectedly.
    fn lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    pub fn crash_after_tmp() -> bool {
        CRASH_AFTER_TMP.load(Ordering::SeqCst)
    }

    /// RAII guard that holds the test serialization mutex and sets
    /// the "crash after tmp fsync" flag. Parallel tests that don't
    /// hold the guard run without observing the flag.
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
#[allow(unused_imports)]
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
}
