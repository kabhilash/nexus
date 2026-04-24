//! Wi-Fi Backend event loop. See DD-003 §§3, 5, 6, 7, 12, 13.

use std::collections::HashMap;
use std::future::pending;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{DisconnectReason, InterfaceKind, NexusEvent, SecurityMode, WifiState};
use nexus_profile_store::{ProfileStore, WifiProfile};
use tokio::sync::{RwLock, broadcast};
use tokio_util::sync::CancellationToken;

use crate::error::{Result, WifiError};
use crate::lifecycle::{WifiInterfaceEntry, scans_suspended, state_label};
use crate::metrics as m;
use crate::power::PowerState;
use crate::profile::to_network_config;
use crate::retry::RetryBook;
use crate::rfkill::{RfkillState, RfkillWriter};
use crate::roam::{RoamPolicy, pick_roam_target};
use crate::scan::{BssCache, ScanScheduler};
use crate::select::select_network;
use crate::supplicant::{DisconnectHint, SupplicantEvent, SupplicantState, WifiSupplicantBackend};
use crate::types::{BssInfo, NetworkHandle, RoamMode, ScanParams};

/// Tunable settings the backend honors at runtime.
#[derive(Debug, Clone, Copy)]
pub struct WifiConfig {
    pub roam_mode: RoamMode,
    pub roam_policy: RoamPolicy,
    pub signal_poll_interval: Duration,
    pub disconnect_cool_down: Duration,
}

/// Interval of the backend's `select!`-loop heartbeat. Tight enough
/// that power-state transitions during otherwise-quiet periods
/// (Sleep → Active) observe the new cadence within ~1 s, loose
/// enough that the idle-daemon wakeup rate is negligible.
const HEARTBEAT: Duration = Duration::from_secs(1);

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            roam_mode: RoamMode::Supplicant,
            roam_policy: RoamPolicy::default(),
            signal_poll_interval: Duration::from_secs(5),
            disconnect_cool_down: Duration::from_secs(2),
        }
    }
}

/// The Wi-Fi Backend. Lives inside a spawned task; the caller
/// keeps the `JoinHandle<Result<()>>`.
pub struct WifiBackend {
    event_tx: broadcast::Sender<NexusEvent>,
    event_rx: broadcast::Receiver<NexusEvent>,
    supplicant_rx: broadcast::Receiver<SupplicantEvent>,
    /// Operator-driven commands from the D-Bus layer (see
    /// [`crate::WifiCommand`]). Drained on the same `select!` loop
    /// as event traffic.
    cmd_rx: tokio::sync::mpsc::Receiver<crate::WifiCommand>,

    supplicant: Box<dyn WifiSupplicantBackend>,
    profile_store: Arc<dyn ProfileStore>,
    profiles: Vec<WifiProfile>,

    interfaces: HashMap<u32, WifiInterfaceEntry>,
    schedulers: HashMap<u32, ScanScheduler>,
    cache: BssCache,
    retry: RetryBook,
    /// `active_handle[ifindex]` → (profile ULID, supplicant handle)
    /// per DD-003 §6.5.
    active_handle: HashMap<u32, (ulid::Ulid, NetworkHandle)>,

    power: Arc<RwLock<PowerState>>,
    config: WifiConfig,
    supplicant_up: bool,

    /// Timestamp of the last successful `SignalPoll` for each
    /// interface. Populated on the heartbeat tick so we only poll
    /// per `WifiConfig::signal_poll_interval`, not every tick.
    last_signal_poll: HashMap<u32, Instant>,

    /// `/dev/rfkill` edges from the watcher task (see
    /// [`crate::rfkill`]). `None` when rfkill couldn't be opened —
    /// tests bypass it and the daemon logs a warning.
    rfkill_rx: Option<tokio::sync::mpsc::Receiver<RfkillState>>,
    /// Write-side handle for the `fi.nexus.Wifi.Powered` setter.
    /// `None` mirrors the read-side; the `SetPowered` command
    /// returns `Unsupported` when the writer isn't wired.
    rfkill_writer: Option<RfkillWriter>,
}

impl WifiBackend {
    pub fn new(
        event_tx: broadcast::Sender<NexusEvent>,
        supplicant_tx: broadcast::Sender<SupplicantEvent>,
        supplicant: Box<dyn WifiSupplicantBackend>,
        profile_store: Arc<dyn ProfileStore>,
        config: WifiConfig,
        cmd_rx: tokio::sync::mpsc::Receiver<crate::WifiCommand>,
    ) -> Self {
        let event_rx = event_tx.subscribe();
        let supplicant_rx = supplicant_tx.subscribe();
        let backend_name = supplicant.name();
        m::set_supplicant_available(backend_name, true);
        Self {
            event_tx,
            event_rx,
            supplicant_rx,
            cmd_rx,
            supplicant,
            profile_store,
            profiles: Vec::new(),
            interfaces: HashMap::new(),
            schedulers: HashMap::new(),
            cache: BssCache::new(),
            retry: RetryBook::new(),
            active_handle: HashMap::new(),
            power: Arc::new(RwLock::new(PowerState::default())),
            config,
            supplicant_up: true,
            last_signal_poll: HashMap::new(),
            rfkill_rx: None,
            rfkill_writer: None,
        }
    }

    /// Attach the `/dev/rfkill` plumbing. Called from
    /// [`crate::spawn_wifi_backend`] after the rfkill watcher is
    /// spawned; tests that don't exercise Powered skip this.
    pub fn with_rfkill(
        mut self,
        rx: tokio::sync::mpsc::Receiver<RfkillState>,
        writer: RfkillWriter,
    ) -> Self {
        self.rfkill_rx = Some(rx);
        self.rfkill_writer = Some(writer);
        self
    }

    /// Handle the caller's `Arc<RwLock<PowerState>>` so external
    /// power-management code (e.g., D-Bus SetPowerState) can
    /// mutate it without a dedicated channel.
    pub fn power_handle(&self) -> Arc<RwLock<PowerState>> {
        Arc::clone(&self.power)
    }

    /// Drive the loop. Returns on `shutdown` cancellation or when
    /// the event bus closes.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        // Load profiles once at startup. Subsequent put/remove calls
        // emit `NexusEvent::ProfileChanged`, which `on_nexus_event`
        // handles with a refresh.
        self.profiles = self.profile_store.load_wifi().await?;

        // 1 s heartbeat. Two jobs:
        //   - Run signal polls on connected interfaces whose
        //     per-interface `signal_poll_interval` deadline has
        //     arrived (DD-003 §7.2).
        //   - Guarantee the select! loop wakes even when the scan
        //     scheduler is suspended (e.g. PowerState::Sleep), so
        //     a subsequent power-state change picks up the new
        //     cadence on the very next iteration.
        let heartbeat = tokio::time::sleep(HEARTBEAT);
        tokio::pin!(heartbeat);

        loop {
            let power = *self.power.read().await;
            let next_scan = self.earliest_scan_deadline(power);

            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::info!("wifi backend shutting down");
                    return Ok(());
                }
                res = self.event_rx.recv() => match res {
                    Ok(event) => {
                        if let Err(e) = self.on_nexus_event(event).await {
                            tracing::warn!(error = %e, "wifi event handler error");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "wifi receiver lagged");
                    }
                },
                res = self.supplicant_rx.recv() => match res {
                    Ok(event) => {
                        if let Err(e) = self.on_supplicant_event(event).await {
                            tracing::warn!(error = %e, "supplicant event handler error");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "supplicant receiver lagged");
                    }
                },
                _ = sleep_until_option(next_scan) => {
                    if let Err(e) = self.fire_scheduled_scans().await {
                        tracing::warn!(error = %e, "scheduled scan error");
                    }
                }
                maybe_cmd = self.cmd_rx.recv() => match maybe_cmd {
                    Some(cmd) => {
                        self.on_command(cmd).await;
                    }
                    // All command senders dropped. That's not a
                    // lifecycle signal — the backend stays up until
                    // `shutdown` fires. Break the select! arm from
                    // retrying the same `None` forever by swapping
                    // in a sentinel channel that never yields.
                    None => {
                        let (_, rx) = tokio::sync::mpsc::channel(1);
                        self.cmd_rx = rx;
                    }
                },
                maybe_rf = recv_rfkill(&mut self.rfkill_rx) => {
                    if let Some(state) = maybe_rf {
                        self.on_rfkill(state).await;
                    }
                }
                _ = &mut heartbeat => {
                    heartbeat.as_mut().reset(tokio::time::Instant::now() + HEARTBEAT);
                    self.on_heartbeat().await;
                },
            }
            self.refresh_metrics();
        }
    }

    /// Map an rfkill edge from the watcher to a
    /// `NexusEvent::WifiRfkillChanged`. Events for wiphys that
    /// aren't in the interface registry (e.g. a USB dongle that
    /// the Interface Monitor hasn't enumerated yet) are dropped —
    /// the next event after the interface lands will reflect
    /// current state.
    async fn on_rfkill(&self, state: RfkillState) {
        let Some(ifindex) = self.ifindex_for_wiphy(&state.wiphy_name) else {
            tracing::debug!(
                wiphy = %state.wiphy_name,
                powered = state.powered,
                "rfkill edge for unknown wiphy; ignoring"
            );
            return;
        };
        let _ = self.event_tx.send(NexusEvent::WifiRfkillChanged {
            ifindex,
            powered: state.powered,
        });
    }

    fn ifindex_for_wiphy(&self, wiphy_name: &str) -> Option<u32> {
        self.interfaces.iter().find_map(|(ifindex, entry)| match &entry.info.kind {
            InterfaceKind::Wireless { wiphy_name: w, .. } if w == wiphy_name => Some(*ifindex),
            _ => None,
        })
    }

    /// Tick handler: for each connected interface, if the signal
    /// poll is due, call the supplicant and emit a
    /// [`NexusEvent::WifiSignalPoll`]. The dbus layer consumes
    /// those to refresh the `SignalDbm` / `Frequency` properties.
    async fn on_heartbeat(&mut self) {
        let now = Instant::now();
        let interval = self.config.signal_poll_interval;
        // Collect first; mutating self across the await would
        // require a split borrow.
        let due: Vec<u32> = self
            .interfaces
            .iter()
            .filter_map(|(ifindex, entry)| {
                if !matches!(entry.state, WifiState::Connected { .. }) {
                    return None;
                }
                let last = self.last_signal_poll.get(ifindex).copied();
                match last {
                    None => Some(*ifindex),
                    Some(t) if now.duration_since(t) >= interval => Some(*ifindex),
                    _ => None,
                }
            })
            .collect();
        for ifindex in due {
            match self.supplicant.signal_info(ifindex).await {
                Ok(info) => {
                    self.last_signal_poll.insert(ifindex, now);
                    let _ = self.event_tx.send(NexusEvent::WifiSignalPoll {
                        ifindex,
                        rssi: info.rssi_dbm,
                        frequency: info.frequency,
                    });
                }
                Err(e) => {
                    tracing::debug!(ifindex, error = %e, "signal_info failed");
                }
            }
        }
    }

    /// Dispatch a [`WifiCommand`] from the D-Bus layer.
    /// The reply `oneshot::Sender` is `_`-consumed when the receiver
    /// has already been dropped — that happens when the client
    /// cancelled mid-flight and we just carry on.
    async fn on_command(&mut self, cmd: crate::WifiCommand) {
        match cmd {
            crate::WifiCommand::Scan {
                ifname,
                params,
                reply,
            } => {
                let result = match self.ifindex_for(&ifname) {
                    Some(ifindex) => self.request_scan(ifindex, params).await,
                    None => Err(WifiError::NotAttached { ifindex: 0 }),
                };
                let _ = reply.send(result);
            }
            crate::WifiCommand::Connect {
                ifname,
                profile_id,
                reply,
            } => {
                let _ = reply.send(self.operator_connect(&ifname, profile_id).await);
            }
            crate::WifiCommand::Disconnect { ifname, reply } => {
                let _ = reply.send(self.operator_disconnect(&ifname).await);
            }
            crate::WifiCommand::Roam {
                ifname,
                bssid,
                reply,
            } => {
                let _ = reply.send(self.operator_roam(&ifname, bssid).await);
            }
            crate::WifiCommand::SetRoamingMode {
                ifname,
                mode,
                reply,
            } => {
                let _ = reply.send(self.operator_set_roaming_mode(&ifname, mode).await);
            }
            crate::WifiCommand::SetPowered { ifname, on, reply } => {
                let _ = reply.send(self.operator_set_powered(&ifname, on).await);
            }
        }
    }

    /// Toggle soft-rfkill for the named interface. Resolves the
    /// ifname to its wiphy via the local registry, then hands off
    /// to [`RfkillWriter::set_blocked`]. Returns `NotAttached` if
    /// the interface isn't registered and `Supplicant` (as a
    /// catch-all for rfkill plumbing errors) on write failure.
    async fn operator_set_powered(&mut self, ifname: &str, on: bool) -> Result<()> {
        let ifindex = self
            .ifindex_for(ifname)
            .ok_or(WifiError::NotAttached { ifindex: 0 })?;
        let wiphy_name = self
            .interfaces
            .get(&ifindex)
            .and_then(|e| match &e.info.kind {
                InterfaceKind::Wireless { wiphy_name, .. } => Some(wiphy_name.clone()),
                _ => None,
            })
            .ok_or(WifiError::NotAttached { ifindex })?;
        let writer = self
            .rfkill_writer
            .as_ref()
            .ok_or_else(|| WifiError::Rfkill {
                ifindex,
                detail: "rfkill writer not available".to_owned(),
            })?;
        writer
            .set_blocked(&wiphy_name, !on)
            .await
            .map_err(|e| WifiError::Rfkill {
                ifindex,
                detail: format!("rfkill write: {e}"),
            })?;
        Ok(())
    }

    /// Operator-initiated `Connect`. Looks up the profile in the
    /// backend's in-memory cache, forgets any prior active handle
    /// on the interface, and drives the supplicant's connect flow.
    /// Does not consult the `retry` book — an explicit operator
    /// action isn't subject to the automatic-selection rate limit.
    async fn operator_connect(&mut self, ifname: &str, profile_id: ulid::Ulid) -> Result<()> {
        let ifindex = self
            .ifindex_for(ifname)
            .ok_or(WifiError::NotAttached { ifindex: 0 })?;
        let profile = self
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .cloned()
            .ok_or_else(|| WifiError::ProfileNotFound {
                id: profile_id.to_string(),
            })?;

        // Forget the previous handle *only if* it's for a different
        // profile. Same-profile re-connects (e.g. the operator
        // deliberately re-issuing Connect) keep the handle so the
        // supplicant can fast-path the association.
        if let Some((prev_profile, prev_handle)) = self.active_handle.remove(&ifindex) {
            if prev_profile != profile.id {
                let _ = self.supplicant.forget_network(ifindex, prev_handle).await;
            } else {
                self.active_handle
                    .insert(ifindex, (prev_profile, prev_handle));
            }
        }

        // Mark state `Connecting` immediately so the D-Bus property
        // reflects intent before the handshake completes. BSSID is
        // unknown at this point (the supplicant picks); zero it and
        // let `State = completed` resolve the final triple.
        if let Some(entry) = self.interfaces.get_mut(&ifindex) {
            entry.state = WifiState::Connecting {
                bssid: nexus_core::MacAddr([0; 6]),
                ssid: profile.network.ssid.clone(),
            };
            self.emit_state(ifindex);
        }

        let net = to_network_config(&profile);
        let handle = self.supplicant.connect(ifindex, &net).await?;
        self.active_handle.insert(ifindex, (profile.id, handle));
        m::record_connect(
            ifname,
            security_tag(&profile.network.security),
            m::connect_outcome::SUCCESS,
        );
        Ok(())
    }

    /// Operator-initiated `Disconnect`. The supplicant tears down
    /// the association; the in-memory active handle is cleared so a
    /// subsequent auto-select cycle can compete fresh.
    async fn operator_disconnect(&mut self, ifname: &str) -> Result<()> {
        let ifindex = self
            .ifindex_for(ifname)
            .ok_or(WifiError::NotAttached { ifindex: 0 })?;
        self.supplicant.disconnect(ifindex).await?;
        self.active_handle.remove(&ifindex);
        Ok(())
    }

    /// Operator-initiated `Roam` to a specific BSSID. Hands off to
    /// the supplicant regardless of roam_mode — wpa_supplicant
    /// rejects the call on its own side when the interface's roam
    /// config says no, and surfacing that as a plain error is more
    /// useful than us silently no-op'ing here.
    async fn operator_roam(&mut self, ifname: &str, bssid: nexus_core::MacAddr) -> Result<()> {
        let ifindex = self
            .ifindex_for(ifname)
            .ok_or(WifiError::NotAttached { ifindex: 0 })?;
        self.supplicant
            .roam(ifindex, crate::types::RoamTarget::Bss(bssid))
            .await
    }

    /// Change the live roam mode. In-memory only — nothing to push
    /// to the supplicant (wpa_supplicant's `BgScan` knob is
    /// orthogonal; DD-003 §7.1 defers that to §9 wiring). The
    /// ifname is validated but otherwise unused — roam mode is a
    /// backend-wide config today, not per-interface.
    async fn operator_set_roaming_mode(
        &mut self,
        ifname: &str,
        mode: crate::types::RoamMode,
    ) -> Result<()> {
        // Validate the ifname so the caller gets `NotAttached`
        // rather than a silent config mutation for a bogus iface.
        let _ifindex = self
            .ifindex_for(ifname)
            .ok_or(WifiError::NotAttached { ifindex: 0 })?;
        self.config.roam_mode = mode;
        Ok(())
    }

    /// Look up the ifindex for a given ifname. O(interfaces) but
    /// the set is tiny (at most a handful of radios on any real
    /// platform).
    fn ifindex_for(&self, ifname: &str) -> Option<u32> {
        self.interfaces
            .iter()
            .find(|(_, e)| e.info.ifname == ifname)
            .map(|(i, _)| *i)
    }

    // -----------------------------------------------------------------
    // NexusEvent dispatch
    // -----------------------------------------------------------------

    async fn on_nexus_event(&mut self, event: NexusEvent) -> Result<()> {
        match event {
            NexusEvent::InterfaceDiscovered(info)
                if matches!(info.kind, InterfaceKind::Wireless { .. }) =>
            {
                let ifindex = info.ifindex;
                let ifname = info.ifname.clone();
                let wiphy_name = match &info.kind {
                    InterfaceKind::Wireless { wiphy_name, .. } => Some(wiphy_name.clone()),
                    _ => None,
                };
                if self.supplicant_up {
                    if let Err(e) = self.supplicant.attach(ifindex, &ifname).await {
                        tracing::warn!(ifname, error = %e, "supplicant attach failed");
                    }
                }
                self.interfaces
                    .insert(ifindex, WifiInterfaceEntry::new(info));
                self.schedulers
                    .insert(ifindex, ScanScheduler::with_defaults());
                // Seed the initial Powered state from sysfs: the
                // rfkill watcher's synthetic `RFKILL_OP_ADD` events
                // may have fired before this interface registered
                // and been dropped as "unknown wiphy" by `on_rfkill`.
                if let Some(w) = wiphy_name {
                    match crate::rfkill::read_current_state(&w) {
                        Ok(powered) => {
                            let _ = self
                                .event_tx
                                .send(NexusEvent::WifiRfkillChanged { ifindex, powered });
                        }
                        Err(e) => {
                            tracing::debug!(wiphy = %w, error = %e, "initial rfkill read failed");
                        }
                    }
                }
                // Kick off an initial scan per DD-003 §5.1.
                self.request_scan(ifindex, ScanParams::default()).await?;
            }
            NexusEvent::InterfaceRemoved { ifindex }
                if self.interfaces.remove(&ifindex).is_some() =>
            {
                self.schedulers.remove(&ifindex);
                self.cache.clear(ifindex);
                self.active_handle.remove(&ifindex);
                self.last_signal_poll.remove(&ifindex);
                if self.supplicant_up {
                    let _ = self.supplicant.detach(ifindex).await;
                }
            }
            // Profile added / removed / updated in the store — the
            // profile store broadcasts this on every successful
            // `put_wifi` / `remove_wifi`. Reload our in-memory cache
            // so `operator_connect` and the automatic selector see
            // the latest set without a daemon restart.
            NexusEvent::ProfileChanged {
                kind: nexus_core::ProfileKind::Wifi,
                ..
            } => match self.profile_store.load_wifi().await {
                Ok(new_profiles) => {
                    tracing::debug!(count = new_profiles.len(), "wifi profile cache reloaded");
                    self.profiles = new_profiles;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "wifi profile reload failed");
                }
            },
            _ => {}
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // SupplicantEvent dispatch — DD-003 §§3.2, 9.5, 12.1
    // -----------------------------------------------------------------

    async fn on_supplicant_event(&mut self, event: SupplicantEvent) -> Result<()> {
        match event {
            SupplicantEvent::State { ifindex, state } => {
                self.on_supplicant_state(ifindex, state).await?;
            }
            SupplicantEvent::ScanComplete { ifindex } => {
                self.on_scan_complete(ifindex).await?;
            }
            SupplicantEvent::DaemonUp => {
                self.supplicant_up = true;
                m::set_supplicant_available(self.supplicant.name(), true);
                // Re-attach every known interface and kick a scan
                // per DD-003 §12.1.
                let ifindices: Vec<(u32, String)> = self
                    .interfaces
                    .iter()
                    .map(|(i, e)| (*i, e.info.ifname.clone()))
                    .collect();
                for (ifindex, ifname) in ifindices {
                    if let Err(e) = self.supplicant.attach(ifindex, &ifname).await {
                        tracing::warn!(ifname, error = %e, "re-attach after daemon up failed");
                    }
                    let _ = self.request_scan(ifindex, ScanParams::default()).await;
                }
            }
            SupplicantEvent::DaemonDown => {
                self.supplicant_up = false;
                m::set_supplicant_available(self.supplicant.name(), false);
                // Transition every interface to Disconnected
                // { SupplicantUnavailable } per §12.1.
                let ifindices: Vec<u32> = self.interfaces.keys().copied().collect();
                for ifindex in ifindices {
                    let was_ready = self.is_connected(ifindex);
                    if let Some(e) = self.interfaces.get_mut(&ifindex) {
                        e.state = WifiState::Disconnected {
                            reason: DisconnectReason::SupplicantUnavailable,
                        };
                    }
                    self.emit_state(ifindex);
                    if was_ready {
                        self.emit_link_lost(ifindex, m::link_lost_reason::SUPPLICANT_DOWN);
                    }
                    self.active_handle.remove(&ifindex);
                }
            }
        }
        Ok(())
    }

    async fn on_supplicant_state(&mut self, ifindex: u32, state: SupplicantState) -> Result<()> {
        // Side effects that need `&self` calls after the entry
        // borrow releases.
        enum After {
            None,
            LinkReady {
                bssid: nexus_core::MacAddr,
            },
            Disconnected {
                reason: DisconnectHint,
                mapped: DisconnectReason,
                was_connected: bool,
                bssid: nexus_core::MacAddr,
            },
        }

        let (ifname, after) = {
            let Some(entry) = self.interfaces.get_mut(&ifindex) else {
                return Ok(());
            };
            let prev_connected = matches!(entry.state, WifiState::Connected { .. });
            let ifname = entry.info.ifname.clone();

            let after = match state {
                SupplicantState::Scanning => {
                    entry.state = WifiState::Scanning;
                    After::None
                }
                SupplicantState::Associating => {
                    let (bssid, ssid) = extract_bssid_ssid(&entry.state);
                    entry.state = WifiState::Connecting { bssid, ssid };
                    After::None
                }
                SupplicantState::Authenticating => {
                    let (bssid, ssid) = extract_bssid_ssid(&entry.state);
                    entry.state = WifiState::Authenticating { bssid, ssid };
                    After::None
                }
                SupplicantState::FourWayHandshake => {
                    let (bssid, ssid) = extract_bssid_ssid(&entry.state);
                    entry.state = WifiState::Handshaking { bssid, ssid };
                    After::None
                }
                SupplicantState::Connected {
                    bssid,
                    ssid,
                    frequency,
                } => {
                    let security = guess_security_mode(entry);
                    entry.state = WifiState::Connected {
                        bssid,
                        ssid,
                        frequency,
                        signal_dbm: -50, // filled in by next signal poll
                        security,
                    };
                    After::LinkReady { bssid }
                }
                SupplicantState::Disconnected { reason } => {
                    let mapped = map_disconnect(reason.clone());
                    let bssid = extract_bssid_ssid(&entry.state).0;
                    entry.state = WifiState::Disconnected {
                        reason: mapped.clone(),
                    };
                    After::Disconnected {
                        reason,
                        mapped,
                        was_connected: prev_connected,
                        bssid,
                    }
                }
            };
            (ifname, after)
        };

        match after {
            After::None => {}
            After::LinkReady { bssid } => {
                m::record_link_ready(&ifname);
                let _ = self.event_tx.send(NexusEvent::WifiLinkReady { ifindex });
                self.retry.record_success(ifindex, bssid);
            }
            After::Disconnected {
                reason,
                mapped,
                was_connected,
                bssid,
            } => {
                if matches!(mapped, DisconnectReason::CredentialsInvalid) {
                    if let Some((profile_id, _)) = self.active_handle.get(&ifindex) {
                        self.retry
                            .mark_credentials_invalid(*profile_id, "wifi auth failure");
                    }
                }
                if bssid.0 != [0; 6] {
                    let _ = self.retry.record_failure(ifindex, bssid, Instant::now());
                }
                if was_connected {
                    self.emit_link_lost(ifindex, reason_label(&reason));
                }
                // Look up the security tag from the profile that
                // was active at connect time — if we still have a
                // handle on it. Falls back to "unknown" only on a
                // truly-orphan disconnect (no prior connect, or
                // profile was removed mid-session).
                let security = self
                    .active_handle
                    .get(&ifindex)
                    .and_then(|(id, _)| self.profiles.iter().find(|p| p.id == *id))
                    .map(|p| security_tag(&p.network.security))
                    .unwrap_or("unknown");
                m::record_connect(
                    &ifname,
                    security,
                    match reason {
                        DisconnectHint::BadCredentials => m::connect_outcome::CREDENTIALS_INVALID,
                        DisconnectHint::AssociationTimeout => m::connect_outcome::ASSOC_TIMEOUT,
                        DisconnectHint::HandshakeTimeout => m::connect_outcome::HANDSHAKE_TIMEOUT,
                        DisconnectHint::AuthFailure => m::connect_outcome::AUTH_FAILURE,
                        _ => m::connect_outcome::OTHER,
                    },
                );
            }
        }
        self.emit_state(ifindex);
        Ok(())
    }

    // -----------------------------------------------------------------
    // Scan flow
    // -----------------------------------------------------------------

    async fn request_scan(&mut self, ifindex: u32, params: ScanParams) -> Result<()> {
        if !self.supplicant_up {
            return Ok(());
        }
        let Some(entry) = self.interfaces.get_mut(&ifindex) else {
            return Err(WifiError::NotAttached { ifindex });
        };
        entry.state = WifiState::Scanning;
        self.emit_state(ifindex);
        self.supplicant.scan(ifindex, params).await
    }

    async fn on_scan_complete(&mut self, ifindex: u32) -> Result<()> {
        let results = self.supplicant.get_scan_results(ifindex).await?;
        self.cache.replace(ifindex, results.clone());

        let _ = self.event_tx.send(NexusEvent::WifiScanComplete {
            ifindex,
            results: results.iter().cloned().map(to_nexus_bss_info).collect(),
        });

        let currently_connected = self.is_connected(ifindex);

        // Update the scheduler. `matched` is true when we either
        // select a new network OR we're already connected and the
        // scan was a roam evaluation.
        let mut matched = false;

        if currently_connected {
            // Roam evaluation per §7.3.
            if let Some(target) = self.evaluate_roam(ifindex, &results).await {
                matched = true;
                let ifname = self.ifname_of(ifindex);
                m::record_roam(&ifname, self.config.roam_mode.as_str(), "attempted");
                let _ = self
                    .supplicant
                    .roam(ifindex, crate::types::RoamTarget::Bss(target))
                    .await;
            }
        } else if let Some((profile, bss)) = select_network(&self.profiles, &results) {
            matched = true;
            self.try_connect(ifindex, profile, bss).await?;
        }

        if let Some(sched) = self.schedulers.get_mut(&ifindex) {
            sched.on_scan_complete(matched, Instant::now());
        }
        Ok(())
    }

    async fn fire_scheduled_scans(&mut self) -> Result<()> {
        let power = *self.power.read().await;
        let ifindices: Vec<u32> = self
            .interfaces
            .iter()
            .filter(|(i, e)| {
                !scans_suspended(&e.state, e.roam_mode)
                    && self
                        .schedulers
                        .get(i)
                        .and_then(|s| s.next_scan_at(power))
                        .is_some_and(|t| t <= Instant::now())
            })
            .map(|(i, _)| *i)
            .collect();
        for ifindex in ifindices {
            let _ = self.request_scan(ifindex, ScanParams::default()).await;
        }
        Ok(())
    }

    async fn try_connect(
        &mut self,
        ifindex: u32,
        profile: WifiProfile,
        bss: BssInfo,
    ) -> Result<()> {
        let now = Instant::now();
        if !self.retry.rate_limit_allows(ifindex, now) {
            return Ok(());
        }
        if self.retry.is_blacklisted(ifindex, bss.bssid, now) {
            return Ok(());
        }

        // DD-003 §6.5: forget the previous handle before issuing
        // a new connect if it's for a different profile.
        if let Some((prev_profile, prev_handle)) = self.active_handle.remove(&ifindex) {
            if prev_profile != profile.id {
                let _ = self.supplicant.forget_network(ifindex, prev_handle).await;
            } else {
                // Same profile: keep the old handle around until we
                // confirm the new one.
                self.active_handle
                    .insert(ifindex, (prev_profile, prev_handle));
            }
        }

        if let Some(entry) = self.interfaces.get_mut(&ifindex) {
            entry.state = WifiState::Connecting {
                bssid: bss.bssid,
                ssid: bss.ssid.clone(),
            };
            self.emit_state(ifindex);
        }

        let net = to_network_config(&profile);
        let handle = self.supplicant.connect(ifindex, &net).await?;
        self.active_handle.insert(ifindex, (profile.id, handle));
        m::record_connect(
            &self.ifname_of(ifindex),
            security_tag(&profile.network.security),
            m::connect_outcome::SUCCESS,
        );
        Ok(())
    }

    async fn evaluate_roam(
        &self,
        ifindex: u32,
        results: &[BssInfo],
    ) -> Option<nexus_core::MacAddr> {
        if self.config.roam_mode != RoamMode::Nexus {
            return None;
        }
        let entry = self.interfaces.get(&ifindex)?;
        let (current_bssid, current_rssi, current_ssid) = match &entry.state {
            WifiState::Connected {
                bssid,
                signal_dbm,
                ssid,
                ..
            } => (*bssid, *signal_dbm, ssid.clone()),
            _ => return None,
        };
        let candidates: Vec<BssInfo> = results
            .iter()
            .filter(|b| b.ssid == current_ssid)
            .cloned()
            .collect();
        pick_roam_target(
            self.config.roam_policy,
            current_bssid,
            current_rssi,
            &candidates,
        )
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    fn is_connected(&self, ifindex: u32) -> bool {
        matches!(
            self.interfaces.get(&ifindex).map(|e| &e.state),
            Some(WifiState::Connected { .. }),
        )
    }

    fn ifname_of(&self, ifindex: u32) -> String {
        self.interfaces
            .get(&ifindex)
            .map(|e| e.info.ifname.clone())
            .unwrap_or_else(|| format!("ifindex{ifindex}"))
    }

    fn emit_state(&self, ifindex: u32) {
        if let Some(entry) = self.interfaces.get(&ifindex) {
            let _ = self.event_tx.send(NexusEvent::WifiStateChanged {
                ifindex,
                state: entry.state.clone(),
            });
        }
    }

    fn emit_link_lost(&self, ifindex: u32, reason: &str) {
        let ifname = self.ifname_of(ifindex);
        m::record_link_lost(&ifname, reason);
        let _ = self.event_tx.send(NexusEvent::WifiLinkLost { ifindex });
    }

    fn earliest_scan_deadline(&self, power: PowerState) -> Option<Instant> {
        self.schedulers
            .values()
            .filter_map(|s| s.next_scan_at(power))
            .min()
    }

    fn refresh_metrics(&self) {
        let mut counts: HashMap<&'static str, u64> = HashMap::new();
        for entry in self.interfaces.values() {
            *counts.entry(state_label(&entry.state)).or_insert(0) += 1;
        }
        for label in &[
            "idle",
            "scanning",
            "connecting",
            "authenticating",
            "handshaking",
            "connected",
            "roaming",
            "disconnected",
            "gone",
        ] {
            m::set_interfaces_managed(label, counts.get(label).copied().unwrap_or(0));
        }
        for ifindex in self.interfaces.keys() {
            let ifname = self.ifname_of(*ifindex);
            m::set_bss_cache_entries(&ifname, self.cache.len(*ifindex) as u64);
            m::set_bssid_blacklisted(
                &ifname,
                self.retry.blacklisted_count(*ifindex, Instant::now()) as u64,
            );
        }
        m::set_profile_credentials_invalid(self.retry.credentials_invalid_count() as u64);
    }
}

async fn sleep_until_option(deadline: Option<Instant>) {
    match deadline {
        Some(t) => {
            let now = Instant::now();
            let d = t
                .saturating_duration_since(now)
                .max(Duration::from_millis(1));
            tokio::time::sleep(d).await;
        }
        None => pending().await,
    }
}

/// `select!`-friendly recv for the optional rfkill receiver. When
/// the watcher isn't wired the future parks forever (the other
/// arms continue to fire); when the watcher dropped its sender
/// we also park rather than loop-spinning on `None`.
async fn recv_rfkill(
    rx: &mut Option<tokio::sync::mpsc::Receiver<RfkillState>>,
) -> Option<RfkillState> {
    match rx.as_mut() {
        Some(r) => match r.recv().await {
            Some(s) => Some(s),
            None => pending().await,
        },
        None => pending().await,
    }
}

fn extract_bssid_ssid(state: &WifiState) -> (nexus_core::MacAddr, nexus_core::Ssid) {
    match state {
        WifiState::Connecting { bssid, ssid }
        | WifiState::Authenticating { bssid, ssid }
        | WifiState::Handshaking { bssid, ssid } => (*bssid, ssid.clone()),
        WifiState::Connected { bssid, ssid, .. } => (*bssid, ssid.clone()),
        _ => (
            nexus_core::MacAddr([0; 6]),
            nexus_core::Ssid::new(b"\0".to_vec())
                .unwrap_or_else(|_| nexus_core::Ssid::new(b"x".to_vec()).unwrap()),
        ),
    }
}

fn guess_security_mode(entry: &WifiInterfaceEntry) -> SecurityMode {
    // Without a live BSS cache lookup we default to Wpa2Psk; the
    // backend refines via scan cache when real implementations
    // arrive.
    entry
        .current_bss_capabilities
        .as_ref()
        .map(|_| SecurityMode::Wpa2Psk)
        .unwrap_or(SecurityMode::Wpa2Psk)
}

fn map_disconnect(hint: DisconnectHint) -> DisconnectReason {
    match hint {
        DisconnectHint::Unspecified => DisconnectReason::Unspecified,
        DisconnectHint::AssociationTimeout => DisconnectReason::ApInitiated,
        DisconnectHint::AuthFailure => DisconnectReason::AuthExpired,
        DisconnectHint::HandshakeTimeout => DisconnectReason::HandshakeTimeout,
        DisconnectHint::BadCredentials => DisconnectReason::CredentialsInvalid,
        DisconnectHint::LocalRequest => DisconnectReason::LocalRequest,
        DisconnectHint::DaemonUnavailable => DisconnectReason::SupplicantUnavailable,
    }
}

fn reason_label(hint: &DisconnectHint) -> &'static str {
    match hint {
        DisconnectHint::DaemonUnavailable => m::link_lost_reason::SUPPLICANT_DOWN,
        _ => m::link_lost_reason::DEAUTH,
    }
}

fn security_tag(security: &nexus_profile_store::SecurityConfig) -> &'static str {
    use nexus_profile_store::SecurityConfig as S;
    match security {
        S::Open => "open",
        S::Owe => "owe",
        S::Wpa2Personal { .. } => "wpa2_personal",
        S::Wpa3Personal { .. } => "wpa3_personal",
        S::Wpa2Wpa3Personal { .. } => "wpa2_wpa3_personal",
        S::Wpa2Enterprise(_) => "wpa2_enterprise",
        S::Wpa3Enterprise(_) => "wpa3_enterprise",
    }
}

/// `NexusEvent::WifiScanComplete.results` is `Vec<BssInfo>` from
/// nexus-core; we keep our own local BSS type (§4.1) for richer
/// fields and translate at the emission boundary.
fn to_nexus_bss_info(bss: BssInfo) -> nexus_core::BssInfo {
    nexus_core::BssInfo {
        bssid: bss.bssid,
        ssid: bss.ssid,
        frequency: bss.frequency,
        signal_dbm: bss.signal_dbm,
        capabilities: nexus_core::BssCapabilities {
            ht: bss.capabilities.ht,
            vht: bss.capabilities.vht,
            he: bss.capabilities.he,
            eht: bss.capabilities.eht,
            ft: bss.capabilities.ft,
            pmf_required: bss.capabilities.pmf_required,
            pmf_capable: bss.capabilities.pmf_capable,
            wps: bss.capabilities.wps,
        },
        security: bss.security,
        age_ms: bss.age_ms,
    }
}
