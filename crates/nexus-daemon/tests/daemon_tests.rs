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
