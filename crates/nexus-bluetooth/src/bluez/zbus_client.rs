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
use nexus_core::{BluetoothAddrExt, MacAddr, NexusEvent};
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

    async fn refresh_adapter(&self, adapter: &str) -> Result<(bool, bool, MacAddr)> {
        let proxy = self.adapter_proxy(adapter).await?;
        let powered = proxy.powered().await?;
        let discovering = proxy.discovering().await?;
        let address_str = proxy.address().await?;
        let address = MacAddr::from_bluez(&address_str).map_err(|e| {
            BtError::Bluez(format!(
                "adapter {adapter} reported unparseable Address {address_str:?}: {e}"
            ))
        })?;
        Ok((powered, discovering, address))
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
        // Always call SetDiscoveryFilter, even for an all-default
        // `filter` — DD-006 §6.4 documents that omitting a field
        // means BlueZ's own default (`Transport: "auto"` in
        // particular), so Nexus has to *assert* that default on
        // every call rather than skip the D-Bus call and hope
        // BlueZ's ambient state (leftover from an earlier session —
        // ours or another client's) happens to match. Skipping this
        // call when the dict came out empty was the root cause of
        // `bt scan` silently inheriting a stale LE-only filter and
        // finding nothing on hardware that had one set from a prior
        // session.
        proxy
            .set_discovery_filter(build_discovery_filter_dict(&filter))
            .await?;
        proxy.start_discovery().await?;
        Ok(())
    }

    async fn stop_discovery(&self, adapter: &str) -> Result<()> {
        self.adapter_proxy(adapter).await?.stop_discovery().await?;
        Ok(())
    }

    async fn pair(&self, device_path: &str) -> Result<()> {
        self.device_proxy(device_path).await?.pair().await?;
        Ok(())
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

/// Build BlueZ's `SetDiscoveryFilter` dict from a [`DiscoveryFilter`].
/// Pure — split out so tests can assert the exact dict shape without
/// a live D-Bus connection, mirroring `nexus-wifi`'s
/// `build_wpa_network_args` pattern.
///
/// Always includes `Transport`, defaulting to `"auto"` when the
/// caller's filter leaves it unset. DD-006 §6.4: omitting a filter
/// field means BlueZ's own default, not "whatever BlueZ already has
/// configured" — the two only coincide if nothing else has touched
/// the adapter's filter since it powered on, which doesn't hold once
/// any other session (ours from an earlier run, or another BlueZ
/// client) has set something different. `start_discovery` calls this
/// unconditionally so every scan deterministically resets the
/// filter rather than skipping `SetDiscoveryFilter` when the dict
/// would otherwise be empty.
fn build_discovery_filter_dict(filter: &DiscoveryFilter) -> HashMap<String, Value<'static>> {
    let mut dict: HashMap<String, Value<'static>> = HashMap::new();
    let transport = match filter.transport {
        Some(DiscoveryTransport::Auto) | None => "auto",
        Some(DiscoveryTransport::Bredr) => "bredr",
        Some(DiscoveryTransport::Le) => "le",
    };
    dict.insert("Transport".into(), Value::new(transport.to_owned()));
    if let Some(rssi) = filter.rssi {
        dict.insert("RSSI".into(), Value::new(rssi));
    }
    if !filter.uuids.is_empty() {
        dict.insert("UUIDs".into(), Value::new(filter.uuids.clone()));
    }
    if filter.duplicate_data {
        dict.insert("DuplicateData".into(), Value::new(true));
    }
    dict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport_of(dict: &HashMap<String, Value<'static>>) -> String {
        String::try_from(dict.get("Transport").expect("Transport key present").clone())
            .expect("Transport is a string")
    }

    #[test]
    fn default_filter_asserts_explicit_auto_transport() {
        // The exact bug: an all-default DiscoveryFilter must still
        // produce a non-empty dict with Transport="auto" — not an
        // empty dict that leaves BlueZ's ambient filter untouched.
        let dict = build_discovery_filter_dict(&DiscoveryFilter::default());
        assert!(!dict.is_empty(), "dict must never be empty");
        assert_eq!(transport_of(&dict), "auto");
        assert!(!dict.contains_key("RSSI"));
        assert!(!dict.contains_key("UUIDs"));
        assert!(!dict.contains_key("DuplicateData"));
    }

    #[test]
    fn explicit_auto_transport_matches_default() {
        let filter = DiscoveryFilter {
            transport: Some(DiscoveryTransport::Auto),
            ..DiscoveryFilter::default()
        };
        assert_eq!(transport_of(&build_discovery_filter_dict(&filter)), "auto");
    }

    #[test]
    fn explicit_transport_is_passed_through() {
        let bredr = DiscoveryFilter {
            transport: Some(DiscoveryTransport::Bredr),
            ..DiscoveryFilter::default()
        };
        assert_eq!(transport_of(&build_discovery_filter_dict(&bredr)), "bredr");

        let le = DiscoveryFilter {
            transport: Some(DiscoveryTransport::Le),
            ..DiscoveryFilter::default()
        };
        assert_eq!(transport_of(&build_discovery_filter_dict(&le)), "le");
    }

    #[test]
    fn optional_fields_are_included_only_when_set() {
        let filter = DiscoveryFilter {
            transport: None,
            rssi: Some(-70),
            uuids: vec!["0000180f-0000-1000-8000-00805f9b34fb".to_owned()],
            duplicate_data: true,
        };
        let dict = build_discovery_filter_dict(&filter);
        assert_eq!(transport_of(&dict), "auto");
        assert_eq!(i16::try_from(dict.get("RSSI").unwrap().clone()).unwrap(), -70);
        assert!(dict.contains_key("UUIDs"));
        assert!(bool::try_from(dict.get("DuplicateData").unwrap().clone()).unwrap());
    }

    #[test]
    fn duplicate_data_false_is_omitted() {
        // `false` is DuplicateData's own default; BlueZ doesn't need
        // to be told to keep doing what it already does.
        let dict = build_discovery_filter_dict(&DiscoveryFilter::default());
        assert!(!dict.contains_key("DuplicateData"));
    }
}
