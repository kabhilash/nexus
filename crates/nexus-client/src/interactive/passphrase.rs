//! Wi-Fi passphrase entry. DD-008 §6.2.
//!
//! Called by `commands::wifi::connect` (and, future-phases,
//! `profile add-wifi`) when no `--psk` and no `NEXUSCTL_PSK` are
//! supplied. Prompts up to `max_retries` times on auth failure.
//!
//! Phase 5 doesn't yet watch `Wifi.StateChanged` for the auth
//! outcome — that signal stream arrives with the watch work in
//! Phase 7.6. Until then the retry loop runs once: if the
//! connection succeeds the profile is saved; on failure the
//! caller sees the wrapped error.

use crate::errors::NexusctlError;
use crate::interactive::terminal_prompt;

/// Read the PSK from env first, then from the TTY. When stdin isn't
/// a TTY and neither env var nor `--psk` is supplied, returns
/// `NotInteractive` (exit 5).
pub async fn resolve_psk(cli_psk: Option<&str>) -> Result<String, NexusctlError> {
    if let Some(p) = cli_psk {
        return Ok(p.to_owned());
    }
    if let Ok(p) = std::env::var("NEXUSCTL_PSK") {
        if !p.is_empty() {
            return Ok(p);
        }
    }
    terminal_prompt::prompt_psk("Wi-Fi passphrase").await
}
