//! Bluetooth Backend. See `dd-004-bluetooth-backend.md`.
//!
//! Public entry point is [`spawn_bluetooth_backend`], which
//! wires up the [`BluezClient`], the event bus, the command channel,
//! and (optionally) the Nexus [`agent::Agent`] at
//! `/fi/nexus/bluez_agent`. Pairing, profile persistence, power
//! management, and BlueZ reconnect supervision all flow through
//! the returned [`BtBackendHandle`].

use std::sync::Arc;

use nexus_core::NexusEvent;
use nexus_profile_store::ProfileStore;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod adapter;
pub mod agent;
pub mod backend;
pub mod bluez;
pub mod device;
pub mod errors;
pub mod metrics;
pub mod pairing;
pub mod types;

pub use agent::{AGENT_CAPABILITY, AGENT_PATH, Agent, spawn_agent};
pub use backend::{BluetoothBackend, BluetoothConfig, BtCommand};
pub use bluez::{BluezClient, MockBluezClient, ZbusBluezClient};
pub use errors::{BtError, Result};
pub use nexus_core::PairingAnswer;
pub use pairing::{classify_pair_error, validate_answer};
pub use types::{
    AuthorizationDecision, BtAdapterEntry, BtAdapterState, BtDeviceEntry, BtDeviceState,
    DiscoveryFilter, DiscoveryTransport, PowerState,
};

/// Handle returned by [`spawn_bluetooth_backend`]. Drop the handle
/// (or cancel `shutdown`) to stop the backend.
pub struct BtBackendHandle {
    pub join: JoinHandle<Result<()>>,
    pub shutdown: CancellationToken,
    pub cmd_tx: mpsc::Sender<BtCommand>,
}

/// Spawn the Bluetooth Backend event loop. See DD-004 §§3, 7. The
/// Agent registration is deferred to the caller — production
/// code calls [`spawn_agent`] against a live zbus `Connection` once
/// the backend is up.
pub fn spawn_bluetooth_backend(
    bluez: Arc<dyn BluezClient>,
    profile_store: Arc<dyn ProfileStore>,
    event_tx: broadcast::Sender<NexusEvent>,
    config: BluetoothConfig,
) -> BtBackendHandle {
    metrics::register();
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let backend = BluetoothBackend::new(
        bluez,
        profile_store,
        event_tx,
        cmd_tx.clone(),
        cmd_rx,
        config,
    );
    let shutdown = CancellationToken::new();
    let shutdown_child = shutdown.clone();
    let join = tokio::spawn(async move { backend.run(shutdown_child).await });
    BtBackendHandle {
        join,
        shutdown,
        cmd_tx,
    }
}
