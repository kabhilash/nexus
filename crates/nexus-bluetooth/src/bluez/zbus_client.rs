//! Production [`BluezClient`] — talks to `org.bluez` on the system
//! bus via zbus. See DD-004 §§6.1, 7.1.
//!
//! The live signal subscription (ObjectManager + PropertiesChanged)
//! is wired up when [`ZbusBluezClient::connect`] is called; the
//! resulting `NexusEvent` stream reaches the backend through the
//! bus. See [`crate::bluez::object_manager`] for the translation
//! layer.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use nexus_core::NexusEvent;
use tokio::sync::{RwLock, broadcast};
use tokio_util::sync::CancellationToken;
use zbus::zvariant::{ObjectPath, Value};

use super::BluezClient;
use super::object_manager::{PumpHandle, spawn_object_manager_pump};
use super::proxies::{Adapter1Proxy, Device1Proxy, ObjectManagerProxy};
use crate::errors::{BtError, Result};
use crate::types::{DiscoveryFilter, DiscoveryTransport};

/// Production BlueZ client. Held as `Arc<dyn BluezClient>` by the
/// backend so spawned tasks can clone it cheaply.
pub struct ZbusBluezClient {
    /// Broadcast for `NexusEvent`s the client synthesizes
    /// (`BluezConnected` / `BluezDisconnected` + whatever the
    /// ObjectManager pump produces).
    event_tx: broadcast::Sender<NexusEvent>,
    /// D-Bus session: the live connection, or `None` before
    /// `connect()` runs or after the connection has dropped. Behind
    /// an `RwLock` so `&self`-taking trait methods can still mutate
    /// it (DD-004 §6.1 concurrency note).
    inner: Arc<RwLock<Option<Session>>>,
}

struct Session {
    connection: zbus::Connection,
    /// Handle to the ObjectManager pump; dropped on disconnect to
    /// stop the subscription.
    #[allow(dead_code)]
    pump: PumpHandle,
    /// Cancellation token aborted on `disconnect` — the pump task
    /// checks it between signals.
    cancel: CancellationToken,
}

impl ZbusBluezClient {
    /// Construct a client that will connect to the system bus on
    /// first [`connect()`][Self::connect]. The sender is stored so
    /// later `connect` calls can emit `BluezConnected` / re-publish
    /// the tree.
    pub fn new(event_tx: broadcast::Sender<NexusEvent>) -> Self {
        Self {
            event_tx,
            inner: Arc::new(RwLock::new(None)),
        }
    }

    async fn session_lock_read(&self) -> Result<tokio::sync::RwLockReadGuard<'_, Option<Session>>> {
        let guard = self.inner.read().await;
        if guard.is_none() {
            return Err(BtError::NotConnected);
        }
        Ok(guard)
    }

    async fn adapter_proxy(&self, adapter: &str) -> Result<Adapter1Proxy<'static>> {
        let guard = self.session_lock_read().await?;
        let sess = guard.as_ref().unwrap();
        let path = ObjectPath::try_from(adapter.to_owned())
            .map_err(|e| BtError::Bluez(format!("invalid adapter path: {e}")))?;
        let proxy = Adapter1Proxy::builder(&sess.connection)
            .path(path)
            .map_err(|e| BtError::Bluez(format!("adapter proxy path: {e}")))?
            .build()
            .await?;
        Ok(proxy)
    }

    async fn device_proxy(&self, device_path: &str) -> Result<Device1Proxy<'static>> {
        let guard = self.session_lock_read().await?;
        let sess = guard.as_ref().unwrap();
        let path = ObjectPath::try_from(device_path.to_owned())
            .map_err(|e| BtError::Bluez(format!("invalid device path: {e}")))?;
        let proxy = Device1Proxy::builder(&sess.connection)
            .path(path)
            .map_err(|e| BtError::Bluez(format!("device proxy path: {e}")))?
            .build()
            .await?;
        Ok(proxy)
    }
}

#[async_trait]
impl BluezClient for ZbusBluezClient {
    async fn connect(&self) -> Result<()> {
        {
            let guard = self.inner.read().await;
            if guard.is_some() {
                return Ok(());
            }
        }
        let connection = zbus::Connection::system().await?;
        let om = ObjectManagerProxy::new(&connection).await?;
        let cancel = CancellationToken::new();
        let pump = spawn_object_manager_pump(
            connection.clone(),
            om,
            self.event_tx.clone(),
            cancel.clone(),
        )
        .await?;

        {
            let mut guard = self.inner.write().await;
            *guard = Some(Session {
                connection,
                pump,
                cancel,
            });
        }
        let _ = self.event_tx.send(NexusEvent::BluezConnected);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        // Try a non-blocking read to avoid a deadlock if the lock
        // is momentarily held for a mutation; a false negative here
        // is harmless — the reconcile tick will try again.
        self.inner.try_read().is_ok_and(|g| g.is_some())
    }

    async fn set_powered(&self, adapter: &str, on: bool) -> Result<()> {
        self.adapter_proxy(adapter).await?.set_powered(on).await?;
        Ok(())
    }

    async fn set_discoverable(&self, adapter: &str, on: bool) -> Result<()> {
        self.adapter_proxy(adapter)
            .await?
            .set_discoverable(on)
            .await?;
        Ok(())
    }

    async fn set_pairable(&self, adapter: &str, on: bool) -> Result<()> {
        self.adapter_proxy(adapter).await?.set_pairable(on).await?;
        Ok(())
    }

    async fn start_discovery(&self, adapter: &str, filter: DiscoveryFilter) -> Result<()> {
        let proxy = self.adapter_proxy(adapter).await?;
        // Build BlueZ's SetDiscoveryFilter dict.
        let mut dict: HashMap<String, Value<'_>> = HashMap::new();
        if let Some(transport) = filter.transport {
            let s = match transport {
                DiscoveryTransport::Auto => "auto",
                DiscoveryTransport::Bredr => "bredr",
                DiscoveryTransport::Le => "le",
            };
            dict.insert("Transport".into(), Value::new(s.to_owned()));
        }
        if let Some(rssi) = filter.rssi {
            dict.insert("RSSI".into(), Value::new(rssi));
        }
        if !filter.uuids.is_empty() {
            dict.insert("UUIDs".into(), Value::new(filter.uuids.clone()));
        }
        if filter.duplicate_data {
            dict.insert("DuplicateData".into(), Value::new(true));
        }
        if !dict.is_empty() {
            proxy.set_discovery_filter(dict).await?;
        }
        proxy.start_discovery().await?;
        Ok(())
    }

    async fn stop_discovery(&self, adapter: &str) -> Result<()> {
        self.adapter_proxy(adapter).await?.stop_discovery().await?;
        Ok(())
    }

    async fn pair(&self, _device_path: &str) -> Result<()> {
        // Deferred to phase 5. The stub returns a typed error so
        // higher-level code can surface a usable D-Bus error today.
        Err(BtError::PairingNotImplemented)
    }

    async fn cancel_pairing(&self, device_path: &str) -> Result<()> {
        self.device_proxy(device_path)
            .await?
            .cancel_pairing()
            .await?;
        Ok(())
    }

    async fn set_trusted(&self, device_path: &str, on: bool) -> Result<()> {
        self.device_proxy(device_path)
            .await?
            .set_trusted(on)
            .await?;
        Ok(())
    }

    async fn connect_device(&self, device_path: &str) -> Result<()> {
        self.device_proxy(device_path).await?.connect().await?;
        Ok(())
    }

    async fn disconnect_device(&self, device_path: &str) -> Result<()> {
        self.device_proxy(device_path).await?.disconnect().await?;
        Ok(())
    }

    async fn forget_device(&self, adapter: &str, device_path: &str) -> Result<()> {
        let proxy = self.adapter_proxy(adapter).await?;
        let path = ObjectPath::try_from(device_path.to_owned())
            .map_err(|e| BtError::Bluez(format!("invalid device path: {e}")))?;
        proxy.remove_device(&path).await?;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "bluez-zbus"
    }
}

impl Drop for ZbusBluezClient {
    fn drop(&mut self) {
        // Best-effort: if a session is still live, abort its pump.
        if let Ok(guard) = self.inner.try_read() {
            if let Some(sess) = guard.as_ref() {
                sess.cancel.cancel();
            }
        }
    }
}
