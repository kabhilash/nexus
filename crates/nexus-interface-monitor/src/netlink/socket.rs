//! Async wrapper around an `AF_NETLINK` datagram socket.
//!
//! Opens the socket with `libc::socket`, binds it, records the
//! kernel-assigned `nl_pid`, and wraps the resulting fd in a
//! [`tokio::io::unix::AsyncFd`] so reads and writes can be driven
//! from async code. The integration-level happy-path test lives
//! under `tests/` and needs a real kernel (DD-001 §11.2); this
//! module is purely mechanical.

use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};

use tokio::io::unix::AsyncFd;

use super::parser::NetlinkMessageHeader;

// ---------------------------------------------------------------------------
// Protocol numbers and setsockopt options from <linux/netlink.h>. We
// hardcode the values that older `libc` releases don't expose so the
// build doesn't depend on the host's libc version.
// ---------------------------------------------------------------------------

pub const NETLINK_ROUTE: libc::c_int = 0;
pub const NETLINK_GENERIC: libc::c_int = 16;

pub const SOL_NETLINK: libc::c_int = 270;
pub const NETLINK_ADD_MEMBERSHIP: libc::c_int = 1;
pub const NETLINK_DROP_MEMBERSHIP: libc::c_int = 2;
pub const NETLINK_EXT_ACK: libc::c_int = 11;
pub const NETLINK_GET_STRICT_CHK: libc::c_int = 12;

// ---------------------------------------------------------------------------
// sockaddr_nl. Kept private; callers never need to construct one.
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockaddrNl {
    nl_family: libc::sa_family_t,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

// ---------------------------------------------------------------------------
// The socket.
// ---------------------------------------------------------------------------

/// Async `AF_NETLINK` datagram socket. The generic wrapper used for
/// both `NETLINK_ROUTE` (`rtnl_fd`) and `NETLINK_GENERIC`
/// (`nl80211_mcast_fd`, `nl80211_rr_fd`) per the naming table in
/// DD-001 §4.
pub struct NetlinkSocket {
    fd: AsyncFd<OwnedFd>,
    /// Kernel-assigned port ID. Stamped into outgoing `nlmsg_pid`.
    port_id: u32,
    /// Per-socket monotonic sequence counter. Stamped into
    /// `nlmsg_seq`.
    seq: AtomicU32,
}

impl NetlinkSocket {
    /// Open a netlink socket for `protocol` and bind it with the
    /// kernel assigning the port ID. `groups` is the classic-netlink
    /// multicast bitmask (e.g., [`crate::netlink::rtnl::RTMGRP_LINK`])
    /// — use `0` for request/response sockets that only need to join
    /// dynamic groups later via [`NetlinkSocket::join_multicast`].
    pub fn open(protocol: libc::c_int, groups: u32) -> io::Result<Self> {
        // SAFETY: `socket(2)` with these arguments is safe. The only
        // failure mode is a negative return, which we propagate.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                protocol,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `socket(2)` returned a fresh owned fd; nothing else
        // holds a copy.
        let owned = unsafe { OwnedFd::from_raw_fd(raw) };

        let addr = SockaddrNl {
            nl_family: libc::AF_NETLINK as libc::sa_family_t,
            nl_pad: 0,
            nl_pid: 0, // let the kernel assign
            nl_groups: groups,
        };
        // SAFETY: `bind(2)` reads `size_of::<SockaddrNl>()` bytes at
        // `&addr`, which is a valid local struct.
        let rc = unsafe {
            libc::bind(
                owned.as_raw_fd(),
                (&addr as *const SockaddrNl).cast::<libc::sockaddr>(),
                size_of::<SockaddrNl>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        // Read back the assigned port ID.
        let mut bound = SockaddrNl::default();
        let mut len = size_of::<SockaddrNl>() as libc::socklen_t;
        // SAFETY: `getsockname(2)` writes up to `len` bytes starting
        // at `&mut bound`, and `len` is initialized to the full struct
        // size.
        let rc = unsafe {
            libc::getsockname(
                owned.as_raw_fd(),
                (&mut bound as *mut SockaddrNl).cast::<libc::sockaddr>(),
                &mut len,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            fd: AsyncFd::new(owned)?,
            port_id: bound.nl_pid,
            seq: AtomicU32::new(1),
        })
    }

    /// Kernel-assigned port ID for this socket.
    pub fn port_id(&self) -> u32 {
        self.port_id
    }

    /// Fetch the next per-socket sequence number. Wraps around on
    /// overflow; netlink doesn't require uniqueness across the u32
    /// range.
    pub fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Stamp `header.seq` and `header.pid` with this socket's
    /// per-request values. Convenience for code that builds messages
    /// incrementally.
    pub fn fresh_header(&self, msg_type: u16, flags: u16) -> NetlinkMessageHeader {
        NetlinkMessageHeader {
            length: 0,
            msg_type,
            flags,
            seq: self.next_seq(),
            pid: self.port_id,
        }
    }

    /// Join a generic-netlink multicast group by runtime-resolved ID
    /// (see `genl::resolve_family`). Rtnetlink sockets pass classic
    /// multicast bitmasks at `open()` time instead.
    pub fn join_multicast(&self, group: u32) -> io::Result<()> {
        // SAFETY: setsockopt reads `size_of::<u32>()` bytes at `&group`.
        let rc = unsafe {
            libc::setsockopt(
                self.fd.get_ref().as_raw_fd(),
                SOL_NETLINK,
                NETLINK_ADD_MEMBERSHIP,
                (&group as *const u32).cast::<libc::c_void>(),
                size_of::<u32>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Drop a previously-joined multicast group.
    pub fn leave_multicast(&self, group: u32) -> io::Result<()> {
        // SAFETY: same invariants as join_multicast.
        let rc = unsafe {
            libc::setsockopt(
                self.fd.get_ref().as_raw_fd(),
                SOL_NETLINK,
                NETLINK_DROP_MEMBERSHIP,
                (&group as *const u32).cast::<libc::c_void>(),
                size_of::<u32>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Enable `NETLINK_EXT_ACK` for richer error reporting (DD-001
    /// §4.1). Best-effort: older kernels may return `ENOPROTOOPT`,
    /// which the caller can ignore.
    pub fn set_ext_ack(&self, enable: bool) -> io::Result<()> {
        self.setsockopt_i32(NETLINK_EXT_ACK, if enable { 1 } else { 0 })
    }

    /// Enable strict checking of request flags (DD-001 §4.1).
    pub fn set_strict_check(&self, enable: bool) -> io::Result<()> {
        self.setsockopt_i32(NETLINK_GET_STRICT_CHK, if enable { 1 } else { 0 })
    }

    /// Request a larger receive buffer. Prefers `SO_RCVBUFFORCE`
    /// (needs `CAP_NET_ADMIN`) and falls back to `SO_RCVBUF`.
    pub fn set_recv_buffer(&self, bytes: libc::c_int) -> io::Result<()> {
        match self.setsockopt_i32_at(libc::SOL_SOCKET, libc::SO_RCVBUFFORCE, bytes) {
            Ok(()) => Ok(()),
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
                self.setsockopt_i32_at(libc::SOL_SOCKET, libc::SO_RCVBUF, bytes)
            }
            Err(e) => Err(e),
        }
    }

    fn setsockopt_i32(&self, opt: libc::c_int, value: libc::c_int) -> io::Result<()> {
        self.setsockopt_i32_at(SOL_NETLINK, opt, value)
    }

    fn setsockopt_i32_at(
        &self,
        level: libc::c_int,
        opt: libc::c_int,
        value: libc::c_int,
    ) -> io::Result<()> {
        // SAFETY: setsockopt reads `size_of::<c_int>()` bytes at `&value`.
        let rc = unsafe {
            libc::setsockopt(
                self.fd.get_ref().as_raw_fd(),
                level,
                opt,
                (&value as *const libc::c_int).cast::<libc::c_void>(),
                size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Send a fully-formed netlink message. Returns the number of
    /// bytes accepted by the kernel.
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.writable().await?;
            match guard.try_io(|inner| {
                // SAFETY: `send(2)` reads `buf.len()` bytes at
                // `buf.as_ptr()`.
                let rc = unsafe {
                    libc::send(
                        inner.as_raw_fd(),
                        buf.as_ptr().cast::<libc::c_void>(),
                        buf.len(),
                        0,
                    )
                };
                if rc < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(rc as usize)
                }
            }) {
                Ok(res) => return res,
                Err(_would_block) => continue,
            }
        }
    }

    /// Receive one netlink datagram into `buf`. Returns the number
    /// of bytes written.
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let mut guard = self.fd.readable().await?;
            match guard.try_io(|inner| {
                // SAFETY: `recv(2)` writes up to `buf.len()` bytes at
                // `buf.as_mut_ptr()`.
                let rc = unsafe {
                    libc::recv(
                        inner.as_raw_fd(),
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                        buf.len(),
                        0,
                    )
                };
                if rc < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(rc as usize)
                }
            }) {
                Ok(res) => return res,
                Err(_would_block) => continue,
            }
        }
    }
}

impl AsRawFd for NetlinkSocket {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.fd.get_ref().as_raw_fd()
    }
}
