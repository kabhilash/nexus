//! Filesystem-backed implementation of [`ProfileStore`] with
//! encryption, a key-ring for active + previous master keys, and
//! hooks for quarantine + metrics + migration. See DD-007 §§4-10.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use nexus_core::{MacAddr, NexusEvent};
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use ulid::Ulid;
use zeroize::Zeroize;

use crate::crypto::{ChaChaCipher, Cipher, CipherError, derive_file_key};
use crate::error::{Result, StoreError};
use crate::keys::MasterKeySource;
use crate::metrics as m;
use crate::quarantine::{self, corrupt_reason};
use crate::trait_def::{ProfileKind, ProfileRef, ProfileStore, RotateReport};
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

pub(crate) const DIR_ETHERNET: &str = "ethernet";
pub(crate) const DIR_WIFI: &str = "wifi";
pub(crate) const DIR_GNSS: &str = "gnss";
pub(crate) const DIR_BLUETOOTH: &str = "bluetooth";
pub(crate) const ROTATION_LOCK_FILE: &str = ".rotation.lock";

/// Two 32-byte keys: the `active` key for writes, and an optional
/// `previous` key used for load fallbacks during an in-flight
/// rotation. Both are zeroized on drop.
#[derive(Default)]
pub(crate) struct KeyRing {
    pub active: [u8; 32],
    pub previous: Option<[u8; 32]>,
}

impl Drop for KeyRing {
    fn drop(&mut self) {
        self.active.zeroize();
        if let Some(p) = self.previous.as_mut() {
            p.zeroize();
        }
    }
}

/// Filesystem-backed profile store.
pub struct ProfileFileStore {
    root: PathBuf,
    pub(crate) keys: Arc<RwLock<KeyRing>>,
    source_name: &'static str,
    pub(crate) event_tx: Option<broadcast::Sender<NexusEvent>>,
}

impl std::fmt::Debug for ProfileFileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfileFileStore")
            .field("root", &self.root)
            .field("keys", &"<redacted>")
            .field("source", &self.source_name)
            .field("event_tx", &self.event_tx.as_ref().map(|_| "<sender>"))
            .finish()
    }
}

impl ProfileFileStore {
    /// Open a store rooted at `root`, pulling the master key from
    /// `source`. Creates the per-kind subdirectories if absent.
    /// Runs pending schema migrations before returning.
    pub fn open(root: impl Into<PathBuf>, source: &dyn MasterKeySource) -> Result<Self> {
        let key = source.master_key().map_err(|e| StoreError::RootDir {
            path: PathBuf::new(),
            source: io::Error::other(format!("master key source '{}': {e}", source.name())),
        })?;
        let store = Self::open_with_key(root, key, source.name())?;
        m::set_master_key_source(source.name());
        // Run any pending schema migrations. This is synchronous
        // (no async I/O besides what the in-proc code does), so
        // it's safe to block here.
        crate::migrate::run_startup_migrations(&store)?;
        Ok(store)
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
            keys: Arc::new(RwLock::new(KeyRing {
                active: key,
                previous: None,
            })),
            source_name,
            event_tx: None,
        })
    }

    /// Builder: attach a broadcast sender so quarantine and rotation
    /// can emit `NexusEvent`s.
    pub fn with_event_tx(mut self, tx: broadcast::Sender<NexusEvent>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    /// Fire a [`NexusEvent::ProfileChanged`] on successful put/remove
    /// so consumers (notably the Wi-Fi and Ethernet backends, which
    /// cache profile sets in memory) know to refresh. Silently
    /// no-ops when no bus was attached — tests that don't care don't
    /// need to plumb through a broadcast channel.
    fn emit_profile_changed(&self, kind: ProfileKind, key: &str) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(NexusEvent::ProfileChanged {
                kind,
                key: key.to_owned(),
            });
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn source_name(&self) -> &'static str {
        self.source_name
    }

    pub(crate) fn dir(&self, sub: &str) -> PathBuf {
        self.root.join(sub)
    }

    pub(crate) fn ethernet_path(&self, ifname: &str) -> PathBuf {
        self.dir(DIR_ETHERNET).join(format!("{ifname}.toml"))
    }

    pub(crate) fn wifi_path(&self, ssid_hash: &str) -> PathBuf {
        self.dir(DIR_WIFI).join(format!("{ssid_hash}.toml"))
    }

    pub(crate) fn gnss_path(&self, id: &Ulid) -> PathBuf {
        self.dir(DIR_GNSS).join(format!("{id}.toml"))
    }

    pub(crate) fn bluetooth_path(&self, id: &Ulid) -> PathBuf {
        self.dir(DIR_BLUETOOTH).join(format!("{id}.toml"))
    }

    /// Construct the (active, previous?) cipher pair for a given
    /// profile. The active cipher is always returned; the previous
    /// cipher is `Some(_)` only during an in-flight rotation.
    pub(crate) async fn cipher_pair(&self, id: &Ulid) -> (ChaChaCipher, Option<ChaChaCipher>) {
        let id_bytes = id.to_bytes();
        let keys = self.keys.read().await;
        let active = ChaChaCipher::new(derive_file_key(&keys.active, &id_bytes));
        let previous = keys
            .previous
            .as_ref()
            .map(|k| ChaChaCipher::new(derive_file_key(k, &id_bytes)));
        (active, previous)
    }
}

#[async_trait]
impl ProfileStore for ProfileFileStore {
    async fn load_ethernet(&self) -> Result<Vec<EthernetProfile>> {
        let mut out = Vec::new();
        for (path, on_disk) in load_all_raw::<EthernetProfileOnDisk>(&self.dir(DIR_ETHERNET))? {
            let (active, previous) = self.cipher_pair(&on_disk.id).await;
            match decrypt_with_fallback(&on_disk, &active, previous.as_ref(), |od, c| {
                decrypt_ethernet(od, c)
            }) {
                Ok(p) => {
                    m::record_profile_loaded(ProfileKind::Ethernet);
                    out.push(p);
                }
                Err(reason) => self.quarantine_load_failure(
                    ProfileKind::Ethernet,
                    &path,
                    &reason,
                    corrupt_reason::DECRYPT_FAIL,
                ),
            }
        }
        out.sort_by_key(|p| p.id);
        m::set_profile_count(ProfileKind::Ethernet, out.len() as u64);
        Ok(out)
    }

    async fn load_ethernet_profile(&self, ifname: &str) -> Result<Option<EthernetProfile>> {
        let path = self.ethernet_path(ifname);
        match load_one::<EthernetProfileOnDisk>(&path)? {
            Some(on_disk) => {
                let (active, previous) = self.cipher_pair(&on_disk.id).await;
                match decrypt_with_fallback(&on_disk, &active, previous.as_ref(), |od, c| {
                    decrypt_ethernet(od, c)
                }) {
                    Ok(p) => Ok(Some(p)),
                    Err(reason) => {
                        self.quarantine_load_failure(
                            ProfileKind::Ethernet,
                            &path,
                            &reason,
                            corrupt_reason::DECRYPT_FAIL,
                        );
                        Ok(None)
                    }
                }
            }
            None => Ok(None),
        }
    }

    async fn put_ethernet(&self, profile: &EthernetProfile) -> Result<()> {
        let started = Instant::now();
        let (active, _) = self.cipher_pair(&profile.id).await;
        let on_disk = encrypt_ethernet(profile, &active).map_err(|e| {
            m::record_write(ProfileKind::Ethernet, m::outcome::CRYPTO_ERROR);
            StoreError::malformed(self.ethernet_path(&profile.interface.name), e.to_string())
        })?;
        let path = self.ethernet_path(&profile.interface.name);
        let result = write_atomic_toml(&path, &on_disk);
        record_write_outcome(ProfileKind::Ethernet, &result, started);
        if result.is_ok() {
            self.emit_profile_changed(ProfileKind::Ethernet, &profile.interface.name);
        }
        result
    }

    async fn remove_ethernet(&self, ifname: &str) -> Result<()> {
        let result = remove_if_present(&self.ethernet_path(ifname));
        if result.is_ok() {
            self.emit_profile_changed(ProfileKind::Ethernet, ifname);
        }
        result
    }

    async fn load_wifi(&self) -> Result<Vec<WifiProfile>> {
        let mut out = Vec::new();
        for (path, on_disk) in load_all_raw::<WifiProfileOnDisk>(&self.dir(DIR_WIFI))? {
            let (active, previous) = self.cipher_pair(&on_disk.id).await;
            match decrypt_with_fallback(&on_disk, &active, previous.as_ref(), |od, c| {
                decrypt_wifi(od, c)
            }) {
                Ok(p) => {
                    m::record_profile_loaded(ProfileKind::Wifi);
                    out.push(p);
                }
                Err(reason) => self.quarantine_load_failure(
                    ProfileKind::Wifi,
                    &path,
                    &reason,
                    corrupt_reason::DECRYPT_FAIL,
                ),
            }
        }
        out.sort_by_key(|p| p.id);
        m::set_profile_count(ProfileKind::Wifi, out.len() as u64);
        Ok(out)
    }

    async fn put_wifi(&self, profile: &WifiProfile) -> Result<()> {
        let started = Instant::now();
        let (active, _) = self.cipher_pair(&profile.id).await;
        let on_disk = encrypt_wifi(profile, &active).map_err(|e| {
            m::record_write(ProfileKind::Wifi, m::outcome::CRYPTO_ERROR);
            StoreError::malformed(self.wifi_path(""), e.to_string())
        })?;
        let hash = ssid_hash(&profile.network.ssid);
        let path = self.wifi_path(&hash);
        let result = write_atomic_toml(&path, &on_disk);
        record_write_outcome(ProfileKind::Wifi, &result, started);
        if result.is_ok() {
            self.emit_profile_changed(ProfileKind::Wifi, &hash);
        }
        result
    }

    async fn remove_wifi(&self, ssid_hash: &str) -> Result<()> {
        let result = remove_if_present(&self.wifi_path(ssid_hash));
        if result.is_ok() {
            self.emit_profile_changed(ProfileKind::Wifi, ssid_hash);
        }
        result
    }

    async fn load_gnss(&self) -> Result<Vec<GnssDeviceProfile>> {
        let mut out: Vec<GnssDeviceProfile> = load_all::<GnssDeviceProfile>(&self.dir(DIR_GNSS))?;
        out.sort_by_key(|p| p.id);
        for _ in &out {
            m::record_profile_loaded(ProfileKind::Gnss);
        }
        m::set_profile_count(ProfileKind::Gnss, out.len() as u64);
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
        let started = Instant::now();
        let path = self.gnss_path(&profile.id);
        let result = write_atomic_toml(&path, profile);
        record_write_outcome(ProfileKind::Gnss, &result, started);
        result
    }

    async fn remove_gnss(&self, id: &Ulid) -> Result<()> {
        remove_if_present(&self.gnss_path(id))
    }

    async fn load_bluetooth(&self) -> Result<Vec<BluetoothProfile>> {
        let mut out: Vec<BluetoothProfile> =
            load_all::<BluetoothProfile>(&self.dir(DIR_BLUETOOTH))?;
        out.sort_by_key(|p| p.id);
        for _ in &out {
            m::record_profile_loaded(ProfileKind::Bluetooth);
        }
        m::set_profile_count(ProfileKind::Bluetooth, out.len() as u64);
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
        let started = Instant::now();
        let path = self.bluetooth_path(&profile.id);
        let result = write_atomic_toml(&path, profile);
        record_write_outcome(ProfileKind::Bluetooth, &result, started);
        result
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

    async fn set_last_connected(
        &self,
        reference: ProfileRef<'_>,
        when: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        match reference {
            ProfileRef::Wifi { ssid_hash } => {
                let path = self.wifi_path(ssid_hash);
                let mut on_disk: WifiProfileOnDisk = match load_one(&path)? {
                    Some(p) => p,
                    None => return Ok(()),
                };
                on_disk.network.last_connected_at = Some(when);
                write_atomic_toml(&path, &on_disk)
            }
            // Ethernet/GNSS/Bluetooth have no auto-select roster
            // that benefits from a per-profile recency stamp.
            ProfileRef::Ethernet { .. }
            | ProfileRef::Gnss { .. }
            | ProfileRef::Bluetooth { .. } => Ok(()),
        }
    }

    async fn rotate_master_key(&self) -> Result<RotateReport> {
        // Generate a fresh random key and rotate to it. Callers that
        // need to persist the new key (file source, TPM source) must
        // wrap this in their own key-persistence step.
        use chacha20poly1305::aead::OsRng;
        use chacha20poly1305::aead::rand_core::RngCore;
        let mut new_key = [0u8; 32];
        OsRng.fill_bytes(&mut new_key);
        let report = crate::rotate::rotate_to_key(self, new_key).await;
        new_key.zeroize();
        report
    }
}

impl ProfileFileStore {
    /// Move `path` to the quarantine directory and log + emit.
    pub(crate) fn quarantine_load_failure(
        &self,
        kind: ProfileKind,
        path: &Path,
        reason: &str,
        reason_label: &str,
    ) {
        match quarantine::quarantine_file(
            &self.root,
            kind,
            path,
            reason,
            reason_label,
            self.event_tx.as_ref(),
        ) {
            Ok(target) => {
                tracing::warn!(
                    path = %path.display(),
                    moved_to = %target.display(),
                    reason,
                    "profile quarantined",
                );
            }
            Err(e) => {
                tracing::error!(
                    path = %path.display(),
                    reason,
                    error = %e,
                    "profile failed to quarantine; leaving in place",
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fallback-decrypt helper
// ---------------------------------------------------------------------------

/// Try `decrypt` with the active cipher first; on `CipherError`,
/// retry with `previous` if present. Returns the reason string on
/// both-fail.
fn decrypt_with_fallback<OnDisk, Parsed, F>(
    on_disk: &OnDisk,
    active: &dyn Cipher,
    previous: Option<&ChaChaCipher>,
    decrypt: F,
) -> std::result::Result<Parsed, String>
where
    OnDisk: Clone,
    F: Fn(OnDisk, &dyn Cipher) -> std::result::Result<Parsed, CipherError>,
{
    match decrypt(on_disk.clone(), active) {
        Ok(p) => Ok(p),
        Err(active_err) => {
            if let Some(prev) = previous {
                match decrypt(on_disk.clone(), prev as &dyn Cipher) {
                    Ok(p) => Ok(p),
                    Err(prev_err) => Err(format!("active: {active_err}; previous: {prev_err}",)),
                }
            } else {
                Err(active_err.to_string())
            }
        }
    }
}

fn record_write_outcome(kind: ProfileKind, result: &Result<()>, started: Instant) {
    let outcome = match result {
        Ok(()) => m::outcome::SUCCESS,
        Err(StoreError::Io { .. } | StoreError::RootDir { .. }) => m::outcome::IO_ERROR,
        Err(StoreError::TomlSerialize(_) | StoreError::TomlDeserialize(_)) => m::outcome::IO_ERROR,
        Err(StoreError::Malformed { .. }) => m::outcome::CRYPTO_ERROR,
        Err(StoreError::NotYetImplemented(_)) => m::outcome::IO_ERROR,
    };
    m::record_write(kind, outcome);
    m::record_write_duration(kind, started.elapsed().as_secs_f64());
}

// ---------------------------------------------------------------------------
// SSID hashing
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

pub(crate) fn mkdir_p(path: &Path) -> Result<()> {
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
    Ok(load_all_raw::<T>(dir)?
        .into_iter()
        .map(|(_, v)| v)
        .collect())
}

/// Like `load_all` but returns `(path, T)` pairs so the caller can
/// quarantine on decrypt failure.
pub(crate) fn load_all_raw<T: serde::de::DeserializeOwned>(
    dir: &Path,
) -> Result<Vec<(PathBuf, T)>> {
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
            out.push((path, parsed));
        }
    }
    Ok(out)
}

pub(crate) fn load_one<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
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

pub(crate) fn write_atomic_toml<T: serde::Serialize>(path: &Path, value: &T) -> Result<()> {
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

/// Inspect the current mode of a file. Used by integration tests.
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
    static CRASH_AFTER_ROTATE_N: std::sync::atomic::AtomicI64 =
        std::sync::atomic::AtomicI64::new(-1);

    fn lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    pub fn crash_after_tmp() -> bool {
        CRASH_AFTER_TMP.load(Ordering::SeqCst)
    }

    /// When set to `Some(n)`, the rotation loop panics after
    /// successfully re-encrypting `n` profiles. Used by the
    /// crash-recovery test.
    pub fn crash_after_rotate_count() -> Option<u32> {
        let v = CRASH_AFTER_ROTATE_N.load(Ordering::SeqCst);
        if v < 0 { None } else { Some(v as u32) }
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

    pub struct CrashAfterRotateGuard {
        _lock: MutexGuard<'static, ()>,
    }

    impl CrashAfterRotateGuard {
        pub fn new(n: u32) -> Self {
            let guard = lock().lock().unwrap_or_else(|p| p.into_inner());
            CRASH_AFTER_ROTATE_N.store(n as i64, Ordering::SeqCst);
            Self { _lock: guard }
        }
    }

    impl Drop for CrashAfterRotateGuard {
        fn drop(&mut self) {
            CRASH_AFTER_ROTATE_N.store(-1, Ordering::SeqCst);
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
