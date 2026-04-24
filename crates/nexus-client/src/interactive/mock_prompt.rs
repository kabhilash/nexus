//! Test-only mocks for the pairing flow.
//!
//! Gated behind `#[cfg(any(test, feature = "interactive-flows-testing"))]`
//! so production builds don't ship them. Integration tests opt in
//! via the `interactive-flows-testing` feature.

#![cfg(any(test, feature = "interactive-flows-testing"))]

use std::sync::Mutex;

use async_trait::async_trait;

use crate::errors::NexusctlError;
use crate::interactive::pairing::{
    AnswerSink, PairingAnswer, PairingEvent, PairingEventSource, PairingOutcome, Prompt,
    PromptData, PromptKind,
};

/// Scripted operator behaviour for [`MockPrompt`]. Every time the
/// flow asks the operator something, the mock looks up the answer
/// in this script.
#[derive(Debug, Clone, Default)]
pub struct Scripted {
    pub accept_confirmation: Option<bool>,
    pub authorize: Option<bool>,
    pub passkey: Option<u32>,
    pub pin: Option<String>,
    pub acknowledge_display: bool,
}

impl Scripted {
    pub fn accept_confirmation() -> Self {
        Self {
            accept_confirmation: Some(true),
            ..Default::default()
        }
    }
    pub fn reject_confirmation() -> Self {
        Self {
            accept_confirmation: Some(false),
            ..Default::default()
        }
    }
    pub fn acknowledge_display() -> Self {
        Self {
            acknowledge_display: true,
            ..Default::default()
        }
    }
    pub fn with_passkey(n: u32) -> Self {
        Self {
            passkey: Some(n),
            ..Default::default()
        }
    }
    pub fn with_pin(pin: &str) -> Self {
        Self {
            pin: Some(pin.to_owned()),
            ..Default::default()
        }
    }
}

/// Recording operator-interaction stub. Every call is logged so
/// tests can assert the flow walked the expected branches.
pub struct MockPrompt {
    script: Scripted,
    calls: Mutex<Vec<String>>,
    pub last_outcome: Mutex<Option<PairingOutcome>>,
}

impl MockPrompt {
    pub fn new(script: Scripted) -> Self {
        Self {
            script,
            calls: Mutex::new(Vec::new()),
            last_outcome: Mutex::new(None),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Prompt for MockPrompt {
    async fn render_progress(&mut self, _note: &str) -> Result<(), NexusctlError> {
        self.calls.lock().unwrap().push("progress".into());
        Ok(())
    }
    async fn confirm(&mut self, _data: &PromptData) -> Result<bool, NexusctlError> {
        self.calls.lock().unwrap().push("confirm".into());
        self.script.accept_confirmation.ok_or(NexusctlError::Other {
            raw: "no scripted answer for confirm".into(),
        })
    }
    async fn ask_passkey(&mut self, _data: &PromptData) -> Result<u32, NexusctlError> {
        self.calls.lock().unwrap().push("ask_passkey".into());
        self.script.passkey.ok_or(NexusctlError::Other {
            raw: "no scripted answer for passkey".into(),
        })
    }
    async fn ask_pin(&mut self, _data: &PromptData) -> Result<String, NexusctlError> {
        self.calls.lock().unwrap().push("ask_pin".into());
        self.script.pin.clone().ok_or(NexusctlError::Other {
            raw: "no scripted answer for pin".into(),
        })
    }
    async fn acknowledge(
        &mut self,
        _kind: PromptKind,
        _data: &PromptData,
    ) -> Result<(), NexusctlError> {
        self.calls.lock().unwrap().push("acknowledge".into());
        if self.script.acknowledge_display {
            Ok(())
        } else {
            Err(NexusctlError::Other {
                raw: "mock not configured to acknowledge".into(),
            })
        }
    }
    async fn authorize(
        &mut self,
        _kind: PromptKind,
        _data: &PromptData,
    ) -> Result<bool, NexusctlError> {
        self.calls.lock().unwrap().push("authorize".into());
        self.script.authorize.ok_or(NexusctlError::Other {
            raw: "no scripted authorize answer".into(),
        })
    }
    async fn render_outcome(&mut self, outcome: &PairingOutcome) -> Result<(), NexusctlError> {
        *self.last_outcome.lock().unwrap() = Some(outcome.clone());
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MockEventSource — scripted event stream
// ---------------------------------------------------------------------------

pub struct MockEventSource {
    queue: Mutex<std::collections::VecDeque<PairingEvent>>,
}

impl MockEventSource {
    pub fn new(events: Vec<PairingEvent>) -> Self {
        Self {
            queue: Mutex::new(events.into()),
        }
    }
}

#[async_trait]
impl PairingEventSource for MockEventSource {
    async fn next(&mut self) -> Option<PairingEvent> {
        // Non-blocking: return what's queued. When the queue is
        // empty, park the task forever so the caller's timeout /
        // ctrl-c wins the select. That's the "stream still alive,
        // just quiet" shape of a real subscription.
        if let Some(e) = self.queue.lock().unwrap().pop_front() {
            return Some(e);
        }
        // Park forever.
        std::future::pending::<Option<PairingEvent>>().await
    }
}

// ---------------------------------------------------------------------------
// MockAnswerSink — records the answers + cancel calls
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct MockAnswerSink {
    answers: Mutex<Vec<PairingAnswer>>,
    cancels: Mutex<u32>,
    /// When set, every call returns this error instead of `Ok`.
    fail_with: Mutex<Option<NexusctlError>>,
}

impl MockAnswerSink {
    pub fn answers(&self) -> Vec<PairingAnswer> {
        self.answers.lock().unwrap().clone()
    }
    pub fn cancel_count(&self) -> u32 {
        *self.cancels.lock().unwrap()
    }
    pub fn inject_error(&self, e: NexusctlError) {
        *self.fail_with.lock().unwrap() = Some(e);
    }
}

#[async_trait]
impl AnswerSink for MockAnswerSink {
    async fn answer(&self, answer: PairingAnswer) -> Result<(), NexusctlError> {
        if let Some(e) = self.fail_with.lock().unwrap().clone() {
            return Err(e);
        }
        self.answers.lock().unwrap().push(answer);
        Ok(())
    }
    async fn cancel_pairing(&self) -> Result<(), NexusctlError> {
        *self.cancels.lock().unwrap() += 1;
        Ok(())
    }
}
