//! Interactive flows. DD-008 §6.
//!
//! Three flows live here:
//!
//! 1. [`pairing`] — Bluetooth pairing. A [`PairingFlow<P>`] drives
//!    the state machine; tests inject a [`mock_prompt::MockPrompt`]
//!    alongside a [`pairing::MockEventSource`] to exercise every
//!    DD-008 §6.1 branch without a real bus.
//!
//! 2. [`passphrase`] — the Wi-Fi PSK prompt used by
//!    `commands::wifi::connect` when no `--psk` /
//!    `NEXUSCTL_PSK` is supplied. Straight retry loop.
//!
//! 3. [`polkit`] — best-effort `pkttyagent` wrapper so PolicyKit
//!    prompts reach the terminal. Structured as a Drop guard so
//!    the child process is cleaned up on every exit path.
//!
//! [`cancellation`] hosts the shared SIGINT plumbing
//! (DD-008 §6.4), including the double-Ctrl-C immediate-termination
//! rule.

pub mod cancellation;
pub mod mock_prompt;
pub mod pairing;
pub mod passphrase;
pub mod polkit;
pub mod terminal_prompt;

pub use pairing::{
    AnswerSink, PairingAnswer, PairingEvent, PairingEventSource, PairingFlow, PairingOutcome,
    Prompt, PromptData, PromptKind,
};
