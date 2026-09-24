//! Regression coverage for a real bug: `spawn_object_manager_pump`
//! used to read a plain `zbus::MessageStream::from(connection)` with
//! no `org.freedesktop.DBus.AddMatch` rule registered for it. Per
//! D-Bus semantics a broadcast signal (`destination` unset) is only
//! delivered to a connection that has an active match rule for it —
//! without one, `InterfacesAdded` / `InterfacesRemoved` /
//! `PropertiesChanged` from a real BlueZ never reach the process at
//! all, regardless of how long anything polls. That bug survived
//! this crate's entire pre-existing test suite because every other
//! test either calls `on_interfaces_added()` / `on_properties_changed()`
//! directly (bypassing real signal delivery) or drives
//! `MockBluezClient`, which doesn't model D-Bus match-rule semantics
//! at all — a mock method call can't fail to be "delivered".
//!
//! This spins up a *private* session bus (mirroring the
//! `dbus-daemon --session --print-address --nofork` harness already
//! used throughout `nexus-dbus`'s test suite) with two independent
//! connections:
//!
//! - A fake `org.bluez` that serves a minimal
//!   `org.freedesktop.DBus.ObjectManager` (so the pump's initial
//!   `GetManagedObjects` call has something to talk to) and then
//!   emits real broadcast signals via `Connection::emit_signal` —
//!   exactly as the real BlueZ does.
//! - The actual `spawn_object_manager_pump` production code
//!   (`nexus_bluetooth::bluez::object_manager`), connected
//!   independently to the same bus, with no special test-only
//!   wiring — if the match-rule bug were still present, this test
//!   would hang waiting for events that never arrive.
//!
//! `ZbusBluezClient::connect()` itself hardcodes `Connection::system()`,
//! so it can't be pointed at a private bus without a test-only seam
//! in production code; calling `spawn_object_manager_pump` directly
//! (a `pub` function, same as `MockBluezClient` reuses
//! `on_interfaces_added` et al.) tests the exact same code path
//! without adding one.

use std::collections::HashMap;
use std::time::Duration;

use nexus_bluetooth::bluez::object_manager::{
    adapter_props, device_props, spawn_object_manager_pump,
};
use nexus_bluetooth::bluez::proxies::ObjectManagerProxy;
use nexus_core::{MacAddr, NexusEvent};
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

// ---------------------------------------------------------------------------
// Private session-bus harness — same pattern as nexus-dbus's tests.
// ---------------------------------------------------------------------------

struct Bus {
    addr: String,
    _process: tokio::process::Child,
}

impl Bus {
    async fn spawn() -> Self {
        let mut child = Command::new("dbus-daemon")
            .arg("--session")
            .arg("--print-address")
            .arg("--nofork")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn dbus-daemon; is it installed?");
        let stdout = child.stdout.take().expect("dbus-daemon stdout");
        let mut reader = tokio::io::BufReader::new(stdout);
        let mut line = String::new();
        use tokio::io::AsyncBufReadExt;
        reader
            .read_line(&mut line)
            .await
            .expect("dbus-daemon produced no address");
        Bus {
            addr: line.trim().to_owned(),
            _process: child,
        }
    }

    async fn connection(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.addr.as_str())
            .unwrap()
            .build()
            .await
            .expect("connect to private bus")
    }
}

/// Minimal fake `org.freedesktop.DBus.ObjectManager` — just enough
/// for `spawn_object_manager_pump`'s initial `GetManagedObjects`
/// call to succeed. Starts empty; the test emits `InterfacesAdded`
/// separately to simulate BlueZ discovering things live.
struct FakeObjectManager;

type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

#[zbus::interface(name = "org.freedesktop.DBus.ObjectManager")]
impl FakeObjectManager {
    #[zbus(name = "GetManagedObjects")]
    fn get_managed_objects(&self) -> ManagedObjects {
        HashMap::new()
    }
}

/// Wait for a matching event with a real deadline instead of hanging
/// forever if the bug regresses.
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
            Ok(Err(e)) => panic!("event bus error: {e}\nseen: {}", seen.join("\n       ")),
            Err(_) => panic!(
                "timed out waiting for a matching event — this is exactly what the \
                 missing-AddMatch bug looks like: BlueZ broadcasts the signal, but \
                 nothing ever arrives at this connection\nseen: {}",
                seen.join("\n       ")
            ),
        }
    }
}

/// Set up the fake-BlueZ side: claim `org.bluez` on the private bus
/// and serve an (initially empty) ObjectManager at `/`.
async fn spawn_fake_bluez(bus: &Bus) -> zbus::Connection {
    let conn = bus.connection().await;
    conn.object_server()
        .at("/", FakeObjectManager)
        .await
        .expect("register fake ObjectManager");
    conn.request_name("org.bluez")
        .await
        .expect("claim org.bluez on the private bus");
    conn
}

/// Set up the real pump under test, connected independently to the
/// same private bus.
async fn spawn_real_pump(
    bus: &Bus,
) -> (
    broadcast::Receiver<NexusEvent>,
    nexus_bluetooth::bluez::object_manager::PumpHandle,
    CancellationToken,
) {
    let conn = bus.connection().await;
    let om = ObjectManagerProxy::new(&conn)
        .await
        .expect("build ObjectManagerProxy");
    let (event_tx, event_rx) = broadcast::channel::<NexusEvent>(64);
    let cancel = CancellationToken::new();
    let handle = spawn_object_manager_pump(conn, om, event_tx, cancel.clone())
        .await
        .expect("spawn_object_manager_pump");
    (event_rx, handle, cancel)
}

#[tokio::test]
async fn pump_receives_live_interfaces_added_for_adapter() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let fake_bluez = spawn_fake_bluez(&bus).await;
    let (mut event_rx, _handle, cancel) = spawn_real_pump(&bus).await;

    // The real BlueZ emits InterfacesAdded from "/" when a new
    // adapter or device object appears.
    let path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
    let ifaces = adapter_props(true, false);
    fake_bluez
        .emit_signal(
            None::<&str>,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(path, ifaces),
        )
        .await
        .expect("emit fake InterfacesAdded");

    let event = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtAdapterChanged { adapter, .. } if adapter == "/org/bluez/hci0"),
        Duration::from_secs(3),
    )
    .await;
    match event {
        NexusEvent::BtAdapterChanged {
            powered,
            discovering,
            ..
        } => {
            assert!(powered);
            assert!(!discovering);
        }
        other => panic!("expected BtAdapterChanged, got {other:?}"),
    }

    cancel.cancel();
}

#[tokio::test]
async fn pump_receives_live_interfaces_added_for_device() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let fake_bluez = spawn_fake_bluez(&bus).await;
    let (mut event_rx, _handle, cancel) = spawn_real_pump(&bus).await;

    let address = MacAddr([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]);
    let path = OwnedObjectPath::try_from("/org/bluez/hci0/dev_11_22_33_44_55_66").unwrap();
    let ifaces = device_props(address, "public", Some("Live Test Phone"), false, false, &[]);
    fake_bluez
        .emit_signal(
            None::<&str>,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(path, ifaces),
        )
        .await
        .expect("emit fake InterfacesAdded");

    let event = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtDeviceDiscovered(info) if info.address == address),
        Duration::from_secs(3),
    )
    .await;
    match event {
        NexusEvent::BtDeviceDiscovered(info) => {
            assert_eq!(info.name.as_deref(), Some("Live Test Phone"));
        }
        other => panic!("expected BtDeviceDiscovered, got {other:?}"),
    }

    cancel.cancel();
}

#[tokio::test]
async fn pump_receives_live_properties_changed() {
    let bus = Bus::spawn().await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let fake_bluez = spawn_fake_bluez(&bus).await;
    let (mut event_rx, _handle, cancel) = spawn_real_pump(&bus).await;

    // Discover the adapter first so there's something to update.
    let path = OwnedObjectPath::try_from("/org/bluez/hci0").unwrap();
    fake_bluez
        .emit_signal(
            None::<&str>,
            "/",
            "org.freedesktop.DBus.ObjectManager",
            "InterfacesAdded",
            &(path, adapter_props(false, false)),
        )
        .await
        .unwrap();
    await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtAdapterChanged { powered: false, .. }),
        Duration::from_secs(3),
    )
    .await;

    // Now flip Powered via a real PropertiesChanged broadcast, the
    // same signal shape BlueZ uses when an adapter property changes
    // out from under a StartDiscovery/set_powered call.
    let mut changed: HashMap<String, OwnedValue> = HashMap::new();
    changed.insert(
        "Powered".to_owned(),
        OwnedValue::try_from(zbus::zvariant::Value::new(true)).unwrap(),
    );
    let invalidated: Vec<String> = Vec::new();
    fake_bluez
        .emit_signal(
            None::<&str>,
            "/org/bluez/hci0",
            "org.freedesktop.DBus.Properties",
            "PropertiesChanged",
            &("org.bluez.Adapter1", changed, invalidated),
        )
        .await
        .expect("emit fake PropertiesChanged");

    let event = await_event(
        &mut event_rx,
        |e| matches!(e, NexusEvent::BtAdapterChanged { powered: true, .. }),
        Duration::from_secs(3),
    )
    .await;
    assert!(matches!(event, NexusEvent::BtAdapterChanged { powered: true, .. }));

    cancel.cancel();
}
