//! [`BackendOps`] adapter that routes Bluetooth method calls from
//! the D-Bus layer into the live [`nexus_bluetooth`] backend.
//!
//! Mirrors the Wi-Fi adapter's shape ([`crate::wifi_ops::WifiBackendOps`]):
//!
//! ```text
//!     fi.nexus.Bluetooth.Powered = true     (D-Bus property set)
//!       └─ BluetoothIface::set_powered       (nexus-dbus)
//!           └─ BackendOps::bt_set_powered
//!               └─ ReloadOps (pass-through)
//!                   └─ BtBackendOps::bt_set_powered  ← here
//!                       └─ mpsc::send(BtCommand::SetAdapterPowered)
//!                           └─ BluetoothBackend::on_command
//!                               └─ BluezClient::set_powered
//!                                   └─ Properties.Set on org.bluez.Adapter1
//! ```
//!
//! Live: `bt_set_powered`, `bt_set_discoverable`, `bt_set_pairable`,
//! `bt_set_trusted`, `bt_start_discovery`, `bt_stop_discovery`,
//! `bt_connect_device`, `bt_disconnect_device`, `bt_forget_device`,
//! `bt_pair`, `bt_cancel_pairing`, `bt_answer_pairing_prompt`.

use std::sync::Arc;

use async_trait::async_trait;
use nexus_bluetooth::{
    BtCommand, BtError, DiscoveryFilter, DiscoveryTransport,
};
use nexus_core::{PairingAnswer, PairingJobId};
use nexus_dbus::{BackendOps, BtDiscoveryFilter, DbusError, ReloadReport, Result};
use tokio::sync::{mpsc, oneshot};

/// `BackendOps` impl that forwards Bluetooth commands over a channel
/// into the spawned [`nexus_bluetooth::BluetoothBackend`]. Holds a
/// pass-through `inner` for every method it doesn't override —
/// keeps the layering ([NoopOps → BtBackendOps → WifiBackendOps →
/// ReloadOps]) consistent with the Wi-Fi adapter.
pub struct BtBackendOps {
    commands: mpsc::Sender<BtCommand>,
    inner: Arc<dyn BackendOps>,
}

impl BtBackendOps {
    pub fn new(commands: mpsc::Sender<BtCommand>, inner: Arc<dyn BackendOps>) -> Arc<Self> {
        Arc::new(Self { commands, inner })
    }
}

/// Map a [`BtError`] from the backend onto the D-Bus error vocabulary.
/// Most failures classify as `ResourceBusy` (bluez is up but
/// something disagreed) rather than `NotFound`; the latter is
/// reserved for "BlueZ doesn't know this adapter."
///
/// Pairing-specific mappings follow DD-006 §6.4's per-method error
/// lists: `Pair` documents `InvalidState` for "device already
/// pairing or paired" (→ `AlreadyPairing`), `AnswerPairingPrompt`
/// documents `UnknownPairingJob` and `InvalidArgument` for "wrong
/// variant type for the prompt kind" (→ `InvalidPromptAnswer`, which
/// carries `nexus_bluetooth::pairing::validate_answer`'s reason
/// text). `UnknownPairingJob` itself maps onto the same generic
/// `NotFound` category `UnknownDevice`/`UnknownAdapter` already use —
/// see DD-006 §11's error-name-vs-category note in
/// `nexus_dbus::errors`, the wire name comes from the message
/// prefix, not a dedicated enum variant per DD-006 label.
fn map_bt_error(e: BtError) -> DbusError {
    match e {
        BtError::UnknownAdapter(_) => DbusError::NotFound(e.to_string()),
        BtError::UnknownDevice(_) => DbusError::NotFound(e.to_string()),
        BtError::UnknownPairingJob(_) => DbusError::NotFound(e.to_string()),
        BtError::AlreadyPairing => DbusError::InvalidState(e.to_string()),
        BtError::InvalidPromptAnswer(_) => DbusError::InvalidArgument(e.to_string()),
        BtError::NotConnected => DbusError::ResourceBusy(e.to_string()),
        // BlueZ-side rejections (busy, timeouts, in-flight conflicts)
        // and everything else (including a prompt oneshot the Agent
        // gave up waiting on) come through as ResourceBusy so clients
        // get a "try again" hint without surfacing internal states.
        _ => DbusError::ResourceBusy(e.to_string()),
    }
}

/// Translate the cross-crate [`BtDiscoveryFilter`] view into the
/// nexus-bluetooth-private [`DiscoveryFilter`]. An unrecognised
/// `transport` string short-circuits to `InvalidArgument` so the
/// operator gets a useful error before BlueZ does.
fn convert_filter(f: BtDiscoveryFilter) -> std::result::Result<DiscoveryFilter, DbusError> {
    let transport = match f.transport.as_deref() {
        None => None,
        Some("auto") => Some(DiscoveryTransport::Auto),
        Some("bredr") => Some(DiscoveryTransport::Bredr),
        Some("le") => Some(DiscoveryTransport::Le),
        Some(other) => {
            return Err(DbusError::InvalidArgument(format!(
                "transport must be 'auto'|'bredr'|'le', got '{other}'"
            )));
        }
    };
    Ok(DiscoveryFilter {
        transport,
        rssi: f.rssi,
        uuids: f.uuids,
        duplicate_data: f.duplicate_data,
    })
}

/// Send a [`BtCommand`] (built by the closure with our oneshot
/// sender) and wait for its reply. Channel-closed and
/// reply-dropped both surface as `FeatureDisabled` (matches the
/// Wi-Fi adapter's vocabulary).
async fn dispatch(
    commands: &mpsc::Sender<BtCommand>,
    make: impl FnOnce(oneshot::Sender<nexus_bluetooth::Result<()>>) -> BtCommand,
) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    commands
        .send(make(tx))
        .await
        .map_err(|_| DbusError::FeatureDisabled("bluetooth: backend channel closed".into()))?;
    match rx.await {
        Ok(r) => r.map_err(map_bt_error),
        Err(_) => Err(DbusError::FeatureDisabled(
            "bluetooth: backend dropped the reply".into(),
        )),
    }
}

#[async_trait]
impl BackendOps for BtBackendOps {
    async fn bt_set_powered(&self, bluez_path: &str, on: bool) -> Result<()> {
        let adapter = bluez_path.to_owned();
        dispatch(&self.commands, |responder| BtCommand::SetAdapterPowered {
            adapter,
            on,
            responder,
        })
        .await
    }

    async fn bt_set_discoverable(&self, bluez_path: &str, on: bool) -> Result<()> {
        let adapter = bluez_path.to_owned();
        dispatch(&self.commands, |responder| {
            BtCommand::SetAdapterDiscoverable {
                adapter,
                on,
                responder,
            }
        })
        .await
    }

    async fn bt_set_pairable(&self, bluez_path: &str, on: bool) -> Result<()> {
        let adapter = bluez_path.to_owned();
        dispatch(&self.commands, |responder| BtCommand::SetAdapterPairable {
            adapter,
            on,
            responder,
        })
        .await
    }

    async fn bt_start_discovery(
        &self,
        bluez_path: &str,
        filter: BtDiscoveryFilter,
    ) -> Result<()> {
        let adapter = bluez_path.to_owned();
        let filter = convert_filter(filter)?;
        dispatch(&self.commands, |responder| BtCommand::StartDiscovery {
            adapter,
            filter,
            responder,
        })
        .await
    }

    async fn bt_stop_discovery(&self, bluez_path: &str) -> Result<()> {
        let adapter = bluez_path.to_owned();
        dispatch(&self.commands, |responder| BtCommand::StopDiscovery {
            adapter,
            responder,
        })
        .await
    }

    async fn bt_connect_device(&self, device_path: &str) -> Result<()> {
        let device_path = device_path.to_owned();
        dispatch(&self.commands, |responder| BtCommand::Connect {
            device_path,
            responder,
        })
        .await
    }

    async fn bt_disconnect_device(&self, device_path: &str) -> Result<()> {
        let device_path = device_path.to_owned();
        dispatch(&self.commands, |responder| BtCommand::Disconnect {
            device_path,
            responder,
        })
        .await
    }

    async fn bt_forget_device(
        &self,
        adapter_bluez_path: &str,
        device_path: &str,
    ) -> Result<()> {
        let adapter = adapter_bluez_path.to_owned();
        let device_path = device_path.to_owned();
        dispatch(&self.commands, |responder| BtCommand::Forget {
            adapter,
            device_path,
            responder,
        })
        .await
    }

    async fn bt_set_trusted(&self, device_path: &str, on: bool) -> Result<()> {
        let device_path = device_path.to_owned();
        dispatch(&self.commands, |responder| BtCommand::SetDeviceTrusted {
            device_path,
            on,
            responder,
        })
        .await
    }

    /// `bt_pair` returns a `PairingJobId`, not `()`, so it can't use
    /// the `dispatch` helper (hardcoded to `Result<()>` responders) —
    /// inlined here rather than generalizing `dispatch` for the one
    /// caller that needs a typed reply.
    async fn bt_pair(&self, device_path: &str) -> Result<PairingJobId> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(BtCommand::Pair {
                device_path: device_path.to_owned(),
                responder: tx,
            })
            .await
            .map_err(|_| DbusError::FeatureDisabled("bluetooth: backend channel closed".into()))?;
        match rx.await {
            Ok(r) => r.map_err(map_bt_error),
            Err(_) => Err(DbusError::FeatureDisabled(
                "bluetooth: backend dropped the reply".into(),
            )),
        }
    }

    async fn bt_cancel_pairing(&self, device_path: &str) -> Result<()> {
        let device_path = device_path.to_owned();
        dispatch(&self.commands, |responder| BtCommand::CancelPairing {
            device_path,
            responder,
        })
        .await
    }

    async fn bt_answer_pairing_prompt(
        &self,
        job_id: PairingJobId,
        answer: PairingAnswer,
    ) -> Result<()> {
        dispatch(&self.commands, |responder| {
            BtCommand::AnswerPairingPrompt {
                job_id,
                answer,
                responder,
            }
        })
        .await
    }

    // ---- Pass-throughs (every other method falls through to
    // whatever's wrapped, typically the Wi-Fi adapter or NoopOps). ----

    async fn wifi_scan(
        &self,
        ifname: &str,
        params: nexus_dbus::ScanParams,
    ) -> Result<()> {
        self.inner.wifi_scan(ifname, params).await
    }
    async fn wifi_connect(&self, ifname: &str, profile_id: ulid::Ulid) -> Result<()> {
        self.inner.wifi_connect(ifname, profile_id).await
    }
    async fn wifi_disconnect(&self, ifname: &str, pause_auto_connect: bool) -> Result<()> {
        self.inner.wifi_disconnect(ifname, pause_auto_connect).await
    }
    async fn wifi_roam(&self, ifname: &str, bssid: nexus_core::MacAddr) -> Result<()> {
        self.inner.wifi_roam(ifname, bssid).await
    }
    async fn wifi_set_powered(&self, ifname: &str, on: bool) -> Result<()> {
        self.inner.wifi_set_powered(ifname, on).await
    }
    async fn wifi_set_roaming_mode(
        &self,
        ifname: &str,
        mode: nexus_dbus::RoamingMode,
    ) -> Result<()> {
        self.inner.wifi_set_roaming_mode(ifname, mode).await
    }
    async fn wifi_provide_credential(
        &self,
        ifname: &str,
        network: &str,
        field: &str,
        value: &str,
    ) -> Result<()> {
        self.inner
            .wifi_provide_credential(ifname, network, field, value)
            .await
    }
    async fn set_power_state(&self, state: nexus_dbus::PowerState) -> Result<()> {
        self.inner.set_power_state(state).await
    }
    async fn reload_config(&self) -> Result<ReloadReport> {
        self.inner.reload_config().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_dbus::NoopOps;

    #[tokio::test]
    async fn set_powered_forwards_command_and_waits_for_reply() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<BtCommand>(4);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(BtCommand::SetAdapterPowered {
                adapter,
                on,
                responder,
            }) = cmd_rx.recv().await
            {
                // The adapter argument is a BlueZ object path —
                // nexus-bluetooth's adapter_proxy parses it as an
                // ObjectPath. Bare ifnames would fail validation.
                assert_eq!(adapter, "/org/bluez/hci0");
                assert!(on);
                let _ = responder.send(Ok(()));
            }
        });
        assert!(
            ops.bt_set_powered("/org/bluez/hci0", true)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn set_powered_surfaces_adapter_not_found() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<BtCommand>(4);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(BtCommand::SetAdapterPowered { responder, .. }) = cmd_rx.recv().await {
                let _ = responder.send(Err(BtError::UnknownAdapter("/org/bluez/hci9".into())));
            }
        });
        let err = ops
            .bt_set_powered("/org/bluez/hci9", true)
            .await
            .unwrap_err();
        assert!(matches!(err, DbusError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn set_powered_fails_cleanly_when_backend_channel_closed() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<BtCommand>(4);
        drop(cmd_rx);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        let err = ops
            .bt_set_powered("/org/bluez/hci0", false)
            .await
            .unwrap_err();
        assert!(matches!(err, DbusError::FeatureDisabled(_)), "{err:?}");
    }

    #[tokio::test]
    async fn non_bt_methods_pass_through_to_inner() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<BtCommand>(1);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        let err = ops.reload_config().await.unwrap_err();
        assert!(matches!(err, DbusError::Unsupported(_)));
    }

    #[tokio::test]
    async fn start_discovery_passes_filter_and_dispatches() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<BtCommand>(4);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(BtCommand::StartDiscovery {
                adapter,
                filter,
                responder,
            }) = cmd_rx.recv().await
            {
                assert_eq!(adapter, "/org/bluez/hci0");
                // Filter should be translated 1:1.
                assert!(matches!(filter.transport, Some(DiscoveryTransport::Le)));
                assert_eq!(filter.rssi, Some(-70));
                let _ = responder.send(Ok(()));
            }
        });
        let f = BtDiscoveryFilter {
            transport: Some("le".into()),
            rssi: Some(-70),
            uuids: Vec::new(),
            duplicate_data: false,
        };
        assert!(
            ops.bt_start_discovery("/org/bluez/hci0", f)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn start_discovery_rejects_unknown_transport() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<BtCommand>(4);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        let f = BtDiscoveryFilter {
            transport: Some("infrared".into()),
            ..Default::default()
        };
        let err = ops
            .bt_start_discovery("/org/bluez/hci0", f)
            .await
            .unwrap_err();
        assert!(matches!(err, DbusError::InvalidArgument(_)), "{err:?}");
    }

    #[tokio::test]
    async fn stop_discovery_dispatches_with_adapter() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<BtCommand>(4);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(BtCommand::StopDiscovery { adapter, responder }) = cmd_rx.recv().await {
                assert_eq!(adapter, "/org/bluez/hci0");
                let _ = responder.send(Ok(()));
            }
        });
        assert!(ops.bt_stop_discovery("/org/bluez/hci0").await.is_ok());
    }

    #[tokio::test]
    async fn connect_device_dispatches_with_device_path() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<BtCommand>(4);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(BtCommand::Connect {
                device_path,
                responder,
            }) = cmd_rx.recv().await
            {
                assert_eq!(device_path, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
                let _ = responder.send(Ok(()));
            }
        });
        assert!(
            ops.bt_connect_device("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF")
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn forget_device_passes_adapter_and_device() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<BtCommand>(4);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(BtCommand::Forget {
                adapter,
                device_path,
                responder,
            }) = cmd_rx.recv().await
            {
                assert_eq!(adapter, "/org/bluez/hci0");
                assert_eq!(device_path, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
                let _ = responder.send(Ok(()));
            }
        });
        assert!(
            ops.bt_forget_device(
                "/org/bluez/hci0",
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn set_trusted_dispatches_set_device_trusted() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<BtCommand>(4);
        let ops = BtBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(BtCommand::SetDeviceTrusted {
                device_path,
                on,
                responder,
            }) = cmd_rx.recv().await
            {
                assert_eq!(device_path, "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF");
                assert!(on);
                let _ = responder.send(Ok(()));
            }
        });
        assert!(
            ops.bt_set_trusted(
                "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF",
                true,
            )
            .await
            .is_ok()
        );
    }
}
