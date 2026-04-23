//! Pluggable Wi-Fi supplicant. See DD-003 §4.1.
//!
//! Every interaction with an external supplicant daemon goes
//! through [`WifiSupplicantBackend`] — the backend's event loop
//! never talks to zbus directly. That keeps the mock-driven test
//! path honest and makes it straightforward to add a second
//! supplicant (iwd, at Phase 10) without churning the Wi-Fi
//! backend itself.

pub mod mock;

#[cfg(feature = "wifi-wpa_supplicant")]
pub mod wpa_supplicant;

// iwd backend is deferred per DD-003 Phase 10. Shape:
//
//     pub mod iwd { /* WifiSupplicantBackend for IwdBackend */ }
//
// Landing item tracks alongside DD-003 §10.
#[cfg(feature = "wifi-iwd")]
pub(crate) mod iwd {}

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{BssInfo, NetworkConfig, NetworkHandle, RoamTarget, ScanParams, SignalInfo};

pub use mock::{MockBehavior, MockSupplicant, MockSupplicantHandle};

/// Asynchronous event emitted by a supplicant implementation.
/// Implementations emit these through a `broadcast::Sender` the
/// caller plugs in at construction time; the Wi-Fi backend drains
/// the channel as one arm of its select! loop.
#[derive(Debug, Clone)]
pub enum SupplicantEvent {
    /// Supplicant state transition surfaced by the daemon.
    State {
        ifindex: u32,
        state: SupplicantState,
    },
    /// A scan just finished; scan results are available via
    /// [`WifiSupplicantBackend::get_scan_results`].
    ScanComplete { ifindex: u32 },
    /// The daemon's D-Bus name appeared (reconnect).
    DaemonUp,
    /// The daemon's D-Bus name disappeared.
    DaemonDown,
}

/// Supplicant-reported state. Coarser than Nexus's internal
/// [`nexus_core::WifiState`] — the backend's lifecycle module
/// translates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupplicantState {
    Disconnected {
        reason: DisconnectHint,
    },
    Scanning,
    Associating,
    Authenticating,
    FourWayHandshake,
    Connected {
        bssid: nexus_core::MacAddr,
        ssid: nexus_core::Ssid,
        frequency: u32,
    },
}

/// Coarse reason hint from the supplicant; finer-grained mapping
/// to `nexus_core::DisconnectReason` happens in the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectHint {
    Unspecified,
    AssociationTimeout,
    AuthFailure,
    HandshakeTimeout,
    BadCredentials,
    LocalRequest,
    DaemonUnavailable,
}

/// Trait implemented by every supplicant binding. Signatures match
/// DD-003 §4.1.
#[async_trait]
pub trait WifiSupplicantBackend: Send + Sync {
    /// Register a wireless interface.
    async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()>;
    /// Unregister a wireless interface.
    async fn detach(&mut self, ifindex: u32) -> Result<()>;
    /// Kick off a scan. Results arrive via `SupplicantEvent::ScanComplete`.
    async fn scan(&mut self, ifindex: u32, params: ScanParams) -> Result<()>;
    /// Most recent scan results from the supplicant's cache.
    async fn get_scan_results(&self, ifindex: u32) -> Result<Vec<BssInfo>>;
    /// Install a network configuration and select it for
    /// connection; returns an opaque handle. Per DD-003 §6.5 the
    /// caller is responsible for forgetting the previous handle
    /// before issuing a new `connect` for the same interface.
    async fn connect(&mut self, ifindex: u32, network: &NetworkConfig) -> Result<NetworkHandle>;
    /// Disconnect but keep the network configuration installed.
    async fn disconnect(&mut self, ifindex: u32) -> Result<()>;
    /// Remove a previously-added network.
    async fn forget_network(&mut self, ifindex: u32, handle: NetworkHandle) -> Result<()>;
    /// Trigger a roam.
    async fn roam(&mut self, ifindex: u32, target: RoamTarget) -> Result<()>;
    /// Query current signal quality.
    async fn signal_info(&self, ifindex: u32) -> Result<SignalInfo>;
    /// Backend identifier for logs and metrics.
    fn name(&self) -> &'static str;
}
