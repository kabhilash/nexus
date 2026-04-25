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

// iwd backend is deferred per DD-003 Phase 10. Enabling the
// feature should fail loudly rather than silently compiling a
// no-op so integrators don't ship a build that quietly drops
// every supplicant call.
#[cfg(feature = "wifi-iwd")]
mod iwd {
    compile_error!(
        "wifi-iwd backend is not implemented; see DD-003 §10 / Phase 10. \
         Disable the `wifi-iwd` Cargo feature until the backend lands."
    );
}

use async_trait::async_trait;

use crate::error::Result;
use crate::types::{BssInfo, NetworkConfig, NetworkHandle, RoamTarget, ScanParams, SignalInfo};

pub use mock::{CredentialReply, MockBehavior, MockSupplicant, MockSupplicantHandle};

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
    /// [`WifiSupplicantBackend::get_scan_results`]. `success`
    /// mirrors wpa_supplicant's `ScanDone(success)` arg — `false`
    /// means the scan was started but no fresh results landed in
    /// the BSS cache (driver busy, radar NOP, etc.). The backend
    /// still processes the event so its scheduler advances; the
    /// scans-total metric uses this to label the outcome.
    ScanComplete { ifindex: u32, success: bool },
    /// The supplicant added or removed a BSS in its cache outside
    /// of a driven scan (Beacon update, age-out). The backend
    /// refreshes the local BSS cache without emitting a public
    /// `WifiScanComplete` event — clients subscribe to those for
    /// scheduled scans, not background freshness updates. S5 /
    /// DD-003 §9.2.
    BssCacheStale { ifindex: u32 },
    /// The daemon's D-Bus name appeared (reconnect).
    DaemonUp,
    /// The daemon's D-Bus name disappeared.
    DaemonDown,
    /// The supplicant is asking for a credential (OTP, password,
    /// passphrase, smartcard PIN, ...) to complete an ongoing
    /// authentication. The backend forwards this as
    /// `NexusEvent::WifiNetworkRequest`; the operator replies via
    /// [`WifiSupplicantBackend::provide_network_credential`].
    /// See DD-003 §9.2.
    NetworkRequest {
        ifindex: u32,
        /// Opaque supplicant-side network object path. Round-trips
        /// verbatim through the reply.
        network: String,
        /// Which credential the supplicant wants (`password`,
        /// `passphrase`, `otp`, `pin`, …).
        field: String,
        /// Human-readable prompt text the supplicant suggests
        /// showing the operator.
        text: String,
    },
}

/// Supplicant-reported state. Coarser than Nexus's internal
/// [`nexus_core::WifiState`] — the backend's lifecycle module
/// translates. DD-003 §9.5: `associating` and `associated` are
/// split into their own variants here so downstream consumers
/// can tell pre-association from post-association without
/// re-deriving from peer events; both still translate to
/// `WifiState::Connecting` at the public layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupplicantState {
    Disconnected {
        reason: DisconnectHint,
    },
    Scanning,
    /// Authentication request sent to the AP; waiting for the
    /// auth response (`State = "associating"` on wpa_supplicant).
    Associating,
    /// Auth response received; association request issued; waiting
    /// for the assoc response — wpa_supplicant's
    /// `State = "associated"`. The pre-4-way window. DD-003 §9.5.
    Associated,
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
/// Variants track the IEEE 802.11 reason-code buckets in DD-003
/// §9.6; [`BadCredentials`](Self::BadCredentials) is a promoted
/// hint the backend sets once an auth/key failure has repeated
/// past the per-BSSID retry threshold (DD-003 §6.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisconnectHint {
    Unspecified,
    /// 802.11 reason 17 — association timeout from the AP.
    AssociationTimeout,
    /// 802.11 reasons 2 / 13 — "previous auth no longer valid" /
    /// "Invalid IE". Distinct from `HandshakeTimeout` and
    /// `EapFailure`.
    AuthFailure,
    /// 802.11 reason 15 — 4-way handshake timeout. For WPA2-PSK
    /// this is almost always wrong PSK in practice; the backend
    /// promotes it to `BadCredentials` after repeats (§6.3).
    HandshakeTimeout,
    /// 802.11 reason 23 — 802.1X EAP authentication failed.
    EapFailure,
    /// 802.11 reason 1 — AP-initiated deauth, unspecified reason.
    ApInitiated,
    /// 802.11 reason 4 — inactivity deauth.
    Inactivity,
    /// 802.11 reasons 6 / 7 — class-2/3-frame protocol glitches.
    ProtocolError,
    /// Promoted by the backend after repeated auth failures. Maps
    /// to `DisconnectReason::CredentialsInvalid`, which is the only
    /// reason `is_permanent()` returns `true`.
    BadCredentials,
    /// Nexus-initiated disconnect (the supplicant's negative -3
    /// code) or 802.11 reason 3 ("STA leaving").
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
    /// Reply to a prior [`SupplicantEvent::NetworkRequest`]. The
    /// `network` string is the opaque path the request carried;
    /// `field` is echoed back so the supplicant can correlate the
    /// reply to the outstanding prompt. Implementations that don't
    /// support dynamic credential requests (e.g. the mock) may
    /// return `WifiError::NotAttached` or leave the default
    /// no-op below.
    async fn provide_network_credential(
        &mut self,
        ifindex: u32,
        network: &str,
        field: &str,
        value: &str,
    ) -> Result<()> {
        // Silence the unused-argument warnings in the default impl;
        // real backends override this entirely.
        let _ = (ifindex, network, field, value);
        Ok(())
    }
    /// Backend identifier for logs and metrics.
    fn name(&self) -> &'static str;
}
