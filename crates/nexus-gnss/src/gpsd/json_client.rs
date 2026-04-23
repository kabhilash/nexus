//! TCP/JSON gpsd client. See DD-005 §§6.1-6.3.
//!
//! The reader task owns the TCP read half exclusively; the control
//! path (`add_device`, `remove_device`) grabs the write half from an
//! `Arc<Mutex<Option<OwnedWriteHalf>>>` held on the client struct.
//! Interior mutability keeps every trait method `&self` so the
//! backend can store the client as `Arc<dyn GpsdClient>`.

use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use nexus_core::NexusEvent;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, broadcast};
use tokio::task::JoinHandle;

use super::GpsdClient;
use super::messages::{GpsdMessage, VersionMessage};
use super::parse::{parse_sky, parse_tpv};
use crate::errors::{GnssError, Result};
use crate::fix::GnssFix;

/// Production gpsd client. Cheap to clone (all state is `Arc`'d).
pub struct JsonGpsdClient {
    endpoint: SocketAddr,
    event_tx: broadcast::Sender<NexusEvent>,
    writer: Arc<Mutex<Option<OwnedWriteHalf>>>,
    /// Handle to the reader task — `Some` + not-finished means live.
    reader_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl JsonGpsdClient {
    /// Construct a client bound to `endpoint`. Connection is lazy —
    /// call [`connect()`][Self::connect] before operating.
    pub fn new(endpoint: SocketAddr, event_tx: broadcast::Sender<NexusEvent>) -> Self {
        Self {
            endpoint,
            event_tx,
            writer: Arc::new(Mutex::new(None)),
            reader_task: Arc::new(Mutex::new(None)),
        }
    }

    /// Default `127.0.0.1:2947` convenience constructor.
    pub fn localhost(event_tx: broadcast::Sender<NexusEvent>) -> Self {
        Self::new(([127, 0, 0, 1], 2947).into(), event_tx)
    }

    async fn send_line(&self, cmd: &str) -> Result<()> {
        let mut guard = self.writer.lock().await;
        let w = guard.as_mut().ok_or(GnssError::NotConnected)?;
        w.write_all(cmd.as_bytes()).await?;
        if !cmd.ends_with('\n') {
            w.write_all(b"\n").await?;
        }
        Ok(())
    }
}

#[async_trait]
impl GpsdClient for JsonGpsdClient {
    async fn connect(&self) -> Result<()> {
        if self.is_connected() {
            return Ok(());
        }

        // Drop any stale reader / writer before reconnecting.
        {
            let mut guard = self.reader_task.lock().await;
            if let Some(h) = guard.take() {
                h.abort();
            }
        }
        {
            let mut guard = self.writer.lock().await;
            *guard = None;
        }

        let stream = TcpStream::connect(self.endpoint).await?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        // gpsd emits VERSION on connect — parse it and validate the
        // protocol is 3.x or newer.
        let mut version_line = String::new();
        reader.read_line(&mut version_line).await?;
        let version: VersionMessage = serde_json::from_str(version_line.trim())?;
        if version.proto_major < 3 {
            return Err(GnssError::GpsdProtocolTooOld {
                got: version.proto_major,
            });
        }
        if version.proto_major > 3 {
            tracing::warn!(
                proto_major = version.proto_major,
                proto_minor = version.proto_minor,
                release = %version.release,
                "gpsd protocol newer than tested; accepting"
            );
        } else {
            tracing::info!(
                release = %version.release,
                proto_major = version.proto_major,
                proto_minor = version.proto_minor,
                "gpsd connected"
            );
        }

        writer
            .write_all(br#"?WATCH={"enable":true,"json":true}"#)
            .await?;
        writer.write_all(b"\n").await?;

        let event_tx = self.event_tx.clone();
        let reader_task = tokio::spawn(reader_loop(reader, event_tx.clone()));

        *self.writer.lock().await = Some(writer);
        *self.reader_task.lock().await = Some(reader_task);

        let _ = self.event_tx.send(NexusEvent::GnssGpsdConnected);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        // `try_lock` keeps this non-blocking at 1 Hz; a false
        // negative only delays the next reconcile tick.
        match self.reader_task.try_lock() {
            Ok(guard) => guard.as_ref().map(|h| !h.is_finished()).unwrap_or(false),
            Err(_) => false,
        }
    }

    async fn add_device(&self, path: &str) -> Result<()> {
        // Serialize through serde_json to escape tricky characters.
        let body = serde_json::json!({ "path": path, "activate": true });
        let cmd = format!("?DEVICE={body}\n");
        self.send_line(&cmd).await
    }

    async fn remove_device(&self, path: &str) -> Result<()> {
        let body = serde_json::json!({ "path": path, "activate": false });
        let cmd = format!("?DEVICE={body}\n");
        self.send_line(&cmd).await
    }

    async fn current_fix(&self, _path: &str) -> Result<Option<GnssFix>> {
        // v0.1 relies purely on the push-driven flow. A future
        // enhancement would issue `?POLL;\n` and correlate the
        // reply — not needed for the steady-state loop.
        Ok(None)
    }

    fn name(&self) -> &'static str {
        "gpsd-json"
    }
}

/// Drain lines from gpsd into `NexusEvent`s until the connection
/// closes. Emits [`NexusEvent::GnssGpsdDisconnected`] on exit.
async fn reader_loop(
    mut reader: BufReader<OwnedReadHalf>,
    event_tx: broadcast::Sender<NexusEvent>,
) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break, // EOF — gpsd closed the socket
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(error = ?e, "gpsd read error");
                break;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        dispatch(trimmed, &event_tx);
    }
    let _ = event_tx.send(NexusEvent::GnssGpsdDisconnected);
}

/// Parse a single gpsd line and emit the matching `NexusEvent`.
/// Separated from the reader loop so the mock can exercise the
/// same translation.
pub(crate) fn dispatch(line: &str, event_tx: &broadcast::Sender<NexusEvent>) {
    match serde_json::from_str::<GpsdMessage>(line) {
        Ok(GpsdMessage::Tpv(msg)) => {
            if let Some((device, fix)) = parse_tpv(&msg) {
                let _ = event_tx.send(NexusEvent::GnssTpvReceived { device, fix });
            }
        }
        Ok(GpsdMessage::Sky(msg)) => {
            if let Some((device, satellites)) = parse_sky(&msg) {
                let _ = event_tx.send(NexusEvent::GnssSatellites { device, satellites });
            }
        }
        Ok(GpsdMessage::Error(err)) => {
            tracing::warn!(message = %err.message, "gpsd ERROR");
        }
        Ok(GpsdMessage::Device(msg)) => {
            if msg.activated.is_none() {
                if let Some(path) = &msg.path {
                    tracing::debug!(device = %path, "gpsd deactivated device");
                }
            }
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(line, error = ?e, "malformed gpsd JSON; skipping");
        }
    }
}
