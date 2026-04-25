//! The GNSS Backend orchestrator. See DD-005 §§5-7, 10-11.
//!
//! One task multiplexes:
//!   - the NexusEvent bus (interface events, raw TPV / SKY events,
//!     gpsd connect / disconnect markers);
//!   - a small command channel (operator-driven calls — currently
//!     just `SetPowerState`, with room for future D-Bus methods);
//!   - a 1 Hz reconcile tick that handles acquisition / stall
//!     timeouts, gpsd reconnect backoff, and outage notifications.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{InterfaceInfo, InterfaceKind, NexusEvent, NotificationData, SatInfo};
use nexus_profile_store::ProfileStore;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::config::{GnssConfig, PowerState};
use crate::errors::Result;
use crate::fix::{GnssFix, fix_quality_ok};
use crate::gpsd::GpsdClient;
use crate::lifecycle::{GnssDeviceEntry, Outcome, check_timeouts, should_emit, tpv_next_state};
use crate::metrics as m;
use crate::profile::hydrate;

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Operator-driven commands. The D-Bus surface (DD-006) translates
/// `SetPowerState` into this variant; additional operator actions
/// are added in later DD revisions.
#[derive(Debug)]
pub enum GnssCommand {
    SetPowerState {
        state: PowerState,
        responder: oneshot::Sender<Result<()>>,
    },
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

pub struct GnssBackend {
    devices: HashMap<u32, GnssDeviceEntry>,
    /// Secondary index `device_path → ifindex`. Maintained on
    /// discover / remove so per-TPV `device_by_path[_mut]` lookups
    /// are O(1) instead of scanning every entry.
    path_index: HashMap<String, u32>,
    gpsd: Arc<dyn GpsdClient>,
    profile_store: Arc<dyn ProfileStore>,
    event_tx: broadcast::Sender<NexusEvent>,
    event_rx: broadcast::Receiver<NexusEvent>,
    cmd_rx: mpsc::Receiver<GnssCommand>,
    cmd_tx: mpsc::Sender<GnssCommand>,
    config: GnssConfig,
    power_state: PowerState,
    first_outage_at: Option<Instant>,
    outage_notified: bool,
    last_reconnect_attempt: Option<Instant>,
    reconnect_attempts: u32,
}

impl GnssBackend {
    pub fn new(
        gpsd: Arc<dyn GpsdClient>,
        profile_store: Arc<dyn ProfileStore>,
        event_tx: broadcast::Sender<NexusEvent>,
        cmd_tx: mpsc::Sender<GnssCommand>,
        cmd_rx: mpsc::Receiver<GnssCommand>,
        config: GnssConfig,
    ) -> Self {
        let event_rx = event_tx.subscribe();
        Self {
            devices: HashMap::new(),
            path_index: HashMap::new(),
            gpsd,
            profile_store,
            event_tx,
            event_rx,
            cmd_rx,
            cmd_tx,
            config,
            power_state: PowerState::Active,
            first_outage_at: None,
            outage_notified: false,
            last_reconnect_attempt: None,
            reconnect_attempts: 0,
        }
    }

    pub fn cmd_tx(&self) -> mpsc::Sender<GnssCommand> {
        self.cmd_tx.clone()
    }

    /// Drive the loop until `shutdown` fires or every input
    /// channel closes.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        let mut reconcile = tokio::time::interval(Duration::from_secs(1));
        reconcile.tick().await; // skip the immediate first fire

        // Initial gpsd connection attempt. Failure is fine — the
        // reconcile tick retries with backoff.
        if let Err(e) = self.gpsd.connect().await {
            warn!(error = %e, "initial gpsd connect failed; will retry");
        }

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    info!("gnss backend shutting down");
                    return Ok(());
                }
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(cmd) => self.handle_command(cmd).await,
                    None => return Ok(()),
                },
                res = self.event_rx.recv() => match res {
                    Ok(event) => {
                        if let Err(e) = self.handle_event(event).await {
                            warn!(error = %e, "gnss event handler error");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "gnss event receiver lagged");
                    }
                },
                _ = reconcile.tick() => {
                    self.reconcile().await;
                }
            }
            self.refresh_metrics();
        }
    }

    // -----------------------------------------------------------------
    // Event dispatch — DD-005 §5.2
    // -----------------------------------------------------------------

    async fn handle_event(&mut self, event: NexusEvent) -> Result<()> {
        match event {
            NexusEvent::InterfaceDiscovered(info)
                if matches!(info.kind, InterfaceKind::Gnss { .. }) =>
            {
                self.on_interface_discovered(info).await?;
            }
            NexusEvent::InterfaceRemoved { ifindex } if self.devices.contains_key(&ifindex) => {
                self.on_interface_removed(ifindex).await;
            }
            NexusEvent::GnssTpvReceived { device, fix } => {
                self.on_tpv(&device, fix);
            }
            NexusEvent::GnssSatellites { device, satellites } => {
                self.on_satellites(&device, satellites);
            }
            NexusEvent::GnssGpsdConnected => {
                m::set_gpsd_connected(true);
                self.on_gpsd_connected().await;
            }
            NexusEvent::GnssGpsdDisconnected => {
                m::set_gpsd_connected(false);
                self.on_gpsd_disconnected();
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_interface_discovered(&mut self, info: InterfaceInfo) -> Result<()> {
        let (device_path, vendor_model) = match &info.kind {
            InterfaceKind::Gnss {
                device_path,
                vendor_model,
                ..
            } => (device_path.clone(), vendor_model.clone()),
            _ => return Ok(()),
        };
        let stored = self
            .profile_store
            .load_gnss_profile_by_path(&device_path)
            .await
            .ok()
            .flatten();
        let mut profile = hydrate(&device_path, stored.as_ref(), &self.config.defaults);
        if profile.label.is_none() {
            profile.label = vendor_model;
        }

        let ifindex = info.ifindex;
        let entry = GnssDeviceEntry::new(info, profile.clone(), Instant::now());
        self.devices.insert(ifindex, entry);
        self.path_index.insert(device_path.clone(), ifindex);
        info!(ifindex, %device_path, profiled = stored.is_some(), "gnss device discovered");

        // Ask gpsd to watch the device if the effective profile
        // opts in. Failure is non-fatal — reconnect re-registers.
        if profile.auto_activate && self.gpsd.is_connected() {
            if let Err(e) = self.gpsd.add_device(&device_path).await {
                warn!(%device_path, error = %e, "gpsd add_device failed; will retry on reconnect");
            }
        }
        Ok(())
    }

    async fn on_interface_removed(&mut self, ifindex: u32) {
        let Some(entry) = self.devices.remove(&ifindex) else {
            return;
        };
        let device_path = entry.device_path().to_owned();
        self.path_index.remove(&device_path);
        if self.gpsd.is_connected() {
            let _ = self.gpsd.remove_device(&device_path).await;
        }
        debug!(%device_path, "gnss device removed");
    }

    fn on_tpv(&mut self, device_path: &str, fix: GnssFix) {
        // In Sleep mode we skip state updates entirely — the stall
        // timer stays frozen so a long sleep doesn't degrade a
        // tracking device on wake. DD-005 §10.
        let suspended = matches!(self.power_state, PowerState::Sleep);
        // Capture before the mutable borrow of self.devices below,
        // so should_emit can see the active power state.
        let power_state = self.power_state;

        let mut emit_payload: Option<GnssFix> = None;
        let mut stall_transition = false;
        let mut filter_reason: Option<&'static str> = None;
        let mut suppress_reason: Option<&'static str> = None;
        let mut state_change: Option<(&'static str, &'static str, &'static str)> = None;
        let device_for_metrics;
        let mode_label;

        {
            let Some(entry) = self.device_by_path_mut(device_path) else {
                return; // TPV for a device we don't know
            };
            device_for_metrics = entry.device_path().to_owned();
            mode_label = m::mode_label(fix.mode);

            if suspended {
                // Freeze last_tpv_at so the stall timer restarts
                // fresh on wake.
                return;
            }

            let now = Instant::now();
            entry.last_tpv_at = Some(now);

            let quality = fix_quality_ok(&fix, &entry.profile);
            let passes = quality.is_ok();
            if let Err(reason) = &quality {
                filter_reason = Some(reason.as_str());
            }

            let previous_label = entry.state.label();
            entry.state = tpv_next_state(&entry.state, &fix, passes, now);
            let new_label = entry.state.label();
            if previous_label == "tracking" && new_label == "degraded" {
                stall_transition = true;
            }
            if previous_label != new_label {
                let reason = if passes { "fix_passed" } else { "fix_failed" };
                info!(
                    device = %device_for_metrics,
                    from = previous_label,
                    to = new_label,
                    reason,
                    "gnss transition"
                );
                state_change = Some((previous_label, new_label, reason));
            }

            if !passes {
                // Quality-failing fixes never emit.
            } else {
                match should_emit(entry, &fix, now, power_state) {
                    Outcome::Emit => {
                        entry.last_fix_emit_at = Some(now);
                        entry.last_emitted_fix = Some(fix.clone());
                        emit_payload = Some(fix.clone());
                    }
                    Outcome::Suppressed(r) => {
                        suppress_reason = Some(r.as_str());
                    }
                }
            }

            if let Some(eph) = fix.horizontal_error_m {
                m::set_horizontal_error(&device_for_metrics, eph);
            }
            m::set_satellites_used(&device_for_metrics, fix.satellites_used as u64);
        }

        m::record_tpv(&device_for_metrics, mode_label);
        if let Some(r) = filter_reason {
            m::record_fix_filtered(&device_for_metrics, r);
        }
        if let Some(r) = suppress_reason {
            m::record_emission_suppressed(&device_for_metrics, r);
        }
        if stall_transition {
            m::record_tpv_stall(&device_for_metrics);
        }

        if let Some((from, to, reason)) = state_change {
            let _ = self.event_tx.send(NexusEvent::GnssStateChanged {
                device: device_for_metrics.clone(),
                from,
                to,
                reason,
            });
        }

        if let Some(fix) = emit_payload {
            let _ = self.event_tx.send(NexusEvent::GnssFixChanged {
                device: device_for_metrics,
                fix,
            });
        }
    }

    fn on_satellites(&mut self, device_path: &str, satellites: Vec<SatInfo>) {
        if let Some(entry) = self.device_by_path_mut(device_path) {
            m::set_satellites_in_view(device_path, satellites.len() as u64);
            entry.last_satellites = satellites;
        }
    }

    async fn on_gpsd_connected(&mut self) {
        info!("gpsd connected; re-registering devices");
        self.reregister_devices().await;
        if self.first_outage_at.is_some() && self.outage_notified {
            self.emit_notification("subsystem_recovered", &[("subsystem", "gpsd")]);
        }
        self.first_outage_at = None;
        self.outage_notified = false;
        self.reconnect_attempts = 0;
    }

    fn on_gpsd_disconnected(&mut self) {
        if self.first_outage_at.is_none() {
            self.first_outage_at = Some(Instant::now());
        }
        warn!("gpsd disconnected; supervisor will retry");
    }

    async fn reregister_devices(&mut self) {
        let to_register: Vec<String> = self
            .devices
            .values()
            .filter(|e| e.profile.auto_activate)
            .map(|e| e.device_path().to_owned())
            .collect();
        for path in to_register {
            if let Err(e) = self.gpsd.add_device(&path).await {
                warn!(device = %path, error = %e, "add_device on reconnect failed");
            }
        }
    }

    // -----------------------------------------------------------------
    // Command dispatch
    // -----------------------------------------------------------------

    async fn handle_command(&mut self, cmd: GnssCommand) {
        match cmd {
            GnssCommand::SetPowerState { state, responder } => {
                let _ = responder.send(self.apply_power_state(state));
            }
        }
    }

    /// DD-005 §10. Sleep suspends TPV-stall detection + emission;
    /// Background applies a 5 s emission floor on top of the
    /// per-device profile's `max_update_hz` (enforced inside
    /// `should_emit` via the `power_state` argument). Active
    /// restores profile values.
    fn apply_power_state(&mut self, next: PowerState) -> Result<()> {
        if self.power_state == next {
            return Ok(());
        }
        let previous = self.power_state;
        self.power_state = next;
        info!(
            from = previous.as_str(),
            to = next.as_str(),
            "gnss power state"
        );

        // On wake, reset the stall timer so a long sleep doesn't
        // instantly degrade a tracking device.
        if previous == PowerState::Sleep && next != PowerState::Sleep {
            let now = Instant::now();
            for entry in self.devices.values_mut() {
                entry.last_tpv_at = Some(now);
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Supervisor — DD-005 §6.4
    // -----------------------------------------------------------------

    async fn reconcile(&mut self) {
        let now = Instant::now();

        // Sleep suspends timeout checks: a long sleep would otherwise
        // instantly declare every device `Degraded` on wake.
        if !matches!(self.power_state, PowerState::Sleep) {
            self.check_all_timeouts(now);
        }

        if self.gpsd.is_connected() {
            self.reconnect_attempts = 0;
            return;
        }

        let backoff = std::cmp::min(
            Duration::from_secs(1 << self.reconnect_attempts.min(5)),
            Duration::from_secs(30),
        );
        if self
            .last_reconnect_attempt
            .is_none_or(|t| t.elapsed() >= backoff)
        {
            self.last_reconnect_attempt = Some(now);
            self.reconnect_attempts += 1;
            m::record_gpsd_reconnect();
            if let Err(e) = self.gpsd.connect().await {
                warn!(error = %e, "gpsd reconnect failed");
            }
        }

        if let Some(t) = self.first_outage_at {
            if t.elapsed().as_secs() >= self.config.gpsd_outage_notify_s as u64
                && !self.outage_notified
            {
                let duration = t.elapsed().as_secs().to_string();
                self.outage_notified = true;
                self.emit_notification(
                    "subsystem_unavailable",
                    &[("subsystem", "gpsd"), ("duration_s", &duration)],
                );
            }
        }
    }

    fn check_all_timeouts(&mut self, now: Instant) {
        let acq = Duration::from_secs(self.config.acquisition_timeout_s as u64);
        let stall = Duration::from_secs(self.config.tpv_stall_timeout_s as u64);
        let mut transitions: Vec<(String, &'static str, &'static str)> = Vec::new();
        for entry in self.devices.values_mut() {
            if let Some(next) = check_timeouts(&entry.state, entry.last_tpv_at, acq, stall, now) {
                let previous = entry.state.label();
                let to_label = next.label();
                let stalled = previous == "tracking" && to_label == "degraded";
                entry.state = next;
                info!(
                    device = %entry.device_path(),
                    from = previous,
                    to = to_label,
                    reason = "timeout",
                    "gnss transition"
                );
                if stalled {
                    m::record_tpv_stall(entry.device_path());
                }
                transitions.push((entry.device_path().to_owned(), previous, to_label));
            }
        }
        for (device, from, to) in transitions {
            let _ = self.event_tx.send(NexusEvent::GnssStateChanged {
                device,
                from,
                to,
                reason: "timeout",
            });
        }
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    fn device_by_path_mut(&mut self, device_path: &str) -> Option<&mut GnssDeviceEntry> {
        let ifindex = *self.path_index.get(device_path)?;
        self.devices.get_mut(&ifindex)
    }

    /// Immutable variant — used by tests (and future diagnostic
    /// paths).
    #[allow(dead_code)]
    fn device_by_path(&self, device_path: &str) -> Option<&GnssDeviceEntry> {
        let ifindex = *self.path_index.get(device_path)?;
        self.devices.get(&ifindex)
    }

    fn emit_notification(&self, kind: &str, fields: &[(&str, &str)]) {
        let mut data = NotificationData::default();
        for (k, v) in fields {
            data.insert((*k).to_owned(), (*v).to_owned());
        }
        let _ = self.event_tx.send(NexusEvent::OperatorNotification {
            kind: kind.to_owned(),
            data,
        });
    }

    fn refresh_metrics(&self) {
        m::set_devices(self.devices.len() as u64);
        for entry in self.devices.values() {
            for label in m::STATE_LABELS {
                m::set_state(entry.device_path(), label, entry.state.label() == *label);
            }
        }
    }
}
