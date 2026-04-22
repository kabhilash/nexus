//! Real-kernel integration tests. Each test creates a dummy
//! interface via `ip link add` and watches for the corresponding
//! NexusEvent through the broadcast bus.
//!
//! These tests require `CAP_NET_ADMIN` to run `ip link`, and they
//! run against the host's network namespace — so they are all
//! `#[ignore]`d by default. Enable with
//!
//!     cargo test -p nexus-interface-monitor \
//!         --features integration-linux -- --ignored --test-threads=1
//!
//! They are additionally gated behind the `integration-linux`
//! feature so a plain `cargo test --all-features` only compiles
//! them on Linux hosts where `ip` is typically available.

#![cfg(feature = "integration-linux")]

use std::process::Command;
use std::time::Duration;

use nexus_core::NexusEvent;
use nexus_interface_monitor::spawn_interface_monitor;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

const TEST_IFACE: &str = "nxtest0";
const WAIT: Duration = Duration::from_secs(5);

fn ip(args: &[&str]) -> bool {
    Command::new("ip")
        .args(args)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn recv_until<F>(rx: &mut broadcast::Receiver<NexusEvent>, mut pred: F) -> Option<NexusEvent>
where
    F: FnMut(&NexusEvent) -> bool,
{
    loop {
        match timeout(WAIT, rx.recv()).await {
            Ok(Ok(event)) => {
                if pred(&event) {
                    return Some(event);
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return None,
        }
    }
}

#[tokio::test]
#[ignore = "requires CAP_NET_ADMIN and host network namespace"]
async fn dummy_interface_add_emits_discovered_and_remove_emits_removed() {
    // Clean up any leftover from a prior failed run.
    let _ = ip(&["link", "del", TEST_IFACE]);

    let (tx, mut rx) = broadcast::channel(64);
    let shutdown = CancellationToken::new();
    let handle = spawn_interface_monitor(tx, shutdown.clone())
        .await
        .expect("spawn monitor");

    assert!(
        ip(&["link", "add", TEST_IFACE, "type", "dummy"]),
        "failed to create dummy interface",
    );

    let discovered = recv_until(&mut rx, |event| {
        matches!(
            event,
            NexusEvent::InterfaceDiscovered(info) if info.ifname == TEST_IFACE,
        )
    })
    .await
    .expect("did not receive InterfaceDiscovered for nxtest0");
    match discovered {
        NexusEvent::InterfaceDiscovered(info) => {
            assert_eq!(info.ifname, TEST_IFACE);
        }
        _ => unreachable!(),
    }

    assert!(
        ip(&["link", "del", TEST_IFACE]),
        "failed to remove dummy interface",
    );

    let removed = recv_until(&mut rx, |event| {
        matches!(event, NexusEvent::InterfaceRemoved { .. })
    })
    .await
    .expect("did not receive InterfaceRemoved");
    assert!(matches!(removed, NexusEvent::InterfaceRemoved { .. }));

    shutdown.cancel();
    let _ = handle.await;
}

#[tokio::test]
#[ignore = "requires CAP_NET_ADMIN and host network namespace"]
async fn dummy_interface_set_up_emits_operstate_and_carrier_changes() {
    let _ = ip(&["link", "del", TEST_IFACE]);

    let (tx, mut rx) = broadcast::channel(128);
    let shutdown = CancellationToken::new();
    let handle = spawn_interface_monitor(tx, shutdown.clone())
        .await
        .expect("spawn monitor");

    assert!(ip(&["link", "add", TEST_IFACE, "type", "dummy"]));
    // Wait for Discovered so the registry knows about the interface
    // before we flip its state.
    recv_until(
        &mut rx,
        |e| matches!(e, NexusEvent::InterfaceDiscovered(info) if info.ifname == TEST_IFACE),
    )
    .await
    .expect("discovered event");

    assert!(ip(&["link", "set", TEST_IFACE, "up"]));
    let saw_state_change = recv_until(&mut rx, |e| {
        matches!(
            e,
            NexusEvent::OperstateChanged { .. } | NexusEvent::CarrierChanged { .. }
        )
    })
    .await;
    assert!(
        saw_state_change.is_some(),
        "expected Operstate/Carrier change after `ip link set up`",
    );

    let _ = ip(&["link", "del", TEST_IFACE]);
    shutdown.cancel();
    let _ = handle.await;
}
