//! A harness with no vendor behind it, for proving a host and the conformance suite itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex;

use crate::configuration::{
    Configuration, ConfigurationCatalog, ConfigurationCategory, ConfigurationOption,
    ConfigurationOptionId, ConfigurationOptionValue, ConfigurationOutcome, ConfigurationPatch,
    ConfigurationState, ConfigurationValue, ConfigurationValueType, RejectedSetting, Rollback,
    SettingRejection,
};
use crate::content::ActivityContent;
use crate::discovery::{AuthMode, AuthState, Discovery, GateVerdict, Model};
use crate::error::{Error, ErrorCode, Result, VendorError};
use crate::event::{
    Activity, ActivityKind, ActivityResult, ActivityStatus, Command, EventKind, Usage,
};
use crate::harness::{
    Capabilities, CapabilityCeiling, DiscoveredCapabilities, Harness, HarnessDescriptor,
    SessionCapabilities, VendorInfo,
};
use crate::host::HostContext;
use crate::identity::HarnessIdentity;
use crate::interaction::{
    Interaction, InteractionId, InteractionKind, Question, QuestionForm, QuestionId,
    QuestionOutcome, QuestionRequest, QuestionResponse,
};
use crate::operation::OperationRef;
use crate::permission::{
    ApprovalDecision, ConfigurationVerdict, DecisionSource, PermissionEffect, PermissionMatrix,
    PermissionOption, PermissionRequest, PermissionResponse, PermissionRisk, PermissionScope,
    broker_response,
};
use crate::session::{
    CancelReason, CloseReason, OpenSession, Session, SessionIds, Steer, SteerOutcome, TurnRequest,
};
use crate::state::{SessionSnapshot, SessionState, SessionStatus, TransportSelection};
use crate::stream::{EventSink, TurnStream};
use crate::transport::TransportKind;

const VENDOR: VendorInfo = VendorInfo {
    company: "Nobody",
    terms_url: "https://example.invalid/terms",
    privacy_url: "https://example.invalid/privacy",
    skills_are_slash_commands: false,
};

/// The one option this harness pretends its vendor exposes beyond the well-known axes.
const FAKE_NATIVE_OPTION: &str = "fake-mode";

/// A [`Harness`] that answers from memory.
///
/// It emits the shape every real harness emits — a turn start, text, an approval that waits for an
/// answer, an activity, usage and a completion — and it keeps the session-scoped facts where they
/// belong, in its [`SessionState`]. A host can be written and tested against it before any vendor
/// CLI exists, and the conformance suite has something to prove itself against.
///
/// It is deliberately not a *kind* fake. It refuses a reset it cannot honour, reports a partially
/// applied patch as partial, and declines to advertise a capability it does not implement, because
/// a fake that says yes to everything proves nothing about the refusal paths a real host will hit.
#[derive(Clone, Debug)]
pub struct FakeHarness {
    descriptor: Arc<HarnessDescriptor>,
    asks_for_approval: bool,
    asks_a_question: bool,
    rejects_answers: bool,
    publishes_session_updates: bool,
}

impl Default for FakeHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHarness {
    /// A harness that asks for one approval per turn.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::testing::FakeHarness;
    /// use mango_external_agents::{Harness, HarnessId};
    ///
    /// let harness = FakeHarness::new();
    /// assert_eq!(harness.descriptor().id(), &HarnessId::claude());
    /// ```
    pub fn new() -> Self {
        Self {
            descriptor: Arc::new(HarnessDescriptor {
                identity: HarnessIdentity::claude(),
                vendor: VENDOR,
                capabilities: CapabilityCeiling::new(Capabilities {
                    structured_streaming: true,
                    interactive_approvals: true,
                    questions: true,
                    resume: true,
                    usage_reporting: true,
                    cancellation: true,
                    steering: true,
                    configuration: true,
                    session_configuration: true,
                    configuration_catalog: true,
                    ..Capabilities::none()
                }),
                transports: &[TransportKind::Stdio],
                vendor_environment_keys: &["FAKE_AGENT_CONFIG_DIR"],
            }),
            asks_for_approval: true,
            asks_a_question: false,
            rejects_answers: false,
            publishes_session_updates: true,
        }
    }

    /// The same harness with turns that never ask for anything.
    #[must_use]
    pub fn without_approvals(mut self) -> Self {
        self.asks_for_approval = false;
        self
    }

    /// The same harness whose turns stop to ask a question rather than for an approval.
    ///
    /// A question grants nothing, so a turn that stops at one is a turn no
    /// [`PermissionBroker`](crate::PermissionBroker) may answer. That distinction is what this
    /// mode exists to make testable.
    #[must_use]
    pub fn asking_a_question(mut self) -> Self {
        self.asks_a_question = true;
        self.asks_for_approval = false;
        self
    }

    /// The same harness that keeps its session state to itself.
    ///
    /// The shape a harness has when it mutates its own fields instead of publishing through
    /// [`SessionState`]: every subscriber waits forever. It exists so the conformance suite's
    /// session-state check has something it is supposed to fail.
    #[must_use]
    pub fn without_session_updates(mut self) -> Self {
        self.publishes_session_updates = false;
        self
    }

    /// The same harness whose session will not take the answer to its own question.
    ///
    /// A vendor that asks for an approval and then refuses the response is a broken round-trip,
    /// not an approval that was answered. This is what proves the conformance suite says so.
    #[must_use]
    pub fn rejecting_answers(mut self) -> Self {
        self.rejects_answers = true;
        self
    }

    /// The catalog this harness pretends its vendor publishes.
    fn catalog() -> ConfigurationCatalog {
        ConfigurationCatalog::new(vec![
            ConfigurationOption::new(
                ConfigurationOptionId::new("model"),
                ConfigurationCategory::Model,
                ConfigurationValueType::Enumerated,
            )
            .with_name("Model")
            .with_values(vec![
                ConfigurationOptionValue::new(ConfigurationValue::text("fake-default"))
                    .as_default(),
                ConfigurationOptionValue::new(ConfigurationValue::text("fast")),
            ])
            .resettable(),
            // Deliberately not resettable: a vendor that cannot put an option back to its own
            // default has to refuse, and a fake where everything resets would never exercise that.
            ConfigurationOption::new(
                ConfigurationOptionId::new(FAKE_NATIVE_OPTION),
                ConfigurationCategory::Other(String::from("fake")),
                ConfigurationValueType::Boolean,
            )
            .with_name("Fake mode"),
        ])
    }
}

#[async_trait::async_trait]
impl Harness for FakeHarness {
    fn descriptor(&self) -> &HarnessDescriptor {
        &self.descriptor
    }

    fn permission_matrix(&self) -> PermissionMatrix {
        PermissionMatrix::build(|_, _| ConfigurationVerdict::supported())
    }

    async fn probe(&self, _host: &HostContext) -> Result<Discovery> {
        Ok(Discovery {
            executable: Some("/nowhere/fake-agent".into()),
            version: Some(String::from("0.1.0")),
            gate: GateVerdict::Usable,
            auth: AuthState::LoggedIn {
                mode: AuthMode::Subscription,
            },
            capabilities: DiscoveredCapabilities::new(*self.descriptor.capabilities.capabilities()),
            permission_matrix: self.permission_matrix(),
            models: vec![Model::new("fake-default")],
            configuration_catalog: Self::catalog(),
        })
    }

    async fn open_session(
        &self,
        host: &HostContext,
        request: OpenSession,
    ) -> Result<Box<dyn Session>> {
        self.validate_open_session(host, &request)?;
        let transport = self.descriptor.resolve_transport(request.transport)?;
        let resumed = request.resume.is_some();
        let native_session_id = request.resume.as_ref().map_or_else(
            || String::from("fake-session-1"),
            |resume| resume.native_session_id.clone(),
        );

        // Requested is what the host asked for; accepted is what this harness would really encode.
        // They are equal here only because the fake accepts everything it is asked for — and they
        // stay separate fields so a host reading one never reads it as the other.
        let requested = request.configuration.requested();
        let configuration = ConfigurationState::new(
            requested.clone(),
            requested,
            // Nothing observed: this fake has no vendor surface that reports its own settings, and
            // copying `accepted` across here is exactly the lie the three-way split prevents.
            Configuration::unknown(),
        );

        let mut snapshot = SessionSnapshot::opening(
            SessionIds {
                session_id: request.session_id,
                native_session_id,
            },
            self.descriptor.identity.clone(),
            TransportSelection::new(request.transport, transport),
            host.now(),
        )
        .with_capabilities(SessionCapabilities::new(
            *self.descriptor.capabilities.capabilities(),
        ))
        .with_configuration(configuration)
        .with_catalog(Self::catalog());
        if resumed {
            snapshot = snapshot.resumed();
        }

        Ok(Box::new(FakeSession {
            state: SessionState::new(Arc::clone(host.clock()), snapshot),
            host: host.clone(),
            asks_for_approval: self.asks_for_approval,
            asks_a_question: self.asks_a_question,
            rejects_answers: self.rejects_answers,
            publishes_session_updates: self.publishes_session_updates,
            pending: Mutex::new(None),
            turns: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }))
    }
}

/// The session [`FakeHarness`] opens.
struct FakeSession {
    state: SessionState,
    host: HostContext,
    asks_for_approval: bool,
    asks_a_question: bool,
    rejects_answers: bool,
    publishes_session_updates: bool,
    pending: Mutex<Option<PendingTurn>>,
    turns: AtomicU64,
    closed: AtomicBool,
}

struct PendingTurn {
    sink: EventSink,
    approval: Option<PermissionRequest>,
    question: Option<QuestionRequest>,
    /// The activity this turn announced and has not closed.
    ///
    /// Held so a cancel and a close can end it. A fake that leaves one running teaches every
    /// harness written against it that a turn may end owing a spinner nobody will stop, which is
    /// what the conformance suite's structure check exists to refuse.
    open_activity: Option<String>,
}

/// Ends everything a stopped turn left open, before its terminal goes out.
///
/// The activity, and the interaction somebody is looking at. `Cancelled` in every case: nothing
/// went wrong with the call or the ask, the turn they belonged to stopped. A fake that skips this
/// teaches every harness written against it that a turn may end owing a spinner nobody will stop
/// or a dialog with no button that does anything.
///
/// Best-effort emits — the sink may already be closed by a racing terminal, and the terminal is the
/// event that matters.
async fn close_open_interactions(turn: &PendingTurn) {
    if let Some(call_id) = turn.open_activity.clone() {
        let _ = turn
            .sink
            .emit(EventKind::ActivityCompleted {
                call_id,
                result: ActivityResult::new(ActivityStatus::Cancelled),
            })
            .await;
    }
    if let Some(question) = &turn.question {
        let _ = turn
            .sink
            .emit(EventKind::QuestionResolved {
                interaction_id: question.interaction.id.clone(),
                outcome: QuestionOutcome::Cancelled,
            })
            .await;
    }
    if let Some(approval) = &turn.approval {
        let _ = turn
            .sink
            .emit(EventKind::ApprovalResolved {
                interaction_id: approval.id().clone(),
                decision: ApprovalDecision::unresolved(
                    "withdrawn",
                    crate::permission::DecisionSource::Cancelled,
                ),
            })
            .await;
    }
}

fn approval_request(
    operation: OperationRef,
    expires_at: std::time::SystemTime,
) -> PermissionRequest {
    PermissionRequest::new(
        Interaction::new(
            InteractionId::new("fake-approval-1"),
            InteractionKind::Permission,
            operation.session_id.clone(),
            expires_at,
        )
        .during(operation),
        ActivityKind::Command,
        "Run `cargo test`",
        vec![
            PermissionOption::new("allow", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once)
                .with_risk(PermissionRisk::Reversible)
                .with_label("Allow"),
            PermissionOption::new("allow-always", PermissionEffect::Allow)
                .with_scope(PermissionScope::Session)
                .policy_changing()
                .with_label("Allow for this session"),
            PermissionOption::new("deny", PermissionEffect::Reject)
                .with_scope(PermissionScope::Once)
                .with_label("Deny"),
        ],
    )
    .with_detail("cargo test --workspace")
}

fn question_request(operation: OperationRef, expires_at: std::time::SystemTime) -> QuestionRequest {
    QuestionRequest::new(
        Interaction::new(
            InteractionId::new("fake-question-1"),
            InteractionKind::Question,
            operation.session_id.clone(),
            expires_at,
        )
        .during(operation),
        vec![
            Question::new(
                QuestionId::new("branch"),
                "Which branch should I work on?",
                QuestionForm::FreeText {
                    placeholder: Some(String::from("main")),
                },
            )
            .required(),
            Question::new(
                QuestionId::new("run-tests"),
                "Run the tests afterwards?",
                QuestionForm::Choice {
                    options: vec![
                        crate::interaction::QuestionOption::new(
                            crate::interaction::QuestionOptionId::new("yes"),
                        )
                        .with_label("Yes"),
                        crate::interaction::QuestionOption::new(
                            crate::interaction::QuestionOptionId::new("no"),
                        )
                        .with_label("No"),
                    ],
                    multi_select: false,
                },
            ),
        ],
    )
    .with_title("Before I start")
}

impl FakeSession {
    async fn finish(
        &self,
        option_id: &str,
        source: DecisionSource,
        owner: Option<&OperationRef>,
    ) -> Result<()> {
        let mut pending = self.pending.lock().await;
        if owner.is_some_and(|owner| {
            pending
                .as_ref()
                .is_none_or(|turn| turn.sink.operation() != *owner)
        }) {
            return Ok(());
        }
        let Some(turn) = pending.take() else {
            return Ok(());
        };
        let decision = turn
            .approval
            .as_ref()
            .and_then(|request| {
                request
                    .options
                    .iter()
                    .find(|option| option.id == option_id)
                    .map(|option| ApprovalDecision::from_option(option, source))
            })
            .unwrap_or_else(|| ApprovalDecision::unresolved(option_id, source));
        let interaction_id = turn.approval.map_or_else(
            || InteractionId::new("fake-approval-1"),
            |request| request.interaction.id,
        );
        turn.sink
            .emit(EventKind::ApprovalResolved {
                interaction_id,
                decision,
            })
            .await?;
        let result = if option_id == "deny" {
            ActivityResult::new(ActivityStatus::Cancelled).with_detail("refused")
        } else {
            ActivityResult::new(ActivityStatus::Completed)
                .with_detail("3 passed")
                .with_content(ActivityContent::Output {
                    text: String::from("test result: ok. 3 passed; 0 failed"),
                })
        };
        turn.sink
            .emit(EventKind::ActivityCompleted {
                call_id: String::from("call-1"),
                result,
            })
            .await?;
        turn.sink
            .emit(EventKind::Usage {
                usage: Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(34),
                    ..Usage::default()
                },
            })
            .await?;
        turn.sink.complete().await
    }

    /// Applies a patch the way a vendor with no reset and one unknown option would.
    fn apply(&self, patch: &ConfigurationPatch) -> ConfigurationOutcome {
        let current = self.state.snapshot().configuration.clone();
        let mut applied = Vec::new();
        let mut rejected = Vec::new();

        for (id, change) in [
            ("model", patch.model.is_keep(), patch.model.is_reset()),
            ("effort", patch.effort.is_keep(), patch.effort.is_reset()),
            ("level", patch.level.is_keep(), patch.level.is_reset()),
            ("routing", patch.routing.is_keep(), patch.routing.is_reset()),
        ]
        .into_iter()
        .map(|(id, keep, reset)| (ConfigurationOptionId::new(id), (keep, reset)))
        {
            let (keep, reset) = change;
            if keep {
                continue;
            }
            // Only `model` is declared resettable in this fake's catalog, so every other reset is
            // the refusal a vendor without reset semantics owes a host.
            if reset && id.as_str() != "model" {
                rejected.push(RejectedSetting::new(
                    id,
                    SettingRejection::ResetNotSupported,
                ));
                continue;
            }
            applied.push(id);
        }
        for (id, change) in &patch.native {
            if change.is_keep() {
                continue;
            }
            if id.as_str() != FAKE_NATIVE_OPTION {
                rejected.push(RejectedSetting::new(
                    id.clone(),
                    SettingRejection::UnknownOption,
                ));
                continue;
            }
            if change.is_reset() {
                rejected.push(RejectedSetting::new(
                    id.clone(),
                    SettingRejection::ResetNotSupported,
                ));
                continue;
            }
            applied.push(id.clone());
        }

        // The accepted half records only what the vendor applied. The requested half still records
        // every host request, including the rejected ones, because the two facts differ here.
        let mut accepted_patch = ConfigurationPatch::new();
        if applied.contains(&ConfigurationOptionId::new("model")) {
            accepted_patch.model = patch.model.clone();
        }
        if applied.contains(&ConfigurationOptionId::new("effort")) {
            accepted_patch.effort = patch.effort.clone();
        }
        if applied.contains(&ConfigurationOptionId::new("level")) {
            accepted_patch.level = patch.level;
        }
        if applied.contains(&ConfigurationOptionId::new("routing")) {
            accepted_patch.routing = patch.routing;
        }
        for (id, change) in &patch.native {
            if applied.contains(id) {
                accepted_patch.native.insert(id.clone(), change.clone());
            }
        }
        let accepted = current.accepted.patched(&accepted_patch);
        let state = ConfigurationState::new(
            current.requested.patched(patch),
            accepted,
            current.observed.clone(),
        );
        let outcome = ConfigurationOutcome::applied(state, applied);
        if rejected.is_empty() {
            return outcome;
        }
        // A vendor whose settings cannot be un-set cannot roll back what already landed, and
        // saying "not attempted" is the difference between a host showing the truth and a host
        // showing a transaction that did not happen.
        outcome.rejecting(rejected, Rollback::NotAttempted)
    }
}

#[async_trait::async_trait]
impl Session for FakeSession {
    fn state(&self) -> &SessionState {
        &self.state
    }

    async fn configure(&self, patch: ConfigurationPatch) -> Result<ConfigurationOutcome> {
        self.require_capability(crate::Capability::SessionConfiguration)?;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed { subject: "session" });
        }
        let outcome = self.apply(&patch);
        self.state.set_configuration(outcome.state.clone());
        Ok(outcome)
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        let mut pending = self.pending.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed { subject: "session" });
        }
        if pending
            .as_ref()
            .is_some_and(|turn| !turn.sink.is_closed() && !turn.sink.is_terminal())
        {
            return Err(Error::Busy.with_dispatch(crate::Dispatch::NotSubmitted));
        }
        pending.take();
        self.validate_turn_request(&request)?;
        if let Some(patch) = &request.configuration {
            let outcome = self.apply(patch);
            self.state.set_configuration(outcome.state);
        }
        let turn = self.turns.fetch_add(1, Ordering::Relaxed) + 1;
        let snapshot = self.state.snapshot();
        let (sink, events) = EventSink::with_limits(
            snapshot.ids.session_id.clone(),
            request.turn_id.clone(),
            request.attempt,
            Arc::clone(self.host.clock()),
            self.host.limits(),
        );
        let native_turn_id = format!("fake-turn-{turn}");

        // The command catalog is session state, not transcript: it says what a person may type
        // next, so it is published where a host can read it before any turn has run.
        if self.publishes_session_updates {
            self.state
                .set_commands(crate::event::normalized_catalog(vec![
                    Command::new("review").with_description("Reviews the diff"),
                ]));
        }

        sink.emit(EventKind::TurnStarted {
            native_turn_id: native_turn_id.clone(),
        })
        .await?;
        sink.emit(EventKind::TextDelta {
            text: format!("working on {}", request.input),
        })
        .await?;

        let stream = || {
            TurnStream::accepted(
                request.turn_id.clone(),
                request.attempt,
                native_turn_id.clone(),
                events,
            )
        };
        let operation = sink.operation();

        if self.asks_a_question {
            let question = question_request(operation, self.host.now() + Duration::from_secs(300));
            *pending = Some(PendingTurn {
                sink: sink.clone(),
                approval: None,
                question: Some(question.clone()),
                open_activity: None,
            });
            // No broker is consulted: a question authorises nothing, so a permission policy has no
            // standing to answer one.
            sink.emit(EventKind::QuestionAsked { request: question })
                .await?;
            return Ok(stream());
        }

        if !self.asks_for_approval {
            sink.complete().await?;
            return Ok(stream());
        }

        sink.emit(EventKind::ActivityStarted {
            call_id: String::from("call-1"),
            activity: Activity::new("Bash", ActivityKind::Command, "cargo test")
                .with_item_id("item-1"),
        })
        .await?;

        let request_for_approval = approval_request(
            operation.clone(),
            self.host.now() + Duration::from_secs(300),
        );
        *pending = Some(PendingTurn {
            sink: sink.clone(),
            approval: Some(request_for_approval.clone()),
            question: None,
            open_activity: Some(String::from("call-1")),
        });
        drop(pending);

        // A host policy answers first when it has one; otherwise the question reaches the host.
        // The event is emitted either way — a turn whose approval a policy answered still shows
        // the host what was asked — so only the answer is conditional.
        let answer = broker_response(self.host.broker(), &request_for_approval).await;
        sink.emit(EventKind::ApprovalRequested {
            request: request_for_approval,
        })
        .await?;
        if let Some(response) = answer {
            self.finish(&response.option_id, response.source, Some(&operation))
                .await?;
        }

        Ok(stream())
    }

    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        self.require_capability(crate::Capability::InteractiveApprovals)?;
        if self.rejects_answers {
            return Err(Error::Vendor(VendorError::new(
                ErrorCode::from_static("fake-answer-rejected"),
                "this harness would not take the answer",
            )));
        }
        self.finish(&response.option_id, response.source, None)
            .await
    }

    async fn answer(&self, response: QuestionResponse) -> Result<()> {
        self.require_capability(crate::Capability::Questions)?;
        // Validated before the pending turn is taken, never after. A refused answer must leave the
        // question outstanding: taking first would let one malformed answer end a turn that nobody
        // successfully answered, and the caller's retry would then find nothing to answer.
        let mut pending = self.pending.lock().await;
        let asked = {
            let Some(turn) = pending.as_ref() else {
                return Ok(());
            };
            let Some(asked) = turn.question.clone() else {
                return Err(Error::Protocol {
                    expected: String::from("an outstanding question"),
                    received: String::from("an approval"),
                });
            };
            asked.validate(&response)?;
            asked
        };
        let Some(turn) = pending.take() else {
            return Ok(());
        };
        turn.sink
            .emit(EventKind::QuestionResolved {
                interaction_id: asked.interaction.id.clone(),
                outcome: QuestionOutcome::Answered {
                    answers: response.answers,
                },
            })
            .await?;
        turn.sink.complete().await
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        let mut pending = self.pending.lock().await;
        let Some(turn) = pending.take() else {
            return Ok(());
        };
        close_open_interactions(&turn).await;
        turn.sink.cancel(reason).await
    }

    async fn close(&self, _reason: CloseReason) -> Result<()> {
        let mut pending = self.pending.lock().await;
        self.closed.store(true, Ordering::Release);
        self.state.set_status(SessionStatus::Closed);
        // Terminal commitment is immediate and remains inside the admission critical section.
        if let Some(turn) = pending.take() {
            // A close ends whatever was running, for the same reason.
            close_open_interactions(&turn).await;
            let _ = turn.sink.cancel(CancelReason::Shutdown).await;
        }
        Ok(())
    }

    async fn steer(&self, steer: Steer) -> Result<SteerOutcome> {
        let sink = self
            .pending
            .lock()
            .await
            .as_ref()
            .map(|turn| turn.sink.clone());
        let Some(sink) = sink else {
            return Ok(SteerOutcome::Rejected {
                reason: crate::session::SteerRejection::TurnAlreadyCompleted,
            });
        };
        sink.emit(EventKind::TextDelta {
            text: format!(" and {}", steer.input),
        })
        .await?;
        Ok(SteerOutcome::Accepted)
    }
}

#[cfg(test)]
mod tests {
    use super::{FAKE_NATIVE_OPTION, FakeHarness};
    use crate::configuration::{
        ConfigurationChange, ConfigurationOptionId, ConfigurationPatch, Rollback,
    };
    use crate::event::EventKind;
    use crate::harness::Harness;
    use crate::host::HostContext;
    use crate::permission::{ApprovalRouting, PermissionLevel, PermissionScope};
    use crate::session::{OpenSession, TurnRequest};
    use crate::testing::FakeLauncher;
    use std::sync::Arc;

    fn host() -> HostContext {
        HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd("/workspace")
            .client_info("test-host", "0.0.0")
            .build()
            .expect("expected a context")
    }

    #[tokio::test]
    async fn an_unanswered_turn_refuses_a_second_start_without_replacing_its_owner() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("chat"))
            .await
            .expect("session");
        let mut first = session
            .start_turn(TurnRequest::new("first", "wait for approval"))
            .await
            .expect("first turn");
        let second = session
            .start_turn(TurnRequest::new("second", "must not replace"))
            .await;
        assert!(
            matches!(second, Err(ref error) if matches!(error.cause(), crate::Error::Busy)),
            "expected typed busy, received {second:?}"
        );
        session
            .cancel(crate::CancelReason::Requested)
            .await
            .expect("cancel original");
        let next = session
            .start_turn(TurnRequest::new("next", "old stream is unread"))
            .await
            .expect("terminal commitment releases admission before drain");
        drop(next);
        let _replacement = session
            .start_turn(TurnRequest::new(
                "replacement",
                "abandoned fake work is inactive",
            ))
            .await
            .expect("abandoned fake work does not retain admission");
        let mut terminal = false;
        while let Some(event) = first.recv().await {
            terminal |= event.is_terminal();
        }
        assert!(
            terminal,
            "expected the original stream to retain its terminal"
        );
    }

    #[tokio::test]
    async fn a_turn_stops_at_its_approval_until_it_is_answered() {
        let harness = FakeHarness::new();
        let host = host();
        let session = harness
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "ship it"))
            .await
            .expect("expected a turn");

        let mut kinds = Vec::new();
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            let asked = matches!(event.kind, EventKind::ApprovalRequested { .. });
            kinds.push(event.kind);
            if terminal || asked {
                break;
            }
        }
        assert!(
            matches!(kinds.last(), Some(EventKind::ApprovalRequested { .. })),
            "expected the turn to stop at its approval, received {kinds:?}"
        );

        let EventKind::ApprovalRequested { request } = kinds.pop().expect("expected the approval")
        else {
            panic!("expected an approval");
        };
        session
            .respond(request.allow().expect("expected an allow"))
            .await
            .expect("expected the answer to land");

        let mut ended = false;
        while let Some(event) = turn.recv().await {
            ended = event.is_terminal();
        }
        assert!(ended, "expected the turn to end once answered");
    }

    /// A turn's first event says which turn the vendor thinks it is running, and the session's own
    /// handle is read from the snapshot instead of riding the stream.
    #[tokio::test]
    async fn a_turn_starts_with_its_own_identity_and_leaves_session_facts_to_the_snapshot() {
        let harness = FakeHarness::new().without_approvals();
        let host = host();
        let session = harness
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");

        let first = turn.recv().await.expect("expected an event");
        assert_eq!(
            first.kind,
            EventKind::TurnStarted {
                native_turn_id: String::from("fake-turn-1")
            }
        );
        assert_eq!(first.turn_id.as_str(), "turn-1");
        assert_eq!(first.attempt, crate::AttemptId::default());

        let snapshot = session.snapshot();
        assert_eq!(snapshot.ids.native_session_id, "fake-session-1");
        assert_eq!(
            snapshot
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>(),
            vec!["review"]
        );
    }

    /// A host that has not started a turn still needs to know what the session can be set to.
    #[tokio::test]
    async fn the_configuration_catalog_is_readable_before_any_turn_runs() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("chat-1"))
            .await
            .expect("expected a session");

        let catalog = session.snapshot().catalog.clone();
        assert_eq!(catalog.len(), 2, "received {:?}", catalog.options());
        assert!(
            catalog
                .option(&ConfigurationOptionId::new("model"))
                .is_some_and(|option| option.resettable)
        );
        assert!(
            catalog
                .option(&ConfigurationOptionId::new("fake-mode"))
                .is_some_and(|option| !option.resettable),
            "expected the fake to expose an option it cannot reset"
        );
    }

    #[tokio::test]
    async fn a_harness_without_approvals_completes_on_its_own() {
        let harness = FakeHarness::new().without_approvals();
        let host = host();
        let session = harness
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");

        let mut last = None;
        while let Some(event) = turn.recv().await {
            last = Some(event.kind);
        }
        assert_eq!(last, Some(EventKind::Completed));
    }

    /// Answering a question tells the agent something; it authorises nothing. A turn stopped at
    /// one must not be answerable through the approval surface, and no broker may answer it.
    #[tokio::test]
    async fn a_question_round_trips_with_its_own_ids_and_grants_nothing() {
        use crate::interaction::{
            Answer, AnswerValue, QuestionId, QuestionOptionId, QuestionResponse,
        };

        let session = FakeHarness::new()
            .asking_a_question()
            .open_session(&host(), OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "ship it"))
            .await
            .expect("expected a turn");

        let mut asked = None;
        while let Some(event) = turn.recv().await {
            if let EventKind::QuestionAsked { request } = event.kind {
                asked = Some(request);
                break;
            }
        }
        let asked = asked.expect("expected the turn to stop at its question");
        assert_eq!(asked.questions.len(), 2);
        assert!(!asked.interaction.kind.grants_authority());

        // A required question left unanswered is refused before anything reaches the vendor.
        let incomplete = QuestionResponse::new(asked.interaction.id.clone(), Vec::new());
        assert!(session.answer(incomplete).await.is_err());

        session
            .answer(QuestionResponse::new(
                asked.interaction.id.clone(),
                vec![
                    Answer::new(QuestionId::new("branch"), AnswerValue::text("next")),
                    Answer::new(
                        QuestionId::new("run-tests"),
                        AnswerValue::chosen(QuestionOptionId::new("yes")),
                    ),
                ],
            ))
            .await
            .expect("expected the answers to land");

        let mut resolved = None;
        let mut ended = false;
        while let Some(event) = turn.recv().await {
            ended = event.is_terminal();
            if let EventKind::QuestionResolved { outcome, .. } = event.kind {
                resolved = Some(outcome);
            }
        }
        assert!(ended, "expected the turn to end once answered");
        let Some(crate::interaction::QuestionOutcome::Answered { answers }) = resolved else {
            panic!("expected an answered outcome, received {resolved:?}");
        };
        assert_eq!(answers.len(), 2);
    }

    /// An approval that a person chose reaches the host's audit trail carrying how far the choice
    /// reached, not just which option id won.
    #[tokio::test]
    async fn a_resolved_approval_carries_the_reach_of_the_option_that_won() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "ship it"))
            .await
            .expect("expected a turn");

        let mut asked = None;
        while let Some(event) = turn.recv().await {
            if let EventKind::ApprovalRequested { request } = event.kind {
                asked = Some(request);
                break;
            }
        }
        let asked = asked.expect("expected an approval");
        session
            .respond(
                asked
                    .respond("allow-always", crate::DecisionSource::User)
                    .expect("expected the standing option to be answerable"),
            )
            .await
            .expect("expected the answer to land");

        let mut decision = None;
        while let Some(event) = turn.recv().await {
            if let EventKind::ApprovalResolved { decision: what, .. } = event.kind {
                decision = Some(what);
            }
        }
        let decision = decision.expect("expected a resolution");
        assert_eq!(decision.option_id, "allow-always");
        assert_eq!(decision.scope, Some(PermissionScope::Session));
        assert!(decision.policy_changing);
        assert!(decision.is_standing());
    }

    /// The narrow choice is preferred, so a broker or a bare `allow()` never writes a standing rule
    /// just because the vendor offered one.
    #[tokio::test]
    async fn the_automatic_allow_takes_the_one_time_option_over_the_session_wide_one() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "ship it"))
            .await
            .expect("expected a turn");

        while let Some(event) = turn.recv().await {
            if let EventKind::ApprovalRequested { request } = event.kind {
                assert_eq!(
                    request.allow().expect("expected an allow").option_id,
                    "allow"
                );
                return;
            }
        }
        panic!("expected an approval");
    }

    /// Settings change outside a turn, and the state a host reads afterwards is the state it sees.
    #[tokio::test]
    async fn a_session_is_reconfigured_between_turns_and_the_snapshot_says_so() {
        let session = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("chat-1"))
            .await
            .expect("expected a session");

        let outcome = session
            .configure(
                ConfigurationPatch::new()
                    .model(ConfigurationChange::Set(String::from("fast")))
                    .level(ConfigurationChange::Set(PermissionLevel::Default))
                    .routing(ConfigurationChange::Set(ApprovalRouting::User)),
            )
            .await
            .expect("expected the patch to land");

        assert!(outcome.is_complete(), "received {outcome:?}");
        assert_eq!(outcome.rollback, Rollback::NotNeeded);
        assert_eq!(
            session.snapshot().configuration.accepted.model.as_deref(),
            Some("fast")
        );
        // An accepted command-line option is not proof of a vendor-observed model.
        assert_eq!(session.snapshot().configuration.observed.model, None);
    }

    /// A vendor with no reset must say so rather than report a success it did not have — and it
    /// must not claim the part that did land was rolled back when it was not.
    #[tokio::test]
    async fn a_partially_applied_patch_is_reported_as_partial_rather_than_as_a_transaction() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("chat-1"))
            .await
            .expect("expected a session");

        let outcome = session
            .configure(
                ConfigurationPatch::new()
                    .model(ConfigurationChange::Set(String::from("fast")))
                    .effort(ConfigurationChange::Reset),
            )
            .await
            .expect("expected an outcome rather than a failure");

        assert!(outcome.is_partial(), "received {outcome:?}");
        assert!(!outcome.is_complete());
        assert_eq!(outcome.rollback, Rollback::NotAttempted);
        assert_eq!(
            outcome.rejected.first().map(|rejected| &rejected.reason),
            Some(&crate::configuration::SettingRejection::ResetNotSupported)
        );
        assert_eq!(
            outcome.applied,
            vec![ConfigurationOptionId::new("model")],
            "expected the part that landed to be named"
        );
    }

    /// An option the vendor does not have is refused by name rather than silently dropped.
    #[tokio::test]
    async fn an_unknown_native_option_is_refused_without_taking_the_known_ones_with_it() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("chat-1"))
            .await
            .expect("expected a session");

        let outcome = session
            .configure(
                ConfigurationPatch::new()
                    .model(ConfigurationChange::Set(String::from("fast")))
                    .native(
                        ConfigurationOptionId::new("invented"),
                        ConfigurationChange::Set(crate::ConfigurationValue::Boolean(true)),
                    ),
            )
            .await
            .expect("expected an outcome");

        assert!(outcome.is_partial());
        assert_eq!(
            outcome.rejected.first().map(|rejected| &rejected.reason),
            Some(&crate::configuration::SettingRejection::UnknownOption)
        );
        assert_eq!(
            session.snapshot().configuration.accepted.model.as_deref(),
            Some("fast"),
            "expected the known option to have landed anyway"
        );
    }

    #[tokio::test]
    async fn rejected_changes_do_not_mutate_the_fake_sessions_accepted_configuration() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        session
            .configure(ConfigurationPatch::new().native(
                ConfigurationOptionId::new(FAKE_NATIVE_OPTION),
                ConfigurationChange::Set(crate::ConfigurationValue::Boolean(true)),
            ))
            .await
            .expect("expected the native setting to land");

        let outcome = session
            .configure(
                ConfigurationPatch::new()
                    .model(ConfigurationChange::Set(String::from("fast")))
                    .level(ConfigurationChange::Reset)
                    .native(
                        ConfigurationOptionId::new(FAKE_NATIVE_OPTION),
                        ConfigurationChange::Reset,
                    )
                    .native(
                        ConfigurationOptionId::new("invented"),
                        ConfigurationChange::Set(crate::ConfigurationValue::Boolean(false)),
                    ),
            )
            .await
            .expect("expected a partial outcome");

        assert_eq!(
            outcome.applied,
            vec![ConfigurationOptionId::new("model")],
            "expected only the accepted model change"
        );
        assert_eq!(outcome.state.accepted.model.as_deref(), Some("fast"));
        assert_eq!(
            outcome
                .state
                .accepted
                .native
                .get(&ConfigurationOptionId::new(FAKE_NATIVE_OPTION)),
            Some(&crate::ConfigurationValue::Boolean(true)),
            "expected the rejected reset to preserve the accepted native value"
        );
        assert!(
            !outcome
                .state
                .accepted
                .native
                .contains_key(&ConfigurationOptionId::new("invented")),
            "expected the rejected option to stay out of accepted configuration"
        );
    }

    /// The bound on the turn channel is the whole point of the bound: a host that stops reading
    /// stops the vendor. What must not stop with it is the host's way back out — `cancel`,
    /// `close` and `respond` all take `pending`, so a `steer` parked on the full channel while
    /// still holding that lock takes the escape hatch down with it.
    ///
    /// Four is exactly what `start_turn` emits before it stops at its approval, so the channel is
    /// full the moment it returns and nothing has read a single event.
    #[tokio::test(start_paused = true)]
    async fn a_steer_parked_on_a_full_turn_does_not_hold_the_lock_a_close_needs() {
        use crate::host::Limits;
        use crate::session::{CloseReason, Session, SteerOutcome, SteerRejection};
        use std::time::Duration;

        let host = HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd("/workspace")
            .client_info("test-host", "0.0.0")
            .limits(Limits {
                turn_channel_capacity: 4,
                ..Limits::default()
            })
            .build()
            .expect("expected a context");
        let session: Arc<dyn Session> = Arc::from(
            FakeHarness::new()
                .open_session(&host, OpenSession::new("chat-1"))
                .await
                .expect("expected a session"),
        );
        let _turn = session
            .start_turn(TurnRequest::new("turn-1", "ship it"))
            .await
            .expect("expected a turn");

        let steering = Arc::clone(&session);
        tokio::spawn(async move { steering.steer(steer("and also run the linter")).await });
        yield_twice().await;

        let closing = Arc::clone(&session);
        tokio::spawn(async move { closing.close(CloseReason::Shutdown).await });
        yield_twice().await;

        // Both are parked on the full channel by now. `close` took the pending turn before it
        // parked, so this answers from an empty slot rather than waiting on a lock nobody holds.
        let outcome = tokio::time::timeout(
            Duration::from_millis(50),
            session.steer(steer("and the formatter")),
        )
        .await
        .expect("expected a steer to answer while the turn channel is full, received one that never returned");

        assert_eq!(
            outcome.expect("expected the steer to be answered"),
            SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted,
            }
        );
    }

    fn steer(input: &str) -> crate::session::Steer {
        crate::session::Steer {
            turn_id: crate::event::TurnId::new("turn-1"),
            native_turn_id: String::from("fake-turn-1"),
            input: String::from(input),
        }
    }

    /// Once to hand the spawned task the thread, once more to let it reach its park.
    async fn yield_twice() {
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
    }
}
