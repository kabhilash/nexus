//! The shared `NexusEvent` broadcast channel — the daemon's one
//! event bus. See `docs/nexus-architecture.md` §6.
//!
//! Every subsystem gets a clone of the `broadcast::Sender` so it
//! can publish events; downstream consumers (the D-Bus service
//! layer, metrics exporters, tests) subscribe via
//! [`nexus_core::NexusEvent`] receivers. Slow consumers that miss
//! messages see a `broadcast::error::RecvError::Lagged` — per
//! architecture doc §6, producers must never block.
//!
//! Channel capacity comes from the daemon config, validated
//! non-zero in [`crate::config::Config::validate`].

use nexus_core::NexusEvent;
use tokio::sync::broadcast;

/// Construct the bus. Returned `Sender` is cheap to clone; one
/// clone per subsystem is the usual pattern.
///
/// Capacity must be > 0 (caller has already validated this via
/// [`crate::config::Config::validate`]).
pub fn spawn_bus(
    capacity: usize,
) -> (
    broadcast::Sender<NexusEvent>,
    broadcast::Receiver<NexusEvent>,
) {
    broadcast::channel(capacity)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_fan_out_to_all_subscribers() {
        let (tx, _rx0) = spawn_bus(16);
        let mut rx1 = tx.subscribe();
        let mut rx2 = tx.subscribe();

        tx.send(NexusEvent::InterfaceRemoved { ifindex: 42 })
            .unwrap();

        match rx1.recv().await.unwrap() {
            NexusEvent::InterfaceRemoved { ifindex } => assert_eq!(ifindex, 42),
            other => panic!("unexpected event on rx1: {other:?}"),
        }
        match rx2.recv().await.unwrap() {
            NexusEvent::InterfaceRemoved { ifindex } => assert_eq!(ifindex, 42),
            other => panic!("unexpected event on rx2: {other:?}"),
        }
    }

    #[tokio::test]
    async fn slow_consumer_sees_lagged_not_producer_blocked() {
        let (tx, _rx0) = spawn_bus(2);
        let mut slow = tx.subscribe();

        // Overfill the buffer.
        for i in 0..10 {
            tx.send(NexusEvent::InterfaceRemoved { ifindex: i })
                .unwrap();
        }

        let err = slow.recv().await.unwrap_err();
        matches!(err, broadcast::error::RecvError::Lagged(_));
    }
}
