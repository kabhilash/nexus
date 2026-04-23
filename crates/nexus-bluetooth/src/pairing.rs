//! Pairing helpers. See DD-004 §§7.3, 8.
//!
//! The pairing state machine itself lives on [`crate::BluetoothBackend`]
//! (see `start_pairing` and `on_pairing_complete`); this module only
//! houses the pure helpers: error classification, prompt-notification
//! builders, and the [`PairingAnswer`] type that flows from the
//! operator back into the backend.

use nexus_core::{
    BtFailureReason, NotificationData, PairingAnswer, PairingJobId, PairingPromptData,
    PairingPromptKind,
};

use crate::errors::BtError;

/// Per-kind validator for [`PairingAnswer`]. DD-006 §6.4 maps each
/// `PairingPromptKind` to exactly one valid answer variant (plus
/// [`PairingAnswer::Cancel`], which is always accepted). This helper
/// returns `Err(reason)` for any mismatch; the backend turns that
/// into a `fi.nexus.Error.InvalidArgument` without disturbing the
/// pending prompt — the operator may retry with a correct answer.
///
/// Also enforces the per-variant payload bounds: PIN = 4-16
/// printable-ASCII characters, passkey = 0..=999_999.
pub fn validate_answer(
    kind: PairingPromptKind,
    answer: &PairingAnswer,
) -> std::result::Result<(), String> {
    // Cancel is universal.
    if matches!(answer, PairingAnswer::Cancel) {
        return Ok(());
    }
    let label = prompt_kind_label(kind);
    match kind {
        PairingPromptKind::RequestPin => match answer {
            PairingAnswer::Pin(pin) => validate_pin(pin),
            _ => Err(format!(
                "{label} expects s (PIN 4-16 ASCII), got {}",
                answer_variant_name(answer)
            )),
        },
        PairingPromptKind::RequestPasskey => match answer {
            PairingAnswer::Passkey(n) if *n <= 999_999 => Ok(()),
            PairingAnswer::Passkey(n) => Err(format!("{label} expects u 0..=999999, got {n}")),
            _ => Err(format!(
                "{label} expects u 0..=999999, got {}",
                answer_variant_name(answer)
            )),
        },
        PairingPromptKind::RequestConfirmation
        | PairingPromptKind::RequestAuthorization
        | PairingPromptKind::AuthorizeService => match answer {
            PairingAnswer::Accept(_) => Ok(()),
            _ => Err(format!(
                "{label} expects b, got {}",
                answer_variant_name(answer)
            )),
        },
        PairingPromptKind::DisplayPasskey | PairingPromptKind::DisplayPin => match answer {
            PairingAnswer::Acknowledge => Ok(()),
            _ => Err(format!(
                "{label} expects s:\"acknowledge\" (or s:\"cancel\"), got {}",
                answer_variant_name(answer)
            )),
        },
    }
}

fn validate_pin(pin: &str) -> std::result::Result<(), String> {
    let len = pin.len();
    if !(4..=16).contains(&len) {
        return Err(format!("PIN length {len} outside 4..=16"));
    }
    if !pin.chars().all(|c| c.is_ascii() && !c.is_control()) {
        return Err("PIN must be printable ASCII".to_owned());
    }
    Ok(())
}

fn answer_variant_name(a: &PairingAnswer) -> &'static str {
    match a {
        PairingAnswer::Pin(_) => "Pin(s)",
        PairingAnswer::Passkey(_) => "Passkey(u)",
        PairingAnswer::Accept(_) => "Accept(b)",
        PairingAnswer::Acknowledge => "Acknowledge",
        PairingAnswer::Cancel => "Cancel",
    }
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
    fn validate_answer_request_pin_happy_path() {
        assert!(
            validate_answer(
                PairingPromptKind::RequestPin,
                &PairingAnswer::Pin("1234".into())
            )
            .is_ok()
        );
        assert!(
            validate_answer(
                PairingPromptKind::RequestPin,
                &PairingAnswer::Pin("0123456789ABCDEF".into())
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_answer_request_pin_rejects_short() {
        let err = validate_answer(
            PairingPromptKind::RequestPin,
            &PairingAnswer::Pin("12".into()),
        )
        .unwrap_err();
        assert!(err.contains("4..=16"), "got {err}");
    }

    #[test]
    fn validate_answer_request_pin_rejects_long() {
        let err = validate_answer(
            PairingPromptKind::RequestPin,
            &PairingAnswer::Pin("x".repeat(17)),
        )
        .unwrap_err();
        assert!(err.contains("4..=16"), "got {err}");
    }

    #[test]
    fn validate_answer_request_pin_rejects_non_ascii() {
        let err = validate_answer(
            PairingPromptKind::RequestPin,
            &PairingAnswer::Pin("1234\u{00E9}".into()),
        )
        .unwrap_err();
        assert!(err.contains("printable ASCII"), "got {err}");
    }

    #[test]
    fn validate_answer_request_pin_rejects_wrong_variant() {
        let err = validate_answer(PairingPromptKind::RequestPin, &PairingAnswer::Accept(true))
            .unwrap_err();
        assert!(
            err.contains("request_pin") && err.contains("Accept"),
            "got {err}"
        );
    }

    #[test]
    fn validate_answer_request_passkey_happy_path() {
        assert!(
            validate_answer(
                PairingPromptKind::RequestPasskey,
                &PairingAnswer::Passkey(0)
            )
            .is_ok()
        );
        assert!(
            validate_answer(
                PairingPromptKind::RequestPasskey,
                &PairingAnswer::Passkey(999_999)
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_answer_request_passkey_rejects_out_of_range() {
        let err = validate_answer(
            PairingPromptKind::RequestPasskey,
            &PairingAnswer::Passkey(1_000_000),
        )
        .unwrap_err();
        assert!(err.contains("0..=999999"), "got {err}");
    }

    #[test]
    fn validate_answer_confirmation_variants() {
        for kind in [
            PairingPromptKind::RequestConfirmation,
            PairingPromptKind::RequestAuthorization,
            PairingPromptKind::AuthorizeService,
        ] {
            assert!(validate_answer(kind, &PairingAnswer::Accept(true)).is_ok());
            assert!(validate_answer(kind, &PairingAnswer::Accept(false)).is_ok());
            let err = validate_answer(kind, &PairingAnswer::Pin("yes".into())).unwrap_err();
            assert!(err.contains("expects b"), "kind {kind:?}, got {err}");
        }
    }

    #[test]
    fn validate_answer_display_variants() {
        for kind in [
            PairingPromptKind::DisplayPasskey,
            PairingPromptKind::DisplayPin,
        ] {
            assert!(validate_answer(kind, &PairingAnswer::Acknowledge).is_ok());
            // Pin(_) would be produced by the D-Bus decoder for any
            // non-"acknowledge"/"cancel" string. Must be rejected here.
            let err = validate_answer(kind, &PairingAnswer::Pin("ok".into())).unwrap_err();
            assert!(err.contains("acknowledge"), "kind {kind:?}, got {err}");
            let err = validate_answer(kind, &PairingAnswer::Accept(true)).unwrap_err();
            assert!(err.contains("acknowledge"), "kind {kind:?}, got {err}");
        }
    }

    #[test]
    fn validate_answer_cancel_is_universal() {
        for kind in [
            PairingPromptKind::RequestPin,
            PairingPromptKind::RequestPasskey,
            PairingPromptKind::RequestConfirmation,
            PairingPromptKind::RequestAuthorization,
            PairingPromptKind::AuthorizeService,
            PairingPromptKind::DisplayPasskey,
            PairingPromptKind::DisplayPin,
        ] {
            assert!(
                validate_answer(kind, &PairingAnswer::Cancel).is_ok(),
                "kind {kind:?}"
            );
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
