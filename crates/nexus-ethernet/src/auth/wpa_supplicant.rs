//! wpa_supplicant-backed [`WiredAuthBackend`]. See DD-002 §6.
//!
//! This file is a compile-only scaffold today. The full
//! implementation requires a live `fi.w1.wpa_supplicant1` daemon
//! which isn't available in the default CI container; the hostapd +
//! FreeRADIUS integration-test harness from DD-002 §10.2 drives the
//! end-to-end path. The types here match the trait shape so callers
//! can hold a `Box<dyn WiredAuthBackend>` in production builds and
//! swap in the real client once the integration harness lands.
//!
//! The key invariants from DD-002 §6 the real implementation must
//! uphold:
//!
//! - `attach` uses `CreateInterface` with `Driver = "wired"`.
//! - `authenticate` clears any prior `active_network` before adding
//!   the new one (otherwise retries leak entries and eventually
//!   break supplicant network selection).
//! - The network dictionary sets `eapol_flags = 0` — without this,
//!   wired 802.1X hangs forever waiting for a 4-way handshake that
//!   never comes.
//! - The `PropertiesChanged` watcher task is stored in
//!   `RegisteredInterface.watcher` and aborted on detach so D-Bus
//!   subscriptions drain.

use std::collections::HashMap;

use async_trait::async_trait;
use futures_util::StreamExt;
use nexus_core::{AuthFailureReason, NexusEvent};
use nexus_profile_store::Dot1xEapConfig;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

use super::{AuthState, WiredAuthBackend};
use crate::error::{EthernetError, Result};

const WPA_SUPPLICANT_BUS_NAME: &str = "fi.w1.wpa_supplicant1";

/// Production wired 802.1X backend. EAP attach / authenticate /
/// detach are still stubs (tracked alongside DD-002 §6); what *is*
/// live is system-bus connection + a `NameOwnerChanged` watcher on
/// `fi.w1.wpa_supplicant1`, which is enough for DD-002 §§9.1-9.2
/// crash-recovery semantics to work end-to-end.
pub struct WpaSupplicantWiredBackend {
    #[allow(dead_code)]
    connection: zbus::Connection,
    event_tx: broadcast::Sender<NexusEvent>,
    registered: HashMap<u32, RegisteredInterface>,
    /// Background task that translates `NameOwnerChanged` on
    /// `fi.w1.wpa_supplicant1` into
    /// `NexusEvent::EthAuthBackendOwnerChanged`. Aborted on drop.
    name_watcher: JoinHandle<()>,
}

impl Drop for WpaSupplicantWiredBackend {
    fn drop(&mut self) {
        self.name_watcher.abort();
    }
}

struct RegisteredInterface {
    #[allow(dead_code)]
    ifname: String,
    #[allow(dead_code)]
    dbus_path: String,
    #[allow(dead_code)]
    active_network: Option<String>,
    watcher: JoinHandle<()>,
}

impl WpaSupplicantWiredBackend {
    /// Connect to the system bus, install a `NameOwnerChanged` watch
    /// on `fi.w1.wpa_supplicant1`, and emit an initial
    /// `EthAuthBackendOwnerChanged` reflecting whether the daemon is
    /// currently up. DD-002 §6.1 / §§9.1-9.2.
    pub async fn new(event_tx: broadcast::Sender<NexusEvent>) -> Result<Self> {
        let connection =
            zbus::Connection::system()
                .await
                .map_err(|e| EthernetError::AuthDaemon {
                    backend: "wpa_supplicant",
                    source: Box::new(e),
                })?;
        let name_watcher = spawn_name_watcher(connection.clone(), event_tx.clone());
        Ok(Self {
            connection,
            event_tx,
            registered: HashMap::new(),
            name_watcher,
        })
    }

    fn ensure_attached(&self, ifindex: u32) -> Result<()> {
        if self.registered.contains_key(&ifindex) {
            Ok(())
        } else {
            Err(EthernetError::NotAttached { ifindex })
        }
    }

    /// Emit an `EthAuthStateChanged` without doing any D-Bus work.
    /// Used by placeholder paths + tests that exercise the trait
    /// with a real-but-disconnected backend.
    fn emit(&self, ifindex: u32, state: AuthState) {
        let _ = self
            .event_tx
            .send(NexusEvent::EthAuthStateChanged { ifindex, state });
    }
}

/// Spawn the `NameOwnerChanged` watcher for `fi.w1.wpa_supplicant1`.
/// Emits an initial `EthAuthBackendOwnerChanged` reflecting the
/// current owner so the lifecycle layer doesn't need to wait for a
/// transition to learn the daemon's state.
fn spawn_name_watcher(
    connection: zbus::Connection,
    event_tx: broadcast::Sender<NexusEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let dbus = match zbus::fdo::DBusProxy::new(&connection).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "wpa_supplicant wired: DBus proxy init failed");
                return;
            }
        };

        let bus_name = match WPA_SUPPLICANT_BUS_NAME.try_into() {
            Ok(n) => n,
            Err(_) => return,
        };
        let initial_present = dbus
            .get_name_owner(bus_name)
            .await
            .map(|owner| !owner.is_empty())
            .unwrap_or(false);
        let _ = event_tx.send(NexusEvent::EthAuthBackendOwnerChanged {
            backend: "wpa_supplicant".to_owned(),
            present: initial_present,
        });

        let mut stream = match dbus.receive_name_owner_changed().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "wpa_supplicant wired: NameOwnerChanged subscribe failed",
                );
                return;
            }
        };
        while let Some(sig) = stream.next().await {
            let Ok(args) = sig.args() else { continue };
            if args.name() != WPA_SUPPLICANT_BUS_NAME {
                continue;
            }
            let present = args
                .new_owner()
                .as_ref()
                .map(|s| !s.as_str().is_empty())
                .unwrap_or(false);
            let _ = event_tx.send(NexusEvent::EthAuthBackendOwnerChanged {
                backend: "wpa_supplicant".to_owned(),
                present,
            });
        }
    })
}

#[async_trait]
impl WiredAuthBackend for WpaSupplicantWiredBackend {
    async fn attach(&mut self, ifindex: u32, ifname: &str) -> Result<()> {
        // TODO: call WpaSupplicant1::CreateInterface with
        //       { Ifname: ifname, Driver: "wired" } and record the
        //       returned object path. On InterfaceExists, fall back
        //       to GetInterface + Disconnect to take ownership.
        //       Subscribe to PropertiesChanged and spawn the watcher
        //       task — see DD-002 §6.2.
        let watcher = tokio::spawn(async {});
        self.registered.insert(
            ifindex,
            RegisteredInterface {
                ifname: ifname.to_owned(),
                dbus_path: format!("/fi/w1/wpa_supplicant1/Interfaces/eth_{ifindex}"),
                active_network: None,
                watcher,
            },
        );
        Ok(())
    }

    async fn authenticate(&mut self, ifindex: u32, _config: &Dot1xEapConfig) -> Result<()> {
        self.ensure_attached(ifindex)?;
        // TODO(DD-002 §6.3): build the network dict
        //   - key_mgmt=IEEE8021X, eap, identity, anonymous_identity
        //   - ca_cert / client_cert / private_key / private_key_passwd
        //   - phase2, domain_suffix_match
        //   - eapol_flags=0 (CRITICAL for wired; see §6.3 note)
        // Remove entry.active_network before add_network so retries
        // don't accumulate.
        //
        // Until the EAP path lands, surface a synthetic
        // `Failed{Other("wpa_supplicant_stub")}` rather than
        // returning `Err`. Returning `Err` from this trait method
        // leaves the lifecycle parked in `Authenticating` forever
        // because the backend's caller only logs and moves on.
        // Emitting `Failed` lets the retry/operator-notification
        // machinery in `EthernetBackend::on_auth_state_changed`
        // handle it as the operational gap it is.
        tracing::warn!(
            ifindex,
            "wpa_supplicant wired auth not yet implemented; emitting Failed{{Other}}",
        );
        self.emit(
            ifindex,
            AuthState::Failed {
                reason: AuthFailureReason::Other("wpa_supplicant_stub".to_owned()),
            },
        );
        Ok(())
    }

    async fn detach(&mut self, ifindex: u32) -> Result<()> {
        if let Some(entry) = self.registered.remove(&ifindex) {
            entry.watcher.abort();
            // TODO: call WpaSupplicant1::RemoveInterface on
            //       entry.dbus_path. Best-effort; benign if the
            //       daemon already removed it.
        }
        Ok(())
    }

    async fn state(&self, ifindex: u32) -> Result<AuthState> {
        self.ensure_attached(ifindex)?;
        // TODO: read supplicant's `State` property via
        //       fi.w1.wpa_supplicant1.Interface.State and translate
        //       via the table in DD-002 §6.4.
        Ok(AuthState::Idle)
    }

    fn name(&self) -> &'static str {
        "wpa_supplicant"
    }
}

/// Translate wpa_supplicant's `State` property string to an
/// [`AuthState`]. Public so the D-Bus watcher task (once wired up)
/// can share the mapping with non-watcher diagnostic paths.
///
/// Per DD-002 §6.4, most transitional states (`disconnected`,
/// `scanning`, `associating`, `associated`) produce no event; only
/// meaningful transitions are emitted. `disconnected` specifically
/// needs context to distinguish intentional detach from failure;
/// callers compare against recent `detach` invocations.
pub fn translate_wpa_state(state_str: &str) -> Option<AuthState> {
    match state_str {
        "authenticating" => Some(AuthState::Authenticating),
        "completed" => Some(AuthState::Authenticated),
        "disconnected" => Some(AuthState::Idle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_wpa_state_covers_dd002_table() {
        assert!(matches!(
            translate_wpa_state("authenticating"),
            Some(AuthState::Authenticating),
        ));
        assert!(matches!(
            translate_wpa_state("completed"),
            Some(AuthState::Authenticated),
        ));
        assert!(matches!(
            translate_wpa_state("disconnected"),
            Some(AuthState::Idle),
        ));
        assert!(translate_wpa_state("associating").is_none());
        assert!(translate_wpa_state("scanning").is_none());
        assert!(translate_wpa_state("unknown_future").is_none());
    }
}
