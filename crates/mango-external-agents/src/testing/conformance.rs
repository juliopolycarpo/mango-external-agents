//! The contract every harness must pass, as a runnable suite.
//!
//! A harness crate calls [`run`] against its own [`Harness`] and a [`HostContext`] whose launcher
//! replays a captured fixture. What is checked here is what a host is entitled to assume — not
//! what any one vendor happens to do — so the checks are about shape and ordering: a turn ends
//! exactly once, every event names its turn, an approval can be answered, a cancelled turn still
//! completes, closing twice is not an error, and every capability a harness did not declare
//! refuses as [`Error::NotSupported`] rather than misbehaving.
//!
//! The suite holds a descriptor to its word in both directions. A capability a harness did not
//! declare must refuse; a capability it **did** declare must be exercised by the fixture it is run
//! against, or the check fails rather than skipping. See [`Outcome::Skipped`] for which absences
//! are still honest skips.

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
    ///
    /// A skip is also not available for a capability the harness **declared**. A descriptor is a
    /// promise a host plans against, so "it advertises approvals and this fixture raised none" is
    /// a [`Self::Failed`], not a skip: the alternative is an advertised capability whose only
    /// evidence is a check that never ran. What stays a skip is a limit of the *suite* or of the
    /// vendor's own shape — no refusable option was offered, or the question asked is one no
    /// automated suite may answer on a person's behalf.
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
    // Opened before the first turn and judged after it, so this check costs no turn of its own.
    // A suite that started one would make every harness's recorded fixture one turn short, which
    // is a cost the suite has no business imposing to observe something a turn already produces.
    let watching = check_session_state(session.as_ref(), &options, &mut report);
    check_turn(session.as_ref(), &options, &mut report).await;
    check_session_update(session.as_ref(), watching, &options, &mut report).await;
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
        approval_outcome(
            answered.as_ref(),
            refusal,
            session.capabilities().has(Capability::InteractiveApprovals),
        ),
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
    report.record(
        "every structure the turn opened, it closed",
        structure_outcome(&events),
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
/// option this suite is willing to pick. The two are not the same verdict. A vendor that offered
/// no refusable option is a shape this suite cannot drive, so it is skipped. A harness that
/// **declared** [`Capability::InteractiveApprovals`] and then raised nothing is a failure: its
/// descriptor told the host to plan for an approval, and a skip would leave that promise with no
/// evidence behind it at all.
fn approval_outcome(
    asked: Option<&crate::permission::PermissionRequest>,
    refusal: Option<crate::error::Result<()>>,
    declared: bool,
) -> Outcome {
    let Some(request) = asked else {
        return if declared {
            Outcome::Failed(String::from(
                "expected a harness declaring interactive approvals to raise one on this turn, \
                 received a turn that raised none: a declared capability needs a fixture that \
                 exercises it, because a skip here would be the only evidence for it",
            ))
        } else {
            Outcome::Skipped(String::from(
                "this harness asked for no approval on this turn",
            ))
        };
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
    report.record(
        "a cancelled turn closes what it opened",
        structure_outcome(&events),
    );
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
/// harness whose turn never terminates.
///
/// Past the capability guard there is no skip left for the harness's own behaviour. A harness that
/// declares questions and asks none on this turn **fails**: the declaration is what a host builds
/// a prompt surface for, and accepting a skip would mean the capability's only evidence is a check
/// that never ran. The two skips that remain are the suite's own limits — a turn it could not
/// start, and a question no automated suite may answer for a person.
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
        let received = if collected.is_err() {
            format!("no question within {:?}", options.turn_timeout)
        } else {
            String::from("a turn that asked none")
        };
        report.record(
            NAME,
            Outcome::Failed(format!(
                "expected a harness declaring questions to ask one on this turn, received \
                 {received}: a declared capability needs a fixture that exercises it, because a \
                 skip here would be the only evidence for it"
            )),
        );
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

/// Session state is readable before any turn has run.
///
/// The check that would have caught the old shape: a harness carrying its session facts on the
/// turn stream has nothing to answer here until somebody starts a turn.
///
/// Returns the subscription [`check_session_update`] judges, opened here because subscribing is
/// reading — a subscription opened after the turn could not tell a harness that published nothing
/// from one that published before anybody was listening.
fn check_session_state(
    session: &dyn Session,
    options: &Options,
    report: &mut Report,
) -> WatchedSession {
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

    WatchedSession {
        // Read here, not where the check is judged: `current()` answers with the live value, so a
        // revision read after the turn would already be the one the turn published, and the check
        // would be comparing a change against itself.
        opened_at: snapshot.revision,
        subscription: session.subscribe(),
    }
}

/// A subscription and the revision it was opened at.
struct WatchedSession {
    opened_at: crate::state::SessionRevision,
    subscription: crate::state::SessionSubscription,
}

/// Whatever a session published while a turn ran reached the subscriber that was listening.
///
/// Deliberately conditional, and worth saying why. There is no way for a harness to tell a host
/// about a session fact except through [`SessionState`](crate::SessionState) — `Session::snapshot`
/// reads from it and nothing else — so "did it publish" and "did a subscriber hear" cannot come
/// apart by accident. What this check is for is the case where they *have*: a harness holding more
/// than one state, or replacing the one it handed out, publishes into something nobody is
/// listening to, and the revision a host reads moves while the subscription stays silent.
///
/// A harness with nothing session-scoped to say during a turn is **skipped**, not failed. Codex
/// announces no command catalog and its thread id is settled at open, so a plain turn changes
/// nothing about the session — and failing it for that would be the suite requiring a vendor
/// surface that does not exist.
async fn check_session_update(
    session: &dyn Session,
    watching: WatchedSession,
    options: &Options,
    report: &mut Report,
) {
    const NAME: &str = "a session update reaches a subscriber";

    let WatchedSession {
        opened_at,
        mut subscription,
    } = watching;
    let published = session.snapshot().revision;
    if published <= opened_at {
        report.record(
            NAME,
            Outcome::Skipped(String::from(
                "this harness published no session change while a turn ran",
            )),
        );
        return;
    }

    let outcome = match tokio::time::timeout(options.turn_timeout, subscription.changed()).await {
        Ok(Some(seen)) if seen.revision >= published => Outcome::Passed,
        Ok(Some(seen)) => Outcome::Failed(format!(
            "expected the subscriber to reach revision {published}, received {}",
            seen.revision
        )),
        Ok(None) => Outcome::Failed(String::from(
            "expected a session update while a turn ran, received a dropped subscription",
        )),
        Err(_) => Outcome::Failed(format!(
            "expected the session change at revision {published} to reach a subscriber within \
             {:?}, received none — a harness publishing into a state nobody holds leaves every \
             subscriber waiting",
            options.turn_timeout
        )),
    };
    report.record(NAME, outcome);
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
        Err(error) if matches!(error.cause(), Error::NotSupported { capability: found } if *found == capability)
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

/// Whether every structure the turn opened reached an end before its terminal.
///
/// The failure this catches is a host rendering something nobody will ever stop: an activity is a
/// control with a running state, a reasoning phase is a block a host keeps open until told
/// otherwise, and an approval or a question is a **dialog somebody is looking at**. A turn that
/// ends owing any of them leaves a reloaded transcript permanently mid-work, or a prompt with no
/// buttons that do anything. Every terminal path owes them equally — a completion, a cancel and an
/// error are all the end of the turn, which is why the cancelled turn is held to this too.
///
/// An activity update or completion for a call nobody started is the same defect from the other
/// side: the host has nothing to apply it to, so it either invents a row or drops the frame.
///
/// Interactions are checked in **one** direction only. A resolution for an ask a host never saw is
/// deliberate in more than one harness — a question this library refuses on the host's behalf is
/// resolved without ever being asked, so that a host has an auditable record of a refusal it was
/// right not to be shown. There is no dialog to leave open in that case, which is what this check
/// is about.
fn structure_outcome(events: &[AgentEvent]) -> Outcome {
    let mut open: Vec<&str> = Vec::new();
    let mut waiting: Vec<&str> = Vec::new();
    let mut reasoning_open = false;
    let mut failures = Vec::new();
    for event in events {
        match &event.kind {
            EventKind::ActivityStarted { call_id, .. } => {
                if open.contains(&call_id.as_str()) {
                    failures.push(format!("activity {call_id} was started twice"));
                }
                open.push(call_id);
            }
            EventKind::ActivityUpdated { call_id, .. } => {
                if !open.contains(&call_id.as_str()) {
                    failures.push(format!("activity {call_id} was updated but never started"));
                }
            }
            EventKind::ActivityCompleted { call_id, .. } => {
                match open.iter().position(|id| id == call_id) {
                    Some(index) => {
                        open.remove(index);
                    }
                    None => {
                        failures.push(format!(
                            "activity {call_id} was completed but never started"
                        ));
                    }
                }
            }
            EventKind::ApprovalRequested { request } => waiting.push(request.id().as_str()),
            EventKind::QuestionAsked { request } => {
                waiting.push(request.interaction.id.as_str());
            }
            EventKind::ApprovalResolved { interaction_id, .. }
            | EventKind::QuestionResolved { interaction_id, .. } => {
                if let Some(index) = waiting.iter().position(|id| *id == interaction_id.as_str()) {
                    waiting.remove(index);
                }
            }
            EventKind::ReasoningStarted => {
                if std::mem::replace(&mut reasoning_open, true) {
                    failures.push(String::from("a reasoning phase was started inside another"));
                }
            }
            EventKind::ReasoningEnded if !std::mem::replace(&mut reasoning_open, false) => {
                failures.push(String::from("a reasoning phase ended without starting"));
            }
            _ => {}
        }
    }
    if !open.is_empty() {
        failures.push(format!("the turn ended with {open:?} still running"));
    }
    if !waiting.is_empty() {
        failures.push(format!(
            "the turn ended with {waiting:?} still waiting for an answer"
        ));
    }
    if reasoning_open {
        failures.push(String::from("the turn ended mid-reasoning"));
    }
    outcome_for(failures)
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
    use crate::event::EventKind;
    use crate::host::HostContext;
    use crate::interaction::QuestionForm;
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

    /// A harness with nothing session-scoped to say during a turn is conformant, and the check
    /// says so rather than failing it for a vendor surface it does not have. Codex is the real
    /// case: no command catalog, and a thread id settled at open.
    #[tokio::test]
    async fn a_harness_that_publishes_nothing_during_a_turn_is_skipped_rather_than_failed() {
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

        report.assert_passed();
        assert!(
            report
                .skipped()
                .iter()
                .any(|check| check.name == "a session update reaches a subscriber"),
            "expected the subscription check to be skipped, received {:?}",
            report.skipped()
        );
    }

    /// And a harness that does publish has it reach a subscriber, which is the half the skip above
    /// cannot stand in for.
    #[tokio::test]
    async fn a_harness_that_publishes_during_a_turn_reaches_its_subscriber() {
        let report = run(&FakeHarness::new(), &host(), Options::default()).await;

        report.assert_passed();
        assert!(
            report.checks.iter().any(|check| {
                check.name == "a session update reaches a subscriber"
                    && check.outcome == Outcome::Passed
            }),
            "expected the subscription check to pass, received {:?}",
            report.checks
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

    /// A harness that declares no questions never reaches the question turn at all.
    ///
    /// The cheapest of the three outcomes, and the only one that is still a skip once the harness
    /// has been held to its own descriptor: nothing was promised, so nothing is owed.
    #[tokio::test]
    async fn a_harness_that_declares_no_questions_skips_the_question_check_at_the_guard() {
        let report = run(&FakeHarness::new(), &host(), Options::default()).await;

        report.assert_passed();
        let skipped = report.skipped();
        assert!(
            skipped.iter().any(|check| {
                check.name == "a question round-trips with the vendor's own ids"
                    && matches!(&check.outcome, Outcome::Skipped(why) if why.contains("does not declare questions"))
            }),
            "expected the question check to skip at the capability guard, received {skipped:?}"
        );
    }

    /// The rule that makes a declaration mean something: advertise it, and the suite requires it.
    ///
    /// Without it, the cheapest way to make this suite green on a capability a harness cannot
    /// really do is to declare it and ship a fixture that never exercises it — the check skips,
    /// the report has no failures, and the only evidence for the advertised capability is a check
    /// that never ran. A host reading that descriptor builds an approval prompt and a question
    /// prompt it will never be able to use.
    #[tokio::test]
    async fn a_declared_interaction_the_fixture_never_raises_fails_rather_than_skipping() {
        let report = run(
            &FakeHarness::new().advertising_an_interaction_it_never_raises(),
            &host(),
            Options {
                // The declared question never arrives, so this bounds the wait for an absence.
                turn_timeout: std::time::Duration::from_millis(200),
                ..Options::default()
            },
        )
        .await;

        for name in [
            "an approval can be answered",
            "a question round-trips with the vendor's own ids",
        ] {
            let check = report
                .checks
                .iter()
                .find(|check| check.name == name)
                .unwrap_or_else(|| {
                    panic!(
                        "expected {name:?} to have run, received {:#?}",
                        report.checks
                    )
                });
            let Outcome::Failed(message) = &check.outcome else {
                panic!(
                    "expected {name:?} to fail for a capability this harness declared and never \
                     exercised, received {:?}",
                    check.outcome
                );
            };
            assert!(
                message.contains("a declared capability needs a fixture that exercises it"),
                "expected {name:?} to name the declaration as the reason, received {message:?}"
            );
        }

        assert!(
            report
                .skipped()
                .iter()
                .all(|check| check.name != "an approval can be answered"
                    && check.name != "a question round-trips with the vendor's own ids"),
            "expected neither interaction check to be recorded as a skip, received {:?}",
            report.skipped()
        );
    }

    /// The other half of the same rule: a harness that declares nothing keeps its skips.
    ///
    /// Turning every absence into a failure would be the opposite mistake — a harness whose vendor
    /// has no approval surface is not broken, and Claude Code is the real case.
    #[tokio::test]
    async fn a_harness_that_declares_neither_interaction_keeps_both_skips() {
        let report = run(
            &FakeHarness::new().without_approvals(),
            &host(),
            Options::default(),
        )
        .await;

        report.assert_passed();
        let skipped: Vec<&'static str> = report
            .skipped()
            .into_iter()
            .map(|check| check.name)
            .collect();
        assert!(
            skipped.contains(&"an approval can be answered")
                && skipped.contains(&"a question round-trips with the vendor's own ids"),
            "expected both interaction checks to skip for a harness that declares neither, \
             received {skipped:?}"
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

    /// Four ways a turn can leave a host rendering something that never resolves, and the one
    /// shape that is fine. Written against the helper rather than a harness because a harness that
    /// produced any of these would be the bug this check exists to name.
    #[test]
    fn a_turn_that_ends_owing_a_structure_fails_the_check() {
        let started = |call_id: &str| EventKind::ActivityStarted {
            call_id: String::from(call_id),
            activity: crate::event::Activity::new(
                "Bash",
                crate::event::ActivityKind::Command,
                "ls",
            ),
        };
        let completed = |call_id: &str| EventKind::ActivityCompleted {
            call_id: String::from(call_id),
            result: crate::event::ActivityResult::new(crate::event::ActivityStatus::Completed),
        };

        assert_eq!(
            super::structure_outcome(&events(vec![
                started("call-1"),
                completed("call-1"),
                EventKind::ReasoningStarted,
                EventKind::ReasoningEnded,
                EventKind::Completed,
            ])),
            Outcome::Passed
        );

        // A refusal resolves an ask the host was never shown, on purpose: it is the auditable
        // record of a question this library declined to put to anybody. There is no dialog to
        // leave open, so it is not a failure.
        assert_eq!(
            super::structure_outcome(&events(vec![
                EventKind::QuestionResolved {
                    interaction_id: crate::interaction::InteractionId::new("never-asked"),
                    outcome: crate::interaction::QuestionOutcome::Refused {
                        reason: crate::interaction::UnsupportedQuestion::ArbitraryForm,
                    },
                },
                EventKind::Completed,
            ])),
            Outcome::Passed
        );

        for (case, kinds) in [
            (
                "an activity left running",
                vec![started("call-1"), EventKind::Completed],
            ),
            (
                "a completion for a call nobody started",
                vec![completed("call-9"), EventKind::Completed],
            ),
            (
                "an update for a call nobody started",
                vec![
                    EventKind::ActivityUpdated {
                        call_id: String::from("call-9"),
                        update: crate::event::ActivityUpdate::new().with_detail("still going"),
                    },
                    EventKind::Completed,
                ],
            ),
            (
                "a turn that ended mid-reasoning",
                vec![EventKind::ReasoningStarted, EventKind::Completed],
            ),
            (
                "a question left waiting for an answer",
                vec![
                    EventKind::QuestionAsked {
                        request: crate::interaction::QuestionRequest::new(
                            crate::interaction::Interaction::new(
                                crate::interaction::InteractionId::new("ask-1"),
                                crate::interaction::InteractionKind::Question,
                                crate::event::SessionId::new("session-1"),
                                std::time::SystemTime::UNIX_EPOCH,
                            ),
                            vec![crate::interaction::Question::new(
                                crate::interaction::QuestionId::new("branch"),
                                "which branch?",
                                QuestionForm::FreeText { placeholder: None },
                            )],
                        ),
                    },
                    EventKind::Completed,
                ],
            ),
            (
                "a reasoning phase that ended without starting",
                vec![EventKind::ReasoningEnded, EventKind::Completed],
            ),
        ] {
            assert!(
                matches!(super::structure_outcome(&events(kinds)), Outcome::Failed(_)),
                "expected {case} to fail the structure check"
            );
        }
    }

    /// Stamped events, for a check that only reads their kinds.
    fn events(kinds: Vec<EventKind>) -> Vec<crate::event::AgentEvent> {
        kinds
            .into_iter()
            .map(|kind| crate::event::AgentEvent {
                session_id: crate::event::SessionId::new("session-1"),
                turn_id: crate::event::TurnId::new("turn-1"),
                attempt: crate::operation::AttemptId::default(),
                at: std::time::SystemTime::UNIX_EPOCH,
                kind,
            })
            .collect()
    }

    #[tokio::test]
    async fn a_report_names_what_failed() {
        let mut report = super::Report::default();
        report.record("something", Outcome::Failed(String::from("it did not")));
        assert!(!report.passed());
        assert_eq!(report.failures().len(), 1);
    }
}
