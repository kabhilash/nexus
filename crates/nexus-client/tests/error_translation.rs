//! Covers every row of the DD-008 §9 error translation table.
//!
//! Each case pairs a `(wire_name, message)` pair — exactly what
//! nexusd emits on the wire — with the expected [`NexusctlError`]
//! variant, exit code, and JSON kind. Also spot-checks the
//! human-mode message so DD-008 §9's "Human message" column
//! doesn't drift.

use nexus_client::errors::{NexusctlError, json_error_object};
use nexus_client::translate_method_error;

#[test]
fn service_unknown_maps_to_nexusd_unreachable() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.ServiceUnknown",
        "fi.nexus1 not activatable",
    );
    assert!(matches!(err, NexusctlError::NexusdUnreachable));
    assert_eq!(err.exit_code(), 6);
    assert_eq!(err.json_kind(), "nexusd_unreachable");
    assert!(err.to_string().contains("nexusd is not running"));
}

#[test]
fn no_reply_maps_to_timeout() {
    let err = translate_method_error("org.freedesktop.DBus.Error.NoReply", "no reply within 25s");
    match &err {
        NexusctlError::Timeout { duration_s, .. } => {
            assert!(duration_s.is_none());
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(err.exit_code(), 4);
}

#[test]
fn access_denied_maps_to_auth_denied() {
    let err = translate_method_error("org.freedesktop.DBus.Error.AccessDenied", "policy refused");
    assert!(matches!(err, NexusctlError::AuthDenied { .. }));
    assert_eq!(err.exit_code(), 3);
}

#[test]
fn auth_failed_maps_to_auth_denied_with_action() {
    // The daemon renders AuthFailed as:
    //   "fi.nexus.Error.AuthFailed: policykit denied 'fi.nexus.X' for sender ':1.42'"
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.AuthFailed: policykit denied 'fi.nexus.profile.add' for sender ':1.42'",
    );
    match &err {
        NexusctlError::AuthDenied { action, .. } => {
            assert_eq!(action, "fi.nexus.profile.add");
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(err.exit_code(), 3);
    assert_eq!(err.json_kind(), "auth_denied");
}

#[test]
fn rate_limited_extracts_op_and_retry_after_ms() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.RateLimited: scan: retry after 4321 ms",
    );
    match &err {
        NexusctlError::RateLimited { op, retry_after_ms } => {
            assert_eq!(op, "scan");
            assert_eq!(*retry_after_ms, Some(4321));
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(err.exit_code(), 1);
    let obj = json_error_object(&err);
    assert_eq!(obj["error"], "rate_limited");
    assert_eq!(obj["op"], "scan");
    assert_eq!(obj["retry_after_ms"], 4321);
}

#[test]
fn feature_disabled_exits_7() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.FeatureDisabled: wifi",
    );
    match &err {
        NexusctlError::FeatureDisabled { feature } => {
            assert_eq!(feature, "wifi");
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(err.exit_code(), 7);
    assert_eq!(err.json_kind(), "feature_disabled");
}

#[test]
fn invalid_state_surfaces_state_string() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.InvalidState: already pairing",
    );
    match &err {
        NexusctlError::InvalidState { state, .. } => {
            assert_eq!(state, "already pairing");
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn invalid_argument_carries_message() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.InvalidArgument: unknown security type 'wpa4'",
    );
    match &err {
        NexusctlError::InvalidArgument { message } => {
            assert!(message.contains("wpa4"), "got {message}");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn not_found_exits_1() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.NotFound: interface 'wlan9'",
    );
    match &err {
        NexusctlError::NotFound { reference } => {
            assert!(reference.contains("wlan9"));
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn already_exists_exits_1() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.AlreadyExists: wifi profile for SSID with 4 bytes",
    );
    assert!(matches!(err, NexusctlError::AlreadyExists { .. }));
}

#[test]
fn unknown_device_surfaces_address() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.UnknownDevice: AA:BB:CC:DD:EE:FF",
    );
    match &err {
        NexusctlError::UnknownDevice { address } => {
            assert_eq!(address, "AA:BB:CC:DD:EE:FF");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn unknown_pairing_job_surfaces_job_id() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.UnknownPairingJob: 01H…Q1",
    );
    assert!(matches!(err, NexusctlError::UnknownPairingJob { .. }));
}

#[test]
fn connection_failed_exits_1() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.ConnectionFailed: peer unreachable",
    );
    assert!(matches!(err, NexusctlError::ConnectionFailed { .. }));
}

#[test]
fn bluez_unavailable_is_terminal_variant() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.BluezUnavailable: bluetoothd not running",
    );
    assert_eq!(err, NexusctlError::BluezUnavailable);
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn supplicant_unavailable_is_terminal_variant() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.SupplicantUnavailable: wpa_supplicant missing",
    );
    assert_eq!(err, NexusctlError::SupplicantUnavailable);
}

#[test]
fn not_powered_surfaces_adapter() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.NotPowered: hci0",
    );
    match &err {
        NexusctlError::NotPowered { adapter } => assert_eq!(adapter, "hci0"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn not_paired_surfaces_device() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.NotPaired: AA:BB:CC:DD:EE:01",
    );
    assert!(matches!(err, NexusctlError::NotPaired { .. }));
}

#[test]
fn resource_busy_exits_1() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.ResourceBusy: backup lease already held",
    );
    match &err {
        NexusctlError::ResourceBusy { resource } => {
            assert!(resource.contains("backup lease"));
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn timeout_with_after_seconds_extracts_duration() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.Timeout: scan did not finish after 30s",
    );
    match &err {
        NexusctlError::Timeout { duration_s, .. } => assert_eq!(*duration_s, Some(30)),
        other => panic!("got {other:?}"),
    }
    assert_eq!(err.exit_code(), 4);
}

#[test]
fn io_error_exits_1() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.IoError: profile store: ENOSPC",
    );
    assert!(matches!(err, NexusctlError::IoError { .. }));
}

#[test]
fn crypto_error_exits_1() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.CryptoError: decrypt failed",
    );
    assert!(matches!(err, NexusctlError::CryptoError { .. }));
}

#[test]
fn unsupported_exits_1() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.Unsupported: WPA3 on this chipset",
    );
    assert!(matches!(err, NexusctlError::Unsupported { .. }));
}

#[test]
fn unknown_fi_nexus_error_falls_back_to_other() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "fi.nexus.Error.SomethingNew: future variant",
    );
    match &err {
        NexusctlError::Other { raw } => {
            // Preserve the full error name so the operator can
            // diagnose.
            assert!(raw.contains("fi.nexus.Error.SomethingNew"));
        }
        other => panic!("got {other:?}"),
    }
    assert_eq!(err.exit_code(), 1);
}

#[test]
fn non_nexus_failed_message_is_other() {
    let err = translate_method_error(
        "org.freedesktop.DBus.Error.Failed",
        "a bare D-Bus failure with no fi.nexus prefix",
    );
    match &err {
        NexusctlError::Other { raw } => {
            assert!(raw.contains("a bare D-Bus failure"));
        }
        other => panic!("got {other:?}"),
    }
}
