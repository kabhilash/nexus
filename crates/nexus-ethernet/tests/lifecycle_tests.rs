//! End-to-end tests that drive the Ethernet Backend through its
//! event loop with a [`MockAuthBackend`]. Covers DD-002 §10.1: every
//! state-machine arc, retries, and fail-fast.
//!
//! Real-kernel integration tests (veth + hostapd + FreeRADIUS) live
//! behind the `integration-linux` feature and are `#[ignore]`d so
//! default CI skips them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{
    AuthFailureReason, InterfaceInfo, InterfaceKind, NexusEvent, Nl80211IfType, OperState,
    PhyCapabilities,
};
use nexus_ethernet::{
    AuthBackendKind, EthernetBackend, EthernetConfig, MockAuthBackend, MockScenario, RetryPolicy,
    WiredAuthBackend, default_ethernet_profile,
};
use nexus_profile_store::{
    Dot1xEapConfig, Dot1xSettings, EapMethod, EthernetProfile, InMemoryKeySource, ProfileFileStore,
    ProfileStore,
};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use ulid::Ulid;

/// Build a plausible InterfaceInfo for an Ethernet interface. The
/// Ethernet Backend only inspects kind, ifindex, ifname, and
/// carrier — the rest is filler.
fn ethernet_info(ifindex: u32, ifname: &str, carrier: bool) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: ifname.to_owned(),
        mac: [0xAA; 6],
        mtu: 1500,
        operstate: if carrier {
            OperState::Up
        } else {
            OperState::Down
        },
        carrier,
        kind: InterfaceKind::Ethernet,
        discovered_at: Instant::now(),
    }
}

/// Non-ethernet info used to prove the backend ignores other kinds.
fn wireless_info(ifindex: u32, ifname: &str) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: ifname.to_owned(),
        mac: [0; 6],
        mtu: 1500,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Wireless {
            wiphy: 0,
            wiphy_name: "phy0".into(),
            wdev: 1,
            iftype: Nl80211IfType(2),
            capabilities: Arc::new(PhyCapabilities::default()),
        },
        discovered_at: Instant::now(),
    }
}

struct Harness {
    tx: broadcast::Sender<NexusEvent>,
    rx: broadcast::Receiver<NexusEvent>,
    shutdown: CancellationToken,
    handle: tokio::task::JoinHandle<nexus_ethernet::Result<()>>,
    scenarios: nexus_ethernet::auth::mock::MockScenarios,
    _store_dir: TempDir,
}

impl Harness {
    async fn start(config: EthernetConfig, with_dot1x_profile: Option<(u32, &str)>) -> Self {
        let (tx, rx) = broadcast::channel(256);
        let shutdown = CancellationToken::new();

        let store_dir = TempDir::new().unwrap();
        let source = InMemoryKeySource::new([0x42; 32]);
        let store = Arc::new(ProfileFileStore::open(store_dir.path(), &source).unwrap())
            as Arc<dyn ProfileStore>;

        if let Some((_, ifname)) = with_dot1x_profile {
            let profile = EthernetProfile {
                id: Ulid::new(),
                schema_version: 1,
                metadata: Default::default(),
                interface: nexus_profile_store::EthInterfaceSettings {
                    name: ifname.to_owned(),
                    auto_connect: true,
                },
                dot1x: Some(Dot1xSettings {
                    enabled: true,
                    eap: Dot1xEapConfig {
                        eap: EapMethod::Peap,
                        identity: "user@corp".into(),
                        anonymous_identity: None,
                        ca_cert: None,
                        client_cert: None,
                        client_key: None,
                        client_key_password: None,
                        phase2: Some("auth=MSCHAPV2".into()),
                        domain_suffix_match: None,
                        password: None,
                    },
                }),
            };
            store.put_ethernet(&profile).await.unwrap();
        }

        let mock = MockAuthBackend::new(tx.clone());
        let scenarios = mock.scenarios();
        let auth_backend: Option<Box<dyn WiredAuthBackend>> = Some(Box::new(mock));

        let backend = EthernetBackend::new(tx.clone(), store, auth_backend, config);
        let handle = tokio::spawn(backend.run(shutdown.clone()));

        Self {
            tx,
            rx,
            shutdown,
            handle,
            scenarios,
            _store_dir: store_dir,
        }
    }

    async fn wait_for<F>(&mut self, mut matches: F) -> Option<NexusEvent>
    where
        F: FnMut(&NexusEvent) -> bool,
    {
        loop {
            match timeout(Duration::from_secs(2), self.rx.recv()).await {
                Ok(Ok(event)) => {
                    if matches(&event) {
                        return Some(event);
                    }
                }
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return None,
            }
        }
    }

    async fn shutdown(self) {
        self.shutdown.cancel();
        let _ = self.handle.await;
    }
}

#[tokio::test]
async fn non_ethernet_interface_is_ignored() {
    let mut h = Harness::start(EthernetConfig::default(), None).await;
    h.tx.send(NexusEvent::InterfaceDiscovered(wireless_info(2, "wlan0")))
        .unwrap();
    // Give the loop a moment to ignore it.
    tokio::time::sleep(Duration::from_millis(30)).await;
    // No Eth events emitted.
    while let Ok(e) = h.rx.try_recv() {
        match e {
            NexusEvent::EthLinkReady { .. } | NexusEvent::EthLinkLost { .. } => {
                panic!("unexpected Eth event for Wi-Fi interface");
            }
            _ => {}
        }
    }
    h.shutdown().await;
}

#[tokio::test]
async fn plain_ethernet_goes_waiting_then_link_ready_on_carrier_up() {
    let mut h = Harness::start(EthernetConfig::default(), None).await;

    h.tx.send(NexusEvent::InterfaceDiscovered(ethernet_info(
        2, "eth0", false,
    )))
    .unwrap();
    h.tx.send(NexusEvent::CarrierChanged {
        ifindex: 2,
        up: true,
    })
    .unwrap();

    let ready = h
        .wait_for(|e| matches!(e, NexusEvent::EthLinkReady { ifindex: 2 }))
        .await;
    assert!(ready.is_some(), "expected EthLinkReady for eth0");

    h.tx.send(NexusEvent::CarrierChanged {
        ifindex: 2,
        up: false,
    })
    .unwrap();
    let lost = h
        .wait_for(|e| matches!(e, NexusEvent::EthLinkLost { ifindex: 2 }))
        .await;
    assert!(lost.is_some(), "expected EthLinkLost after carrier drop");

    h.shutdown().await;
}

#[tokio::test]
async fn carrier_up_at_discovery_starts_state_machine() {
    let mut h = Harness::start(EthernetConfig::default(), None).await;
    h.tx.send(NexusEvent::InterfaceDiscovered(ethernet_info(
        3, "eth1", true,
    )))
    .unwrap();
    let ready = h
        .wait_for(|e| matches!(e, NexusEvent::EthLinkReady { ifindex: 3 }))
        .await;
    assert!(ready.is_some());
    h.shutdown().await;
}

#[tokio::test]
async fn dot1x_success_path_emits_link_ready() {
    let mut h = Harness::start(EthernetConfig::default(), Some((5, "eth0"))).await;
    h.scenarios.set(5, MockScenario::ImmediateSuccess);

    h.tx.send(NexusEvent::InterfaceDiscovered(ethernet_info(
        5, "eth0", false,
    )))
    .unwrap();
    h.tx.send(NexusEvent::CarrierChanged {
        ifindex: 5,
        up: true,
    })
    .unwrap();

    let ready = h
        .wait_for(|e| matches!(e, NexusEvent::EthLinkReady { ifindex: 5 }))
        .await;
    assert!(ready.is_some(), "expected EthLinkReady after auth success");

    h.shutdown().await;
}

#[tokio::test]
async fn dot1x_fail_fast_on_bad_credentials_does_not_loop() {
    let config = EthernetConfig::default().with_retry(
        Duration::from_millis(20),
        Duration::from_millis(200),
        2.0,
        0,
    );
    let mut h = Harness::start(config, Some((7, "eth0"))).await;
    h.scenarios.set(
        7,
        MockScenario::ImmediateFailure(AuthFailureReason::BadCredentials),
    );

    h.tx.send(NexusEvent::InterfaceDiscovered(ethernet_info(
        7, "eth0", true,
    )))
    .unwrap();

    // Observe the first auth attempt fail.
    let failed = h
        .wait_for(|e| {
            matches!(
                e,
                NexusEvent::EthAuthStateChanged {
                    ifindex: 7,
                    state: nexus_core::AuthState::Failed {
                        reason: AuthFailureReason::BadCredentials,
                    },
                },
            )
        })
        .await;
    assert!(failed.is_some(), "expected BadCredentials failure");

    // Within a window well past the max backoff (200ms), there must
    // be no further Authenticating event — fail-fast is honored.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut saw_retry = false;
    while let Ok(e) = h.rx.try_recv() {
        if let NexusEvent::EthAuthStateChanged {
            ifindex: 7,
            state: nexus_core::AuthState::Authenticating,
        } = e
        {
            saw_retry = true;
        }
    }
    assert!(!saw_retry, "BadCredentials is fail-fast; must not retry",);

    h.shutdown().await;
}

#[tokio::test]
async fn dot1x_retriable_failure_schedules_another_attempt() {
    let config = EthernetConfig::default().with_retry(
        Duration::from_millis(20),
        Duration::from_millis(40),
        2.0,
        0,
    );
    let mut h = Harness::start(config, Some((9, "eth0"))).await;
    h.scenarios.set(
        9,
        MockScenario::ImmediateFailure(AuthFailureReason::Timeout),
    );

    h.tx.send(NexusEvent::InterfaceDiscovered(ethernet_info(
        9, "eth0", true,
    )))
    .unwrap();

    // Drain until we see at least two Authenticating events — the
    // initial attempt and a retry.
    let mut authenticating_count = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    while authenticating_count < 2 && tokio::time::Instant::now() < deadline {
        match timeout(Duration::from_millis(50), h.rx.recv()).await {
            Ok(Ok(NexusEvent::EthAuthStateChanged {
                ifindex: 9,
                state: nexus_core::AuthState::Authenticating,
            })) => {
                authenticating_count += 1;
            }
            Ok(Ok(_)) | Ok(Err(_)) => continue,
            Err(_) => continue,
        }
    }
    assert!(
        authenticating_count >= 2,
        "expected at least one retry; saw {authenticating_count} Authenticating events",
    );

    h.shutdown().await;
}

#[tokio::test]
async fn carrier_drop_during_authentication_tears_down() {
    let mut h = Harness::start(EthernetConfig::default(), Some((11, "eth0"))).await;
    h.scenarios.set(
        11,
        MockScenario::SuccessAfter {
            delay: Duration::from_secs(10),
        },
    );

    h.tx.send(NexusEvent::InterfaceDiscovered(ethernet_info(
        11, "eth0", true,
    )))
    .unwrap();

    // Wait for Authenticating to appear so we know the backend
    // actually started auth.
    let started = h
        .wait_for(|e| {
            matches!(
                e,
                NexusEvent::EthAuthStateChanged {
                    ifindex: 11,
                    state: nexus_core::AuthState::Authenticating,
                },
            )
        })
        .await;
    assert!(started.is_some());

    // Carrier drops mid-auth.
    h.tx.send(NexusEvent::CarrierChanged {
        ifindex: 11,
        up: false,
    })
    .unwrap();

    // No LinkLost expected (was never Ready); check that a later
    // LinkReady doesn't spuriously arrive from the canceled auth.
    tokio::time::sleep(Duration::from_millis(100)).await;
    while let Ok(e) = h.rx.try_recv() {
        if let NexusEvent::EthLinkReady { ifindex: 11 } = e {
            panic!("LinkReady should not fire after carrier drop");
        }
    }

    h.shutdown().await;
}

#[tokio::test]
async fn interface_removed_drops_state_and_detaches_auth() {
    let mut h = Harness::start(EthernetConfig::default(), Some((13, "eth0"))).await;
    h.scenarios.set(13, MockScenario::ImmediateSuccess);

    h.tx.send(NexusEvent::InterfaceDiscovered(ethernet_info(
        13, "eth0", true,
    )))
    .unwrap();
    let _ = h
        .wait_for(|e| matches!(e, NexusEvent::EthLinkReady { ifindex: 13 }))
        .await;

    h.tx.send(NexusEvent::InterfaceRemoved { ifindex: 13 })
        .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    // After removal, the backend should ignore further events for
    // that ifindex.
    h.tx.send(NexusEvent::CarrierChanged {
        ifindex: 13,
        up: false,
    })
    .unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut spurious_lost = false;
    while let Ok(e) = h.rx.try_recv() {
        if let NexusEvent::EthLinkLost { ifindex: 13 } = e {
            spurious_lost = true;
        }
    }
    assert!(
        !spurious_lost,
        "removed interface should not produce LinkLost on later carrier events",
    );

    h.shutdown().await;
}

#[tokio::test]
async fn missing_profile_defaults_to_no_auth() {
    let mut h = Harness::start(EthernetConfig::default(), None).await;
    // No profile stored for eth7; the backend falls back to the
    // default profile and treats the link as non-802.1X.
    h.tx.send(NexusEvent::InterfaceDiscovered(ethernet_info(
        15, "eth7", true,
    )))
    .unwrap();
    let ready = h
        .wait_for(|e| matches!(e, NexusEvent::EthLinkReady { ifindex: 15 }))
        .await;
    assert!(ready.is_some());
    let profile = default_ethernet_profile("eth7");
    assert_eq!(profile.interface.name, "eth7");
    h.shutdown().await;
}

#[tokio::test]
async fn retry_policy_exposes_backoff_bounds() {
    let p = RetryPolicy {
        initial: Duration::from_millis(10),
        max: Duration::from_millis(40),
        multiplier: 2.0,
        max_attempts: 0,
    };
    assert_eq!(p.backoff_for(0), Duration::from_millis(10));
    assert_eq!(p.backoff_for(1), Duration::from_millis(20));
    assert_eq!(p.backoff_for(2), Duration::from_millis(40));
    assert_eq!(p.backoff_for(5), Duration::from_millis(40));
    let _ = AuthBackendKind::WpaSupplicant.as_str();
}

// ---------------------------------------------------------------------------
// Real-kernel tests (behind feature = "integration-linux"). These
// assume a test harness the user sets up externally — veth pair +
// hostapd + FreeRADIUS per DD-002 §10.2. All `#[ignore]`d.
// ---------------------------------------------------------------------------

#[cfg(feature = "integration-linux")]
#[tokio::test]
#[ignore = "requires CAP_NET_ADMIN, veth pair, hostapd, FreeRADIUS per DD-002 §10.2"]
async fn real_hostapd_eap_peap_round_trip() {
    // Placeholder — real implementation drives:
    //   1. ip link add veth-nxu type veth peer veth-nxa
    //   2. configure hostapd + freeradius against veth-nxa
    //   3. spawn_ethernet_backend with WpaSupplicantWiredBackend
    //   4. synthesize InterfaceDiscovered + CarrierChanged(true) for
    //      the veth-nxu end and wait for EthLinkReady
}

#[cfg(feature = "integration-linux")]
#[tokio::test]
#[ignore = "requires the hostapd fixture from DD-002 §10.2"]
async fn real_hostapd_killed_mid_auth_triggers_retry_loop() {
    // Placeholder — kill hostapd during the EAP exchange, assert
    // the backend enters retry and recovers when hostapd restarts.
}
