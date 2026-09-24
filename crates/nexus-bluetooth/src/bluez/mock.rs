//! In-memory [`BluezClient`] for unit / integration tests. Every
//! call records its arguments on a shared [`MockBluezState`] and
//! mutating calls trigger the matching [`NexusEvent`] on the bus
//! the backend subscribes to.
//!
//! Test flow (mirrors DD-004 §14.1):
//!
//! ```ignore
//! let (event_tx, _) = broadcast::channel(64);
//! let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
//! mock.publish_adapter("/org/bluez/hci0").await;
//! mock.publish_device("/org/bluez/hci0", addr, false).await;
//! ```

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nexus_core::{BtDeviceInfo, MacAddr, NexusEvent};
use tokio::sync::broadcast;

use super::BluezClient;
use super::object_manager::{adapter_props, device_props, on_interfaces_added};
use crate::errors::{BtError, Result};
use crate::types::DiscoveryFilter;

/// Asynchronous hook a test installs via
/// [`MockBluezClient::on_pair`]. The hook is invoked when the
/// backend calls [`BluezClient::pair`] on a device path; its
/// returned future drives the simulated pair exchange and the mock
/// awaits the outcome before returning from `pair()`.
pub type PairHook =
    Arc<dyn Fn(String) -> Pin<Box<dyn Future<Output = Result<()>> + Send>> + Send + Sync>;

/// Recorded history — tests assert on this to verify the backend
/// issued the expected BlueZ calls.
#[derive(Debug, Default)]
pub struct MockBluezState {
    pub connected: bool,
    pub powered: HashMap<String, bool>,
    pub discoverable: HashMap<String, bool>,
    pub pairable: HashMap<String, bool>,
    pub discovering: HashMap<String, bool>,
    pub trusted: HashMap<String, bool>,
    /// BlueZ's own view of each adapter's `Address` — independent of
    /// `powered`/`discovering` since real BlueZ never changes
    /// `Adapter1.Address` via `PropertiesChanged`. Read by
    /// [`BluezClient::refresh_adapter`]; set directly via
    /// [`MockBluezClient::set_adapter_address`] rather than through
    /// a synthesized signal.
    pub address: HashMap<String, MacAddr>,
    /// Calls the backend has made. Tests pattern-match on this.
    pub calls: Vec<MockCall>,
    /// Canned error for the next call of the given name, popped on
    /// match. Used to simulate BlueZ errors.
    pub next_errors: Vec<(String, BtError)>,
}

/// Snapshot of the mock's observable state (sans the error queue,
/// which carries non-Clone [`BtError`] values). Tests assert on
/// this snapshot rather than the live state.
#[derive(Debug, Default, Clone)]
pub struct MockBluezSnapshot {
    pub connected: bool,
    pub powered: HashMap<String, bool>,
    pub discoverable: HashMap<String, bool>,
    pub pairable: HashMap<String, bool>,
    pub discovering: HashMap<String, bool>,
    pub trusted: HashMap<String, bool>,
    pub address: HashMap<String, MacAddr>,
    pub calls: Vec<MockCall>,
}

#[derive(Debug, Clone)]
pub enum MockCall {
    Connect,
    SetPowered(String, bool),
    SetDiscoverable(String, bool),
    SetPairable(String, bool),
    StartDiscovery(String, DiscoveryFilter),
    StopDiscovery(String),
    Pair(String),
    CancelPairing(String),
    SetTrusted(String, bool),
    ConnectDevice(String),
    DisconnectDevice(String),
    ForgetDevice(String, String),
    RefreshAdapter(String),
}

/// Full mock client. Cheap to clone (Arc'd state + broadcast
/// sender).
#[derive(Clone)]
pub struct MockBluezClient {
    event_tx: broadcast::Sender<NexusEvent>,
    state: Arc<Mutex<MockBluezState>>,
    pair_hook: Arc<Mutex<Option<PairHook>>>,
}

impl MockBluezClient {
    pub fn new(event_tx: broadcast::Sender<NexusEvent>) -> Self {
        Self {
            event_tx,
            state: Arc::new(Mutex::new(MockBluezState::default())),
            pair_hook: Arc::new(Mutex::new(None)),
        }
    }

    /// Install a hook run on every `pair()` call. Use this to
    /// script the agent-callback flow in tests; returning `Ok(())`
    /// simulates BlueZ's Pair() succeeding, returning an error
    /// simulates a pairing failure with the supplied BlueZ message.
    pub fn on_pair<F, Fut>(&self, hook: F)
    where
        F: Fn(String) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let boxed: PairHook = Arc::new(move |path| Box::pin(hook(path)));
        *self.pair_hook.lock().unwrap() = Some(boxed);
    }

    /// Snapshot the current state. Returns a cloneable view that
    /// omits the error queue (which carries non-Clone
    /// [`BtError`]).
    pub fn state(&self) -> MockBluezSnapshot {
        let guard = self.state.lock().unwrap();
        MockBluezSnapshot {
            connected: guard.connected,
            powered: guard.powered.clone(),
            discoverable: guard.discoverable.clone(),
            pairable: guard.pairable.clone(),
            discovering: guard.discovering.clone(),
            trusted: guard.trusted.clone(),
            address: guard.address.clone(),
            calls: guard.calls.clone(),
        }
    }

    fn record(&self, call: MockCall) {
        self.state.lock().unwrap().calls.push(call);
    }

    fn consume_canned_error(&self, name: &str) -> Option<BtError> {
        let mut guard = self.state.lock().unwrap();
        let idx = guard.next_errors.iter().position(|(k, _)| k == name)?;
        Some(guard.next_errors.remove(idx).1)
    }

    /// Simulate BlueZ publishing an adapter object. Synthesizes
    /// `BtAdapterChanged` on the event bus, driving the backend
    /// from `Unavailable → Present` or further.
    pub async fn publish_adapter(&self, adapter_path: &str, powered: bool, discovering: bool) {
        let ifaces = adapter_props(powered, discovering);
        on_interfaces_added(adapter_path, &ifaces, &self.event_tx);
        let mut guard = self.state.lock().unwrap();
        guard.powered.insert(adapter_path.to_owned(), powered);
        guard
            .discovering
            .insert(adapter_path.to_owned(), discovering);
    }

    /// Simulate BlueZ removing an adapter. The backend's device-
    /// cleanup on `InterfaceRemoved` takes care of state teardown
    /// (DD-001 is authoritative).
    pub async fn publish_adapter_removed(&self, adapter_path: &str) {
        let mut guard = self.state.lock().unwrap();
        guard.powered.remove(adapter_path);
        guard.discovering.remove(adapter_path);
    }

    /// Simulate BlueZ publishing a device under the given adapter.
    pub async fn publish_device(&self, adapter_path: &str, address: MacAddr, paired: bool) {
        self.publish_device_full(adapter_path, address, paired, false, &[])
            .await;
    }

    pub async fn publish_device_full(
        &self,
        adapter_path: &str,
        address: MacAddr,
        paired: bool,
        connected: bool,
        uuids: &[&str],
    ) {
        let path = format!(
            "{adapter_path}/{}",
            <MacAddr as nexus_core::BluetoothAddrExt>::to_object_path_component(&address)
        );
        let props = device_props(address, "public", None, paired, connected, uuids);
        on_interfaces_added(&path, &props, &self.event_tx);
    }

    /// Simulate a device removal (`InterfacesRemoved`).
    pub async fn publish_device_removed(&self, device_path: &str) {
        super::object_manager::on_interfaces_removed(
            device_path,
            &[super::proxies::iface::DEVICE1.to_owned()],
            &self.event_tx,
        );
    }

    /// Simulate BlueZ flipping a property on the adapter.
    pub async fn publish_adapter_props(
        &self,
        adapter_path: &str,
        powered: bool,
        discovering: bool,
    ) {
        use zbus::zvariant::{OwnedValue, Value};
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "Powered".to_owned(),
            OwnedValue::try_from(Value::new(powered)).unwrap(),
        );
        changed.insert(
            "Discovering".to_owned(),
            OwnedValue::try_from(Value::new(discovering)).unwrap(),
        );
        super::object_manager::on_properties_changed(
            adapter_path,
            super::proxies::iface::ADAPTER1,
            &changed,
            &self.event_tx,
        );
        let mut guard = self.state.lock().unwrap();
        guard.powered.insert(adapter_path.to_owned(), powered);
        guard
            .discovering
            .insert(adapter_path.to_owned(), discovering);
    }

    /// Simulate a device `Connected` flip.
    pub async fn publish_device_connected(&self, device_path: &str, connected: bool) {
        use zbus::zvariant::{OwnedValue, Value};
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "Connected".to_owned(),
            OwnedValue::try_from(Value::new(connected)).unwrap(),
        );
        super::object_manager::on_properties_changed(
            device_path,
            super::proxies::iface::DEVICE1,
            &changed,
            &self.event_tx,
        );
    }

    /// Simulate BlueZ flipping a device's `Paired` property.
    pub async fn publish_device_paired(&self, device_path: &str, paired: bool) {
        use zbus::zvariant::{OwnedValue, Value};
        let mut changed: HashMap<String, OwnedValue> = HashMap::new();
        changed.insert(
            "Paired".to_owned(),
            OwnedValue::try_from(Value::new(paired)).unwrap(),
        );
        super::object_manager::on_properties_changed(
            device_path,
            super::proxies::iface::DEVICE1,
            &changed,
            &self.event_tx,
        );
    }

    /// Set BlueZ's adapter properties without emitting
    /// `PropertiesChanged` or `InterfacesAdded` — simulates the
    /// signal (or the initial `ObjectManager` snapshot) getting
    /// lost, so tests can exercise the reconcile-driven self-heal
    /// path ([`BluezClient::refresh_adapter`]) independently of the
    /// signal-driven one.
    pub fn set_adapter_props_silently(&self, adapter_path: &str, powered: bool, discovering: bool) {
        let mut guard = self.state.lock().unwrap();
        guard.powered.insert(adapter_path.to_owned(), powered);
        guard
            .discovering
            .insert(adapter_path.to_owned(), discovering);
    }

    /// Set BlueZ's own view of an adapter's `Address`, as
    /// [`BluezClient::refresh_adapter`] would read it. Separate from
    /// [`Self::set_adapter_props_silently`] since real BlueZ never
    /// changes `Adapter1.Address` via `PropertiesChanged` — there's
    /// no signal-driven equivalent to bypass, only the initial value
    /// a test wants `refresh_adapter` to observe.
    pub fn set_adapter_address(&self, adapter_path: &str, address: MacAddr) {
        self.state
            .lock()
            .unwrap()
            .address
            .insert(adapter_path.to_owned(), address);
    }

    /// Queue a canned error for the next call to the named method.
    /// Name matches the `MockCall` variant name in lowercase snake
    /// case, e.g. `"connect_device"`.
    pub fn inject_error(&self, method: &str, err: BtError) {
        self.state
            .lock()
            .unwrap()
            .next_errors
            .push((method.to_owned(), err));
    }

    /// Pre-seed a [`BtDeviceInfo`] on the bus without going through
    /// the interfaces-added path — useful for tests that want a
    /// device visible to the backend without populating a matching
    /// mock adapter.
    pub fn emit_device_discovered(&self, info: BtDeviceInfo) {
        let _ = self.event_tx.send(NexusEvent::BtDeviceDiscovered(info));
    }
}

#[async_trait]
impl BluezClient for MockBluezClient {
    async fn connect(&self) -> Result<()> {
        self.record(MockCall::Connect);
        if let Some(err) = self.consume_canned_error("connect") {
            return Err(err);
        }
        self.state.lock().unwrap().connected = true;
        let _ = self.event_tx.send(NexusEvent::BluezConnected);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.state.lock().unwrap().connected
    }

    async fn refresh_adapter(&self, adapter: &str) -> Result<(bool, bool, MacAddr)> {
        self.record(MockCall::RefreshAdapter(adapter.to_owned()));
        let guard = self.state.lock().unwrap();
        let powered = guard.powered.get(adapter).copied().unwrap_or(false);
        let discovering = guard.discovering.get(adapter).copied().unwrap_or(false);
        let address = guard
            .address
            .get(adapter)
            .copied()
            .unwrap_or(MacAddr([0; 6]));
        Ok((powered, discovering, address))
    }

    async fn set_powered(&self, adapter: &str, on: bool) -> Result<()> {
        self.record(MockCall::SetPowered(adapter.to_owned(), on));
        if let Some(err) = self.consume_canned_error("set_powered") {
            return Err(err);
        }
        // Simulate BlueZ firing PropertiesChanged.
        self.publish_adapter_props(adapter, on, false).await;
        Ok(())
    }

    async fn set_discoverable(&self, adapter: &str, on: bool) -> Result<()> {
        self.record(MockCall::SetDiscoverable(adapter.to_owned(), on));
        self.state
            .lock()
            .unwrap()
            .discoverable
            .insert(adapter.to_owned(), on);
        Ok(())
    }

    async fn set_pairable(&self, adapter: &str, on: bool) -> Result<()> {
        self.record(MockCall::SetPairable(adapter.to_owned(), on));
        self.state
            .lock()
            .unwrap()
            .pairable
            .insert(adapter.to_owned(), on);
        Ok(())
    }

    async fn start_discovery(&self, adapter: &str, filter: DiscoveryFilter) -> Result<()> {
        self.record(MockCall::StartDiscovery(adapter.to_owned(), filter));
        if let Some(err) = self.consume_canned_error("start_discovery") {
            return Err(err);
        }
        // Mirror BlueZ: Discovering goes true, Powered stays whatever
        // it was.
        let powered = self
            .state
            .lock()
            .unwrap()
            .powered
            .get(adapter)
            .copied()
            .unwrap_or(true);
        self.publish_adapter_props(adapter, powered, true).await;
        Ok(())
    }

    async fn stop_discovery(&self, adapter: &str) -> Result<()> {
        self.record(MockCall::StopDiscovery(adapter.to_owned()));
        let powered = self
            .state
            .lock()
            .unwrap()
            .powered
            .get(adapter)
            .copied()
            .unwrap_or(true);
        self.publish_adapter_props(adapter, powered, false).await;
        Ok(())
    }

    async fn pair(&self, device_path: &str) -> Result<()> {
        self.record(MockCall::Pair(device_path.to_owned()));
        if let Some(err) = self.consume_canned_error("pair") {
            return Err(err);
        }
        // Grab the hook future outside the lock so we don't hold
        // the mutex across the await.
        let fut_opt = {
            let guard = self.pair_hook.lock().unwrap();
            guard.as_ref().map(|h| h(device_path.to_owned()))
        };
        match fut_opt {
            Some(fut) => fut.await,
            None => {
                // Default: simulate a successful pair exchange.
                // Flip the Device1.Paired flag to reflect BlueZ's
                // post-pair state.
                self.publish_device_paired(device_path, true).await;
                Ok(())
            }
        }
    }

    async fn cancel_pairing(&self, device_path: &str) -> Result<()> {
        self.record(MockCall::CancelPairing(device_path.to_owned()));
        Ok(())
    }

    async fn set_trusted(&self, device_path: &str, on: bool) -> Result<()> {
        self.record(MockCall::SetTrusted(device_path.to_owned(), on));
        self.state
            .lock()
            .unwrap()
            .trusted
            .insert(device_path.to_owned(), on);
        Ok(())
    }

    async fn connect_device(&self, device_path: &str) -> Result<()> {
        self.record(MockCall::ConnectDevice(device_path.to_owned()));
        if let Some(err) = self.consume_canned_error("connect_device") {
            return Err(err);
        }
        self.publish_device_connected(device_path, true).await;
        Ok(())
    }

    async fn disconnect_device(&self, device_path: &str) -> Result<()> {
        self.record(MockCall::DisconnectDevice(device_path.to_owned()));
        self.publish_device_connected(device_path, false).await;
        Ok(())
    }

    async fn forget_device(&self, adapter: &str, device_path: &str) -> Result<()> {
        self.record(MockCall::ForgetDevice(
            adapter.to_owned(),
            device_path.to_owned(),
        ));
        self.publish_device_removed(device_path).await;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "bluez-mock"
    }
}
