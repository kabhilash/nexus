//! Cold-boot enumeration (DD-001 §5.4). Turns the raw netlink dumps
//! and udev scans into `Vec<InterfaceInfo>` ready to seed the
//! registry before the main loop starts.
//!
//! This module is organized as pure parsing helpers first (unit-test
//! friendly, no I/O) and async wiring underneath that drives the
//! real sockets.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use nexus_core::{InterfaceInfo, InterfaceKind, Nl80211IfType, OperState, PhyCapabilities};

use crate::MonitorError;
use crate::netlink::genl::{
    FamilyInfo, GENL_HDRLEN, GenlHeader, ResolveError, parse_genl_header, resolve_family,
};
use crate::netlink::nl80211::{
    NL80211_ATTR_IFINDEX, NL80211_ATTR_IFNAME, NL80211_ATTR_IFTYPE, NL80211_ATTR_WDEV,
    NL80211_ATTR_WIPHY, NL80211_CMD_GET_INTERFACE, NL80211_FAMILY_NAME, NL80211_GENL_VERSION,
};
use crate::netlink::parser::{
    AttributeIter, MessageIter, NLM_F_DUMP, NLM_F_REQUEST, NLMSG_DONE, NLMSG_ERROR, NLMSG_HDRLEN,
    NetlinkMessageHeader, finalize_message_length, parse_nlmsgerr,
};
use crate::netlink::rtnl::{
    ARPHRD_ETHER, IF_OPER_DORMANT, IF_OPER_DOWN, IF_OPER_LOWERLAYERDOWN, IF_OPER_NOTPRESENT,
    IF_OPER_TESTING, IF_OPER_UNKNOWN, IF_OPER_UP, LinkMessage, RTM_DELLINK, RTM_NEWLINK,
    build_rtm_getlink_dump_request, parse_link_message,
};
use crate::netlink::socket::NetlinkSocket;
use crate::registry::{Registry, bt_ifindex};
use crate::udev::{enumerate_bluetooth, enumerate_gnss};

// ---------------------------------------------------------------------------
// Intermediate types produced by the nl80211 interface dump.
// ---------------------------------------------------------------------------

/// Subset of `NL80211_CMD_NEW_INTERFACE` attributes Nexus consumes at
/// cold boot. Full PHY capability parsing comes in a later prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nl80211InterfaceInfo {
    pub ifindex: u32,
    pub ifname: Option<String>,
    pub wiphy: u32,
    pub wdev: u64,
    pub iftype: u32,
}

/// Parse one `NL80211_CMD_NEW_INTERFACE` payload into a minimal info
/// struct. Returns `None` if the required attributes are missing.
pub fn parse_nl80211_interface_attrs(
    attrs_buf: &[u8],
) -> Result<Option<Nl80211InterfaceInfo>, crate::netlink::ParseError> {
    let mut ifindex: Option<u32> = None;
    let mut ifname: Option<String> = None;
    let mut wiphy: Option<u32> = None;
    let mut wdev: Option<u64> = None;
    let mut iftype: Option<u32> = None;

    for attr in AttributeIter::new(attrs_buf) {
        let attr = attr?;
        match attr.attr_type {
            NL80211_ATTR_IFINDEX => ifindex = Some(attr.u32()?),
            NL80211_ATTR_IFNAME => ifname = Some(attr.cstr()?.to_owned()),
            NL80211_ATTR_WIPHY => wiphy = Some(attr.u32()?),
            NL80211_ATTR_WDEV => wdev = Some(attr.u64()?),
            NL80211_ATTR_IFTYPE => iftype = Some(attr.u32()?),
            _ => {}
        }
    }

    match (ifindex, wiphy, wdev, iftype) {
        (Some(ifindex), Some(wiphy), Some(wdev), Some(iftype)) => Ok(Some(Nl80211InterfaceInfo {
            ifindex,
            ifname,
            wiphy,
            wdev,
            iftype,
        })),
        _ => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Pure: LinkMessage → InterfaceInfo.
// ---------------------------------------------------------------------------

/// Map a raw `IFLA_OPERSTATE` byte to [`OperState`]. Unknown values
/// (new kernels may add more) fall through to `Unknown`.
pub fn map_operstate(raw: u8) -> OperState {
    match raw {
        IF_OPER_NOTPRESENT => OperState::NotPresent,
        IF_OPER_DOWN => OperState::Down,
        IF_OPER_LOWERLAYERDOWN => OperState::LowerLayerDown,
        IF_OPER_TESTING => OperState::Testing,
        IF_OPER_DORMANT => OperState::Dormant,
        IF_OPER_UP => OperState::Up,
        IF_OPER_UNKNOWN => OperState::Unknown,
        _ => OperState::Unknown,
    }
}

/// Apply DD-001 §5.1's classification rule: drop non-`ARPHRD_ETHER`
/// hardware and virtual/stacked interfaces, then tag with the
/// Wi-Fi wiphy info from nl80211 where present.
///
/// Returns `None` for interfaces Nexus doesn't manage in v0.1 (veth,
/// bridge, loopback, etc.).
pub fn classify_link(
    link: &LinkMessage,
    wireless: Option<&Nl80211InterfaceInfo>,
) -> Option<InterfaceInfo> {
    if link.header.ifi_type != ARPHRD_ETHER {
        return None;
    }
    if link.is_virtual_kind() {
        return None;
    }

    let kind = match wireless {
        Some(w) => InterfaceKind::Wireless {
            wiphy: w.wiphy,
            wiphy_name: format!("phy{}", w.wiphy),
            wdev: w.wdev,
            iftype: Nl80211IfType(w.iftype),
            capabilities: Arc::new(PhyCapabilities::default()),
        },
        None => InterfaceKind::Ethernet,
    };

    let ifname = link
        .ifname
        .clone()
        .unwrap_or_else(|| format!("if{}", link.header.index));

    Some(InterfaceInfo {
        ifindex: link.header.index as u32,
        ifname,
        mac: link.mac.unwrap_or([0; 6]),
        mtu: link.mtu.unwrap_or(0),
        operstate: map_operstate(link.operstate.unwrap_or(IF_OPER_UNKNOWN)),
        carrier: link.carrier.unwrap_or(false),
        kind,
        discovered_at: Instant::now(),
    })
}

// ---------------------------------------------------------------------------
// Pure: dump-byte-stream parsers. Used both by the async dump
// readers below and by unit tests that feed canned bytes.
// ---------------------------------------------------------------------------

/// Classify every RTM_NEWLINK message in a dump buffer, optionally
/// tagging with wiphy info from a parallel nl80211 dump.
pub fn classify_dump(
    link_messages: &[LinkMessage],
    wireless_by_ifindex: &HashMap<u32, Nl80211InterfaceInfo>,
) -> Vec<InterfaceInfo> {
    link_messages
        .iter()
        .filter_map(|link| {
            let ifindex = link.header.index as u32;
            classify_link(link, wireless_by_ifindex.get(&ifindex))
        })
        .collect()
}

/// Extract every `LinkMessage` from a contiguous rtnl dump buffer,
/// returning the parsed links plus any non-dump messages (hotplug
/// events that arrived on the same socket during the dump — §5.5).
pub fn parse_rtnl_dump(
    buf: &[u8],
    dump_seq: u32,
) -> Result<RtnlDumpResult, crate::netlink::ParseError> {
    let mut out = RtnlDumpResult::default();
    for msg_result in MessageIter::new(buf) {
        let msg = msg_result?;
        let is_dump = msg.header.seq == dump_seq;
        match msg.header.msg_type {
            NLMSG_DONE if is_dump => {
                out.done = true;
                break;
            }
            NLMSG_ERROR if is_dump => {
                let err = parse_nlmsgerr(msg.payload)?;
                if err.error != 0 {
                    out.dump_errno = Some(err.error);
                }
                out.done = true;
                break;
            }
            RTM_NEWLINK => {
                let link = parse_link_message(msg.payload)?;
                if is_dump {
                    out.dump_links.push(link);
                } else {
                    out.pending_newlink.push(link);
                }
            }
            RTM_DELLINK => {
                let link = parse_link_message(msg.payload)?;
                out.pending_dellink.push(link.header.index as u32);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Result of [`parse_rtnl_dump`].
#[derive(Debug, Default, Clone)]
pub struct RtnlDumpResult {
    /// RTM_NEWLINK messages that matched the request sequence.
    pub dump_links: Vec<LinkMessage>,
    /// RTM_NEWLINK messages with a different seq (hotplug events
    /// interleaved with the dump). Queued for post-dump processing
    /// per §5.5.
    pub pending_newlink: Vec<LinkMessage>,
    /// RTM_DELLINK events interleaved with the dump (as ifindex).
    pub pending_dellink: Vec<u32>,
    /// True once NLMSG_DONE for the dump seq was observed.
    pub done: bool,
    /// `Some(negative errno)` when the dump itself returned an error.
    pub dump_errno: Option<i32>,
}

/// Extract the (ifindex → Nl80211InterfaceInfo) map from a contiguous
/// nl80211 GET_INTERFACE dump buffer. Non-matching seq messages are
/// ignored — nl80211 multicast lives on a different socket so
/// there's nothing to preserve here.
pub fn parse_nl80211_interface_dump(
    buf: &[u8],
    dump_seq: u32,
    family_id: u16,
) -> Result<HashMap<u32, Nl80211InterfaceInfo>, crate::netlink::ParseError> {
    let mut out = HashMap::new();
    for msg_result in MessageIter::new(buf) {
        let msg = msg_result?;
        if msg.header.seq != dump_seq {
            continue;
        }
        match msg.header.msg_type {
            NLMSG_DONE => break,
            NLMSG_ERROR => break,
            t if t == family_id => {
                let (_genl, attrs) = parse_genl_header(msg.payload)?;
                if let Some(info) = parse_nl80211_interface_attrs(attrs)? {
                    out.insert(info.ifindex, info);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Request builders.
// ---------------------------------------------------------------------------

/// Build an NL80211 `CMD_GET_INTERFACE` dump request.
pub fn build_nl80211_get_interface_dump_request(seq: u32, port_id: u32, family_id: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(NLMSG_HDRLEN + GENL_HDRLEN);
    let hdr = NetlinkMessageHeader {
        length: 0,
        msg_type: family_id,
        flags: NLM_F_REQUEST | NLM_F_DUMP,
        seq,
        pid: port_id,
    };
    buf.extend_from_slice(&hdr.to_bytes());
    buf.extend_from_slice(
        &GenlHeader {
            cmd: NL80211_CMD_GET_INTERFACE,
            version: NL80211_GENL_VERSION,
        }
        .to_bytes(),
    );
    finalize_message_length(&mut buf);
    buf
}

// ---------------------------------------------------------------------------
// Async wiring: drive the parsers against real sockets.
// ---------------------------------------------------------------------------

const DUMP_RECV_BUFFER: usize = 65536;

/// Send an `RTM_GETLINK` dump, read until `NLMSG_DONE`, and return the
/// parsed result. Hotplug events interleaved with the dump are
/// surfaced through the `pending_*` fields per DD-001 §5.5.
pub async fn dump_rtnl_links(socket: &NetlinkSocket) -> Result<RtnlDumpResult, MonitorError> {
    let seq = socket.next_seq();
    let request = build_rtm_getlink_dump_request(seq, socket.port_id());
    socket.send(&request).await?;

    let mut buf = vec![0u8; DUMP_RECV_BUFFER];
    let mut accumulated = RtnlDumpResult::default();
    while !accumulated.done {
        let n = socket.recv(&mut buf).await?;
        let partial = parse_rtnl_dump(&buf[..n], seq)?;
        accumulated.dump_links.extend(partial.dump_links);
        accumulated.pending_newlink.extend(partial.pending_newlink);
        accumulated.pending_dellink.extend(partial.pending_dellink);
        if partial.done {
            accumulated.done = true;
            accumulated.dump_errno = partial.dump_errno;
        }
    }

    if let Some(errno) = accumulated.dump_errno {
        return Err(MonitorError::RtnlDumpFailed(errno));
    }
    Ok(accumulated)
}

/// Send an `NL80211_CMD_GET_INTERFACE` dump and parse the
/// responses. Returns a map keyed by ifindex.
pub async fn dump_nl80211_interfaces(
    socket: &NetlinkSocket,
    family_id: u16,
) -> Result<HashMap<u32, Nl80211InterfaceInfo>, MonitorError> {
    let seq = socket.next_seq();
    let request = build_nl80211_get_interface_dump_request(seq, socket.port_id(), family_id);
    socket.send(&request).await?;

    let mut out = HashMap::new();
    let mut buf = vec![0u8; DUMP_RECV_BUFFER];
    'outer: loop {
        let n = socket.recv(&mut buf).await?;
        for msg_result in MessageIter::new(&buf[..n]) {
            let msg = msg_result?;
            if msg.header.seq != seq {
                continue;
            }
            match msg.header.msg_type {
                NLMSG_DONE => break 'outer,
                NLMSG_ERROR => {
                    let err = parse_nlmsgerr(msg.payload)?;
                    if err.error != 0 {
                        tracing::warn!(
                            errno = err.error,
                            "nl80211 GET_INTERFACE returned an error; Wi-Fi classification degraded",
                        );
                    }
                    break 'outer;
                }
                t if t == family_id => {
                    let (_genl, attrs) = parse_genl_header(msg.payload)?;
                    if let Some(info) = parse_nl80211_interface_attrs(attrs)? {
                        out.insert(info.ifindex, info);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// Resolve the nl80211 family. `FamilyUnavailable` (Wi-Fi not on this
/// kernel) is mapped to `Ok(None)` — the cold-boot path then treats
/// every interface as Ethernet per DD-001 §9.4.
pub async fn resolve_nl80211(socket: &NetlinkSocket) -> Result<Option<FamilyInfo>, MonitorError> {
    match resolve_family(socket, NL80211_FAMILY_NAME).await {
        Ok(info) => Ok(Some(info)),
        Err(ResolveError::FamilyUnavailable { name, errno }) => {
            tracing::warn!(
                family = name,
                errno,
                "nl80211 family unavailable; wireless classification disabled",
            );
            Ok(None)
        }
        Err(e) => Err(MonitorError::from(e)),
    }
}

// ---------------------------------------------------------------------------
// Cold-boot orchestration.
// ---------------------------------------------------------------------------

/// Deps the cold-boot path needs; injected so the orchestrator can be
/// tested against fakes if needed.
pub struct ColdBoot<'a> {
    pub rtnl: &'a NetlinkSocket,
    pub nl80211_rr: Option<&'a NetlinkSocket>,
    pub nl80211_family: Option<&'a FamilyInfo>,
}

/// Run DD-001 §5.4's cold-boot sequence end-to-end: rtnl dump →
/// nl80211 classify → udev scan → synthesize InterfaceInfo records
/// and (for BT/GNSS) allocate synthesized ifindex values against the
/// supplied registry.
///
/// Returns the full list of discovered interfaces plus any rtnl
/// events that arrived on the multicast socket during the dump
/// (queued per §5.5, to be processed immediately after).
pub async fn cold_boot_enumerate(
    deps: ColdBoot<'_>,
    registry: &mut Registry,
) -> Result<ColdBootOutcome, MonitorError> {
    let dump = dump_rtnl_links(deps.rtnl).await?;

    let wireless_by_ifindex = match (deps.nl80211_rr, deps.nl80211_family) {
        (Some(rr), Some(family)) => dump_nl80211_interfaces(rr, family.id).await?,
        _ => HashMap::new(),
    };

    let mut discovered: Vec<InterfaceInfo> = classify_dump(&dump.dump_links, &wireless_by_ifindex);

    // Bluetooth + GNSS via udev. udev failures are logged but don't
    // fail cold boot — the network side can still function.
    match enumerate_bluetooth() {
        Ok(adapters) => {
            for adapter in adapters {
                let info = InterfaceInfo {
                    ifindex: bt_ifindex(adapter.hci_index),
                    ifname: adapter.hci_name.clone(),
                    mac: adapter.bt_address.unwrap_or([0; 6]),
                    mtu: 0,
                    operstate: OperState::Up,
                    carrier: true,
                    kind: InterfaceKind::Bluetooth {
                        hci_name: adapter.hci_name,
                        hci_index: adapter.hci_index,
                        bt_address: nexus_core::MacAddr(adapter.bt_address.unwrap_or([0; 6])),
                        bluez_path: adapter.bluez_path,
                    },
                    discovered_at: Instant::now(),
                };
                discovered.push(info);
            }
        }
        Err(e) => tracing::warn!(error = %e, "udev Bluetooth enumeration failed"),
    }

    match enumerate_gnss() {
        Ok(devices) => {
            for device in devices {
                let ifindex = registry.allocate_gnss_ifindex();
                let ifname = device
                    .device_path
                    .file_name()
                    .map(|o| o.to_string_lossy().into_owned())
                    .unwrap_or_else(|| device.gpsd_device.clone());
                let info = InterfaceInfo {
                    ifindex,
                    ifname,
                    mac: [0; 6],
                    mtu: 0,
                    operstate: OperState::Up,
                    carrier: true,
                    kind: InterfaceKind::Gnss {
                        device_path: device.device_path.to_string_lossy().into_owned(),
                        gpsd_device: device.gpsd_device,
                        vendor_model: device.vendor_model,
                    },
                    discovered_at: Instant::now(),
                };
                discovered.push(info);
            }
        }
        Err(e) => tracing::warn!(error = %e, "udev GNSS enumeration failed"),
    }

    for info in &discovered {
        registry.insert(info.clone());
    }

    Ok(ColdBootOutcome {
        discovered,
        pending_newlink: dump.pending_newlink,
        pending_dellink: dump.pending_dellink,
    })
}

/// Result of [`cold_boot_enumerate`]. `pending_*` queues hold events
/// that arrived on the rtnl multicast subscription during the dump.
#[derive(Debug, Default)]
pub struct ColdBootOutcome {
    pub discovered: Vec<InterfaceInfo>,
    pub pending_newlink: Vec<LinkMessage>,
    pub pending_dellink: Vec<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::netlink::parser::{NLM_F_MULTI, encode_attribute, finalize_message_length};
    use crate::netlink::rtnl::{
        ARPHRD_ETHER, IFF_BROADCAST, IFF_LOWER_UP, IFF_RUNNING, IFF_UP, IFINFOMSG_SIZE,
        IFLA_ADDRESS, IFLA_CARRIER, IFLA_IFNAME, IFLA_INFO_KIND, IFLA_LINKINFO, IFLA_MTU,
        IFLA_OPERSTATE, IfInfoHeader,
    };

    fn build_newlink(
        seq: u32,
        ifindex: i32,
        ifname: &str,
        mac: [u8; 6],
        virt_kind: Option<&str>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        let hdr = NetlinkMessageHeader {
            length: 0,
            msg_type: RTM_NEWLINK,
            flags: NLM_F_MULTI,
            seq,
            pid: 0,
        };
        buf.extend_from_slice(&hdr.to_bytes());
        let ifi = IfInfoHeader {
            family: 0,
            ifi_type: ARPHRD_ETHER,
            index: ifindex,
            flags: IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_LOWER_UP,
            change: 0xFFFF_FFFF,
        };
        buf.extend_from_slice(&ifi.to_bytes());

        let mut name_bytes = Vec::from(ifname.as_bytes());
        name_bytes.push(0);
        encode_attribute(&mut buf, IFLA_IFNAME, &name_bytes);
        encode_attribute(&mut buf, IFLA_ADDRESS, &mac);
        encode_attribute(&mut buf, IFLA_MTU, &1500u32.to_ne_bytes());
        encode_attribute(&mut buf, IFLA_OPERSTATE, &[IF_OPER_UP]);
        encode_attribute(&mut buf, IFLA_CARRIER, &[1u8]);

        if let Some(kind) = virt_kind {
            let mut linkinfo_payload = Vec::new();
            let mut kind_bytes = Vec::from(kind.as_bytes());
            kind_bytes.push(0);
            encode_attribute(&mut linkinfo_payload, IFLA_INFO_KIND, &kind_bytes);
            encode_attribute(&mut buf, IFLA_LINKINFO, &linkinfo_payload);
        }

        finalize_message_length(&mut buf);
        buf
    }

    fn nlmsg_done(seq: u32) -> Vec<u8> {
        let hdr = NetlinkMessageHeader {
            length: NLMSG_HDRLEN as u32,
            msg_type: NLMSG_DONE,
            flags: NLM_F_MULTI,
            seq,
            pid: 0,
        };
        hdr.to_bytes().to_vec()
    }

    #[test]
    fn parse_rtnl_dump_collects_matching_seq_and_queues_pending() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&build_newlink(42, 2, "eth0", [0xAA; 6], None));
        stream.extend_from_slice(&build_newlink(42, 3, "eth1", [0xBB; 6], None));
        // Hotplug event arriving with a different seq during dump.
        stream.extend_from_slice(&build_newlink(0, 4, "eth2", [0xCC; 6], None));
        stream.extend_from_slice(&nlmsg_done(42));

        let result = parse_rtnl_dump(&stream, 42).unwrap();
        assert!(result.done);
        assert_eq!(result.dump_errno, None);
        assert_eq!(result.dump_links.len(), 2);
        assert_eq!(result.pending_newlink.len(), 1);
        assert_eq!(result.pending_dellink.len(), 0);
        assert_eq!(result.dump_links[0].ifname.as_deref(), Some("eth0"));
        assert_eq!(result.dump_links[1].ifname.as_deref(), Some("eth1"));
        assert_eq!(result.pending_newlink[0].ifname.as_deref(), Some("eth2"));
    }

    #[test]
    fn classify_dump_filters_virtual_interfaces() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&build_newlink(42, 2, "eth0", [0x01; 6], None));
        stream.extend_from_slice(&build_newlink(42, 3, "veth0", [0x02; 6], Some("veth")));
        stream.extend_from_slice(&build_newlink(42, 4, "br0", [0x03; 6], Some("bridge")));
        stream.extend_from_slice(&nlmsg_done(42));

        let dump = parse_rtnl_dump(&stream, 42).unwrap();
        let infos = classify_dump(&dump.dump_links, &HashMap::new());
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].ifname, "eth0");
        assert!(matches!(infos[0].kind, InterfaceKind::Ethernet));
    }

    #[test]
    fn classify_promotes_to_wireless_when_nl80211_lookup_matches() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&build_newlink(42, 5, "wlan0", [0xCC; 6], None));
        stream.extend_from_slice(&nlmsg_done(42));
        let dump = parse_rtnl_dump(&stream, 42).unwrap();

        let mut wireless = HashMap::new();
        wireless.insert(
            5,
            Nl80211InterfaceInfo {
                ifindex: 5,
                ifname: Some("wlan0".into()),
                wiphy: 0,
                wdev: 1,
                iftype: 2, // NL80211_IFTYPE_STATION
            },
        );

        let infos = classify_dump(&dump.dump_links, &wireless);
        assert_eq!(infos.len(), 1);
        match &infos[0].kind {
            InterfaceKind::Wireless {
                wiphy,
                wdev,
                iftype,
                ..
            } => {
                assert_eq!(*wiphy, 0);
                assert_eq!(*wdev, 1);
                assert_eq!(iftype.0, 2);
            }
            other => panic!("expected Wireless, got {other:?}"),
        }
    }

    #[test]
    fn map_operstate_covers_every_named_variant() {
        assert_eq!(map_operstate(IF_OPER_UP), OperState::Up);
        assert_eq!(map_operstate(IF_OPER_DOWN), OperState::Down);
        assert_eq!(
            map_operstate(IF_OPER_LOWERLAYERDOWN),
            OperState::LowerLayerDown,
        );
        assert_eq!(map_operstate(IF_OPER_DORMANT), OperState::Dormant);
        assert_eq!(map_operstate(IF_OPER_TESTING), OperState::Testing);
        assert_eq!(map_operstate(IF_OPER_NOTPRESENT), OperState::NotPresent);
        assert_eq!(map_operstate(IF_OPER_UNKNOWN), OperState::Unknown);
        // Future kernel: unknown value defaults to Unknown.
        assert_eq!(map_operstate(99), OperState::Unknown);
    }

    #[test]
    fn build_nl80211_get_interface_dump_layout() {
        let bytes = build_nl80211_get_interface_dump_request(1, 99, 0x17);
        // 16 nlmsghdr + 4 genlmsghdr = 20 bytes (no attrs).
        assert_eq!(bytes.len(), 20);
        // Length is patched into the first 4 bytes.
        assert_eq!(
            u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            20,
        );
        // Type is the family id (0x17) in host byte order.
        assert_eq!(u16::from_ne_bytes([bytes[4], bytes[5]]), 0x17);
        // genlmsghdr
        assert_eq!(
            &bytes[NLMSG_HDRLEN..NLMSG_HDRLEN + GENL_HDRLEN],
            &[NL80211_CMD_GET_INTERFACE, NL80211_GENL_VERSION, 0, 0],
        );
    }

    #[test]
    fn ifinfomsg_size_matches_dd_001() {
        assert_eq!(IFINFOMSG_SIZE, 16);
    }
}
