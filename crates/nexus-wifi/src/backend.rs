//! Wi-Fi Backend event loop. See DD-003 §§3, 5, 6, 7, 12, 13.

use std::collections::HashMap;
use std::future::pending;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{DisconnectReason, InterfaceKind, NexusEvent, SecurityMode, WifiState};
use nexus_interface_monitor::MonitorCommand;
use nexus_profile_store::{ProfileRef, ProfileStore, SecurityConfig, WifiProfile};
use tokio::sync::{RwLock, broadcast};
use tokio_util::sync::CancellationToken;

use crate::error::{Result, WifiError};
use crate::lifecycle::{WifiInterfaceEntry, scans_suspended, state_label};
use crate::metrics as m;
use crate::power::PowerState;
use crate::profile::{profile_key, to_network_config};
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
    /// DD-003 §12.4: how long an interface is allowed to sit in
    /// `Connecting` / `Authenticating` / `Handshaking` before the
    /// backend declares a driver wedge and bounces `IFF_UP`.
    /// Defaults to 30 s; tests shrink it to keep the loop fast.
    pub driver_wedge_threshold: Duration,
}

/// Interval of the backend's `select!`-loop heartbeat. Tight enough
/// that power-state transitions during otherwise-quiet periods
/// (Sleep → Active) observe the new cadence within ~1 s, loose
/// enough that the idle-daemon wakeup rate is negligible.
const HEARTBEAT: Duration = Duration::from_secs(1);

/// DD-003 §12.4 threshold — how long an interface is allowed to
/// sit in a pre-connected state (Connecting / Authenticating /
/// Handshaking) before the backend declares a driver wedge and
/// toggles the interface admin state.
const DRIVER_WEDGE_THRESHOLD: Duration = Duration::from_secs(30);

/// DD-003 §12.4 — the short down-window between `SetAdminUp(false)`
/// and `SetAdminUp(true)` that unsticks most brcmfmac / ath10k
/// firmwares.
const DRIVER_WEDGE_DOWN_WINDOW: Duration = Duration::from_secs(2);

/// DD-003 §7.3 — minimum spacing between Nexus-driven roam-
/// evaluation scans. Keeps a persistently-weak RSSI from blasting
/// a directed scan on every heartbeat.
const ROAM_EVAL_MIN_SPACING: Duration = Duration::from_secs(30);

/// Common 2.4 GHz channels (1, 6, 11) the directed roam scan
/// always includes. Covers the long tail of APs that only operate
/// on the non-overlapping set.
const ROAM_EVAL_2GHZ_DEFAULTS: &[u32] = &[2412, 2437, 2462];

impl Default for WifiConfig {
    fn default() -> Self {
        Self {
            roam_mode: RoamMode::Supplicant,
            roam_policy: RoamPolicy::default(),
            signal_poll_interval: Duration::from_secs(5),
            disconnect_cool_down: Duration::from_secs(2),
            driver_wedge_threshold: DRIVER_WEDGE_THRESHOLD,
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

    /// Profile IDs the operator paused via
    /// `Wifi.Disconnect(pause_auto_connect=true)`. The automatic
    /// selector skips these (see [`select_network`]). Runtime-only
    /// — never persisted, never touches `WifiNetworkSettings::auto_connect`
    /// on disk. Cleared by an explicit `Connect` to the same
    /// profile, by `ProfileChanged` reconciliation (the on-disk
    /// shape changed, so let auto-connect re-evaluate), and by
    /// daemon restart.
    paused_profiles: std::collections::HashSet<ulid::Ulid>,

    power: Arc<RwLock<PowerState>>,
    config: WifiConfig,
    supplicant_up: bool,

    /// Timestamp of the last successful `SignalPoll` for each
    /// interface. Populated on the heartbeat tick so we only poll
    /// per `WifiConfig::signal_poll_interval`, not every tick.
    last_signal_poll: HashMap<u32, Instant>,

    /// Last observed rfkill state per interface, fed by the
    /// `/dev/rfkill` watcher via [`Self::on_rfkill`]. `false` means
    /// rfkill is asserted (radio off). Absent means we have no
    /// rfkill information for the interface yet — the heartbeat
    /// signal-poll path treats absent as "assume powered" so the
    /// pre-rfkill-watcher behaviour is unchanged.
    interface_powered: HashMap<u32, bool>,

    /// Deadline at which each `Disconnected`-but-not-permanent
    /// interface transitions to `Idle` and re-scans (DD-003 §3.2
    /// / §6.4). Entries are inserted on the Disconnected transition
    /// and removed either on expiry or when the interface moves
    /// to another state out of band.
    disconnect_cooldowns: HashMap<u32, Instant>,

    /// Timestamp of the most recent `Connecting` entry per
    /// interface. Consumed on `Connected` to record
    /// `nexus_wifi_connect_duration_seconds` (DD-003 §12.5).
    connect_started_at: HashMap<u32, Instant>,

    /// Last observed [`PowerState`]. Used to detect a Sleep →
    /// Active / Background edge and run the wake-from-sleep
    /// recovery (DD-003 §13.3).
    last_power_state: PowerState,

    /// `/dev/rfkill` edges from the watcher task (see
    /// [`crate::rfkill`]). `None` when rfkill couldn't be opened —
    /// tests bypass it and the daemon logs a warning.
    rfkill_rx: Option<tokio::sync::mpsc::Receiver<RfkillState>>,
    /// Write-side handle for the `fi.nexus.Wifi.Powered` setter.
    /// `None` mirrors the read-side; the `SetPowered` command
    /// returns `Unsupported` when the writer isn't wired.
    rfkill_writer: Option<RfkillWriter>,

    /// Sender for [`MonitorCommand`] — `None` when the Interface
    /// Monitor isn't wired into this backend (tests, or a deployment
    /// without it). The driver-wedge recovery (DD-003 §12.4) soft-
    /// fails to a log line when this is absent.
    monitor_commands: Option<tokio::sync::mpsc::Sender<MonitorCommand>>,

    /// Timestamp at which each interface entered its current
    /// pre-connected state (`Connecting` / `Authenticating` /
    /// `Handshaking`). Cleared on transition to `Connected` /
    /// `Disconnected`. Drives the driver-wedge detector.
    dwell_since: HashMap<u32, Instant>,

    /// Last time the backend fired a Nexus-mode roam-evaluation
    /// scan for each interface. Used to rate-limit the trigger in
    /// [`on_heartbeat`] so a persistently-low RSSI doesn't blast
    /// a directed scan every tick.
    last_roam_scan: HashMap<u32, Instant>,

    /// In-flight roam: the target BSSID the backend asked the
    /// supplicant to roam to. Consumed on the next
    /// `SupplicantState::Connected` (success = bssid matches, fail
    /// otherwise) or on a terminal `Disconnected` (always fail).
    roam_in_flight: HashMap<u32, nexus_core::MacAddr>,

    /// In-flight scan: start timestamp + classified type. Consumed
    /// on the next `SupplicantEvent::ScanComplete` to emit
    /// `nexus_wifi_scans_total{outcome}` and
    /// `nexus_wifi_scan_duration_seconds`. K5 / DD-003 §12.5.
    scan_in_flight: HashMap<u32, ScanInFlight>,
}

/// Per-interface scan-tracking record. Cheap (Instant + a label
/// pointer); cleared on every `ScanComplete`.
#[derive(Debug, Clone, Copy)]
struct ScanInFlight {
    started: Instant,
    /// `m::scan_type::*` label.
    scan_type: &'static str,
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
            paused_profiles: std::collections::HashSet::new(),
            power: Arc::new(RwLock::new(PowerState::default())),
            config,
            supplicant_up: true,
            last_signal_poll: HashMap::new(),
            interface_powered: HashMap::new(),
            disconnect_cooldowns: HashMap::new(),
            connect_started_at: HashMap::new(),
            last_power_state: PowerState::default(),
            rfkill_rx: None,
            rfkill_writer: None,
            monitor_commands: None,
            dwell_since: HashMap::new(),
            last_roam_scan: HashMap::new(),
            roam_in_flight: HashMap::new(),
            scan_in_flight: HashMap::new(),
        }
    }

    /// Wire the Interface Monitor's [`MonitorCommand`] sender into
    /// the backend. Used by the daemon to give driver-wedge recovery
    /// a path back to the monitor's rtnetlink socket (DD-003 §12.4).
    pub fn with_monitor_commands(
        mut self,
        tx: tokio::sync::mpsc::Sender<MonitorCommand>,
    ) -> Self {
        self.monitor_commands = Some(tx);
        self
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

    /// Wire only the read-side of `/dev/rfkill`. Used by integration
    /// tests that want to drive `on_rfkill` (and the backend's
    /// state-machine response) without opening the real device or
    /// providing a write-capable [`RfkillWriter`]. The write path is
    /// tested separately via `operator_set_powered` in deployments
    /// where the watcher actually opens `/dev/rfkill`.
    pub fn with_rfkill_rx(mut self, rx: tokio::sync::mpsc::Receiver<RfkillState>) -> Self {
        self.rfkill_rx = Some(rx);
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
                    let now_power = *self.power.read().await;
                    if self.last_power_state == PowerState::Sleep
                        && now_power != PowerState::Sleep
                    {
                        self.on_wake().await;
                    }
                    self.last_power_state = now_power;
                    self.on_heartbeat().await;
                    self.process_driver_wedges().await;
                    self.process_disconnect_cooldowns().await;
                },
            }
            self.refresh_metrics();
        }
    }

    /// Map an rfkill edge from the watcher to a
    /// `NexusEvent::WifiRfkillChanged` and drive the corresponding
    /// state-machine transition. Events for wiphys that aren't in
    /// the interface registry (e.g. a USB dongle that the Interface
    /// Monitor hasn't enumerated yet) are dropped — the next event
    /// after the interface lands will reflect current state.
    async fn on_rfkill(&mut self, state: RfkillState) {
        let Some(ifindex) = self.ifindex_for_wiphy(&state) else {
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
        if state.powered {
            self.apply_radio_on(ifindex);
        } else {
            self.apply_radio_off(ifindex);
        }
    }

    /// Drive the state machine to `Disconnected{RfKilled}` and
    /// release every per-interface bookkeeping slot that only makes
    /// sense while the radio is active. Idempotent — a re-entry
    /// while already `Disconnected{RfKilled}` is a no-op.
    ///
    /// Called from both `on_rfkill(false)` (kernel CHANGE event from
    /// `/dev/rfkill`) and `operator_set_powered(false)` (D-Bus
    /// SetPowered after a successful rfkill write). The kernel
    /// CHANGE event that follows a successful operator write hits
    /// here too; the second call is the no-op.
    fn apply_radio_off(&mut self, ifindex: u32) {
        self.interface_powered.insert(ifindex, false);
        let already_rf_killed = matches!(
            self.interfaces.get(&ifindex).map(|e| &e.state),
            Some(WifiState::Disconnected {
                reason: DisconnectReason::RfKilled
            })
        );
        if already_rf_killed {
            return;
        }
        let was_associated = matches!(
            self.interfaces.get(&ifindex).map(|e| &e.state),
            Some(WifiState::Connected { .. } | WifiState::Roaming { .. })
        );
        if let Some(entry) = self.interfaces.get_mut(&ifindex) {
            entry.state = WifiState::Disconnected {
                reason: DisconnectReason::RfKilled,
            };
        } else {
            return;
        }
        // Release every slot whose semantics depend on a live radio.
        // RfKilled is permanent (DD-003 §3.2 / `is_permanent()`), so
        // we deliberately do NOT arm a disconnect cooldown — the
        // only way out is a `Powered=true` edge, handled by
        // `apply_radio_on`.
        self.dwell_since.remove(&ifindex);
        self.connect_started_at.remove(&ifindex);
        self.active_handle.remove(&ifindex);
        self.disconnect_cooldowns.remove(&ifindex);
        self.roam_in_flight.remove(&ifindex);
        self.scan_in_flight.remove(&ifindex);
        self.last_signal_poll.remove(&ifindex);
        if was_associated {
            self.emit_link_lost(ifindex, m::link_lost_reason::RFKILL);
        }
        self.emit_state(ifindex);
    }

    /// Promote `Disconnected{RfKilled}` back to `Idle` and kick the
    /// scheduler so the normal scan + auto-select path runs. Other
    /// states are left alone — a `Powered=true` while we were
    /// `Disconnected{Other}` doesn't change anything (we'll cool
    /// down to Idle on the normal cooldown sweep).
    fn apply_radio_on(&mut self, ifindex: u32) {
        self.interface_powered.insert(ifindex, true);
        let was_rf_killed = matches!(
            self.interfaces.get(&ifindex).map(|e| &e.state),
            Some(WifiState::Disconnected {
                reason: DisconnectReason::RfKilled
            })
        );
        if !was_rf_killed {
            return;
        }
        if let Some(entry) = self.interfaces.get_mut(&ifindex) {
            entry.state = WifiState::Idle;
        }
        self.emit_state(ifindex);
        if let Some(sched) = self.schedulers.get_mut(&ifindex) {
            sched.fire_now(Instant::now());
        }
    }

    /// True when we have positive evidence the interface's radio is
    /// off (rfkill asserted). Absent rfkill info → assume powered.
    /// Used to gate every operation that requires a live radio.
    fn radio_off(&self, ifindex: u32) -> bool {
        matches!(self.interface_powered.get(&ifindex), Some(false))
    }

    /// Correlate an incoming rfkill edge to the interface it belongs
    /// to. Primary match is `state.device_path` against each
    /// candidate wiphy's own sysfs device path (see
    /// `rfkill::wiphy_device_path`) — robust against drivers that
    /// register the WLAN rfkill's `name` independently of
    /// `NL80211_ATTR_WIPHY_NAME`. Falls back to a `wiphy_name` string
    /// match when `device_path` is unavailable (e.g. hwsim / unit
    /// tests, which inject `RfkillState` directly with no sysfs
    /// backing).
    fn ifindex_for_wiphy(&self, state: &RfkillState) -> Option<u32> {
        if let Some(device_path) = &state.device_path {
            let by_device = self.interfaces.iter().find_map(|(ifindex, entry)| match &entry.info.kind {
                InterfaceKind::Wireless { wiphy_name, .. } => {
                    (crate::rfkill::wiphy_device_path(wiphy_name).as_ref() == Some(device_path))
                        .then_some(*ifindex)
                }
                _ => None,
            });
            if by_device.is_some() {
                return by_device;
            }
        }
        self.interfaces.iter().find_map(|(ifindex, entry)| match &entry.info.kind {
            InterfaceKind::Wireless { wiphy_name, .. } if wiphy_name == &state.wiphy_name => Some(*ifindex),
            _ => None,
        })
    }

    /// Tick handler: for each connected interface, if the signal
    /// poll is due, call the supplicant, update the cached RSSI on
    /// the `Connected` state, and emit a
    /// [`NexusEvent::WifiSignalPoll`]. The in-state RSSI is what
    /// `evaluate_roam` reads for the signal-degradation trigger
    /// (DD-003 §7.3) — without this refresh the variant's
    /// `signal_dbm` stays at its connect-time sentinel and the
    /// Nexus-mode roam path never fires.
    async fn on_heartbeat(&mut self) {
        let now = Instant::now();
        let power = *self.power.read().await;
        // DD-003 §13.1: signal polling follows the power state.
        // `Active` uses the configured interval (default 5 s);
        // `Background` triples it (~15 s); `Sleep` disables polling.
        let Some(interval) = signal_poll_interval(self.config.signal_poll_interval, power) else {
            return;
        };
        let due: Vec<u32> = self
            .interfaces
            .iter()
            .filter_map(|(ifindex, entry)| {
                if !matches!(entry.state, WifiState::Connected { .. }) {
                    return None;
                }
                // Skip interfaces whose radio is rfkilled. The
                // cached WifiState may still be `Connected`
                // because wpa_supplicant hasn't emitted the
                // matching Disconnected yet, but `SignalPoll`
                // against a powered-off radio just returns
                // `Failed to read signal` until association
                // collapses. Absent rfkill info → assume powered
                // (test harnesses, deployments without /dev/rfkill).
                if matches!(self.interface_powered.get(ifindex), Some(false)) {
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
            // Record the attempt up front so a failed `signal_info`
            // doesn't trigger a retry on every 1 Hz heartbeat tick —
            // we wait for the next configured interval just like a
            // successful poll. Without this, a transient supplicant
            // error (or a known-quirky `SignalPoll` return signature)
            // pegs the supplicant proxy at 1 Hz indefinitely.
            self.last_signal_poll.insert(ifindex, now);
            match self.supplicant.signal_info(ifindex).await {
                Ok(info) => {
                    let ifname = self.ifname_of(ifindex);
                    if let Some(entry) = self.interfaces.get_mut(&ifindex) {
                        if let WifiState::Connected {
                            ref mut signal_dbm, ..
                        } = entry.state
                        {
                            *signal_dbm = info.rssi_dbm;
                        }
                    }
                    m::set_signal_dbm(&ifname, info.rssi_dbm);
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

        // DD-003 §5.1 (trigger 4) / §7.3: when RSSI has dropped
        // below `roam_trigger_dbm` and Nexus is the roam authority,
        // fire a directed scan so the next ScanDone feeds
        // `evaluate_roam`. Rate-limited to `ROAM_EVAL_MIN_SPACING`
        // so a persistently-low signal doesn't saturate the radio.
        self.maybe_trigger_roam_scans(now).await;
    }

    /// Kick a directed scan on every `Connected` interface whose
    /// RSSI has dropped past the roam trigger, subject to the
    /// rate-limit window. No-op when `roam_mode != Nexus` — the
    /// supplicant owns roam decisions in the other modes.
    async fn maybe_trigger_roam_scans(&mut self, now: Instant) {
        if self.config.roam_mode != RoamMode::Nexus {
            return;
        }
        let trigger = self.config.roam_policy.trigger_dbm;
        // Gather candidates (ifindex, ssid, freqs) up front so we
        // can release the immutable borrows before calling
        // `request_scan` (which takes `&mut self`).
        let mut candidates: Vec<(u32, nexus_core::Ssid, Vec<u32>)> = Vec::new();
        for (ifindex, entry) in &self.interfaces {
            let WifiState::Connected {
                ref ssid,
                signal_dbm,
                frequency,
                ..
            } = entry.state
            else {
                continue;
            };
            if signal_dbm > trigger {
                continue;
            }
            if let Some(last) = self.last_roam_scan.get(ifindex) {
                if now.duration_since(*last) < ROAM_EVAL_MIN_SPACING {
                    continue;
                }
            }
            let freqs = self.likely_frequencies(*ifindex, ssid, frequency);
            candidates.push((*ifindex, ssid.clone(), freqs));
        }
        for (ifindex, ssid, frequencies) in candidates {
            self.last_roam_scan.insert(ifindex, now);
            let params = ScanParams {
                ssids: vec![ssid],
                frequencies,
                active: true,
                allow_roam: true,
            };
            if let Err(e) = self.request_scan(ifindex, params).await {
                tracing::debug!(ifindex, error = %e, "roam-evaluation scan failed");
            }
        }
    }

    /// DD-003 §7.3 `get_likely_frequencies`: union of (a) every
    /// frequency on which any BSS with `ssid` has been seen in the
    /// recent scan cache, (b) the current association's frequency,
    /// and (c) the common 2.4 GHz channels 1/6/11. A targeted scan
    /// of this set completes in ~1 s on typical chipsets versus
    /// 3–5 s for a full-spectrum scan — which matters a lot for
    /// roaming latency.
    fn likely_frequencies(
        &self,
        ifindex: u32,
        ssid: &nexus_core::Ssid,
        current_frequency: u32,
    ) -> Vec<u32> {
        let mut out: Vec<u32> = self
            .cache
            .list(ifindex)
            .into_iter()
            .filter(|b| &b.ssid == ssid)
            .map(|b| b.frequency)
            .collect();
        if current_frequency != 0 {
            out.push(current_frequency);
        }
        out.extend_from_slice(ROAM_EVAL_2GHZ_DEFAULTS);
        out.sort_unstable();
        out.dedup();
        out
    }

    /// DD-003 §12.4 driver-wedge detector. Walks the dwell map; any
    /// interface that has been pre-connected for longer than
    /// `DRIVER_WEDGE_THRESHOLD` is probably stuck in firmware.
    /// Recovery: detach the supplicant, bounce `IFF_UP` via the
    /// Interface Monitor's rtnetlink socket, re-attach, and flag
    /// the interface `Disconnected { DriverWedge }` so the cooldown
    /// gate rolls it back to `Idle` and the normal retry path
    /// takes over.
    async fn process_driver_wedges(&mut self) {
        let now = Instant::now();
        let threshold = self.config.driver_wedge_threshold;
        let wedged: Vec<u32> = self
            .dwell_since
            .iter()
            .filter_map(|(i, since)| (now.duration_since(*since) >= threshold).then_some(*i))
            .collect();
        for ifindex in wedged {
            self.recover_wedged_interface(ifindex).await;
        }
    }

    async fn recover_wedged_interface(&mut self, ifindex: u32) {
        let ifname = self.ifname_of(ifindex);
        tracing::warn!(
            ifindex,
            ifname = %ifname,
            "wifi: pre-connected state exceeded driver-wedge threshold; recovering"
        );
        m::record_driver_wedge_recovery(&ifname);

        // Clear bookkeeping first so state updates from the detach
        // don't tangle with a second wedge trip.
        self.dwell_since.remove(&ifindex);
        self.connect_started_at.remove(&ifindex);
        self.active_handle.remove(&ifindex);

        // 1. Detach the supplicant so it doesn't fight the flap.
        if self.supplicant_up {
            let _ = self.supplicant.detach(ifindex).await;
        }

        // 2. Bounce IFF_UP via the Interface Monitor. If the monitor
        // sender isn't wired (tests, config without the monitor),
        // skip straight to re-attach — the recovery degrades but
        // the Disconnected transition below still unblocks the
        // normal retry flow.
        if let Some(cmd_tx) = self.monitor_commands.clone() {
            if let Err(e) = cmd_tx
                .send(MonitorCommand::SetAdminUp {
                    ifindex,
                    up: false,
                    reply: None,
                })
                .await
            {
                tracing::warn!(ifindex, error = %e, "SetAdminUp(false) send failed");
            }
            tokio::time::sleep(DRIVER_WEDGE_DOWN_WINDOW).await;
            if let Err(e) = cmd_tx
                .send(MonitorCommand::SetAdminUp {
                    ifindex,
                    up: true,
                    reply: None,
                })
                .await
            {
                tracing::warn!(ifindex, error = %e, "SetAdminUp(true) send failed");
            }
        } else {
            tracing::debug!(
                ifindex,
                "monitor command channel not wired; skipping IFF_UP flap"
            );
        }

        // 3. Re-attach the supplicant so it can discover the fresh
        // interface state.
        if self.supplicant_up {
            if let Err(e) = self.supplicant.attach(ifindex, &ifname).await {
                tracing::warn!(ifindex, error = %e, "post-wedge re-attach failed");
            }
        }

        // 4. Mark the interface Disconnected { DriverWedge } and
        // arm the cooldown — the normal Idle → scan path will pick
        // it back up.
        if let Some(entry) = self.interfaces.get_mut(&ifindex) {
            entry.state = WifiState::Disconnected {
                reason: DisconnectReason::DriverWedge,
            };
        }
        self.emit_state(ifindex);
        self.disconnect_cooldowns
            .insert(ifindex, Instant::now() + self.config.disconnect_cool_down);
    }

    /// Handle the Sleep → Active / Background edge (DD-003 §13.3).
    /// Force every scan scheduler to fire immediately, then probe
    /// each `Connected` interface's signal; if the probe errors the
    /// link didn't survive suspend, so emit
    /// `Disconnected { PostSleepRecovery }` and let the normal
    /// reconnect path take over. `PostSleepRecovery` is a transient
    /// reason so the cooldown gate rolls the interface back into
    /// `Idle` on the next tick.
    async fn on_wake(&mut self) {
        let now = Instant::now();
        for sched in self.schedulers.values_mut() {
            sched.fire_now(now);
        }
        let connected: Vec<u32> = self
            .interfaces
            .iter()
            .filter_map(|(ifindex, e)| {
                matches!(e.state, WifiState::Connected { .. }).then_some(*ifindex)
            })
            .collect();
        for ifindex in connected {
            match self.supplicant.signal_info(ifindex).await {
                Ok(info) => {
                    let ifname = self.ifname_of(ifindex);
                    if let Some(entry) = self.interfaces.get_mut(&ifindex) {
                        if let WifiState::Connected {
                            ref mut signal_dbm, ..
                        } = entry.state
                        {
                            *signal_dbm = info.rssi_dbm;
                        }
                    }
                    m::set_signal_dbm(&ifname, info.rssi_dbm);
                }
                Err(e) => {
                    tracing::info!(
                        ifindex,
                        error = %e,
                        "post-wake signal probe failed; marking link lost"
                    );
                    if let Some(entry) = self.interfaces.get_mut(&ifindex) {
                        entry.state = WifiState::Disconnected {
                            reason: DisconnectReason::PostSleepRecovery,
                        };
                    }
                    self.emit_state(ifindex);
                    self.emit_link_lost(ifindex, m::link_lost_reason::CARRIER_DOWN);
                    self.active_handle.remove(&ifindex);
                    self.connect_started_at.remove(&ifindex);
                    // PostSleepRecovery is transient; arm the cooldown
                    // so the next tick moves the interface to Idle
                    // and the scheduler (already fired above) runs.
                    self.disconnect_cooldowns
                        .insert(ifindex, now + self.config.disconnect_cool_down);
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
                    None => Err(WifiError::UnknownInterface {
                        ifname: ifname.clone(),
                    }),
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
            crate::WifiCommand::Disconnect {
                ifname,
                pause_auto_connect,
                reply,
            } => {
                let _ = reply.send(
                    self.operator_disconnect(&ifname, pause_auto_connect)
                        .await,
                );
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
            crate::WifiCommand::ProvideCredential {
                ifname,
                network,
                field,
                value,
                reply,
            } => {
                let _ = reply.send(
                    self.operator_provide_credential(&ifname, &network, &field, &value)
                        .await,
                );
            }
        }
    }

    /// Forward a credential reply from the D-Bus layer into the
    /// supplicant. DD-003 §9.2.
    async fn operator_provide_credential(
        &mut self,
        ifname: &str,
        network: &str,
        field: &str,
        value: &str,
    ) -> Result<()> {
        let ifindex = self.require_ifindex(ifname)?;
        self.supplicant
            .provide_network_credential(ifindex, network, field, value)
            .await
    }

    /// Toggle soft-rfkill for the named interface. Resolves the
    /// ifname to its wiphy via the local registry, then hands off
    /// to [`RfkillWriter::set_blocked`]. Returns `NotAttached` if
    /// the interface isn't registered and `Supplicant` (as a
    /// catch-all for rfkill plumbing errors) on write failure.
    ///
    /// On a successful rfkill write the local state machine is
    /// driven optimistically — the kernel `RFKILL_OP_CHANGE` event
    /// the watcher delivers a moment later runs through `on_rfkill`
    /// and lands in the same idempotent helper, so the second
    /// arrival is a no-op. The optimism keeps the bus surface and
    /// the backend's internal state in lockstep from the moment
    /// `SetPowered` returns.
    async fn operator_set_powered(&mut self, ifname: &str, on: bool) -> Result<()> {
        let ifindex = self.require_ifindex(ifname)?;
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
            })?
            .clone();
        writer
            .set_blocked(&wiphy_name, !on)
            .await
            .map_err(|e| WifiError::Rfkill {
                ifindex,
                detail: format!("rfkill write: {e}"),
            })?;
        let _ = self.event_tx.send(NexusEvent::WifiRfkillChanged {
            ifindex,
            powered: on,
        });
        if on {
            self.apply_radio_on(ifindex);
        } else {
            self.apply_radio_off(ifindex);
        }
        Ok(())
    }

    /// Operator-initiated `Connect`. Looks up the profile in the
    /// backend's in-memory cache, forgets any prior active handle
    /// on the interface, and drives the supplicant's connect flow.
    /// Does not consult the `retry` book — an explicit operator
    /// action isn't subject to the automatic-selection rate limit.
    async fn operator_connect(&mut self, ifname: &str, profile_id: ulid::Ulid) -> Result<()> {
        let ifindex = self.require_ifindex(ifname)?;
        if self.radio_off(ifindex) {
            return Err(WifiError::Rfkill {
                ifindex,
                detail: "radio is rfkilled".to_owned(),
            });
        }
        let profile = self
            .profiles
            .iter()
            .find(|p| p.id == profile_id)
            .cloned()
            .ok_or_else(|| WifiError::ProfileNotFound {
                id: profile_id.to_string(),
            })?;

        // An explicit Connect is the operator's way of saying
        // "I want this network now." Clear any pause that
        // `Disconnect(pause_auto_connect=true)` may have left
        // for this profile.
        self.paused_profiles.remove(&profile_id);

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
        let now = Instant::now();
        self.active_handle.insert(ifindex, (profile.id, handle));
        self.connect_started_at.insert(ifindex, now);
        self.dwell_since.insert(ifindex, now);
        // Success metric is recorded on `SupplicantState::Connected`
        // (C4). `operator_connect` only dispatches; the handshake
        // completion — or failure — is what the counter tracks.
        Ok(())
    }

    /// Operator-initiated `Disconnect`. The supplicant tears down
    /// the association; the in-memory active handle is cleared so a
    /// subsequent auto-select cycle can compete fresh.
    ///
    /// When `pause_auto_connect` is true and there was an active
    /// profile, its id is added to [`Self::paused_profiles`] so
    /// [`select_network`] skips it on the next auto-select tick.
    /// The on-disk profile is *not* modified — DD-006 §6.3 calls
    /// this out as the differentiator from `Profile.Update`.
    async fn operator_disconnect(
        &mut self,
        ifname: &str,
        pause_auto_connect: bool,
    ) -> Result<()> {
        let ifindex = self.require_ifindex(ifname)?;
        let active_profile = self.active_handle.get(&ifindex).map(|(id, _)| *id);
        self.supplicant.disconnect(ifindex).await?;
        self.active_handle.remove(&ifindex);
        if pause_auto_connect {
            if let Some(id) = active_profile {
                self.paused_profiles.insert(id);
                tracing::info!(
                    ifname,
                    profile_id = %id,
                    "wifi profile paused from auto-connect (runtime only)"
                );
            }
        }
        Ok(())
    }

    /// Operator-initiated `Roam` to a specific BSSID. Hands off to
    /// the supplicant regardless of roam_mode — wpa_supplicant
    /// rejects the call on its own side when the interface's roam
    /// config says no, and surfacing that as a plain error is more
    /// useful than us silently no-op'ing here.
    async fn operator_roam(&mut self, ifname: &str, bssid: nexus_core::MacAddr) -> Result<()> {
        let ifindex = self.require_ifindex(ifname)?;
        if self.radio_off(ifindex) {
            return Err(WifiError::Rfkill {
                ifindex,
                detail: "radio is rfkilled".to_owned(),
            });
        }
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
        // Validate the ifname so the caller gets `UnknownInterface`
        // rather than a silent config mutation for a bogus iface.
        let _ifindex = self.require_ifindex(ifname)?;
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

    /// `ifindex_for` plus an `UnknownInterface` error on miss. S2:
    /// every operator-facing path uses this helper instead of the
    /// `NotAttached { ifindex: 0 }` sentinel that collided with
    /// real ifindex 0 (`lo`).
    fn require_ifindex(&self, ifname: &str) -> Result<u32> {
        self.ifindex_for(ifname).ok_or_else(|| WifiError::UnknownInterface {
            ifname: ifname.to_owned(),
        })
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
                // Try to attach. The result determines the entry's
                // initial state — DD-003 §3.1: `Idle` only after a
                // successful attach. K8: failure puts the interface
                // in `Disconnected { SupplicantUnavailable }` and
                // skips the initial scan; the next `DaemonUp`
                // re-attach attempt will roll it back into Idle.
                let attach_ok = if self.supplicant_up {
                    match self.supplicant.attach(ifindex, &ifname).await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(ifname, error = %e, "supplicant attach failed");
                            false
                        }
                    }
                } else {
                    false
                };
                let mut entry = WifiInterfaceEntry::new(info);
                if !attach_ok {
                    entry.state = WifiState::Disconnected {
                        reason: DisconnectReason::SupplicantUnavailable,
                    };
                }
                self.interfaces.insert(ifindex, entry);
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
                if attach_ok {
                    // Kick off an initial scan per DD-003 §5.1.
                    self.request_scan(ifindex, initial_scan_params(&self.profiles))
                        .await?;
                } else {
                    self.emit_state(ifindex);
                }
            }
            NexusEvent::InterfaceRemoved { ifindex }
                if self.interfaces.remove(&ifindex).is_some() =>
            {
                self.schedulers.remove(&ifindex);
                self.cache.clear(ifindex);
                self.active_handle.remove(&ifindex);
                self.last_signal_poll.remove(&ifindex);
                self.interface_powered.remove(&ifindex);
                self.disconnect_cooldowns.remove(&ifindex);
                self.connect_started_at.remove(&ifindex);
                self.dwell_since.remove(&ifindex);
                self.last_roam_scan.remove(&ifindex);
                self.roam_in_flight.remove(&ifindex);
                self.scan_in_flight.remove(&ifindex);
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
                    // Reconcile the RetryBook with the on-disk flag.
                    // An operator clearing `credentials_invalid = false`
                    // in the file (or putting a fresh profile) should
                    // re-enable auto-select immediately.
                    for p in &new_profiles {
                        if !p.network.credentials_invalid && self.retry.credentials_invalid(p.id) {
                            self.retry.clear_credentials_invalid(p.id);
                        }
                    }
                    // Drop runtime auto-connect pauses whose
                    // profile is gone or was edited. The operator
                    // changing the on-disk shape is implicit
                    // re-engagement; let auto-select re-evaluate.
                    let live: std::collections::HashSet<ulid::Ulid> =
                        new_profiles.iter().map(|p| p.id).collect();
                    self.paused_profiles.retain(|id| live.contains(id));
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
            SupplicantEvent::ScanComplete { ifindex, success } => {
                self.on_scan_complete(ifindex, success).await?;
            }
            SupplicantEvent::BssCacheStale { ifindex } => {
                // S5: refresh the local BSS cache without emitting
                // a public WifiScanComplete event. Drop a debug
                // log on read failures (the next driven scan will
                // re-populate the cache anyway).
                match self.supplicant.get_scan_results(ifindex).await {
                    Ok(results) => self.cache.replace(ifindex, results),
                    Err(e) => {
                        tracing::debug!(
                            ifindex,
                            error = %e,
                            "BssCacheStale refresh failed"
                        );
                    }
                }
            }
            SupplicantEvent::NetworkRequest {
                ifindex,
                network,
                field,
                text,
            } => {
                // Just relay to the D-Bus layer; the operator side
                // responds via `WifiCommand::ProvideCredential`.
                // DD-003 §9.2 / DD-006 §9.
                let _ = self.event_tx.send(NexusEvent::WifiNetworkRequest {
                    ifindex,
                    network,
                    field,
                    text,
                });
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
                let scan_params = initial_scan_params(&self.profiles);
                for (ifindex, ifname) in ifindices {
                    if let Err(e) = self.supplicant.attach(ifindex, &ifname).await {
                        tracing::warn!(ifname, error = %e, "re-attach after daemon up failed");
                    }
                    let _ = self.request_scan(ifindex, scan_params.clone()).await;
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
            DwellStart,
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

        // While the radio is off, the supplicant's state machine is
        // unreliable — a stale `Connected` event can arrive after
        // rfkill engaged, or wpa_supplicant may emit a generic
        // `Disconnected{LocalRequest}` whose mapped reason would
        // overwrite our authoritative `Disconnected{RfKilled}`.
        // The rfkill path (`apply_radio_off`) is the source of
        // truth for this window; just consume the supplicant event
        // without touching state.
        if self.radio_off(ifindex) {
            return Ok(());
        }

        // Pre-compute the security-mode lookup that `Connected` needs
        // — it reaches into `self.cache` and `self.profiles`, which
        // conflict with the `&mut entry` borrow below.
        let precomputed_security = if let SupplicantState::Connected { bssid, .. } = &state {
            let bssid = *bssid;
            let active_security = self
                .active_handle
                .get(&ifindex)
                .and_then(|(id, _)| self.profiles.iter().find(|p| p.id == *id))
                .map(|p| p.network.security.clone());
            Some(resolve_connected_security(
                self.cache.lookup(ifindex, bssid).as_ref(),
                active_security.as_ref(),
            ))
        } else {
            None
        };

        let (ifname, after) = {
            let Some(entry) = self.interfaces.get_mut(&ifindex) else {
                return Ok(());
            };
            let prev_connected = matches!(entry.state, WifiState::Connected { .. });
            let prev_bssid: Option<nexus_core::MacAddr> = match &entry.state {
                WifiState::Connected { bssid, .. } => Some(*bssid),
                _ => None,
            };
            let prev_signal_dbm: Option<i32> = match &entry.state {
                WifiState::Connected { signal_dbm, .. } => Some(*signal_dbm),
                _ => None,
            };
            let ifname = entry.info.ifname.clone();

            let after = match state {
                SupplicantState::Scanning => {
                    // wpa_supplicant fires `State=scanning` for both
                    // standalone pre-association scans AND background
                    // scans done while still associated. DD-003 §3.1
                    // has no `Connected → Scanning` edge — a
                    // background scan keeps the link up at the kernel
                    // level, so don't fold the cached state to
                    // `Scanning` when we were already in a live
                    // association. Same protection
                    // `request_scan` already applies to backend-
                    // initiated scans.
                    //
                    // Without this guard, the next `State=completed`
                    // re-emit looks like a `not-Connected → Connected`
                    // transition (`prev_connected` flipped to false
                    // while we were folded to Scanning) and the
                    // `After::LinkReady` path fans out — fresh
                    // `WifiLinkReady` event, inflated connect-success
                    // metrics, and a connectivity-probe re-run on
                    // every roam-eval / signal-poll-driven scan.
                    if !matches!(
                        entry.state,
                        WifiState::Connected { .. } | WifiState::Roaming { .. }
                    ) {
                        entry.state = WifiState::Scanning;
                    }
                    After::None
                }
                SupplicantState::Associating | SupplicantState::Associated => {
                    // DD-003 §9.5 collapses both into Connecting at
                    // the public layer; the SupplicantState split
                    // exists for internal observability (K1). S4:
                    // when there's no prior context (the supplicant
                    // moved the interface unilaterally — typical
                    // after a nexusd restart with a stale config),
                    // emit Connecting with synthetic placeholders
                    // that the next `Connected` event will overwrite.
                    let (bssid, ssid) = extract_bssid_ssid(&entry.state)
                        .unwrap_or_else(placeholder_assoc);
                    entry.state = WifiState::Connecting { bssid, ssid };
                    After::DwellStart
                }
                SupplicantState::Authenticating => {
                    let (bssid, ssid) = extract_bssid_ssid(&entry.state)
                        .unwrap_or_else(placeholder_assoc);
                    entry.state = WifiState::Authenticating { bssid, ssid };
                    After::DwellStart
                }
                SupplicantState::FourWayHandshake => {
                    let (bssid, ssid) = extract_bssid_ssid(&entry.state)
                        .unwrap_or_else(placeholder_assoc);
                    entry.state = WifiState::Handshaking { bssid, ssid };
                    After::DwellStart
                }
                SupplicantState::Connected {
                    bssid,
                    ssid,
                    frequency,
                } => {
                    // Same-BSSID Connected re-emit (typically from the
                    // wpa_supplicant adapter's reconciliation tick at
                    // RECONCILE_INTERVAL = 2 s — see
                    // crates/nexus-wifi/src/supplicant/wpa_supplicant.rs)
                    // is a state refresh, not a fresh link edge.
                    // Keeping the cached signal_dbm avoids clobbering
                    // it with the -50 dBm sentinel until the next
                    // genuine signal poll, and skipping `After::LinkReady`
                    // stops the downstream WifiLinkReady fan-out (the
                    // connectivity probe re-runs every refresh, the
                    // connect-success metrics get inflated, etc.).
                    let same_bss_refresh = prev_connected && prev_bssid == Some(bssid);
                    let signal_dbm = if same_bss_refresh {
                        prev_signal_dbm.unwrap_or(-50)
                    } else {
                        -50 // sentinel; filled by next signal poll
                    };
                    entry.state = WifiState::Connected {
                        bssid,
                        ssid,
                        frequency,
                        signal_dbm,
                        security: precomputed_security
                            .expect("precomputed for Connected arm above"),
                    };
                    if same_bss_refresh {
                        After::None
                    } else {
                        After::LinkReady { bssid }
                    }
                }
                SupplicantState::Disconnected { reason } => {
                    let mapped = map_disconnect(reason.clone());
                    // BSSID is only meaningful when the prior state
                    // carried one; zero MAC means "no association
                    // context" and the retry book skips its
                    // record_failure step.
                    let bssid = extract_bssid_ssid(&entry.state)
                        .map(|(b, _)| b)
                        .unwrap_or(nexus_core::MacAddr([0; 6]));
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

        // Any state transition out of `Disconnected` cancels a
        // pending cooldown. Re-armed below on re-entry.
        if !matches!(after, After::Disconnected { .. }) {
            self.disconnect_cooldowns.remove(&ifindex);
        }

        match after {
            After::None => {}
            After::DwellStart => {
                // First entry into a pre-connected state arms the
                // dwell timer that the driver-wedge detector
                // consults. Subsequent transitions inside the
                // pre-connected window (Associating → Auth → 4-way)
                // refresh it so only genuine sticking counts.
                self.dwell_since.insert(ifindex, Instant::now());
            }
            After::LinkReady { bssid } => {
                self.dwell_since.remove(&ifindex);
                // Stamp `last_connected_at` on the active profile —
                // both in-memory and on disk — so the auto-select
                // tiebreaker after a reboot prefers the network we
                // were just on. Best-effort: a write failure
                // doesn't block link-ready emission.
                self.stamp_last_connected(ifindex).await;
                // Resolve any in-flight roam: this `Connected`
                // transition is the answer to the roam dispatch.
                // Success means the reported BSSID matches the
                // target; a mismatch means the supplicant fell back
                // to another BSS (e.g. FT rejected, scan picked a
                // different candidate). DD-003 §7 / §12.5.
                if let Some(target) = self.roam_in_flight.remove(&ifindex) {
                    let outcome = if bssid == target { "success" } else { "fail" };
                    m::record_roam(&ifname, self.config.roam_mode.as_str(), outcome);
                    // A roam completion isn't a fresh
                    // `LinkReady` — we were already connected —
                    // so skip the link-ready counter and the
                    // connect-attempt metrics for this path.
                    let _ = self.event_tx.send(NexusEvent::WifiLinkReady { ifindex });
                    self.retry.record_success(ifindex, bssid);
                    // Drop the connect-started stamp if one was
                    // somehow still around; a roam-born Connected
                    // doesn't belong in the connect_duration
                    // histogram.
                    self.connect_started_at.remove(&ifindex);
                } else {
                    m::record_link_ready(&ifname);
                    let _ = self.event_tx.send(NexusEvent::WifiLinkReady { ifindex });
                    self.retry.record_success(ifindex, bssid);
                    // Emit the success counter + duration histogram
                    // now that the handshake actually completed.
                    // DD-003 §12.5.
                    let security = self
                        .active_handle
                        .get(&ifindex)
                        .and_then(|(id, _)| self.profiles.iter().find(|p| p.id == *id))
                        .map(|p| security_tag(&p.network.security))
                        .unwrap_or("unknown");
                    m::record_connect(&ifname, security, m::connect_outcome::SUCCESS);
                    if let Some(started) = self.connect_started_at.remove(&ifindex) {
                        let secs = started.elapsed().as_secs_f64();
                        m::record_connect_duration(&ifname, security, secs);
                    }
                }
            }
            After::Disconnected {
                reason,
                mapped,
                was_connected,
                bssid,
            } => {
                let active_profile = self.active_handle.get(&ifindex).map(|(id, _)| *id);
                // A `BadCredentials` hint is already a definitive
                // auth failure (mock path, or future wpa_supplicant
                // paths that learn to distinguish wrong-key from
                // generic handshake timeout). Promote the profile
                // straight away.
                if matches!(mapped, DisconnectReason::CredentialsInvalid) {
                    if let Some(profile_id) = active_profile {
                        self.promote_credentials_invalid(profile_id, "wifi auth failure")
                            .await;
                    }
                }
                // Record the BSSID failure and check whether this
                // one tripped the blacklist. For WPA2/WPA3-Personal
                // a 4-way handshake timeout that recurs past the
                // retry threshold is, in practice, almost always a
                // wrong passphrase — promote to CredentialsInvalid
                // per DD-003 §6.3's "Authentication failure (bad
                // PSK)" row, which the blunt DisconnectReason code
                // alone can't distinguish.
                let just_blacklisted = if bssid.0 != [0; 6] {
                    self.retry.record_failure(ifindex, bssid, Instant::now())
                } else {
                    false
                };
                if just_blacklisted
                    && matches!(reason, DisconnectHint::HandshakeTimeout)
                    && !matches!(mapped, DisconnectReason::CredentialsInvalid)
                {
                    if let Some(profile_id) = active_profile {
                        if self.profiles.iter().any(|p| {
                            p.id == profile_id && is_psk_like(&p.network.security)
                        }) {
                            self.promote_credentials_invalid(
                                profile_id,
                                "4-way handshake timeouts past retry threshold",
                            )
                            .await;
                        }
                    }
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
                        DisconnectHint::AuthFailure | DisconnectHint::EapFailure => {
                            m::connect_outcome::AUTH_FAILURE
                        }
                        _ => m::connect_outcome::OTHER,
                    },
                );

                // Drop the connect-started stamp — this attempt
                // ended without reaching `Connected`. No duration
                // is emitted for failed attempts.
                self.connect_started_at.remove(&ifindex);
                // Clear the dwell timer: the pre-connected window
                // ended (cleanly or otherwise). Wedge detection
                // only looks at live pre-connected states.
                self.dwell_since.remove(&ifindex);
                // An in-flight roam that ended in Disconnected is a
                // fail. DD-003 §12.5.
                if self.roam_in_flight.remove(&ifindex).is_some() {
                    m::record_roam(&ifname, self.config.roam_mode.as_str(), "fail");
                }

                // Arm the cooldown unless the reason is permanent
                // (CredentialsInvalid). Permanent reasons keep the
                // interface in `Disconnected` until the operator
                // updates the profile — DD-003 §3.2 / §6.4.
                if !mapped.is_permanent() {
                    self.disconnect_cooldowns.insert(
                        ifindex,
                        Instant::now() + self.config.disconnect_cool_down,
                    );
                } else {
                    self.disconnect_cooldowns.remove(&ifindex);
                }
            }
        }
        self.emit_state(ifindex);
        Ok(())
    }

    /// Walk pending cooldowns; for each interface whose deadline
    /// has elapsed, transition out of `Disconnected { .. }` back to
    /// `Idle` and kick a fresh scan. Safe to call on every
    /// heartbeat — it's O(interfaces) with tiny constants.
    async fn process_disconnect_cooldowns(&mut self) {
        let now = Instant::now();
        let due: Vec<u32> = self
            .disconnect_cooldowns
            .iter()
            .filter_map(|(i, t)| (*t <= now).then_some(*i))
            .collect();
        for ifindex in due {
            self.disconnect_cooldowns.remove(&ifindex);
            let should_transition = matches!(
                self.interfaces.get(&ifindex).map(|e| &e.state),
                Some(WifiState::Disconnected { .. })
            );
            if !should_transition {
                continue;
            }
            if let Some(entry) = self.interfaces.get_mut(&ifindex) {
                entry.state = WifiState::Idle;
            }
            self.emit_state(ifindex);
            if let Some(sched) = self.schedulers.get_mut(&ifindex) {
                sched.fire_now(now);
            }
        }
    }

    // -----------------------------------------------------------------
    // Scan flow
    // -----------------------------------------------------------------

    async fn request_scan(&mut self, ifindex: u32, mut params: ScanParams) -> Result<()> {
        if !self.supplicant_up {
            return Ok(());
        }
        // Skip the scan when the radio is off. Without this the
        // post-cooldown sweep (firing on every Disconnected → Idle
        // transition) would drive scans against a powered-off radio
        // until the supplicant tore the association down, after
        // which the cooldown gate would re-fire on every tick. The
        // radio-on edge (`apply_radio_on`) explicitly fires the
        // scheduler once Powered=true comes back.
        if self.radio_off(ifindex) {
            return Ok(());
        }
        // Only let wpa_supplicant act on scan results autonomously
        // when the operator has explicitly delegated roaming to it.
        // Callers that already set `allow_roam = true` (roam-
        // evaluation scans in `nexus` mode) keep their flag. DD-003
        // §4.1 / §9.3.
        if !params.allow_roam && self.config.roam_mode == RoamMode::Supplicant {
            params.allow_roam = true;
        }
        let Some(entry) = self.interfaces.get_mut(&ifindex) else {
            return Err(WifiError::NotAttached { ifindex });
        };
        // Only fold to `Scanning` when the interface isn't
        // already in an association — DD-003 §3.1 has no
        // `Connected → Scanning` edge, and overwriting Connected
        // here would make the post-scan `evaluate_roam` path miss
        // the `currently_connected` check (C8 directed roam-eval
        // scans fire while the link is up).
        if matches!(
            entry.state,
            WifiState::Idle | WifiState::Disconnected { .. } | WifiState::Gone
        ) {
            entry.state = WifiState::Scanning;
            self.emit_state(ifindex);
        }
        let scan_type = classify_scan(&params);
        self.scan_in_flight.insert(
            ifindex,
            ScanInFlight {
                started: Instant::now(),
                scan_type,
            },
        );
        // Forward the request; on failure, retire the in-flight
        // record immediately and label the metric `failed` —
        // there will be no `ScanComplete` to roll it through.
        if let Err(e) = self.supplicant.scan(ifindex, params).await {
            let ifname = self.ifname_of(ifindex);
            if self.scan_in_flight.remove(&ifindex).is_some() {
                m::record_scan(&ifname, scan_type, m::scan_outcome::FAILED);
            }
            return Err(e);
        }
        Ok(())
    }

    async fn on_scan_complete(&mut self, ifindex: u32, success: bool) -> Result<()> {
        // Roll up the in-flight record into the metric. K5 / DD-003
        // §12.5 — duration is meaningful even on `aborted`; the
        // outcome label disambiguates.
        let now = Instant::now();
        if let Some(record) = self.scan_in_flight.remove(&ifindex) {
            let ifname = self.ifname_of(ifindex);
            let outcome = if success {
                m::scan_outcome::SUCCESS
            } else {
                m::scan_outcome::ABORTED
            };
            m::record_scan(&ifname, record.scan_type, outcome);
            m::record_scan_duration(
                &ifname,
                record.scan_type,
                now.duration_since(record.started).as_secs_f64(),
            );
        }

        let results = self.supplicant.get_scan_results(ifindex).await?;
        self.cache.replace(ifindex, results.clone());

        let _ = self.event_tx.send(NexusEvent::WifiScanComplete {
            ifindex,
            success,
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
                self.dispatch_roam(ifindex, target).await;
            }
        } else if let Some((profile, bss)) =
            select_network(&self.profiles, &results, &self.paused_profiles)
        {
            matched = true;
            self.try_connect(ifindex, profile, bss).await?;
        }

        if let Some(sched) = self.schedulers.get_mut(&ifindex) {
            sched.on_scan_complete(matched, now);
        }
        Ok(())
    }

    /// Dispatch a roam toward `target`. Snapshots the current
    /// `(from, ssid)` so the backend can transition into
    /// [`WifiState::Roaming`] while the supplicant runs the
    /// 802.11r / reassociation dance; the matching `Connected`
    /// transition in `on_supplicant_state` emits the
    /// `roams_total{outcome=success|fail}` counter and folds the
    /// state back to `Connected`. DD-003 §7 / §12.5.
    async fn dispatch_roam(&mut self, ifindex: u32, target: nexus_core::MacAddr) {
        if self.radio_off(ifindex) {
            return;
        }
        let ifname = self.ifname_of(ifindex);
        let (from, ssid) = match self.interfaces.get(&ifindex).map(|e| &e.state) {
            Some(WifiState::Connected { bssid, ssid, .. }) => (*bssid, ssid.clone()),
            _ => {
                // Not currently connected — operator-initiated roam
                // outside a live association. Send the command
                // through anyway so the supplicant's error surfaces
                // to the caller, but skip the state transition.
                m::record_roam(&ifname, self.config.roam_mode.as_str(), "attempted");
                let _ = self
                    .supplicant
                    .roam(ifindex, crate::types::RoamTarget::Bss(target))
                    .await;
                return;
            }
        };
        if let Some(entry) = self.interfaces.get_mut(&ifindex) {
            entry.state = WifiState::Roaming {
                from,
                to: target,
                ssid,
            };
        }
        self.emit_state(ifindex);
        self.roam_in_flight.insert(ifindex, target);
        m::record_roam(&ifname, self.config.roam_mode.as_str(), "attempted");
        if let Err(e) = self
            .supplicant
            .roam(ifindex, crate::types::RoamTarget::Bss(target))
            .await
        {
            // Supplicant rejected the dispatch outright — record
            // the failure now rather than waiting for a timeout.
            tracing::warn!(ifindex, error = %e, "roam dispatch failed");
            m::record_roam(&ifname, self.config.roam_mode.as_str(), "fail");
            self.roam_in_flight.remove(&ifindex);
        }
    }

    async fn fire_scheduled_scans(&mut self) -> Result<()> {
        let power = *self.power.read().await;
        let ifindices: Vec<u32> = self
            .interfaces
            .iter()
            .filter(|(i, e)| {
                !scans_suspended(&e.state, e.roam_mode)
                    // Skip rfkilled radios so a stuck deadline
                    // doesn't tight-loop the select! arm.
                    // `apply_radio_on` re-fires the scheduler when
                    // the radio comes back.
                    && !matches!(self.interface_powered.get(i), Some(false))
                    && self
                        .schedulers
                        .get(i)
                        .and_then(|s| s.next_scan_at(power))
                        .is_some_and(|t| t <= Instant::now())
            })
            .map(|(i, _)| *i)
            .collect();
        let scan_params = initial_scan_params(&self.profiles);
        for ifindex in ifindices {
            let _ = self.request_scan(ifindex, scan_params.clone()).await;
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
        if self.radio_off(ifindex) {
            // Auto-select path: silently swallow. The radio is off,
            // so the operator just paused everything; resuming on
            // the radio-on edge fires a fresh scan that will retry.
            return Ok(());
        }
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
        let now = Instant::now();
        self.active_handle.insert(ifindex, (profile.id, handle));
        self.connect_started_at.insert(ifindex, now);
        self.dwell_since.insert(ifindex, now);
        // Success metric is recorded on `SupplicantState::Connected`
        // (C4) — a successful `AddNetwork + SelectNetwork` just
        // means the supplicant accepted the config, not that the
        // handshake will land.
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

    /// Stamp `last_connected_at = now` on the profile currently
    /// associated with `ifindex`, both in-memory and on disk. The
    /// recency-based tiebreaker in [`select_network`] reads this
    /// field on the next boot to prefer the network the operator
    /// was most recently using over a stranger that happens to
    /// have a stronger signal. Best-effort — store-write failures
    /// log at warn and don't block the LinkReady path.
    async fn stamp_last_connected(&mut self, ifindex: u32) {
        let Some(profile_id) = self.active_handle.get(&ifindex).map(|(id, _)| *id) else {
            return;
        };
        let now = chrono::Utc::now();
        let ssid_hash = {
            let Some(profile) = self.profiles.iter_mut().find(|p| p.id == profile_id) else {
                return;
            };
            profile.network.last_connected_at = Some(now);
            profile_key(profile)
        };
        if let Err(e) = self
            .profile_store
            .set_last_connected(
                ProfileRef::Wifi {
                    ssid_hash: &ssid_hash,
                },
                now,
            )
            .await
        {
            tracing::warn!(
                profile_id = %profile_id,
                error = %e,
                "wifi: persisting last_connected_at failed; in-memory only"
            );
        }
    }

    /// DD-003 §6.3 / §12.3: mark a profile as having invalid
    /// credentials in all three places that need to agree — the
    /// [`RetryBook`] (which accounts the per-session block), the
    /// in-memory [`WifiProfile`] cache (which [`select_network`]
    /// filters on), and the Profile Store on disk (which survives
    /// a restart). Idempotent.
    async fn promote_credentials_invalid(
        &mut self,
        profile_id: ulid::Ulid,
        reason: &'static str,
    ) {
        self.retry.mark_credentials_invalid(profile_id, reason);
        let ssid_hash = {
            let Some(profile) = self.profiles.iter_mut().find(|p| p.id == profile_id) else {
                return;
            };
            if profile.network.credentials_invalid {
                return;
            }
            profile.network.credentials_invalid = true;
            profile_key(profile)
        };
        tracing::warn!(
            profile_id = %profile_id,
            reason,
            "wifi profile marked credentials_invalid"
        );
        if let Err(e) = self
            .profile_store
            .set_credentials_invalid(
                ProfileRef::Wifi {
                    ssid_hash: &ssid_hash,
                },
                true,
            )
            .await
        {
            // Non-fatal: the in-memory flag still blocks auto-select
            // this session; the store will catch up on next put or
            // manual edit.
            tracing::warn!(
                profile_id = %profile_id,
                error = %e,
                "persisting credentials_invalid to profile store failed"
            );
        }
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
            .iter()
            // Skip rfkilled radios so the select! arm doesn't
            // tight-loop on a stale deadline that `request_scan`
            // will only refuse. `apply_radio_on` re-fires the
            // scheduler when the radio comes back, so we don't
            // miss the wake-up.
            .filter(|(i, _)| !matches!(self.interface_powered.get(i), Some(false)))
            .filter_map(|(_, s)| s.next_scan_at(power))
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

/// Synthetic `(zero_mac, "?")` returned when a state transition
/// arrives with no prior association context — usually because
/// the supplicant moved the interface unilaterally (e.g. after
/// a nexusd restart with a stale wpa_supplicant config). The
/// `?` SSID byte is deliberately printable so the placeholder is
/// obvious in logs; it gets overwritten by the next `Connected`
/// event. S4.
fn placeholder_assoc() -> (nexus_core::MacAddr, nexus_core::Ssid) {
    (
        nexus_core::MacAddr([0; 6]),
        nexus_core::Ssid::new(b"?".to_vec()).expect("single-byte ssid is valid"),
    )
}

/// Pull `(bssid, ssid)` out of any state that carries them.
/// Returns `None` for states with no association context (`Idle`,
/// `Scanning`, `Disconnected`, `Roaming`, `Gone`). S4: previously
/// fell back to a synthetic `\0` SSID, which leaked into
/// `WifiStateChanged.Connecting { ssid: "\0" }` when a transition
/// arrived without an intervening event. Callers now thread the
/// `None` case explicitly.
fn extract_bssid_ssid(state: &WifiState) -> Option<(nexus_core::MacAddr, nexus_core::Ssid)> {
    match state {
        WifiState::Connecting { bssid, ssid }
        | WifiState::Authenticating { bssid, ssid }
        | WifiState::Handshaking { bssid, ssid } => Some((*bssid, ssid.clone())),
        WifiState::Connected { bssid, ssid, .. } => Some((*bssid, ssid.clone())),
        WifiState::Roaming { to, ssid, .. } => Some((*to, ssid.clone())),
        _ => None,
    }
}

/// Pick the `SecurityMode` to report on the `WifiState::Connected`
/// payload. When a BSS entry is available, prefer the first mode
/// the profile is compatible with — that's the mode the 4-way /
/// EAP exchange actually used. Fall back to the profile's intrinsic
/// mode when the BSS cache is cold (e.g. after a supplicant
/// restart), and to `Wpa2Psk` only when we have neither signal
/// (startup race; supplicant reports Connected before the first
/// ScanDone has populated the cache).
fn resolve_connected_security(
    bss: Option<&crate::types::BssInfo>,
    profile_security: Option<&SecurityConfig>,
) -> SecurityMode {
    if let (Some(bss), Some(sec)) = (bss, profile_security) {
        if let Some(mode) = bss
            .security
            .iter()
            .find(|m| crate::select::security_compatible(sec, std::slice::from_ref(*m)))
        {
            return *mode;
        }
    }
    if let Some(sec) = profile_security {
        return intrinsic_security_mode(sec);
    }
    if let Some(bss) = bss {
        if let Some(mode) = bss.security.first() {
            return *mode;
        }
    }
    SecurityMode::Wpa2Psk
}

/// The default `SecurityMode` a profile configures when the BSS
/// cache has no better info. Mirrors the `SecurityConfig ↔
/// SecurityMode` correspondence in DD-003 §8.1.
fn intrinsic_security_mode(security: &SecurityConfig) -> SecurityMode {
    match security {
        SecurityConfig::Open => SecurityMode::Open,
        SecurityConfig::Owe => SecurityMode::Owe,
        SecurityConfig::Wpa2Personal { .. } => SecurityMode::Wpa2Psk,
        SecurityConfig::Wpa3Personal { .. } => SecurityMode::Wpa3Sae,
        SecurityConfig::Wpa2Wpa3Personal { .. } => SecurityMode::Wpa2Wpa3Transition,
        SecurityConfig::Wpa2Enterprise(_) => SecurityMode::Wpa2Eap,
        SecurityConfig::Wpa3Enterprise(_) => SecurityMode::Wpa3Eap,
    }
}

fn map_disconnect(hint: DisconnectHint) -> DisconnectReason {
    match hint {
        DisconnectHint::Unspecified => DisconnectReason::Unspecified,
        DisconnectHint::AssociationTimeout => DisconnectReason::Other("assoc_timeout".into()),
        DisconnectHint::AuthFailure => DisconnectReason::AuthExpired,
        DisconnectHint::HandshakeTimeout => DisconnectReason::HandshakeTimeout,
        DisconnectHint::EapFailure => DisconnectReason::EapFailure,
        DisconnectHint::ApInitiated => DisconnectReason::ApInitiated,
        DisconnectHint::Inactivity => DisconnectReason::Inactivity,
        DisconnectHint::ProtocolError => DisconnectReason::ProtocolError,
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

/// Pick the `nexus_wifi_scans_total{type=…}` label for a scan.
/// Closer-to-`roam`-shaped configs win over closer-to-`hidden`
/// shapes; broadcast is the catch-all. K5 / DD-003 §12.5.
fn classify_scan(params: &ScanParams) -> &'static str {
    if params.allow_roam && !params.ssids.is_empty() {
        return m::scan_type::ROAM;
    }
    if !params.ssids.is_empty() {
        return m::scan_type::HIDDEN;
    }
    if !params.frequencies.is_empty() {
        return m::scan_type::DIRECTED;
    }
    m::scan_type::BROADCAST
}

/// Build the [`ScanParams`] used for the startup / scheduled /
/// post-DaemonUp scans. DD-003 §5.1 trigger #1 specifies an
/// active broadcast; §5.1 trigger #5 says profiles with
/// `hidden = true` should be probed with a directed SSID. wpa_supplicant's
/// `Scan(SSIDs=…, Type=active)` accepts both — a non-empty SSID
/// list adds named probes alongside the broadcast probe in the
/// same dwell, so one scan covers both triggers.
fn initial_scan_params(profiles: &[WifiProfile]) -> ScanParams {
    let ssids = profiles
        .iter()
        .filter(|p| p.network.hidden)
        .map(|p| p.network.ssid.clone())
        .collect();
    ScanParams {
        ssids,
        frequencies: Vec::new(),
        active: true,
        // `request_scan` flips `allow_roam = true` when the roam
        // mode is `Supplicant`; default to false here so the
        // intent of this call (steady-state scan, not a roam
        // evaluation) is explicit.
        allow_roam: false,
    }
}

/// Dilate the base signal-poll interval by the current power
/// state. Returns `None` in `Sleep` to suspend polling entirely.
/// DD-003 §13.1: `Active` = base, `Background` ≈ ×3 (15 s when
/// base is 5 s), `Sleep` = paused.
fn signal_poll_interval(base: Duration, power: PowerState) -> Option<Duration> {
    match power {
        PowerState::Active => Some(base),
        PowerState::Background => Some(base * 3),
        PowerState::Sleep => None,
    }
}

/// True for security modes that use a preshared passphrase / PSK
/// (WPA2-PSK, SAE, transition). The 4-way-handshake timeout is
/// almost always a wrong passphrase on these modes; Enterprise
/// flows go through EAP and need different handling (DD-003 §6.3).
fn is_psk_like(security: &SecurityConfig) -> bool {
    matches!(
        security,
        SecurityConfig::Wpa2Personal { .. }
            | SecurityConfig::Wpa3Personal { .. }
            | SecurityConfig::Wpa2Wpa3Personal { .. }
    )
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

#[cfg(test)]
mod helper_tests {
    //! Pure-helper coverage for the §14.1 audit gaps that don't
    //! need a running backend (K5 classifier, S4 placeholder,
    //! C5 power-state dilation, security-mode resolver, PSK
    //! classifier). The end-to-end behaviour tests live in
    //! `tests/backend_tests.rs`.
    use super::*;
    use nexus_core::{MacAddr, SecurityMode, Ssid};
    use nexus_profile_store::{SecurityConfig, WpaPsk};

    fn passphrase(s: &str) -> nexus_profile_store::SecretString {
        nexus_profile_store::SecretString::from(s)
    }

    fn bss(bssid: [u8; 6], ssid: &[u8], modes: Vec<SecurityMode>) -> BssInfo {
        BssInfo {
            bssid: MacAddr(bssid),
            ssid: Ssid::new(ssid.to_vec()).unwrap(),
            frequency: 2412,
            signal_dbm: -50,
            capabilities: crate::types::BssCapabilities::default(),
            security: modes,
            age_ms: 0,
        }
    }

    // ---- K5 classify_scan ------------------------------------------------

    #[test]
    fn classify_scan_broadcast_for_default_params() {
        assert_eq!(
            classify_scan(&ScanParams::default()),
            m::scan_type::BROADCAST
        );
    }

    #[test]
    fn classify_scan_directed_for_frequency_restricted_only() {
        let p = ScanParams {
            frequencies: vec![2412, 2437],
            active: true,
            ..Default::default()
        };
        assert_eq!(classify_scan(&p), m::scan_type::DIRECTED);
    }

    #[test]
    fn classify_scan_hidden_for_named_ssids_without_allow_roam() {
        let p = ScanParams {
            ssids: vec![Ssid::new(b"corp".to_vec()).unwrap()],
            active: true,
            ..Default::default()
        };
        assert_eq!(classify_scan(&p), m::scan_type::HIDDEN);
    }

    #[test]
    fn classify_scan_roam_when_ssids_and_allow_roam_both_set() {
        let p = ScanParams {
            ssids: vec![Ssid::new(b"corp".to_vec()).unwrap()],
            frequencies: vec![2412],
            active: true,
            allow_roam: true,
        };
        assert_eq!(classify_scan(&p), m::scan_type::ROAM);
    }

    // ---- S4 placeholder_assoc -------------------------------------------

    #[test]
    fn placeholder_assoc_uses_zero_mac_and_question_mark_ssid() {
        let (mac, ssid) = placeholder_assoc();
        assert_eq!(mac, MacAddr([0; 6]));
        assert_eq!(ssid.as_bytes(), b"?");
    }

    // ---- extract_bssid_ssid (S4) ----------------------------------------

    #[test]
    fn extract_bssid_ssid_returns_none_for_idle_and_disconnected() {
        assert!(extract_bssid_ssid(&WifiState::Idle).is_none());
        assert!(extract_bssid_ssid(&WifiState::Scanning).is_none());
        assert!(extract_bssid_ssid(&WifiState::Gone).is_none());
        assert!(
            extract_bssid_ssid(&WifiState::Disconnected {
                reason: DisconnectReason::Unspecified,
            })
            .is_none()
        );
    }

    #[test]
    fn extract_bssid_ssid_pulls_from_connected_and_roaming() {
        let connected = WifiState::Connected {
            bssid: MacAddr([0xAA; 6]),
            ssid: Ssid::new(b"corp".to_vec()).unwrap(),
            frequency: 5180,
            signal_dbm: -55,
            security: SecurityMode::Wpa2Psk,
        };
        let (bssid, ssid) = extract_bssid_ssid(&connected).unwrap();
        assert_eq!(bssid, MacAddr([0xAA; 6]));
        assert_eq!(ssid.as_bytes(), b"corp");

        let roaming = WifiState::Roaming {
            from: MacAddr([0x01; 6]),
            to: MacAddr([0x02; 6]),
            ssid: Ssid::new(b"corp".to_vec()).unwrap(),
        };
        let (bssid, _) = extract_bssid_ssid(&roaming).unwrap();
        assert_eq!(bssid, MacAddr([0x02; 6])); // returns the *target* BSSID
    }

    // ---- C5 signal_poll_interval ----------------------------------------

    #[test]
    fn signal_poll_interval_dilates_with_power_state() {
        let base = Duration::from_secs(5);
        assert_eq!(signal_poll_interval(base, PowerState::Active), Some(base));
        assert_eq!(
            signal_poll_interval(base, PowerState::Background),
            Some(base * 3)
        );
        assert_eq!(signal_poll_interval(base, PowerState::Sleep), None);
    }

    // ---- is_psk_like ----------------------------------------------------

    #[test]
    fn is_psk_like_covers_personal_modes_only() {
        assert!(is_psk_like(&SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(passphrase("p")),
        }));
        assert!(is_psk_like(&SecurityConfig::Wpa3Personal {
            passphrase: passphrase("p"),
        }));
        assert!(is_psk_like(&SecurityConfig::Wpa2Wpa3Personal {
            passphrase: passphrase("p"),
        }));
        assert!(!is_psk_like(&SecurityConfig::Open));
        assert!(!is_psk_like(&SecurityConfig::Owe));
        // Enterprise has its own EAP-failure path; not PSK-like.
        let eap = nexus_profile_store::Dot1xEapConfig {
            eap: nexus_profile_store::EapMethod::Peap,
            identity: "u".into(),
            anonymous_identity: None,
            ca_cert: None,
            client_cert: None,
            client_key: None,
            client_key_password: None,
            phase2: None,
            domain_suffix_match: None,
            password: Some(passphrase("p")),
        };
        assert!(!is_psk_like(&SecurityConfig::Wpa2Enterprise(eap.clone())));
        assert!(!is_psk_like(&SecurityConfig::Wpa3Enterprise(eap)));
    }

    // ---- intrinsic_security_mode ----------------------------------------

    #[test]
    fn intrinsic_security_mode_matches_dd003_section_8_1() {
        assert_eq!(intrinsic_security_mode(&SecurityConfig::Open), SecurityMode::Open);
        assert_eq!(intrinsic_security_mode(&SecurityConfig::Owe), SecurityMode::Owe);
        assert_eq!(
            intrinsic_security_mode(&SecurityConfig::Wpa2Personal {
                psk: WpaPsk::Passphrase(passphrase("p")),
            }),
            SecurityMode::Wpa2Psk,
        );
        assert_eq!(
            intrinsic_security_mode(&SecurityConfig::Wpa3Personal {
                passphrase: passphrase("p"),
            }),
            SecurityMode::Wpa3Sae,
        );
        assert_eq!(
            intrinsic_security_mode(&SecurityConfig::Wpa2Wpa3Personal {
                passphrase: passphrase("p"),
            }),
            SecurityMode::Wpa2Wpa3Transition,
        );
    }

    // ---- resolve_connected_security (C1) --------------------------------

    #[test]
    fn resolve_security_prefers_bss_advertised_compatible_mode() {
        // Profile says WPA2-Personal; BSS advertises both
        // transition mode and SAE — resolver should pick the
        // first compatible advertised mode (transition is
        // accepted by Wpa2Personal).
        let bss = bss(
            [0xAA; 6],
            b"corp",
            vec![SecurityMode::Wpa2Wpa3Transition, SecurityMode::Wpa3Sae],
        );
        let profile_security = SecurityConfig::Wpa2Personal {
            psk: WpaPsk::Passphrase(passphrase("p")),
        };
        assert_eq!(
            resolve_connected_security(Some(&bss), Some(&profile_security)),
            SecurityMode::Wpa2Wpa3Transition,
        );
    }

    #[test]
    fn resolve_security_falls_back_to_intrinsic_when_cache_cold() {
        // No BSS in cache yet; rely on the profile.
        let profile_security = SecurityConfig::Wpa3Personal {
            passphrase: passphrase("p"),
        };
        assert_eq!(
            resolve_connected_security(None, Some(&profile_security)),
            SecurityMode::Wpa3Sae,
        );
    }

    #[test]
    fn resolve_security_defaults_to_wpa2_psk_when_neither_signal() {
        assert_eq!(
            resolve_connected_security(None, None),
            SecurityMode::Wpa2Psk,
        );
    }

    // ---- map_disconnect (covers every DisconnectHint variant) -----------

    #[test]
    fn map_disconnect_covers_every_hint_variant() {
        use crate::supplicant::DisconnectHint as H;
        assert!(matches!(
            map_disconnect(H::Unspecified),
            DisconnectReason::Unspecified
        ));
        assert!(matches!(
            map_disconnect(H::AuthFailure),
            DisconnectReason::AuthExpired
        ));
        assert!(matches!(
            map_disconnect(H::HandshakeTimeout),
            DisconnectReason::HandshakeTimeout
        ));
        assert!(matches!(
            map_disconnect(H::EapFailure),
            DisconnectReason::EapFailure
        ));
        assert!(matches!(
            map_disconnect(H::ApInitiated),
            DisconnectReason::ApInitiated
        ));
        assert!(matches!(
            map_disconnect(H::Inactivity),
            DisconnectReason::Inactivity
        ));
        assert!(matches!(
            map_disconnect(H::ProtocolError),
            DisconnectReason::ProtocolError
        ));
        assert!(matches!(
            map_disconnect(H::BadCredentials),
            DisconnectReason::CredentialsInvalid
        ));
        assert!(matches!(
            map_disconnect(H::LocalRequest),
            DisconnectReason::LocalRequest
        ));
        assert!(matches!(
            map_disconnect(H::DaemonUnavailable),
            DisconnectReason::SupplicantUnavailable
        ));
        // Sanity: the only reason that's_permanent across the
        // map is BadCredentials → CredentialsInvalid.
        assert!(map_disconnect(H::BadCredentials).is_permanent());
        assert!(!map_disconnect(H::HandshakeTimeout).is_permanent());
    }
}
