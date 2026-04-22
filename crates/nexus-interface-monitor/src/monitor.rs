//! Main Interface Monitor task: cold-boot enumeration followed by
//! the `tokio::select!` loop that multiplexes rtnetlink, nl80211
//! multicast, and udev monitor events. See DD-001 §8 and Phase 6.

use std::collections::HashMap;
use std::future::pending;

use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, OperState};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::MonitorError;
use crate::enumerate::{
    ColdBoot, Nl80211InterfaceInfo, classify_link, cold_boot_enumerate, dump_nl80211_interfaces,
    resolve_nl80211,
};
use crate::netlink::genl::FamilyInfo;
use crate::netlink::nl80211::{
    NL80211_MCAST_GROUP_CONFIG, NL80211_MCAST_GROUP_MLME, NL80211_MCAST_GROUP_SCAN,
};
use crate::netlink::parser::{MessageIter, NLMSG_DONE, NLMSG_ERROR};
use crate::netlink::rtnl::{
    LinkMessage, RTM_DELLINK, RTM_NEWLINK, RTMGRP_LINK, parse_link_message,
};
use crate::netlink::socket::{NETLINK_GENERIC, NETLINK_ROUTE, NetlinkSocket};
use crate::registry::{Registry, bt_ifindex};
use crate::udev::{BluetoothAdapter, GnssDevice, UdevAction, UdevMonitorHandle};

const RECV_BUFFER: usize = 65536;

/// Everything the monitor task owns for the duration of its run.
pub struct MonitorTask {
    pub registry: Registry,
    pub event_tx: broadcast::Sender<NexusEvent>,
    pub rtnl: NetlinkSocket,
    pub nl80211_rr: Option<NetlinkSocket>,
    pub nl80211_mcast: Option<NetlinkSocket>,
    pub nl80211_family: Option<FamilyInfo>,
    pub udev: Option<UdevMonitorHandle>,
    /// Map of Wi-Fi ifindex → classification metadata captured at
    /// cold boot. Runtime new interfaces default to Ethernet until
    /// the classification state machine (Phase 7) is wired up.
    pub wireless_by_ifindex: HashMap<u32, Nl80211InterfaceInfo>,
}

impl MonitorTask {
    /// Open every socket and drop a warning (rather than failing) if
    /// an optional subsystem isn't available on this kernel.
    pub async fn bootstrap(event_tx: broadcast::Sender<NexusEvent>) -> Result<Self, MonitorError> {
        let rtnl = NetlinkSocket::open(NETLINK_ROUTE, RTMGRP_LINK)?;
        // Best-effort: wider receive buffer and richer error
        // reporting. DD-001 §4.1 says CAP_NET_ADMIN is not required
        // to fall back, and older kernels may not expose the options.
        let _ = rtnl.set_recv_buffer(1024 * 1024);
        let _ = rtnl.set_ext_ack(true);
        let _ = rtnl.set_strict_check(true);

        let nl80211_rr = NetlinkSocket::open(NETLINK_GENERIC, 0).ok();
        let nl80211_mcast = NetlinkSocket::open(NETLINK_GENERIC, 0).ok();

        let nl80211_family = if let Some(rr) = nl80211_rr.as_ref() {
            resolve_nl80211(rr).await?
        } else {
            None
        };

        if let (Some(family), Some(mcast)) = (&nl80211_family, &nl80211_mcast) {
            for group in [
                NL80211_MCAST_GROUP_CONFIG,
                NL80211_MCAST_GROUP_SCAN,
                NL80211_MCAST_GROUP_MLME,
            ] {
                if let Some(id) = family.mcast_group(group) {
                    if let Err(e) = mcast.join_multicast(id) {
                        tracing::warn!(group, error = %e, "failed to join nl80211 mcast group");
                    }
                }
            }
        }

        let udev = match UdevMonitorHandle::spawn().await {
            Ok(u) => Some(u),
            Err(e) => {
                tracing::warn!(error = %e, "udev monitor unavailable; Bluetooth/GNSS hotplug disabled");
                None
            }
        };

        Ok(Self {
            registry: Registry::new(),
            event_tx,
            rtnl,
            nl80211_rr,
            nl80211_mcast,
            nl80211_family,
            udev,
            wireless_by_ifindex: HashMap::new(),
        })
    }

    /// Drive the monitor. Returns `Ok(())` on clean shutdown via the
    /// cancellation token.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<(), MonitorError> {
        // Cold-boot enumeration.
        let outcome = cold_boot_enumerate(
            ColdBoot {
                rtnl: &self.rtnl,
                nl80211_rr: self.nl80211_rr.as_ref(),
                nl80211_family: self.nl80211_family.as_ref(),
            },
            &mut self.registry,
        )
        .await?;

        // Remember the wireless classification for runtime handling.
        if let (Some(rr), Some(family)) = (self.nl80211_rr.as_ref(), self.nl80211_family.as_ref()) {
            self.wireless_by_ifindex = dump_nl80211_interfaces(rr, family.id).await?;
        }

        for info in &outcome.discovered {
            send_event(
                &self.event_tx,
                NexusEvent::InterfaceDiscovered(info.clone()),
            );
        }

        // Drain the rtnl hotplug queue that accumulated during the
        // dump (§5.5). We already applied cold-boot records to the
        // registry; `apply_rtnl_*link` diffs against that.
        for link in outcome.pending_newlink {
            apply_rtnl_newlink(
                &mut self.registry,
                &self.event_tx,
                &self.wireless_by_ifindex,
                &link,
            );
        }
        for ifindex in outcome.pending_dellink {
            apply_rtnl_dellink(&mut self.registry, &self.event_tx, ifindex);
        }

        // Main loop. Destructure into the fields we need so the
        // select!'s futures can hold disjoint borrows.
        let Self {
            mut registry,
            event_tx,
            rtnl,
            nl80211_mcast,
            mut udev,
            wireless_by_ifindex,
            ..
        } = self;

        let mut rtnl_buf = vec![0u8; RECV_BUFFER];
        let mut nl80211_buf = vec![0u8; RECV_BUFFER];

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::info!("interface monitor shutting down");
                    return Ok(());
                }
                res = rtnl.recv(&mut rtnl_buf) => {
                    let n = res?;
                    if let Err(e) = process_rtnl_datagram(
                        &mut registry,
                        &event_tx,
                        &wireless_by_ifindex,
                        &rtnl_buf[..n],
                    ) {
                        tracing::warn!(error = %e, "rtnl parse error; continuing");
                    }
                }
                res = optional_recv(nl80211_mcast.as_ref(), &mut nl80211_buf) => {
                    match res {
                        Ok(n) => {
                            // Phase 7+ will parse these; for now
                            // just acknowledge traffic so the kernel
                            // doesn't buffer indefinitely.
                            tracing::debug!(bytes = n, "nl80211 mcast datagram");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "nl80211 mcast recv failed");
                        }
                    }
                }
                Some(action) = optional_udev(udev.as_mut()) => {
                    handle_udev_action(&mut registry, &event_tx, action);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stand-alone handlers — kept free-function so the main loop can hold
// disjoint borrows of `self`'s fields across `select!` branches.
// ---------------------------------------------------------------------------

fn process_rtnl_datagram(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    wireless: &HashMap<u32, Nl80211InterfaceInfo>,
    buf: &[u8],
) -> Result<(), MonitorError> {
    for msg_result in MessageIter::new(buf) {
        let msg = msg_result?;
        match msg.header.msg_type {
            NLMSG_DONE | NLMSG_ERROR => {}
            RTM_NEWLINK => {
                let link = parse_link_message(msg.payload)?;
                apply_rtnl_newlink(registry, event_tx, wireless, &link);
            }
            RTM_DELLINK => {
                let link = parse_link_message(msg.payload)?;
                apply_rtnl_dellink(registry, event_tx, link.header.index as u32);
            }
            _ => {}
        }
    }
    Ok(())
}

fn apply_rtnl_newlink(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    wireless: &HashMap<u32, Nl80211InterfaceInfo>,
    link: &LinkMessage,
) {
    let ifindex = link.header.index as u32;
    let wireless_info = wireless.get(&ifindex);
    let classified = match classify_link(link, wireless_info) {
        Some(info) => info,
        None => return, // filtered (virtual / non-ether)
    };

    match registry.get(ifindex).cloned() {
        None => {
            registry.insert(classified.clone());
            send_event(event_tx, NexusEvent::InterfaceDiscovered(classified));
        }
        Some(existing) => {
            let mut next = existing.clone();
            if existing.carrier != classified.carrier {
                next.carrier = classified.carrier;
                send_event(
                    event_tx,
                    NexusEvent::CarrierChanged {
                        ifindex,
                        up: classified.carrier,
                    },
                );
            }
            if existing.operstate != classified.operstate {
                next.operstate = classified.operstate;
                send_event(
                    event_tx,
                    NexusEvent::OperstateChanged {
                        ifindex,
                        state: classified.operstate,
                    },
                );
            }
            // Other fields (mtu, mac, name) can change but don't
            // warrant a dedicated NexusEvent variant in v0.1.
            next.ifname = classified.ifname.clone();
            next.mac = classified.mac;
            next.mtu = classified.mtu;
            registry.insert(next);
        }
    }
}

fn apply_rtnl_dellink(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    ifindex: u32,
) {
    if registry.remove(ifindex).is_some() {
        send_event(event_tx, NexusEvent::InterfaceRemoved { ifindex });
    }
}

fn handle_udev_action(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    action: UdevAction,
) {
    match action {
        UdevAction::BluetoothAdd(adapter) => apply_bluetooth_add(registry, event_tx, adapter),
        UdevAction::BluetoothRemove { hci_index, .. } => {
            apply_bluetooth_remove(registry, event_tx, hci_index);
        }
        UdevAction::GnssAdd(device) => apply_gnss_add(registry, event_tx, device),
        UdevAction::GnssRemove { device_path } => {
            apply_gnss_remove(registry, event_tx, &device_path);
        }
    }
}

fn apply_bluetooth_add(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    adapter: BluetoothAdapter,
) {
    let ifindex = bt_ifindex(adapter.hci_index);
    if registry.contains(ifindex) {
        return;
    }
    let mac = adapter.bt_address.unwrap_or([0; 6]);
    let info = InterfaceInfo {
        ifindex,
        ifname: adapter.hci_name.clone(),
        mac,
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Bluetooth {
            hci_name: adapter.hci_name,
            hci_index: adapter.hci_index,
            bt_address: nexus_core::MacAddr(mac),
            bluez_path: adapter.bluez_path,
        },
        discovered_at: std::time::Instant::now(),
    };
    registry.insert(info.clone());
    send_event(event_tx, NexusEvent::InterfaceDiscovered(info));
}

fn apply_bluetooth_remove(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    hci_index: u32,
) {
    let ifindex = bt_ifindex(hci_index);
    apply_rtnl_dellink(registry, event_tx, ifindex);
}

fn apply_gnss_add(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    device: GnssDevice,
) {
    if registry.iter().any(|info| {
        matches!(
            &info.kind,
            InterfaceKind::Gnss { device_path, .. }
                if std::path::Path::new(device_path) == device.device_path,
        )
    }) {
        return;
    }
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
        discovered_at: std::time::Instant::now(),
    };
    registry.insert(info.clone());
    send_event(event_tx, NexusEvent::InterfaceDiscovered(info));
}

fn apply_gnss_remove(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    device_path: &std::path::Path,
) {
    let target = registry
        .iter()
        .find(|info| {
            matches!(
                &info.kind,
                InterfaceKind::Gnss { device_path: p, .. }
                    if std::path::Path::new(p) == device_path,
            )
        })
        .map(|info| info.ifindex);
    if let Some(ifindex) = target {
        apply_rtnl_dellink(registry, event_tx, ifindex);
    }
}

// ---------------------------------------------------------------------------
// Helpers for the conditional-branch futures in tokio::select!.
// ---------------------------------------------------------------------------

/// Read from an optional socket, or pend forever if absent.
async fn optional_recv(socket: Option<&NetlinkSocket>, buf: &mut [u8]) -> std::io::Result<usize> {
    match socket {
        Some(s) => s.recv(buf).await,
        None => pending().await,
    }
}

/// Yield the next udev action, or pend forever if the monitor thread
/// isn't running.
async fn optional_udev(udev: Option<&mut UdevMonitorHandle>) -> Option<UdevAction> {
    match udev {
        Some(u) => u.next_action().await,
        None => pending().await,
    }
}

/// broadcast-send helper. A channel with zero receivers returns
/// `Err`, which per DD-001 Phase 5's event-bus rule is non-fatal —
/// the monitor's job is to emit, not to ensure delivery.
pub(crate) fn send_event(tx: &broadcast::Sender<NexusEvent>, event: NexusEvent) {
    if let Err(e) = tx.send(event) {
        tracing::trace!(error = %e, "no active receivers for NexusEvent");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Instant;

    use nexus_core::{InterfaceInfo, InterfaceKind, OperState};
    use tokio::sync::broadcast;

    use super::*;
    use crate::netlink::parser::{
        NLM_F_MULTI, NLMSG_HDRLEN, NetlinkMessageHeader, encode_attribute, finalize_message_length,
    };
    use crate::netlink::rtnl::{
        ARPHRD_ETHER, IF_OPER_UP, IFF_BROADCAST, IFF_LOWER_UP, IFF_RUNNING, IFF_UP, IFLA_ADDRESS,
        IFLA_CARRIER, IFLA_IFNAME, IFLA_MTU, IFLA_OPERSTATE, IfInfoHeader,
    };

    fn build_newlink(ifindex: i32, ifname: &str, carrier: bool) -> Vec<u8> {
        let mut buf = Vec::new();
        let hdr = NetlinkMessageHeader {
            length: 0,
            msg_type: RTM_NEWLINK,
            flags: NLM_F_MULTI,
            seq: 0,
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
        encode_attribute(&mut buf, IFLA_ADDRESS, &[0xAA; 6]);
        encode_attribute(&mut buf, IFLA_MTU, &1500u32.to_ne_bytes());
        encode_attribute(&mut buf, IFLA_OPERSTATE, &[IF_OPER_UP]);
        encode_attribute(&mut buf, IFLA_CARRIER, &[u8::from(carrier)]);
        finalize_message_length(&mut buf);
        buf
    }

    fn seed_eth0(registry: &mut Registry) {
        registry.insert(InterfaceInfo {
            ifindex: 2,
            ifname: "eth0".to_owned(),
            mac: [0xAA; 6],
            mtu: 1500,
            operstate: OperState::Up,
            carrier: true,
            kind: InterfaceKind::Ethernet,
            discovered_at: Instant::now(),
        });
    }

    fn drain_events(rx: &mut broadcast::Receiver<NexusEvent>) -> Vec<NexusEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    #[test]
    fn new_interface_emits_interface_discovered() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut registry = Registry::new();
        let wireless = HashMap::new();

        let raw = build_newlink(2, "eth0", true);
        let (msg, _) = crate::netlink::parser::parse_message(&raw).unwrap();
        let link = parse_link_message(msg.payload).unwrap();
        apply_rtnl_newlink(&mut registry, &tx, &wireless, &link);

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            NexusEvent::InterfaceDiscovered(info) => {
                assert_eq!(info.ifindex, 2);
                assert_eq!(info.ifname, "eth0");
                assert!(info.carrier);
            }
            other => panic!("expected InterfaceDiscovered, got {other:?}"),
        }
    }

    #[test]
    fn carrier_change_on_known_interface_emits_carrier_changed() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut registry = Registry::new();
        seed_eth0(&mut registry);
        let wireless = HashMap::new();

        let raw = build_newlink(2, "eth0", false); // carrier dropped
        let (msg, _) = crate::netlink::parser::parse_message(&raw).unwrap();
        let link = parse_link_message(msg.payload).unwrap();
        apply_rtnl_newlink(&mut registry, &tx, &wireless, &link);

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            NexusEvent::CarrierChanged { ifindex, up } => {
                assert_eq!(*ifindex, 2);
                assert!(!up);
            }
            other => panic!("expected CarrierChanged, got {other:?}"),
        }
    }

    #[test]
    fn dellink_emits_interface_removed_only_when_registered() {
        let (tx, mut rx) = broadcast::channel(16);
        let mut registry = Registry::new();

        apply_rtnl_dellink(&mut registry, &tx, 99);
        assert!(drain_events(&mut rx).is_empty());

        seed_eth0(&mut registry);
        apply_rtnl_dellink(&mut registry, &tx, 2);
        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            NexusEvent::InterfaceRemoved { ifindex: 2 },
        ));
    }

    #[test]
    fn replaying_recorded_dump_emits_interface_discovered_per_link() {
        let (tx, mut rx) = broadcast::channel(32);
        let mut registry = Registry::new();
        let wireless = HashMap::new();

        let mut buf = Vec::new();
        buf.extend_from_slice(&build_newlink(2, "eth0", true));
        buf.extend_from_slice(&build_newlink(3, "eth1", true));
        buf.extend_from_slice(&build_newlink(4, "eth2", true));
        let done = NetlinkMessageHeader {
            length: NLMSG_HDRLEN as u32,
            msg_type: NLMSG_DONE,
            flags: NLM_F_MULTI,
            seq: 0,
            pid: 0,
        };
        buf.extend_from_slice(&done.to_bytes());

        process_rtnl_datagram(&mut registry, &tx, &wireless, &buf).unwrap();

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 3);
        let names: Vec<String> = events
            .iter()
            .filter_map(|e| match e {
                NexusEvent::InterfaceDiscovered(info) => Some(info.ifname.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["eth0", "eth1", "eth2"]);
    }

    #[test]
    fn wireless_classification_uses_cold_boot_map() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut registry = Registry::new();
        let mut wireless = HashMap::new();
        wireless.insert(
            5,
            Nl80211InterfaceInfo {
                ifindex: 5,
                ifname: Some("wlan0".into()),
                wiphy: 0,
                wdev: 1,
                iftype: 2,
            },
        );

        let raw = build_newlink(5, "wlan0", true);
        let (msg, _) = crate::netlink::parser::parse_message(&raw).unwrap();
        let link = parse_link_message(msg.payload).unwrap();
        apply_rtnl_newlink(&mut registry, &tx, &wireless, &link);

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            NexusEvent::InterfaceDiscovered(info) => {
                assert!(matches!(info.kind, InterfaceKind::Wireless { .. }));
            }
            other => panic!("expected wireless InterfaceDiscovered, got {other:?}"),
        }
    }

    #[test]
    fn bluetooth_add_then_remove_emits_paired_events() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut registry = Registry::new();
        let adapter = BluetoothAdapter {
            hci_name: "hci0".into(),
            hci_index: 0,
            bt_address: Some([0xDE; 6]),
            bluez_path: "/org/bluez/hci0".into(),
        };
        apply_bluetooth_add(&mut registry, &tx, adapter);
        apply_bluetooth_remove(&mut registry, &tx, 0);

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            &events[0],
            NexusEvent::InterfaceDiscovered(info) if matches!(info.kind, InterfaceKind::Bluetooth { .. }),
        ));
        assert!(matches!(events[1], NexusEvent::InterfaceRemoved { .. }));
    }
}
