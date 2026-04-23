//! Per-interface lifecycle state. See DD-003 §3.
//!
//! This crate re-exports [`nexus_core::WifiState`] as its lifecycle
//! type — DD-003's `WifiInterfaceState` is the same enum from a
//! wire-type perspective, and sharing the definition guarantees the
//! `NexusEvent::WifiStateChanged` payload stays in lockstep with
//! the backend's internal state machine.

use nexus_core::{InterfaceInfo, WifiState};

use crate::types::{BssCapabilities, RoamMode};

/// Alias kept for clarity when reading DD-003.
pub type WifiInterfaceState = WifiState;

/// Short metric label for the `state` tag in
/// `nexus_wifi_interfaces_managed`.
pub fn state_label(state: &WifiState) -> &'static str {
    match state {
        WifiState::Idle => "idle",
        WifiState::Scanning => "scanning",
        WifiState::Connecting { .. } => "connecting",
        WifiState::Authenticating { .. } => "authenticating",
        WifiState::Handshaking { .. } => "handshaking",
        WifiState::Connected { .. } => "connected",
        WifiState::Roaming { .. } => "roaming",
        WifiState::Disconnected { .. } => "disconnected",
        WifiState::Gone => "gone",
    }
}

/// True when the interface should suspend scheduled scans. Per
/// DD-003 §5.4, scanning suspends in `Connected` unless the
/// roaming mode is `Nexus` (signal degradation may still trigger a
/// targeted rescan).
pub fn scans_suspended(state: &WifiState, roam_mode: RoamMode) -> bool {
    matches!(state, WifiState::Connected { .. }) && roam_mode != RoamMode::Nexus
}

/// Per-interface record the backend keeps in its registry.
#[derive(Debug, Clone)]
pub struct WifiInterfaceEntry {
    pub info: InterfaceInfo,
    pub state: WifiState,
    pub roam_mode: RoamMode,
    /// Most recent capabilities parsed from scan results for the
    /// currently-associated BSS; used for 802.11r gating.
    pub current_bss_capabilities: Option<BssCapabilities>,
}

impl WifiInterfaceEntry {
    pub fn new(info: InterfaceInfo) -> Self {
        Self {
            info,
            state: WifiState::Idle,
            roam_mode: RoamMode::default(),
            current_bss_capabilities: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use nexus_core::{MacAddr, SecurityMode, Ssid};

    use super::*;

    fn connected_state() -> WifiState {
        WifiState::Connected {
            bssid: MacAddr([0xAA; 6]),
            ssid: Ssid::new(b"x".to_vec()).unwrap(),
            frequency: 2412,
            signal_dbm: -50,
            security: SecurityMode::Wpa2Psk,
        }
    }

    #[test]
    fn state_labels_cover_every_variant() {
        assert_eq!(state_label(&WifiState::Idle), "idle");
        assert_eq!(state_label(&WifiState::Scanning), "scanning");
        assert_eq!(state_label(&connected_state()), "connected");
        assert_eq!(state_label(&WifiState::Gone), "gone");
    }

    #[test]
    fn scans_suspended_only_when_connected_without_nexus_roam() {
        assert!(scans_suspended(&connected_state(), RoamMode::Supplicant));
        assert!(scans_suspended(&connected_state(), RoamMode::Off));
        assert!(!scans_suspended(&connected_state(), RoamMode::Nexus));
        assert!(!scans_suspended(&WifiState::Idle, RoamMode::Supplicant));
        assert!(!scans_suspended(&WifiState::Scanning, RoamMode::Supplicant));
    }
}
