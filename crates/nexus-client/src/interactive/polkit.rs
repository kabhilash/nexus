//! PolicyKit agent handoff. DD-008 §6.3.
//!
//! When the calling user isn't already running a desktop PolicyKit
//! agent (the typical SSH case), mutating commands would otherwise
//! fail with `AuthFailed` without the operator getting a chance to
//! authenticate. This module spawns `pkttyagent --process $$` so
//! PolicyKit prompts land on the current terminal, and tears it
//! down on Drop.
//!
//! Phase 5 implements the pragmatic subset: the full
//! `org.freedesktop.PolicyKit1.Authority.RegisterAuthenticationAgent`
//! dance is deferred. `pkttyagent` itself registers as an agent
//! for the caller's subject, which is sufficient for the
//! nexusctl/nexusd session-over-SSH scenario DD-008 §6.3 targets.
//! If `pkttyagent` isn't on PATH we warn once and proceed — root
//! / NOPASSWD sudo / an already-running agent can still satisfy
//! PolicyKit.

use std::process::Stdio;

/// Guard: spawning this future owns a `pkttyagent` child process.
/// On drop the child is killed so the terminal is released.
pub struct PolkitAgent {
    child: Option<tokio::process::Child>,
}

impl PolkitAgent {
    /// Try to spawn `pkttyagent --process <ourself>`. Returns a
    /// guard; drop it to kill the child. When `pkttyagent` isn't
    /// installed, prints a one-line warning to stderr and returns
    /// an empty guard (Drop is a no-op).
    pub fn spawn() -> Self {
        let pid = std::process::id().to_string();
        let spawn_result = tokio::process::Command::new("pkttyagent")
            .arg("--process")
            .arg(pid)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn();
        match spawn_result {
            Ok(child) => PolkitAgent { child: Some(child) },
            Err(e) => {
                // Most common cause: pkttyagent isn't installed.
                // Warn once so operators know why the PolicyKit
                // prompt never appeared; proceed without.
                eprintln!(
                    "nexusctl: pkttyagent not available ({e}); PolicyKit prompts \
                     will not reach this terminal — run as root or install \
                     `policykit-1`"
                );
                PolkitAgent { child: None }
            }
        }
    }

    /// `None` when the caller passed `--no-polkit-agent` or the
    /// config set `[interactive] auto_polkit_agent = false`.
    /// Otherwise calls `spawn`.
    pub fn maybe_spawn(enable: bool) -> Option<Self> {
        if enable { Some(Self::spawn()) } else { None }
    }
}

impl Drop for PolkitAgent {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            // `start_kill` is non-blocking; we don't wait for the
            // child in Drop since async runtimes may already be
            // shutting down. `pkttyagent` exits cleanly on SIGTERM.
            let _ = child.start_kill();
        }
    }
}
