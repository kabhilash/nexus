//! Event loop for the Ethernet Backend. See DD-002 §3.3.

use std::collections::HashMap;
use std::future::pending;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{AuthFailureReason, AuthState, InterfaceKind, NexusEvent, NotificationData};
use nexus_profile_store::ProfileStore;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::auth::WiredAuthBackend;
use crate::config::EthernetConfig;
use crate::error::{EthernetError, Result};
use crate::lifecycle::{EthInterfaceEntry, EthInterfaceState};
use crate::metrics as m;
use crate::profile::{default_ethernet_profile, profile_requires_auth};

/// Per-interface auxiliary state the backend tracks alongside the
/// lifecycle entry. Kept in a parallel map so `EthInterfaceEntry`
/// stays a pure state-machine record.
#[derive(Debug, Clone, Default)]
struct AuthContext {
    /// When the current (or most recent) authentication attempt
    /// started — used for the `auth_duration_seconds` histogram.
    started_at: Option<Instant>,
    /// Monotonically increasing count of `AuthState::Failed`
    /// arrivals since the last `Authenticated`. Drives
    /// `RetryPolicy::next_attempt`. Survives the
    /// `AuthFailed → Authenticating` retry transition (the
    /// `EthInterfaceState` does not, by design — see DD-002 §3.1).
    /// Reset to 0 on `Authenticated`.
    failed_attempts: u32,
}

/// The Ethernet Backend. Owned by the spawned task; the handle the
/// caller keeps is the `JoinHandle<Result<()>>`.
pub struct EthernetBackend {
    event_tx: broadcast::Sender<NexusEvent>,
    event_rx: broadcast::Receiver<NexusEvent>,
    interfaces: HashMap<u32, EthInterfaceEntry>,
    auth_ctx: HashMap<u32, AuthContext>,
    auth_backend: Option<Box<dyn WiredAuthBackend>>,
    profile_store: Arc<dyn ProfileStore>,
    config: EthernetConfig,
}

impl EthernetBackend {
    pub fn new(
        event_tx: broadcast::Sender<NexusEvent>,
        profile_store: Arc<dyn ProfileStore>,
        auth_backend: Option<Box<dyn WiredAuthBackend>>,
        config: EthernetConfig,
    ) -> Self {
        let event_rx = event_tx.subscribe();
        let backend_name = auth_backend.as_ref().map(|b| b.name()).unwrap_or("none");
        m::set_auth_backend_available(backend_name, auth_backend.is_some());
        Self {
            event_tx,
            event_rx,
            interfaces: HashMap::new(),
            auth_ctx: HashMap::new(),
            auth_backend,
            profile_store,
            config,
        }
    }

    /// Drive the event loop until `shutdown` is cancelled or the
    /// event bus is closed. Returns `Ok(())` on clean shutdown.
    pub async fn run(mut self, shutdown: CancellationToken) -> Result<()> {
        loop {
            let next_retry = self.earliest_retry_deadline();
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::info!("ethernet backend shutting down");
                    return Ok(());
                }
                res = self.event_rx.recv() => {
                    match res {
                        Ok(event) => {
                            if let Err(e) = self.handle_event(event).await {
                                tracing::warn!(error = %e, "ethernet event handler error");
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(lagged = n, "ethernet receiver lagged");
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            tracing::info!("event bus closed; ethernet backend exiting");
                            return Ok(());
                        }
                    }
                }
                _ = sleep_until_option(next_retry) => {
                    if let Err(e) = self.handle_retries_due().await {
                        tracing::warn!(error = %e, "ethernet retry error");
                    }
                }
            }
            self.refresh_state_gauges();
        }
    }

    // -----------------------------------------------------------------
    // Event dispatch
    // -----------------------------------------------------------------

    async fn handle_event(&mut self, event: NexusEvent) -> Result<()> {
        match event {
            NexusEvent::InterfaceDiscovered(info)
                if matches!(info.kind, InterfaceKind::Ethernet) =>
            {
                let ifindex = info.ifindex;
                let had_carrier = info.carrier;
                let profile = self
                    .profile_store
                    .load_ethernet_profile(&info.ifname)
                    .await?
                    .unwrap_or_else(|| default_ethernet_profile(&info.ifname));
                self.interfaces
                    .insert(ifindex, EthInterfaceEntry::new(info, profile));
                self.auth_ctx.insert(ifindex, AuthContext::default());

                self.emit_lifecycle(ifindex);

                if had_carrier {
                    self.on_carrier_up(ifindex).await?;
                }
            }
            NexusEvent::CarrierChanged { ifindex, up: true }
                if self.interfaces.contains_key(&ifindex) =>
            {
                self.on_carrier_up(ifindex).await?;
            }
            NexusEvent::CarrierChanged { ifindex, up: false }
                if self.interfaces.contains_key(&ifindex) =>
            {
                self.on_carrier_down(ifindex).await?;
            }
            NexusEvent::InterfaceRemoved { ifindex } => {
                if self.interfaces.remove(&ifindex).is_some() {
                    self.auth_ctx.remove(&ifindex);
                    if let Some(auth) = self.auth_backend.as_mut() {
                        let _ = auth.detach(ifindex).await;
                    }
                    if let Some(ifname) = self.ifname_hint(ifindex) {
                        m::record_link_lost(&ifname, m::link_lost_reason::REMOVED);
                    }
                }
            }
            NexusEvent::EthAuthStateChanged { ifindex, state } => {
                self.on_auth_state_changed(ifindex, state).await?;
            }
            NexusEvent::EthAuthBackendOwnerChanged { backend, present } => {
                self.on_auth_backend_owner_changed(&backend, present).await;
            }
            _ => {}
        }
        Ok(())
    }

    /// DD-002 §§9.1, 9.2: react to wpa_supplicant / ead D-Bus name
    /// transitions. On disappearance, every `Authenticating` /
    /// `Authenticated` interface drops to
    /// `AuthFailed { ServerUnreachable }` and emits `EthLinkLost` if
    /// it had been ready. On reappearance, the existing retry timer
    /// picks up the parked entries.
    async fn on_auth_backend_owner_changed(&mut self, backend: &str, present: bool) {
        m::set_auth_backend_available(backend, present);
        if present {
            return;
        }
        let now = Instant::now();
        let affected: Vec<u32> = self
            .interfaces
            .iter()
            .filter_map(|(ifindex, e)| match e.state {
                EthInterfaceState::Authenticating | EthInterfaceState::Authenticated => {
                    Some(*ifindex)
                }
                _ => None,
            })
            .collect();
        for ifindex in affected {
            let Some(entry) = self.interfaces.get_mut(&ifindex) else {
                continue;
            };
            let was_ready = entry.state.is_ready();
            let ifname = entry.info.ifname.clone();
            let attempts = self
                .auth_ctx
                .get(&ifindex)
                .map(|c| c.failed_attempts)
                .unwrap_or(0);
            let reason = AuthFailureReason::ServerUnreachable;
            let next = self.config.retry.next_attempt(&reason, attempts, now);
            let retry_after = next.unwrap_or(now + Duration::from_secs(3600));
            entry.state = EthInterfaceState::AuthFailed {
                retry_after,
                attempts,
                reason,
            };
            self.emit_lifecycle(ifindex);
            if was_ready {
                m::record_link_lost(&ifname, m::link_lost_reason::AUTH_FAILURE);
                let _ = self.event_tx.send(NexusEvent::EthLinkLost { ifindex });
            }
        }
    }

    async fn on_carrier_up(&mut self, ifindex: u32) -> Result<()> {
        enum Action {
            NoAuth {
                ifname: String,
            },
            StartAuth {
                ifname: String,
                config: nexus_profile_store::Dot1xEapConfig,
            },
            AuthUnavailable {
                ifname: String,
            },
        }

        let action = {
            let entry = self
                .interfaces
                .get_mut(&ifindex)
                .ok_or_else(|| EthernetError::NotAttached { ifindex })?;

            if profile_requires_auth(&entry.profile) {
                let dot1x = entry.profile.dot1x.as_ref().unwrap();
                if self.auth_backend.is_some() {
                    entry.state = EthInterfaceState::Authenticating;
                    Action::StartAuth {
                        ifname: entry.info.ifname.clone(),
                        config: dot1x.eap.clone(),
                    }
                } else {
                    entry.state = EthInterfaceState::AuthFailed {
                        retry_after: Instant::now() + Duration::from_secs(60),
                        attempts: 0,
                        reason: AuthFailureReason::ServerUnreachable,
                    };
                    Action::AuthUnavailable {
                        ifname: entry.info.ifname.clone(),
                    }
                }
            } else {
                entry.state = EthInterfaceState::LinkReady;
                Action::NoAuth {
                    ifname: entry.info.ifname.clone(),
                }
            }
        };

        self.emit_lifecycle(ifindex);

        match action {
            Action::NoAuth { ifname } => {
                m::record_link_ready(&ifname);
                let _ = self.event_tx.send(NexusEvent::EthLinkReady { ifindex });
            }
            Action::StartAuth { ifname, config } => {
                // Record the auth-start instant so the duration
                // histogram has a baseline when Authenticated /
                // AuthFailed arrives.
                self.auth_ctx.entry(ifindex).or_default().started_at = Some(Instant::now());
                let auth = self.auth_backend.as_mut().expect("checked above");
                if let Err(e) = auth.attach(ifindex, &ifname).await {
                    tracing::warn!(ifname, error = %e, "auth.attach failed");
                }
                if let Err(e) = auth.authenticate(ifindex, &config).await {
                    tracing::warn!(ifname, error = %e, "auth.authenticate failed");
                }
            }
            Action::AuthUnavailable { ifname } => {
                // DD-002 §9.1: 802.1X required but no backend up.
                // Park the entry in `AuthFailed { ServerUnreachable }`
                // (already done above) and emit a distinct metric so
                // dashboards can distinguish "no backend" from real
                // auth failures. Not an event-handler error — the
                // lifecycle is well-defined; the loop continues.
                tracing::error!(
                    ifname,
                    "802.1X profile requires auth but no backend is configured; parked in auth_failed",
                );
                m::record_auth_attempt(&ifname, m::auth_outcome::BACKEND_UNAVAILABLE);
            }
        }
        Ok(())
    }

    async fn on_carrier_down(&mut self, ifindex: u32) -> Result<()> {
        let (was_ready, ifname) = {
            let entry = self
                .interfaces
                .get_mut(&ifindex)
                .ok_or_else(|| EthernetError::NotAttached { ifindex })?;
            let ready = entry.state.is_ready();
            entry.state = EthInterfaceState::WaitingForCarrier;
            (ready, entry.info.ifname.clone())
        };

        self.emit_lifecycle(ifindex);

        if let Some(auth) = self.auth_backend.as_mut() {
            let _ = auth.detach(ifindex).await;
        }

        if was_ready {
            m::record_link_lost(&ifname, m::link_lost_reason::CARRIER_DOWN);
            let _ = self.event_tx.send(NexusEvent::EthLinkLost { ifindex });
        }
        Ok(())
    }

    async fn on_auth_state_changed(&mut self, ifindex: u32, state: AuthState) -> Result<()> {
        let Some(entry) = self.interfaces.get_mut(&ifindex) else {
            return Ok(());
        };
        let ifname = entry.info.ifname.clone();
        let was_ready = entry.state.is_ready();

        match state {
            AuthState::Authenticated => {
                entry.state = EthInterfaceState::Authenticated;
                if let Some(ctx) = self.auth_ctx.get_mut(&ifindex) {
                    ctx.failed_attempts = 0;
                    if let Some(start) = ctx.started_at.take() {
                        m::record_auth_duration(&ifname, start.elapsed().as_secs_f64());
                    }
                }
                self.emit_lifecycle(ifindex);
                m::record_auth_attempt(&ifname, m::auth_outcome::SUCCESS);
                m::record_link_ready(&ifname);
                let _ = self.event_tx.send(NexusEvent::EthLinkReady { ifindex });
            }
            AuthState::Failed { reason } => {
                let ctx = self.auth_ctx.entry(ifindex).or_default();
                ctx.failed_attempts = ctx.failed_attempts.saturating_add(1);
                let attempts = ctx.failed_attempts;
                let started_at = ctx.started_at.take();

                let now = Instant::now();
                let next = self.config.retry.next_attempt(&reason, attempts, now);
                let retry_after = next.unwrap_or(now + Duration::from_secs(3600));
                let outcome = m::outcome_for(&reason);
                let retriable = crate::retry::is_retriable(&reason);
                let reason_label = failure_reason_label(&reason).to_owned();

                if let Some(e) = self.interfaces.get_mut(&ifindex) {
                    e.state = EthInterfaceState::AuthFailed {
                        retry_after,
                        attempts,
                        reason,
                    };
                }

                if let Some(start) = started_at {
                    m::record_auth_duration(&ifname, start.elapsed().as_secs_f64());
                }

                self.emit_lifecycle(ifindex);

                m::record_auth_attempt(&ifname, outcome);
                if next.is_some() && attempts > 1 {
                    m::record_auth_retry(&ifname);
                }

                if !retriable {
                    // DD-002 §9.3 fail-fast: do not retry until the
                    // operator fixes the profile. `handle_retries_due`
                    // also re-checks `is_retriable` so the parked
                    // entry is never re-armed.
                    tracing::error!(
                        ifname,
                        "802.1X fail-fast; profile needs operator attention",
                    );
                    // DD-002 §9.6: surface to the operator via
                    // `fi.nexus.Manager.NotificationEvent`. Profile-
                    // store `credentials_invalid` is not yet wired
                    // for ethernet (the on-disk schema lacks the
                    // field today; tracked alongside DD-007).
                    let mut data = NotificationData::default();
                    data.insert("ifname", ifname.clone());
                    data.insert("reason", reason_label.clone());
                    let _ = self.event_tx.send(NexusEvent::OperatorNotification {
                        kind: "eth_credentials_invalid".to_owned(),
                        data,
                    });
                }

                if was_ready {
                    m::record_link_lost(&ifname, m::link_lost_reason::AUTH_FAILURE);
                    let _ = self.event_tx.send(NexusEvent::EthLinkLost { ifindex });
                }
            }
            AuthState::Authenticating | AuthState::Idle => {
                // Intermediate — no lifecycle transition.
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Retries
    // -----------------------------------------------------------------

    fn earliest_retry_deadline(&self) -> Option<Instant> {
        self.interfaces
            .values()
            .filter_map(|e| match &e.state {
                EthInterfaceState::AuthFailed {
                    retry_after,
                    reason,
                    ..
                } if crate::retry::is_retriable(reason) => Some(*retry_after),
                _ => None,
            })
            .min()
    }

    async fn handle_retries_due(&mut self) -> Result<()> {
        let now = Instant::now();
        let due: Vec<u32> = self
            .interfaces
            .iter()
            .filter_map(|(ifindex, entry)| match &entry.state {
                EthInterfaceState::AuthFailed {
                    retry_after,
                    reason,
                    ..
                } if *retry_after <= now && crate::retry::is_retriable(reason) => Some(*ifindex),
                _ => None,
            })
            .collect();

        for ifindex in due {
            let entry = match self.interfaces.get(&ifindex) {
                Some(e) => e,
                None => continue,
            };
            let EthInterfaceState::AuthFailed { attempts, .. } = entry.state else {
                continue;
            };
            if !profile_requires_auth(&entry.profile) {
                continue;
            }
            let eap_config = entry
                .profile
                .dot1x
                .as_ref()
                .expect("requires_auth invariant")
                .eap
                .clone();
            let ifname = entry.info.ifname.clone();

            // Only retry if we actually have an auth backend; the
            // AuthUnavailable path parks the entry with the same
            // `AuthFailed` shape and `is_retriable(ServerUnreachable)`
            // is true, so we'd reach here on every tick until the
            // backend reattaches.
            if self.auth_backend.is_none() {
                continue;
            }

            if let Some(e) = self.interfaces.get_mut(&ifindex) {
                e.state = EthInterfaceState::Authenticating;
            }
            self.emit_lifecycle(ifindex);
            self.auth_ctx.entry(ifindex).or_default().started_at = Some(Instant::now());
            let auth = self.auth_backend.as_mut().expect("checked above");
            if let Err(e) = auth.authenticate(ifindex, &eap_config).await {
                tracing::warn!(
                    ifname,
                    attempts,
                    error = %e,
                    "retry authenticate failed",
                );
            }
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------

    fn refresh_state_gauges(&self) {
        let mut counts: HashMap<&'static str, u64> = HashMap::new();
        for entry in self.interfaces.values() {
            *counts.entry(entry.state.label()).or_insert(0) += 1;
        }
        for label in &[
            "registered",
            "waiting_carrier",
            "link_ready",
            "authenticating",
            "authenticated",
            "auth_failed",
            "gone",
        ] {
            m::set_interfaces_managed(label, counts.get(label).copied().unwrap_or(0));
        }
    }

    fn ifname_hint(&self, ifindex: u32) -> Option<String> {
        self.interfaces.get(&ifindex).map(|e| e.info.ifname.clone())
    }

    /// Emit `NexusEvent::EthLifecycleStateChanged` reflecting the
    /// current `EthInterfaceState` for `ifindex`. Idempotent — callers
    /// invoke it after every `entry.state = ...` assignment so the
    /// D-Bus layer's `fi.nexus.Ethernet` cache stays in sync per
    /// DD-006 §6.2.
    fn emit_lifecycle(&self, ifindex: u32) {
        let Some(entry) = self.interfaces.get(&ifindex) else {
            return;
        };
        let state = entry.state.label().to_owned();
        let eap_method = entry
            .profile
            .dot1x
            .as_ref()
            .filter(|d| d.enabled)
            .map(|d| d.eap.eap.as_str().to_owned());
        let auth_failure_reason = match &entry.state {
            EthInterfaceState::AuthFailed { reason, .. } => {
                Some(failure_reason_label(reason).to_owned())
            }
            _ => None,
        };
        let _ = self.event_tx.send(NexusEvent::EthLifecycleStateChanged {
            ifindex,
            state,
            eap_method,
            auth_failure_reason,
        });
    }
}

/// DD-006 §6.2 `AuthFailureReason` wire labels.
fn failure_reason_label(reason: &AuthFailureReason) -> &'static str {
    match reason {
        AuthFailureReason::BadCredentials => "bad_credentials",
        AuthFailureReason::ServerUnreachable => "server_unreachable",
        AuthFailureReason::CertificateRejected => "certificate_rejected",
        AuthFailureReason::Timeout => "timeout",
        AuthFailureReason::Other(_) => "other",
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
