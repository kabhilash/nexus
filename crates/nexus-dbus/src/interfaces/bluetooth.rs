//! `fi.nexus.Bluetooth` — DD-006 §6.4. Properties + the operator
//! mutating surface (`Powered` / `Discoverable` / `Pairable`
//! setters, `StartDiscovery` / `StopDiscovery` methods).

use std::collections::HashMap;
use std::sync::Arc;

use nexus_core::{PairingAnswer, PairingJobId};
use ulid::Ulid;
use zbus::fdo;
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use crate::authz::actions;
use crate::backend_ops::BtDiscoveryFilter;
use crate::errors::DbusError;
use crate::paths::bluetooth_device_path;
use crate::services::{Feature, Services};
use crate::state::InterfaceKindData;

pub struct BluetoothIface {
    pub services: Arc<Services>,
    pub ifname: String,
}

impl BluetoothIface {
    pub fn new(services: Arc<Services>, ifname: impl Into<String>) -> Self {
        Self {
            services,
            ifname: ifname.into(),
        }
    }

    async fn with_cache<R>(
        &self,
        default: R,
        f: impl FnOnce(&crate::state::BluetoothAdapterState) -> R,
    ) -> R {
        let guard = self.services.state.read().await;
        match guard.interfaces.get(&self.ifname).map(|e| &e.kind_data) {
            Some(InterfaceKindData::Bluetooth(c)) => f(c),
            _ => default,
        }
    }

    /// Resolve this adapter's BlueZ object path (`/org/bluez/hciN`)
    /// from the registry cache. Internal nexus-bluetooth callers pass
    /// the path everywhere (`adapter_proxy` parses it as an
    /// `ObjectPath`); the kernel ifname `hci0` is NOT a valid object
    /// path on its own. When the cache hasn't seen this adapter yet
    /// (rare, but possible during cold-boot races), fall back to the
    /// canonical BlueZ scheme so the call still has a chance of
    /// landing.
    async fn bluez_path(&self) -> String {
        let guard = self.services.state.read().await;
        guard
            .interfaces
            .get(&self.ifname)
            .and_then(|e| match &e.info.kind {
                nexus_core::InterfaceKind::Bluetooth { bluez_path, .. } => {
                    Some(bluez_path.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| format!("/org/bluez/{}", self.ifname))
    }

    /// Resolve a `fi.nexus.BluetoothDevice` object path (as passed to
    /// `Pair(device: o)` / `CancelPairing(device: o)`) into the raw
    /// BlueZ device path `nexus-bluetooth`'s `BtCommand`s expect.
    /// There's no reverse-path-parsing shortcut here (unlike
    /// `BluetoothDeviceIface::device_bluez_path`, which already knows
    /// its own address) — this searches this adapter's cached
    /// `known_devices` for an entry whose reconstructed path matches,
    /// returning `None` (→ `fi.nexus.Error.UnknownDevice`) if the
    /// path isn't a child of this adapter.
    async fn resolve_device_bluez_path(&self, device: &str) -> Option<String> {
        let guard = self.services.state.read().await;
        let InterfaceKindData::Bluetooth(c) = &guard.interfaces.get(&self.ifname)?.kind_data
        else {
            return None;
        };
        c.known_devices.values().find_map(|d| {
            let path = bluetooth_device_path(&self.ifname, &d.info.address);
            (path == device).then(|| d.info.device_path.clone())
        })
    }

    fn check_feature(&self) -> fdo::Result<()> {
        self.services
            .enabled
            .require(Feature::Bluetooth)
            .map_err(fdo::Error::from)
    }

    async fn require_auth(&self, hdr: &Header<'_>, action: &str) -> fdo::Result<()> {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        if self
            .services
            .auth
            .check(action, &sender)
            .await
            .is_authorized()
        {
            Ok(())
        } else {
            Err(fdo::Error::from(DbusError::AuthFailed(format!(
                "policykit denied '{action}' for sender '{sender}'"
            ))))
        }
    }
}

#[zbus::interface(name = "fi.nexus.Bluetooth")]
impl BluetoothIface {
    /// `Address` is the adapter's BD_ADDR. It's a hardware ID that's
    /// fixed once the controller is fully up, but the value Nexus
    /// first observes at discovery can be a zeroed placeholder for
    /// two different reasons: some controllers (BCM/Cypress/Marvell)
    /// have firmware that finishes loading *after* the kernel
    /// registers the HCI device (`nexus-interface-monitor::udev` can
    /// catch up on the resulting `change` uevent), and UART/serdev-
    /// attached controllers (no USB HCI device) never expose a kernel
    /// sysfs address at all, in which case that path never fires.
    /// BlueZ's own `Adapter1.Address` is authoritative and covers
    /// both cases — `nexus-bluetooth`'s reconcile loop
    /// (`BluezClient::refresh_adapter`, DD-004 §7.3) polls it and
    /// corrects `InterfaceInfo.kind` via `NexusEvent::MacChanged`
    /// whenever it disagrees with the cache. We read from
    /// `InterfaceInfo.kind` here rather than the mutable
    /// `BluetoothAdapterState.address` (which only gets populated on
    /// the BlueZ-side `BtAdapterChanged` event and was empty for
    /// adapters whose only event was the initial discovery), so this
    /// property can change post-discovery — in practice rarely, and
    /// usually just once.
    #[zbus(property, name = "Address")]
    async fn address(&self) -> String {
        use nexus_core::BluetoothAddrExt;
        let guard = self.services.state.read().await;
        match guard.interfaces.get(&self.ifname).map(|e| &e.info.kind) {
            Some(nexus_core::InterfaceKind::Bluetooth { bt_address, .. }) => {
                bt_address.to_bluez()
            }
            _ => String::new(),
        }
    }

    #[zbus(property, name = "Powered")]
    async fn powered(&self) -> bool {
        self.with_cache(false, |c| c.powered).await
    }

    /// `Powered` writable side — `fi.nexus.set_power`. Mirrors the
    /// Wi-Fi pattern: feature-gated, polkit-checked against the
    /// caller's unique bus name (which zbus threads in via
    /// `#[zbus(header)]`), then routed through
    /// [`crate::BackendOps::bt_set_powered`] to BlueZ. The setter
    /// returns `zbus::Result<()>` per zbus's property contract;
    /// `DbusError` flows through `zbus::fdo::Error::from`.
    #[zbus(property)]
    async fn set_powered(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
        on: bool,
    ) -> zbus::Result<()> {
        gate_property_setter(self, hdr.as_ref(), actions::SET_POWER).await?;
        let path = self.bluez_path().await;
        self.services
            .ops
            .bt_set_powered(&path, on)
            .await
            .map_err(|e| zbus::Error::from(zbus::fdo::Error::from(e)))
    }

    /// `Discoverable` writable — `fi.nexus.connect`. Routes to
    /// [`crate::BackendOps::bt_set_discoverable`].
    #[zbus(property)]
    async fn set_discoverable(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
        on: bool,
    ) -> zbus::Result<()> {
        gate_property_setter(self, hdr.as_ref(), actions::CONNECT).await?;
        let path = self.bluez_path().await;
        self.services
            .ops
            .bt_set_discoverable(&path, on)
            .await
            .map_err(|e| zbus::Error::from(zbus::fdo::Error::from(e)))
    }

    /// `Pairable` writable — `fi.nexus.connect`. Routes to
    /// [`crate::BackendOps::bt_set_pairable`].
    #[zbus(property)]
    async fn set_pairable(
        &self,
        #[zbus(header)] hdr: Option<Header<'_>>,
        on: bool,
    ) -> zbus::Result<()> {
        gate_property_setter(self, hdr.as_ref(), actions::CONNECT).await?;
        let path = self.bluez_path().await;
        self.services
            .ops
            .bt_set_pairable(&path, on)
            .await
            .map_err(|e| zbus::Error::from(zbus::fdo::Error::from(e)))
    }

    /// `StartDiscovery(filter: a{sv}) -> ()` — DD-006 §6.4.
    /// Recognised keys: `transport` (s), `rssi` (n), `uuids` (as),
    /// `duplicate_data` (b). Unknown keys are ignored — clients can
    /// probe future-added knobs without server-side validation churn.
    async fn start_discovery(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        filter: HashMap<String, OwnedValue>,
    ) -> fdo::Result<()> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let parsed = decode_discovery_filter(&filter).map_err(|e| {
            fdo::Error::from(DbusError::InvalidArgument(e))
        })?;
        let path = self.bluez_path().await;
        self.services
            .ops
            .bt_start_discovery(&path, parsed)
            .await
            .map_err(fdo::Error::from)
    }

    /// `StopDiscovery() -> ()` — DD-006 §6.4. The bluetooth backend
    /// only stops the radio when *every* nexus-driven discovery
    /// session has been stopped (BlueZ handles ref-counting across
    /// senders).
    async fn stop_discovery(&self, #[zbus(header)] hdr: Header<'_>) -> fdo::Result<()> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let path = self.bluez_path().await;
        self.services
            .ops
            .bt_stop_discovery(&path)
            .await
            .map_err(fdo::Error::from)
    }

    /// `Pair(device: o) -> (job_id: s)` — DD-006 §6.4. Returns a
    /// pairing job id (ULID string) that correlates subsequent
    /// `PairingPrompt` and `PairingComplete` signals on this adapter
    /// object.
    async fn pair(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
    ) -> fdo::Result<String> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let device_path = self
            .resolve_device_bluez_path(device.as_str())
            .await
            .ok_or_else(|| {
                fdo::Error::from(DbusError::NotFound(format!("unknown device: {device}")))
            })?;
        let job_id = self
            .services
            .ops
            .bt_pair(&device_path)
            .await
            .map_err(fdo::Error::from)?;
        Ok(job_id.0.to_string())
    }

    /// `CancelPairing(device: o) -> ()` — DD-006 §6.4. Cancel an
    /// in-flight pairing. Maps to BlueZ's `CancelPairing`.
    async fn cancel_pairing(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        device: OwnedObjectPath,
    ) -> fdo::Result<()> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let device_path = self
            .resolve_device_bluez_path(device.as_str())
            .await
            .ok_or_else(|| {
                fdo::Error::from(DbusError::NotFound(format!("unknown device: {device}")))
            })?;
        self.services
            .ops
            .bt_cancel_pairing(&device_path)
            .await
            .map_err(fdo::Error::from)
    }

    /// `AnswerPairingPrompt(job_id: s, answer: v) -> ()` — DD-006
    /// §6.4. `answer`'s D-Bus type varies with the pending prompt's
    /// kind (`s` for PIN/acknowledge/the universal "cancel", `u` for
    /// a passkey, `b` for accept/reject); [`decode_pairing_answer`]
    /// does the type-shape parsing. The backend then validates the
    /// parsed answer against the *actually* pending kind
    /// (`nexus_bluetooth::pairing::validate_answer`) before resolving
    /// the Agent's oneshot — this method never needs to know which
    /// kind of prompt is pending.
    async fn answer_pairing_prompt(
        &self,
        #[zbus(header)] hdr: Header<'_>,
        job_id: String,
        answer: Value<'_>,
    ) -> fdo::Result<()> {
        self.check_feature()?;
        self.require_auth(&hdr, actions::CONNECT).await?;
        let job_id = Ulid::from_string(&job_id)
            .map(PairingJobId)
            .map_err(|e| fdo::Error::from(DbusError::InvalidArgument(format!("bad job_id: {e}"))))?;
        let answer = decode_pairing_answer(&answer).map_err(fdo::Error::from)?;
        self.services
            .ops
            .bt_answer_pairing_prompt(job_id, answer)
            .await
            .map_err(fdo::Error::from)
    }

    /// `PairingStarted(job_id: s, device: o)` — DD-006 §6.4. Emitted
    /// from the service event loop (`service::emit_bt_pairing_started`)
    /// via raw `connection.emit_signal`, same as `fi.nexus.Manager`'s
    /// event-driven signals — this declaration exists for
    /// introspection so typed proxy clients see the signal's shape.
    #[zbus(signal)]
    pub async fn pairing_started(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        device: &ObjectPath<'_>,
    ) -> zbus::Result<()>;

    /// `PairingPrompt(job_id: s, kind: s, data: a{sv})` — DD-006
    /// §6.4. See that section for `kind`'s value set and `data`'s
    /// per-kind keys.
    #[zbus(signal)]
    pub async fn pairing_prompt(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        kind: &str,
        data: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    /// `PairingComplete(job_id: s, success: b, reason: s)` — DD-006
    /// §6.4. `reason` is `""` on success, else one of `"rejected"` /
    /// `"timeout"` / `"auth_failed"` / `"connection_failed"` /
    /// `"other"`.
    #[zbus(signal)]
    pub async fn pairing_complete(
        emitter: &SignalEmitter<'_>,
        job_id: &str,
        success: bool,
        reason: &str,
    ) -> zbus::Result<()>;

    #[zbus(property, name = "Discoverable")]
    async fn discoverable(&self) -> bool {
        self.with_cache(false, |c| c.discoverable).await
    }

    #[zbus(property, name = "Pairable")]
    async fn pairable(&self) -> bool {
        self.with_cache(false, |c| c.pairable).await
    }

    #[zbus(property, name = "Discovering")]
    async fn discovering(&self) -> bool {
        self.with_cache(false, |c| c.discovering).await
    }

    #[zbus(property, name = "NexusDiscovering")]
    async fn nexus_discovering(&self) -> bool {
        self.with_cache(false, |c| c.nexus_discovering).await
    }

    #[zbus(property, name = "KnownDevices")]
    async fn known_devices(&self) -> Vec<OwnedObjectPath> {
        let adapter = self.ifname.clone();
        self.with_cache(Vec::<OwnedObjectPath>::new(), |c| {
            c.known_devices
                .values()
                .filter_map(|d| {
                    let path = bluetooth_device_path(&adapter, &d.info.address);
                    ObjectPath::try_from(path).ok().map(OwnedObjectPath::from)
                })
                .collect()
        })
        .await
    }

    #[zbus(property, name = "State")]
    async fn state(&self) -> String {
        self.with_cache(String::new(), |c| c.state.clone()).await
    }
}

/// Common gate for the property setters on `BluetoothIface`:
/// feature-disabled short-circuit + polkit deny → `AuthFailed`.
/// Returns the same `zbus::Result<()>` shape every property setter
/// uses, so each one can `?`-propagate.
async fn gate_property_setter(
    iface: &BluetoothIface,
    hdr: Option<&Header<'_>>,
    action: &str,
) -> zbus::Result<()> {
    if !iface.services.enabled.is_enabled(Feature::Bluetooth) {
        return Err(zbus::Error::from(zbus::fdo::Error::from(
            DbusError::FeatureDisabled("bluetooth".to_owned()),
        )));
    }
    let sender = hdr
        .and_then(|h| h.sender().map(|s| s.to_string()))
        .unwrap_or_default();
    if iface
        .services
        .auth
        .check(action, &sender)
        .await
        .is_authorized()
    {
        Ok(())
    } else {
        Err(zbus::Error::from(zbus::fdo::Error::AuthFailed(format!(
            "policykit denied '{action}' for sender '{sender}'"
        ))))
    }
}

/// Decode the `Bluetooth.StartDiscovery(filter: a{sv})` dict into
/// the cross-crate [`BtDiscoveryFilter`] view. Unknown keys are
/// silently ignored — clients can probe new fields without
/// breaking the call. Type mismatches return a human-readable
/// reason (mapped to `InvalidArgument` by the caller).
fn decode_discovery_filter(
    dict: &HashMap<String, OwnedValue>,
) -> std::result::Result<BtDiscoveryFilter, String> {
    let mut out = BtDiscoveryFilter::default();
    if let Some(v) = dict.get("transport") {
        let s: &str = <&str>::try_from(v)
            .map_err(|e| format!("'transport' must be a string: {e}"))?;
        out.transport = Some(s.to_owned());
    }
    if let Some(v) = dict.get("rssi") {
        let n = i16::try_from(v)
            .map_err(|e| format!("'rssi' must be int16: {e}"))?;
        out.rssi = Some(n);
    }
    if let Some(v) = dict.get("uuids") {
        let arr = <&zbus::zvariant::Array>::try_from(v)
            .map_err(|e| format!("'uuids' must be a string array: {e}"))?;
        out.uuids = arr
            .iter()
            .filter_map(|item| <&str>::try_from(item).ok().map(str::to_owned))
            .collect();
    }
    if let Some(v) = dict.get("duplicate_data") {
        out.duplicate_data = bool::try_from(v)
            .map_err(|e| format!("'duplicate_data' must be a boolean: {e}"))?;
    }
    Ok(out)
}

/// Decode a D-Bus variant payload into a neutral [`PairingAnswer`].
///
/// This is the D-Bus-edge translation layer for `AnswerPairingPrompt`
/// per DD-006 §6.4. It performs type-shape validation only — the
/// backend applies the per-prompt-kind map (so this function never
/// needs to know which kind of prompt is pending). Recognised shapes:
///
/// - `s:"cancel"` → [`PairingAnswer::Cancel`]
/// - `s:"acknowledge"` → [`PairingAnswer::Acknowledge`]
/// - `s:<other>` → [`PairingAnswer::Pin`]
/// - `u:<n>` → [`PairingAnswer::Passkey`]
/// - `b:<v>` → [`PairingAnswer::Accept`]
///
/// The backend validates length + printable-ASCII on the `Pin`
/// variant for RequestPin prompts (and rejects `Pin` for every
/// other prompt kind), and the `0..=999_999` range on `Passkey`.
///
/// Any other variant type returns `fi.nexus.Error.InvalidArgument`
/// with a message naming the expected types.
pub fn decode_pairing_answer(value: &Value<'_>) -> crate::errors::Result<PairingAnswer> {
    // String: three sub-shapes — "cancel", "acknowledge", anything
    // else (treated as a PIN candidate; the backend's per-kind
    // validator accepts it for RequestPin and rejects it elsewhere).
    if let Ok(s) = <&str>::try_from(value) {
        return Ok(match s {
            "cancel" => PairingAnswer::Cancel,
            "acknowledge" => PairingAnswer::Acknowledge,
            other => PairingAnswer::Pin(other.to_owned()),
        });
    }
    if let Ok(n) = u32::try_from(value) {
        return Ok(PairingAnswer::Passkey(n));
    }
    if let Ok(b) = bool::try_from(value) {
        return Ok(PairingAnswer::Accept(b));
    }
    Err(DbusError::InvalidArgument(format!(
        "answer variant must be s, u, or b; got {:?}",
        value.value_signature()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_string_cancel() {
        let v = Value::new("cancel");
        assert_eq!(decode_pairing_answer(&v).unwrap(), PairingAnswer::Cancel);
    }

    #[test]
    fn decode_string_acknowledge() {
        let v = Value::new("acknowledge");
        assert_eq!(
            decode_pairing_answer(&v).unwrap(),
            PairingAnswer::Acknowledge
        );
    }

    #[test]
    fn decode_string_other_becomes_pin_candidate() {
        let v = Value::new("1234");
        match decode_pairing_answer(&v).unwrap() {
            PairingAnswer::Pin(s) => assert_eq!(s, "1234"),
            other => panic!("expected Pin, got {other:?}"),
        }
    }

    #[test]
    fn decode_u32_becomes_passkey() {
        let v = Value::U32(123_456);
        assert_eq!(
            decode_pairing_answer(&v).unwrap(),
            PairingAnswer::Passkey(123_456)
        );
    }

    #[test]
    fn decode_bool_becomes_accept() {
        assert_eq!(
            decode_pairing_answer(&Value::Bool(true)).unwrap(),
            PairingAnswer::Accept(true)
        );
        assert_eq!(
            decode_pairing_answer(&Value::Bool(false)).unwrap(),
            PairingAnswer::Accept(false)
        );
    }

    #[test]
    fn decode_unsupported_variant_is_invalid_argument() {
        // Array of strings is not a recognised shape.
        let arr: Vec<String> = vec!["a".into(), "b".into()];
        let v = Value::new(arr);
        let err = decode_pairing_answer(&v).unwrap_err();
        assert!(
            matches!(err, DbusError::InvalidArgument(_)),
            "expected InvalidArgument, got {err:?}"
        );
    }

    #[test]
    fn decode_signed_int_is_invalid_argument() {
        // `i32` is not in the allowed set — only `u` is accepted
        // for passkey.
        let v = Value::I32(-1);
        let err = decode_pairing_answer(&v).unwrap_err();
        assert!(matches!(err, DbusError::InvalidArgument(_)));
    }
}
