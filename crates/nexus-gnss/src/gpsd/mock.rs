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
    canned_errors: Vec<(String, GnssError)>,
    current_fix: std::collections::HashMap<String, GnssFix>,
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

    /// Queue an error for the next call of the given name
    /// (`"connect"`, `"add_device"`, `"remove_device"`,
    /// `"current_fix"`).
    pub fn inject_error(&self, method: &str, err: GnssError) {
        self.state
            .lock()
            .unwrap()
            .canned_errors
            .push((method.to_owned(), err));
    }

    /// Preset the cached fix `current_fix()` returns for a device.
    pub fn set_current_fix(&self, device_path: &str, fix: GnssFix) {
        self.state
            .lock()
            .unwrap()
            .current_fix
            .insert(device_path.to_owned(), fix);
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

    fn consume_err(&self, method: &str) -> Option<GnssError> {
        let mut s = self.state.lock().unwrap();
        let idx = s.canned_errors.iter().position(|(k, _)| k == method)?;
        Some(s.canned_errors.remove(idx).1)
    }
}

#[async_trait]
impl GpsdClient for MockGpsdClient {
    async fn connect(&self) -> Result<()> {
        self.state.lock().unwrap().calls.push(MockCall::Connect);
        if let Some(e) = self.consume_err("connect") {
            return Err(e);
        }
        self.state.lock().unwrap().connected = true;
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
        if let Some(e) = self.consume_err("add_device") {
            return Err(e);
        }
        Ok(())
    }

    async fn remove_device(&self, path: &str) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(MockCall::RemoveDevice(path.to_owned()));
        if let Some(e) = self.consume_err("remove_device") {
            return Err(e);
        }
        Ok(())
    }

    async fn current_fix(&self, path: &str) -> Result<Option<GnssFix>> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(MockCall::CurrentFix(path.to_owned()));
        if let Some(e) = self.consume_err("current_fix") {
            return Err(e);
        }
        Ok(self.state.lock().unwrap().current_fix.get(path).cloned())
    }

    fn name(&self) -> &'static str {
        "gpsd-mock"
    }
}
