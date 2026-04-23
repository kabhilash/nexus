//! BlueZ abstraction layer. See DD-004 §6.1.
//!
//! All BlueZ interaction goes through the [`BluezClient`] trait.
//! The production impl is [`zbus_client::ZbusBluezClient`], which
//! speaks `zbus` to the system bus. Tests use [`mock::MockBluezClient`],
//! which implements the same trait with an in-memory event queue so
//! the backend can be driven without a live BlueZ daemon.

pub mod mock;
pub mod object_manager;
pub mod proxies;
pub mod zbus_client;

use async_trait::async_trait;

use crate::errors::Result;
use crate::types::DiscoveryFilter;

pub use mock::MockBluezClient;
pub use zbus_client::ZbusBluezClient;

/// A BlueZ client backend. Implementations drive BlueZ over D-Bus
/// (or a mock queue) and translate its `ObjectManager` +
/// `PropertiesChanged` traffic into [`nexus_core::NexusEvent`]
/// variants.
///
/// DD-004 §6.1 — **every method takes `&self`**. Implementations
/// store their mutable connection state behind interior
/// synchronization (e.g. `Mutex`, `OnceCell`) so the backend can
/// keep the client as `Arc<dyn BluezClient>` and clone handles
/// into spawned tasks without a borrow-checker fight.
#[async_trait]
pub trait BluezClient: Send + Sync {
    /// Establish or re-establish the connection to BlueZ.
    /// Idempotent. On success, emits
    /// [`nexus_core::NexusEvent::BluezConnected`] and begins
    /// republishing the current `ObjectManager` tree (which arrives
    /// at the backend as a sequence of `BtAdapterChanged` /
    /// `BtDeviceDiscovered`).
    async fn connect(&self) -> Result<()>;

    /// True when the client currently has a live connection to
    /// BlueZ. Used by the reconcile supervisor (DD-004 §7.3) to
    /// decide whether to retry `connect()`.
    fn is_connected(&self) -> bool;

    /// Set the adapter's `Powered` property.
    async fn set_powered(&self, adapter: &str, on: bool) -> Result<()>;

    /// Set the adapter's `Discoverable` property.
    async fn set_discoverable(&self, adapter: &str, on: bool) -> Result<()>;

    /// Set the adapter's `Pairable` property.
    async fn set_pairable(&self, adapter: &str, on: bool) -> Result<()>;

    /// Begin a discovery session on the adapter. Idempotent — if
    /// already discovering, returns `Ok` without calling BlueZ
    /// again. The filter is applied via `SetDiscoveryFilter` before
    /// `StartDiscovery`.
    async fn start_discovery(&self, adapter: &str, filter: DiscoveryFilter) -> Result<()>;

    /// Stop the discovery session on the adapter. Idempotent.
    async fn stop_discovery(&self, adapter: &str) -> Result<()>;

    /// Initiate pairing. The registered Agent handles callbacks;
    /// returns when BlueZ's `Pair()` method returns.
    async fn pair(&self, device_path: &str) -> Result<()>;

    /// Cancel an in-flight pairing.
    async fn cancel_pairing(&self, device_path: &str) -> Result<()>;

    /// Mark a paired device trusted.
    async fn set_trusted(&self, device_path: &str, on: bool) -> Result<()>;

    /// Connect to a device.
    async fn connect_device(&self, device_path: &str) -> Result<()>;

    /// Disconnect but keep the bond.
    async fn disconnect_device(&self, device_path: &str) -> Result<()>;

    /// Remove the bond and drop from BlueZ's registry (maps to
    /// `Adapter1.RemoveDevice`).
    async fn forget_device(&self, adapter: &str, device_path: &str) -> Result<()>;

    /// Backend identifier for logs and metrics.
    fn name(&self) -> &'static str;
}
