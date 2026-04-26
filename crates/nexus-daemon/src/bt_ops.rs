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
//! Live: `bt_set_powered` only (the smallest slice needed for
//! `nexusctl bt power on/off`). Discoverable / Pairable / discovery
//! / device methods stay on `inner` (typically `NoopOps`) until a
//! follow-up commit grows the surface.

use std::sync::Arc;

use async_trait::async_trait;
use nexus_bluetooth::{BtCommand, BtError};
use nexus_dbus::{BackendOps, DbusError, ReloadReport, Result};
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
fn map_bt_error(e: BtError) -> DbusError {
    match e {
        BtError::UnknownAdapter(_) => DbusError::NotFound(e.to_string()),
        BtError::UnknownDevice(_) => DbusError::NotFound(e.to_string()),
        BtError::NotConnected => DbusError::ResourceBusy(e.to_string()),
        // BlueZ-side rejections (busy, timeouts, in-flight conflicts)
        // and pairing-flow errors come through as ResourceBusy so
        // clients get a "try again" hint without surfacing internal
        // states.
        _ => DbusError::ResourceBusy(e.to_string()),
    }
}

#[async_trait]
impl BackendOps for BtBackendOps {
    async fn bt_set_powered(&self, ifname: &str, on: bool) -> Result<()> {
        let (responder, reply) = oneshot::channel();
        self.commands
            .send(BtCommand::SetAdapterPowered {
                adapter: ifname.to_owned(),
                on,
                responder,
            })
            .await
            .map_err(|_| {
                DbusError::FeatureDisabled("bluetooth: backend channel closed".into())
            })?;
        match reply.await {
            Ok(r) => r.map_err(map_bt_error),
            Err(_) => Err(DbusError::FeatureDisabled(
                "bluetooth: backend dropped the reply".into(),
            )),
        }
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
}
