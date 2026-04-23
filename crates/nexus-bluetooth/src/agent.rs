//! BlueZ pairing Agent. See DD-004 §8.1.
//!
//! The Agent is a D-Bus object at `/fi/nexus/bluez_agent` that
//! BlueZ calls during pairing. Every method implementation follows
//! the same shape:
//!
//! 1. Ask the backend for the current [`PairingJobId`] via
//!    `BtCommand::LookupPairingJob` (or, for
//!    `RequestAuthorization` / `AuthorizeService`, fast-path via
//!    `BtCommand::LookupAuthorizationPolicy`).
//! 2. Deposit a [`oneshot::Sender<PairingAnswer>`] in the backend's
//!    `pending_prompt_answers` map via
//!    `BtCommand::RegisterPromptOneshot`. The backend emits both
//!    `BtPairingPrompt` and the matching `OperatorNotification`.
//! 3. Await the answer with a timeout bounded by
//!    `agent_response_timeout_s`.
//! 4. Translate the answer into BlueZ's expected return type (or an
//!    error).
//!
//! The `Agent` struct below exposes the logic as plain async methods.
//! The real `#[zbus::interface]` impl is a thin wrapper
//! ([`spawn_agent`]) that forwards BlueZ calls into these methods;
//! keeping the logic separate makes it mock-testable without a
//! live zbus connection.

use std::time::Duration;

use nexus_core::{PairingAnswer, PairingJobId, PairingPromptData, PairingPromptKind};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

use crate::backend::BtCommand;
use crate::errors::{BtError, Result};
use crate::metrics as m;
use crate::pairing::prompt_kind_label;
use crate::types::AuthorizationDecision;

/// Agent object. Holds a clone of the backend's command sender so
/// BlueZ callbacks can reach the backend from the zbus object-server
/// task. `response_timeout` bounds each Agent method — operator
/// silence triggers a timeout error the backend classifies as
/// `PairingTimeout`.
#[derive(Clone)]
pub struct Agent {
    cmd_tx: mpsc::Sender<BtCommand>,
    response_timeout: Duration,
}

impl Agent {
    pub fn new(cmd_tx: mpsc::Sender<BtCommand>, response_timeout_s: u32) -> Self {
        Self {
            cmd_tx,
            response_timeout: Duration::from_secs(response_timeout_s as u64),
        }
    }

    /// Look up the in-flight [`PairingJobId`] for the given device,
    /// or `None` if pairing isn't currently in progress.
    async fn lookup_job(&self, device_path: &str) -> Result<Option<PairingJobId>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(BtCommand::LookupPairingJob {
                device_path: device_path.to_owned(),
                responder: tx,
            })
            .await
            .map_err(|_| BtError::Bluez("backend gone".into()))?;
        rx.await
            .map_err(|_| BtError::Bluez("lookup dropped".into()))
    }

    /// Register a prompt oneshot with the backend and await the
    /// operator's response.
    async fn register_and_await(
        &self,
        job_id: PairingJobId,
        kind: PairingPromptKind,
        data: PairingPromptData,
    ) -> Result<PairingAnswer> {
        let (tx_answer, rx_answer) = oneshot::channel();
        self.cmd_tx
            .send(BtCommand::RegisterPromptOneshot {
                job_id,
                sender: tx_answer,
                kind,
                data,
            })
            .await
            .map_err(|_| BtError::Bluez("backend gone".into()))?;
        match tokio::time::timeout(self.response_timeout, rx_answer).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err(BtError::Bluez("oneshot dropped".into())),
            Err(_) => Err(BtError::Bluez("operator response timed out".into())),
        }
    }

    /// Ask the backend whether an incoming authorization can be
    /// fast-pathed past operator prompt based on the device's
    /// stored profile.
    async fn lookup_authorization(
        &self,
        device_path: &str,
        service_uuid: Option<&str>,
    ) -> Result<AuthorizationDecision> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(BtCommand::LookupAuthorizationPolicy {
                device_path: device_path.to_owned(),
                service_uuid: service_uuid.map(|s| s.to_owned()),
                responder: tx,
            })
            .await
            .map_err(|_| BtError::Bluez("backend gone".into()))?;
        rx.await
            .map_err(|_| BtError::Bluez("policy lookup dropped".into()))
    }

    // -----------------------------------------------------------------
    // BlueZ Agent1 method handlers. The real zbus #[interface] glue
    // in `spawn_agent` forwards the arguments into these methods.
    // Testable in isolation.
    // -----------------------------------------------------------------

    /// `RequestPinCode(device) -> string`. See BlueZ agent-api.txt.
    pub async fn request_pin_code(&self, device_path: &str) -> Result<String> {
        m::record_agent_callback(prompt_kind_label(PairingPromptKind::RequestPin));
        let job_id = self.require_job(device_path).await?;
        let data = PairingPromptData {
            device_path: device_path.to_owned(),
            passkey: None,
            pincode: None,
            service_uuid: None,
        };
        match self
            .register_and_await(job_id, PairingPromptKind::RequestPin, data)
            .await?
        {
            PairingAnswer::Pin(pin) => Ok(pin),
            PairingAnswer::Cancel => Err(BtError::Bluez("cancelled".into())),
            _ => Err(BtError::Bluez("wrong answer variant".into())),
        }
    }

    /// `RequestPasskey(device) -> uint32`.
    pub async fn request_passkey(&self, device_path: &str) -> Result<u32> {
        m::record_agent_callback(prompt_kind_label(PairingPromptKind::RequestPasskey));
        let job_id = self.require_job(device_path).await?;
        let data = PairingPromptData {
            device_path: device_path.to_owned(),
            passkey: None,
            pincode: None,
            service_uuid: None,
        };
        match self
            .register_and_await(job_id, PairingPromptKind::RequestPasskey, data)
            .await?
        {
            PairingAnswer::Passkey(n) => Ok(n),
            PairingAnswer::Cancel => Err(BtError::Bluez("cancelled".into())),
            _ => Err(BtError::Bluez("wrong answer variant".into())),
        }
    }

    /// `DisplayPasskey(device, passkey, entered)`. Notification
    /// only — returns as soon as the operator acknowledges.
    pub async fn display_passkey(
        &self,
        device_path: &str,
        passkey: u32,
        _entered: u16,
    ) -> Result<()> {
        m::record_agent_callback(prompt_kind_label(PairingPromptKind::DisplayPasskey));
        let job_id = self.require_job(device_path).await?;
        let data = PairingPromptData {
            device_path: device_path.to_owned(),
            passkey: Some(passkey),
            pincode: None,
            service_uuid: None,
        };
        // Notification-only: any answer (including Acknowledge / Cancel)
        // allows the method to return.
        let _ = self
            .register_and_await(job_id, PairingPromptKind::DisplayPasskey, data)
            .await?;
        Ok(())
    }

    /// `DisplayPinCode(device, pincode)`. Notification only.
    pub async fn display_pin_code(&self, device_path: &str, pincode: &str) -> Result<()> {
        m::record_agent_callback(prompt_kind_label(PairingPromptKind::DisplayPin));
        let job_id = self.require_job(device_path).await?;
        let data = PairingPromptData {
            device_path: device_path.to_owned(),
            passkey: None,
            pincode: Some(pincode.to_owned()),
            service_uuid: None,
        };
        let _ = self
            .register_and_await(job_id, PairingPromptKind::DisplayPin, data)
            .await?;
        Ok(())
    }

    /// `RequestConfirmation(device, passkey)`. SSP numeric
    /// comparison. Returns Ok when the operator accepts.
    pub async fn request_confirmation(&self, device_path: &str, passkey: u32) -> Result<()> {
        m::record_agent_callback(prompt_kind_label(PairingPromptKind::RequestConfirmation));
        let job_id = self.require_job(device_path).await?;
        let data = PairingPromptData {
            device_path: device_path.to_owned(),
            passkey: Some(passkey),
            pincode: None,
            service_uuid: None,
        };
        match self
            .register_and_await(job_id, PairingPromptKind::RequestConfirmation, data)
            .await?
        {
            PairingAnswer::Accept(true) => Ok(()),
            _ => Err(BtError::Bluez("rejected".into())),
        }
    }

    /// `RequestAuthorization(device)`. The device is already paired
    /// and wants to reconnect — consult the stored profile first.
    /// A synthesized `PairingJobId` is used for the prompt so
    /// `AnswerPairingPrompt` flows through the same code path.
    pub async fn request_authorization(&self, device_path: &str) -> Result<()> {
        m::record_agent_callback(prompt_kind_label(PairingPromptKind::RequestAuthorization));
        match self.lookup_authorization(device_path, None).await? {
            AuthorizationDecision::Accept => return Ok(()),
            AuthorizationDecision::Reject => {
                return Err(BtError::Bluez("rejected by policy".into()));
            }
            AuthorizationDecision::Prompt => {}
        }
        let job_id = crate::pairing::new_job_id();
        let data = PairingPromptData {
            device_path: device_path.to_owned(),
            passkey: None,
            pincode: None,
            service_uuid: None,
        };
        match self
            .register_and_await(job_id, PairingPromptKind::RequestAuthorization, data)
            .await?
        {
            PairingAnswer::Accept(true) => Ok(()),
            _ => Err(BtError::Bluez("rejected".into())),
        }
    }

    /// `AuthorizeService(device, uuid)`. Same as
    /// [`request_authorization`](Self::request_authorization) but
    /// scoped to a specific service UUID.
    pub async fn authorize_service(&self, device_path: &str, uuid: &str) -> Result<()> {
        m::record_agent_callback(prompt_kind_label(PairingPromptKind::AuthorizeService));
        match self.lookup_authorization(device_path, Some(uuid)).await? {
            AuthorizationDecision::Accept => return Ok(()),
            AuthorizationDecision::Reject => {
                return Err(BtError::Bluez("rejected by policy".into()));
            }
            AuthorizationDecision::Prompt => {}
        }
        let job_id = crate::pairing::new_job_id();
        let data = PairingPromptData {
            device_path: device_path.to_owned(),
            passkey: None,
            pincode: None,
            service_uuid: Some(uuid.to_owned()),
        };
        match self
            .register_and_await(job_id, PairingPromptKind::AuthorizeService, data)
            .await?
        {
            PairingAnswer::Accept(true) => Ok(()),
            _ => Err(BtError::Bluez("rejected".into())),
        }
    }

    /// `Cancel()`. BlueZ is giving up on the pairing. No-op at the
    /// Agent level — the pair-driver task will observe a `Pair()`
    /// error and emit `BtPairingComplete`.
    pub async fn cancel(&self) {
        // nothing to do; log for traceability
    }

    async fn require_job(&self, device_path: &str) -> Result<PairingJobId> {
        match self.lookup_job(device_path).await? {
            Some(job) => Ok(job),
            None => {
                warn!(%device_path, "agent callback with no in-flight pairing");
                Err(BtError::Bluez("no in-flight pairing for device".into()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// zbus wiring — feature-gated so tests don't need a live bus.
// ---------------------------------------------------------------------------

/// Object path under which Nexus registers its Agent on BlueZ.
pub const AGENT_PATH: &str = "/fi/nexus/bluez_agent";

/// Capability string passed to `RegisterAgent`. `"KeyboardDisplay"`
/// covers every SSP flow; see DD-004 §8.1.
pub const AGENT_CAPABILITY: &str = "KeyboardDisplay";

/// Register the Nexus Agent with BlueZ. The caller owns the zbus
/// `Connection`; this helper serves the [`Agent`] object at
/// `/fi/nexus/bluez_agent` and calls `RegisterAgent` +
/// `RequestDefaultAgent` on `org.bluez.AgentManager1`. Returns
/// `Ok(())` on success, including the "another agent already
/// registered" benign case (DD-004 §8.1). Errors propagate otherwise.
pub async fn spawn_agent(conn: &zbus::Connection, agent: Agent) -> Result<()> {
    use crate::bluez::proxies::AgentManager1Proxy;

    let iface = ZbusAgent { inner: agent };
    conn.object_server()
        .at(AGENT_PATH, iface)
        .await
        .map_err(|e| BtError::Bluez(format!("register agent object: {e}")))?;

    let mgr = AgentManager1Proxy::new(conn).await?;
    let path = zbus::zvariant::ObjectPath::try_from(AGENT_PATH)
        .map_err(|e| BtError::Bluez(format!("agent path invalid: {e}")))?;
    match mgr.register_agent(&path, AGENT_CAPABILITY).await {
        Ok(()) => {
            if let Err(e) = mgr.request_default_agent(&path).await {
                warn!(error = %e, "RequestDefaultAgent failed (non-fatal)");
            }
            tracing::info!(path = AGENT_PATH, "nexus registered as bluez agent");
            Ok(())
        }
        Err(zbus::Error::MethodError(name, _detail, _))
            if name.as_str() == "org.bluez.Error.AlreadyExists" =>
        {
            warn!("another bluez agent is already registered — nexus pairings will use it");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// zbus-facing wrapper around [`Agent`]. Every method is a thin
/// translation of D-Bus arguments into [`Agent`]'s async methods.
struct ZbusAgent {
    inner: Agent,
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl ZbusAgent {
    async fn release(&self) -> zbus::fdo::Result<()> {
        // BlueZ calls this on shutdown. No state to tear down.
        Ok(())
    }

    async fn request_pin_code(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::fdo::Result<String> {
        self.inner
            .request_pin_code(&device.to_string())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn display_pin_code(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
        pincode: String,
    ) -> zbus::fdo::Result<()> {
        self.inner
            .display_pin_code(&device.to_string(), &pincode)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn request_passkey(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::fdo::Result<u32> {
        self.inner
            .request_passkey(&device.to_string())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn display_passkey(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
        passkey: u32,
        entered: u16,
    ) -> zbus::fdo::Result<()> {
        self.inner
            .display_passkey(&device.to_string(), passkey, entered)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn request_confirmation(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
        passkey: u32,
    ) -> zbus::fdo::Result<()> {
        self.inner
            .request_confirmation(&device.to_string(), passkey)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn request_authorization(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::fdo::Result<()> {
        self.inner
            .request_authorization(&device.to_string())
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn authorize_service(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
        uuid: String,
    ) -> zbus::fdo::Result<()> {
        self.inner
            .authorize_service(&device.to_string(), &uuid)
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }

    async fn cancel(&self) -> zbus::fdo::Result<()> {
        self.inner.cancel().await;
        Ok(())
    }
}
