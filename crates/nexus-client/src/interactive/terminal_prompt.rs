//! `TerminalPrompt` — the production [`Prompt`] impl. DD-008 §6.1.
//!
//! Uses `dialoguer` for the actual stdin/stdout interaction. When
//! stdin isn't a TTY, individual prompt methods fail with
//! `NotInteractive` rather than blocking forever.

use std::io::IsTerminal;

use async_trait::async_trait;
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Password};

use crate::errors::NexusctlError;
use crate::interactive::pairing::{PairingOutcome, Prompt, PromptData, PromptKind};

pub struct TerminalPrompt;

impl Default for TerminalPrompt {
    fn default() -> Self {
        Self::new()
    }
}

impl TerminalPrompt {
    pub fn new() -> Self {
        Self
    }

    fn require_tty(&self, op: &str) -> Result<(), NexusctlError> {
        if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
            Ok(())
        } else {
            Err(NexusctlError::NotInteractive {
                operation: op.to_owned(),
            })
        }
    }
}

#[async_trait]
impl Prompt for TerminalPrompt {
    async fn render_progress(&mut self, note: &str) -> Result<(), NexusctlError> {
        // Progress lines go to stderr so stdout is reserved for the
        // command's actual output.
        eprintln!("{note}");
        Ok(())
    }

    async fn confirm(&mut self, data: &PromptData) -> Result<bool, NexusctlError> {
        self.require_tty("pairing confirm")?;
        let passkey = data
            .passkey
            .map(|n| format!(" (passkey {n:06})"))
            .unwrap_or_default();
        let msg = format!(
            "Device {} is requesting pairing{passkey}. Accept?",
            short_device(data)
        );
        let ans = tokio::task::spawn_blocking(move || {
            let theme = ColorfulTheme::default();
            Confirm::with_theme(&theme).with_prompt(msg).interact()
        })
        .await
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt join: {e}"),
        })?
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt: {e}"),
        })?;
        Ok(ans)
    }

    async fn ask_passkey(&mut self, data: &PromptData) -> Result<u32, NexusctlError> {
        self.require_tty("pairing passkey entry")?;
        let msg = format!("Passkey for {}: ", short_device(data));
        let raw: String = tokio::task::spawn_blocking(move || {
            let theme = ColorfulTheme::default();
            Input::<String>::with_theme(&theme)
                .with_prompt(msg)
                .interact_text()
        })
        .await
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt join: {e}"),
        })?
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt: {e}"),
        })?;
        raw.trim()
            .parse::<u32>()
            .map_err(|_| NexusctlError::InvalidArgument {
                message: format!("passkey must be a 0..=999999 integer, got `{raw}`"),
            })
    }

    async fn ask_pin(&mut self, data: &PromptData) -> Result<String, NexusctlError> {
        self.require_tty("pairing PIN entry")?;
        let msg = format!("PIN for {} (4-16 ASCII chars): ", short_device(data));
        let raw: String = tokio::task::spawn_blocking(move || {
            let theme = ColorfulTheme::default();
            Input::<String>::with_theme(&theme)
                .with_prompt(msg)
                .interact_text()
        })
        .await
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt join: {e}"),
        })?
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt: {e}"),
        })?;
        Ok(raw.trim().to_owned())
    }

    async fn acknowledge(
        &mut self,
        kind: PromptKind,
        data: &PromptData,
    ) -> Result<(), NexusctlError> {
        self.require_tty("pairing acknowledge")?;
        let value = match kind {
            PromptKind::DisplayPasskey => data
                .passkey
                .map(|n| format!("passkey {n:06}"))
                .unwrap_or_else(|| "passkey <missing>".into()),
            PromptKind::DisplayPin => data
                .pincode
                .clone()
                .unwrap_or_else(|| "pin <missing>".into()),
            _ => "<unexpected>".into(),
        };
        eprintln!(
            "Confirm the {} showing on {} matches — press Enter once seen.",
            value,
            short_device(data)
        );
        let _: String = tokio::task::spawn_blocking(move || {
            let theme = ColorfulTheme::default();
            Input::<String>::with_theme(&theme)
                .allow_empty(true)
                .with_prompt("acknowledge")
                .interact_text()
        })
        .await
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt join: {e}"),
        })?
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt: {e}"),
        })?;
        Ok(())
    }

    async fn authorize(
        &mut self,
        kind: PromptKind,
        data: &PromptData,
    ) -> Result<bool, NexusctlError> {
        self.require_tty("pairing authorize")?;
        let detail = match kind {
            PromptKind::AuthorizeService => data
                .service_uuid
                .clone()
                .map(|u| format!(" for service {u}"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        let msg = format!("Authorize {}{detail}?", short_device(data));
        let ans = tokio::task::spawn_blocking(move || {
            let theme = ColorfulTheme::default();
            Confirm::with_theme(&theme).with_prompt(msg).interact()
        })
        .await
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt join: {e}"),
        })?
        .map_err(|e| NexusctlError::Other {
            raw: format!("prompt: {e}"),
        })?;
        Ok(ans)
    }

    async fn render_outcome(&mut self, outcome: &PairingOutcome) -> Result<(), NexusctlError> {
        let line = match outcome {
            PairingOutcome::Paired => "pairing complete".to_owned(),
            PairingOutcome::Rejected { reason } => format!("pairing rejected: {reason}"),
            PairingOutcome::Cancelled => "pairing cancelled".to_owned(),
            PairingOutcome::TimedOut => "pairing timed out".to_owned(),
            PairingOutcome::Failed { reason } => format!("pairing failed: {reason}"),
        };
        eprintln!("{line}");
        Ok(())
    }
}

fn short_device(data: &PromptData) -> String {
    // "/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF" → "AA:BB:CC:DD:EE:FF".
    if let Some(last) = data.device_path.rsplit('/').next() {
        if let Some(stripped) = last.strip_prefix("dev_") {
            return stripped.replace('_', ":");
        }
        return last.to_owned();
    }
    data.device_path.clone()
}

/// Helper for `wifi connect` / `profile add-wifi` interactive PSK
/// entry. Returns the passphrase typed by the operator with no
/// echo. Fails with `NotInteractive` when stdin isn't a TTY.
pub async fn prompt_psk(prompt: &str) -> Result<String, NexusctlError> {
    if !std::io::stdin().is_terminal() {
        return Err(NexusctlError::NotInteractive {
            operation: "Wi-Fi PSK entry".into(),
        });
    }
    let theme = ColorfulTheme::default();
    let label = prompt.to_owned();
    let psk: String = tokio::task::spawn_blocking(move || {
        Password::with_theme(&theme).with_prompt(label).interact()
    })
    .await
    .map_err(|e| NexusctlError::Other {
        raw: format!("prompt join: {e}"),
    })?
    .map_err(|e| NexusctlError::Other {
        raw: format!("prompt: {e}"),
    })?;
    Ok(psk)
}
