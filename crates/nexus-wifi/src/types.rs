//! Wi-Fi-local supporting types. See DD-003 §§4.1-4.3.
//!
//! `SecurityMode` lives in `nexus-core` because it appears in
//! `NexusEvent::WifiState { security }`; `NetworkConfig`,
//! `ScanParams`, `BssInfo`, and the roam / handle types are
//! Wi-Fi-backend-local and live here.

use nexus_core::{MacAddr, SecurityMode, Ssid};
use nexus_profile_store::SecurityConfig;

/// Scan parameters accepted by [`crate::supplicant::WifiSupplicantBackend::scan`].
#[derive(Debug, Clone, Default)]
pub struct ScanParams {
    /// Empty = broadcast scan.
    pub ssids: Vec<Ssid>,
    /// Empty = all supported channels.
    pub frequencies: Vec<u32>,
    /// `true` for active scans (probe requests); `false` for
    /// passive (listen-only).
    pub active: bool,
    /// `true` only when the roaming mode is `supplicant` or when
    /// the backend is explicitly evaluating a roam (DD-003 §4.1).
    pub allow_roam: bool,
}

/// Scan result entry. See DD-003 §4.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BssInfo {
    pub bssid: MacAddr,
    pub ssid: Ssid,
    pub frequency: u32,
    pub signal_dbm: i32,
    pub capabilities: BssCapabilities,
    /// BSSes may offer multiple modes simultaneously (e.g., WPA2/WPA3
    /// transition). All advertised modes are captured so the
    /// selector can pick the best match.
    pub security: Vec<SecurityMode>,
    /// Age of the most recent beacon / probe response in
    /// milliseconds.
    pub age_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BssCapabilities {
    pub ht: bool,
    pub vht: bool,
    pub he: bool,
    pub eht: bool,
    pub ft: bool,
    pub pmf_required: bool,
    pub pmf_capable: bool,
    pub wps: bool,
}

/// Output of profile → supplicant translation. Consumed by
/// [`crate::supplicant::WifiSupplicantBackend::connect`].
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub ssid: Ssid,
    pub hidden: bool,
    pub security: SecurityConfig,
    pub priority: i32,
    pub bssid_preferred: Option<MacAddr>,
    pub bssid_blacklist: Vec<MacAddr>,
    pub scan_freqs: Vec<u32>,
}

/// Opaque supplicant-side network identifier. DD-003 §4.1 requires
/// each `connect()` call return a fresh handle; the backend forgets
/// the previous handle before issuing a new connect against the
/// same interface (§6.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkHandle(pub String);

impl From<&str> for NetworkHandle {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Target for [`crate::supplicant::WifiSupplicantBackend::roam`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoamTarget {
    /// Let the supplicant pick the next BSS.
    Auto,
    /// Explicitly roam to this BSSID.
    Bss(MacAddr),
}

/// Signal quality snapshot from `NL80211_CMD_GET_STATION`. See
/// DD-003 §7.2.
#[derive(Debug, Clone, PartialEq)]
pub struct SignalInfo {
    pub bssid: MacAddr,
    pub rssi_dbm: i32,
    pub noise_dbm: Option<i32>,
    pub snr_db: Option<i32>,
    pub tx_bitrate_mbps: f32,
    pub rx_bitrate_mbps: f32,
    pub frequency: u32,
}

/// Per-profile disposition toward the supplicant roaming logic. See
/// DD-003 §7.1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoamMode {
    /// Never roam; stay on the current BSS until disconnection.
    Off,
    /// Let the supplicant's internal logic drive roaming.
    #[default]
    Supplicant,
    /// Nexus explicitly decides when to roam.
    Nexus,
}

impl RoamMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RoamMode::Off => "off",
            RoamMode::Supplicant => "supplicant",
            RoamMode::Nexus => "nexus",
        }
    }
}

/// Convert the profile-store security config to this crate's alias.
/// Kept as a one-liner helper so the rest of the codebase doesn't
/// have to reach into nexus-profile-store directly for the variant
/// names.
pub fn profile_security(config: &SecurityConfig) -> SecurityConfig {
    config.clone()
}
