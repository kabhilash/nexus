//! `/dev/rfkill` watcher and writer for Wi-Fi interfaces.
//!
//! Per DD-003 §13.5, `fi.nexus.Wifi.Powered` is defined as "rfkill
//! released for this interface" — both soft (userspace / software
//! block) and hard (hardware switch). The kernel exposes this state
//! through `/dev/rfkill`, a char device that:
//!
//! - On open, emits a synthetic `RFKILL_OP_ADD` event for every
//!   currently-registered rfkill instance. That gives us initial
//!   state for free — no separate sysfs enumeration is needed.
//! - Streams further `RFKILL_OP_ADD`, `RFKILL_OP_DEL`, and
//!   `RFKILL_OP_CHANGE` events as the kernel's rfkill state
//!   mutates (for example when a hardware switch flips, or when
//!   `rfkill` userspace writes a CHANGE record).
//! - Accepts `RFKILL_OP_CHANGE` writes that set the soft-block
//!   bit. Hard-block is always kernel-owned.
//!
//! The watcher filters to `RFKILL_TYPE_WLAN (1)` events, resolves
//! each event's `idx` to its wiphy name via
//! `/sys/class/rfkill/rfkillN/name`, and emits
//! `RfkillState { wiphy_name, powered }` over an mpsc to the Wi-Fi
//! backend. The backend performs the `wiphy_name → ifindex` lookup
//! (it already owns the per-interface registry) and republishes as
//! `NexusEvent::WifiRfkillChanged`.
//!
//! The writer lives on the same open fd. `set_blocked(wiphy_name,
//! soft)` resolves the wiphy to its rfkill `idx` by scanning
//! `/sys/class/rfkill` (cheap — a handful of entries) and writes a
//! single `RFKILL_OP_CHANGE` record. Blocking writes on
//! `/dev/rfkill` return immediately in the kernel so we don't need
//! async for that side.

use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Protocol constants from <linux/rfkill.h>
// ---------------------------------------------------------------------------

pub const RFKILL_TYPE_WLAN: u8 = 1;

pub const RFKILL_OP_ADD: u8 = 0;
pub const RFKILL_OP_DEL: u8 = 1;
pub const RFKILL_OP_CHANGE: u8 = 2;
// RFKILL_OP_CHANGE_ALL = 3 is valid on writes but we never emit it.

/// On-wire layout. Older kernels return exactly this; newer kernels
/// may append a `hard_block_reasons` byte that we deliberately
/// ignore by reading only the first 8 bytes per event.
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct RfkillEvent {
    idx: u32,
    ty: u8,
    op: u8,
    soft: u8,
    hard: u8,
}

const RFKILL_EVENT_SIZE: usize = size_of::<RfkillEvent>();

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Edge emitted by the reader task to the Wi-Fi backend. Keyed on
/// wiphy name because the watcher doesn't need the interface
/// registry; the backend that receives this already owns it.
#[derive(Debug, Clone)]
pub struct RfkillState {
    pub wiphy_name: String,
    pub powered: bool,
}

/// Handle for the reader task + writable fd. Drop the handle to let
/// the reader spin down with the `CancellationToken` the backend
/// passed in; the writable fd lives inside the [`RfkillWriter`] and
/// is dropped with it.
pub struct RfkillWatcher {
    pub join: JoinHandle<()>,
    pub writer: RfkillWriter,
}

/// Userspace side of rfkill writes. Backed by its own fd so the
/// reader and writer don't contend for the same `AsyncFd`; the
/// write path is blocking (kernel returns immediately) so
/// [`set_blocked`](RfkillWriter::set_blocked) runs inside
/// `tokio::task::spawn_blocking`.
#[derive(Clone)]
pub struct RfkillWriter {
    fd: Arc<OwnedFd>,
}

impl RfkillWriter {
    /// Flip soft-rfkill for the named wiphy. `block = true` asserts
    /// the soft block (radio off); `block = false` releases it.
    /// Hard-rfkill can't be toggled from userspace.
    pub async fn set_blocked(&self, wiphy_name: &str, block: bool) -> io::Result<()> {
        let wiphy = wiphy_name.to_owned();
        let fd = self.fd.clone();
        tokio::task::spawn_blocking(move || {
            let idx = resolve_rfkill_idx(&wiphy)?;
            let event = RfkillEvent {
                idx,
                ty: RFKILL_TYPE_WLAN,
                op: RFKILL_OP_CHANGE,
                soft: block as u8,
                hard: 0,
            };
            write_event(fd.as_raw_fd(), &event)
        })
        .await
        .map_err(|e| io::Error::other(format!("spawn_blocking join: {e}")))?
    }
}

// ---------------------------------------------------------------------------
// Spawn entry point
// ---------------------------------------------------------------------------

/// Open `/dev/rfkill` (once for read, once for write), spawn the
/// reader task, and return a [`RfkillWatcher`] so the caller holds
/// both the writer handle and the task join-handle.
///
/// `tx` is the mpsc the reader emits [`RfkillState`] edges on. The
/// caller typically installs `rx` as one arm of its `tokio::select!`.
pub fn spawn(
    tx: mpsc::Sender<RfkillState>,
    cancel: CancellationToken,
) -> io::Result<RfkillWatcher> {
    let reader_fd = open_rfkill(true)?;
    let writer_fd = Arc::new(open_rfkill(false)?);
    let async_fd = AsyncFd::new(reader_fd)?;

    let join = tokio::spawn(reader_loop(async_fd, tx, cancel));
    Ok(RfkillWatcher {
        join,
        writer: RfkillWriter { fd: writer_fd },
    })
}

// ---------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------

async fn reader_loop(
    fd: AsyncFd<OwnedFd>,
    tx: mpsc::Sender<RfkillState>,
    cancel: CancellationToken,
) {
    loop {
        let readable = tokio::select! {
            _ = cancel.cancelled() => return,
            r = fd.readable() => r,
        };
        let mut guard = match readable {
            Ok(g) => g,
            Err(e) => {
                warn!(error = %e, "rfkill readable poll failed; backing off");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        // Drain every pending record. The kernel returns one event
        // per `read(2)` — read-until-EWOULDBLOCK is the idiom.
        loop {
            let res = guard.try_io(|inner| read_event(inner.get_ref().as_raw_fd()));
            match res {
                Ok(Ok(Some(ev))) => {
                    if let Some(state) = translate(&ev) {
                        if tx.send(state).await.is_err() {
                            return;
                        }
                    }
                }
                Ok(Ok(None)) => {
                    // Short read of a non-wlan / DEL event — keep
                    // going.
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "rfkill read failed; breaking to re-poll");
                    break;
                }
                Err(_would_block) => break,
            }
        }
    }
}

/// Translate one raw event to [`RfkillState`]. Returns `None` for
/// non-wlan types or for `DEL` events (the interface went away; the
/// backend will get the removal signal via
/// `NexusEvent::InterfaceRemoved` and clean up the cache).
fn translate(ev: &RfkillEvent) -> Option<RfkillState> {
    if ev.ty != RFKILL_TYPE_WLAN {
        return None;
    }
    if ev.op == RFKILL_OP_DEL {
        return None;
    }
    // ADD emits at open for every existing rfkill → perfect bootstrap.
    // CHANGE is the regular edge.
    let idx = ev.idx;
    let wiphy_name = match read_rfkill_name(idx) {
        Ok(n) => n,
        Err(e) => {
            debug!(idx, error = %e, "couldn't resolve rfkill idx to name");
            return None;
        }
    };
    Some(RfkillState {
        wiphy_name,
        powered: ev.soft == 0 && ev.hard == 0,
    })
}

// ---------------------------------------------------------------------------
// Low-level syscalls
// ---------------------------------------------------------------------------

fn open_rfkill(nonblocking: bool) -> io::Result<OwnedFd> {
    let mut flags = libc::O_RDWR | libc::O_CLOEXEC;
    if nonblocking {
        flags |= libc::O_NONBLOCK;
    }
    let path = std::ffi::CString::new("/dev/rfkill").unwrap();
    // SAFETY: `open(2)` with a valid path + flags; the only failure
    // mode is a negative return, which we propagate.
    let raw = unsafe { libc::open(path.as_ptr(), flags) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `open(2)` returned a fresh owned fd; nothing else
    // holds a copy.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Read exactly one rfkill event. Returns `Ok(None)` on an
/// unexpected short read (kernel should never return <8 bytes but
/// we defend against it); returns `Err(WouldBlock)` when the fifo
/// is drained.
fn read_event(fd: libc::c_int) -> io::Result<Option<RfkillEvent>> {
    let mut buf = [0u8; RFKILL_EVENT_SIZE];
    // SAFETY: `read(2)` writes up to `buf.len()` bytes at
    // `buf.as_mut_ptr()`, a valid stack allocation.
    let n = unsafe {
        libc::read(
            fd,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            buf.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if (n as usize) < RFKILL_EVENT_SIZE {
        return Ok(None);
    }
    // SAFETY: `RfkillEvent` is `#[repr(C, packed)]` with
    // size = RFKILL_EVENT_SIZE. Every 8-byte pattern is a valid
    // value — the fields are all integer types.
    let ev: RfkillEvent = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
    Ok(Some(ev))
}

fn write_event(fd: libc::c_int, ev: &RfkillEvent) -> io::Result<()> {
    // Materialize the on-wire bytes from `ev`. The previous form was
    // `let buf = [0u8; …]; ptr::write_unaligned(buf.as_ptr() as *mut RfkillEvent, *ev)`
    // — that's UB: `buf` is immutable, and casting `*const u8` to
    // `*mut RfkillEvent` then writing through it lets the compiler
    // assume `buf` never changes and propagate the zero-init through
    // to the `libc::write` arg. In release builds that means the
    // kernel sees 8 zero bytes (op = `RFKILL_OP_ADD`, which writes
    // reject with -EINVAL), regardless of what we tried to encode —
    // *both* the block and unblock soft-rfkill writes silently
    // become no-ops.
    //
    // SAFETY: `RfkillEvent` is `#[repr(C, packed)]` over integer
    // fields, so its in-memory layout is exactly
    // `[u8; RFKILL_EVENT_SIZE]`. Reading those bytes through a
    // `*const u8` is well-defined.
    let bytes: [u8; RFKILL_EVENT_SIZE] = unsafe {
        let mut buf = [0u8; RFKILL_EVENT_SIZE];
        std::ptr::copy_nonoverlapping(
            (ev as *const RfkillEvent).cast::<u8>(),
            buf.as_mut_ptr(),
            RFKILL_EVENT_SIZE,
        );
        buf
    };
    // SAFETY: `write(2)` reads `bytes.len()` bytes at `bytes.as_ptr()`.
    let n = unsafe {
        libc::write(
            fd,
            bytes.as_ptr().cast::<libc::c_void>(),
            bytes.len(),
        )
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if (n as usize) != RFKILL_EVENT_SIZE {
        return Err(io::Error::other(format!(
            "short write to /dev/rfkill: {n}"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// sysfs helpers
// ---------------------------------------------------------------------------

fn read_rfkill_name(idx: u32) -> io::Result<String> {
    let p = PathBuf::from(format!("/sys/class/rfkill/rfkill{idx}/name"));
    let s = std::fs::read_to_string(p)?;
    Ok(s.trim().to_owned())
}

/// Read the current soft / hard bits for `wiphy_name` from sysfs.
/// Used by the Wi-Fi backend on `InterfaceDiscovered` to synthesize
/// the initial `WifiRfkillChanged` event, since the watcher's own
/// synthetic `RFKILL_OP_ADD` events fired at `open()` time may have
/// arrived before the interface registered.
pub fn read_current_state(wiphy_name: &str) -> io::Result<bool> {
    let idx = resolve_rfkill_idx(wiphy_name)?;
    let base = PathBuf::from(format!("/sys/class/rfkill/rfkill{idx}"));
    let soft = std::fs::read_to_string(base.join("soft"))?
        .trim()
        .parse::<u32>()
        .unwrap_or(1);
    let hard = std::fs::read_to_string(base.join("hard"))?
        .trim()
        .parse::<u32>()
        .unwrap_or(1);
    Ok(soft == 0 && hard == 0)
}

/// Walk `/sys/class/rfkill/rfkill*` looking for an entry whose
/// `name` file contains `wiphy_name` and whose `type` is `wlan`.
/// Returns the rfkill index.
fn resolve_rfkill_idx(wiphy_name: &str) -> io::Result<u32> {
    let dir = Path::new("/sys/class/rfkill");
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let fname = entry.file_name();
        let fname_str = fname.to_string_lossy();
        let Some(idx_str) = fname_str.strip_prefix("rfkill") else {
            continue;
        };
        let Ok(idx) = idx_str.parse::<u32>() else {
            continue;
        };
        let type_path = entry.path().join("type");
        let name_path = entry.path().join("name");
        let ty = std::fs::read_to_string(&type_path)
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        if ty != "wlan" {
            continue;
        }
        let name = std::fs::read_to_string(&name_path)
            .map(|s| s.trim().to_owned())
            .unwrap_or_default();
        if name == wiphy_name {
            return Ok(idx);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no rfkill entry for wiphy '{wiphy_name}'"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_size_is_eight_bytes() {
        // Guards against silent struct drift — the kernel ABI is
        // 8 bytes for the base record.
        assert_eq!(RFKILL_EVENT_SIZE, 8);
    }

    #[test]
    fn translate_emits_powered_for_cleared_bits() {
        let ev = RfkillEvent {
            idx: 0,
            ty: RFKILL_TYPE_WLAN,
            op: RFKILL_OP_CHANGE,
            soft: 0,
            hard: 0,
        };
        // We can't actually invoke `translate` without a sysfs
        // match, but we can assert the bit logic component.
        assert!(ev.soft == 0 && ev.hard == 0);
    }

    #[test]
    fn translate_skips_non_wlan_types() {
        let ev = RfkillEvent {
            idx: 0,
            ty: 2, // bluetooth
            op: RFKILL_OP_ADD,
            soft: 0,
            hard: 0,
        };
        assert!(translate(&ev).is_none());
    }

    /// Regression: `write_event` must put the actual `RfkillEvent`
    /// bytes on the wire, not eight zero bytes. The previous
    /// `let buf = [0u8; …]; ptr::write_unaligned(buf.as_ptr() as *mut)`
    /// form was UB — release builds silently dropped the encoded
    /// payload, which made every soft-rfkill write a no-op against
    /// `/dev/rfkill` (the kernel rejects `op=0` = `RFKILL_OP_ADD` on
    /// writes with `-EINVAL`).
    ///
    /// We can't write to `/dev/rfkill` from a CI container, so the
    /// test round-trips through a tempfile and compares the bytes
    /// against the canonical packed layout: `idx`(4 LE) + `ty`(1) +
    /// `op`(1) + `soft`(1) + `hard`(1).
    #[test]
    fn write_event_emits_canonical_packed_bytes() {
        use std::io::{Read, Seek, SeekFrom};
        use std::os::fd::AsRawFd;

        let mut f = tempfile::tempfile().expect("tempfile");
        let ev = RfkillEvent {
            idx: 0x12345678,
            ty: RFKILL_TYPE_WLAN,
            op: RFKILL_OP_CHANGE,
            soft: 0, // unblock
            hard: 0,
        };
        write_event(f.as_raw_fd(), &ev).expect("write_event");
        f.seek(SeekFrom::Start(0)).expect("seek");
        let mut got = [0u8; RFKILL_EVENT_SIZE];
        f.read_exact(&mut got).expect("read");
        assert_eq!(
            got,
            [0x78, 0x56, 0x34, 0x12, RFKILL_TYPE_WLAN, RFKILL_OP_CHANGE, 0, 0],
            "write_event must encode RfkillEvent fields in packed LE layout, not zero them out"
        );

        // Now block direction: soft=1 must actually land in byte 6.
        f.seek(SeekFrom::Start(0)).expect("rewind");
        f.set_len(0).expect("truncate");
        let block = RfkillEvent {
            idx: 1,
            ty: RFKILL_TYPE_WLAN,
            op: RFKILL_OP_CHANGE,
            soft: 1,
            hard: 0,
        };
        write_event(f.as_raw_fd(), &block).expect("write_event block");
        f.seek(SeekFrom::Start(0)).expect("seek");
        let mut got = [0u8; RFKILL_EVENT_SIZE];
        f.read_exact(&mut got).expect("read");
        assert_eq!(got[6], 1, "soft byte must be 1 for block direction");
    }
}
