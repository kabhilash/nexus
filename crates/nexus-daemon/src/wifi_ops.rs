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
//! Live: `wifi_scan`, `wifi_connect`, `wifi_disconnect`, `wifi_roam`,
//! `wifi_set_roaming_mode`, and `wifi_set_powered` — each forwards a
//! [`nexus_wifi::WifiCommand`] to the backend task and awaits its
//! [`oneshot`] reply. `SetPowered` flips soft-rfkill through the
//! backend's `/dev/rfkill` writer (see `nexus-wifi::rfkill`).
//!
//! Pass-through: `set_power_state` and `reload_config` still fall
//! through to `inner` (typically `NoopOps`).

use std::sync::Arc;

use async_trait::async_trait;
use nexus_core::MacAddr;
use nexus_dbus::{BackendOps, DbusError, ReloadReport, Result, RoamingMode, ScanParams};
use nexus_wifi::{WifiCommand, WifiError, types as wifi_types};
use tokio::sync::{mpsc, oneshot};
use ulid::Ulid;

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
        WifiError::UnknownInterface { .. } => DbusError::NotFound(e.to_string()),
        WifiError::NoProfileMatch { .. } => DbusError::NotFound(e.to_string()),
        WifiError::ProfileNotFound { .. } => DbusError::NotFound(e.to_string()),
        // Supplicant-layer failures manifest as transient (busy) at
        // the D-Bus surface — the client's typical response is to
        // wait and retry. Prefix the message with "supplicant:" so
        // the origin is visible without a dedicated error name.
        WifiError::Supplicant { .. } => DbusError::ResourceBusy(e.to_string()),
        WifiError::ProfileStore(_) => DbusError::Io(std::io::Error::other(e.to_string())),
        // rfkill failures surface as Io — the ultimate source is
        // a `write(2)` against `/dev/rfkill` or a missing plumbing
        // bit at startup; neither is an auth or rate-limit class.
        WifiError::Rfkill { .. } => DbusError::Io(std::io::Error::other(e.to_string())),
    }
}

/// Translate nexus-dbus's `RoamingMode` enum into nexus-wifi's
/// `RoamMode`. Identical shape; separate crates keep them distinct
/// so the D-Bus surface can evolve independently.
fn convert_roaming_mode(m: RoamingMode) -> wifi_types::RoamMode {
    match m {
        RoamingMode::Off => wifi_types::RoamMode::Off,
        RoamingMode::Supplicant => wifi_types::RoamMode::Supplicant,
        RoamingMode::Nexus => wifi_types::RoamMode::Nexus,
    }
}

/// Small helper: send a [`WifiCommand`], await its oneshot reply,
/// and map the nested errors. Every `wifi_*` method below follows
/// the same pattern — four nearly-identical blocks would bury the
/// interesting bit, so factor it out.
async fn dispatch(
    commands: &mpsc::Sender<WifiCommand>,
    make: impl FnOnce(oneshot::Sender<nexus_wifi::Result<()>>) -> WifiCommand,
) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    commands
        .send(make(tx))
        .await
        .map_err(|_| DbusError::FeatureDisabled("wifi: backend channel closed".into()))?;
    match rx.await {
        Ok(r) => r.map_err(map_wifi_error),
        Err(_) => Err(DbusError::FeatureDisabled(
            "wifi: backend dropped the reply".into(),
        )),
    }
}

#[async_trait]
impl BackendOps for WifiBackendOps {
    async fn wifi_scan(&self, ifname: &str, params: ScanParams) -> Result<()> {
        let ifname = ifname.to_owned();
        let params = convert_params(params);
        dispatch(&self.commands, |tx| WifiCommand::Scan {
            ifname,
            params,
            reply: tx,
        })
        .await
    }

    async fn wifi_connect(&self, ifname: &str, profile_id: Ulid) -> Result<()> {
        let ifname = ifname.to_owned();
        dispatch(&self.commands, |tx| WifiCommand::Connect {
            ifname,
            profile_id,
            reply: tx,
        })
        .await
    }

    async fn wifi_disconnect(&self, ifname: &str) -> Result<()> {
        let ifname = ifname.to_owned();
        dispatch(&self.commands, |tx| WifiCommand::Disconnect {
            ifname,
            reply: tx,
        })
        .await
    }

    async fn wifi_roam(&self, ifname: &str, bssid: MacAddr) -> Result<()> {
        let ifname = ifname.to_owned();
        dispatch(&self.commands, |tx| WifiCommand::Roam {
            ifname,
            bssid,
            reply: tx,
        })
        .await
    }

    async fn wifi_set_roaming_mode(&self, ifname: &str, mode: RoamingMode) -> Result<()> {
        let ifname = ifname.to_owned();
        let mode = convert_roaming_mode(mode);
        dispatch(&self.commands, |tx| WifiCommand::SetRoamingMode {
            ifname,
            mode,
            reply: tx,
        })
        .await
    }

    // `Powered` on a Wi-Fi interface is defined as "rfkill
    // released" (DD-006 §6.3). Route through the backend, which
    // owns a `/dev/rfkill` writer (`nexus-wifi::rfkill`). The
    // writer issues `RFKILL_OP_CHANGE` against the wiphy the
    // interface is bound to; the read path surfaces the new state
    // back through `NexusEvent::WifiRfkillChanged`.
    async fn wifi_set_powered(&self, ifname: &str, on: bool) -> Result<()> {
        let ifname = ifname.to_owned();
        dispatch(&self.commands, |tx| WifiCommand::SetPowered {
            ifname,
            on,
            reply: tx,
        })
        .await
    }

    async fn wifi_provide_credential(
        &self,
        ifname: &str,
        network: &str,
        field: &str,
        value: &str,
    ) -> Result<()> {
        let ifname = ifname.to_owned();
        let network = network.to_owned();
        let field = field.to_owned();
        let value = value.to_owned();
        dispatch(&self.commands, |tx| WifiCommand::ProvideCredential {
            ifname,
            network,
            field,
            value,
            reply: tx,
        })
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

    #[tokio::test]
    async fn connect_forwards_profile_id_and_waits_for_reply() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        let id = Ulid::new();
        tokio::spawn(async move {
            if let Some(WifiCommand::Connect {
                ifname,
                profile_id,
                reply,
            }) = cmd_rx.recv().await
            {
                assert_eq!(ifname, "wlan0");
                assert_eq!(profile_id, id);
                let _ = reply.send(Ok(()));
            }
        });
        assert!(ops.wifi_connect("wlan0", id).await.is_ok());
    }

    #[tokio::test]
    async fn connect_surfaces_profile_not_found_as_not_found() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(WifiCommand::Connect { reply, .. }) = cmd_rx.recv().await {
                let _ = reply.send(Err(WifiError::ProfileNotFound {
                    id: "01H...".into(),
                }));
            }
        });
        let err = ops.wifi_connect("wlan0", Ulid::new()).await.unwrap_err();
        assert!(matches!(err, DbusError::NotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn disconnect_forwards_and_succeeds() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(WifiCommand::Disconnect { ifname, reply }) = cmd_rx.recv().await {
                assert_eq!(ifname, "wlan0");
                let _ = reply.send(Ok(()));
            }
        });
        assert!(ops.wifi_disconnect("wlan0").await.is_ok());
    }

    #[tokio::test]
    async fn roam_passes_bssid_through() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        let bssid = MacAddr([0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01]);
        tokio::spawn(async move {
            if let Some(WifiCommand::Roam {
                ifname,
                bssid: got,
                reply,
            }) = cmd_rx.recv().await
            {
                assert_eq!(ifname, "wlan0");
                assert_eq!(got, bssid);
                let _ = reply.send(Ok(()));
            }
        });
        assert!(ops.wifi_roam("wlan0", bssid).await.is_ok());
    }

    #[tokio::test]
    async fn set_roaming_mode_translates_enum() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(WifiCommand::SetRoamingMode { mode, reply, .. }) = cmd_rx.recv().await {
                // Verify the dbus → wifi translation hit the right
                // variant.
                assert!(matches!(mode, wifi_types::RoamMode::Nexus));
                let _ = reply.send(Ok(()));
            }
        });
        assert!(
            ops.wifi_set_roaming_mode("wlan0", RoamingMode::Nexus)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn set_powered_forwards_command_and_waits_for_reply() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<WifiCommand>(4);
        let ops = WifiBackendOps::new(cmd_tx, NoopOps::arc());
        tokio::spawn(async move {
            if let Some(WifiCommand::SetPowered { ifname, on, reply }) = cmd_rx.recv().await {
                assert_eq!(ifname, "wlan0");
                assert!(on);
                let _ = reply.send(Ok(()));
            }
        });
        assert!(ops.wifi_set_powered("wlan0", true).await.is_ok());
    }
}
