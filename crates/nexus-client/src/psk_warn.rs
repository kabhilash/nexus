//! `--psk` credential-leak warning. DD-008 §6.2.
//!
//! When a PSK lands on the CLI via `--psk <value>` (rather than
//! `NEXUSCTL_PSK` or interactive entry), it's visible in `ps`,
//! shell history, and process accounting — a classic credential
//! leak. nexusctl prints a one-line warning to stderr so operators
//! notice the footgun. `--no-warn-psk` or `NEXUSCTL_NO_WARN_PSK=1`
//! suppresses it for scripted callers who accept the risk.

use std::io::Write;

pub const WARNING: &str = "warning: --psk exposes the passphrase to `ps` and shell history; \
     prefer NEXUSCTL_PSK, or an interactive prompt";

/// Emit the warning to `stderr` when applicable:
/// - `psk` is `Some(_)` (caller supplied `--psk`), AND
/// - `no_warn_psk` is `false`, AND
/// - `NEXUSCTL_NO_WARN_PSK` env var is not set to a truthy value.
pub fn maybe_warn_psk(psk: Option<&str>, no_warn_psk: bool, stderr: &mut dyn Write) {
    if psk.is_none() || no_warn_psk {
        return;
    }
    if matches!(
        std::env::var("NEXUSCTL_NO_WARN_PSK").as_deref(),
        Ok("1") | Ok("true")
    ) {
        return;
    }
    let _ = writeln!(stderr, "nexusctl: {WARNING}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_psk_emits_nothing() {
        let mut buf = Vec::new();
        maybe_warn_psk(None, false, &mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn psk_with_flag_suppressed() {
        let mut buf = Vec::new();
        maybe_warn_psk(Some("s3cret"), true, &mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn psk_without_suppression_emits_warning() {
        // Ensure the env var isn't set in this test's environment.
        // SAFETY: Tests run single-threaded-per-process by default
        // unless the crate opts into `cargo-nextest`; temporarily
        // clearing a well-known name is safe here.
        unsafe {
            std::env::remove_var("NEXUSCTL_NO_WARN_PSK");
        }
        let mut buf = Vec::new();
        maybe_warn_psk(Some("s3cret"), false, &mut buf);
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("--psk"), "got {s}");
        assert!(s.contains("ps"), "got {s}");
    }
}
