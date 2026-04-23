//! Spawns the real `nexusctl` binary and asserts the exit codes /
//! shape promised by DD-008 §4.3.
//!
//! Phase 1 covers the two paths that don't need a running nexusd:
//!
//! 1. `--help` exits 0 with a usage line (clap's `display_help`).
//! 2. `--bus <missing>` makes `zbus::Connection::for_address` fail,
//!    which we map to `NexusctlError::NexusdUnreachable` (exit 6).
//!
//! End-to-end tests against a real daemon land in a later phase
//! once the binary has more than one mutating call to verify.

use std::process::Command;

fn nexusctl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_nexusctl"))
}

#[test]
fn help_exits_zero() {
    let status = nexusctl().arg("--help").output().expect("spawn");
    assert!(status.status.success(), "expected 0, got {status:?}");
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(stdout.contains("nexusctl"), "stdout: {stdout}");
}

#[test]
fn missing_bus_returns_exit_6() {
    // A bogus unix path → zbus's address resolver fails before any
    // method call. Our `from_zbus_error` translates that to
    // NexusdUnreachable, which exits 6.
    let bogus = "unix:path=/tmp/nexusctl-test-no-such-socket-1234567";
    let out = nexusctl()
        .args(["--bus", bogus, "status"])
        .output()
        .expect("spawn");
    let code = out.status.code().expect("normal exit");
    assert_eq!(code, 6, "got {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nexusd"), "stderr: {stderr}");
}

#[test]
fn unknown_subcommand_exits_2() {
    // clap's InvalidSubcommand exits 2 by default — matches DD-008
    // §4.3 "Usage error".
    let out = nexusctl().arg("bogus").output().expect("spawn");
    let code = out.status.code().expect("normal exit");
    assert_eq!(code, 2, "got {out:?}");
}
