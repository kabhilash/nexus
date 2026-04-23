//! The Bluetooth Backend orchestrator. See DD-004 §§7, 8, 12, 13.
//!
//! Multiplexes between the event bus (DD-001 `InterfaceDiscovered`
//! / `InterfaceRemoved` + BlueZ's `BtAdapterChanged` /
//! `BtDeviceDiscovered` / … variants synthesized by the BlueZ
//! client) and a command channel (operator-driven calls from the
//! D-Bus layer, the registered Agent, and internal driver tasks).
//!
//! Pairing runs concurrently with the main loop: [`BtCommand::Pair`]
//! transitions the device state, spawns a driver that awaits
//! [`BluezClient::pair`], and the result arrives asynchronously as
//! [`NexusEvent::BtPairingComplete`]. The Agent (`crate::agent`)
//! coordinates BlueZ's per-callback prompts via the
//! [`BtCommand::RegisterPromptOneshot`] +
//! [`BtCommand::AnswerPairingPrompt`] pair, so operator responses
//! resolve through the same code path whether they come from a
//! D-Bus method handler or an auto-accept profile policy.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{
    BluetoothAddrExt, BtDeviceInfo, BtFailureReason, BtTransport, InterfaceKind, MacAddr,
    NexusEvent, NotificationData, PairingAnswer, PairingJobId, PairingPromptData,
    PairingPromptKind,
};
use nexus_profile_store::{BluetoothProfile, ProfileMetadata, ProfileStore};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};
use ulid::Ulid;

use crate::adapter::{self, AdapterSignal};
use crate::bluez::BluezClient;
use crate::device::{self, DeviceSignal};
use crate::errors::{BtError, Result};
use crate::metrics as m;
use crate::pairing::{self, build_prompt_notification, classify_pair_error};
use crate::types::{
    AuthorizationDecision, BtAdapterEntry, BtDeviceEntry, BtDeviceState, DiscoveryFilter,
    PowerState, initial_device_state,
};

// ---------------------------------------------------------------------------
// Commands — DD-004 §7.2
// ---------------------------------------------------------------------------

/// Commands other tasks send the backend. D-Bus method handlers,
/// the registered Agent, and operator-facing D-Bus surface (DD-006)
/// all funnel their requests through this channel.
#[derive(Debug)]
pub enum BtCommand {
    Pair {
        device_path: String,
        responder: oneshot::Sender<Result<PairingJobId>>,
    },
    Connect {
        device_path: String,
        responder: oneshot::Sender<Result<()>>,
    },
    Disconnect {
        device_path: String,
        responder: oneshot::Sender<Result<()>>,
    },
    Forget {
        adapter: String,
        device_path: String,
        responder: oneshot::Sender<Result<()>>,
    },
    SetAdapterPowered {
        adapter: String,
        on: bool,
        responder: oneshot::Sender<Result<()>>,
    },
    SetAdapterDiscoverable {
        adapter: String,
        on: bool,
        responder: oneshot::Sender<Result<()>>,
    },
    SetAdapterPairable {
        adapter: String,
        on: bool,
        responder: oneshot::Sender<Result<()>>,
    },
    StartDiscovery {
        adapter: String,
        filter: DiscoveryFilter,
        responder: oneshot::Sender<Result<()>>,
    },
    StopDiscovery {
        adapter: String,
        responder: oneshot::Sender<Result<()>>,
    },
    CancelPairing {
        device_path: String,
        responder: oneshot::Sender<Result<()>>,
    },

    /// Operator response to an Agent prompt. The backend looks up
    /// the pending oneshot in `pending_prompt_answers` and
    /// resolves it, which wakes the Agent method so it can return
    /// the answer to BlueZ.
    AnswerPairingPrompt {
        job_id: PairingJobId,
        answer: PairingAnswer,
        responder: oneshot::Sender<Result<()>>,
    },

    // ---- Agent-driven ----
    /// Agent queries the backend's in-flight pairing job for a
    /// device. Used at the top of every Agent method so the
    /// callback correlates with the operator's Pair() call.
    LookupPairingJob {
        device_path: String,
        responder: oneshot::Sender<Option<PairingJobId>>,
    },

    /// Agent asks whether an incoming authorization can bypass
    /// the operator prompt based on the device's stored profile.
    LookupAuthorizationPolicy {
        device_path: String,
        service_uuid: Option<String>,
        responder: oneshot::Sender<AuthorizationDecision>,
    },

    /// Agent deposits a oneshot sender the backend resolves when
    /// the operator calls `AnswerPairingPrompt`. The backend emits
    /// `BtPairingPrompt` and `OperatorNotification` at the same
    /// time, so the operator UI can surface the prompt.
    RegisterPromptOneshot {
        job_id: PairingJobId,
        sender: oneshot::Sender<PairingAnswer>,
        kind: PairingPromptKind,
        data: PairingPromptData,
    },

    /// Operator-driven power-state transition. See DD-004 §12.
    SetPowerState {
        state: PowerState,
        responder: oneshot::Sender<Result<()>>,
    },
}

// ---------------------------------------------------------------------------
// Configuration — DD-004 §10
// ---------------------------------------------------------------------------

/// Backend-tunable knobs.
#[derive(Debug, Clone)]
pub struct BluetoothConfig {
    pub pairing_timeout_s: u32,
    pub agent_response_timeout_s: u32,
    pub discovery_timeout_s: u32,
    pub discovery_device_ttl_s: u32,
    pub bluez_outage_notify_s: u32,
    pub auto_power_on_startup: bool,
    pub default_discovery_filter: DiscoveryFilter,
    /// Whether to register Nexus as the BlueZ Agent at startup.
    /// If false, some other process (e.g. `bluetoothctl`) owns the
    /// Agent role and pairing prompts go there instead.
    pub register_agent: bool,
}

impl Default for BluetoothConfig {
    fn default() -> Self {
        Self {
            pairing_timeout_s: 60,
            agent_response_timeout_s: 45,
            discovery_timeout_s: 30,
            discovery_device_ttl_s: 300,
            bluez_outage_notify_s: 60,
            auto_power_on_startup: true,
            default_discovery_filter: DiscoveryFilter {
                transport: None,
                rssi: Some(-90),
                uuids: Vec::new(),
                duplicate_data: false,
            },
            register_agent: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Backend struct + event loop
// ---------------------------------------------------------------------------

/// Entry in `pending_prompt_answers`: the oneshot the Agent is
/// waiting on, paired with the prompt kind the operator must match.
struct PendingPrompt {
    kind: PairingPromptKind,
    sender: oneshot::Sender<PairingAnswer>,
}

/// In-memory bookkeeping for the backend. Owned by the main task;
/// accessed only from there.
pub struct BluetoothBackend {
    adapters: HashMap<u32, BtAdapterEntry>,
    bluez: Arc<dyn BluezClient>,
    profile_store: Arc<dyn ProfileStore>,
    event_tx: broadcast::Sender<NexusEvent>,
    event_rx: broadcast::Receiver<NexusEvent>,
    cmd_rx: mpsc::Receiver<BtCommand>,
    cmd_tx: mpsc::Sender<BtCommand>,
    config: BluetoothConfig,
    /// Pairing-prompt oneshot senders the Agent deposits via
    /// `RegisterPromptOneshot`. Keyed by the pairing job. See
    /// DD-004 §7.2's assumption: at most one outstanding prompt
    /// per job at any time. The stored [`PairingPromptKind`] lets
    /// `AnswerPairingPrompt` validate the operator's answer variant
    /// against DD-006 §6.4's per-kind map.
    pending_prompt_answers: HashMap<PairingJobId, PendingPrompt>,
    /// Current power state. Defaults to `Active`.
    power_state: PowerState,
    first_bluez_outage_at: Option<Instant>,
    outage_notified: bool,
    last_reconnect_attempt: Option<Instant>,
    reconnect_attempts: u32,
}

impl BluetoothBackend {
    pub fn new(
        bluez: Arc<dyn BluezClient>,
        profile_store: Arc<dyn ProfileStore>,
        event_tx: broadcast::Sender<NexusEvent>,
        cmd_tx: mpsc::Sender<BtCommand>,
        cmd_rx: mpsc::Receiver<BtCommand>,
        config: BluetoothConfig,
    ) -> Self {
        let event_rx = event_tx.subscribe();
        Self {
            adapters: HashMap::new(),
            bluez,
            profile_store,
            event_tx,
            event_rx,
            cmd_rx,
            cmd_tx,
            config,
            pending_prompt_answers: HashMap::new(),
            power_state: PowerState::Active,
            first_bluez_outage_at: None,
            outage_notified: false,
            last_reconnect_attempt: None,
            reconnect_attempts: 0,
        }
    }

    /// Hand out a clone of the command sender so D-Bus handlers
    /// and the Agent task can issue commands.
    pub fn cmd_tx(&self) -> mpsc::Sender<BtCommand> {
        self.cmd_tx.clone()
    }

    /// Drive the loop until `shutdown` is cancelled or every input
    /// channel closes.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        let mut reconcile = tokio::time::interval(Duration::from_secs(1));
        reconcile.tick().await; // swallow the immediate first tick

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    info!("bluetooth backend shutting down");
                    return Ok(());
                }
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(cmd) => self.handle_command(cmd).await,
                    None => return Ok(()),
                },
                res = self.event_rx.recv() => match res {
                    Ok(event) => {
                        if let Err(e) = self.handle_event(event).await {
                            warn!(error = %e, "bluetooth event handler error");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(lagged = n, "bluetooth event receiver lagged");
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
    // NexusEvent dispatch
    // -----------------------------------------------------------------

    async fn handle_event(&mut self, event: NexusEvent) -> Result<()> {
        match event {
            NexusEvent::InterfaceDiscovered(info)
                if matches!(info.kind, InterfaceKind::Bluetooth { .. }) =>
            {
                self.on_interface_discovered(info);
            }
            NexusEvent::InterfaceRemoved { ifindex } if self.adapters.contains_key(&ifindex) => {
                self.on_interface_removed(ifindex);
            }
            NexusEvent::BluezConnected => {
                m::set_bluez_connected(true);
                info!("bluez connected; awaiting object-manager republish");
            }
            NexusEvent::BluezDisconnected => {
                m::set_bluez_connected(false);
                self.on_bluez_disconnected();
            }
            NexusEvent::BtAdapterChanged {
                adapter,
                powered,
                discovering,
            } => {
                self.on_adapter_props(&adapter, powered, discovering);
            }
            NexusEvent::BtDeviceDiscovered(info) => {
                self.on_device_added(info).await?;
            }
            NexusEvent::BtDeviceConnected { adapter, address } => {
                self.on_device_connected(&adapter, address);
            }
            NexusEvent::BtDeviceDisconnected { adapter, address } => {
                self.on_device_disconnected(&adapter, address);
            }
            NexusEvent::BtPairingComplete {
                job_id,
                success,
                reason,
            } => {
                self.on_pairing_complete(job_id, success, reason).await;
            }
            _ => {}
        }
        Ok(())
    }

    fn on_interface_discovered(&mut self, info: nexus_core::InterfaceInfo) {
        let (bluez_path, hci_name) = match &info.kind {
            InterfaceKind::Bluetooth {
                bluez_path,
                hci_name,
                ..
            } => (bluez_path.clone(), hci_name.clone()),
            _ => return,
        };
        let ifindex = info.ifindex;
        info!(ifindex, %hci_name, %bluez_path, "bt adapter discovered");
        let entry = BtAdapterEntry::new(info, bluez_path);
        self.adapters.insert(ifindex, entry);
    }

    fn on_interface_removed(&mut self, ifindex: u32) {
        let Some(entry) = self.adapters.remove(&ifindex) else {
            return;
        };
        let pending_jobs: Vec<PairingJobId> = entry
            .devices
            .values()
            .filter_map(|d| d.pairing_job)
            .collect();
        for dev in entry.devices.values() {
            if matches!(
                dev.state,
                BtDeviceState::Connected { .. } | BtDeviceState::Connecting { .. }
            ) {
                let _ = self.event_tx.send(NexusEvent::BtDeviceDisconnected {
                    adapter: entry.bluez_path.clone(),
                    address: dev.info.address,
                });
            }
        }
        for job in pending_jobs {
            if let Some(pending) = self.pending_prompt_answers.remove(&job) {
                let _ = pending.sender.send(PairingAnswer::Cancel);
            }
        }
    }

    fn on_bluez_disconnected(&mut self) {
        for entry in self.adapters.values_mut() {
            entry.state =
                adapter::next_state(&entry.state, AdapterSignal::BluezLost, Instant::now());
            // Cancel pairings tied to devices on this adapter.
            for dev in entry.devices.values() {
                if let Some(job) = dev.pairing_job {
                    if let Some(pending) = self.pending_prompt_answers.remove(&job) {
                        let _ = pending.sender.send(PairingAnswer::Cancel);
                    }
                }
            }
            entry.devices.clear();
            entry.nexus_has_discovery_session = false;
            entry.discovery_started_at = None;
        }
        if self.first_bluez_outage_at.is_none() {
            self.first_bluez_outage_at = Some(Instant::now());
        }
    }

    fn on_adapter_props(&mut self, bluez_path: &str, powered: bool, discovering: bool) {
        let Some(entry) = self.adapter_by_bluez_path_mut(bluez_path) else {
            return;
        };
        let previous_label = entry.state.label();
        let next = adapter::next_state(
            &entry.state,
            AdapterSignal::PropsChanged {
                powered,
                discovering,
            },
            Instant::now(),
        );
        if next.label() != previous_label {
            debug!(
                adapter = %bluez_path,
                from = previous_label,
                to = next.label(),
                "bt adapter transition"
            );
        }
        entry.state = next;
        entry.powered = powered;
    }

    async fn on_device_added(&mut self, info: BtDeviceInfo) -> Result<()> {
        // Load a matching profile before mutating, so we can decide
        // on an auto-connect before the .await boundaries steal a
        // borrow.
        let profile = self
            .profile_store
            .load_bluetooth_profile_by_address(&info.address)
            .await
            .ok()
            .flatten();

        let Some(adapter_entry) = self.adapter_by_bluez_path_mut(&info.adapter) else {
            return Ok(());
        };
        let address = info.address;
        let device_path = info.device_path.clone();
        let state = initial_device_state(&info);
        let entry = adapter_entry
            .devices
            .entry(device_path.clone())
            .or_insert_with(|| BtDeviceEntry::new(info.clone(), state.clone()));
        entry.info = info.clone();
        entry.state = state.clone();
        entry.profile = profile.clone();
        entry.last_seen = Instant::now();

        debug!(
            adapter = %info.adapter,
            address = %address.to_bluez(),
            profiled = profile.is_some(),
            "device added"
        );

        // Auto-connect: only fire when we have a profile opting in,
        // the device reports Paired, it isn't blocked, and we're
        // not in the Sleep power state.
        let auto_connect = profile
            .as_ref()
            .map(|p| p.auto_connect && !info.blocked)
            .unwrap_or(false);
        let allowed_by_power = matches!(
            self.power_state,
            PowerState::Active | PowerState::Background
        );
        if auto_connect && allowed_by_power && matches!(state, BtDeviceState::Paired) {
            let (tx, _rx) = oneshot::channel();
            let _ = self
                .cmd_tx
                .send(BtCommand::Connect {
                    device_path,
                    responder: tx,
                })
                .await;
            info!(
                address = %address.to_bluez(),
                "auto-connect: queued Connect command from stored profile"
            );
        }
        Ok(())
    }

    fn on_device_connected(&mut self, adapter: &str, address: MacAddr) {
        if let Some(dev) = self.device_by_address_mut(adapter, &address) {
            dev.info.connected = true;
            dev.last_seen = Instant::now();
            let services = dev.info.uuids.clone();
            dev.state = device::next_state(
                &dev.state,
                DeviceSignal::PropsConnectedTrue { services },
                Instant::now(),
            );
            m::record_connection(adapter, m::connect_outcome::SUCCESS);
        }
    }

    fn on_device_disconnected(&mut self, adapter: &str, address: MacAddr) {
        if let Some(dev) = self.device_by_address_mut(adapter, &address) {
            dev.info.connected = false;
            dev.last_seen = Instant::now();
            let paired = dev.info.paired;
            dev.state = device::next_state(
                &dev.state,
                DeviceSignal::PropsConnectedFalse { paired },
                Instant::now(),
            );
        }
    }

    // -----------------------------------------------------------------
    // Pairing — DD-004 §§7.2, 8
    // -----------------------------------------------------------------

    /// Start pairing. Transitions the device to `Pairing`, emits
    /// `BtPairingStarted`, and spawns a driver that awaits
    /// `BluezClient::pair()`. Returns the `job_id` immediately.
    async fn start_pairing(&mut self, device_path: &str) -> Result<PairingJobId> {
        let Some(dev) = self.device_by_path_mut(device_path) else {
            return Err(BtError::UnknownDevice(device_path.to_owned()));
        };
        if matches!(dev.state, BtDeviceState::Pairing { .. }) {
            return Err(BtError::AlreadyPairing);
        }
        let job_id = pairing::new_job_id();
        dev.state = device::next_state(
            &dev.state,
            DeviceSignal::OperatorPair { job_id },
            Instant::now(),
        );
        dev.pairing_job = Some(job_id);

        let _ = self.event_tx.send(NexusEvent::BtPairingStarted {
            job_id,
            device: device_path.to_owned(),
        });

        // Spawn the driver. Arc<dyn BluezClient> lets it call pair()
        // without a borrow-checker fight; the trait's &self
        // signatures make this safe.
        let bluez = Arc::clone(&self.bluez);
        let event_tx = self.event_tx.clone();
        let device_path_owned = device_path.to_owned();
        let pairing_timeout = Duration::from_secs(self.config.pairing_timeout_s as u64);
        tokio::spawn(async move {
            let outcome =
                match tokio::time::timeout(pairing_timeout, bluez.pair(&device_path_owned)).await {
                    Ok(inner) => inner,
                    Err(_) => Err(BtError::Bluez(
                        "org.bluez.Error.AuthenticationTimeout: pair timeout".into(),
                    )),
                };
            let (success, reason) = match &outcome {
                Ok(()) => (true, None),
                Err(e) => (false, Some(classify_pair_error(e))),
            };
            let _ = event_tx.send(NexusEvent::BtPairingComplete {
                job_id,
                success,
                reason,
            });
        });

        Ok(job_id)
    }

    async fn on_pairing_complete(
        &mut self,
        job_id: PairingJobId,
        success: bool,
        reason: Option<BtFailureReason>,
    ) {
        // Discard any still-pending prompt entry.
        self.pending_prompt_answers.remove(&job_id);

        // Find the device whose pairing_job matches.
        let Some((adapter_path, device_path)) = self.adapters.values().find_map(|adapter| {
            adapter
                .devices
                .iter()
                .find(|(_, e)| e.pairing_job == Some(job_id))
                .map(|(path, _)| (adapter.bluez_path.clone(), path.clone()))
        }) else {
            // Device removed mid-pair — nothing to finalize.
            return;
        };

        let new_state = if success {
            BtDeviceState::Paired
        } else {
            BtDeviceState::Failed {
                reason: reason
                    .clone()
                    .unwrap_or(BtFailureReason::Unknown("".into())),
                at: Instant::now(),
            }
        };

        // Grab the info snapshot we'll need for the profile write
        // before we release the mutable borrow on the adapter entry.
        let device_info = if let Some(dev) = self.device_by_path_mut(&device_path) {
            dev.state = new_state.clone();
            dev.pairing_job = None;
            dev.info.paired = success;
            if success {
                Some(dev.info.clone())
            } else {
                None
            }
        } else {
            None
        };

        let outcome_label = pair_outcome_label(success, reason.as_ref());
        m::record_pairing(&adapter_path, outcome_label);

        if !success {
            let mut data = NotificationData::default();
            data.insert("device", device_path.clone());
            if let Some(r) = &reason {
                data.insert("reason", format!("{r:?}"));
            }
            let _ = self.event_tx.send(NexusEvent::OperatorNotification {
                kind: "bluetooth_pairing_failed".to_owned(),
                data,
            });
            return;
        }

        // Success path: persist a profile and apply Trusted.
        if let Some(info) = device_info {
            let profile = build_profile(&info, &adapter_path);
            let trusted = profile.auto_connect;
            if let Err(e) = self.profile_store.put_bluetooth(&profile).await {
                warn!(
                    error = %e,
                    address = %info.address.to_bluez(),
                    "failed to persist bluetooth profile after pair"
                );
            } else if let Some(dev) = self.device_by_path_mut(&device_path) {
                dev.profile = Some(profile.clone());
            }
            if trusted {
                if let Err(e) = self.bluez.set_trusted(&device_path, true).await {
                    warn!(
                        error = %e,
                        device = %device_path,
                        "failed to set Trusted=true on BlueZ after pair"
                    );
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Command dispatch — DD-004 §7.2
    // -----------------------------------------------------------------

    async fn handle_command(&mut self, cmd: BtCommand) {
        match cmd {
            BtCommand::Pair {
                device_path,
                responder,
            } => {
                let result = self.start_pairing(&device_path).await;
                let _ = responder.send(result);
            }
            BtCommand::AnswerPairingPrompt {
                job_id,
                answer,
                responder,
            } => {
                let result = self.answer_pairing_prompt(job_id, answer);
                let _ = responder.send(result);
            }
            BtCommand::Connect {
                device_path,
                responder,
            } => {
                if let Some(dev) = self.device_by_path_mut(&device_path) {
                    dev.state = device::next_state(
                        &dev.state,
                        DeviceSignal::OperatorConnect,
                        Instant::now(),
                    );
                }
                let adapter_path = self.adapter_for_device(&device_path);
                let result = self.bluez.connect_device(&device_path).await;
                if let Some(adapter) = adapter_path {
                    let outcome = if result.is_ok() {
                        m::connect_outcome::SUCCESS
                    } else {
                        m::connect_outcome::FAILED
                    };
                    m::record_connection(&adapter, outcome);
                }
                let _ = responder.send(result);
            }
            BtCommand::Disconnect {
                device_path,
                responder,
            } => {
                if let Some(dev) = self.device_by_path_mut(&device_path) {
                    dev.state = device::next_state(
                        &dev.state,
                        DeviceSignal::OperatorDisconnect,
                        Instant::now(),
                    );
                }
                let result = self.bluez.disconnect_device(&device_path).await;
                let _ = responder.send(result);
            }
            BtCommand::Forget {
                adapter,
                device_path,
                responder,
            } => {
                let profile_id = self
                    .device_by_path(&device_path)
                    .and_then(|d| d.profile.as_ref().map(|p| p.id));
                let bluez_result = self.bluez.forget_device(&adapter, &device_path).await;
                // Remove the stored profile even if BlueZ errored —
                // we're making "ensure forgotten" idempotent.
                if let Some(id) = profile_id {
                    if let Err(e) = self.profile_store.remove_bluetooth(&id).await {
                        warn!(error = %e, "failed to remove bluetooth profile during forget");
                    }
                }
                if let Some(dev) = self.device_by_path_mut(&device_path) {
                    dev.state =
                        device::next_state(&dev.state, DeviceSignal::Removed, Instant::now());
                    dev.profile = None;
                }
                let _ = responder.send(bluez_result);
            }
            BtCommand::CancelPairing {
                device_path,
                responder,
            } => {
                // Drop the pending prompt oneshot — the Agent's
                // awaiting receiver will error out, which in turn
                // lets BlueZ's Pair() return with rejection.
                if let Some(job_id) = self
                    .device_by_path(&device_path)
                    .and_then(|d| d.pairing_job)
                {
                    if let Some(pending) = self.pending_prompt_answers.remove(&job_id) {
                        let _ = pending.sender.send(PairingAnswer::Cancel);
                    }
                }
                let result = self.bluez.cancel_pairing(&device_path).await;
                let _ = responder.send(result);
            }
            BtCommand::SetAdapterPowered {
                adapter,
                on,
                responder,
            } => {
                let _ = responder.send(self.bluez.set_powered(&adapter, on).await);
            }
            BtCommand::SetAdapterDiscoverable {
                adapter,
                on,
                responder,
            } => {
                let _ = responder.send(self.bluez.set_discoverable(&adapter, on).await);
            }
            BtCommand::SetAdapterPairable {
                adapter,
                on,
                responder,
            } => {
                let _ = responder.send(self.bluez.set_pairable(&adapter, on).await);
            }
            BtCommand::StartDiscovery {
                adapter,
                filter,
                responder,
            } => {
                let result = self.bluez.start_discovery(&adapter, filter).await;
                if result.is_ok() {
                    if let Some(entry) = self.adapter_by_bluez_path_mut(&adapter) {
                        entry.nexus_has_discovery_session = true;
                        entry.discovery_started_at = Some(Instant::now());
                    }
                    m::record_discovery(&adapter);
                }
                let _ = responder.send(result);
            }
            BtCommand::StopDiscovery { adapter, responder } => {
                let result = self.bluez.stop_discovery(&adapter).await;
                if let Some(entry) = self.adapter_by_bluez_path_mut(&adapter) {
                    entry.nexus_has_discovery_session = false;
                    entry.discovery_started_at = None;
                }
                let _ = responder.send(result);
            }
            BtCommand::LookupPairingJob {
                device_path,
                responder,
            } => {
                let job = self
                    .device_by_path(&device_path)
                    .and_then(|d| d.pairing_job);
                let _ = responder.send(job);
            }
            BtCommand::LookupAuthorizationPolicy {
                device_path,
                service_uuid,
                responder,
            } => {
                let decision = lookup_authorization(
                    self.device_by_path(&device_path),
                    service_uuid.as_deref(),
                );
                let _ = responder.send(decision);
            }
            BtCommand::RegisterPromptOneshot {
                job_id,
                sender,
                kind,
                data,
            } => {
                self.handle_register_prompt(job_id, sender, kind, data);
            }
            BtCommand::SetPowerState { state, responder } => {
                let result = self.apply_power_state(state).await;
                let _ = responder.send(result);
            }
        }
    }

    /// Operator-supplied answer to a pending pairing prompt. Per
    /// DD-006 §6.4, each `PairingPromptKind` accepts exactly one
    /// answer-variant family; mismatches return
    /// `InvalidPromptAnswer` without disturbing the pending prompt
    /// so the operator can retry.
    fn answer_pairing_prompt(&mut self, job_id: PairingJobId, answer: PairingAnswer) -> Result<()> {
        let Some(pending) = self.pending_prompt_answers.get(&job_id) else {
            return Err(BtError::UnknownPairingJob(job_id));
        };
        if let Err(reason) = crate::pairing::validate_answer(pending.kind, &answer) {
            return Err(BtError::InvalidPromptAnswer(reason));
        }
        // Validation passed — consume the pending entry and resolve
        // the Agent's oneshot. `send` fails only if the Agent has
        // given up on its receiver (e.g., response timeout elapsed);
        // surface that as `PairingJobGone`.
        let pending = self
            .pending_prompt_answers
            .remove(&job_id)
            .expect("entry present; just peeked");
        pending
            .sender
            .send(answer)
            .map_err(|_| BtError::PairingJobGone)
    }

    fn handle_register_prompt(
        &mut self,
        job_id: PairingJobId,
        sender: oneshot::Sender<PairingAnswer>,
        kind: PairingPromptKind,
        data: PairingPromptData,
    ) {
        // Incoming-authorization prompts synthesize a fresh
        // `job_id` (the device isn't in a pair-in-flight state);
        // accept those unconditionally. Pairing prompts require
        // the job_id to still be live.
        let is_auth_prompt = matches!(
            kind,
            PairingPromptKind::RequestAuthorization | PairingPromptKind::AuthorizeService
        );
        let job_live = self
            .adapters
            .values()
            .flat_map(|a| a.devices.values())
            .any(|e| e.pairing_job == Some(job_id));
        if !is_auth_prompt && !job_live {
            // Drop `sender`; the Agent's receiver errors out.
            tracing::debug!(?job_id, "prompt for stale job; dropping");
            return;
        }
        self.pending_prompt_answers
            .insert(job_id, PendingPrompt { kind, sender });
        let _ = self.event_tx.send(NexusEvent::BtPairingPrompt {
            job_id,
            kind,
            data: data.clone(),
        });
        let _ = self.event_tx.send(NexusEvent::OperatorNotification {
            kind: "bluetooth_pairing_prompt".to_owned(),
            data: build_prompt_notification(job_id, kind, &data),
        });
    }

    // -----------------------------------------------------------------
    // Power state — DD-004 §12
    // -----------------------------------------------------------------

    async fn apply_power_state(&mut self, state: PowerState) -> Result<()> {
        if self.power_state == state {
            return Ok(());
        }
        let previous = self.power_state;
        self.power_state = state;
        info!(
            from = previous.as_str(),
            to = state.as_str(),
            "bt power state"
        );

        let adapters: Vec<String> = self
            .adapters
            .values()
            .map(|a| a.bluez_path.clone())
            .collect();

        match state {
            PowerState::Sleep => {
                // Stop any Nexus-owned discovery before powering off.
                for path in &adapters {
                    let _ = self.bluez.stop_discovery(path).await;
                    if let Err(e) = self.bluez.set_powered(path, false).await {
                        warn!(adapter = %path, error = %e, "power off failed");
                    }
                }
            }
            PowerState::Active | PowerState::Background => {
                if previous == PowerState::Sleep && self.config.auto_power_on_startup {
                    for path in &adapters {
                        if let Err(e) = self.bluez.set_powered(path, true).await {
                            warn!(adapter = %path, error = %e, "power on failed");
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Supervisor — DD-004 §§7.3, 12
    // -----------------------------------------------------------------

    async fn reconcile(&mut self) {
        if self.bluez.is_connected() {
            if self.first_bluez_outage_at.is_some() && self.outage_notified {
                self.emit_notification("subsystem_recovered", &[("subsystem", "bluez")]);
            }
            self.first_bluez_outage_at = None;
            self.outage_notified = false;
            self.reconnect_attempts = 0;
            self.enforce_discovery_timeout().await;
            self.gc_stale_devices();
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
            self.last_reconnect_attempt = Some(Instant::now());
            self.reconnect_attempts += 1;
            m::record_bluez_reconnect();
            if let Err(e) = self.bluez.connect().await {
                warn!(error = %e, "bluez reconnect failed");
            }
        }
        if let Some(t) = self.first_bluez_outage_at {
            if t.elapsed().as_secs() >= self.config.bluez_outage_notify_s as u64
                && !self.outage_notified
            {
                self.outage_notified = true;
                self.emit_notification(
                    "subsystem_unavailable",
                    &[
                        ("subsystem", "bluez"),
                        ("duration_s", &t.elapsed().as_secs().to_string()),
                    ],
                );
            }
        }
    }

    async fn enforce_discovery_timeout(&mut self) {
        if self.config.discovery_timeout_s == 0 {
            return;
        }
        let timeout = Duration::from_secs(self.config.discovery_timeout_s as u64);
        let expired: Vec<String> = self
            .adapters
            .values()
            .filter(|a| {
                a.nexus_has_discovery_session
                    && a.discovery_started_at
                        .is_some_and(|t| t.elapsed() >= timeout)
            })
            .map(|a| a.bluez_path.clone())
            .collect();
        for adapter in expired {
            if let Err(e) = self.bluez.stop_discovery(&adapter).await {
                warn!(%adapter, error = %e, "auto-stop discovery failed");
            }
            if let Some(entry) = self.adapter_by_bluez_path_mut(&adapter) {
                entry.nexus_has_discovery_session = false;
                entry.discovery_started_at = None;
            }
        }
    }

    /// Drop unpaired, unbonded, unconnected device entries that
    /// haven't been observed for the configured TTL. Keeps the
    /// metric cardinality bounded in high-BLE-advertising
    /// environments (DD-004 §13.2).
    fn gc_stale_devices(&mut self) {
        let ttl = self.config.discovery_device_ttl_s;
        if ttl == 0 {
            return;
        }
        let cutoff = Duration::from_secs(ttl as u64);
        let now = Instant::now();
        for entry in self.adapters.values_mut() {
            entry.devices.retain(|_, dev| {
                if dev.info.paired
                    || dev.info.bonded
                    || dev.info.connected
                    || matches!(
                        dev.state,
                        BtDeviceState::Pairing { .. } | BtDeviceState::Connecting { .. }
                    )
                {
                    return true;
                }
                now.saturating_duration_since(dev.last_seen) < cutoff
            });
        }
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    fn adapter_by_bluez_path_mut(&mut self, path: &str) -> Option<&mut BtAdapterEntry> {
        self.adapters.values_mut().find(|e| e.bluez_path == path)
    }

    fn device_by_path(&self, device_path: &str) -> Option<&BtDeviceEntry> {
        self.adapters
            .values()
            .find_map(|a| a.devices.get(device_path))
    }

    fn device_by_path_mut(&mut self, device_path: &str) -> Option<&mut BtDeviceEntry> {
        self.adapters
            .values_mut()
            .find_map(|a| a.devices.get_mut(device_path))
    }

    fn device_by_address_mut(
        &mut self,
        adapter_path: &str,
        address: &MacAddr,
    ) -> Option<&mut BtDeviceEntry> {
        let adapter = self.adapter_by_bluez_path_mut(adapter_path)?;
        adapter
            .devices
            .values_mut()
            .find(|d| &d.info.address == address)
    }

    fn adapter_for_device(&self, device_path: &str) -> Option<String> {
        self.adapters
            .values()
            .find(|a| a.devices.contains_key(device_path))
            .map(|a| a.bluez_path.clone())
    }

    fn refresh_metrics(&self) {
        m::set_adapters(self.adapters.len() as u64);
        for entry in self.adapters.values() {
            m::set_devices(&entry.bluez_path, entry.devices.len() as u64);
            for label in ADAPTER_LABELS {
                m::set_adapter_state(&entry.bluez_path, label, entry.state.label() == *label);
            }
            for dev in entry.devices.values() {
                for label in DEVICE_LABELS {
                    m::set_device_state(
                        &entry.bluez_path,
                        &dev.info.address.to_bluez_lower(),
                        label,
                        dev.state.label() == *label,
                    );
                }
            }
        }
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
}

const ADAPTER_LABELS: &[&str] = &["unavailable", "present", "powered", "discovering", "gone"];

const DEVICE_LABELS: &[&str] = &[
    "discovered",
    "pairing",
    "paired",
    "connecting",
    "connected",
    "disconnecting",
    "failed",
    "removed",
];

fn lookup_authorization(
    device: Option<&BtDeviceEntry>,
    _service_uuid: Option<&str>,
) -> AuthorizationDecision {
    match device {
        None => AuthorizationDecision::Prompt,
        Some(entry) => match &entry.profile {
            None => AuthorizationDecision::Prompt,
            Some(p) if !p.auto_accept_incoming => AuthorizationDecision::Prompt,
            // `authorized_services` is not on the current
            // [`BluetoothProfile`] (DD-007 deferred the field); a
            // device with `auto_accept_incoming = true` is accepted
            // for any service. When the whitelist lands, gate here.
            Some(_) => AuthorizationDecision::Accept,
        },
    }
}

fn pair_outcome_label(success: bool, reason: Option<&BtFailureReason>) -> &'static str {
    if success {
        return m::pair_outcome::SUCCESS;
    }
    match reason {
        Some(BtFailureReason::PairingRejected) => m::pair_outcome::REJECTED,
        Some(BtFailureReason::PairingTimeout) => m::pair_outcome::TIMEOUT,
        Some(BtFailureReason::PairingAuthFailed) => m::pair_outcome::AUTH_FAILED,
        _ => m::pair_outcome::OTHER,
    }
}

/// Build a fresh [`BluetoothProfile`] from a newly-paired device
/// snapshot and the adapter path it bonded under. See DD-004 §7.3.
fn build_profile(info: &BtDeviceInfo, adapter_path: &str) -> BluetoothProfile {
    let mut preferences = std::collections::BTreeMap::new();
    preferences.insert(
        "transport".to_owned(),
        transport_string(info.transport).to_owned(),
    );
    if let Some(alias) = &info.alias {
        preferences.insert("alias".to_owned(), alias.clone());
    }
    let now = chrono::Utc::now();
    BluetoothProfile {
        id: Ulid::new(),
        schema_version: 1,
        metadata: ProfileMetadata {
            created_at: Some(now),
            updated_at: Some(now),
            label: None,
        },
        adapter_path: adapter_path.to_owned(),
        device_address: info.address,
        device_name: info.alias.clone().or_else(|| info.name.clone()),
        auto_connect: true,
        auto_accept_incoming: false,
        preferences,
    }
}

fn transport_string(t: BtTransport) -> &'static str {
    match t {
        BtTransport::Bredr => "bredr",
        BtTransport::Le => "le",
        BtTransport::Dual => "dual",
    }
}

/// Convenience trait so lower-case MAC labels are a one-liner.
trait MacAddrExt {
    fn to_bluez_lower(&self) -> String;
}

impl MacAddrExt for MacAddr {
    fn to_bluez_lower(&self) -> String {
        let [a, b, c, d, e, g] = self.0;
        format!("{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_info() -> BtDeviceInfo {
        BtDeviceInfo {
            adapter: "/org/bluez/hci0".into(),
            device_path: "/org/bluez/hci0/dev_00_00_00_00_00_00".into(),
            address: MacAddr([0; 6]),
            address_type: nexus_core::BtAddressType::LePublic,
            name: None,
            alias: None,
            rssi: None,
            tx_power: None,
            uuids: vec![],
            transport: nexus_core::BtTransport::Le,
            manufacturer_data: Default::default(),
            paired: true,
            bonded: true,
            trusted: true,
            blocked: false,
            connected: false,
        }
    }

    #[test]
    fn lookup_authorization_prompts_without_profile() {
        let result = lookup_authorization(None, None);
        assert_eq!(result, AuthorizationDecision::Prompt);
    }

    #[test]
    fn lookup_authorization_prompts_with_profile_auto_accept_false() {
        let mut info = dummy_info();
        info.paired = true;
        let entry_info = info;
        let profile = BluetoothProfile {
            id: Ulid::new(),
            schema_version: 1,
            metadata: Default::default(),
            adapter_path: "/org/bluez/hci0".into(),
            device_address: entry_info.address,
            device_name: None,
            auto_connect: true,
            auto_accept_incoming: false,
            preferences: Default::default(),
        };
        let mut entry = BtDeviceEntry::new(entry_info, BtDeviceState::Paired);
        entry.profile = Some(profile);
        let result = lookup_authorization(Some(&entry), None);
        assert_eq!(result, AuthorizationDecision::Prompt);
    }

    #[test]
    fn lookup_authorization_accepts_with_auto_accept_flag() {
        let profile = BluetoothProfile {
            id: Ulid::new(),
            schema_version: 1,
            metadata: Default::default(),
            adapter_path: "/org/bluez/hci0".into(),
            device_address: MacAddr([0; 6]),
            device_name: None,
            auto_connect: true,
            auto_accept_incoming: true,
            preferences: Default::default(),
        };
        let mut entry = BtDeviceEntry::new(dummy_info(), BtDeviceState::Paired);
        entry.profile = Some(profile);
        assert_eq!(
            lookup_authorization(Some(&entry), None),
            AuthorizationDecision::Accept,
        );
        assert_eq!(
            lookup_authorization(Some(&entry), Some("0000180f-0000-1000-8000-00805f9b34fb")),
            AuthorizationDecision::Accept,
        );
    }

    #[test]
    fn pair_outcome_label_success() {
        assert_eq!(pair_outcome_label(true, None), m::pair_outcome::SUCCESS);
    }

    #[test]
    fn pair_outcome_label_maps_reasons() {
        assert_eq!(
            pair_outcome_label(false, Some(&BtFailureReason::PairingRejected)),
            m::pair_outcome::REJECTED
        );
        assert_eq!(
            pair_outcome_label(false, Some(&BtFailureReason::PairingTimeout)),
            m::pair_outcome::TIMEOUT
        );
        assert_eq!(
            pair_outcome_label(false, Some(&BtFailureReason::PairingAuthFailed)),
            m::pair_outcome::AUTH_FAILED
        );
        assert_eq!(
            pair_outcome_label(false, Some(&BtFailureReason::ConnectionFailed)),
            m::pair_outcome::OTHER
        );
    }
}
