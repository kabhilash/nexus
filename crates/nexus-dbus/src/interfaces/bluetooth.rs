//! `fi.nexus.Bluetooth` — DD-006 §6.4. Read-only for phase 2.

use std::sync::Arc;

use nexus_core::PairingAnswer;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, Value};

use crate::errors::DbusError;
use crate::paths::bluetooth_device_path;
use crate::services::Services;
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
}

#[zbus::interface(name = "fi.nexus.Bluetooth")]
impl BluetoothIface {
    #[zbus(property, name = "Address")]
    async fn address(&self) -> String {
        self.with_cache(String::new(), |c| c.address.clone()).await
    }

    #[zbus(property, name = "Powered")]
    async fn powered(&self) -> bool {
        self.with_cache(false, |c| c.powered).await
    }

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

/// Decode a D-Bus variant payload into a neutral [`PairingAnswer`].
///
/// This is the D-Bus-edge translation layer for `AnswerPairingPrompt`
/// per DD-006 §6.4. It performs type-shape validation only — the
/// backend applies the per-prompt-kind map (so this function never
/// needs to know which kind of prompt is pending). Recognised shapes:
///
/// - `s:"cancel"`      → [`PairingAnswer::Cancel`]
/// - `s:"acknowledge"` → [`PairingAnswer::Acknowledge`]
/// - `s:<other>`       → [`PairingAnswer::Pin`] (backend validates
///                        length + printable-ASCII for RequestPin,
///                        and rejects the Pin variant for every
///                        other kind)
/// - `u:<n>`           → [`PairingAnswer::Passkey`] (backend
///                        validates the 0..=999_999 range)
/// - `b:<v>`           → [`PairingAnswer::Accept`]
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
