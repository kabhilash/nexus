//! Top-level errors for the Bluetooth Backend. See DD-004 §7.3.

use nexus_core::PairingJobId;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, BtError>;

#[derive(Debug, Error)]
pub enum BtError {
    /// BlueZ returned a D-Bus error; the message is BlueZ's own
    /// string. Pairing and connection flows later classify this via
    /// substring match (DD-004 §7.3 `classify_pair_error`).
    #[error("BlueZ D-Bus error: {0}")]
    Bluez(String),

    /// BlueZ D-Bus is not connected. Operators get this when they
    /// call a backend method during the window between
    /// `BluezDisconnected` and the next `BluezConnected`.
    #[error("BlueZ D-Bus not connected")]
    NotConnected,

    /// Operation referenced a device path the backend hasn't seen
    /// (either never published by BlueZ, or removed before the call
    /// landed).
    #[error("unknown device: {0}")]
    UnknownDevice(String),

    /// Operation referenced an adapter the backend hasn't seen.
    #[error("unknown adapter: {0}")]
    UnknownAdapter(String),

    /// Pair() called on a device that's already in `Pairing` state.
    #[error("device already has a pairing in flight")]
    AlreadyPairing,

    /// `AnswerPairingPrompt` referenced a job the backend doesn't
    /// have a pending Agent oneshot for.
    #[error("unknown pairing job: {0:?}")]
    UnknownPairingJob(PairingJobId),

    /// `AnswerPairingPrompt` supplied a variant that doesn't match
    /// the pending prompt's kind (DD-006 §6.4 per-kind variant
    /// map) — e.g. boolean for `RequestPin`, or string "yes" for
    /// `RequestConfirmation`. The pending prompt remains armed so
    /// the operator may retry with a correct answer.
    #[error("invalid pairing-answer variant: {0}")]
    InvalidPromptAnswer(String),

    /// The Agent's oneshot receiver was dropped before the operator
    /// answered.
    #[error("pairing job gone before answer arrived")]
    PairingJobGone,

    /// `RegisterAgent` lost the race — another D-Bus client holds
    /// the BlueZ Agent role.
    #[error("agent capability conflict (another agent is registered)")]
    AgentConflict,

    /// Operation needs a powered adapter; the named adapter is off.
    #[error("operation on powered-off adapter: {0}")]
    AdapterNotPowered(String),

    /// Profile Store surfaced a problem.
    #[error("profile store error: {0}")]
    ProfileStore(String),
}

impl From<zbus::Error> for BtError {
    fn from(e: zbus::Error) -> Self {
        match e {
            zbus::Error::MethodError(name, detail, _) => {
                BtError::Bluez(format!("{name}: {}", detail.unwrap_or_default()))
            }
            other => BtError::Bluez(format!("{other}")),
        }
    }
}

impl From<nexus_profile_store::StoreError> for BtError {
    fn from(e: nexus_profile_store::StoreError) -> Self {
        BtError::ProfileStore(e.to_string())
    }
}
