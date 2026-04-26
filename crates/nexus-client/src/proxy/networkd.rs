//! `org.freedesktop.network1.Manager` proxy (off-Nexus). Used by
//! `wifi show` / `eth show` to source the IP-layer rows on the
//! connected card — Nexus deliberately does not publish IPv4 /
//! IPv6 / route state on `fi.nexus.Interface` (nexus-architecture.md
//! ADR-001 `No built-in IP management`). See
//! integration-knowledge-graph `flow:read-ip-info-for-connected-iface`.

#[zbus::proxy(
    interface = "org.freedesktop.network1.Manager",
    default_service = "org.freedesktop.network1",
    default_path = "/org/freedesktop/network1"
)]
pub trait NetworkdManager {
    /// `DescribeLink(ifindex: i) -> (json: s)` — per-link JSON dump.
    /// Carries `Addresses[]` / `Routes[]` / DHCP-lease metadata for
    /// the requested interface. (`Describe()` itself is the no-arg
    /// manager-wide dump; the per-link split was added in systemd
    /// 250.)
    #[zbus(name = "DescribeLink")]
    fn describe_link(&self, ifindex: i32) -> zbus::Result<String>;

    /// `Describe() -> (json: s)` — manager-wide JSON dump. Used as
    /// the older-systemd fallback when `DescribeLink` is missing;
    /// the per-link blob is then extracted from the `Interfaces[]`
    /// array by ifindex.
    #[zbus(name = "Describe")]
    fn describe(&self) -> zbus::Result<String>;
}
