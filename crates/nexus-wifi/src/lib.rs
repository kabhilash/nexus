//! Wi-Fi Backend. See `docs/dd-003-wifi-backend.md`.
//!
//! This crate owns the per-interface Wi-Fi lifecycle: scan
//! scheduling, profile matching, connection flow, retry/blacklist,
//! roaming, signal polling, and supplicant crash recovery. IP-layer
//! configuration is systemd-networkd's responsibility and lives
//! outside Nexus.
//!
//! The public entry point is [`spawn_wifi_backend`]. Callers wire
//! up the `NexusEvent` bus, a supplicant implementation (typically
//! [`supplicant::MockSupplicant`] in tests or the
//! `wpa_supplicant`-backed real one in production), a
//! [`ProfileStore`], and the receive side of a
//! [`WifiCommand`] channel that bridges operator actions from the
//! D-Bus surface. The backend drains events on its own task; the
//! returned [`WifiBackendHandle`] exposes power-state mutation and
//! the shutdown token.
//!
//! # What's done today (per DD-003 phases)
//!
//! | Phase | Status |
//! |-------|--------|
//! | 1 Skeleton + Lifecycle       | ✅ |
//! | 2 Supplicant trait + Mock    | ✅ |
//! | 3 Profile store + matching   | ✅ (w/ hot reload via `ProfileChanged`) |
//! | 4 wpa_supplicant backend     | ✅ all 7 security modes + PMF (§8.2) + FT (§7.4) |
//! | 5 Scanning                   | ✅ scheduler + SSIDs/Channels narrowing |
//! | 6 Connect + failure handling | ✅ retry, blacklist, credentials-invalid |
//! | 7 Roaming                    | ✅ modes off/supplicant/nexus + hysteresis + signal-poll |
//! | 8 Supplicant crash recovery  | ✅ NameOwnerChanged → re-attach + rescan |
//! | 9 Power management           | ✅ scan cadence per PowerState; Sleep → pause |
//! | 10 iwd backend               | ⏳ placeholder module; out of scope for v0.x |
//! | 11 Metrics + integration     | ✅ metric coverage; hwsim harness needs CI kernel |
//!
//! # Known residuals (Tier 3+ gap review, post-0.x)
//!
//! - `WifiBackendOps::wifi_set_powered` is still pass-through to
//!   `NoopOps`. Power-state control needs rtnl / rfkill plumbing
//!   that lives outside this crate.
//! - The iwd backend (`supplicant::iwd`) remains an empty
//!   placeholder — no production path exercises it today, and the
//!   wpa_supplicant backend covers every platform we currently
//!   target.
//! - Integration testing against mac80211_hwsim + hostapd (DD-003
//!   §14.2) is gated by the `integration-linux` Cargo feature;
//!   the harness itself still needs to be written when a
//!   kernel-module-capable CI runner becomes available.

use std::sync::Arc;

use nexus_core::NexusEvent;
use nexus_profile_store::ProfileStore;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod backend;
pub mod error;
pub mod lifecycle;
pub mod metrics;
pub mod power;
pub mod profile;
pub mod retry;
pub mod roam;
pub mod scan;
pub mod select;
pub mod supplicant;
pub mod types;

pub use backend::{WifiBackend, WifiConfig};
pub use error::{Result, WifiError};
pub use power::PowerState;
pub use supplicant::{SupplicantEvent, WifiSupplicantBackend};

/// Operator-driven command routed from the D-Bus layer into the
/// backend's event loop. Each variant carries a oneshot reply so
/// callers can distinguish "dispatched OK" from per-ifname errors
/// like [`WifiError::NotAttached`]. Kept public so the daemon crate
/// can construct a [`mpsc::Sender<WifiCommand>`] that bridges
/// [`nexus_dbus::BackendOps`] into the backend.
pub enum WifiCommand {
    /// Trigger a scan on `ifname`. Maps 1:1 to
    /// [`WifiSupplicantBackend::scan`]; results arrive later via
    /// `NexusEvent::WifiScanComplete`.
    Scan {
        ifname: String,
        params: types::ScanParams,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Operator-initiated connect to a named profile. Looks the
    /// profile up in the backend's in-memory cache (populated at
    /// startup from [`ProfileStore::load_wifi`]), translates it
    /// via [`crate::profile::to_network_config`], and calls
    /// [`WifiSupplicantBackend::connect`]. Bypasses the automatic
    /// selection path's rate-limit + blacklist checks — an
    /// operator-explicit connect isn't in the same class as the
    /// backend's own retry loop.
    Connect {
        ifname: String,
        profile_id: ulid::Ulid,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Disconnect the current session. Matches DD-006 §6.3
    /// `Wifi.Disconnect()` semantics: the supplicant tears down
    /// the association; the network entry is kept so future
    /// auto-connect attempts don't have to rebuild it.
    Disconnect {
        ifname: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Targeted roam to `bssid`. Only meaningful in `roam_mode =
    /// "nexus"`; in `"off"` / `"supplicant"` modes the supplicant
    /// is responsible for roam decisions and this call is a no-op
    /// on its end (but we still route it through for observability).
    Roam {
        ifname: String,
        bssid: nexus_core::MacAddr,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Change the roaming mode on a live interface. Affects the
    /// backend's own scan scheduling (roam evaluation only runs
    /// under `Nexus` mode) and — for `Supplicant` — hands the
    /// decision back to wpa_supplicant.
    SetRoamingMode {
        ifname: String,
        mode: types::RoamMode,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Handle returned by [`spawn_wifi_backend`]. Drop the handle to
/// stop the backend — the [`CancellationToken`] it holds is clone
/// of the one passed to `run`.
pub struct WifiBackendHandle {
    pub join: JoinHandle<Result<()>>,
    pub shutdown: CancellationToken,
    pub power: Arc<RwLock<PowerState>>,
}

/// Default depth for callers that use [`command_channel`]. Chosen
/// so a burst of D-Bus requests can queue without backpressure,
/// but a stuck backend eventually surfaces as `channel full` rather
/// than silent OOM.
pub const COMMAND_CHANNEL_DEPTH: usize = 32;

/// Convenience constructor for the command channel. Callers that
/// need to keep the sender alive outside the supervised wifi task
/// (the daemon's D-Bus layer does) create the channel here and
/// pass the receiver into [`spawn_wifi_backend`].
pub fn command_channel() -> (mpsc::Sender<WifiCommand>, mpsc::Receiver<WifiCommand>) {
    mpsc::channel(COMMAND_CHANNEL_DEPTH)
}

/// Spawn the Wi-Fi backend event loop. See DD-003 §§3, 5, 6, 7.
pub fn spawn_wifi_backend(
    event_tx: broadcast::Sender<NexusEvent>,
    supplicant_tx: broadcast::Sender<SupplicantEvent>,
    supplicant: Box<dyn WifiSupplicantBackend>,
    profile_store: Arc<dyn ProfileStore>,
    config: WifiConfig,
    commands: mpsc::Receiver<WifiCommand>,
) -> WifiBackendHandle {
    metrics::register();
    let backend = WifiBackend::new(
        event_tx,
        supplicant_tx,
        supplicant,
        profile_store,
        config,
        commands,
    );
    let power = backend.power_handle();
    let shutdown = CancellationToken::new();
    let shutdown_child = shutdown.clone();
    let join = tokio::spawn(async move { backend.run(shutdown_child).await });
    WifiBackendHandle {
        join,
        shutdown,
        power,
    }
}

/// Test helper: build a [`SecretString`](nexus_profile_store::SecretString)
/// from a plain string literal. Kept here (rather than duplicated
/// per-module) so the unit tests for `profile.rs` and `select.rs`
/// share a single spelling.
#[cfg(test)]
pub(crate) fn secretstring(value: &str) -> nexus_profile_store::SecretString {
    nexus_profile_store::SecretString::from(value)
}
