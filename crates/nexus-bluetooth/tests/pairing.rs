//! Pairing end-to-end tests. See DD-004 §§8, 14.1.
//!
//! The mock is wired with a `pair_hook` that simulates BlueZ's
//! Pair() method: it calls the installed [`Agent`] like BlueZ
//! would, awaiting the operator's [`AnswerPairingPrompt`] via the
//! command channel.

use std::sync::Arc;
use std::time::Duration;

use nexus_bluetooth::bluez::BluezClient;
use nexus_bluetooth::bluez::mock::MockCall;
use nexus_bluetooth::{
    Agent, BluetoothConfig, BtCommand, MockBluezClient, PairingAnswer, PowerState,
    spawn_bluetooth_backend,
};
use nexus_core::{
    BluetoothAddrExt, InterfaceInfo, InterfaceKind, MacAddr, NexusEvent, OperState, PairingJobId,
    PairingPromptKind,
};
use nexus_profile_store::{InMemoryKeySource, ProfileFileStore, ProfileStore};
use tempfile::TempDir;
use tokio::sync::{broadcast, oneshot};

fn bt_interface(ifindex: u32, hci: &str, bluez_path: &str) -> InterfaceInfo {
    InterfaceInfo {
        ifindex,
        ifname: hci.to_owned(),
        mac: [0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13],
        mtu: 0,
        operstate: OperState::Up,
        carrier: true,
        kind: InterfaceKind::Bluetooth {
            hci_name: hci.to_owned(),
            hci_index: ifindex,
            bt_address: MacAddr([0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13]),
            bluez_path: bluez_path.to_owned(),
        },
        discovered_at: std::time::Instant::now(),
    }
}

async fn start_store() -> (TempDir, Arc<dyn ProfileStore>) {
    let tmp = TempDir::new().unwrap();
    let keys = InMemoryKeySource::new([0x55u8; 32]);
    let store = ProfileFileStore::open(tmp.path(), &keys).unwrap();
    (tmp, Arc::new(store))
}

async fn await_event<F: Fn(&NexusEvent) -> bool>(
    rx: &mut broadcast::Receiver<NexusEvent>,
    pred: F,
    timeout: Duration,
) -> NexusEvent {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(event)) => {
                if pred(&event) {
                    return event;
                }
                seen.push(format!("{event:?}"));
            }
            Ok(Err(_)) | Err(_) => panic!("timed out\nseen: {}", seen.join("\n       ")),
        }
    }
}

/// Install a default pair_hook that simulates BlueZ invoking
/// RequestConfirmation on a given Agent. The hook:
///   1. Calls `agent.request_confirmation(device_path, passkey)`,
///      which in turn consults the backend (LookupPairingJob,
///      RegisterPromptOneshot, await with timeout).
///   2. Returns the agent's result to the mock, which propagates
///      it as the Pair() return value.
fn install_confirmation_hook(mock: &MockBluezClient, agent: Agent, passkey: u32) {
    mock.on_pair(move |path| {
        let agent = agent.clone();
        async move { agent.request_confirmation(&path, passkey).await }
    });
}

/// Happy path: operator Pair → Agent RequestConfirmation → operator
/// Accept → BlueZ Pair() returns → BtPairingComplete(success) →
/// profile stored → set_trusted on BlueZ.
#[tokio::test]
async fn pair_numeric_comparison_success_stores_profile_and_trusts() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(128);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle = spawn_bluetooth_backend(
        client,
        store.clone(),
        event_tx.clone(),
        BluetoothConfig::default(),
    );

    // Wire the agent into the mock pair hook.
    let agent = Agent::new(handle.cmd_tx.clone(), 5);
    install_confirmation_hook(&mock, agent, 123_456);

    // Adapter & device discovered.
    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            1,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    let addr = MacAddr([0xAA, 0xBB, 0xCC, 0x01, 0x02, 0x03]);
    mock.publish_device("/org/bluez/hci0", addr, false).await;
    let device_path = format!("/org/bluez/hci0/{}", addr.to_object_path_component());
    // Let the InterfaceDiscovered event land before we issue Pair.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Operator Pair.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Pair {
            device_path: device_path.clone(),
            responder: tx,
        })
        .await
        .unwrap();
    let job_id = rx.await.unwrap().unwrap();

    // Expect BtPairingStarted and BtPairingPrompt.
    let _ = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingStarted { .. }),
        Duration::from_secs(2),
    )
    .await;
    let prompt_event = await_event(
        &mut event_rx,
        |e| {
            matches!(
                e,
                NexusEvent::BtPairingPrompt {
                    kind: PairingPromptKind::RequestConfirmation,
                    ..
                }
            )
        },
        Duration::from_secs(2),
    )
    .await;
    let prompt_job_id = match prompt_event {
        NexusEvent::BtPairingPrompt { job_id, .. } => job_id,
        _ => unreachable!(),
    };
    assert_eq!(prompt_job_id, job_id);

    // Operator answers Accept(true).
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::AnswerPairingPrompt {
            job_id,
            answer: PairingAnswer::Accept(true),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    // Expect BtPairingComplete { success: true }.
    let complete = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingComplete { success: true, .. }),
        Duration::from_secs(2),
    )
    .await;
    if let NexusEvent::BtPairingComplete { job_id: j, .. } = complete {
        assert_eq!(j, job_id);
    }

    // Give the backend a moment to persist + call set_trusted.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let calls = mock.state().calls;
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, MockCall::SetTrusted(p, true) if p == &device_path)),
        "expected SetTrusted(true) call; got {calls:?}"
    );
    // Profile persisted.
    let profile = store
        .load_bluetooth_profile_by_address(&addr)
        .await
        .unwrap();
    let profile = profile.expect("bluetooth profile should be stored after pair");
    assert_eq!(profile.device_address, addr);
    assert!(profile.auto_connect);

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// Operator rejects the pairing — state lands in Failed, no profile
/// is stored, no Trusted set.
#[tokio::test]
async fn pair_rejected_by_operator_lands_in_failed_no_profile() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(128);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle = spawn_bluetooth_backend(
        client,
        store.clone(),
        event_tx.clone(),
        BluetoothConfig::default(),
    );
    let agent = Agent::new(handle.cmd_tx.clone(), 5);
    install_confirmation_hook(&mock, agent, 42);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            2,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    let addr = MacAddr([0xBB; 6]);
    mock.publish_device("/org/bluez/hci0", addr, false).await;
    let device_path = format!("/org/bluez/hci0/{}", addr.to_object_path_component());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Pair {
            device_path: device_path.clone(),
            responder: tx,
        })
        .await
        .unwrap();
    let job_id = rx.await.unwrap().unwrap();
    let _ = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingPrompt { .. }),
        Duration::from_secs(2),
    )
    .await;

    // Operator rejects.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::AnswerPairingPrompt {
            job_id,
            answer: PairingAnswer::Accept(false),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    // Expect BtPairingComplete { success: false }.
    let complete = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingComplete { success: false, .. }),
        Duration::from_secs(2),
    )
    .await;
    if let NexusEvent::BtPairingComplete { job_id: j, .. } = complete {
        assert_eq!(j, job_id);
    }
    // No SetTrusted was called.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let calls = mock.state().calls;
    assert!(
        !calls
            .iter()
            .any(|c| matches!(c, MockCall::SetTrusted(_, true))),
        "SetTrusted should NOT fire on rejected pair; got {calls:?}"
    );
    // No profile.
    let profile = store
        .load_bluetooth_profile_by_address(&addr)
        .await
        .unwrap();
    assert!(profile.is_none(), "no profile expected, got {profile:?}");

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// No AnswerPairingPrompt arrives within agent_response_timeout_s
/// — Agent method returns an error, backend classifies as
/// PairingTimeout, device state moves to Failed.
#[tokio::test]
async fn pair_operator_timeout_lands_in_failed() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(128);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());
    // Agent response timeout < pairing_timeout_s so the error we
    // see is the agent's "operator response timed out" string.
    let agent = Agent::new(handle.cmd_tx.clone(), 1);
    install_confirmation_hook(&mock, agent, 111_111);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            3,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    let addr = MacAddr([0xCC; 6]);
    mock.publish_device("/org/bluez/hci0", addr, false).await;
    let device_path = format!("/org/bluez/hci0/{}", addr.to_object_path_component());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Pair {
            device_path,
            responder: tx,
        })
        .await
        .unwrap();
    let _ = rx.await.unwrap().unwrap();

    let complete = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingComplete { success: false, .. }),
        Duration::from_secs(5),
    )
    .await;
    match complete {
        NexusEvent::BtPairingComplete {
            reason: Some(nexus_core::BtFailureReason::PairingTimeout),
            ..
        } => {}
        other => panic!("expected PairingTimeout; got {other:?}"),
    }

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// Incoming RequestAuthorization with auto_accept_incoming profile
/// flag resolves with Accept without firing a prompt.
#[tokio::test]
async fn incoming_authorization_auto_accepts_with_profile() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;

    // Pre-seed a profile with auto_accept_incoming = true.
    use nexus_profile_store::BluetoothProfile;
    let addr = MacAddr([0x10, 0x20, 0x30, 0x40, 0x50, 0x60]);
    let profile = BluetoothProfile {
        id: ulid::Ulid::new(),
        schema_version: 1,
        metadata: Default::default(),
        adapter_path: "/org/bluez/hci0".into(),
        device_address: addr,
        device_name: Some("trusted-kb".into()),
        auto_connect: true,
        auto_accept_incoming: true,
        preferences: Default::default(),
    };
    store.put_bluetooth(&profile).await.unwrap();

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());
    let agent = Agent::new(handle.cmd_tx.clone(), 5);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            4,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    mock.publish_device_full("/org/bluez/hci0", addr, true, false, &[])
        .await;
    let device_path = format!("/org/bluez/hci0/{}", addr.to_object_path_component());
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Simulate BlueZ invoking RequestAuthorization. The agent
    // consults LookupAuthorizationPolicy, gets Accept, returns Ok
    // without emitting a prompt.
    let result = agent.request_authorization(&device_path).await;
    assert!(result.is_ok(), "auto-accept should short-circuit");

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// Incoming RequestAuthorization without a profile falls through to
/// the prompt path.
#[tokio::test]
async fn incoming_authorization_prompts_without_profile() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());
    let agent = Agent::new(handle.cmd_tx.clone(), 2);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            5,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    let addr = MacAddr([0xEE; 6]);
    mock.publish_device_full("/org/bluez/hci0", addr, false, false, &[])
        .await;
    let device_path = format!("/org/bluez/hci0/{}", addr.to_object_path_component());
    tokio::time::sleep(Duration::from_millis(80)).await;

    // Drive RequestAuthorization in the background while we
    // answer the prompt from the test side.
    let agent_clone = agent.clone();
    let device_path_owned = device_path.clone();
    let jh =
        tokio::spawn(async move { agent_clone.request_authorization(&device_path_owned).await });

    // The backend should emit a BtPairingPrompt with a synthesized
    // job id.
    let event = await_event(
        &mut event_rx,
        |e| {
            matches!(
                e,
                NexusEvent::BtPairingPrompt {
                    kind: PairingPromptKind::RequestAuthorization,
                    ..
                }
            )
        },
        Duration::from_secs(2),
    )
    .await;
    let job_id = match event {
        NexusEvent::BtPairingPrompt { job_id, .. } => job_id,
        _ => unreachable!(),
    };

    // Operator approves.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::AnswerPairingPrompt {
            job_id,
            answer: PairingAnswer::Accept(true),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();
    let result = jh.await.unwrap();
    assert!(result.is_ok());

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// BlueZ restart mid-pair: BluezDisconnected arrives while Pair() is
/// outstanding → pending prompt is cancelled, device state cleared
/// when BlueZ is back.
#[tokio::test]
async fn bluez_restart_mid_pair_cleans_up_pending_prompt() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(128);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());
    let agent = Agent::new(handle.cmd_tx.clone(), 5);
    install_confirmation_hook(&mock, agent, 99_999);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            6,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    let addr = MacAddr([0xFE; 6]);
    mock.publish_device("/org/bluez/hci0", addr, false).await;
    let device_path = format!("/org/bluez/hci0/{}", addr.to_object_path_component());
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Pair {
            device_path,
            responder: tx,
        })
        .await
        .unwrap();
    let _job_id = rx.await.unwrap().unwrap();
    let _ = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingPrompt { .. }),
        Duration::from_secs(2),
    )
    .await;

    // Simulate BlueZ going down. The backend clears adapter devices
    // and cancels any pending prompt — the agent's await resolves
    // with Cancel, BlueZ's Pair() call (still outstanding in the
    // mock) completes, and BtPairingComplete(success=false) fires.
    event_tx.send(NexusEvent::BluezDisconnected).unwrap();
    let complete = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingComplete { success: false, .. }),
        Duration::from_secs(3),
    )
    .await;
    match complete {
        NexusEvent::BtPairingComplete { success: false, .. } => {}
        other => panic!("expected failure, got {other:?}"),
    }

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// Auto-connect: a Paired device discovered with a stored
/// `auto_connect = true` profile triggers an internal Connect
/// command.
#[tokio::test]
async fn auto_connect_fires_on_paired_device_with_profile() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    use nexus_profile_store::BluetoothProfile;
    let addr = MacAddr([0xAB, 0xCD, 0xEF, 0x11, 0x22, 0x33]);
    let profile = BluetoothProfile {
        id: ulid::Ulid::new(),
        schema_version: 1,
        metadata: Default::default(),
        adapter_path: "/org/bluez/hci0".into(),
        device_address: addr,
        device_name: Some("saved-kb".into()),
        auto_connect: true,
        auto_accept_incoming: false,
        preferences: Default::default(),
    };
    store.put_bluetooth(&profile).await.unwrap();

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            7,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    // Paired device appearing → auto-connect should fire.
    mock.publish_device_full("/org/bluez/hci0", addr, true, false, &[])
        .await;

    // Expect BtDeviceConnected from the auto-connect path.
    await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtDeviceConnected { address, .. } if *address == addr),
        Duration::from_secs(2),
    )
    .await;

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// Power state transitions: Sleep powers every adapter off; Active
/// powers them back on.
#[tokio::test]
async fn power_state_sleep_powers_off_active_powers_on() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle =
        spawn_bluetooth_backend(client, store, event_tx.clone(), BluetoothConfig::default());

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            8,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::SetPowerState {
            state: PowerState::Sleep,
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::SetPowerState {
            state: PowerState::Active,
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();

    let calls = mock.state().calls;
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, MockCall::SetPowered(_, false))),
        "expected power-off on Sleep; got {calls:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, MockCall::SetPowered(_, true))),
        "expected power-on on Active; got {calls:?}"
    );

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

/// Forget of a paired device removes the stored profile.
#[tokio::test]
async fn forget_paired_device_removes_profile() {
    let (event_tx, _rx0) = broadcast::channel::<NexusEvent>(64);
    let mut event_rx = event_tx.subscribe();
    let (_tmp, store) = start_store().await;

    let mock = Arc::new(MockBluezClient::new(event_tx.clone()));
    let client: Arc<dyn BluezClient> = mock.clone();
    mock.connect().await.unwrap();
    let handle = spawn_bluetooth_backend(
        client,
        store.clone(),
        event_tx.clone(),
        BluetoothConfig::default(),
    );
    let agent = Agent::new(handle.cmd_tx.clone(), 5);
    install_confirmation_hook(&mock, agent, 1);

    event_tx
        .send(NexusEvent::InterfaceDiscovered(bt_interface(
            9,
            "hci0",
            "/org/bluez/hci0",
        )))
        .unwrap();
    mock.publish_adapter("/org/bluez/hci0", true, false).await;
    let addr = MacAddr([0x42; 6]);
    mock.publish_device("/org/bluez/hci0", addr, false).await;
    let device_path = format!("/org/bluez/hci0/{}", addr.to_object_path_component());
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Pair.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Pair {
            device_path: device_path.clone(),
            responder: tx,
        })
        .await
        .unwrap();
    let job_id = rx.await.unwrap().unwrap();
    let _ = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingPrompt { .. }),
        Duration::from_secs(2),
    )
    .await;
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::AnswerPairingPrompt {
            job_id,
            answer: PairingAnswer::Accept(true),
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();
    let _ = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtPairingComplete { success: true, .. }),
        Duration::from_secs(2),
    )
    .await;

    // Profile exists now.
    assert!(
        store
            .load_bluetooth_profile_by_address(&addr)
            .await
            .unwrap()
            .is_some()
    );

    // Forget.
    let (tx, rx) = oneshot::channel();
    handle
        .cmd_tx
        .send(BtCommand::Forget {
            adapter: "/org/bluez/hci0".into(),
            device_path,
            responder: tx,
        })
        .await
        .unwrap();
    rx.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;

    assert!(
        store
            .load_bluetooth_profile_by_address(&addr)
            .await
            .unwrap()
            .is_none(),
        "profile should be gone after Forget"
    );

    handle.shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle.join).await;
}

// Silence unused import of PairingJobId; it's referenced through
// pattern matches above under compile expansion.
#[allow(dead_code)]
fn _touch(_j: PairingJobId) {}
