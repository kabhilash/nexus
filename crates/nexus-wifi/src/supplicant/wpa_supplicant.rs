//! wpa_supplicant-backed [`WifiSupplicantBackend`]. See DD-003 §9.
//!
//! This file is a compile-only scaffold. The full zbus client
//! implementation requires a live `fi.w1.wpa_supplicant1` daemon
//! which isn't present in the default CI container; the
//! mac80211_hwsim + hostapd integration harness from DD-003 §14.2
//! drives the end-to-end flow. Shape here matches the trait so
//! `Box<dyn WifiSupplicantBackend>` in production swaps from the
//! mock without churning call sites.
//!
//! Invariants from DD-003 §9 the real impl must honor:
//!
//! - `attach` uses `CreateInterface` with `Driver = "nl80211"`.
//! - Every security mode (§9.4 network-arg translation) has its
//!   own inline-table builder. PMF mode gating per §8.2.
//! - Subscribe to `PropertiesChanged` on the interface object;
//!   `ScanDone` and `BSSAdded` / `BSSRemoved` each have their own
//!   signal. Tasks abort on `detach` to drain subscriptions.
//! - On retry: `remove_network` on the previous handle before
//!   `add_network` + `select_network`.

use async_trait::async_trait;
use tokio::sync::broadcast;

use super::{SupplicantEvent, WifiSupplicantBackend};
use crate::error::{Result, WifiError};
use crate::types::{BssInfo, NetworkConfig, NetworkHandle, RoamTarget, ScanParams, SignalInfo};

pub struct WpaSupplicantBackend {
    #[allow(dead_code)]
    connection: zbus::Connection,
    #[allow(dead_code)]
    event_tx: broadcast::Sender<SupplicantEvent>,
}

impl WpaSupplicantBackend {
    pub async fn new(event_tx: broadcast::Sender<SupplicantEvent>) -> Result<Self> {
        let connection = zbus::Connection::system()
            .await
            .map_err(|e| WifiError::Supplicant {
                backend: "wpa_supplicant",
                source: Box::new(e),
            })?;
        // TODO: subscribe to `NameOwnerChanged` on
        //       `fi.w1.wpa_supplicant1` so daemon appearance /
        //       disappearance drives SupplicantEvent::DaemonUp /
        //       DaemonDown (DD-003 §12.1).
        Ok(Self {
            connection,
            event_tx,
        })
    }
}

/// Translate wpa_supplicant's `State` property string into a
/// [`super::SupplicantState`] shape per DD-003 §9.5.
pub fn translate_wpa_state(state_str: &str) -> Option<super::SupplicantState> {
    use super::{DisconnectHint, SupplicantState};
    match state_str {
        "scanning" => Some(SupplicantState::Scanning),
        "associating" => Some(SupplicantState::Associating),
        "authenticating" => Some(SupplicantState::Authenticating),
        "4way_handshake" | "group_handshake" => Some(SupplicantState::FourWayHandshake),
        "disconnected" => Some(SupplicantState::Disconnected {
            reason: DisconnectHint::Unspecified,
        }),
        // `associated` / `completed` / transitional states that
        // don't map 1:1 here are resolved by the PropertiesChanged
        // watcher once the BSSID / SSID fields arrive alongside
        // `completed`.
        _ => None,
    }
}

/// Translate a numeric 802.11 disconnect reason (the wire-format
/// `Reason Code` from IEEE 802.11-2020 Table 9-49) into the coarse
/// [`super::DisconnectHint`] consumed by the backend. See
/// DD-003 §9.6.
pub fn translate_disconnect_reason(code: i32) -> super::DisconnectHint {
    use super::DisconnectHint;
    match code {
        // Locally initiated disconnects
        -3 | 1 => DisconnectHint::LocalRequest,
        // Auth-related
        2 | 13 => DisconnectHint::AuthFailure,
        // 4-way handshake failure
        15 => DisconnectHint::HandshakeTimeout,
        // Association / driver timeout
        3 | 4 | 23 => DisconnectHint::AssociationTimeout,
        // PSK failures surface as reason 15 or via EAPOL events; be
        // tolerant and accept either path.
        _ => DisconnectHint::Unspecified,
    }
}

#[async_trait]
impl WifiSupplicantBackend for WpaSupplicantBackend {
    async fn attach(&mut self, _ifindex: u32, _ifname: &str) -> Result<()> {
        // TODO: WpaSupplicant1.CreateInterface with Driver="nl80211".
        //       On InterfaceExists, GetInterface + Disconnect.
        //       Subscribe to PropertiesChanged on the returned
        //       object path and relay state transitions through
        //       self.event_tx.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "zbus wiring lands with the hwsim harness".into(),
        })
    }

    async fn detach(&mut self, _ifindex: u32) -> Result<()> {
        // TODO: WpaSupplicant1.RemoveInterface; abort PropertiesChanged
        //       watcher task.
        Ok(())
    }

    async fn scan(&mut self, _ifindex: u32, _params: ScanParams) -> Result<()> {
        // TODO: Interface1.Scan(Type, SSIDs, Channels). ScanDone signal
        //       arrives on the interface object.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "not yet wired".into(),
        })
    }

    async fn get_scan_results(&self, _ifindex: u32) -> Result<Vec<BssInfo>> {
        // TODO: read Interface1.BSSs property, then for each path
        //       read BSS1.SSID/BSSID/Frequency/Signal/WPA/RSN.
        Ok(Vec::new())
    }

    async fn connect(&mut self, _ifindex: u32, _network: &NetworkConfig) -> Result<NetworkHandle> {
        // TODO: build the network dict per §9.4 (key_mgmt,
        //       ssid, psk / sae / eap, ieee80211w, priority,
        //       scan_freq, bssid_blacklist) and call
        //       Interface1.AddNetwork + SelectNetwork. Return the
        //       network object path as the opaque handle.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "not yet wired".into(),
        })
    }

    async fn disconnect(&mut self, _ifindex: u32) -> Result<()> {
        // TODO: Interface1.Disconnect.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "not yet wired".into(),
        })
    }

    async fn forget_network(&mut self, _ifindex: u32, _handle: NetworkHandle) -> Result<()> {
        // TODO: Interface1.RemoveNetwork(handle).
        Ok(())
    }

    async fn roam(&mut self, _ifindex: u32, _target: RoamTarget) -> Result<()> {
        // TODO: Interface1.Roam(bssid) for targeted roam;
        //       Interface1.Reassociate for auto.
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "not yet wired".into(),
        })
    }

    async fn signal_info(&self, _ifindex: u32) -> Result<SignalInfo> {
        // TODO: Interface1.SignalPoll returns an a{sv} with keys
        //       linkspeed/noise/frequency/rssi/…
        Err(WifiError::Supplicant {
            backend: "wpa_supplicant",
            source: "not yet wired".into(),
        })
    }

    fn name(&self) -> &'static str {
        "wpa_supplicant"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_table_covers_dd003_section_9_5() {
        use super::super::SupplicantState;
        assert!(matches!(
            translate_wpa_state("scanning"),
            Some(SupplicantState::Scanning)
        ));
        assert!(matches!(
            translate_wpa_state("associating"),
            Some(SupplicantState::Associating)
        ));
        assert!(matches!(
            translate_wpa_state("authenticating"),
            Some(SupplicantState::Authenticating)
        ));
        assert!(matches!(
            translate_wpa_state("4way_handshake"),
            Some(SupplicantState::FourWayHandshake)
        ));
        assert!(matches!(
            translate_wpa_state("group_handshake"),
            Some(SupplicantState::FourWayHandshake)
        ));
        assert!(matches!(
            translate_wpa_state("disconnected"),
            Some(SupplicantState::Disconnected { .. })
        ));
        assert!(translate_wpa_state("completed").is_none());
        assert!(translate_wpa_state("unknown_future").is_none());
    }

    #[test]
    fn disconnect_reasons_split_into_retriable_and_fail_fast() {
        use super::super::DisconnectHint;
        assert!(matches!(
            translate_disconnect_reason(15),
            DisconnectHint::HandshakeTimeout
        ));
        assert!(matches!(
            translate_disconnect_reason(2),
            DisconnectHint::AuthFailure
        ));
        assert!(matches!(
            translate_disconnect_reason(3),
            DisconnectHint::AssociationTimeout
        ));
        assert!(matches!(
            translate_disconnect_reason(-3),
            DisconnectHint::LocalRequest
        ));
        assert!(matches!(
            translate_disconnect_reason(999),
            DisconnectHint::Unspecified
        ));
    }
}
