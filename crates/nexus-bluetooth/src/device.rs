//! Pure per-device state transitions. See DD-004 §5.2.
//!
//! Like [`crate::adapter`], this module is purely a `(state,
//! signal) → state` mapping. The backend owns the
//! [`crate::types::BtDeviceEntry`] and drives everything else.

use std::time::Instant;

use nexus_core::{BtFailureReason, PairingJobId};

use crate::types::BtDeviceState;

/// Input to the device state machine. Mirrors DD-004 §5.2.
#[derive(Debug, Clone)]
pub enum DeviceSignal {
    /// BlueZ observed the device for the first time; initial flags
    /// pulled from the `Device1` property bag.
    ObservedFromBluez { paired: bool, connected: bool },
    /// BlueZ fired `PropertiesChanged` with `Paired = true`.
    PropsPairedTrue,
    /// BlueZ fired `PropertiesChanged` with `Connected = true`.
    PropsConnectedTrue { services: Vec<String> },
    /// BlueZ fired `PropertiesChanged` with `Connected = false`.
    /// Bonded devices drop back to `Paired`; unbonded BLE to
    /// `Discovered`.
    PropsConnectedFalse { paired: bool },
    /// Operator called `Pair(device)`. The backend subsequently
    /// spawns a driver task that awaits BlueZ's Pair() return.
    OperatorPair { job_id: PairingJobId },
    /// Operator called `Connect(device)`.
    OperatorConnect,
    /// Operator called `Disconnect(device)`.
    OperatorDisconnect,
    /// The pair-driver task reported completion.
    PairComplete {
        success: bool,
        reason: Option<BtFailureReason>,
    },
    /// BlueZ's `InterfacesRemoved` dropped the device, or operator
    /// called `Forget`.
    Removed,
}

/// Compute the next device state. Pure; `now` is injected for
/// determinism in tests.
pub fn next_state(current: &BtDeviceState, signal: DeviceSignal, now: Instant) -> BtDeviceState {
    use BtDeviceState as D;
    use DeviceSignal as S;

    match (current, signal) {
        // Removal / Forget always wins.
        (_, S::Removed) => D::Removed,

        // Initial observation. Choose by BlueZ-reported flags.
        (
            _,
            S::ObservedFromBluez {
                connected: true, ..
            },
        ) => D::Connected {
            since: now,
            services: Vec::new(),
        },
        (_, S::ObservedFromBluez { paired: true, .. }) => D::Paired,
        (_, S::ObservedFromBluez { .. }) => D::Discovered,

        // Paired toggle is authoritative — even a Pairing-in-flight
        // device should land in Paired once BlueZ confirms.
        (_, S::PropsPairedTrue) => D::Paired,

        // Connected toggles.
        (_, S::PropsConnectedTrue { services }) => D::Connected {
            since: now,
            services,
        },
        (D::Connected { .. }, S::PropsConnectedFalse { paired: true }) => D::Paired,
        (D::Connected { .. }, S::PropsConnectedFalse { paired: false }) => D::Discovered,
        (D::Disconnecting, S::PropsConnectedFalse { paired: true }) => D::Paired,
        (D::Disconnecting, S::PropsConnectedFalse { paired: false }) => D::Discovered,
        // From any non-Connected state, Connected=false is a no-op.
        (other, S::PropsConnectedFalse { .. }) => other.clone(),

        // Operator-driven transitions.
        (D::Discovered | D::Failed { .. }, S::OperatorPair { job_id }) => D::Pairing {
            job_id,
            started_at: now,
        },
        (D::Paired | D::Discovered | D::Failed { .. }, S::OperatorConnect) => {
            D::Connecting { since: now }
        }
        (D::Connected { .. }, S::OperatorDisconnect) => D::Disconnecting,

        // Pair driver outcome.
        (D::Pairing { .. }, S::PairComplete { success: true, .. }) => D::Paired,
        (
            D::Pairing { .. },
            S::PairComplete {
                success: false,
                reason,
            },
        ) => D::Failed {
            reason: reason.unwrap_or(BtFailureReason::Unknown("".into())),
            at: now,
        },

        // Anything else: keep the current state.
        (other, _) => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn observe_unpaired_lands_in_discovered() {
        let got = next_state(
            &BtDeviceState::Discovered,
            DeviceSignal::ObservedFromBluez {
                paired: false,
                connected: false,
            },
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Discovered));
    }

    #[test]
    fn observe_paired_lands_in_paired() {
        let got = next_state(
            &BtDeviceState::Discovered,
            DeviceSignal::ObservedFromBluez {
                paired: true,
                connected: false,
            },
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Paired));
    }

    #[test]
    fn observe_connected_wins_over_paired_flag() {
        let got = next_state(
            &BtDeviceState::Discovered,
            DeviceSignal::ObservedFromBluez {
                paired: true,
                connected: true,
            },
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Connected { .. }));
    }

    #[test]
    fn operator_pair_transitions_from_discovered() {
        let job = PairingJobId(ulid::Ulid::new());
        let got = next_state(
            &BtDeviceState::Discovered,
            DeviceSignal::OperatorPair { job_id: job },
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Pairing { .. }));
    }

    #[test]
    fn pair_complete_success_lands_in_paired() {
        let start = BtDeviceState::Pairing {
            job_id: PairingJobId(ulid::Ulid::new()),
            started_at: t0(),
        };
        let got = next_state(
            &start,
            DeviceSignal::PairComplete {
                success: true,
                reason: None,
            },
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Paired));
    }

    #[test]
    fn pair_complete_failure_lands_in_failed_with_reason() {
        let start = BtDeviceState::Pairing {
            job_id: PairingJobId(ulid::Ulid::new()),
            started_at: t0(),
        };
        let got = next_state(
            &start,
            DeviceSignal::PairComplete {
                success: false,
                reason: Some(BtFailureReason::PairingRejected),
            },
            t0(),
        );
        match got {
            BtDeviceState::Failed {
                reason: BtFailureReason::PairingRejected,
                ..
            } => {}
            other => panic!("expected Failed(PairingRejected), got {other:?}"),
        }
    }

    #[test]
    fn connected_to_disconnecting_on_operator_disconnect() {
        let start = BtDeviceState::Connected {
            since: t0(),
            services: vec![],
        };
        let got = next_state(&start, DeviceSignal::OperatorDisconnect, t0());
        assert!(matches!(got, BtDeviceState::Disconnecting));
    }

    #[test]
    fn disconnecting_to_paired_on_props_connected_false() {
        let got = next_state(
            &BtDeviceState::Disconnecting,
            DeviceSignal::PropsConnectedFalse { paired: true },
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Paired));
    }

    #[test]
    fn connected_to_discovered_on_unbonded_disconnect() {
        let got = next_state(
            &BtDeviceState::Connected {
                since: t0(),
                services: vec![],
            },
            DeviceSignal::PropsConnectedFalse { paired: false },
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Discovered));
    }

    #[test]
    fn removed_wins_from_any_state() {
        for start in [
            BtDeviceState::Discovered,
            BtDeviceState::Paired,
            BtDeviceState::Connecting { since: t0() },
            BtDeviceState::Connected {
                since: t0(),
                services: vec![],
            },
        ] {
            let got = next_state(&start, DeviceSignal::Removed, t0());
            assert!(matches!(got, BtDeviceState::Removed));
        }
    }

    #[test]
    fn bonded_connect_from_paired_goes_to_connecting() {
        let got = next_state(&BtDeviceState::Paired, DeviceSignal::OperatorConnect, t0());
        assert!(matches!(got, BtDeviceState::Connecting { .. }));
    }

    #[test]
    fn ble_unbonded_connect_from_discovered_goes_to_connecting() {
        let got = next_state(
            &BtDeviceState::Discovered,
            DeviceSignal::OperatorConnect,
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Connecting { .. }));
    }

    #[test]
    fn failed_can_retry_pair() {
        let start = BtDeviceState::Failed {
            reason: BtFailureReason::PairingRejected,
            at: t0(),
        };
        let got = next_state(
            &start,
            DeviceSignal::OperatorPair {
                job_id: PairingJobId(ulid::Ulid::new()),
            },
            t0(),
        );
        assert!(matches!(got, BtDeviceState::Pairing { .. }));
    }
}
