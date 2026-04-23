//! Atomic-write crash-recovery test. The store's write path is
//! instrumented with a "crash after tmp fsync, before rename" hook
//! (see `fs_store::test_hooks`). This test toggles the hook, expects
//! a panic, catches it, and verifies that the pre-crash content of
//! the target file is intact.
//!
//! The three sub-scenarios (from-scratch, overwrite, crash) run in a
//! single test function so the crash-hook's global flag doesn't
//! trip other threads in a parallel run — the hook's guard
//! serializes tests against *each other*, but only when every
//! participating test takes the same lock. Keeping everything in
//! one test avoids the coordination problem entirely.

use std::panic;

use nexus_profile_store::fs_store::write_atomic;
use tempfile::TempDir;

#[test]
fn atomic_write_scenarios() {
    // -- Scenario 1: write from scratch creates the file.
    {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new.toml");
        write_atomic(&path, b"fresh = true\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh = true\n");
    }

    // -- Scenario 2: overwrite replaces content atomically.
    {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.toml");
        std::fs::write(&path, b"old = true\n").unwrap();
        write_atomic(&path, b"new = true\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new = true\n");
    }

    // -- Scenario 3: simulated crash between tmp fsync and rename
    //    leaves the original file intact. The CrashAfterTmpGuard
    //    takes a static mutex so no other test can hit `write_atomic`
    //    while the flag is set.
    {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("target.toml");
        std::fs::write(&path, b"original = true\n").unwrap();

        let outcome = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            let _guard = nexus_profile_store::fs_store::test_hooks::CrashAfterTmpGuard::new();
            write_atomic(&path, b"replacement = true\n").unwrap();
        }));
        assert!(outcome.is_err(), "expected simulated crash");

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(
            contents, b"original = true\n",
            "original file must survive a mid-write crash",
        );
    }
}
