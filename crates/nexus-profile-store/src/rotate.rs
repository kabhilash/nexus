//! Master-key rotation. See DD-007 §4.5.
//!
//! Flow:
//!
//! 1. Take an exclusive `flock(LOCK_EX)` on
//!    `<root>/.rotation.lock`. Held for the entire rotation.
//! 2. Shift `active → previous`, `new_key → active` in the store's
//!    in-memory key ring. Reads now accept both keys; writes use
//!    the new active.
//! 3. For every profile file, decrypt with the (possibly `previous`)
//!    old cipher, re-encrypt under the new active cipher, and
//!    rewrite atomically. Emit
//!    `NexusEvent::ProfileStoreRotationProgress` every 10 profiles
//!    or every 500 ms, whichever fires first.
//! 4. On success, clear `previous` and release the lock.
//! 5. On failure mid-rotation, leave `previous` populated so
//!    subsequent reads still find profiles still encrypted under
//!    the old key. Emit an `OperatorNotification` with
//!    `kind = "master_key_degraded"`.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nexus_core::NexusEvent;
use tokio::sync::broadcast;
use ulid::Ulid;
use zeroize::Zeroize;

use crate::crypto::{ChaChaCipher, Cipher};
use crate::error::{Result, StoreError};
use crate::fs_store::{
    DIR_BLUETOOTH, DIR_ETHERNET, DIR_GNSS, DIR_WIFI, ProfileFileStore, ROTATION_LOCK_FILE,
    load_all_raw, test_hooks, write_atomic_toml,
};
use crate::metrics as m;
use crate::quarantine;
use crate::trait_def::RotateReport;
use crate::types::bluetooth::BluetoothProfile;
use crate::types::ethernet::{EthernetProfileOnDisk, decrypt_ethernet, encrypt_ethernet};
use crate::types::gnss::GnssDeviceProfile;
use crate::types::wifi::{WifiProfileOnDisk, decrypt_wifi, encrypt_wifi};

const PROGRESS_EVERY_N: u32 = 10;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// Rotate the store's master key to `new_key`. Caller is
/// responsible for persisting `new_key` via whatever master-key
/// source is in use — this function only rewrites the profile
/// files.
pub async fn rotate_to_key(store: &ProfileFileStore, new_key: [u8; 32]) -> Result<RotateReport> {
    let started = Instant::now();
    let event_tx = store.event_tx.as_ref();

    let lock_path = store.root().join(ROTATION_LOCK_FILE);
    let _guard = acquire_rotation_lock(&lock_path)?;

    // Install the new key as active; demote the old one to previous.
    {
        let mut keys = store.keys.write().await;
        let old_active = keys.active;
        keys.previous = Some(old_active);
        keys.active = new_key;
    }

    let total = count_profiles(store)?;
    let mut completed = 0u32;
    let mut last_progress = Instant::now();

    // Emit an initial 0/total so consumers see the job start.
    send_progress(event_tx, 0, total);

    match run_rotation(store, &mut completed, &mut last_progress, total, event_tx).await {
        Ok(()) => {
            // Clear previous only on full success.
            let mut keys = store.keys.write().await;
            if let Some(mut prev) = keys.previous.take() {
                prev.zeroize();
            }
            send_progress(event_tx, completed, total);
            m::record_rotation(m::outcome::SUCCESS);
            m::record_rotation_duration(started.elapsed().as_secs_f64());
            Ok(RotateReport {
                profiles_rewritten: completed,
                duration: started.elapsed(),
            })
        }
        Err(e) => {
            tracing::error!(
                completed,
                total,
                error = %e,
                "master-key rotation failed mid-flight; previous key retained for reads",
            );
            quarantine::notify_master_key_degraded(event_tx, &e.to_string(), completed, total);
            m::record_rotation(m::outcome::IO_ERROR);
            m::record_rotation_duration(started.elapsed().as_secs_f64());
            Err(e)
        }
    }
}

async fn run_rotation(
    store: &ProfileFileStore,
    completed: &mut u32,
    last_progress: &mut Instant,
    total: u32,
    event_tx: Option<&broadcast::Sender<NexusEvent>>,
) -> Result<()> {
    rotate_dir_wifi(store, completed, last_progress, total, event_tx).await?;
    rotate_dir_ethernet(store, completed, last_progress, total, event_tx).await?;
    rotate_dir_gnss(store, completed, last_progress, total, event_tx).await?;
    rotate_dir_bluetooth(store, completed, last_progress, total, event_tx).await?;
    Ok(())
}

async fn rotate_dir_wifi(
    store: &ProfileFileStore,
    completed: &mut u32,
    last_progress: &mut Instant,
    total: u32,
    event_tx: Option<&broadcast::Sender<NexusEvent>>,
) -> Result<()> {
    for (path, on_disk) in load_all_raw::<WifiProfileOnDisk>(&store.dir(DIR_WIFI))? {
        let (active, previous) = store.cipher_pair(&on_disk.id).await;
        let profile = decrypt_under_old(&on_disk, &active, previous.as_ref(), |od, c| {
            decrypt_wifi(od, c)
        })?;
        let new_on_disk = encrypt_wifi(&profile, &active)
            .map_err(|e| StoreError::malformed(&path, e.to_string()))?;
        write_atomic_toml(&path, &new_on_disk)?;
        bump_progress(completed, last_progress, total, event_tx);
    }
    Ok(())
}

async fn rotate_dir_ethernet(
    store: &ProfileFileStore,
    completed: &mut u32,
    last_progress: &mut Instant,
    total: u32,
    event_tx: Option<&broadcast::Sender<NexusEvent>>,
) -> Result<()> {
    for (path, on_disk) in load_all_raw::<EthernetProfileOnDisk>(&store.dir(DIR_ETHERNET))? {
        let (active, previous) = store.cipher_pair(&on_disk.id).await;
        let profile = decrypt_under_old(&on_disk, &active, previous.as_ref(), |od, c| {
            decrypt_ethernet(od, c)
        })?;
        let new_on_disk = encrypt_ethernet(&profile, &active)
            .map_err(|e| StoreError::malformed(&path, e.to_string()))?;
        write_atomic_toml(&path, &new_on_disk)?;
        bump_progress(completed, last_progress, total, event_tx);
    }
    Ok(())
}

async fn rotate_dir_gnss(
    store: &ProfileFileStore,
    completed: &mut u32,
    last_progress: &mut Instant,
    total: u32,
    event_tx: Option<&broadcast::Sender<NexusEvent>>,
) -> Result<()> {
    // GNSS profiles have no credentials; we rewrite them so the
    // count matches `total` (and so a future migration that *does*
    // encrypt a GNSS field doesn't need a different code path).
    for (path, profile) in load_all_raw::<GnssDeviceProfile>(&store.dir(DIR_GNSS))? {
        write_atomic_toml(&path, &profile)?;
        bump_progress(completed, last_progress, total, event_tx);
    }
    Ok(())
}

async fn rotate_dir_bluetooth(
    store: &ProfileFileStore,
    completed: &mut u32,
    last_progress: &mut Instant,
    total: u32,
    event_tx: Option<&broadcast::Sender<NexusEvent>>,
) -> Result<()> {
    for (path, profile) in load_all_raw::<BluetoothProfile>(&store.dir(DIR_BLUETOOTH))? {
        write_atomic_toml(&path, &profile)?;
        bump_progress(completed, last_progress, total, event_tx);
    }
    Ok(())
}

fn decrypt_under_old<OnDisk, Parsed, F>(
    on_disk: &OnDisk,
    active: &dyn Cipher,
    previous: Option<&ChaChaCipher>,
    decrypt: F,
) -> Result<Parsed>
where
    OnDisk: Clone,
    F: Fn(OnDisk, &dyn Cipher) -> std::result::Result<Parsed, crate::crypto::CipherError>,
{
    // The profile may already be encrypted under the *new* active
    // key (if rotation is re-running after a partial failure); try
    // active first, then previous.
    match decrypt(on_disk.clone(), active) {
        Ok(p) => Ok(p),
        Err(_) => {
            let prev = previous.ok_or_else(|| {
                StoreError::malformed(
                    PathBuf::new(),
                    "no previous key available for fallback decrypt",
                )
            })?;
            decrypt(on_disk.clone(), prev as &dyn Cipher)
                .map_err(|e| StoreError::malformed(PathBuf::new(), format!("rotate decrypt: {e}")))
        }
    }
}

fn bump_progress(
    completed: &mut u32,
    last_progress: &mut Instant,
    total: u32,
    event_tx: Option<&broadcast::Sender<NexusEvent>>,
) {
    *completed += 1;

    // Crash-hook: the tests use this to simulate a mid-rotation
    // failure. Production builds never set this.
    if let Some(n) = test_hooks::crash_after_rotate_count() {
        if *completed == n {
            panic!("simulated rotation crash after {n} profiles");
        }
    }

    let due = *completed % PROGRESS_EVERY_N == 0 || last_progress.elapsed() >= PROGRESS_INTERVAL;
    if due {
        send_progress(event_tx, *completed, total);
        *last_progress = Instant::now();
    }
}

fn send_progress(event_tx: Option<&broadcast::Sender<NexusEvent>>, completed: u32, total: u32) {
    if let Some(tx) = event_tx {
        let _ = tx.send(NexusEvent::ProfileStoreRotationProgress { completed, total });
    }
}

fn count_profiles(store: &ProfileFileStore) -> Result<u32> {
    let mut n = 0u32;
    for dir in [DIR_WIFI, DIR_ETHERNET, DIR_GNSS, DIR_BLUETOOTH] {
        n += count_toml_in(&store.dir(dir))?;
    }
    Ok(n)
}

fn count_toml_in(dir: &std::path::Path) -> Result<u32> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(StoreError::io(dir, e)),
    };
    let mut n = 0u32;
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::io(dir, e))?;
        if entry.path().extension().and_then(|s| s.to_str()) == Some("toml") {
            n += 1;
        }
    }
    Ok(n)
}

// ---------------------------------------------------------------------------
// Rotation lock via flock
// ---------------------------------------------------------------------------

struct RotationLock {
    _file: std::fs::File,
    fd: std::os::fd::RawFd,
}

impl Drop for RotationLock {
    fn drop(&mut self) {
        // SAFETY: fd is still valid; flock(LOCK_UN) on an unlocked
        // fd is a no-op.
        unsafe {
            libc::flock(self.fd, libc::LOCK_UN);
        }
    }
}

fn acquire_rotation_lock(path: &std::path::Path) -> Result<RotationLock> {
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|e| StoreError::io(path, e))?;
    let fd = file.as_raw_fd();
    // SAFETY: fd is valid for the duration of the `file` binding.
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            m::record_rotation(m::outcome::LOCK_CONTENTION);
            return Err(StoreError::io(
                path,
                io::Error::other("rotation already in progress"),
            ));
        }
        return Err(StoreError::io(path, err));
    }
    Ok(RotationLock { _file: file, fd })
}

/// Generate a fresh 32-byte key suitable for use as the new master
/// key. Exposed for callers that want to coordinate rotation +
/// key-source persistence themselves (e.g. rewriting
/// `keys/master.key` before calling [`rotate_to_key`]).
pub fn generate_master_key() -> [u8; 32] {
    use chacha20poly1305::aead::OsRng;
    use chacha20poly1305::aead::rand_core::RngCore;
    let mut k = [0u8; 32];
    OsRng.fill_bytes(&mut k);
    k
}

// Kept as a hint for future implementers: accepting an unused
// `Ulid` is the natural shape for tests that already have a profile
// id to hand; remove this when such a helper exists.
#[allow(dead_code)]
fn _ulid_unused(_id: &Ulid) {}
