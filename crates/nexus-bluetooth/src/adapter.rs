//! Pure per-adapter state transitions. See DD-004 §4.2.
//!
//! This module owns the state-transition logic only — the backend
//! orchestrates who owns the [`crate::types::BtAdapterEntry`] and emits metrics;
//! this module just maps `(current_state, signal) → next_state`.

use std::time::Instant;

use crate::types::BtAdapterState;

/// Input to the adapter state machine. Each variant corresponds
/// directly to a DD-004 §4.2 transition trigger.
#[derive(Debug, Clone, Copy)]
pub enum AdapterSignal {
    /// BlueZ's `ObjectManager` just published the adapter for the
    /// first time (or the backend reconnected and the ObjectManager
    /// republish delivered it again).
    BluezPublished { powered: bool, discovering: bool },
    /// A `PropertiesChanged` on `org.bluez.Adapter1` updated one or
    /// both of `Powered` / `Discovering`.
    PropsChanged { powered: bool, discovering: bool },
    /// DD-001's `InterfaceRemoved` or BlueZ's `InterfacesRemoved`
    /// — the adapter is gone regardless of previous state.
    Removed,
    /// The BlueZ D-Bus connection dropped. The adapter reverts to
    /// `Unavailable` until BlueZ republishes it.
    BluezLost,
}

/// Compute the next state for a signal. Pure; no I/O. The backend
/// writes `entry.state = next_state(...)` and emits a
/// `BtAdapterChanged`-equivalent metric inside its event handler.
///
/// `now` is injected rather than read from `Instant::now()` so
/// tests are deterministic.
pub fn next_state(current: &BtAdapterState, signal: AdapterSignal, now: Instant) -> BtAdapterState {
    use AdapterSignal as S;
    use BtAdapterState as A;

    match (current, signal) {
        // Kernel / BlueZ removal wins from any state.
        (_, S::Removed) => A::Gone,
        (_, S::BluezLost) => A::Unavailable,

        // ObjectManager publish — the initial reveal of an adapter
        // whose state may already have advanced (BlueZ can hand us
        // a powered + discovering adapter in one go).
        (_, S::BluezPublished { powered: false, .. }) => A::Present,
        (
            _,
            S::BluezPublished {
                powered: true,
                discovering: false,
            },
        ) => A::Powered,
        (
            _,
            S::BluezPublished {
                powered: true,
                discovering: true,
            },
        ) => A::Discovering { since: now },

        // PropertiesChanged — the steady-state driver.
        (A::Unavailable, S::PropsChanged { powered: false, .. }) => A::Present,
        (
            A::Unavailable,
            S::PropsChanged {
                powered: true,
                discovering: false,
            },
        ) => A::Powered,
        (
            A::Unavailable,
            S::PropsChanged {
                powered: true,
                discovering: true,
            },
        ) => A::Discovering { since: now },
        (
            A::Present,
            S::PropsChanged {
                powered: true,
                discovering: false,
            },
        ) => A::Powered,
        (
            A::Present,
            S::PropsChanged {
                powered: true,
                discovering: true,
            },
        ) => A::Discovering { since: now },
        (A::Powered, S::PropsChanged { powered: false, .. }) => A::Present,
        (
            A::Powered,
            S::PropsChanged {
                powered: true,
                discovering: true,
            },
        ) => A::Discovering { since: now },
        (
            A::Powered,
            S::PropsChanged {
                powered: true,
                discovering: false,
            },
        ) => A::Powered,
        (
            A::Discovering { since },
            S::PropsChanged {
                powered: true,
                discovering: true,
            },
        ) => A::Discovering { since: *since },
        (
            A::Discovering { .. },
            S::PropsChanged {
                powered: true,
                discovering: false,
            },
        ) => A::Powered,
        (A::Discovering { .. }, S::PropsChanged { powered: false, .. }) => A::Present,
        (A::Gone, _) => A::Gone,

        // Any uncovered combination: retain the current state.
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
    fn publish_powered_lands_in_powered() {
        let got = next_state(
            &BtAdapterState::Unavailable,
            AdapterSignal::BluezPublished {
                powered: true,
                discovering: false,
            },
            t0(),
        );
        assert!(matches!(got, BtAdapterState::Powered));
    }

    #[test]
    fn publish_powered_and_discovering_skips_powered_intermediate() {
        let got = next_state(
            &BtAdapterState::Unavailable,
            AdapterSignal::BluezPublished {
                powered: true,
                discovering: true,
            },
            t0(),
        );
        assert!(matches!(got, BtAdapterState::Discovering { .. }));
    }

    #[test]
    fn props_changed_powered_from_present_lands_in_powered() {
        let got = next_state(
            &BtAdapterState::Present,
            AdapterSignal::PropsChanged {
                powered: true,
                discovering: false,
            },
            t0(),
        );
        assert!(matches!(got, BtAdapterState::Powered));
    }

    #[test]
    fn power_off_from_powered_falls_back_to_present() {
        let got = next_state(
            &BtAdapterState::Powered,
            AdapterSignal::PropsChanged {
                powered: false,
                discovering: false,
            },
            t0(),
        );
        assert!(matches!(got, BtAdapterState::Present));
    }

    #[test]
    fn discovering_to_powered_and_back() {
        let mid = next_state(
            &BtAdapterState::Powered,
            AdapterSignal::PropsChanged {
                powered: true,
                discovering: true,
            },
            t0(),
        );
        assert!(matches!(mid, BtAdapterState::Discovering { .. }));
        let back = next_state(
            &mid,
            AdapterSignal::PropsChanged {
                powered: true,
                discovering: false,
            },
            t0(),
        );
        assert!(matches!(back, BtAdapterState::Powered));
    }

    #[test]
    fn discovering_stays_with_original_since_on_reentry() {
        let t1 = Instant::now();
        let start = BtAdapterState::Discovering { since: t1 };
        let got = next_state(
            &start,
            AdapterSignal::PropsChanged {
                powered: true,
                discovering: true,
            },
            Instant::now(),
        );
        match got {
            BtAdapterState::Discovering { since } => assert_eq!(since, t1),
            other => panic!("expected Discovering, got {other:?}"),
        }
    }

    #[test]
    fn removed_wins_from_any_state() {
        for start in [
            BtAdapterState::Unavailable,
            BtAdapterState::Present,
            BtAdapterState::Powered,
            BtAdapterState::Discovering {
                since: Instant::now(),
            },
        ] {
            let got = next_state(&start, AdapterSignal::Removed, t0());
            assert!(matches!(got, BtAdapterState::Gone));
        }
    }

    #[test]
    fn bluez_lost_reverts_to_unavailable() {
        let got = next_state(&BtAdapterState::Powered, AdapterSignal::BluezLost, t0());
        assert!(matches!(got, BtAdapterState::Unavailable));
    }

    #[test]
    fn gone_is_terminal() {
        let got = next_state(
            &BtAdapterState::Gone,
            AdapterSignal::PropsChanged {
                powered: true,
                discovering: true,
            },
            t0(),
        );
        assert!(matches!(got, BtAdapterState::Gone));
    }
}
