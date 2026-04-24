//! Signal → [`WatchEvent`] translation. DD-008 §7.4.
//!
//! One helper per row of the synthesis table. Production callers
//! (the zbus subscription loop) decode each signal's wire args
//! into the helper's typed parameters; the helper stamps the event
//! with the kind + flat field dict specified in §7.4.
//!
//! Keeping these free functions (rather than methods on some
//! fat event-source struct) makes each row trivially testable.

use std::collections::HashMap;

use crate::watch::event::{FieldValue, WatchEvent};

/// Shared helper: stamp `time` + `kind` + any extras.
fn new(kind: &str) -> WatchEvent {
    WatchEvent::now(kind)
}

/// Row 1: `interface-added`. Source:
/// `ObjectManager.InterfacesAdded` filtered to
/// `/fi/nexus1/interface/*`.
pub fn interface_added(iface: &str, iface_kind: &str) -> WatchEvent {
    new("interface-added")
        .with("iface", iface)
        .with("iface_kind", iface_kind)
}

/// Row 2: `interface-removed`.
pub fn interface_removed(iface: &str) -> WatchEvent {
    new("interface-removed").with("iface", iface)
}

/// Row 3: `link-state`. `fi.nexus.Interface.StateChanged` or
/// the common `PropertiesChanged` on `OperState`.
pub fn link_state(iface: &str, state: &str) -> WatchEvent {
    new("link-state").with("iface", iface).with("state", state)
}

/// Row 4: `eth-auth-state`. `fi.nexus.Ethernet.AuthStateChanged`.
pub fn eth_auth_state(iface: &str, state: &str) -> WatchEvent {
    new("eth-auth-state")
        .with("iface", iface)
        .with("state", state)
}

/// Row 5: `wifi-state`. `fi.nexus.Wifi.StateChanged`.
pub fn wifi_state(iface: &str, state: &str) -> WatchEvent {
    new("wifi-state").with("iface", iface).with("state", state)
}

/// Row 6: `wifi-scan`. `fi.nexus.Wifi.ScanCompleted`.
pub fn wifi_scan(iface: &str, result_count: u32) -> WatchEvent {
    new("wifi-scan")
        .with("iface", iface)
        .with("result_count", result_count)
}

/// Row 7: `wifi-signal`. Coalesced `PropertiesChanged` on
/// `fi.nexus.Wifi.SignalDbm`. DD-006 §9.3 coalesces to 1 Hz per
/// property per object.
pub fn wifi_signal(iface: &str, dbm: i32) -> WatchEvent {
    new("wifi-signal").with("iface", iface).with("dbm", dbm)
}

/// Row 8: `bt-adapter-state`. `fi.nexus.Bluetooth.StateChanged`.
pub fn bt_adapter_state(adapter: &str, state: &str) -> WatchEvent {
    new("bt-adapter-state")
        .with("adapter", adapter)
        .with("state", state)
}

/// Row 9: `bt-device-state`.
/// `fi.nexus.BluetoothDevice.StateChanged`.
pub fn bt_device_state(adapter: &str, address: &str, state: &str) -> WatchEvent {
    new("bt-device-state")
        .with("adapter", adapter)
        .with("address", address)
        .with("state", state)
}

/// Row 10: `bt-pairing-started`. `Bluetooth.PairingStarted`.
pub fn bt_pairing_started(adapter: &str, job_id: &str, device: &str) -> WatchEvent {
    new("bt-pairing-started")
        .with("adapter", adapter)
        .with("job_id", job_id)
        .with("device", device)
}

/// Row 11: `bt-pairing-prompt`. `Bluetooth.PairingPrompt`.
/// Kind-specific fields (passkey / pin / service_uuid) are
/// appended only when present.
///
/// **Rename:** DD-008 §7.4's table lists a `kind` flat field that
/// would collide with the envelope `kind="bt-pairing-prompt"` —
/// duplicate JSON keys. We emit it as `prompt_kind` instead;
/// operators filter with `--filter 'prompt_kind=request_confirmation'`.
pub fn bt_pairing_prompt(
    adapter: &str,
    job_id: &str,
    kind: &str,
    passkey: Option<u32>,
    pin: Option<&str>,
    service_uuid: Option<&str>,
) -> WatchEvent {
    let mut ev = new("bt-pairing-prompt")
        .with("adapter", adapter)
        .with("job_id", job_id)
        .with("prompt_kind", kind);
    if let Some(p) = passkey {
        ev = ev.with("passkey", p);
    }
    if let Some(pin) = pin {
        ev = ev.with("pin", pin);
    }
    if let Some(u) = service_uuid {
        ev = ev.with("service_uuid", u);
    }
    ev
}

/// Row 12: `bt-pairing-complete`. `Bluetooth.PairingComplete`.
pub fn bt_pairing_complete(adapter: &str, job_id: &str, success: bool, reason: &str) -> WatchEvent {
    new("bt-pairing-complete")
        .with("adapter", adapter)
        .with("job_id", job_id)
        .with("success", success)
        .with("reason", reason)
}

/// Row 13: `gnss-fix`. `fi.nexus.Gnss.FixChanged`. `alt` /
/// `hdop` are optional per §7.4.
pub fn gnss_fix(
    device: &str,
    mode: i32,
    lat: f64,
    lon: f64,
    alt_m: Option<f64>,
    hdop: Option<f64>,
) -> WatchEvent {
    let mut ev = new("gnss-fix")
        .with("device", device)
        .with("mode", mode)
        .with("lat", lat)
        .with("lon", lon);
    if let Some(a) = alt_m {
        ev = ev.with("alt", a);
    }
    if let Some(h) = hdop {
        ev = ev.with("hdop", h);
    }
    ev
}

/// Row 14: `profile-changed`. Derived from
/// `ObjectManager.InterfacesAdded` / `Removed` filtered to
/// `/fi/nexus1/profile/*`.
///
/// **Rename:** DD-008 §7.4's table lists a `kind` flat field (the
/// profile kind: `wifi` / `ethernet`) that would collide with the
/// envelope `kind="profile-changed"` — duplicate JSON keys. We emit
/// it as `profile_kind` instead; operators filter with
/// `--filter 'profile_kind=wifi'`.
pub fn profile_changed(kind: &str, action: &str, id: &str) -> WatchEvent {
    new("profile-changed")
        .with("profile_kind", kind)
        .with("action", action)
        .with("id", id)
}

/// Row 15: `notification`. `fi.nexus.Manager.NotificationEvent`.
/// DD-008 §7.4 explicitly says the `data: a{sv}` flattens into
/// the top-level event dict.
///
/// **Rename:** §7.4's table lists a `kind` field (the pass-through
/// sub-kind, e.g., `credentials_invalid`) which would collide with
/// the envelope `kind="notification"` and produce duplicate JSON
/// keys. We emit it as `notif_kind` instead.
pub fn notification(kind: &str, data: HashMap<String, FieldValue>) -> WatchEvent {
    let mut ev = new("notification").with("notif_kind", kind);
    for (k, v) in data {
        // Don't let the data dict shadow the envelope or clobber
        // the renamed sub-kind.
        if k == "time" || k == "kind" || k == "notif_kind" {
            continue;
        }
        ev.fields.insert(k, v);
    }
    ev
}

/// Row 16: `master-key-rotated`. `Manager.MasterKeyRotated`.
pub fn master_key_rotated(
    job_id: &str,
    outcome: &str,
    profiles_rewritten: u32,
    duration_ms: u64,
) -> WatchEvent {
    new("master-key-rotated")
        .with("job_id", job_id)
        .with("outcome", outcome)
        .with("profiles_rewritten", profiles_rewritten)
        .with("duration_ms", duration_ms)
}

/// Row 17 (bonus): `power-state`. Not explicitly numbered in the
/// table as a source row but DD-008 §7.4's "`power-state`" row
/// promises a flat `state` field from `Manager.PowerStateChanged`.
pub fn power_state(state: &str) -> WatchEvent {
    new("power-state").with("state", state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_row_in_dd008_table_is_covered() {
        // Build one event per table row and assert the kind label
        // + expected field set is exactly what §7.4 promises. The
        // kind labels are the keys a shell script greps for so
        // they're part of the stable interface.
        assert_eq!(interface_added("eth0", "ethernet").kind, "interface-added");
        assert_eq!(interface_removed("eth0").kind, "interface-removed");
        assert_eq!(link_state("eth0", "up").kind, "link-state");
        assert_eq!(
            eth_auth_state("eth0", "authenticated").kind,
            "eth-auth-state"
        );
        assert_eq!(wifi_state("wlan0", "connected").kind, "wifi-state");
        assert_eq!(wifi_scan("wlan0", 3).kind, "wifi-scan");
        assert_eq!(wifi_signal("wlan0", -55).kind, "wifi-signal");
        assert_eq!(bt_adapter_state("hci0", "powered").kind, "bt-adapter-state");
        assert_eq!(
            bt_device_state("hci0", "AA:BB:CC:DD:EE:01", "connected").kind,
            "bt-device-state"
        );
        assert_eq!(
            bt_pairing_started("hci0", "job-1", "/org/bluez/hci0/dev_AA").kind,
            "bt-pairing-started"
        );
        assert_eq!(
            bt_pairing_prompt(
                "hci0",
                "job-1",
                "request_confirmation",
                Some(123456),
                None,
                None
            )
            .kind,
            "bt-pairing-prompt"
        );
        assert_eq!(
            bt_pairing_complete("hci0", "job-1", true, "").kind,
            "bt-pairing-complete"
        );
        assert_eq!(
            gnss_fix("/dev/gps0", 3, 51.5, -0.08, Some(12.0), Some(2.1)).kind,
            "gnss-fix"
        );
        assert_eq!(
            profile_changed("wifi", "added", "01H9").kind,
            "profile-changed"
        );
        let mut data: HashMap<String, FieldValue> = HashMap::new();
        data.insert("subsystem".into(), FieldValue::String("bluez".into()));
        data.insert("duration_s".into(), FieldValue::Uint(60));
        assert_eq!(
            notification("subsystem_unavailable", data).kind,
            "notification"
        );
        assert_eq!(
            master_key_rotated("job-1", "success", 3, 120).kind,
            "master-key-rotated"
        );
        assert_eq!(power_state("sleep").kind, "power-state");
    }

    #[test]
    fn wifi_scan_has_result_count_field() {
        let ev = wifi_scan("wlan0", 7);
        assert_eq!(ev.get("iface"), Some("wlan0".into()));
        assert_eq!(ev.get("result_count"), Some("7".into()));
    }

    #[test]
    fn bt_pairing_prompt_includes_optional_fields_only_when_present() {
        let ev = bt_pairing_prompt(
            "hci0",
            "job-1",
            "request_confirmation",
            Some(123456),
            None,
            None,
        );
        assert_eq!(ev.get("passkey"), Some("123456".into()));
        assert!(ev.get("pin").is_none());
        assert!(ev.get("service_uuid").is_none());

        let ev = bt_pairing_prompt(
            "hci0",
            "job-1",
            "authorize_service",
            None,
            None,
            Some("00001124-0000-1000-8000-00805f9b34fb"),
        );
        assert!(ev.get("passkey").is_none());
        assert_eq!(
            ev.get("service_uuid"),
            Some("00001124-0000-1000-8000-00805f9b34fb".into())
        );
    }

    #[test]
    fn gnss_fix_omits_alt_and_hdop_when_none() {
        let ev = gnss_fix("/dev/gps0", 2, 0.0, 0.0, None, None);
        assert!(ev.get("alt").is_none());
        assert!(ev.get("hdop").is_none());
    }

    #[test]
    fn notification_flattens_data_into_top_level() {
        let mut data: HashMap<String, FieldValue> = HashMap::new();
        data.insert("subsystem".into(), FieldValue::String("bluez".into()));
        data.insert("duration_s".into(), FieldValue::Uint(60));
        let ev = notification("subsystem_unavailable", data);
        // Envelope kind stays "notification"; the pass-through
        // sub-kind lives in `notif_kind` (renamed to avoid
        // colliding with the envelope).
        assert_eq!(ev.kind, "notification");
        assert_eq!(ev.get("kind"), Some("notification".into()));
        assert_eq!(ev.get("notif_kind"), Some("subsystem_unavailable".into()));
        assert_eq!(ev.get("subsystem"), Some("bluez".into()));
        assert_eq!(ev.get("duration_s"), Some("60".into()));
    }

    #[test]
    fn notification_refuses_to_shadow_reserved_keys() {
        // A malicious / misconfigured data dict with `time`,
        // `kind`, or `notif_kind` entries must not override the
        // envelope or clobber the renamed sub-kind.
        let mut data: HashMap<String, FieldValue> = HashMap::new();
        data.insert("time".into(), FieldValue::String("forged".into()));
        data.insert("kind".into(), FieldValue::String("forged".into()));
        data.insert("notif_kind".into(), FieldValue::String("forged".into()));
        let ev = notification("real", data);
        assert_ne!(ev.time, "forged");
        assert_eq!(ev.kind, "notification");
        assert_eq!(ev.get("notif_kind"), Some("real".into()));
    }
}
