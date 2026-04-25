//! Programmable mock supplicant used by unit + integration tests.
//! See DD-003 §14.1.
//!
//! Every `SupplicantEvent` variant is producible from tests; the
//! behavior knob lets callers script scan results, connect
//! outcomes, and crash transitions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nexus_core::MacAddr;
use tokio::sync::broadcast;

use super::{DisconnectHint, SupplicantEvent, SupplicantState, WifiSupplicantBackend};
use crate::error::{Result, WifiError};
use crate::types::{BssInfo, NetworkConfig, NetworkHandle, RoamTarget, ScanParams, SignalInfo};

/// Shared handle: cheap to clone so a test can drive the mock's
/// behavior while the mock itself lives inside the backend.
#[derive(Clone, Default)]
pub struct MockSupplicantHandle {
    inner: Arc<Mutex<MockState>>,
}

#[derive(Default)]
struct MockState {
    scan_results: HashMap<u32, Vec<BssInfo>>,
    connect_outcome: HashMap<u32, MockBehavior>,
    signal_info: HashMap<u32, SignalInfo>,
    daemon_up: bool,
    handle_counter: u64,
    /// Append-log of every `scan(ifindex, params)` call. Tests for
    /// the C8 directed-roam-scan trigger and K5 metric classifier
    /// inspect this.
    scan_calls: Vec<(u32, ScanParams)>,
    /// Append-log of every `roam(ifindex, target)` call. Tests for
    /// C9 (Roaming + outcome) inspect this.
    roam_calls: Vec<(u32, RoamTarget)>,
    /// Append-log of every `provide_network_credential` call. Tests
    /// for C10 inspect this.
    credential_replies: Vec<CredentialReply>,
    /// When `true`, the next `signal_info` call returns
    /// `WifiError::NotAttached { ifindex }`. Used by the C6
    /// wake-from-sleep test to drive the post-wake probe failure
    /// path. Sticky: tests reset it explicitly.
    fail_signal_info: bool,
}

/// Recorded shape of a `provide_network_credential` invocation.
/// The mock doesn't otherwise interpret the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialReply {
    pub ifindex: u32,
    pub network: String,
    pub field: String,
    pub value: String,
}

/// Scripted outcome for the next `connect` call on a given ifindex.
#[derive(Debug, Clone, Default)]
pub enum MockBehavior {
    /// Full path: Associating → Authenticating → 4-Way → Connected.
    #[default]
    Success,
    /// Hop straight to the Connected state (Open / OWE flows do).
    OpenSuccess,
    /// Connect fails with the given hint.
    Fail(DisconnectHint),
    /// Report the supplicant daemon disappeared mid-connect.
    DaemonDown,
}

impl MockSupplicantHandle {
    pub fn new() -> Self {
        let state = MockState {
            daemon_up: true,
            ..MockState::default()
        };
        Self {
            inner: Arc::new(Mutex::new(state)),
        }
    }

    pub fn set_scan_results(&self, ifindex: u32, results: Vec<BssInfo>) {
        self.inner
            .lock()
            .unwrap()
            .scan_results
            .insert(ifindex, results);
    }

    pub fn set_connect_outcome(&self, ifindex: u32, behavior: MockBehavior) {
        self.inner
            .lock()
            .unwrap()
            .connect_outcome
            .insert(ifindex, behavior);
    }

    pub fn set_signal(&self, ifindex: u32, info: SignalInfo) {
        self.inner.lock().unwrap().signal_info.insert(ifindex, info);
    }

    /// Toggle the "daemon up" flag. When false, mutating calls
    /// return `WifiError::Supplicant` and no events are emitted.
    pub fn set_daemon_up(&self, up: bool) {
        self.inner.lock().unwrap().daemon_up = up;
    }

    fn daemon_up(&self) -> bool {
        self.inner.lock().unwrap().daemon_up
    }

    /// Snapshot every recorded `scan(ifindex, params)` call. Tests
    /// for C8 / K5 inspect this.
    pub fn scan_calls(&self) -> Vec<(u32, ScanParams)> {
        self.inner.lock().unwrap().scan_calls.clone()
    }

    /// Snapshot every recorded `roam(ifindex, target)` call. Tests
    /// for C9 inspect this.
    pub fn roam_calls(&self) -> Vec<(u32, RoamTarget)> {
        self.inner.lock().unwrap().roam_calls.clone()
    }

    /// Snapshot every recorded `provide_network_credential` call.
    /// Tests for C10 inspect this.
    pub fn credential_replies(&self) -> Vec<CredentialReply> {
        self.inner.lock().unwrap().credential_replies.clone()
    }

    /// When `true`, the next `signal_info` call returns
    /// `WifiError::NotAttached { ifindex }` regardless of the
    /// scripted `set_signal` value. C6 uses this to make the
    /// post-wake probe fail.
    pub fn set_signal_info_fails(&self, fail: bool) {
        self.inner.lock().unwrap().fail_signal_info = fail;
    }

    fn fresh_handle(&self) -> NetworkHandle {
        let mut s = self.inner.lock().unwrap();
        s.handle_counter += 1;
        NetworkHandle(format!("mock/net/{}", s.handle_counter))
    }
}

/// The mock itself. Holds the event sender so it can emit on
/// behalf of the scripted behavior.
pub struct MockSupplicant {
    event_tx: broadcast::Sender<SupplicantEvent>,
    state: MockSupplicantHandle,
    attached: HashMap<u32, String>,
}

impl MockSupplicant {
    pub fn new(event_tx: broadcast::Sender<SupplicantEvent>) -> Self {
        Self {
            event_tx,
            state: MockSupplicantHandle::new(),
            attached: HashMap::new(),
        }
    }

    /// Clone a handle so the test can program behavior while the
    /// supplicant lives inside the backend.
    pub fn handle(&self) -> MockSupplicantHandle {
        self.state.clone()
    }

    fn send(&self, event: SupplicantEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Emit a scripted connect sequence given the desired outcome.
    fn drive_connect(&self, ifindex: u32, network: &NetworkConfig) {
        let outcome = self
            .state
            .inner
            .lock()
            .unwrap()
            .connect_outcome
            .get(&ifindex)
            .cloned()
            .unwrap_or_default();

        // Pick any BSS from the scan cache for the requested SSID
        // so the Connected payload is plausible. Fall back to a
        // synthetic BSSID.
        let bss = self
            .state
            .inner
            .lock()
            .unwrap()
            .scan_results
            .get(&ifindex)
            .and_then(|v| v.iter().find(|b| b.ssid == network.ssid).cloned());
        let bssid = bss.as_ref().map(|b| b.bssid).unwrap_or(MacAddr([0x02; 6]));
        let frequency = bss.as_ref().map(|b| b.frequency).unwrap_or(2412);

        match outcome {
            MockBehavior::Success => {
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::Associating,
                });
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::Authenticating,
                });
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::FourWayHandshake,
                });
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::Connected {
                        bssid,
                        ssid: network.ssid.clone(),
                        frequency,
                    },
                });
            }
            MockBehavior::OpenSuccess => {
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::Associating,
                });
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::Connected {
                        bssid,
                        ssid: network.ssid.clone(),
                        frequency,
                    },
                });
            }
            MockBehavior::Fail(reason) => {
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::Associating,
                });
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::Disconnected { reason },
                });
            }
            MockBehavior::DaemonDown => {
                self.state.set_daemon_up(false);
                self.send(SupplicantEvent::State {
                    ifindex,
                    state: SupplicantState::Disconnected {
                        reason: DisconnectHint::DaemonUnavailable,
                    },
                });
                self.send(SupplicantEvent::DaemonDown);
            }
        }
    }
}

#[async_trait]
impl WifiSupplicantBackend for MockSupplicant {
    async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()> {
        if !self.state.daemon_up() {
            return Err(WifiError::Supplicant {
                backend: "mock",
                source: "daemon down".into(),
            });
        }
        self.attached.insert(ifindex, ifname.to_owned());
        Ok(())
    }

    async fn detach(&mut self, ifindex: u32) -> Result<()> {
        self.attached.remove(&ifindex);
        Ok(())
    }

    async fn scan(&mut self, ifindex: u32, params: ScanParams) -> Result<()> {
        if !self.attached.contains_key(&ifindex) {
            return Err(WifiError::NotAttached { ifindex });
        }
        // Record the call so tests can assert on params (C8 / K5).
        self.state
            .inner
            .lock()
            .unwrap()
            .scan_calls
            .push((ifindex, params));
        // Immediately complete. Real supplicants take 3-5 seconds
        // for a full-spectrum scan; tests that need timing can sleep
        // after `scan()` returns.
        self.send(SupplicantEvent::ScanComplete {
            ifindex,
            success: true,
        });
        Ok(())
    }

    async fn get_scan_results(&self, ifindex: u32) -> Result<Vec<BssInfo>> {
        Ok(self
            .state
            .inner
            .lock()
            .unwrap()
            .scan_results
            .get(&ifindex)
            .cloned()
            .unwrap_or_default())
    }

    async fn connect(&mut self, ifindex: u32, network: &NetworkConfig) -> Result<NetworkHandle> {
        if !self.attached.contains_key(&ifindex) {
            return Err(WifiError::NotAttached { ifindex });
        }
        let handle = self.state.fresh_handle();
        self.drive_connect(ifindex, network);
        Ok(handle)
    }

    async fn disconnect(&mut self, ifindex: u32) -> Result<()> {
        if self.attached.contains_key(&ifindex) {
            self.send(SupplicantEvent::State {
                ifindex,
                state: SupplicantState::Disconnected {
                    reason: DisconnectHint::LocalRequest,
                },
            });
        }
        Ok(())
    }

    async fn forget_network(&mut self, _ifindex: u32, _handle: NetworkHandle) -> Result<()> {
        // Mock: nothing to persist.
        Ok(())
    }

    async fn roam(&mut self, ifindex: u32, target: RoamTarget) -> Result<()> {
        // Record before consuming the target so the assertion log
        // sees it verbatim (C9).
        self.state
            .inner
            .lock()
            .unwrap()
            .roam_calls
            .push((ifindex, target.clone()));
        let bssid = match target {
            RoamTarget::Auto => MacAddr([0x55; 6]),
            RoamTarget::Bss(b) => b,
        };
        // Jump straight to Connected on the new BSSID. Real
        // supplicants go through Associating again; tests that care
        // about that transition scan specifically for it.
        let (ssid, frequency) = self
            .state
            .inner
            .lock()
            .unwrap()
            .scan_results
            .get(&ifindex)
            .and_then(|v| v.iter().find(|b| b.bssid == bssid).cloned())
            .map(|b| (b.ssid, b.frequency))
            .unwrap_or_else(|| (nexus_core::Ssid::new(b"mock".to_vec()).unwrap(), 2412));
        self.send(SupplicantEvent::State {
            ifindex,
            state: SupplicantState::Connected {
                bssid,
                ssid,
                frequency,
            },
        });
        Ok(())
    }

    async fn signal_info(&self, ifindex: u32) -> Result<SignalInfo> {
        let state = self.state.inner.lock().unwrap();
        if state.fail_signal_info {
            return Err(WifiError::NotAttached { ifindex });
        }
        state
            .signal_info
            .get(&ifindex)
            .cloned()
            .ok_or(WifiError::NotAttached { ifindex })
    }

    async fn provide_network_credential(
        &mut self,
        ifindex: u32,
        network: &str,
        field: &str,
        value: &str,
    ) -> Result<()> {
        self.state
            .inner
            .lock()
            .unwrap()
            .credential_replies
            .push(CredentialReply {
                ifindex,
                network: network.to_owned(),
                field: field.to_owned(),
                value: value.to_owned(),
            });
        Ok(())
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use nexus_core::Ssid;

    use super::*;

    fn net(ssid: &[u8]) -> NetworkConfig {
        NetworkConfig {
            ssid: Ssid::new(ssid.to_vec()).unwrap(),
            hidden: false,
            security: nexus_profile_store::SecurityConfig::Open,
            priority: 0,
            bssid_preferred: None,
            bssid_blacklist: vec![],
            scan_freqs: vec![],
            fast_transition: false,
        }
    }

    #[tokio::test]
    async fn success_path_emits_full_state_sequence() {
        let (tx, mut rx) = broadcast::channel(32);
        let mut sup = MockSupplicant::new(tx);
        sup.attach(2, "wlan0").await.unwrap();

        sup.handle().set_connect_outcome(2, MockBehavior::Success);
        let _ = sup.connect(2, &net(b"x")).await.unwrap();

        let mut states = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let SupplicantEvent::State { state, .. } = event {
                states.push(state);
            }
        }
        assert_eq!(states.len(), 4);
        assert!(matches!(states[0], SupplicantState::Associating));
        assert!(matches!(states[1], SupplicantState::Authenticating));
        assert!(matches!(states[2], SupplicantState::FourWayHandshake));
        assert!(matches!(states[3], SupplicantState::Connected { .. }));
    }

    #[tokio::test]
    async fn fail_behavior_emits_disconnected_with_hint() {
        let (tx, mut rx) = broadcast::channel(32);
        let mut sup = MockSupplicant::new(tx);
        sup.attach(2, "wlan0").await.unwrap();
        sup.handle()
            .set_connect_outcome(2, MockBehavior::Fail(DisconnectHint::BadCredentials));
        let _ = sup.connect(2, &net(b"x")).await.unwrap();

        let mut last = None;
        while let Ok(e) = rx.try_recv() {
            if let SupplicantEvent::State { state, .. } = e {
                last = Some(state);
            }
        }
        assert!(matches!(
            last,
            Some(SupplicantState::Disconnected {
                reason: DisconnectHint::BadCredentials,
            }),
        ));
    }

    #[tokio::test]
    async fn daemon_down_behavior_emits_daemondown_event() {
        let (tx, mut rx) = broadcast::channel(32);
        let mut sup = MockSupplicant::new(tx);
        sup.attach(2, "wlan0").await.unwrap();
        sup.handle()
            .set_connect_outcome(2, MockBehavior::DaemonDown);
        let _ = sup.connect(2, &net(b"x")).await.unwrap();

        let mut saw_daemon_down = false;
        while let Ok(e) = rx.try_recv() {
            if let SupplicantEvent::DaemonDown = e {
                saw_daemon_down = true;
            }
        }
        assert!(saw_daemon_down);
        assert!(!sup.state.daemon_up());
    }

    #[tokio::test]
    async fn scan_complete_event_fires_after_scan_call() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut sup = MockSupplicant::new(tx);
        sup.attach(2, "wlan0").await.unwrap();
        sup.scan(2, ScanParams::default()).await.unwrap();
        let event = rx.try_recv().unwrap();
        assert!(matches!(
            event,
            SupplicantEvent::ScanComplete {
                ifindex: 2,
                success: true,
            }
        ));
    }

    #[tokio::test]
    async fn scan_on_unattached_errors() {
        let (tx, _rx) = broadcast::channel(4);
        let mut sup = MockSupplicant::new(tx);
        assert!(matches!(
            sup.scan(99, ScanParams::default()).await.unwrap_err(),
            WifiError::NotAttached { ifindex: 99 },
        ));
    }
}
