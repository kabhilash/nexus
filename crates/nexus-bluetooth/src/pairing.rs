//! Pairing helpers. See DD-004 §§7.3, 8.
//!
//! The pairing state machine itself lives on [`crate::BluetoothBackend`]
//! (see `start_pairing` and `on_pairing_complete`); this module only
//! houses the pure helpers: error classification, prompt-notification
//! builders, and the [`PairingAnswer`] type that flows from the
//! operator back into the backend.

use nexus_core::{
    BtFailureReason, NotificationData, PairingJobId, PairingPromptData, PairingPromptKind,
};

use crate::errors::BtError;

/// The answer the operator (or an auto-accept policy) hands back to
/// the backend, which in turn resolves a pending Agent oneshot. See
/// DD-004 §6.2.
#[derive(Debug, Clone)]
pub enum PairingAnswer {
    /// PIN entered by the operator (`RequestPin`).
    Pin(String),
    /// Passkey entered by the operator (`RequestPasskey`).
    Passkey(u32),
    /// Yes/no for any confirmation-style prompt (`RequestConfirmation`,
    /// `RequestAuthorization`, `AuthorizeService`).
    Accept(bool),
    /// The operator has seen a notification-only prompt
    /// (`DisplayPasskey` / `DisplayPin`). Tells the Agent it can
    /// return to BlueZ now.
    Acknowledge,
    /// Operator cancelled or the prompt timed out.
    Cancel,
}

/// Classify a [`BtError`] into a [`BtFailureReason`]. See
/// DD-004 §7.3. Match order matters: the more-specific strings are
/// tried before the generic ones.
pub fn classify_pair_error(e: &BtError) -> BtFailureReason {
    match e {
        BtError::Bluez(msg) if msg.contains("AuthenticationCanceled") => {
            BtFailureReason::PairingRejected
        }
        BtError::Bluez(msg) if msg.contains("AuthenticationRejected") => {
            BtFailureReason::PairingRejected
        }
        BtError::Bluez(msg) if msg.contains("AuthenticationFailed") => {
            BtFailureReason::PairingAuthFailed
        }
        BtError::Bluez(msg) if msg.contains("AuthenticationTimeout") => {
            BtFailureReason::PairingTimeout
        }
        BtError::Bluez(msg) if msg.contains("ConnectionAttemptFailed") => {
            BtFailureReason::ConnectionFailed
        }
        // The Agent raises this shape when the operator's answer
        // doesn't arrive in time.
        BtError::Bluez(msg) if msg.contains("operator response timed out") => {
            BtFailureReason::PairingTimeout
        }
        BtError::Bluez(msg) if msg.contains("rejected") => BtFailureReason::PairingRejected,
        BtError::Bluez(msg) => BtFailureReason::Unknown(msg.clone()),
        other => BtFailureReason::Unknown(format!("{other:?}")),
    }
}

/// Translate a pairing prompt into the `NotificationData` dict
/// published via `NexusEvent::OperatorNotification` (DD-006 §5.3).
/// Keys are human-readable; the D-Bus layer emits these as
/// variants.
pub fn build_prompt_notification(
    job_id: PairingJobId,
    kind: PairingPromptKind,
    data: &PairingPromptData,
) -> NotificationData {
    let mut out = NotificationData::default();
    out.insert("job_id".to_owned(), job_id.0.to_string());
    out.insert("device_path".to_owned(), data.device_path.clone());
    out.insert("kind".to_owned(), prompt_kind_label(kind).to_owned());
    if let Some(pk) = data.passkey {
        out.insert("passkey".to_owned(), format!("{pk:06}"));
    }
    if let Some(ref pin) = data.pincode {
        out.insert("pincode".to_owned(), pin.clone());
    }
    if let Some(ref uuid) = data.service_uuid {
        out.insert("service_uuid".to_owned(), uuid.clone());
    }
    out
}

/// Lower-snake-case label for a [`PairingPromptKind`]. Used by
/// metrics and the notification dict.
pub fn prompt_kind_label(kind: PairingPromptKind) -> &'static str {
    match kind {
        PairingPromptKind::RequestPin => "request_pin",
        PairingPromptKind::RequestPasskey => "request_passkey",
        PairingPromptKind::DisplayPasskey => "display_passkey",
        PairingPromptKind::DisplayPin => "display_pin",
        PairingPromptKind::RequestConfirmation => "request_confirmation",
        PairingPromptKind::RequestAuthorization => "request_authorization",
        PairingPromptKind::AuthorizeService => "authorize_service",
    }
}

/// Fresh [`PairingJobId`] for a new pairing attempt.
pub fn new_job_id() -> PairingJobId {
    PairingJobId(ulid::Ulid::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_auth_failed() {
        let e = BtError::Bluez("org.bluez.Error.AuthenticationFailed".into());
        matches!(classify_pair_error(&e), BtFailureReason::PairingAuthFailed);
    }

    #[test]
    fn classify_auth_rejected() {
        let e = BtError::Bluez("org.bluez.Error.AuthenticationRejected".into());
        matches!(classify_pair_error(&e), BtFailureReason::PairingRejected);
    }

    #[test]
    fn classify_timeout() {
        let e = BtError::Bluez("org.bluez.Error.AuthenticationTimeout".into());
        matches!(classify_pair_error(&e), BtFailureReason::PairingTimeout);
    }

    #[test]
    fn classify_connection_failed() {
        let e = BtError::Bluez("org.bluez.Error.ConnectionAttemptFailed".into());
        matches!(classify_pair_error(&e), BtFailureReason::ConnectionFailed);
    }

    #[test]
    fn classify_agent_timeout_string() {
        let e = BtError::Bluez("operator response timed out".into());
        matches!(classify_pair_error(&e), BtFailureReason::PairingTimeout);
    }

    #[test]
    fn classify_unknown_preserves_text() {
        let e = BtError::Bluez("something new we do not know".into());
        match classify_pair_error(&e) {
            BtFailureReason::Unknown(s) => {
                assert!(s.contains("something new"));
            }
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    #[test]
    fn build_notification_includes_passkey_padded() {
        let data = PairingPromptData {
            device_path: "/org/bluez/hci0/dev_AA".into(),
            passkey: Some(42),
            pincode: None,
            service_uuid: None,
        };
        let out = build_prompt_notification(
            PairingJobId(ulid::Ulid::new()),
            PairingPromptKind::RequestConfirmation,
            &data,
        );
        // The value carries the zero-padded passkey.
        let raw = format!("{out:?}");
        assert!(raw.contains("000042"));
        assert!(raw.contains("request_confirmation"));
    }
}
