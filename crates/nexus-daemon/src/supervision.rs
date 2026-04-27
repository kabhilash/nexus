//! Subsystem supervision — spawn, restart on crash, join on
//! shutdown. See `docs/nexus-architecture.md` §5.
//!
//! Each subsystem is modelled as "a future that runs until the
//! shared shutdown token is cancelled". The supervisor wraps that
//! future in a loop that:
//!
//!   1. Awaits one iteration of the subsystem's `run()` future.
//!   2. On `Err`, logs `ERROR`, emits
//!      [`NexusEvent::OperatorNotification`] with `kind =
//!      "subsystem_crash"`, and — if restart is enabled and the
//!      shutdown token is still live — waits a backoff interval
//!      before the next iteration. Backoff is exponential, capped
//!      by `SupervisionSection::restart_max_backoff`.
//!   3. On `Ok(())`, assumes graceful shutdown and exits the loop.
//!
//! Subsystems that expose their own shutdown token (Wi-Fi,
//! Bluetooth, GNSS, D-Bus — each returns a handle carrying one) are
//! wired by their per-subsystem spawn helper: the daemon cancels
//! the shared token, which the helper bridges into the subsystem's
//! private token.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use nexus_core::{NexusEvent, NotificationData};
use thiserror::Error;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::SupervisionSection;

/// Identifies which subsystem a supervisor is managing — used in log
/// spans and in the `kind` field of the crash notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubsystemName {
    InterfaceMonitor,
    Ethernet,
    Wifi,
    Bluetooth,
    Gnss,
    Dbus,
    Connectivity,
}

impl SubsystemName {
    pub fn as_str(self) -> &'static str {
        match self {
            SubsystemName::InterfaceMonitor => "interface_monitor",
            SubsystemName::Ethernet => "ethernet",
            SubsystemName::Wifi => "wifi",
            SubsystemName::Bluetooth => "bluetooth",
            SubsystemName::Gnss => "gnss",
            SubsystemName::Dbus => "dbus",
            SubsystemName::Connectivity => "connectivity",
        }
    }
}

/// Errors surfaced when wiring a supervisor. The per-iteration
/// subsystem errors are logged and converted to
/// `OperatorNotification`, not returned.
#[derive(Debug, Error)]
pub enum SupervisionError {
    #[error("subsystem body failed to start: {0}")]
    Startup(anyhow::Error),
}

/// Spawn a supervised task. `factory` is called once per attempt;
/// each call returns the subsystem future to run. The future must
/// honour `shutdown` — the supervisor relies on it to stop the
/// inner task when the outer token is cancelled.
///
/// The returned `JoinHandle` resolves when either (a) the factory
/// returned `Ok(())` or (b) `shutdown` was cancelled between
/// attempts. Subsystem errors never propagate out of the
/// supervisor; they are logged + published as
/// `OperatorNotification`.
pub fn spawn_supervised<F, Fut>(
    name: SubsystemName,
    config: SupervisionSection,
    event_tx: broadcast::Sender<NexusEvent>,
    shutdown: CancellationToken,
    factory: F,
) -> JoinHandle<()>
where
    F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let factory = Arc::new(factory);
    tokio::spawn(async move {
        run_supervised_loop(name, config, event_tx, shutdown, factory).await;
    })
}

async fn run_supervised_loop<F, Fut>(
    name: SubsystemName,
    config: SupervisionSection,
    event_tx: broadcast::Sender<NexusEvent>,
    shutdown: CancellationToken,
    factory: Arc<F>,
) where
    F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let mut backoff = config.restart_initial_backoff;
    loop {
        if shutdown.is_cancelled() {
            return;
        }
        info!(subsystem = name.as_str(), "subsystem starting");
        let fut = factory(shutdown.clone());
        match fut.await {
            Ok(()) => {
                info!(subsystem = name.as_str(), "subsystem exited cleanly");
                return;
            }
            Err(err) => {
                error!(subsystem = name.as_str(), error = %err, "subsystem crashed");
                emit_crash_notification(&event_tx, name, &err);

                if !config.restart || shutdown.is_cancelled() {
                    warn!(
                        subsystem = name.as_str(),
                        "not restarting (restart disabled or shutting down)"
                    );
                    return;
                }

                // Sleep before the next attempt — cancel-aware so a
                // `SIGTERM` arriving during backoff still exits fast.
                let sleep = tokio::time::sleep(backoff);
                tokio::pin!(sleep);
                tokio::select! {
                    _ = &mut sleep => {}
                    _ = shutdown.cancelled() => {
                        info!(subsystem = name.as_str(), "shutdown received during backoff");
                        return;
                    }
                }
                backoff = next_backoff(backoff, &config);
            }
        }
    }
}

fn next_backoff(current: Duration, config: &SupervisionSection) -> Duration {
    let bumped = current.mul_f64(config.restart_multiplier.max(1.0));
    bumped.min(config.restart_max_backoff)
}

fn emit_crash_notification(
    event_tx: &broadcast::Sender<NexusEvent>,
    name: SubsystemName,
    err: &anyhow::Error,
) {
    let mut data = NotificationData::new();
    data.insert("subsystem", name.as_str().to_owned());
    data.insert("error", format!("{err:#}"));
    // Publishing is best-effort; if there are no subscribers the
    // event is dropped, which is fine.
    let _ = event_tx.send(NexusEvent::OperatorNotification {
        kind: "subsystem_crash".to_owned(),
        data,
    });
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    fn fast_config() -> SupervisionSection {
        SupervisionSection {
            restart: true,
            restart_initial_backoff: Duration::from_millis(5),
            restart_max_backoff: Duration::from_millis(20),
            restart_multiplier: 2.0,
        }
    }

    #[tokio::test]
    async fn clean_exit_does_not_restart() {
        let (tx, _rx) = broadcast::channel(4);
        let cancel = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let join = spawn_supervised(
            SubsystemName::InterfaceMonitor,
            fast_config(),
            tx,
            cancel.clone(),
            move |_shutdown| {
                calls_c.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            },
        );
        join.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn crash_restarts_until_cancel() {
        let (tx, mut rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let join = spawn_supervised(
            SubsystemName::Ethernet,
            fast_config(),
            tx,
            cancel.clone(),
            move |_shutdown| {
                let n = calls_c.fetch_add(1, Ordering::SeqCst);
                async move {
                    if n < 2 {
                        Err(anyhow::anyhow!("boom {n}"))
                    } else {
                        Ok(())
                    }
                }
            },
        );
        join.await.unwrap();
        assert!(calls.load(Ordering::SeqCst) >= 3);

        // First two crashes produce OperatorNotification events.
        let mut crashes = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, NexusEvent::OperatorNotification { ref kind, .. }
                if kind == "subsystem_crash")
            {
                crashes += 1;
            }
        }
        assert_eq!(crashes, 2);
    }

    #[tokio::test]
    async fn restart_disabled_means_one_attempt_after_crash() {
        let (tx, _rx) = broadcast::channel(4);
        let cancel = CancellationToken::new();
        let mut cfg = fast_config();
        cfg.restart = false;
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_c = Arc::clone(&calls);
        let join = spawn_supervised(
            SubsystemName::Wifi,
            cfg,
            tx,
            cancel.clone(),
            move |_shutdown| {
                calls_c.fetch_add(1, Ordering::SeqCst);
                async { Err(anyhow::anyhow!("always fails")) }
            },
        );
        join.await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancel_propagates_to_subsystem() {
        let (tx, _rx) = broadcast::channel(4);
        let cancel = CancellationToken::new();
        let cancel_c = cancel.clone();
        let join = spawn_supervised(
            SubsystemName::Dbus,
            fast_config(),
            tx,
            cancel.clone(),
            move |shutdown| async move {
                shutdown.cancelled().await;
                Ok(())
            },
        );
        // Give the factory a tick to start.
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancel_c.cancel();

        tokio::time::timeout(Duration::from_secs(1), join)
            .await
            .expect("supervisor must exit within 1s of cancel")
            .unwrap();
    }

    #[test]
    fn backoff_caps_at_max() {
        let cfg = SupervisionSection {
            restart: true,
            restart_initial_backoff: Duration::from_millis(100),
            restart_max_backoff: Duration::from_millis(250),
            restart_multiplier: 10.0,
        };
        let b1 = next_backoff(Duration::from_millis(100), &cfg);
        assert_eq!(b1, Duration::from_millis(250));
    }

    #[test]
    fn subsystem_name_strings() {
        assert_eq!(
            SubsystemName::InterfaceMonitor.as_str(),
            "interface_monitor"
        );
        assert_eq!(SubsystemName::Dbus.as_str(), "dbus");
        assert_eq!(SubsystemName::Connectivity.as_str(), "connectivity");
    }
}
