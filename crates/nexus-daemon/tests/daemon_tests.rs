//! Daemon integration tests. See `docs/nexus-architecture.md` §5.
//!
//! - `minimal_fixture_parses`: the shipped `tests/fixtures/minimal.toml`
//!   parses without error and reflects its declared values.
//! - `disabled_subsystem_is_skipped`: setting `enabled = false` on
//!   any subsystem removes it from `enabled_subsystems()`. The
//!   daemon loop uses this list to decide what to spawn, so this
//!   also proves no events flow from a disabled subsystem.
//! - `sigterm_shuts_down_within_5s`: spawns the `nexusd` binary
//!   against `minimal.toml`, sends `SIGTERM`, and asserts the
//!   process exits in under 5 seconds.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nexus_daemon::{Config, SubsystemName};

#[test]
fn help_flag_prints_usage_and_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_nexusd");
    let out = Command::new(bin)
        .arg("--help")
        .output()
        .expect("spawn nexusd --help");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Key topics the operator expects to see.
    assert!(stdout.contains("Usage:"), "stdout: {stdout}");
    assert!(stdout.contains("--config"), "stdout: {stdout}");
    assert!(stdout.contains("RUST_LOG"), "stdout: {stdout}");
    assert!(stdout.contains("SIGTERM"), "stdout: {stdout}");
    // Help goes to stdout, not stderr (Unix convention).
    assert!(out.stderr.is_empty(), "stderr: {:?}", out.stderr);
}

#[test]
fn version_flag_prints_version_and_exits_zero() {
    let bin = env!("CARGO_BIN_EXE_nexusd");
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .expect("spawn nexusd --version");
    assert!(out.status.success(), "exit: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // "nexusd x.y.z\n" — must start with the binary name and
    // include the cargo package version.
    assert!(stdout.starts_with("nexusd "), "stdout: {stdout}");
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "stdout: {stdout}"
    );
}

#[test]
fn config_load_error_surfaces_on_stderr_not_silent() {
    // Regression: an error from `Config::load_from_path` used to be
    // routed only through `tracing::error!`, which fires before
    // `init_tracing` installs a subscriber — the operator saw exit
    // 1 with zero output and no way to tell what went wrong. The
    // fix emits the error on stderr unconditionally.
    let bin = env!("CARGO_BIN_EXE_nexusd");
    let tmp = tempfile::tempdir().unwrap();
    let cfg_path = tmp.path().join("nexus.toml");
    // Missing leading `b` — mirrors the real-world typo that first
    // surfaced this bug.
    std::fs::write(&cfg_path, "us_capacity = 512\n").unwrap();

    let out = Command::new(bin)
        .arg("--config")
        .arg(&cfg_path)
        .output()
        .expect("spawn nexusd");
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1, got {:?}",
        out.status
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nexusd:"), "stderr: {stderr}");
    // Some slice of the config path must appear so the operator
    // knows which file failed.
    assert!(stderr.contains("nexus.toml"), "stderr: {stderr}");
}

#[test]
fn fatal_preflight_finding_aborts_with_action_item() {
    // Point profile_store.root at a plain file — preflight classifies
    // that as fatal and prints an action item. nexusd must exit 1
    // before any subsystem spawns (we never bind a bus name, so no
    // cleanup is needed).
    let bin = env!("CARGO_BIN_EXE_nexusd");
    let tmp = tempfile::tempdir().unwrap();
    let blocker = tmp.path().join("not-a-dir");
    std::fs::write(&blocker, b"junk").unwrap();
    let cfg_path = tmp.path().join("nexus.toml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"
bus_capacity = 32
log_level = "error"

[supervision]
restart = false

[interface_monitor]
enabled = false

[profile_store]
root = "{}"
key_source = "in_memory"
in_memory_seed = "3333333333333333333333333333333333333333333333333333333333333333"

[dbus]
enabled = false
bus_name = "fi.nexus1.test.preflight"
use_session_bus = true
allow_all_authz = true

[ethernet]
enabled = false

[wifi]
enabled = false

[bluetooth]
enabled = false

[gnss]
enabled = false
"#,
            blocker.display(),
        ),
    )
    .unwrap();

    let out = Command::new(bin)
        .arg("--config")
        .arg(&cfg_path)
        .output()
        .expect("spawn nexusd");
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1, got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    // The finding header and action-item line both land on stderr.
    assert!(stderr.contains("[fatal] profile_store"), "stderr: {stderr}");
    assert!(stderr.contains("-> "), "stderr: {stderr}");
}

#[test]
fn unknown_flag_exits_with_usage_code() {
    let bin = env!("CARGO_BIN_EXE_nexusd");
    let out = Command::new(bin)
        .arg("--totally-made-up")
        .output()
        .expect("spawn nexusd with bad flag");
    assert_eq!(
        out.status.code(),
        Some(2),
        "expected exit 2 (usage), got {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nexusd:"), "stderr: {stderr}");
    // Error hint points operators at --help.
    assert!(stderr.contains("--help"), "stderr: {stderr}");
}

fn fixture(name: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests");
    p.push("fixtures");
    p.push(name);
    p
}

#[test]
fn minimal_fixture_parses() {
    let cfg = Config::load_from_path(fixture("minimal.toml")).expect("minimal.toml should parse");
    assert_eq!(cfg.bus_capacity, 128);
    assert_eq!(cfg.log_level, "info");
    assert!(cfg.interface_monitor.enabled);
    assert!(cfg.ethernet.enabled);
    assert!(!cfg.wifi.enabled);
    assert!(!cfg.bluetooth.enabled);
    assert!(!cfg.gnss.enabled);
    assert!(cfg.dbus.enabled);
    assert!(cfg.dbus.use_session_bus);
    assert!(cfg.dbus.allow_all_authz);
    assert_eq!(cfg.profile_store.key_source, "in_memory");
}

#[test]
fn disabled_subsystem_is_skipped() {
    // Start from all-enabled defaults, turn Wi-Fi off, verify it
    // drops off the supervisor plan.
    let mut cfg = Config::default();
    cfg.gnss.enabled = true; // default is off — force on to round-trip below
    let full = cfg.enabled_subsystems();
    assert!(full.contains(&SubsystemName::Wifi));
    assert!(full.contains(&SubsystemName::Gnss));

    cfg.wifi.enabled = false;
    cfg.gnss.enabled = false;
    let trimmed = cfg.enabled_subsystems();
    assert!(!trimmed.contains(&SubsystemName::Wifi));
    assert!(!trimmed.contains(&SubsystemName::Gnss));
    assert!(trimmed.contains(&SubsystemName::InterfaceMonitor));
    assert!(trimmed.contains(&SubsystemName::Dbus));
}

#[test]
fn sigterm_shuts_down_within_5s() {
    // Spawn the nexusd binary against a disposable copy of the
    // minimal fixture with paths rewritten to test-private dirs.
    let bin = env!("CARGO_BIN_EXE_nexusd");
    let tmp = tempfile::tempdir().unwrap();
    let bus_addr = format!("unix:path={}", tmp.path().join("bus").display());

    // Spawn our own session-style bus so the daemon does not fight
    // the global one or fail on CI where no bus is available.
    let dbus_socket = tmp.path().join("bus");
    let mut bus = match Command::new("dbus-daemon")
        .arg("--session")
        .arg("--nofork")
        .arg(format!("--address=unix:path={}", dbus_socket.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("dbus-daemon unavailable ({e}); skipping sigterm test");
            return;
        }
    };
    // Poll the socket for up to 2 s.
    let socket_ready = Instant::now();
    while !dbus_socket.exists() {
        if socket_ready.elapsed() > Duration::from_secs(2) {
            let _ = bus.kill();
            eprintln!("bus never came up; skipping sigterm test");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let cfg_path = tmp.path().join("nexus.toml");
    let profile_root = tmp.path().join("profiles");
    std::fs::create_dir_all(&profile_root).unwrap();
    std::fs::write(
        &cfg_path,
        format!(
            r#"
bus_capacity = 64
log_level = "warn"

[supervision]
restart = false

[interface_monitor]
enabled = true

[profile_store]
root = "{}"
key_source = "in_memory"
in_memory_seed = "1111111111111111111111111111111111111111111111111111111111111111"

[dbus]
enabled = true
bus_name = "fi.nexus1.test.shutdown"
use_session_bus = false
address = "{}"
allow_all_authz = true

[ethernet]
enabled = false

[wifi]
enabled = false

[bluetooth]
enabled = false

[gnss]
enabled = false
"#,
            profile_root.display(),
            bus_addr,
        ),
    )
    .unwrap();

    let mut child = Command::new(bin)
        .arg("--config")
        .arg(&cfg_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nexusd");

    // Give it a moment to come up.
    std::thread::sleep(Duration::from_millis(300));

    // Send SIGTERM via libc.
    let pid = child.id() as i32;
    // SAFETY: `pid` is a PID we just spawned and still own; `kill`
    // is async-signal-safe and returns a value we check.
    let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
    assert_eq!(rc, 0, "kill(SIGTERM) returned non-zero");

    // Wait up to 5 s for graceful exit.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait().unwrap() {
            Some(status) => {
                // Exit should be zero; SIGTERM handler converts the
                // signal to a normal ExitCode::SUCCESS.
                assert!(status.success(), "non-zero exit: {status:?}");
                let _ = bus.kill();
                return;
            }
            None => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = bus.kill();
                    panic!("nexusd did not exit within 5 s of SIGTERM");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// End-to-end ReloadConfig: spawn the real `nexusd`, edit the
/// config file (change `log_level` and `dbus.bus_name`), invoke
/// `Manager.ReloadConfig` over D-Bus, and assert the report has
/// `log_level` in `applied` and `dbus.bus_name` in `deferred`. This
/// exercises the file re-read, the diff, the BackendOps wiring, and
/// the D-Bus method end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reload_config_end_to_end_classifies_changes() {
    let bin = env!("CARGO_BIN_EXE_nexusd");
    let tmp = tempfile::tempdir().unwrap();
    let bus_addr = format!("unix:path={}", tmp.path().join("bus").display());
    let dbus_socket = tmp.path().join("bus");

    let mut bus = match Command::new("dbus-daemon")
        .arg("--session")
        .arg("--nofork")
        .arg(format!("--address=unix:path={}", dbus_socket.display()))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("dbus-daemon unavailable ({e}); skipping reload test");
            return;
        }
    };
    let socket_ready = Instant::now();
    while !dbus_socket.exists() {
        if socket_ready.elapsed() > Duration::from_secs(2) {
            let _ = bus.kill();
            eprintln!("bus never came up; skipping reload test");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let cfg_path = tmp.path().join("nexus.toml");
    let profile_root = tmp.path().join("profiles");
    std::fs::create_dir_all(&profile_root).unwrap();
    let initial = format!(
        r#"
bus_capacity = 64
log_level = "warn"

[supervision]
restart = false

[interface_monitor]
enabled = true

[profile_store]
root = "{}"
key_source = "in_memory"
in_memory_seed = "2222222222222222222222222222222222222222222222222222222222222222"

[dbus]
enabled = true
bus_name = "fi.nexus1.test.reload"
use_session_bus = false
address = "{}"
allow_all_authz = true
rate_limit_admin_per_min = 30

[ethernet]
enabled = false

[wifi]
enabled = false

[bluetooth]
enabled = false

[gnss]
enabled = false
"#,
        profile_root.display(),
        bus_addr,
    );
    std::fs::write(&cfg_path, &initial).unwrap();

    let mut child = Command::new(bin)
        .arg("--config")
        .arg(&cfg_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn nexusd");
    // Give the daemon a moment to come up + own the bus name.
    tokio::time::sleep(Duration::from_millis(800)).await;

    // Edit the config: a reloadable change (log_level) and a
    // startup-only change (dbus.bus_name).
    let edited = initial
        .replace("log_level = \"warn\"", "log_level = \"debug\"")
        .replace(
            "bus_name = \"fi.nexus1.test.reload\"",
            "bus_name = \"fi.nexus1.test.reload.future\"",
        );
    std::fs::write(&cfg_path, edited).unwrap();

    // Connect to the private bus and invoke ReloadConfig.
    let conn = match zbus::connection::Builder::address(bus_addr.as_str()) {
        Ok(b) => match b.build().await {
            Ok(c) => c,
            Err(e) => {
                let _ = child.kill();
                let _ = bus.kill();
                panic!("connect failed: {e}");
            }
        },
        Err(e) => {
            let _ = child.kill();
            let _ = bus.kill();
            panic!("address parse failed: {e}");
        }
    };
    let reply = conn
        .call_method(
            Some("fi.nexus1.test.reload"),
            "/fi/nexus1",
            Some("fi.nexus.Manager"),
            "ReloadConfig",
            &(),
        )
        .await
        .expect("ReloadConfig call");

    use std::collections::HashMap;
    use zbus::zvariant::OwnedValue;
    let dict: HashMap<String, OwnedValue> = reply.body().deserialize().unwrap();

    // Decode "applied" and "deferred" string arrays.
    let decode_array = |v: &OwnedValue| -> Vec<String> {
        let arr: &zbus::zvariant::Array = v.downcast_ref().unwrap();
        arr.iter()
            .map(|item| <&str>::try_from(item).unwrap().to_owned())
            .collect()
    };
    let applied = decode_array(&dict["applied"]);
    let deferred = decode_array(&dict["deferred"]);
    assert!(
        applied.contains(&"log_level".to_string()),
        "expected log_level in applied; got {applied:?}"
    );
    assert!(
        deferred.contains(&"dbus.bus_name".to_string()),
        "expected dbus.bus_name in deferred; got {deferred:?}"
    );

    // Tear down.
    let pid = child.id() as i32;
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = child.wait();
    let _ = bus.kill();
}
