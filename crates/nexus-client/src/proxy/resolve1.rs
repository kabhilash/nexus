//! `org.freedesktop.resolve1.Link` proxy (off-Nexus). The DNS row
//! on the connected card reads this property — see
//! integration-knowledge-graph
//! `flow:read-ip-info-for-connected-iface` step 5.

#[zbus::proxy(
    interface = "org.freedesktop.resolve1.Link",
    default_service = "org.freedesktop.resolve1"
)]
pub trait ResolveLink {
    /// `DNS` (a(iay)) — per-link DNS server list. Each entry is
    /// (address_family, address_bytes); family `2` is IPv4
    /// (4 bytes), family `10` is IPv6 (16 bytes). Available since
    /// systemd 232.
    #[zbus(property, name = "DNS")]
    fn dns(&self) -> zbus::Result<Vec<(i32, Vec<u8>)>>;
}
