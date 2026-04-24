//! Bluetooth pairing state machine. DD-008 §6.1.
//!
//! # Shape
//!
//! The flow is a tokio `select!` loop that multiplexes:
//! - the `PairingPrompt` / `PairingComplete` signal stream from
//!   nexusd (abstracted as [`PairingEventSource`]);
//! - operator SIGINT (Ctrl-C);
//! - an overall timeout.
//!
//! Operator interaction sits behind the [`Prompt`] trait so the
//! state machine is exercise-able without a terminal. The
//! production impl is [`crate::interactive::terminal_prompt::TerminalPrompt`];
//! tests inject [`crate::interactive::mock_prompt::MockPrompt`].
//!
//! Operator answers leave the flow through an [`AnswerSink`]:
//! production wires `Manager.AnswerPairingPrompt`, mock records.
//!
//! # Race resolution
//!
//! Per DD-008 §6.1 step 6: when SIGINT arrives and
//! `PairingComplete(success=true)` is already queued, the flow
//! treats the success as authoritative — the pairing really did
//! finish before the operator's cancel took effect, and refusing
//! it would require tearing down an already-usable bond.
//! Concretely: after catching SIGINT we drain pending events for
//! a short grace window before declaring cancellation.

use std::time::Duration;

use async_trait::async_trait;

use crate::errors::NexusctlError;

// ---------------------------------------------------------------------------
// Prompt trait + data shapes
// ---------------------------------------------------------------------------

/// DD-006 §6.4 prompt kinds. Same variants the daemon emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKind {
    RequestPin,
    RequestPasskey,
    RequestConfirmation,
    RequestAuthorization,
    AuthorizeService,
    DisplayPasskey,
    DisplayPin,
}

impl PromptKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptKind::RequestPin => "request_pin",
            PromptKind::RequestPasskey => "request_passkey",
            PromptKind::RequestConfirmation => "request_confirmation",
            PromptKind::RequestAuthorization => "request_authorization",
            PromptKind::AuthorizeService => "authorize_service",
            PromptKind::DisplayPasskey => "display_passkey",
            PromptKind::DisplayPin => "display_pin",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptData {
    pub device_path: String,
    pub passkey: Option<u32>,
    pub pincode: Option<String>,
    pub service_uuid: Option<String>,
}

/// Answer the operator provides. Mirrors the DD-006 §6.4 answer
/// variants plus the universal `Cancel`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingAnswer {
    /// `RequestPin` — 4-16 ASCII chars.
    Pin(String),
    /// `RequestPasskey` — 0..=999_999.
    Passkey(u32),
    /// `RequestConfirmation` / `RequestAuthorization` /
    /// `AuthorizeService` — yes/no.
    Accept(bool),
    /// `DisplayPasskey` / `DisplayPin` — operator saw the value.
    Acknowledge,
    /// Universal cancel — works on every kind.
    Cancel,
}

/// Operator-interaction trait. Each method corresponds to one
/// `PairingPrompt` kind from DD-006 §6.4.
#[async_trait]
pub trait Prompt: Send {
    /// Show the adapter/device pair and an informational
    /// note before any prompts. Handlers may choose to paint a
    /// progress indicator.
    async fn render_progress(&mut self, note: &str) -> Result<(), NexusctlError>;

    /// The numeric-comparison flow. Display the passkey, ask
    /// yes/no.
    async fn confirm(&mut self, data: &PromptData) -> Result<bool, NexusctlError>;

    /// Prompt for a 6-digit passkey (DD-006 §6.4: 0..=999999).
    async fn ask_passkey(&mut self, data: &PromptData) -> Result<u32, NexusctlError>;

    /// Prompt for a PIN string (DD-006 §6.4: 4-16 ASCII).
    async fn ask_pin(&mut self, data: &PromptData) -> Result<String, NexusctlError>;

    /// DisplayPasskey / DisplayPin — nothing to input, just show
    /// the value and wait for acknowledgement.
    async fn acknowledge(
        &mut self,
        kind: PromptKind,
        data: &PromptData,
    ) -> Result<(), NexusctlError>;

    /// RequestAuthorization / AuthorizeService — yes/no, plus a
    /// service-uuid hint for the latter.
    async fn authorize(
        &mut self,
        kind: PromptKind,
        data: &PromptData,
    ) -> Result<bool, NexusctlError>;

    /// Final outcome line. Called once per `run()`.
    async fn render_outcome(&mut self, outcome: &PairingOutcome) -> Result<(), NexusctlError>;
}

// ---------------------------------------------------------------------------
// Events + sinks
// ---------------------------------------------------------------------------

/// One pump from the pairing-signal stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingEvent {
    Prompt {
        kind: PromptKind,
        data: PromptData,
    },
    /// `success = false` with `reason = "cancelled"` / `"rejected"`
    /// / `"timeout"` / ... . Nexusd's
    /// `PairingComplete(success, reason)` signal.
    Complete {
        success: bool,
        reason: String,
    },
}

/// Source of [`PairingEvent`]s. Production wraps zbus signal
/// streams; tests yield from a Vec.
#[async_trait]
pub trait PairingEventSource: Send {
    /// Wait for the next event. `None` means the stream closed
    /// (e.g. daemon went away) — the flow treats that as an error.
    async fn next(&mut self) -> Option<PairingEvent>;
}

/// Callback the flow invokes to forward an operator answer to the
/// backend.
#[async_trait]
pub trait AnswerSink: Send + Sync {
    async fn answer(&self, answer: PairingAnswer) -> Result<(), NexusctlError>;
    async fn cancel_pairing(&self) -> Result<(), NexusctlError>;
}

// ---------------------------------------------------------------------------
// Outcome
// ---------------------------------------------------------------------------

/// Terminal outcome of a pairing flow. Maps to an exit code via
/// [`PairingOutcome::exit_code`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingOutcome {
    Paired,
    Rejected { reason: String },
    Cancelled,
    TimedOut,
    Failed { reason: String },
}

impl PairingOutcome {
    pub fn exit_code(&self) -> i32 {
        match self {
            PairingOutcome::Paired => 0,
            PairingOutcome::Cancelled => 130,
            PairingOutcome::TimedOut => 4,
            // Rejected / Failed are general-mutation errors.
            PairingOutcome::Rejected { .. } | PairingOutcome::Failed { .. } => 1,
        }
    }
}

// ---------------------------------------------------------------------------
// The flow itself
// ---------------------------------------------------------------------------

/// Settings for a pairing run. Separate from [`PairingFlow`] so
/// tests can bypass real signal handling.
pub struct PairingFlowConfig {
    /// How long the flow waits before giving up. Real callers
    /// supply `bluetooth.agent_response_timeout_s` (DD-006 §6.4).
    pub overall_timeout: Duration,
    /// Grace window to wait for `PairingComplete` after SIGINT /
    /// timeout before declaring cancellation. DD-008 §6.4 calls
    /// for ~2 s.
    pub cancel_grace: Duration,
}

impl Default for PairingFlowConfig {
    fn default() -> Self {
        Self {
            overall_timeout: Duration::from_secs(90),
            cancel_grace: Duration::from_secs(2),
        }
    }
}

pub struct PairingFlow<P: Prompt> {
    pub prompt: P,
    pub events: Box<dyn PairingEventSource>,
    pub answer: std::sync::Arc<dyn AnswerSink>,
    pub config: PairingFlowConfig,
}

impl<P: Prompt> PairingFlow<P> {
    pub fn new(
        prompt: P,
        events: Box<dyn PairingEventSource>,
        answer: std::sync::Arc<dyn AnswerSink>,
        config: PairingFlowConfig,
    ) -> Self {
        Self {
            prompt,
            events,
            answer,
            config,
        }
    }

    /// Drive the pairing loop. Multiplexes the event source with
    /// Ctrl-C and the overall timeout. Returns the terminal
    /// [`PairingOutcome`].
    ///
    /// The `run_with_signal` variant below accepts an external
    /// "SIGINT fired" flag so tests drive the race-resolution arms
    /// deterministically.
    pub async fn run(self) -> PairingOutcome {
        let ctrl_c = tokio::signal::ctrl_c();
        self.run_with_signal(ctrl_c).await
    }

    /// Test-friendly entry point. `signal` is whatever future the
    /// caller wants to treat as SIGINT — tests can pass
    /// `std::future::pending()` (never fires) or a
    /// `oneshot::Receiver` they fire manually.
    pub async fn run_with_signal<S>(mut self, signal: S) -> PairingOutcome
    where
        S: std::future::Future<Output = std::io::Result<()>> + Send,
    {
        let overall = tokio::time::sleep(self.config.overall_timeout);
        tokio::pin!(signal);
        tokio::pin!(overall);

        loop {
            tokio::select! {
                // Fairness: prefer event arrivals over the signal
                // arm when both are ready, so a near-simultaneous
                // PairingComplete + SIGINT resolves as Paired. This
                // is the DD-008 §6.1 step-6 race rule, encoded in
                // one line.
                biased;

                ev = self.events.next() => {
                    match ev {
                        None => {
                            return PairingOutcome::Failed {
                                reason: "pairing signal stream ended unexpectedly".into(),
                            };
                        }
                        Some(PairingEvent::Complete { success: true, .. }) => {
                            let _ = self.prompt.render_outcome(&PairingOutcome::Paired).await;
                            return PairingOutcome::Paired;
                        }
                        Some(PairingEvent::Complete { success: false, reason }) => {
                            let outcome = classify_complete(&reason);
                            let _ = self.prompt.render_outcome(&outcome).await;
                            return outcome;
                        }
                        Some(PairingEvent::Prompt { kind, data }) => {
                            if let Err(e) = self.handle_prompt(kind, &data).await {
                                let outcome = PairingOutcome::Failed {
                                    reason: e.to_string(),
                                };
                                let _ = self.prompt.render_outcome(&outcome).await;
                                return outcome;
                            }
                        }
                    }
                }

                _ = &mut signal => {
                    return self.cancel_and_drain().await;
                }

                _ = &mut overall => {
                    return self.cancel_and_timeout().await;
                }
            }
        }
    }

    async fn handle_prompt(
        &mut self,
        kind: PromptKind,
        data: &PromptData,
    ) -> Result<(), NexusctlError> {
        let answer = match kind {
            PromptKind::RequestConfirmation => {
                if self.prompt.confirm(data).await? {
                    PairingAnswer::Accept(true)
                } else {
                    PairingAnswer::Accept(false)
                }
            }
            PromptKind::RequestAuthorization | PromptKind::AuthorizeService => {
                if self.prompt.authorize(kind, data).await? {
                    PairingAnswer::Accept(true)
                } else {
                    PairingAnswer::Accept(false)
                }
            }
            PromptKind::RequestPasskey => {
                PairingAnswer::Passkey(self.prompt.ask_passkey(data).await?)
            }
            PromptKind::RequestPin => PairingAnswer::Pin(self.prompt.ask_pin(data).await?),
            PromptKind::DisplayPasskey | PromptKind::DisplayPin => {
                self.prompt.acknowledge(kind, data).await?;
                PairingAnswer::Acknowledge
            }
        };
        self.answer.answer(answer).await
    }

    /// SIGINT path. Issue `CancelPairing`, drain events for the
    /// configured grace window, and resolve with the race rule
    /// — if a `Complete(success=true)` arrives inside the grace
    /// period we honour it.
    async fn cancel_and_drain(mut self) -> PairingOutcome {
        let _ = self.answer.cancel_pairing().await;
        let grace = tokio::time::sleep(self.config.cancel_grace);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                biased;
                ev = self.events.next() => match ev {
                    Some(PairingEvent::Complete { success: true, .. }) => {
                        let _ = self.prompt.render_outcome(&PairingOutcome::Paired).await;
                        return PairingOutcome::Paired;
                    }
                    Some(PairingEvent::Complete { success: false, reason }) => {
                        // Even if the reason is not "cancelled",
                        // treat the SIGINT path as Cancelled so the
                        // exit code matches the operator's intent.
                        let outcome = if reason.is_empty() {
                            PairingOutcome::Cancelled
                        } else {
                            PairingOutcome::Rejected { reason }
                        };
                        let _ = self.prompt.render_outcome(&outcome).await;
                        return PairingOutcome::Cancelled;
                    }
                    // Other events (prompts) arriving during the
                    // grace window are ignored — we're on our way
                    // out.
                    Some(_) => {}
                    None => {
                        let _ = self.prompt.render_outcome(&PairingOutcome::Cancelled).await;
                        return PairingOutcome::Cancelled;
                    }
                },
                _ = &mut grace => {
                    let _ = self.prompt.render_outcome(&PairingOutcome::Cancelled).await;
                    return PairingOutcome::Cancelled;
                }
            }
        }
    }

    async fn cancel_and_timeout(mut self) -> PairingOutcome {
        let _ = self.answer.cancel_pairing().await;
        // Same grace semantics as ctrl-c, but the terminal outcome
        // is TimedOut unless a Complete(success=true) sneaks in
        // under the wire.
        let grace = tokio::time::sleep(self.config.cancel_grace);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                biased;
                ev = self.events.next() => match ev {
                    Some(PairingEvent::Complete { success: true, .. }) => {
                        let _ = self.prompt.render_outcome(&PairingOutcome::Paired).await;
                        return PairingOutcome::Paired;
                    }
                    Some(_) | None => {}
                },
                _ = &mut grace => {
                    let _ = self.prompt.render_outcome(&PairingOutcome::TimedOut).await;
                    return PairingOutcome::TimedOut;
                }
            }
        }
    }
}

fn classify_complete(reason: &str) -> PairingOutcome {
    match reason {
        "rejected" | "pairing_rejected" => PairingOutcome::Rejected {
            reason: reason.into(),
        },
        "cancelled" => PairingOutcome::Cancelled,
        "timeout" | "pairing_timeout" => PairingOutcome::TimedOut,
        other => PairingOutcome::Failed {
            reason: other.into(),
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interactive::mock_prompt::{MockAnswerSink, MockEventSource, MockPrompt, Scripted};
    use std::sync::Arc;

    fn flow_fast<P: Prompt>(
        prompt: P,
        events: MockEventSource,
        answer: Arc<MockAnswerSink>,
    ) -> PairingFlow<P> {
        let sink: Arc<dyn AnswerSink> = answer;
        PairingFlow::new(
            prompt,
            Box::new(events),
            sink,
            PairingFlowConfig {
                overall_timeout: Duration::from_millis(200),
                cancel_grace: Duration::from_millis(100),
            },
        )
    }

    #[tokio::test]
    async fn happy_path_request_confirmation_accept() {
        let events = MockEventSource::new(vec![
            PairingEvent::Prompt {
                kind: PromptKind::RequestConfirmation,
                data: PromptData {
                    device_path: "/org/bluez/hci0/dev_AA".into(),
                    passkey: Some(123_456),
                    ..Default::default()
                },
            },
            PairingEvent::Complete {
                success: true,
                reason: "".into(),
            },
        ]);
        let prompt = MockPrompt::new(Scripted::accept_confirmation());
        let answer = Arc::new(MockAnswerSink::default());
        let outcome = flow_fast(prompt, events, Arc::clone(&answer))
            .run_with_signal(std::future::pending::<std::io::Result<()>>())
            .await;
        assert_eq!(outcome, PairingOutcome::Paired);
        assert_eq!(outcome.exit_code(), 0);
        let answers = answer.answers();
        assert_eq!(answers, vec![PairingAnswer::Accept(true)]);
    }

    #[tokio::test]
    async fn operator_rejects_confirmation_classifies_as_rejected() {
        let events = MockEventSource::new(vec![
            PairingEvent::Prompt {
                kind: PromptKind::RequestConfirmation,
                data: PromptData::default(),
            },
            PairingEvent::Complete {
                success: false,
                reason: "rejected".into(),
            },
        ]);
        let prompt = MockPrompt::new(Scripted::reject_confirmation());
        let answer = Arc::new(MockAnswerSink::default());
        let outcome = flow_fast(prompt, events, Arc::clone(&answer))
            .run_with_signal(std::future::pending::<std::io::Result<()>>())
            .await;
        assert!(matches!(outcome, PairingOutcome::Rejected { .. }));
        assert_eq!(outcome.exit_code(), 1);
        assert_eq!(answer.answers(), vec![PairingAnswer::Accept(false)]);
    }

    #[tokio::test]
    async fn overall_timeout_issues_cancel_and_exits_4() {
        // Emit only the prompt — no Complete. The prompt handler
        // will park on the operator (which the mock answers
        // instantly), then we wait for Complete that never comes.
        let events = MockEventSource::new(vec![PairingEvent::Prompt {
            kind: PromptKind::RequestConfirmation,
            data: PromptData::default(),
        }]);
        let prompt = MockPrompt::new(Scripted::accept_confirmation());
        let answer = Arc::new(MockAnswerSink::default());
        let sink: Arc<dyn AnswerSink> = Arc::clone(&answer) as Arc<dyn AnswerSink>;
        let flow = PairingFlow::new(
            prompt,
            Box::new(events),
            sink,
            PairingFlowConfig {
                overall_timeout: Duration::from_millis(100),
                cancel_grace: Duration::from_millis(30),
            },
        );
        let outcome = flow
            .run_with_signal(std::future::pending::<std::io::Result<()>>())
            .await;
        assert_eq!(outcome, PairingOutcome::TimedOut);
        assert_eq!(outcome.exit_code(), 4);
        // Cancel was issued during the cancel_and_timeout path.
        assert_eq!(answer.cancel_count(), 1);
    }

    #[tokio::test]
    async fn sigint_before_any_prompt_cancels() {
        // Stream is empty — the signal fires immediately.
        let events = MockEventSource::new(vec![]);
        let prompt = MockPrompt::new(Scripted::accept_confirmation());
        let answer = Arc::new(MockAnswerSink::default());
        let (tx, rx) = tokio::sync::oneshot::channel();
        // Fire SIGINT right away.
        let _ = tx.send(Ok::<(), std::io::Error>(()));
        let outcome = flow_fast(prompt, events, Arc::clone(&answer))
            .run_with_signal(async move { rx.await.unwrap() })
            .await;
        assert_eq!(outcome, PairingOutcome::Cancelled);
        assert_eq!(outcome.exit_code(), 130);
        assert_eq!(answer.cancel_count(), 1);
    }

    #[tokio::test]
    async fn sigint_concurrent_with_success_resolves_as_paired() {
        // Both the success event and the signal are "ready" at the
        // same tokio poll. `biased` in the select! prefers the
        // event source, so the outcome is Paired.
        let events = MockEventSource::new(vec![PairingEvent::Complete {
            success: true,
            reason: "".into(),
        }]);
        let prompt = MockPrompt::new(Scripted::accept_confirmation());
        let answer = Arc::new(MockAnswerSink::default());
        let (tx, rx) = tokio::sync::oneshot::channel();
        let _ = tx.send(Ok::<(), std::io::Error>(()));
        let outcome = flow_fast(prompt, events, Arc::clone(&answer))
            .run_with_signal(async move { rx.await.unwrap() })
            .await;
        assert_eq!(outcome, PairingOutcome::Paired);
        assert_eq!(answer.cancel_count(), 0);
    }

    #[tokio::test]
    async fn display_passkey_acknowledge_then_success() {
        let events = MockEventSource::new(vec![
            PairingEvent::Prompt {
                kind: PromptKind::DisplayPasskey,
                data: PromptData {
                    passkey: Some(42),
                    ..Default::default()
                },
            },
            PairingEvent::Complete {
                success: true,
                reason: "".into(),
            },
        ]);
        let prompt = MockPrompt::new(Scripted::acknowledge_display());
        let answer = Arc::new(MockAnswerSink::default());
        let outcome = flow_fast(prompt, events, Arc::clone(&answer))
            .run_with_signal(std::future::pending::<std::io::Result<()>>())
            .await;
        assert_eq!(outcome, PairingOutcome::Paired);
        assert_eq!(answer.answers(), vec![PairingAnswer::Acknowledge]);
    }
}
