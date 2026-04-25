//! In-memory [`GpsdClient`] for unit / integration tests.
//!
//! The mock records every control call so tests can assert on the
//! sequence, and exposes `feed_line` / `feed_tpv` / `feed_sky`
//! helpers that shove raw gpsd JSON into the event bus through the
//! same translation path the real reader uses. Tests drive the
//! backend end-to-end by publishing events; the mock itself is
//! stateless with respect to the flow.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use nexus_core::NexusEvent;
use tokio::sync::broadcast;

use super::GpsdClient;
use super::json_client::dispatch;
use crate::errors::{GnssError, Result};
use crate::fix::GnssFix;

#[derive(Debug, Clone)]
pub enum MockCall {
    Connect,
    AddDevice(String),
    RemoveDevice(String),
    CurrentFix(String),
}

#[derive(Debug, Default)]
struct MockState {
    connected: bool,
    calls: Vec<MockCall>,
    /// Number of subsequent `connect()` calls that should fail with
    /// `GnssError::NotConnected`. Decremented on each failure;
    /// callers use `fail_next_connect(n)` to set up.
    pending_connect_failures: u32,
}

/// Shareable mock. Every method is `&self`; internal state is
/// behind a `Mutex`.
#[derive(Clone)]
pub struct MockGpsdClient {
    event_tx: broadcast::Sender<NexusEvent>,
    state: Arc<Mutex<MockState>>,
}

impl MockGpsdClient {
    pub fn new(event_tx: broadcast::Sender<NexusEvent>) -> Self {
        Self {
            event_tx,
            state: Arc::new(Mutex::new(MockState::default())),
        }
    }

    /// Record of every control-plane call, in order.
    pub fn calls(&self) -> Vec<MockCall> {
        self.state.lock().unwrap().calls.clone()
    }

    /// Make the next `n` `connect()` calls fail with
    /// `GnssError::NotConnected`. Used to drive the supervisor's
    /// reconnect-backoff path in fault-injection tests.
    pub fn fail_next_connect(&self, n: u32) {
        self.state.lock().unwrap().pending_connect_failures = n;
    }

    /// Pretend the connection dropped. The next `is_connected()`
    /// returns false and `connect()` can reconnect.
    pub fn simulate_disconnect(&self) {
        self.state.lock().unwrap().connected = false;
        let _ = self.event_tx.send(NexusEvent::GnssGpsdDisconnected);
    }

    /// Feed one raw gpsd JSON line through the normal translation
    /// layer. Used by tests to drive TPV / SKY / DEVICE messages.
    pub fn feed_line(&self, line: &str) {
        dispatch(line, &self.event_tx);
    }

    /// Emit a TPV event directly — skips the JSON round-trip.
    pub fn feed_tpv(&self, device: &str, fix: GnssFix) {
        let _ = self.event_tx.send(NexusEvent::GnssTpvReceived {
            device: device.to_owned(),
            fix,
        });
    }

    /// Emit a SKY event directly.
    pub fn feed_sky(&self, device: &str, satellites: Vec<crate::fix::SatInfo>) {
        let _ = self.event_tx.send(NexusEvent::GnssSatellites {
            device: device.to_owned(),
            satellites,
        });
    }
}

#[async_trait]
impl GpsdClient for MockGpsdClient {
    async fn connect(&self) -> Result<()> {
        let mut s = self.state.lock().unwrap();
        s.calls.push(MockCall::Connect);
        if s.pending_connect_failures > 0 {
            s.pending_connect_failures -= 1;
            return Err(GnssError::NotConnected);
        }
        s.connected = true;
        drop(s);
        let _ = self.event_tx.send(NexusEvent::GnssGpsdConnected);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.state.lock().unwrap().connected
    }

    async fn add_device(&self, path: &str) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(MockCall::AddDevice(path.to_owned()));
        Ok(())
    }

    async fn remove_device(&self, path: &str) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(MockCall::RemoveDevice(path.to_owned()));
        Ok(())
    }

    async fn current_fix(&self, path: &str) -> Result<Option<GnssFix>> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(MockCall::CurrentFix(path.to_owned()));
        Ok(None)
    }

    fn name(&self) -> &'static str {
        "gpsd-mock"
    }
}
