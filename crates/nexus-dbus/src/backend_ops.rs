//! Operator-driven actions the D-Bus layer routes to backends.
//!
//! This trait is the seam that lets the D-Bus crate talk to Wi-Fi
//! / Ethernet / Bluetooth / GNSS without depending on each
//! backend's specific command-channel type. Integrators (the daemon
//! crate) implement [`BackendOps`] by translating each call into
//! the appropriate `mpsc::Sender<…Command>` send + oneshot await.
//!
//! Every method has a default implementation that returns
//! [`crate::DbusError::Unsupported`], so a partial implementation
//! is valid: tests can pass a no-op [`NoopOps`], and integrators
//! can land backends incrementally without breaking compilation.

use async_trait::async_trait;
use nexus_core::{MacAddr, PairingAnswer, PairingJobId};
use ulid::Ulid;

use crate::errors::{DbusError, Result};
use crate::state::PowerState;

/// Result of a `Manager.ReloadConfig` call. See DD-006 §5.2.
///
/// `applied`  — sections whose new values took effect at runtime.
/// `deferred` — sections that differed from the live config but
///              require a daemon restart (e.g., `dbus.bus_name`). The
///              live value is unchanged; the next restart picks up
///              the new value.
/// `errors`   — `(section, reason)` pairs for sections that failed
///              to reload due to invalid values. The live value for
///              those sections is unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReloadReport {
    pub applied: Vec<String>,
    pub deferred: Vec<String>,
    pub errors: Vec<(String, String)>,
}

impl ReloadReport {
    pub fn is_empty(&self) -> bool {
        self.applied.is_empty() && self.deferred.is_empty() && self.errors.is_empty()
    }
}

/// Wi-Fi scan parameters parsed from the `Scan(params: a{sv})` dict.
#[derive(Debug, Clone, Default)]
pub struct ScanParams {
    pub active: bool,
    pub ssids: Vec<Vec<u8>>,
    pub frequencies: Vec<u32>,
    pub allow_roam: bool,
}

/// Bluetooth discovery filter parsed from the
/// `Bluetooth.StartDiscovery(filter: a{sv})` dict — DD-006 §6.4.
/// Mirrors `nexus_bluetooth::DiscoveryFilter`; kept here so the
/// `BackendOps` trait stays decoupled from the bluetooth crate. The
/// daemon's adapter translates this into the bluetooth-crate type
/// before sending the command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BtDiscoveryFilter {
    /// `None` means BlueZ's "auto"; otherwise one of `"auto"`,
    /// `"bredr"`, `"le"`. Anything else is rejected at the
    /// translation boundary.
    pub transport: Option<String>,
    pub rssi: Option<i16>,
    pub uuids: Vec<String>,
    pub duplicate_data: bool,
}

/// Wi-Fi roaming-mode strings — DD-006 §6.3 `RoamingMode` property.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoamingMode {
    Off,
    Supplicant,
    Nexus,
}

impl RoamingMode {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "off" => RoamingMode::Off,
            "supplicant" => RoamingMode::Supplicant,
            "nexus" => RoamingMode::Nexus,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            RoamingMode::Off => "off",
            RoamingMode::Supplicant => "supplicant",
            RoamingMode::Nexus => "nexus",
        }
    }
}

/// Backend-routing trait. Each method maps to a DD-006 §6
/// mutating method. The default impls return `Unsupported` so
/// integrators that haven't wired a backend yet still get a
/// well-defined D-Bus error rather than a silent panic.
#[async_trait]
pub trait BackendOps: Send + Sync {
    // ---- Wi-Fi (DD-006 §6.3) ----
    async fn wifi_scan(&self, _ifname: &str, _params: ScanParams) -> Result<()> {
        Err(DbusError::Unsupported("wifi_scan".into()))
    }
    async fn wifi_connect(&self, _ifname: &str, _profile_id: Ulid) -> Result<()> {
        Err(DbusError::Unsupported("wifi_connect".into()))
    }
    async fn wifi_disconnect(&self, _ifname: &str, _pause_auto_connect: bool) -> Result<()> {
        Err(DbusError::Unsupported("wifi_disconnect".into()))
    }
    async fn wifi_roam(&self, _ifname: &str, _bssid: MacAddr) -> Result<()> {
        Err(DbusError::Unsupported("wifi_roam".into()))
    }
    async fn wifi_set_powered(&self, _ifname: &str, _on: bool) -> Result<()> {
        Err(DbusError::Unsupported("wifi_set_powered".into()))
    }
    async fn wifi_set_roaming_mode(&self, _ifname: &str, _mode: RoamingMode) -> Result<()> {
        Err(DbusError::Unsupported("wifi_set_roaming_mode".into()))
    }
    /// `Wifi.ProvideCredential` — DD-006 §6.3, DD-003 §9.2. Hands
    /// an operator-supplied credential back to the supplicant as
    /// the reply to a prior `NetworkRequest` signal.
    async fn wifi_provide_credential(
        &self,
        _ifname: &str,
        _network: &str,
        _field: &str,
        _value: &str,
    ) -> Result<()> {
        Err(DbusError::Unsupported("wifi_provide_credential".into()))
    }

    // ---- Bluetooth (DD-006 §6.4 / §6.6) ----
    //
    // Adapter / device path arguments below are always BlueZ object
    // paths (`/org/bluez/hciN[/dev_AA_BB_…]`), never bare ifnames.
    // The kernel ifname is not a valid `zbus::ObjectPath` on its own
    // — the D-Bus interfaces look up the path from the registry
    // before calling these methods.

    /// `fi.nexus.Bluetooth.Powered` setter — flip the BlueZ adapter's
    /// `Powered` property via the live BlueZ client. Routes to
    /// `nexus_bluetooth::BtCommand::SetAdapterPowered`.
    async fn bt_set_powered(&self, _bluez_path: &str, _on: bool) -> Result<()> {
        Err(DbusError::Unsupported("bt_set_powered".into()))
    }

    /// `fi.nexus.Bluetooth.Discoverable` setter. Routes to
    /// `nexus_bluetooth::BtCommand::SetAdapterDiscoverable`.
    async fn bt_set_discoverable(&self, _bluez_path: &str, _on: bool) -> Result<()> {
        Err(DbusError::Unsupported("bt_set_discoverable".into()))
    }

    /// `fi.nexus.Bluetooth.Pairable` setter. Routes to
    /// `nexus_bluetooth::BtCommand::SetAdapterPairable`.
    async fn bt_set_pairable(&self, _bluez_path: &str, _on: bool) -> Result<()> {
        Err(DbusError::Unsupported("bt_set_pairable".into()))
    }

    /// `fi.nexus.Bluetooth.StartDiscovery(filter)`. Routes to
    /// `nexus_bluetooth::BtCommand::StartDiscovery`.
    async fn bt_start_discovery(
        &self,
        _bluez_path: &str,
        _filter: BtDiscoveryFilter,
    ) -> Result<()> {
        Err(DbusError::Unsupported("bt_start_discovery".into()))
    }

    /// `fi.nexus.Bluetooth.StopDiscovery()`. Routes to
    /// `nexus_bluetooth::BtCommand::StopDiscovery`.
    async fn bt_stop_discovery(&self, _bluez_path: &str) -> Result<()> {
        Err(DbusError::Unsupported("bt_stop_discovery".into()))
    }

    /// `fi.nexus.BluetoothDevice.Connect()`. Routes to
    /// `nexus_bluetooth::BtCommand::Connect`.
    async fn bt_connect_device(&self, _device_path: &str) -> Result<()> {
        Err(DbusError::Unsupported("bt_connect_device".into()))
    }

    /// `fi.nexus.BluetoothDevice.Disconnect()`. Routes to
    /// `nexus_bluetooth::BtCommand::Disconnect`.
    async fn bt_disconnect_device(&self, _device_path: &str) -> Result<()> {
        Err(DbusError::Unsupported("bt_disconnect_device".into()))
    }

    /// `fi.nexus.BluetoothDevice.Forget()`. Drops the device from
    /// BlueZ's registry AND from Nexus's profile store. Routes to
    /// `nexus_bluetooth::BtCommand::Forget`.
    async fn bt_forget_device(
        &self,
        _adapter_bluez_path: &str,
        _device_path: &str,
    ) -> Result<()> {
        Err(DbusError::Unsupported("bt_forget_device".into()))
    }

    /// `fi.nexus.BluetoothDevice.Trusted` setter. Routes to
    /// `nexus_bluetooth::BtCommand::SetDeviceTrusted`.
    async fn bt_set_trusted(&self, _device_path: &str, _on: bool) -> Result<()> {
        Err(DbusError::Unsupported("bt_set_trusted".into()))
    }

    /// `fi.nexus.Bluetooth.Pair(device)` / `fi.nexus.BluetoothDevice.Pair()`.
    /// Routes to `nexus_bluetooth::BtCommand::Pair`. Returns the
    /// pairing job id that correlates the `PairingPrompt` /
    /// `PairingComplete` signals fired on the adapter object
    /// (DD-006 §6.4).
    async fn bt_pair(&self, _device_path: &str) -> Result<PairingJobId> {
        Err(DbusError::Unsupported("bt_pair".into()))
    }

    /// `fi.nexus.Bluetooth.CancelPairing(device)` /
    /// `fi.nexus.BluetoothDevice.CancelPairing()`. Routes to
    /// `nexus_bluetooth::BtCommand::CancelPairing`.
    async fn bt_cancel_pairing(&self, _device_path: &str) -> Result<()> {
        Err(DbusError::Unsupported("bt_cancel_pairing".into()))
    }

    /// `fi.nexus.Bluetooth.AnswerPairingPrompt(job_id, answer)`.
    /// Routes to `nexus_bluetooth::BtCommand::AnswerPairingPrompt`,
    /// which validates `answer`'s shape against the pending prompt's
    /// kind (DD-006 §6.4's per-kind variant map) before resolving
    /// the Agent's pending oneshot.
    async fn bt_answer_pairing_prompt(
        &self,
        _job_id: PairingJobId,
        _answer: PairingAnswer,
    ) -> Result<()> {
        Err(DbusError::Unsupported("bt_answer_pairing_prompt".into()))
    }

    // ---- Manager-level (DD-006 §5) ----
    async fn set_power_state(&self, _state: PowerState) -> Result<()> {
        Err(DbusError::Unsupported("set_power_state".into()))
    }

    /// Re-read `nexus.toml` from disk, diff against the live config,
    /// apply what's safely reloadable at runtime, and return the
    /// applied/deferred/errors report. A structural parse error on
    /// the file propagates as [`DbusError::Io`] (the D-Bus layer
    /// maps that to `fi.nexus.Error.IoError` per DD-006 §5.2).
    async fn reload_config(&self) -> Result<ReloadReport> {
        Err(DbusError::Unsupported("reload_config".into()))
    }
}

/// Default no-op implementation. Every method returns
/// `Unsupported`, which lets tests instantiate the service
/// without wiring any real backends.
pub struct NoopOps;

impl NoopOps {
    pub fn arc() -> std::sync::Arc<dyn BackendOps> {
        std::sync::Arc::new(NoopOps)
    }
}

#[async_trait]
impl BackendOps for NoopOps {}

// ---------------------------------------------------------------------------
// Recording mock for tests
// ---------------------------------------------------------------------------

/// A test-friendly [`BackendOps`] that records every call. Tests
/// inspect [`RecordingOps::calls`] after a method exercise.
pub struct RecordingOps {
    inner: std::sync::Mutex<Vec<RecordedCall>>,
    /// When set, every backend method returns this error instead
    /// of `Ok(())`. Useful for verifying error propagation.
    next_error: std::sync::Mutex<Option<DbusError>>,
}

#[derive(Debug, Clone)]
pub enum RecordedCall {
    WifiScan { ifname: String, params: ScanParams },
    WifiConnect { ifname: String, profile_id: Ulid },
    WifiDisconnect { ifname: String, pause_auto_connect: bool },
    WifiRoam { ifname: String, bssid: MacAddr },
    WifiSetPowered { ifname: String, on: bool },
    BtSetPowered { ifname: String, on: bool },
    BtSetDiscoverable { adapter_path: String, on: bool },
    BtSetPairable { adapter_path: String, on: bool },
    BtStartDiscovery {
        adapter_path: String,
        filter: BtDiscoveryFilter,
    },
    BtStopDiscovery { adapter_path: String },
    BtConnectDevice { device_path: String },
    BtDisconnectDevice { device_path: String },
    BtForgetDevice {
        adapter_path: String,
        device_path: String,
    },
    BtSetTrusted { device_path: String, on: bool },
    BtPair { device_path: String },
    BtCancelPairing { device_path: String },
    BtAnswerPairingPrompt {
        job_id: PairingJobId,
        answer: PairingAnswer,
    },
    WifiSetRoamingMode { ifname: String, mode: RoamingMode },
    WifiProvideCredential {
        ifname: String,
        network: String,
        field: String,
        value: String,
    },
    SetPowerState(PowerState),
}

impl RecordingOps {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            inner: std::sync::Mutex::new(Vec::new()),
            next_error: std::sync::Mutex::new(None),
        })
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.inner.lock().unwrap().clone()
    }

    pub fn inject_error(&self, err: DbusError) {
        *self.next_error.lock().unwrap() = Some(err);
    }

    fn consume_error(&self) -> Option<DbusError> {
        self.next_error.lock().unwrap().take()
    }

    fn record(&self, call: RecordedCall) {
        self.inner.lock().unwrap().push(call);
    }
}

#[async_trait]
impl BackendOps for RecordingOps {
    async fn wifi_scan(&self, ifname: &str, params: ScanParams) -> Result<()> {
        self.record(RecordedCall::WifiScan {
            ifname: ifname.to_owned(),
            params,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn wifi_connect(&self, ifname: &str, profile_id: Ulid) -> Result<()> {
        self.record(RecordedCall::WifiConnect {
            ifname: ifname.to_owned(),
            profile_id,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn wifi_disconnect(&self, ifname: &str, pause_auto_connect: bool) -> Result<()> {
        self.record(RecordedCall::WifiDisconnect {
            ifname: ifname.to_owned(),
            pause_auto_connect,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn wifi_roam(&self, ifname: &str, bssid: MacAddr) -> Result<()> {
        self.record(RecordedCall::WifiRoam {
            ifname: ifname.to_owned(),
            bssid,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn wifi_set_powered(&self, ifname: &str, on: bool) -> Result<()> {
        self.record(RecordedCall::WifiSetPowered {
            ifname: ifname.to_owned(),
            on,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_set_powered(&self, bluez_path: &str, on: bool) -> Result<()> {
        self.record(RecordedCall::BtSetPowered {
            ifname: bluez_path.to_owned(),
            on,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_set_discoverable(&self, bluez_path: &str, on: bool) -> Result<()> {
        self.record(RecordedCall::BtSetDiscoverable {
            adapter_path: bluez_path.to_owned(),
            on,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_set_pairable(&self, bluez_path: &str, on: bool) -> Result<()> {
        self.record(RecordedCall::BtSetPairable {
            adapter_path: bluez_path.to_owned(),
            on,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_start_discovery(
        &self,
        bluez_path: &str,
        filter: BtDiscoveryFilter,
    ) -> Result<()> {
        self.record(RecordedCall::BtStartDiscovery {
            adapter_path: bluez_path.to_owned(),
            filter,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_stop_discovery(&self, bluez_path: &str) -> Result<()> {
        self.record(RecordedCall::BtStopDiscovery {
            adapter_path: bluez_path.to_owned(),
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_connect_device(&self, device_path: &str) -> Result<()> {
        self.record(RecordedCall::BtConnectDevice {
            device_path: device_path.to_owned(),
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_disconnect_device(&self, device_path: &str) -> Result<()> {
        self.record(RecordedCall::BtDisconnectDevice {
            device_path: device_path.to_owned(),
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_forget_device(
        &self,
        adapter_bluez_path: &str,
        device_path: &str,
    ) -> Result<()> {
        self.record(RecordedCall::BtForgetDevice {
            adapter_path: adapter_bluez_path.to_owned(),
            device_path: device_path.to_owned(),
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_set_trusted(&self, device_path: &str, on: bool) -> Result<()> {
        self.record(RecordedCall::BtSetTrusted {
            device_path: device_path.to_owned(),
            on,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_pair(&self, device_path: &str) -> Result<PairingJobId> {
        self.record(RecordedCall::BtPair {
            device_path: device_path.to_owned(),
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(PairingJobId(Ulid::new()))
    }
    async fn bt_cancel_pairing(&self, device_path: &str) -> Result<()> {
        self.record(RecordedCall::BtCancelPairing {
            device_path: device_path.to_owned(),
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn bt_answer_pairing_prompt(
        &self,
        job_id: PairingJobId,
        answer: PairingAnswer,
    ) -> Result<()> {
        self.record(RecordedCall::BtAnswerPairingPrompt { job_id, answer });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn wifi_set_roaming_mode(&self, ifname: &str, mode: RoamingMode) -> Result<()> {
        self.record(RecordedCall::WifiSetRoamingMode {
            ifname: ifname.to_owned(),
            mode,
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn wifi_provide_credential(
        &self,
        ifname: &str,
        network: &str,
        field: &str,
        value: &str,
    ) -> Result<()> {
        self.record(RecordedCall::WifiProvideCredential {
            ifname: ifname.to_owned(),
            network: network.to_owned(),
            field: field.to_owned(),
            value: value.to_owned(),
        });
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
    async fn set_power_state(&self, state: PowerState) -> Result<()> {
        self.record(RecordedCall::SetPowerState(state));
        if let Some(e) = self.consume_error() {
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn noop_returns_unsupported() {
        let ops = NoopOps;
        let result = ops.wifi_scan("wlan0", ScanParams::default()).await;
        assert!(matches!(result, Err(DbusError::Unsupported(_))));
    }

    #[tokio::test]
    async fn recording_collects_calls() {
        let ops = RecordingOps::new();
        ops.wifi_disconnect("wlan0", false).await.unwrap();
        ops.set_power_state(PowerState::Sleep).await.unwrap();
        let calls = ops.calls();
        assert_eq!(calls.len(), 2);
        assert!(matches!(calls[0], RecordedCall::WifiDisconnect { .. }));
        assert!(matches!(
            calls[1],
            RecordedCall::SetPowerState(PowerState::Sleep)
        ));
    }

    #[tokio::test]
    async fn recording_captures_bt_set_powered() {
        let ops = RecordingOps::new();
        ops.bt_set_powered("/org/bluez/hci0", true).await.unwrap();
        let calls = ops.calls();
        assert_eq!(calls.len(), 1);
        match &calls[0] {
            RecordedCall::BtSetPowered { ifname, on } => {
                assert_eq!(ifname, "/org/bluez/hci0");
                assert!(*on);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn roaming_mode_parses() {
        assert_eq!(RoamingMode::parse("off"), Some(RoamingMode::Off));
        assert_eq!(
            RoamingMode::parse("supplicant"),
            Some(RoamingMode::Supplicant)
        );
        assert_eq!(RoamingMode::parse("nexus"), Some(RoamingMode::Nexus));
        assert_eq!(RoamingMode::parse("bogus"), None);
    }
}
