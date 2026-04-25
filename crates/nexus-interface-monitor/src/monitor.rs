//! Main Interface Monitor task: cold-boot enumeration followed by
//! the `tokio::select!` loop that multiplexes rtnetlink, nl80211
//! multicast, nl80211 unicast probe responses, udev monitor events,
//! classify timeouts, the nl80211 retry timer, and the shutdown
//! token. See DD-001 §§7, 8, 9.1, 9.4, and Phase 6-8.

use std::collections::HashMap;
use std::future::pending;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, OperState, PhyCapabilities};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::MonitorError;
use crate::classify::{ClassifyOutcome, ClassifyTracker};
use crate::command::MonitorCommand;
use crate::enumerate::{
    ColdBoot, Nl80211InterfaceInfo, classify_link, cold_boot_enumerate, dump_nl80211_interfaces,
    dump_nl80211_wiphys, parse_nl80211_interface_attrs, resolve_nl80211,
};
use crate::metrics as m;
use crate::netlink::genl::{FamilyInfo, GenlHeader, parse_genl_header};
use crate::netlink::nl80211::{
    NL80211_CMD_GET_INTERFACE, NL80211_GENL_VERSION, NL80211_MCAST_GROUP_CONFIG,
    NL80211_MCAST_GROUP_MLME, NL80211_MCAST_GROUP_SCAN,
};
use crate::netlink::parser::{
    MessageIter, NLM_F_ACK, NLM_F_REQUEST, NLMSG_DONE, NLMSG_ERROR, NLMSG_HDRLEN,
    NetlinkMessageHeader, encode_attribute, finalize_message_length, parse_nlmsgerr,
};
use crate::netlink::rtnl::{
    ARPHRD_ETHER, IFF_UP, IFINFOMSG_SIZE, IfInfoHeader, LinkMessage, RTM_DELLINK, RTM_NEWLINK,
    RTMGRP_LINK, parse_link_message,
};
use crate::netlink::socket::{NETLINK_GENERIC, NETLINK_ROUTE, NetlinkSocket};
use crate::recover::{Nl80211RetryTimer, compute_registry_diff, is_enobufs};
use crate::registry::{Registry, bt_ifindex};
use crate::udev::{BluetoothAdapter, GnssDevice, UdevAction, UdevMonitorHandle};

// `GenlHeader` lives in `netlink::genl`, not `netlink::parser`. Re-export
// correctly from the parser prelude path used above.
// (The compile error if we imported the wrong path catches this.)

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
    pub wireless_by_ifindex: HashMap<u32, Nl80211InterfaceInfo>,
    pub wiphy_caps: HashMap<u32, Arc<PhyCapabilities>>,
}

impl MonitorTask {
    pub async fn bootstrap(event_tx: broadcast::Sender<NexusEvent>) -> Result<Self, MonitorError> {
        let rtnl = NetlinkSocket::open(NETLINK_ROUTE, RTMGRP_LINK)?;
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
            join_nl80211_mcast_groups(family, mcast);
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
            wiphy_caps: HashMap::new(),
        })
    }

    pub async fn run(
        mut self,
        shutdown: CancellationToken,
        mut commands: mpsc::Receiver<MonitorCommand>,
    ) -> Result<(), MonitorError> {
        let cold_boot_started = Instant::now();

        let outcome = cold_boot_enumerate(
            ColdBoot {
                rtnl: &self.rtnl,
                nl80211_rr: self.nl80211_rr.as_ref(),
                nl80211_family: self.nl80211_family.as_ref(),
            },
            &mut self.registry,
        )
        .await?;

        self.wireless_by_ifindex = outcome.wireless_by_ifindex.clone();
        self.wiphy_caps = outcome.wiphy_caps.clone();

        m::record_discovery_duration(cold_boot_started.elapsed().as_secs_f64());

        for info in &outcome.discovered {
            m::record_event_for(info, m::event_label::INTERFACE_DISCOVERED);
            send_event(
                &self.event_tx,
                NexusEvent::InterfaceDiscovered(info.clone()),
            );
        }
        m::refresh_interface_counts(self.registry.iter());

        // Drain hotplug events that arrived during the rtnl dump (§5.5).
        for link in outcome.pending_newlink {
            apply_rtnl_newlink(
                &mut self.registry,
                &self.event_tx,
                &self.wireless_by_ifindex,
                &self.wiphy_caps,
                &link,
            );
        }
        for ifindex in outcome.pending_dellink {
            apply_rtnl_dellink(&mut self.registry, &self.event_tx, ifindex);
        }
        m::refresh_interface_counts(self.registry.iter());

        // Destructure into the fields we need so the select!'s futures
        // can hold disjoint borrows.
        let Self {
            mut registry,
            event_tx,
            rtnl,
            nl80211_rr,
            nl80211_mcast,
            mut nl80211_family,
            mut udev,
            mut wireless_by_ifindex,
            mut wiphy_caps,
            ..
        } = self;

        let mut classify = ClassifyTracker::new();
        let mut nl80211_retry = Nl80211RetryTimer::default();
        if nl80211_family.is_none() {
            nl80211_retry.arm(Instant::now());
        }

        let mut rtnl_buf = vec![0u8; RECV_BUFFER];
        let mut mcast_buf = vec![0u8; RECV_BUFFER];
        let mut rr_buf = vec![0u8; RECV_BUFFER];

        loop {
            // Select sleep deadline = min(next classify timeout, next
            // nl80211 retry). If neither is set we pend forever.
            let sleep_deadline = match (classify.next_deadline(), nl80211_retry.next_deadline()) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (Some(d), None) | (None, Some(d)) => Some(d),
                (None, None) => None,
            };

            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::info!("interface monitor shutting down");
                    return Ok(());
                }
                res = rtnl.recv(&mut rtnl_buf) => {
                    let n = match res {
                        Ok(n) => n,
                        Err(e) if is_enobufs(&e) => {
                            tracing::warn!("rtnl ENOBUFS — re-enumerating");
                            m::record_error(m::error_source::RTNL_ENOBUFS);
                            reenumerate(
                                &rtnl,
                                nl80211_rr.as_ref(),
                                nl80211_family.as_ref(),
                                &mut registry,
                                &mut wireless_by_ifindex,
                                &mut wiphy_caps,
                                &event_tx,
                            ).await?;
                            m::refresh_interface_counts(registry.iter());
                            continue;
                        }
                        Err(e) => {
                            m::record_error(m::error_source::RTNL_RECV);
                            tracing::warn!(error = %e, "rtnl recv failed; retrying");
                            continue;
                        }
                    };
                    if let Err(e) = process_rtnl_datagram(
                        &mut registry,
                        &event_tx,
                        &wireless_by_ifindex,
                        &wiphy_caps,
                        &mut classify,
                        nl80211_rr.as_ref(),
                        nl80211_family.as_ref(),
                        &rtnl_buf[..n],
                    ).await {
                        tracing::warn!(error = %e, "rtnl parse error; continuing");
                        m::record_error(m::error_source::RTNL_PARSE);
                    }
                    m::refresh_interface_counts(registry.iter());
                }
                res = optional_recv(nl80211_mcast.as_ref(), &mut mcast_buf) => {
                    match res {
                        Ok(n) => {
                            process_nl80211_mcast_datagram(
                                &mut registry,
                                &event_tx,
                                &mut wireless_by_ifindex,
                                &wiphy_caps,
                                &mut classify,
                                nl80211_family.as_ref(),
                                &mcast_buf[..n],
                            );
                        }
                        Err(e) if is_enobufs(&e) => {
                            m::record_error(m::error_source::NL80211_ENOBUFS);
                            tracing::warn!("nl80211 mcast ENOBUFS");
                        }
                        Err(e) => {
                            m::record_error(m::error_source::NL80211_MCAST_RECV);
                            tracing::warn!(error = %e, "nl80211 mcast recv failed");
                        }
                    }
                }
                res = optional_recv(nl80211_rr.as_ref(), &mut rr_buf) => {
                    match res {
                        Ok(n) => {
                            process_nl80211_rr_datagram(
                                &mut registry,
                                &event_tx,
                                &mut wireless_by_ifindex,
                                &wiphy_caps,
                                &mut classify,
                                nl80211_family.as_ref(),
                                &rr_buf[..n],
                            );
                        }
                        Err(e) => {
                            m::record_error(m::error_source::NL80211_PARSE);
                            tracing::warn!(error = %e, "nl80211 rr recv failed");
                        }
                    }
                }
                Some(action) = optional_udev(udev.as_mut()) => {
                    handle_udev_action(&mut registry, &event_tx, action);
                    m::refresh_interface_counts(registry.iter());
                }
                _ = optional_sleep(sleep_deadline) => {
                    let now = Instant::now();
                    // Classify timeouts.
                    for outcome in classify.sweep_timeouts(now) {
                        m::record_error(m::error_source::CLASSIFY_TIMEOUT);
                        apply_classify_outcome(
                            &mut registry,
                            &event_tx,
                            &wiphy_caps,
                            outcome,
                        );
                    }
                    // nl80211 retry.
                    if nl80211_retry.poll(now) {
                        match try_recover_nl80211(
                            nl80211_rr.as_ref(),
                            nl80211_mcast.as_ref(),
                        ).await {
                            Some(family) => {
                                tracing::info!("nl80211 resolved after retry");
                                nl80211_family = Some(family);
                                nl80211_retry.disable();
                                // Reclassify existing Ethernet-labeled
                                // interfaces that now turn out to be
                                // Wireless, per DD-001 §9.4 step 3.
                                if let (Some(rr), Some(family)) =
                                    (nl80211_rr.as_ref(), nl80211_family.as_ref())
                                {
                                    wireless_by_ifindex =
                                        dump_nl80211_interfaces(rr, family.id)
                                            .await
                                            .unwrap_or_default();
                                    wiphy_caps = dump_nl80211_wiphys(rr, family.id)
                                        .await
                                        .unwrap_or_default();
                                    reclassify_existing_as_wireless(
                                        &mut registry,
                                        &event_tx,
                                        &wireless_by_ifindex,
                                        &wiphy_caps,
                                    );
                                    m::refresh_interface_counts(registry.iter());
                                }
                            }
                            None => {
                                tracing::debug!("nl80211 still unavailable; will retry");
                            }
                        }
                    }
                    m::refresh_interface_counts(registry.iter());
                }
                Some(cmd) = commands.recv() => {
                    handle_monitor_command(&rtnl, cmd).await;
                }
            }
        }
    }
}

/// Dispatch a [`MonitorCommand`]. Lives here rather than on
/// [`MonitorTask`] because by the time the `select!` loop runs the
/// task has been destructured into disjoint borrows.
async fn handle_monitor_command(rtnl: &NetlinkSocket, cmd: MonitorCommand) {
    match cmd {
        MonitorCommand::SetAdminUp {
            ifindex,
            up,
            reply,
        } => {
            let result = set_admin_up(rtnl, ifindex, up).await;
            if let Err(ref e) = result {
                tracing::warn!(ifindex, up, error = %e, "SetAdminUp failed");
            } else {
                tracing::debug!(ifindex, up, "SetAdminUp dispatched");
            }
            if let Some(tx) = reply {
                let _ = tx.send(result);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// nl80211 mcast helpers.
// ---------------------------------------------------------------------------

fn join_nl80211_mcast_groups(family: &FamilyInfo, mcast: &NetlinkSocket) {
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

async fn try_recover_nl80211(
    rr: Option<&NetlinkSocket>,
    mcast: Option<&NetlinkSocket>,
) -> Option<FamilyInfo> {
    let rr = rr?;
    match resolve_nl80211(rr).await.ok().flatten() {
        Some(family) => {
            if let Some(mcast) = mcast {
                join_nl80211_mcast_groups(&family, mcast);
            }
            Some(family)
        }
        None => None,
    }
}

// ---------------------------------------------------------------------------
// rtnl handlers
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn process_rtnl_datagram(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    wireless: &HashMap<u32, Nl80211InterfaceInfo>,
    wiphy_caps: &HashMap<u32, Arc<PhyCapabilities>>,
    classify: &mut ClassifyTracker,
    nl80211_rr: Option<&NetlinkSocket>,
    nl80211_family: Option<&FamilyInfo>,
    buf: &[u8],
) -> Result<(), MonitorError> {
    for msg_result in MessageIter::new(buf) {
        let msg = msg_result?;
        match msg.header.msg_type {
            NLMSG_DONE | NLMSG_ERROR => {}
            RTM_NEWLINK => {
                let link = parse_link_message(msg.payload)?;
                let ifindex = link.header.index as u32;
                // Known interface? Diff and emit update events.
                if registry.contains(ifindex) {
                    apply_rtnl_newlink(registry, event_tx, wireless, wiphy_caps, &link);
                    continue;
                }
                // Unknown + ARPHRD_ETHER + not a virtual device →
                // enter `Classifying` and issue a unicast probe.
                if link.header.ifi_type != ARPHRD_ETHER || link.is_virtual_kind() {
                    continue;
                }
                let probe_seq = match (nl80211_rr, nl80211_family) {
                    (Some(rr), Some(family)) => {
                        Some(send_classify_probe(rr, family.id, ifindex).await)
                    }
                    _ => None,
                };
                classify.start(link, probe_seq, Instant::now());
            }
            RTM_DELLINK => {
                let link = parse_link_message(msg.payload)?;
                let ifindex = link.header.index as u32;
                classify.cancel(ifindex);
                apply_rtnl_dellink(registry, event_tx, ifindex);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Send an `NL80211_CMD_GET_INTERFACE` probe for a single ifindex.
/// Returns the request's seq so the tracker can correlate responses.
async fn send_classify_probe(socket: &NetlinkSocket, family_id: u16, ifindex: u32) -> u32 {
    let seq = socket.next_seq();
    let mut buf = Vec::with_capacity(NLMSG_HDRLEN + 4 + 8);
    let hdr = NetlinkMessageHeader {
        length: 0,
        msg_type: family_id,
        flags: NLM_F_REQUEST,
        seq,
        pid: socket.port_id(),
    };
    buf.extend_from_slice(&hdr.to_bytes());
    buf.extend_from_slice(
        &GenlHeader {
            cmd: NL80211_CMD_GET_INTERFACE,
            version: NL80211_GENL_VERSION,
        }
        .to_bytes(),
    );
    encode_attribute(
        &mut buf,
        crate::netlink::nl80211::NL80211_ATTR_IFINDEX,
        &ifindex.to_ne_bytes(),
    );
    finalize_message_length(&mut buf);

    if let Err(e) = socket.send(&buf).await {
        tracing::warn!(ifindex, error = %e, "classify probe send failed; will time out");
    }
    seq
}

fn apply_rtnl_newlink(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    wireless: &HashMap<u32, Nl80211InterfaceInfo>,
    wiphy_caps: &HashMap<u32, Arc<PhyCapabilities>>,
    link: &LinkMessage,
) {
    let ifindex = link.header.index as u32;
    let wireless_info = wireless.get(&ifindex);
    let caps = wireless_info.and_then(|w| wiphy_caps.get(&w.wiphy).cloned());
    let classified = match classify_link(link, wireless_info, caps) {
        Some(info) => info,
        None => return,
    };

    match registry.get(ifindex).cloned() {
        None => {
            m::record_event_for(&classified, m::event_label::INTERFACE_DISCOVERED);
            registry.insert(classified.clone());
            send_event(event_tx, NexusEvent::InterfaceDiscovered(classified));
        }
        Some(existing) if existing.ifname != classified.ifname => {
            // Kernel rename (`RTM_NEWLINK` with same ifindex but new
            // `IFLA_IFNAME`). Downstream surfaces are keyed by ifname
            // — D-Bus paths (`/fi/nexus1/interface/<ifname>`,
            // DD-006 §6), per-interface profiles
            // (`/var/lib/nexus/ethernet/<ifname>.toml`, DD-002 §8.2),
            // metric labels — so we model the rename as a logical
            // re-discovery: emit `InterfaceRemoved` followed by
            // `InterfaceDiscovered`. Each consumer drops its
            // by-ifname caches and re-establishes against the new
            // identifier. systemd's predictable-name renamer is the
            // common trigger; in production this typically fires
            // before nexusd starts, but `ip link set name …` and
            // USB-Ethernet replug can hit the same path.
            m::record_event_for(&existing, m::event_label::INTERFACE_REMOVED);
            registry.remove(ifindex);
            send_event(event_tx, NexusEvent::InterfaceRemoved { ifindex });
            m::record_event_for(&classified, m::event_label::INTERFACE_DISCOVERED);
            registry.insert(classified.clone());
            send_event(event_tx, NexusEvent::InterfaceDiscovered(classified));
        }
        Some(existing) => {
            let mut next = existing.clone();
            if existing.carrier != classified.carrier {
                next.carrier = classified.carrier;
                m::record_event(
                    m::kind_label(&existing.kind),
                    m::event_label::CARRIER_CHANGED,
                );
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
                m::record_event(
                    m::kind_label(&existing.kind),
                    m::event_label::OPERSTATE_CHANGED,
                );
                send_event(
                    event_tx,
                    NexusEvent::OperstateChanged {
                        ifindex,
                        state: classified.operstate,
                    },
                );
            }
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
    if let Some(existing) = registry.remove(ifindex) {
        m::record_event_for(&existing, m::event_label::INTERFACE_REMOVED);
        send_event(event_tx, NexusEvent::InterfaceRemoved { ifindex });
    }
}

// ---------------------------------------------------------------------------
// nl80211 datagram handling (mcast + unicast probe responses).
// ---------------------------------------------------------------------------

fn process_nl80211_mcast_datagram(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    wireless: &mut HashMap<u32, Nl80211InterfaceInfo>,
    wiphy_caps: &HashMap<u32, Arc<PhyCapabilities>>,
    classify: &mut ClassifyTracker,
    family: Option<&FamilyInfo>,
    buf: &[u8],
) {
    let family_id = match family {
        Some(f) => f.id,
        None => return,
    };
    for msg_result in MessageIter::new(buf) {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(error = %e, "nl80211 mcast parse error");
                m::record_error(m::error_source::NL80211_PARSE);
                return;
            }
        };
        if msg.header.msg_type != family_id {
            continue;
        }
        let (genl, attrs) = match parse_genl_header(msg.payload) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // We only look at NEW_INTERFACE (cmd 7) here — see DD-001 §§5.2, 7.
        if genl.cmd != 7 {
            continue;
        }
        let info = match parse_nl80211_interface_attrs(attrs) {
            Ok(Some(i)) => i,
            _ => continue,
        };
        wireless.insert(info.ifindex, info.clone());
        // Did this multicast resolve a pending classification?
        if let Some(outcome) = classify.resolve_via_multicast(info.clone()) {
            apply_classify_outcome(registry, event_tx, wiphy_caps, outcome);
            continue;
        }
        // Late-wireless-registration: the interface was already
        // registered as Ethernet. Two-step transition per §7.4.
        if let Some(existing) = registry.get(info.ifindex).cloned() {
            if matches!(existing.kind, InterfaceKind::Ethernet) {
                apply_rtnl_dellink(registry, event_tx, info.ifindex);
                let link = synthetic_link_from(&existing);
                let wireless_map: HashMap<u32, Nl80211InterfaceInfo> =
                    HashMap::from([(info.ifindex, info)]);
                apply_rtnl_newlink(registry, event_tx, &wireless_map, wiphy_caps, &link);
            }
        }
    }
}

fn process_nl80211_rr_datagram(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    wireless: &mut HashMap<u32, Nl80211InterfaceInfo>,
    wiphy_caps: &HashMap<u32, Arc<PhyCapabilities>>,
    classify: &mut ClassifyTracker,
    family: Option<&FamilyInfo>,
    buf: &[u8],
) {
    let family_id = match family {
        Some(f) => f.id,
        None => return,
    };
    for msg_result in MessageIter::new(buf) {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                m::record_error(m::error_source::NL80211_PARSE);
                tracing::warn!(error = %e, "nl80211 rr parse error");
                return;
            }
        };
        let seq = msg.header.seq;
        match msg.header.msg_type {
            NLMSG_ERROR => {
                if let Ok(err) = parse_nlmsgerr(msg.payload) {
                    if let Some(outcome) = classify.resolve_probe_error(seq, err.error) {
                        apply_classify_outcome(registry, event_tx, wiphy_caps, outcome);
                    }
                }
            }
            t if t == family_id => {
                let (_genl, attrs) = match parse_genl_header(msg.payload) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Ok(Some(info)) = parse_nl80211_interface_attrs(attrs) {
                    wireless.insert(info.ifindex, info.clone());
                    if let Some(outcome) = classify.resolve_probe_success(seq, info) {
                        apply_classify_outcome(registry, event_tx, wiphy_caps, outcome);
                    }
                }
            }
            _ => {}
        }
    }
}

fn apply_classify_outcome(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    wiphy_caps: &HashMap<u32, Arc<PhyCapabilities>>,
    outcome: ClassifyOutcome,
) {
    match outcome {
        ClassifyOutcome::Ethernet(link) => {
            let empty = HashMap::new();
            apply_rtnl_newlink(registry, event_tx, &empty, wiphy_caps, &link);
        }
        ClassifyOutcome::Wireless { link, nl80211 } => {
            let ifindex = link.header.index as u32;
            let wireless_map: HashMap<u32, Nl80211InterfaceInfo> =
                HashMap::from([(ifindex, nl80211)]);
            apply_rtnl_newlink(registry, event_tx, &wireless_map, wiphy_caps, &link);
        }
    }
}

/// Reconstruct a minimal `LinkMessage` from a registered
/// `InterfaceInfo`. Used for the late-wireless-registration two-step:
/// we need to re-emit a Discovered event through the normal rtnl
/// path so diff/classify behavior is consistent.
fn synthetic_link_from(info: &InterfaceInfo) -> LinkMessage {
    use crate::netlink::rtnl::{ARPHRD_ETHER, IfInfoHeader};
    LinkMessage {
        header: IfInfoHeader {
            family: 0,
            ifi_type: ARPHRD_ETHER,
            index: info.ifindex as i32,
            flags: 0,
            change: 0xFFFF_FFFF,
        },
        ifname: Some(info.ifname.clone()),
        mac: Some(info.mac),
        mtu: Some(info.mtu),
        operstate: Some(match info.operstate {
            OperState::Up => crate::netlink::rtnl::IF_OPER_UP,
            OperState::Dormant => crate::netlink::rtnl::IF_OPER_DORMANT,
            OperState::Down => crate::netlink::rtnl::IF_OPER_DOWN,
            OperState::LowerLayerDown => crate::netlink::rtnl::IF_OPER_LOWERLAYERDOWN,
            OperState::Testing => crate::netlink::rtnl::IF_OPER_TESTING,
            OperState::NotPresent => crate::netlink::rtnl::IF_OPER_NOTPRESENT,
            OperState::Unknown => crate::netlink::rtnl::IF_OPER_UNKNOWN,
        }),
        carrier: Some(info.carrier),
        info_kind: None,
        link: None,
        phys_port_name: None,
    }
}

fn reclassify_existing_as_wireless(
    registry: &mut Registry,
    event_tx: &broadcast::Sender<NexusEvent>,
    wireless: &HashMap<u32, Nl80211InterfaceInfo>,
    wiphy_caps: &HashMap<u32, Arc<PhyCapabilities>>,
) {
    for (ifindex, info) in wireless.iter() {
        if let Some(existing) = registry.get(*ifindex).cloned() {
            if matches!(existing.kind, InterfaceKind::Ethernet) {
                apply_rtnl_dellink(registry, event_tx, *ifindex);
                let link = synthetic_link_from(&existing);
                let wireless_map: HashMap<u32, Nl80211InterfaceInfo> =
                    HashMap::from([(*ifindex, info.clone())]);
                apply_rtnl_newlink(registry, event_tx, &wireless_map, wiphy_caps, &link);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Re-enumeration on ENOBUFS (§9.1).
// ---------------------------------------------------------------------------

async fn reenumerate(
    rtnl: &NetlinkSocket,
    nl80211_rr: Option<&NetlinkSocket>,
    nl80211_family: Option<&FamilyInfo>,
    registry: &mut Registry,
    wireless_by_ifindex: &mut HashMap<u32, Nl80211InterfaceInfo>,
    wiphy_caps: &mut HashMap<u32, Arc<PhyCapabilities>>,
    event_tx: &broadcast::Sender<NexusEvent>,
) -> Result<(), MonitorError> {
    let before: HashMap<u32, InterfaceInfo> =
        registry.iter().map(|i| (i.ifindex, i.clone())).collect();

    // Fresh dumps (preserve the existing registry until we've built
    // the new one, so diff can see both sides).
    let mut scratch = Registry::new();
    // Preserve the GNSS ifindex counter so we don't reuse IDs.
    for _ in 0..registry
        .iter()
        .filter(|info| crate::registry::is_gnss_ifindex(info.ifindex))
        .count()
    {
        scratch.allocate_gnss_ifindex();
    }

    let outcome = cold_boot_enumerate(
        ColdBoot {
            rtnl,
            nl80211_rr,
            nl80211_family,
        },
        &mut scratch,
    )
    .await?;

    *wireless_by_ifindex = outcome.wireless_by_ifindex;
    *wiphy_caps = outcome.wiphy_caps;
    // Adopt the new registry.
    *registry = scratch;

    let after: HashMap<u32, InterfaceInfo> =
        registry.iter().map(|i| (i.ifindex, i.clone())).collect();

    for diff in compute_registry_diff(&before, &after) {
        let event = diff.into_nexus_event();
        match &event {
            NexusEvent::InterfaceDiscovered(info) => {
                m::record_event_for(info, m::event_label::INTERFACE_DISCOVERED);
            }
            NexusEvent::InterfaceRemoved { .. } => {
                m::record_event("unknown", m::event_label::INTERFACE_REMOVED);
            }
            NexusEvent::CarrierChanged { .. } => {
                m::record_event("unknown", m::event_label::CARRIER_CHANGED);
            }
            NexusEvent::OperstateChanged { .. } => {
                m::record_event("unknown", m::event_label::OPERSTATE_CHANGED);
            }
            _ => {}
        }
        send_event(event_tx, event);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// udev hotplug handling.
// ---------------------------------------------------------------------------

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
    m::record_event_for(&info, m::event_label::INTERFACE_DISCOVERED);
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
    m::record_event_for(&info, m::event_label::INTERFACE_DISCOVERED);
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
// Select-branch future helpers.
// ---------------------------------------------------------------------------

async fn optional_recv(socket: Option<&NetlinkSocket>, buf: &mut [u8]) -> io::Result<usize> {
    match socket {
        Some(s) => s.recv(buf).await,
        None => pending().await,
    }
}

/// Build and send an `RTM_NEWLINK` that flips `IFF_UP` on `ifindex`.
/// `ifi_change = IFF_UP` so the kernel only touches that one flag;
/// every other flag on the interface is preserved. Matches what
/// `ip link set dev X up/down` emits on the wire. DD-003 §12.4.
async fn set_admin_up(
    rtnl: &NetlinkSocket,
    ifindex: u32,
    up: bool,
) -> Result<(), String> {
    let flags = if up { IFF_UP } else { 0 };
    let ifi = IfInfoHeader {
        family: libc::AF_UNSPEC as u8,
        ifi_type: 0,
        index: ifindex as i32,
        flags,
        change: IFF_UP,
    };
    let hdr = rtnl.fresh_header(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK);
    let mut buf = Vec::with_capacity(NLMSG_HDRLEN + IFINFOMSG_SIZE);
    buf.extend_from_slice(&hdr.to_bytes());
    buf.extend_from_slice(&ifi.to_bytes());
    finalize_message_length(&mut buf);
    rtnl.send(&buf).await.map_err(|e| e.to_string())?;
    // Fire-and-forget for now — the next RTM_NEWLINK multicast that
    // lands in the regular rtnl loop is the behavioural confirmation
    // Nexus cares about. Draining the ACK here without coordinating
    // with the main recv loop would race that loop for the reply.
    Ok(())
}

async fn optional_udev(udev: Option<&mut UdevMonitorHandle>) -> Option<UdevAction> {
    match udev {
        Some(u) => u.next_action().await,
        None => pending().await,
    }
}

async fn optional_sleep(deadline: Option<Instant>) {
    match deadline {
        Some(t) => {
            let now = Instant::now();
            let sleep = t
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            tokio::time::sleep(sleep).await;
        }
        None => pending().await,
    }
}

/// broadcast-send helper. A channel with zero receivers returns `Err`,
/// which per DD-001's event-bus rule is non-fatal — the monitor's job
/// is to emit, not to ensure delivery.
pub(crate) fn send_event(tx: &broadcast::Sender<NexusEvent>, event: NexusEvent) {
    if let Err(e) = tx.send(event) {
        tracing::trace!(error = %e, "no active receivers for NexusEvent");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Instant;

    use nexus_core::{InterfaceInfo, InterfaceKind, OperState};
    use tokio::sync::broadcast;

    use super::*;
    use crate::netlink::parser::{
        NLM_F_MULTI, NetlinkMessageHeader, encode_attribute, finalize_message_length,
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
        let caps = HashMap::new();

        let raw = build_newlink(2, "eth0", true);
        let (msg, _) = crate::netlink::parser::parse_message(&raw).unwrap();
        let link = parse_link_message(msg.payload).unwrap();
        apply_rtnl_newlink(&mut registry, &tx, &wireless, &caps, &link);

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        match &events[0] {
            NexusEvent::InterfaceDiscovered(info) => {
                assert_eq!(info.ifname, "eth0");
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
        let caps = HashMap::new();

        let raw = build_newlink(2, "eth0", false);
        let (msg, _) = crate::netlink::parser::parse_message(&raw).unwrap();
        let link = parse_link_message(msg.payload).unwrap();
        apply_rtnl_newlink(&mut registry, &tx, &wireless, &caps, &link);

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            NexusEvent::CarrierChanged {
                ifindex: 2,
                up: false,
            }
        ));
    }

    #[test]
    fn rtnl_newlink_with_renamed_ifname_emits_remove_then_discovered() {
        // DD-002 §15 / DD-006 §6: ifname is the path key downstream.
        // A kernel rename (RTM_NEWLINK with same ifindex but a new
        // IFLA_IFNAME) must surface as InterfaceRemoved +
        // InterfaceDiscovered so consumers re-establish their
        // by-ifname caches against the new identifier.
        let (tx, mut rx) = broadcast::channel(16);
        let mut registry = Registry::new();
        seed_eth0(&mut registry);
        let wireless = HashMap::new();
        let caps = HashMap::new();

        let raw = build_newlink(2, "enp1s0", true);
        let (msg, _) = crate::netlink::parser::parse_message(&raw).unwrap();
        let link = parse_link_message(msg.payload).unwrap();
        apply_rtnl_newlink(&mut registry, &tx, &wireless, &caps, &link);

        let events = drain_events(&mut rx);
        assert_eq!(
            events.len(),
            2,
            "expected exactly InterfaceRemoved + InterfaceDiscovered, got {events:?}",
        );
        assert!(matches!(
            events[0],
            NexusEvent::InterfaceRemoved { ifindex: 2 },
        ));
        match &events[1] {
            NexusEvent::InterfaceDiscovered(info) => {
                assert_eq!(info.ifindex, 2);
                assert_eq!(info.ifname, "enp1s0");
            }
            other => panic!("expected InterfaceDiscovered(enp1s0), got {other:?}"),
        }
        // Registry now reflects the new name.
        assert_eq!(registry.get(2).unwrap().ifname, "enp1s0");
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
    fn late_wireless_registration_fires_remove_then_discovered() {
        // Seed eth0 as Ethernet in the registry, then feed a
        // classification outcome that maps the same ifindex to
        // Wireless. The two-step transition should emit
        // InterfaceRemoved followed by InterfaceDiscovered(Wireless).
        let (tx, mut rx) = broadcast::channel(16);
        let mut registry = Registry::new();
        seed_eth0(&mut registry);

        // Build a synthetic rtnl datagram for the two-step:
        // apply_rtnl_dellink then apply_rtnl_newlink against a
        // wireless_map.
        apply_rtnl_dellink(&mut registry, &tx, 2);

        let raw = build_newlink(2, "eth0", true);
        let (msg, _) = crate::netlink::parser::parse_message(&raw).unwrap();
        let link = parse_link_message(msg.payload).unwrap();
        let wireless_map: HashMap<u32, Nl80211InterfaceInfo> = [(
            2u32,
            Nl80211InterfaceInfo {
                ifindex: 2,
                ifname: Some("eth0".into()),
                wiphy: 0,
                wdev: 1,
                iftype: 2,
            },
        )]
        .into_iter()
        .collect();
        let caps: HashMap<u32, Arc<PhyCapabilities>> = [(
            0u32,
            Arc::new(PhyCapabilities {
                wiphy: 0,
                wiphy_name: "phy0".into(),
                ..PhyCapabilities::default()
            }),
        )]
        .into_iter()
        .collect();
        apply_rtnl_newlink(&mut registry, &tx, &wireless_map, &caps, &link);

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], NexusEvent::InterfaceRemoved { .. }));
        match &events[1] {
            NexusEvent::InterfaceDiscovered(info) => {
                assert!(matches!(info.kind, InterfaceKind::Wireless { .. }));
            }
            other => panic!("expected wireless InterfaceDiscovered, got {other:?}"),
        }
    }
}
