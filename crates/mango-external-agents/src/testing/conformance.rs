//! The contract every harness must pass, as a runnable suite.
//!
//! A harness crate calls [`run`] against its own [`Harness`] and a [`HostContext`] whose launcher
//! replays a captured fixture. What is checked here is what a host is entitled to assume — not
//! what any one vendor happens to do — so the checks are about shape and ordering: a turn ends
//! exactly once, every event names its turn, an approval can be answered, a cancelled turn still
//! completes, closing twice is not an error, and every capability a harness did not declare
//! refuses as [`Error::NotSupported`] rather than misbehaving.

use std::time::Duration;

use crate::error::Error;
use crate::event::{AgentEvent, EventKind};
use crate::harness::{Capability, Harness};
use crate::host::HostContext;
use crate::session::{
    CancelReason, CloseReason, OpenSession, ReviewRequest, ReviewTarget, Session, SessionQuery,
    Steer, TurnRequest,
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
    check_turn(session.as_ref(), &options, &mut report).await;
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
                .map(|option| option.kind)
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
    let capabilities = session.info().capabilities;
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

    report.record(
        "an undeclared capability refuses rather than misbehaves",
        outcome_for(failures),
    );
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
    let mismatched: Vec<String> = events
        .iter()
        .filter(|event| event.turn_id.as_str() != turn_id || event.session_id != session_id)
        .map(|event| format!("{:?}/{:?}", event.session_id, event.turn_id))
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
            message.contains("would not take the answer"),
            "expected the harness's own refusal in the message, received {message:?}"
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
