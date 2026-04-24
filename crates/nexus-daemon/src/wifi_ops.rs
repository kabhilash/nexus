//! [`BackendOps`] adapter that routes Wi-Fi method calls from the
//! D-Bus layer into the live [`nexus_wifi`] backend.
//!
//! Shape matches the established layering:
//!
//! ```text
//!     fi.nexus.Wifi.Scan          (D-Bus method)
//!       └─ WifiIface::scan         (nexus-dbus)
//!           └─ BackendOps::wifi_scan
//!               └─ ReloadOps (pass-through)
//!                   └─ WifiBackendOps::wifi_scan  ← here
//!                       └─ mpsc::send(WifiCommand::Scan)
//!                           └─ WifiBackend::on_command
//!                               └─ WpaSupplicantBackend::scan
//!                                   └─ Interface1.Scan (wpa_supplicant D-Bus)
//! ```
//!
//! Only the Wi-Fi method leg is live today. Every other method
//! delegates to the `inner` impl so the caller can layer
//! [`crate::ReloadOps`] on top and `NoopOps::arc()` at the bottom.
//! Wiring additional methods (connect, disconnect, roam,
//! set_powered, set_roaming_mode) will grow this module as the
//! corresponding [`nexus_wifi::WifiCommand`] variants land.

use std::sync::Arc;

use async_trait::async_trait;
use nexus_core::MacAddr;
use nexus_dbus::{BackendOps, DbusError, ReloadReport, Result, RoamingMode, ScanParams};
use nexus_wifi::{WifiCommand, WifiError, types as wifi_types};
use tokio::sync::{mpsc, oneshot};

/// `BackendOps` impl that forwards Wi-Fi commands over a channel
/// into the spawned [`nexus_wifi::WifiBackend`].
pub struct WifiBackendOps {
    commands: mpsc::Sender<WifiCommand>,
    inner: Arc<dyn BackendOps>,
}

impl WifiBackendOps {
    pub fn new(commands: mpsc::Sender<WifiCommand>, inner: Arc<dyn BackendOps>) -> Arc<Self> {
        Arc::new(Self { commands, inner })
    }
}

/// Map a Wi-Fi-crate `ScanParams` view of the D-Bus `ScanParams`.
/// Field names are identical; the Wi-Fi crate's struct also tracks
/// `ssids: Vec<Ssid>` (typed) rather than `Vec<Vec<u8>>` (raw), so
/// we have to validate + drop malformed entries here.
fn convert_params(p: ScanParams) -> wifi_types::ScanParams {
    use nexus_core::Ssid;
    let ssids = p
        .ssids
        .into_iter()
        .filter_map(|b| Ssid::new(b).ok())
        .collect();
    wifi_types::ScanParams {
        active: p.active,
        ssids,
        frequencies: p.frequencies,
        allow_roam: p.allow_roam,
    }
}

/// Map a [`WifiError`] (from inside the backend) onto the D-Bus
/// error vocabulary. Keeps the "not attached" case distinct —
/// clients care whether the error was "backend doesn't know this
/// ifname" vs "wpa_supplicant rejected the call."
fn map_wifi_error(e: WifiError) -> DbusError {
    match e {
        WifiError::NotAttached { .. } => DbusError::NotFound(e.to_string()),
        WifiError::NoProfileMatch { .. } => DbusError::NotFound(e.to_string()),
        // Supplicant-layer failures manifest as transient (busy) at
        // the D-Bus surface — the client's typical response is to
        // wait and retry. Prefix the message with "supplicant:" so
        // the origin is visible without a dedicated error name.
        WifiError::Supplicant { .. } => DbusError::ResourceBusy(e.to_string()),
        WifiError::ProfileStore(_) => DbusError::Io(std::io::Error::other(e.to_string())),
    }
}

#[async_trait]
impl BackendOps for WifiBackendOps {
    async fn wifi_scan(&self, ifname: &str, params: ScanParams) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.commands
            .send(WifiCommand::Scan {
                ifname: ifname.to_owned(),
                params: convert_params(params),
                reply: tx,
            })
            .await
            .map_err(|_| {
                // Backend task is gone — feature is effectively
                // disabled until the daemon restarts.
                DbusError::FeatureDisabled("wifi: backend channel closed".into())
            })?;
        match rx.await {
            Ok(r) => r.map_err(map_wifi_error),
            Err(_) => Err(DbusError::FeatureDisabled(
                "wifi: backend dropped the scan reply".into(),
            )),
        }
    }

    // Everything else delegates downward. As new WifiCommand
    // variants land (connect, disconnect, roam, set_powered,
    // set_roaming_mode), each gets its own override here.

    async fn wifi_connect(&self, ifname: &str, profile_id: ulid::Ulid) -> Result<()> {
        self.inner.wifi_connect(ifname, profile_id).await
    }
    async fn wifi_disconnect(&self, ifname: &str) -> Result<()> {
        self.inner.wifi_disconnect(ifname).await
    }
    async fn wifi_roam(&self, ifname: &str, bssid: MacAddr) -> Result<()> {
        self.inner.wifi_roam(ifname, bssid).await
    }
    async fn wifi_set_powered(&self, ifname: &str, on: bool) -> Result<()> {
        self.inner.wifi_set_powered(ifname, on).await
    }
    async fn wifi_set_roaming_mode(&self, ifname: &str, mode: RoamingMode) -> Result<()> {
        self.inner.wifi_set_roaming_mode(ifname, mode).await
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
    async fn scan_forwards_command_and_waits_for_reply() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());

        // Spawn a fake backend task that replies Ok(()).
        tokio::spawn(async move {
            if let Some(WifiCommand::Scan {
                ifname,
                params,
                reply,
            }) = cmd_rx.recv().await
            {
                assert_eq!(ifname, "wlan0");
                assert!(params.active);
                let _ = reply.send(Ok(()));
            }
        });

        let res = ops
            .wifi_scan(
                "wlan0",
                ScanParams {
                    active: true,
                    ..Default::default()
                },
            )
            .await;
        assert!(res.is_ok(), "{res:?}");
    }

    #[tokio::test]
    async fn scan_surfaces_not_attached_as_not_found() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(WifiCommand::Scan { reply, .. }) = cmd_rx.recv().await {
                let _ = reply.send(Err(WifiError::NotAttached { ifindex: 0 }));
            }
        });
        let err = ops
            .wifi_scan("wlan0", ScanParams::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DbusError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn scan_fails_cleanly_when_backend_channel_closed() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<WifiCommand>(4);
        drop(cmd_rx);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        let err = ops
            .wifi_scan("wlan0", ScanParams::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DbusError::FeatureDisabled(_)), "{err:?}");
    }

    #[tokio::test]
    async fn supplicant_error_surfaces_as_resource_busy() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(WifiCommand::Scan { reply, .. }) = cmd_rx.recv().await {
                let _ = reply.send(Err(WifiError::Supplicant {
                    backend: "wpa_supplicant",
                    source: "bus error".into(),
                }));
            }
        });
        let err = ops
            .wifi_scan("wlan0", ScanParams::default())
            .await
            .unwrap_err();
        assert!(matches!(err, DbusError::ResourceBusy(_)), "{err:?}");
    }

    #[tokio::test]
    async fn non_wifi_methods_pass_through_to_inner() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<WifiCommand>(1);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        // reload_config on NoopOps returns Unsupported.
        let err = ops.reload_config().await.unwrap_err();
        assert!(matches!(err, DbusError::Unsupported(_)));
    }
}
