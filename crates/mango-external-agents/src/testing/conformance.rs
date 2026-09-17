//! The contract every harness must pass, as a runnable suite.
//!
//! A harness crate calls [`run`] against its own [`Harness`] and a [`HostContext`] whose launcher
//! replays a captured fixture. What is checked here is what a host is entitled to assume — not
//! what any one vendor happens to do — so the checks are about shape and ordering: a turn ends
//! exactly once, every event names its turn, an approval can be answered, a cancelled turn still
//! completes, closing twice is not an error, and every capability a harness did not declare
//! refuses as [`Error::NotSupported`] rather than misbehaving.

use std::time::Duration;

use crate::configuration::ConfigurationPatch;
use crate::error::Error;
use crate::event::{AgentEvent, EventKind};
use crate::harness::{Capability, Harness};
use crate::host::HostContext;
use crate::interaction::{
    Answer, AnswerValue, InteractionId, QuestionForm, QuestionRequest, QuestionResponse,
};
use crate::session::{
    Attachment, AttachmentKind, CancelReason, CloseReason, OpenSession, ReviewRequest,
    ReviewTarget, Session, SessionQuery, Steer, TurnRequest,
};

/// How one check went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// It held.
    Passed,
    /// It did not, and this is what happened.
    Failed(String),
    /// It could not be run, and this is why.
    ///
    /// A skip is not a pass. A harness whose fixture never produces an approval cannot prove it
    /// answers one, and saying so is more useful than a green tick.
    Skipped(String),
}

/// One check and how it went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// What was checked.
    pub name: &'static str,
    /// How it went.
    pub outcome: Outcome,
}

/// Everything the suite checked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// Every check, in the order it ran.
    pub checks: Vec<Check>,
}

impl Report {
    /// Every check that failed.
    pub fn failures(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| matches!(check.outcome, Outcome::Failed(_)))
            .collect()
    }

    /// Every check that could not be run.
    pub fn skipped(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| matches!(check.outcome, Outcome::Skipped(_)))
            .collect()
    }

    /// Whether nothing failed.
    pub fn passed(&self) -> bool {
        self.failures().is_empty()
    }

    /// Panics with every failure, for use as a test's last line.
    ///
    /// # Panics
    ///
    /// When any check failed.
    pub fn assert_passed(&self) {
        assert!(
            self.passed(),
            "expected a conformant harness, received {} failing checks: {:#?}",
            self.failures().len(),
            self.failures()
        );
    }

    fn record(&mut self, name: &'static str, outcome: Outcome) {
        self.checks.push(Check { name, outcome });
    }
}

/// How long the suite waits for a harness at each step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The session id to open under.
    pub session_id: String,
    /// What to say to the agent.
    pub prompt: String,
    /// How long one turn may take before the suite gives up on it.
    pub turn_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            session_id: String::from("conformance-session"),
            prompt: String::from("say hello"),
            turn_timeout: Duration::from_secs(30),
        }
    }
}

/// Runs the suite against one harness.
///
/// Never panics and never returns an error: a harness that fails is a [`Report`] full of
/// failures, which is more useful than a backtrace from the first one.
pub async fn run(harness: &dyn Harness, host: &HostContext, options: Options) -> Report {
    let mut report = Report::default();
    check_descriptor(harness, &mut report);
    check_permission_matrix(harness, &mut report);
    check_discovery(harness, host, &mut report).await;

    let session = match harness
        .open_session(host, OpenSession::new(options.session_id.clone()))
        .await
    {
        Ok(session) => {
            report.record("open_session", Outcome::Passed);
            session
        }
        Err(error) => {
            report.record("open_session", Outcome::Failed(error.to_string()));
            return report;
        }
    };

    check_ids(session.as_ref(), &options, &mut report);
    check_session_state(session.as_ref(), &options, &mut report).await;
    check_turn(session.as_ref(), &options, &mut report).await;
    check_questions(session.as_ref(), &options, &mut report).await;
    check_cancelled_turn(session.as_ref(), &options, &mut report).await;
    check_optional_methods(session.as_ref(), &mut report).await;
    check_close(session.as_ref(), &mut report).await;
    report
}

fn check_descriptor(harness: &dyn Harness, report: &mut Report) {
    let descriptor = harness.descriptor();
    let outcome = if descriptor.transports.is_empty() {
        Outcome::Failed(String::from(
            "expected at least one transport kind, received none",
        ))
    } else {
        Outcome::Passed
    };
    report.record("the descriptor declares a transport", outcome);

    let vendor = descriptor.vendor;
    let outcome = if vendor.company.is_empty()
        || !vendor.terms_url.starts_with("https://")
        || !vendor.privacy_url.starts_with("https://")
    {
        Outcome::Failed(format!(
            "expected a company and two https documents, received {vendor:?}"
        ))
    } else {
        Outcome::Passed
    };
    report.record("the descriptor names the vendor and its documents", outcome);
}

fn check_permission_matrix(harness: &dyn Harness, report: &mut Report) {
    let matrix = harness.permission_matrix();
    let mut failures = Vec::new();
    if matrix.cells().len() != 6 {
        failures.push(format!(
            "expected 6 cells, received {}",
            matrix.cells().len()
        ));
    }
    for cell in matrix.cells() {
        if cell.supported == cell.unsupported_reason.is_some() {
            failures.push(format!(
                "expected a reason exactly when unsupported, received {cell:?}"
            ));
        }
        let unattended = cell.level == crate::permission::PermissionLevel::FullAccess
            || cell.routing == crate::permission::ApprovalRouting::AutoReview;
        if cell.unattended != unattended {
            failures.push(format!(
                "expected unattended={unattended}, received {cell:?}"
            ));
        }
    }
    report.record(
        "the permission matrix is complete and consistent",
        outcome_for(failures),
    );
}

async fn check_discovery(harness: &dyn Harness, host: &HostContext, report: &mut Report) {
    match harness.discover(host).await {
        Ok(discovery) => {
            let ceiling = harness.descriptor().capabilities;
            let beyond = discovery.capabilities.beyond(&ceiling);
            let outcome = if beyond.is_empty() {
                Outcome::Passed
            } else {
                Outcome::Failed(format!(
                    "expected capabilities within the descriptor's ceiling, received {beyond:?} beyond it"
                ))
            };
            report.record("discovery stays within the declared ceiling", outcome);
        }
        Err(error) => report.record(
            "discovery stays within the declared ceiling",
            Outcome::Skipped(format!("discovery failed on this machine: {error}")),
        ),
    }
}

fn check_ids(session: &dyn Session, options: &Options, report: &mut Report) {
    let ids = session.ids();
    let mut failures = Vec::new();
    if ids.session_id.as_str() != options.session_id {
        failures.push(format!(
            "expected the host's own session id {:?}, received {:?}",
            options.session_id,
            ids.session_id.as_str()
        ));
    }
    if ids.native_session_id.trim().is_empty() {
        failures.push(String::from(
            "expected a native session id, received an empty one",
        ));
    }
    report.record("the session answers to both ids", outcome_for(failures));
}

async fn check_turn(session: &dyn Session, options: &Options, report: &mut Report) {
    let request = TurnRequest::new("conformance-turn-1", options.prompt.clone());
    let mut turn = match session.start_turn(request).await {
        Ok(turn) => turn,
        Err(error) => {
            report.record("a turn starts", Outcome::Failed(error.to_string()));
            return;
        }
    };
    report.record("a turn starts", Outcome::Passed);

    let mut events = Vec::new();
    let mut answered = None;
    let mut refusal = None;
    let collected = tokio::time::timeout(options.turn_timeout, async {
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            // A turn can stop at either kind of ask, and a turn nobody unblocks never reaches a
            // terminal — so this check would report "no terminal" about a harness that was
            // waiting, politely, for an answer it was never sent.
            if let EventKind::QuestionAsked { request } = &event.kind {
                decline_or_cancel(session, request).await;
            }
            if let EventKind::ApprovalRequested { request } = &event.kind
                && answered.is_none()
            {
                answered = Some(request.clone());
                // Refused, never granted. This runs against a live `Harness` and the host's real
                // launcher: nothing in the signature says the launcher has to be a fake, so
                // someone will point the suite at an installed CLI in a real workspace. A refusal
                // proves the round-trip just as well as a grant, and "approvals are brokered,
                // never auto-answered" is not a rule the conformance suite gets to be the
                // exception to. A vendor that offers no refusal is recorded as skipped below.
                if let Ok(response) = request.deny() {
                    refusal = Some(session.respond(response).await);
                }
            }
            events.push(event);
            if terminal {
                break;
            }
        }
        drain_queued(&mut turn, &mut events);
    })
    .await;

    // Recorded before the turn's own outcome, because it is known either way: a harness that
    // refused the answer is exactly the harness whose turn then never terminates, and reporting
    // only the timeout would hide the reason for it.
    report.record(
        "an approval can be answered",
        approval_outcome(answered.as_ref(), refusal),
    );

    if collected.is_err() {
        report.record(
            "a turn ends exactly once",
            Outcome::Failed(format!(
                "expected the turn to end within {:?}, received {} events and no terminal",
                options.turn_timeout,
                events.len()
            )),
        );
        return;
    }

    report.record("a turn ends exactly once", terminal_outcome(&events));
    report.record(
        "every event names its session and turn",
        stamping_outcome(&events, "conformance-turn-1", session),
    );
}

/// The least this suite can say to one round of questions.
///
/// Declined wherever declining is allowed. A required question cannot be declined, so it is
/// answered with the vendor's own first choice, or with an obviously synthetic string.
///
/// Answering here is not the exception to "approvals are brokered, never auto-answered" that it
/// might look like. That rule is about **authority**, and a question grants none by construction —
/// [`InteractionKind::grants_authority`](crate::InteractionKind::grants_authority) is false for
/// every one of these. The suite already types a prompt at the agent; saying "the first option" to
/// a question it then asks authorises nothing further, and the alternative is a check that can
/// never pass against any harness that marks a question required.
fn minimal_answers(request: &QuestionRequest) -> QuestionResponse {
    QuestionResponse::new(
        request.interaction.id.clone(),
        request
            .questions
            .iter()
            .map(|question| {
                let value = match (&question.form, question.required) {
                    (_, false) => AnswerValue::Declined,
                    (QuestionForm::Choice { options, .. }, true) => {
                        options.first().map_or(AnswerValue::Declined, |option| {
                            AnswerValue::chosen(option.id.clone())
                        })
                    }
                    (QuestionForm::FreeText { .. }, true) => {
                        AnswerValue::text("conformance-suite-placeholder")
                    } // No catch-all: `QuestionForm` is non-exhaustive to everyone else, but in
                      // here a new arm is a compile error, which is right. Deciding what this suite
                      // says to a new kind of question is part of adding one.
                };
                Answer::new(question.id.clone(), value)
            })
            .collect(),
    )
}

/// Answers a question minimally, or cancels the turn when even that is refused.
///
/// Either way the turn reaches a terminal, which is what the checks around this are about: a turn
/// nobody unblocks never ends, and reporting "no terminal" about a harness that was waiting
/// politely for an answer it was never sent would be a check failing for the suite's own reason.
async fn decline_or_cancel(session: &dyn Session, request: &QuestionRequest) {
    let response = minimal_answers(request);
    if request.validate(&response).is_ok() && session.answer(response).await.is_ok() {
        return;
    }
    let _ = session.cancel(CancelReason::Requested).await;
}

/// Whether the approval round-trip held, given the question asked and what answering it returned.
///
/// `None` for the refusal means no answer was ever sent — the vendor asked nothing, or offered no
/// option this suite is willing to pick. Neither is a pass, and neither is a failure of the
/// harness.
fn approval_outcome(
    asked: Option<&crate::permission::PermissionRequest>,
    refusal: Option<crate::error::Result<()>>,
) -> Outcome {
    let Some(request) = asked else {
        return Outcome::Skipped(String::from(
            "this harness asked for no approval on this turn",
        ));
    };
    match refusal {
        Some(Ok(())) => Outcome::Passed,
        Some(Err(error)) => Outcome::Failed(format!(
            "expected the refusal to be accepted, received {error}"
        )),
        None => Outcome::Skipped(format!(
            "this vendor offered no way to refuse: received {:?}",
            request
                .options
                .iter()
                .map(|option| option.effect)
                .collect::<Vec<_>>()
        )),
    }
}

async fn check_cancelled_turn(session: &dyn Session, options: &Options, report: &mut Report) {
    let request = TurnRequest::new("conformance-turn-2", options.prompt.clone());
    let mut turn = match session.start_turn(request).await {
        Ok(turn) => turn,
        Err(error) => {
            report.record(
                "a cancelled turn still completes",
                Outcome::Skipped(format!("a second turn could not be started: {error}")),
            );
            return;
        }
    };

    if let Err(error) = session.cancel(CancelReason::Requested).await {
        report.record(
            "a cancelled turn still completes",
            Outcome::Failed(format!("expected the cancel to land, received {error}")),
        );
        return;
    }

    let mut events = Vec::new();
    let drained = tokio::time::timeout(options.turn_timeout, async {
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            events.push(event);
            if terminal {
                break;
            }
        }
        drain_queued(&mut turn, &mut events);
    })
    .await;

    let outcome = if drained.is_err() {
        Outcome::Failed(format!(
            "expected a cancelled turn to end within {:?}, received {} events and no terminal",
            options.turn_timeout,
            events.len()
        ))
    } else if !events.iter().any(AgentEvent::is_terminal) {
        Outcome::Failed(String::from(
            "expected a cancelled turn to end with a completion or an error, received neither",
        ))
    } else if events
        .iter()
        .any(|event| matches!(event.kind, EventKind::Cancelled { .. }))
        && !matches!(
            events.last().map(|event| &event.kind),
            Some(EventKind::Completed | EventKind::Error { .. })
        )
    {
        Outcome::Failed(String::from(
            "expected the cancellation marker to be followed by a terminal, received it last",
        ))
    } else {
        Outcome::Passed
    };
    report.record("a cancelled turn still completes", outcome);
}

async fn check_optional_methods(session: &dyn Session, report: &mut Report) {
    let capabilities = session.capabilities();
    let mut failures = Vec::new();

    if !capabilities.has(Capability::Steering) {
        let outcome = session
            .steer(Steer {
                turn_id: crate::event::TurnId::new("conformance-turn-1"),
                native_turn_id: String::from("conformance"),
                input: String::from("also this"),
            })
            .await;
        if !refuses_as_unsupported(&outcome, Capability::Steering) {
            failures.push(String::from(
                "expected steering to refuse as unsupported, received something else",
            ));
        }
    }

    if !capabilities.has(Capability::SessionListing) {
        let outcome = session.list_sessions(SessionQuery::default()).await;
        if !refuses_as_unsupported(&outcome, Capability::SessionListing) {
            failures.push(String::from(
                "expected session listing to refuse as unsupported, received something else",
            ));
        }
    }

    if !capabilities.has(Capability::NativeReview) {
        let outcome = session
            .start_review(ReviewRequest {
                turn_id: crate::event::TurnId::new("conformance-review-1"),
                target: ReviewTarget::UncommittedChanges,
            })
            .await
            .map(|_| ());
        if !refuses_as_unsupported(&outcome, Capability::NativeReview) {
            failures.push(String::from(
                "expected a native review to refuse as unsupported, received something else",
            ));
        }
    }

    if !capabilities.has(Capability::AccountUsage) {
        let outcome = session.refresh_account_usage().await;
        if !refuses_as_unsupported(&outcome, Capability::AccountUsage) {
            failures.push(String::from(
                "expected account usage to refuse as unsupported, received something else",
            ));
        }
    }

    if !capabilities.has(Capability::InteractiveApprovals) {
        let outcome = session
            .respond(crate::permission::PermissionResponse::from_user(
                InteractionId::new("conformance-approval-unsupported"),
                "deny",
            ))
            .await;
        if !refuses_as_unsupported(&outcome, Capability::InteractiveApprovals) {
            failures.push(String::from(
                "expected interactive approvals to refuse as unsupported, received something else",
            ));
        }
    }

    if !capabilities.has(Capability::Configuration) {
        let outcome = session
            .start_turn(
                TurnRequest::new("conformance-configuration-unsupported", "say hello")
                    .with_configuration(ConfigurationPatch::new()),
            )
            .await
            .map(|_| ());
        if !refuses_as_unsupported(&outcome, Capability::Configuration) {
            failures.push(String::from(
                "expected per-turn configuration to refuse as unsupported, received something else",
            ));
        }
    }

    if !capabilities.has(Capability::Images) {
        let outcome = session
            .start_turn(
                TurnRequest::new("conformance-image-unsupported", "look at this").with_attachments(
                    vec![Attachment {
                        id: String::from("conformance-image"),
                        name: String::from("image.png"),
                        mime_type: String::from("image/png"),
                        kind: AttachmentKind::Image,
                        bytes: vec![0x89, 0x50],
                    }],
                ),
            )
            .await
            .map(|_| ());
        if !refuses_as_unsupported(&outcome, Capability::Images) {
            failures.push(String::from(
                "expected images to refuse as unsupported, received something else",
            ));
        }
    }

    if !capabilities.has(Capability::Questions) {
        let outcome = session
            .answer(QuestionResponse::new(
                InteractionId::new("conformance-question-unsupported"),
                Vec::new(),
            ))
            .await;
        if !refuses_as_unsupported(&outcome, Capability::Questions) {
            failures.push(String::from(
                "expected questions to refuse as unsupported, received something else",
            ));
        }
    }

    if !capabilities.has(Capability::SessionConfiguration) {
        let outcome = session
            .configure(ConfigurationPatch::new())
            .await
            .map(|_| ());
        if !refuses_as_unsupported(&outcome, Capability::SessionConfiguration) {
            failures.push(String::from(
                "expected mid-session configuration to refuse as unsupported, received something \
                 else",
            ));
        }
    }

    report.record(
        "an undeclared capability refuses rather than misbehaves",
        outcome_for(failures),
    );
}

/// A harness that advertises questions round-trips one with the vendor's own ids.
///
/// The positive case, which the refusal check in [`check_optional_methods`] cannot stand in for: a
/// harness that declared [`Capability::Questions`] and then cannot take an answer is exactly the
/// harness whose turn never terminates. Skipped rather than passed when the harness declares the
/// capability but this turn asked nothing — a skip says more than a green tick.
async fn check_questions(session: &dyn Session, options: &Options, report: &mut Report) {
    const NAME: &str = "a question round-trips with the vendor's own ids";

    if !session.capabilities().has(Capability::Questions) {
        report.record(
            NAME,
            Outcome::Skipped(String::from("this harness does not declare questions")),
        );
        return;
    }

    let mut turn = match session
        .start_turn(TurnRequest::new(
            "conformance-question-turn",
            options.prompt.clone(),
        ))
        .await
    {
        Ok(turn) => turn,
        Err(error) => {
            report.record(
                NAME,
                Outcome::Skipped(format!(
                    "a turn for the question could not be started: {error}"
                )),
            );
            return;
        }
    };

    let mut asked = None;
    let collected = tokio::time::timeout(options.turn_timeout, async {
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            match event.kind {
                EventKind::QuestionAsked { request } => {
                    asked = Some(request);
                    break;
                }
                // A turn that stopped at an approval is a turn that is not going to ask a
                // question, and it will wait for an answer forever. Refuse it — never grant —
                // and stop, rather than spending this check's whole budget on a stream that has
                // already said what it is doing.
                EventKind::ApprovalRequested { request } => {
                    if let Ok(response) = request.deny() {
                        let _ = session.respond(response).await;
                    }
                    break;
                }
                _ => {}
            }
            if terminal {
                break;
            }
        }
    })
    .await;

    let Some(request) = asked else {
        let reason = if collected.is_err() {
            format!("no question arrived within {:?}", options.turn_timeout)
        } else {
            String::from("this harness asked no question on this turn")
        };
        report.record(NAME, Outcome::Skipped(reason));
        let _ = session.cancel(CancelReason::Requested).await;
        return;
    };

    let mut failures = Vec::new();
    if request.interaction.kind.grants_authority() {
        failures.push(String::from(
            "expected a question to grant no authority, received one marked as a permission",
        ));
    }

    let response = minimal_answers(&request);
    // A shape this suite cannot answer at all is a well-formed refusal rather than a broken
    // round-trip, so it is reported as a skip rather than as a failure of the harness.
    if let Err(error) = request.validate(&response) {
        report.record(
            NAME,
            Outcome::Skipped(format!(
                "this harness asked something the suite may not answer for a person: {error}"
            )),
        );
        let _ = session.cancel(CancelReason::Requested).await;
        return;
    }
    if let Err(error) = session.answer(response).await {
        failures.push(format!(
            "expected the answers to be accepted, received {error}"
        ));
    }

    report.record(NAME, outcome_for(failures));
    let _ = session.cancel(CancelReason::Requested).await;
}

/// Session state is readable before any turn has run, and a subscription cannot miss a change.
///
/// The check that would have caught the old shape: a harness carrying its session facts on the
/// turn stream has nothing to answer here until somebody starts a turn.
async fn check_session_state(session: &dyn Session, options: &Options, report: &mut Report) {
    let snapshot = session.snapshot();
    let mut failures = Vec::new();
    if snapshot.ids.session_id.as_str() != options.session_id {
        failures.push(format!(
            "expected the snapshot to name session {:?}, received {:?}",
            options.session_id,
            snapshot.ids.session_id.as_str()
        ));
    }
    if snapshot.transport.was_substituted() {
        failures.push(format!(
            "expected the effective transport to be the requested one, received {:?} for a request \
             of {:?}",
            snapshot.transport.effective, snapshot.transport.requested
        ));
    }
    if !snapshot
        .capabilities
        .within(&crate::harness::DiscoveredCapabilities::all())
    {
        failures.push(String::from(
            "expected session capabilities inside the whole table, received a claim beyond it",
        ));
    }
    report.record(
        "session state is readable before any turn",
        outcome_for(failures),
    );

    // Subscribing is reading, so a change published afterwards cannot fall into a gap between the
    // two. What this proves is that a harness publishes its state through `SessionState` at all:
    // one that mutated a private field would leave the subscriber waiting forever.
    let mut subscription = session.subscribe();
    let before = subscription.current().revision;
    let started = session
        .start_turn(TurnRequest::new(
            "conformance-state-turn",
            options.prompt.clone(),
        ))
        .await;
    let outcome = match started {
        // Only an unstartable turn is a skip. A turn that *did* start and then published nothing is
        // the failure this check exists for — a harness mutating private state instead of
        // publishing through `SessionState` leaves a subscriber waiting forever, and reporting that
        // as a skip would make the one check written to catch it unable to fail.
        Err(error) => Outcome::Skipped(format!(
            "a turn could not be started to observe a session change: {error}"
        )),
        Ok(_turn) => match tokio::time::timeout(options.turn_timeout, subscription.changed()).await
        {
            Ok(Some(seen)) if seen.revision > before => Outcome::Passed,
            Ok(Some(seen)) => Outcome::Failed(format!(
                "expected a later revision than {before}, received {}",
                seen.revision
            )),
            Ok(None) => Outcome::Failed(String::from(
                "expected a session update while a turn ran, received a dropped subscription",
            )),
            Err(_) => Outcome::Failed(format!(
                "expected a session update within {:?} of a turn starting, received none — a \
                 harness that mutates its own state instead of publishing through SessionState \
                 leaves every subscriber waiting",
                options.turn_timeout
            )),
        },
    };
    report.record("a session update reaches a subscriber", outcome);
    let _ = session.cancel(CancelReason::Requested).await;
}

async fn check_close(session: &dyn Session, report: &mut Report) {
    let first = session.close(CloseReason::Requested).await;
    let second = session.close(CloseReason::Requested).await;
    let outcome = match (first, second) {
        (Ok(()), Ok(())) => Outcome::Passed,
        (Err(error), _) => Outcome::Failed(format!("expected a clean close, received {error}")),
        (Ok(()), Err(error)) => Outcome::Failed(format!(
            "expected closing twice to be harmless, received {error}"
        )),
    };
    report.record("closing twice is not an error", outcome);
}

fn refuses_as_unsupported<T>(outcome: &crate::error::Result<T>, capability: Capability) -> bool {
    matches!(
        outcome,
        Err(Error::NotSupported { capability: found }) if *found == capability
    )
}

/// Whatever is already sitting behind the terminal, without waiting for more.
///
/// Breaking at the terminal and stopping there is what made "nothing follows the terminal" a check
/// that cannot fail: the loop never looks. An event already queued behind it is a turn that ended
/// twice, or that kept talking afterwards, and it is exactly what a host would see.
fn drain_queued(turn: &mut crate::stream::TurnStream, events: &mut Vec<AgentEvent>) {
    while let Ok(event) = turn.try_recv() {
        events.push(event);
    }
}

fn terminal_outcome(events: &[AgentEvent]) -> Outcome {
    let terminals = events.iter().filter(|event| event.is_terminal()).count();
    if terminals != 1 {
        return Outcome::Failed(format!(
            "expected exactly one terminal event, received {terminals}"
        ));
    }
    if !events.last().is_some_and(AgentEvent::is_terminal) {
        return Outcome::Failed(String::from(
            "expected the terminal event last, received events after it",
        ));
    }
    Outcome::Passed
}

fn stamping_outcome(events: &[AgentEvent], turn_id: &str, session: &dyn Session) -> Outcome {
    let session_id = session.ids().session_id.clone();
    let attempt = crate::operation::AttemptId::default();
    let mismatched: Vec<String> = events
        .iter()
        .filter(|event| {
            event.turn_id.as_str() != turn_id
                || event.session_id != session_id
                || event.attempt != attempt
        })
        .map(|event| event.operation().to_string())
        .collect();
    if mismatched.is_empty() {
        return Outcome::Passed;
    }
    Outcome::Failed(format!(
        "expected every event to name {session_id}/{turn_id}, received {mismatched:?}"
    ))
}

fn outcome_for(failures: Vec<String>) -> Outcome {
    if failures.is_empty() {
        return Outcome::Passed;
    }
    Outcome::Failed(failures.join("; "))
}

#[cfg(test)]
mod tests {
    use super::{Options, Outcome, run};
    use crate::host::HostContext;
    use crate::testing::{FakeHarness, FakeLauncher};
    use std::sync::Arc;

    fn host() -> HostContext {
        HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd("/workspace")
            .client_info("conformance", "0.0.0")
            .build()
            .expect("expected a context")
    }

    #[tokio::test]
    async fn the_fake_harness_is_conformant() {
        let report = run(&FakeHarness::new(), &host(), Options::default()).await;
        report.assert_passed();
        assert!(
            report.checks.len() >= 8,
            "expected the whole suite to run, received {:?}",
            report.checks
        );
    }

    #[tokio::test]
    async fn a_harness_that_never_asks_skips_the_approval_check_rather_than_passing_it() {
        let report = run(
            &FakeHarness::new().without_approvals(),
            &host(),
            Options::default(),
        )
        .await;

        report.assert_passed();
        let skipped = report.skipped();
        assert!(
            skipped
                .iter()
                .any(|check| check.name == "an approval can be answered"),
            "expected the approval check to be skipped, received {skipped:?}"
        );
    }

    /// The negative fixture for the session-state check. A harness that keeps its state to itself
    /// leaves every subscriber waiting, and before this the suite reported that as a skip — which
    /// `Report::passed` treats as green, making the one check written to catch it unable to fail.
    #[tokio::test]
    async fn a_harness_that_publishes_no_session_state_fails_the_subscription_check() {
        let report = run(
            &FakeHarness::new().without_session_updates(),
            &host(),
            Options {
                // Short, because the whole point is that nothing ever arrives.
                turn_timeout: std::time::Duration::from_millis(50),
                ..Options::default()
            },
        )
        .await;

        let failures = report.failures();
        assert!(
            failures
                .iter()
                .any(|check| check.name == "a session update reaches a subscriber"),
            "expected the subscription check to fail, received {failures:?}"
        );
        assert!(
            report
                .skipped()
                .iter()
                .all(|check| check.name != "a session update reaches a subscriber"),
            "expected the failure not to be reported as a skip"
        );
    }

    /// The positive question case. A harness that declares `Questions` and then cannot take an
    /// answer is the harness whose turn never terminates, and the refusal check cannot see that:
    /// it only ever asks harnesses that declared nothing.
    #[tokio::test]
    async fn a_harness_that_asks_a_question_round_trips_one() {
        let report = run(
            &FakeHarness::new().asking_a_question(),
            &host(),
            Options::default(),
        )
        .await;

        report.assert_passed();
        assert!(
            report.checks.iter().any(|check| {
                check.name == "a question round-trips with the vendor's own ids"
                    && check.outcome == Outcome::Passed
            }),
            "expected the question check to pass, received {:?}",
            report.checks
        );
    }

    /// A harness whose turns stop at an approval instead must not spend the check's whole budget
    /// waiting for a question that is never coming — it says so and moves on.
    #[tokio::test]
    async fn a_harness_that_asks_for_approval_instead_skips_the_question_check_promptly() {
        let report = run(&FakeHarness::new(), &host(), Options::default()).await;

        let skipped = report.skipped();
        assert!(
            skipped
                .iter()
                .any(|check| check.name == "a question round-trips with the vendor's own ids"),
            "expected the question check to be skipped, received {skipped:?}"
        );
    }

    /// A vendor that asks and then will not take the answer has not answered anything. The check
    /// is about the round-trip, so ignoring what `respond` returned made it a check on `deny()`
    /// building an option.
    #[tokio::test]
    async fn an_answer_the_harness_would_not_take_fails_the_approval_check() {
        let options = Options {
            // The rejected answer leaves the fake's turn unfinished, so this bounds the wait
            // rather than sitting out the default half-minute.
            turn_timeout: std::time::Duration::from_millis(200),
            ..Options::default()
        };
        let report = run(&FakeHarness::new().rejecting_answers(), &host(), options).await;

        let failure = report
            .failures()
            .into_iter()
            .find(|check| check.name == "an approval can be answered")
            .map(|check| check.outcome.clone());
        let Some(Outcome::Failed(message)) = failure else {
            panic!(
                "expected the approval check to fail, received {:?}",
                report.checks
            );
        };
        assert!(
            message.contains("vendor failure"),
            "expected a safe vendor failure diagnostic, received {message:?}"
        );
    }

    #[tokio::test]
    async fn a_report_names_what_failed() {
        let mut report = super::Report::default();
        report.record("something", Outcome::Failed(String::from("it did not")));
        assert!(!report.passed());
        assert_eq!(report.failures().len(), 1);
    }
}
