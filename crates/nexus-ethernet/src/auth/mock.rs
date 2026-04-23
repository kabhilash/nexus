//! Programmable mock implementation of [`WiredAuthBackend`] used
//! by unit and integration tests. See DD-002 §§3.3, 5.1, 10.1.
//!
//! Tests install a [`MockScenario`] per ifindex and then let the
//! Ethernet Backend drive `attach` / `authenticate`. The mock
//! schedules async emission of `NexusEvent::EthAuthStateChanged`
//! on the event bus; the real backend picks those up the same way
//! it would from a live wpa_supplicant.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use nexus_core::NexusEvent;
use nexus_profile_store::Dot1xEapConfig;
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use super::{AuthFailureReason, AuthState, WiredAuthBackend};
use crate::error::{EthernetError, Result};

/// Canned behavior for one ifindex.
#[derive(Debug, Clone)]
pub enum MockScenario {
    /// Emit `Authenticated` immediately after `authenticate`.
    ImmediateSuccess,
    /// Emit `Failed { reason }` immediately after `authenticate`.
    ImmediateFailure(AuthFailureReason),
    /// Emit `Authenticating`, then `Authenticated` after `delay`.
    SuccessAfter { delay: Duration },
    /// Emit `Authenticating`, then `Failed { reason }` after `delay`.
    FailAfter {
        delay: Duration,
        reason: AuthFailureReason,
    },
    /// Emit `Authenticating` and nothing else — simulates a backend
    /// that never completes (operator must reset).
    Hang,
}

impl Default for MockScenario {
    fn default() -> Self {
        MockScenario::ImmediateSuccess
    }
}

/// Shared state the mock publishes its scenarios through. Cheap to
/// clone; pass one to the backend at construction time and keep
/// another for the test to set scenarios via
/// [`MockAuthBackend::set_scenario`].
#[derive(Clone, Default)]
pub struct MockScenarios {
    inner: Arc<Mutex<HashMap<u32, MockScenario>>>,
}

impl MockScenarios {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, ifindex: u32, scenario: MockScenario) {
        self.inner.lock().unwrap().insert(ifindex, scenario);
    }

    pub fn get(&self, ifindex: u32) -> MockScenario {
        self.inner
            .lock()
            .unwrap()
            .get(&ifindex)
            .cloned()
            .unwrap_or_default()
    }
}

/// Test backend. Stores scenarios via [`MockScenarios`]; emits
/// auth-state events through `event_tx`.
pub struct MockAuthBackend {
    event_tx: broadcast::Sender<NexusEvent>,
    scenarios: MockScenarios,
    registered: HashMap<u32, String>,
    in_flight: HashMap<u32, AbortHandle>,
    current_state: Arc<Mutex<HashMap<u32, AuthState>>>,
}

impl MockAuthBackend {
    pub fn new(event_tx: broadcast::Sender<NexusEvent>) -> Self {
        Self {
            event_tx,
            scenarios: MockScenarios::new(),
            registered: HashMap::new(),
            in_flight: HashMap::new(),
            current_state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Clone the scenarios handle so the test can inject behavior
    /// while the backend is running inside the Ethernet Backend task.
    pub fn scenarios(&self) -> MockScenarios {
        self.scenarios.clone()
    }

    /// Convenience: same as `self.scenarios().set(ifindex, scenario)`.
    pub fn set_scenario(&self, ifindex: u32, scenario: MockScenario) {
        self.scenarios.set(ifindex, scenario);
    }

    fn record(&self, ifindex: u32, state: AuthState) {
        self.current_state.lock().unwrap().insert(ifindex, state);
    }

    fn emit(&self, ifindex: u32, state: AuthState) {
        self.record(ifindex, state.clone());
        let _ = self
            .event_tx
            .send(NexusEvent::EthAuthStateChanged { ifindex, state });
    }
}

#[async_trait]
impl WiredAuthBackend for MockAuthBackend {
    async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()> {
        self.registered.insert(ifindex, ifname.to_owned());
        self.record(ifindex, AuthState::Idle);
        Ok(())
    }

    async fn authenticate(&mut self, ifindex: u32, _config: &Dot1xEapConfig) -> Result<()> {
        if !self.registered.contains_key(&ifindex) {
            return Err(EthernetError::NotAttached { ifindex });
        }

        // Cancel any prior in-flight scenario for this ifindex so a
        // retry doesn't race with the previous attempt.
        if let Some(prev) = self.in_flight.remove(&ifindex) {
            prev.abort();
        }

        let scenario = self.scenarios.get(ifindex);
        let event_tx = self.event_tx.clone();
        let current_state = Arc::clone(&self.current_state);

        let emit_in_task = move |state: AuthState| {
            current_state.lock().unwrap().insert(ifindex, state.clone());
            let _ = event_tx.send(NexusEvent::EthAuthStateChanged { ifindex, state });
        };

        match scenario {
            MockScenario::ImmediateSuccess => {
                self.emit(ifindex, AuthState::Authenticating);
                self.emit(ifindex, AuthState::Authenticated);
            }
            MockScenario::ImmediateFailure(reason) => {
                self.emit(ifindex, AuthState::Authenticating);
                self.emit(ifindex, AuthState::Failed { reason });
            }
            MockScenario::SuccessAfter { delay } => {
                self.emit(ifindex, AuthState::Authenticating);
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    emit_in_task(AuthState::Authenticated);
                });
                self.in_flight.insert(ifindex, handle.abort_handle());
            }
            MockScenario::FailAfter { delay, reason } => {
                self.emit(ifindex, AuthState::Authenticating);
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    emit_in_task(AuthState::Failed { reason });
                });
                self.in_flight.insert(ifindex, handle.abort_handle());
            }
            MockScenario::Hang => {
                self.emit(ifindex, AuthState::Authenticating);
            }
        }
        Ok(())
    }

    async fn detach(&mut self, ifindex: u32) -> Result<()> {
        if let Some(handle) = self.in_flight.remove(&ifindex) {
            handle.abort();
        }
        self.registered.remove(&ifindex);
        self.current_state.lock().unwrap().remove(&ifindex);
        Ok(())
    }

    async fn state(&self, ifindex: u32) -> Result<AuthState> {
        Ok(self
            .current_state
            .lock()
            .unwrap()
            .get(&ifindex)
            .cloned()
            .unwrap_or(AuthState::Idle))
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eap_config() -> Dot1xEapConfig {
        Dot1xEapConfig {
            eap: nexus_profile_store::EapMethod::Peap,
            identity: "u".into(),
            anonymous_identity: None,
            ca_cert: None,
            client_cert: None,
            client_key: None,
            client_key_password: None,
            phase2: None,
            domain_suffix_match: None,
            password: None,
        }
    }

    #[tokio::test]
    async fn immediate_success_emits_two_events() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut backend = MockAuthBackend::new(tx);
        backend.set_scenario(2, MockScenario::ImmediateSuccess);
        backend.attach(2, "eth0").await.unwrap();
        backend.authenticate(2, &eap_config()).await.unwrap();

        let mut states = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let NexusEvent::EthAuthStateChanged { state, .. } = event {
                states.push(state);
            }
        }
        assert_eq!(states.len(), 2);
        assert!(matches!(states[0], AuthState::Authenticating));
        assert!(matches!(states[1], AuthState::Authenticated));
    }

    #[tokio::test]
    async fn immediate_failure_emits_failed_with_reason() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut backend = MockAuthBackend::new(tx);
        backend.set_scenario(
            3,
            MockScenario::ImmediateFailure(AuthFailureReason::BadCredentials),
        );
        backend.attach(3, "eth1").await.unwrap();
        backend.authenticate(3, &eap_config()).await.unwrap();

        let mut last = None;
        while let Ok(event) = rx.try_recv() {
            if let NexusEvent::EthAuthStateChanged { state, .. } = event {
                last = Some(state);
            }
        }
        assert!(matches!(
            last,
            Some(AuthState::Failed {
                reason: AuthFailureReason::BadCredentials,
            }),
        ));
    }

    #[tokio::test]
    async fn authenticate_on_unattached_returns_err() {
        let (tx, _rx) = broadcast::channel(8);
        let mut backend = MockAuthBackend::new(tx);
        let err = backend.authenticate(99, &eap_config()).await.unwrap_err();
        assert!(matches!(err, EthernetError::NotAttached { ifindex: 99 }));
    }
}
