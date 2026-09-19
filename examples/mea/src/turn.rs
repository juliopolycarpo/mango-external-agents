//! Drives one `mea turn` lifecycle after a harness has opened its session.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use mango_external_agents::event::{AgentEvent, EventKind};
use mango_external_agents::{
    ActivityContent, BrokerDecision, CancelReason, CloseReason, Error, InteractionId,
    PermissionBroker, PermissionRequest, PermissionResponse, QuestionRequest, QuestionResponse,
    Result, Session, TurnRequest, TurnStream,
};

/// How long one turn is given before it is cancelled.
const TURN_DEADLINE: Duration = Duration::from_secs(300);

/// Runs one turn, prints its events and closes the session.
///
/// # Errors
///
/// Returns an error reported while starting or driving the turn, or while closing its session.
///
/// # Example
///
/// ```text
/// mea turn --harness codex "summarise this repository"
/// ```
#[cfg(test)]
pub async fn run(session: &dyn Session, request: TurnRequest) -> Result<()> {
    run_with_format(session, request, false).await
}

/// Runs and closes a turn, optionally emitting NDJSON. Example: `mea turn --json "hello"`.
pub async fn run_with_format(
    session: &dyn Session,
    request: TurnRequest,
    json: bool,
) -> Result<()> {
    let input = crate::ask::TerminalInput::new();
    let broker = crate::terminal::TerminalBroker::new(input.clone());
    run_with_host(session, request, json, &broker, &input).await
}

#[cfg(test)]
async fn run_with_broker(
    session: &dyn Session,
    request: TurnRequest,
    json: bool,
    broker: &dyn PermissionBroker,
) -> Result<()> {
    let input = crate::ask::TerminalInput::new();
    run_with_host(session, request, json, broker, &input).await
}

/// The same, with both host-facing surfaces injected: who decides an approval, and who types an
/// answer. Separate because they are separate authorities — one grants, the other does not.
async fn run_with_host(
    session: &dyn Session,
    request: TurnRequest,
    json: bool,
    broker: &dyn PermissionBroker,
    asker: &dyn crate::ask::QuestionInput,
) -> Result<()> {
    let outcome = async {
        let mut stream = session.start_turn(request).await?;
        match tokio::time::timeout(
            TURN_DEADLINE,
            print_turn(session, &mut stream, json, broker, asker),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                session.cancel(CancelReason::Timeout).await?;
                Err(Error::Timeout {
                    operation: String::from("mea turn"),
                    after: TURN_DEADLINE,
                })
            }
        }
    }
    .await;
    let closed = session.close(CloseReason::Requested).await;
    outcome.and(closed)
}

/// Prints events and keeps terminal input concurrent with the stream that may withdraw it.
async fn print_turn(
    session: &dyn Session,
    turn: &mut TurnStream,
    json: bool,
    broker: &dyn PermissionBroker,
    asker: &dyn crate::ask::QuestionInput,
) -> Result<()> {
    let mut pending: Option<PendingPrompt<'_>> = None;
    let mut queued = VecDeque::new();
    let mut deferred_prompt_error = None;
    loop {
        let next = match pending.as_mut() {
            Some(prompt) => tokio::select! {
                biased;
                event = turn.recv() => NextTurnItem::Event(event),
                outcome = &mut prompt.outcome => NextTurnItem::Prompt(outcome),
            },
            None => NextTurnItem::Event(turn.recv().await),
        };
        match next {
            NextTurnItem::Prompt(outcome) => {
                pending = None;
                if let Some(error) = outcome?.submit(session).await? {
                    deferred_prompt_error = Some(error);
                }
                start_next_prompt(&mut pending, &mut queued, asker, broker);
            }
            NextTurnItem::Event(None) => {
                return deferred_prompt_error.map_or(Ok(()), |deferred| Err(deferred.error));
            }
            NextTurnItem::Event(Some(event)) => {
                if event.is_terminal() {
                    pending = None;
                    queued.clear();
                    deferred_prompt_error = None;
                } else if let Some(resolved) = ResolvedPrompt::from_event(&event.kind) {
                    if pending
                        .as_ref()
                        .is_some_and(|prompt| prompt.matches(resolved))
                    {
                        pending = None;
                    }
                    queued.retain(|prompt| !prompt.matches(resolved));
                    if deferred_prompt_error
                        .as_ref()
                        .is_some_and(|deferred| deferred.identity.matches(resolved))
                    {
                        deferred_prompt_error = None;
                    }
                }
                if let Some(prompt) = print_event(&event, json)? {
                    queued.push_back(prompt);
                }
                start_next_prompt(&mut pending, &mut queued, asker, broker);
            }
        }
    }
}

enum NextTurnItem {
    Event(Option<AgentEvent>),
    Prompt(Result<PromptOutcome>),
}

enum PromptRequest {
    Question(QuestionRequest),
    Approval(PermissionRequest),
}

impl PromptRequest {
    fn matches(&self, resolved: ResolvedPrompt<'_>) -> bool {
        match (self, resolved) {
            (Self::Question(request), ResolvedPrompt::Question(interaction_id)) => {
                request.interaction.id == *interaction_id
            }
            (Self::Approval(request), ResolvedPrompt::Approval(interaction_id)) => {
                request.interaction.id == *interaction_id
            }
            _ => false,
        }
    }
}

#[derive(Clone, Copy)]
enum ResolvedPrompt<'a> {
    Question(&'a InteractionId),
    Approval(&'a InteractionId),
}

impl<'a> ResolvedPrompt<'a> {
    fn from_event(event: &'a EventKind) -> Option<Self> {
        match event {
            EventKind::QuestionResolved { interaction_id, .. } => {
                Some(Self::Question(interaction_id))
            }
            EventKind::ApprovalResolved { interaction_id, .. } => {
                Some(Self::Approval(interaction_id))
            }
            _ => None,
        }
    }
}

enum PromptIdentity {
    Question(InteractionId),
    Approval(InteractionId),
}

impl PromptIdentity {
    fn matches(&self, resolved: ResolvedPrompt<'_>) -> bool {
        match (self, resolved) {
            (Self::Question(interaction_id), ResolvedPrompt::Question(resolved)) => {
                interaction_id == resolved
            }
            (Self::Approval(interaction_id), ResolvedPrompt::Approval(resolved)) => {
                interaction_id == resolved
            }
            _ => false,
        }
    }
}

struct DeferredPromptError {
    identity: PromptIdentity,
    error: Error,
}

/// The session reports these exact shapes after the interaction has already ended.
fn is_stale_prompt_error(identity: &PromptIdentity, error: &Error) -> bool {
    let (closed_subject, stale, expired, inactive) = match identity {
        PromptIdentity::Question(_) => (
            "question",
            "a question round this session is still waiting on",
            "a question round whose deadline has not expired",
            "an active turn that owns this question round",
        ),
        PromptIdentity::Approval(_) => (
            "approval",
            "an approval this session is still waiting on",
            "an approval whose deadline has not expired",
            "an active turn that owns this approval",
        ),
    };
    match error {
        Error::Closed { subject } => *subject == closed_subject,
        Error::Protocol { expected, .. } => {
            expected == stale || expected == expired || expected == inactive
        }
        _ => false,
    }
}

enum PromptOutcome {
    Question(QuestionResponse),
    Approval(PermissionResponse),
}

impl PromptOutcome {
    /// Submits one response, retaining a stale refusal until the stream confirms its resolution.
    async fn submit(self, session: &dyn Session) -> Result<Option<DeferredPromptError>> {
        let (identity, result) = match self {
            Self::Question(answer) => (
                PromptIdentity::Question(answer.interaction_id.clone()),
                session.answer(answer).await,
            ),
            Self::Approval(response) => (
                PromptIdentity::Approval(response.interaction_id.clone()),
                session.respond(response).await,
            ),
        };
        let Err(error) = result else {
            return Ok(None);
        };
        if !is_stale_prompt_error(&identity, &error) {
            return Err(error);
        }
        Ok(Some(DeferredPromptError { identity, error }))
    }
}

struct PendingPrompt<'a> {
    prompt: PromptRequest,
    outcome: Pin<Box<dyn Future<Output = Result<PromptOutcome>> + Send + 'a>>,
}

impl<'a> PendingPrompt<'a> {
    fn new(
        prompt: PromptRequest,
        asker: &'a dyn crate::ask::QuestionInput,
        broker: &'a dyn PermissionBroker,
    ) -> Self {
        let outcome: Pin<Box<dyn Future<Output = Result<PromptOutcome>> + Send + 'a>> =
            match &prompt {
                PromptRequest::Question(request) => {
                    let request = request.clone();
                    Box::pin(async move {
                        Ok(PromptOutcome::Question(
                            crate::ask::answer_round(asker, request).await,
                        ))
                    })
                }
                PromptRequest::Approval(request) => {
                    let request = request.clone();
                    Box::pin(async move {
                        let decision = broker.decide(&request).await;
                        Ok(PromptOutcome::Approval(permission_response(
                            &request, decision,
                        )?))
                    })
                }
            };
        Self { prompt, outcome }
    }

    fn matches(&self, resolved: ResolvedPrompt<'_>) -> bool {
        self.prompt.matches(resolved)
    }
}

/// Starts the oldest interaction only after the preceding terminal prompt has settled.
fn start_next_prompt<'a>(
    pending: &mut Option<PendingPrompt<'a>>,
    queued: &mut VecDeque<PromptRequest>,
    asker: &'a dyn crate::ask::QuestionInput,
    broker: &'a dyn PermissionBroker,
) {
    if pending.is_some() {
        return;
    }
    if let Some(prompt) = queued.pop_front() {
        *pending = Some(PendingPrompt::new(prompt, asker, broker));
    }
}

/// Turns a broker decision into one of the response options the vendor offered.
fn permission_response(
    request: &PermissionRequest,
    decision: BrokerDecision,
) -> Result<PermissionResponse> {
    match decision {
        BrokerDecision::Allow => match request.allow() {
            Ok(allow) => Ok(allow),
            Err(_) => request.deny(),
        },
        BrokerDecision::Deny { .. } | BrokerDecision::Ask => request.deny(),
    }
}

/// Renders one event and returns an interaction that should wait for terminal input.
fn print_event(event: &AgentEvent, json: bool) -> Result<Option<PromptRequest>> {
    if json {
        println!("{}", serde_json::json!(event));
    }
    match &event.kind {
        EventKind::TextDelta { text } if !json => print!("{text}"),
        // A question is not a permission, and this is the branch that proves it: nothing here
        // consults the broker, and answering grants the agent nothing.
        EventKind::QuestionAsked { request } => {
            if !json {
                println!("{}", serde_json::json!(event.kind));
            }
            return Ok(Some(PromptRequest::Question(request.clone())));
        }
        EventKind::ActivityStarted { activity, .. } if !json => {
            println!("{}", serde_json::json!(event.kind));
            print_content(activity.content.as_ref());
        }
        EventKind::ActivityUpdated { update, .. } if !json => {
            println!("{}", serde_json::json!(event.kind));
            print_content(update.content.as_ref());
        }
        EventKind::ActivityCompleted { result, .. } if !json => {
            println!("{}", serde_json::json!(event.kind));
            print_content(result.content.as_ref());
        }
        EventKind::ApprovalRequested { request } => {
            if !json {
                println!("{}", serde_json::json!(event.kind));
            }
            return Ok(Some(PromptRequest::Approval(request.clone())));
        }
        EventKind::Error { error } => {
            if !json {
                println!("{}", serde_json::json!(event.kind));
            }
            return Err(Error::Vendor(error.clone()));
        }
        other if !json => println!("{}", serde_json::json!(other)),
        _ => {}
    }
    Ok(None)
}

/// Renders structured content as the structure it is, beside the JSON the line above printed.
///
/// The JSON already carries every field; this exists so a person running the smoke tool can see at
/// a glance that a plan arrived as steps and a diff as files, which is the whole reason the
/// harnesses stopped flattening them into a sentence.
fn print_content(content: Option<&ActivityContent>) {
    match content {
        Some(ActivityContent::Plan { steps }) => {
            for step in steps {
                println!("    [{}] {}", step.status, step.title);
            }
        }
        Some(ActivityContent::Diff { files }) => {
            for file in files {
                let counts = match (file.added_lines, file.removed_lines) {
                    (Some(added), Some(removed)) => format!(" (+{added} -{removed})"),
                    _ => String::new(),
                };
                println!("    {}{counts}", file.path);
            }
        }
        Some(ActivityContent::Output { text }) => println!("    {text}"),
        Some(ActivityContent::Empty) => {}
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    use mango_external_agents::testing::{FakeHarness, FakeLauncher};
    use mango_external_agents::{
        ActivityKind, ApprovalDecision, BrokerDecision, CancelReason, CloseReason, DecisionSource,
        Error, ErrorCode, EventKind, EventSink, Harness, HostContext, Interaction, InteractionId,
        InteractionKind, OpenSession, PermissionBroker, PermissionEffect, PermissionOption,
        PermissionRequest, PermissionResponse, Question, QuestionForm, QuestionId, QuestionOutcome,
        QuestionRequest, QuestionResponse, Result, Session, SessionState, SystemClock, TurnRequest,
        TurnStream, VendorError,
    };

    use super::run;

    fn host() -> HostContext {
        HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd("/workspace")
            .client_info("mea-test", "0.0.0")
            .build()
            .expect("expected a host")
    }

    #[tokio::test(start_paused = true)]
    async fn refuses_an_approval_and_closes_the_session() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("mea-test"))
            .await
            .expect("expected a fake session");

        tokio::time::timeout(
            Duration::from_millis(100),
            run(
                session.as_ref(),
                TurnRequest::new("mea-turn-1", "write a file"),
            ),
        )
        .await
        .expect("expected mea to answer the approval instead of waiting")
        .expect("expected the turn to finish");

        let error = session
            .start_turn(TurnRequest::new("mea-turn-2", "try again"))
            .await
            .expect_err("expected the session to have been closed");
        assert!(matches!(error, Error::Closed { subject: "session" }));
    }

    /// The branch a question takes, and the reason it is a separate test from the approval above:
    /// a turn that stops to ask something is a turn no broker may answer, so nothing but the
    /// question path can unblock it. Before this, `mea` printed the question and waited for a
    /// deadline that ends the turn.
    #[tokio::test(start_paused = true)]
    async fn answers_a_question_without_consulting_the_broker_and_closes_the_session() {
        let broker = RecordingBroker::default();
        let session = FakeHarness::new()
            .asking_a_question()
            .open_session(&host(), OpenSession::new("mea-question"))
            .await
            .expect("expected a fake session");

        tokio::time::timeout(
            Duration::from_millis(100),
            super::run_with_host(
                session.as_ref(),
                TurnRequest::new("mea-turn-1", "which branch?"),
                true,
                &broker,
                &NobodyTyping,
            ),
        )
        .await
        .expect("expected mea to answer the question instead of waiting")
        .expect("expected the turn to finish");

        assert!(
            !broker.consulted.load(Ordering::SeqCst),
            "a question grants no authority, so no broker may be asked one"
        );
    }

    /// Nobody at the keyboard, injected rather than inferred from the process's own stdin: a run
    /// from an interactive shell would otherwise block a thread in `read_line` and fail this test
    /// for a reason that has nothing to do with what it checks.
    struct NobodyTyping;

    #[async_trait::async_trait]
    impl crate::ask::QuestionInput for NobodyTyping {
        async fn read_line(&self, _prompt: &str) -> Option<String> {
            None
        }
    }

    /// A broker that records whether anything ever asked it to decide.
    #[derive(Default)]
    struct RecordingBroker {
        consulted: AtomicBool,
    }

    #[async_trait::async_trait]
    impl mango_external_agents::PermissionBroker for RecordingBroker {
        async fn decide(
            &self,
            _request: &mango_external_agents::PermissionRequest,
        ) -> mango_external_agents::BrokerDecision {
            self.consulted.store(true, Ordering::SeqCst);
            mango_external_agents::BrokerDecision::Ask
        }
    }

    /// A server that withdraws a question while the terminal is still waiting for a line.
    ///
    /// The withdrawal and terminal are queued before the host starts reading. The old turn loop
    /// consumed the ask and then waited on the keyboard, so this fake left its terminal events
    /// unread until the five-minute deadline.
    struct WithdrawnQuestionSession {
        inner: Box<dyn Session>,
        answers: AtomicUsize,
        input_started: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl Session for WithdrawnQuestionSession {
        fn state(&self) -> &SessionState {
            self.inner.state()
        }

        async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
            let (sink, events) = EventSink::new(
                self.ids().session_id,
                request.turn_id.clone(),
                request.attempt,
                Arc::new(SystemClock),
                3,
            );
            let question = QuestionRequest::new(
                Interaction::new(
                    InteractionId::new("withdrawn-question"),
                    InteractionKind::Question,
                    self.ids().session_id,
                    SystemTime::now() + Duration::from_secs(60),
                ),
                vec![
                    Question::new(
                        QuestionId::new("branch"),
                        "Which branch?",
                        QuestionForm::FreeText { placeholder: None },
                    )
                    .required(),
                ],
            );
            let interaction_id = question.interaction.id.clone();
            sink.emit(EventKind::QuestionAsked { request: question })
                .await?;
            let input_started = Arc::clone(&self.input_started);
            let resolving = sink.clone();
            tokio::spawn(async move {
                input_started.notified().await;
                let _ = resolving
                    .emit(EventKind::QuestionResolved {
                        interaction_id,
                        outcome: QuestionOutcome::Expired,
                    })
                    .await;
                let _ = resolving.complete().await;
            });
            Ok(TurnStream::accepted(
                request.turn_id,
                request.attempt,
                "withdrawn-question-turn",
                events,
            ))
        }

        async fn respond(&self, response: PermissionResponse) -> Result<()> {
            self.inner.respond(response).await
        }

        async fn answer(&self, _response: QuestionResponse) -> Result<()> {
            self.answers.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn cancel(&self, reason: CancelReason) -> Result<()> {
            self.inner.cancel(reason).await
        }

        async fn close(&self, reason: CloseReason) -> Result<()> {
            self.inner.close(reason).await
        }
    }

    /// An interactive keyboard with a line held back forever.
    struct WithheldInput {
        started: Arc<tokio::sync::Notify>,
        cancelled: Arc<AtomicBool>,
    }

    struct PromptCancellation(Arc<AtomicBool>);

    impl Drop for PromptCancellation {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[async_trait::async_trait]
    impl crate::ask::QuestionInput for WithheldInput {
        async fn read_line(&self, _prompt: &str) -> Option<String> {
            self.started.notify_one();
            let _cancelled = PromptCancellation(Arc::clone(&self.cancelled));
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_withdrawn_question_stops_its_keyboard_prompt_and_finishes_the_turn() {
        let input_started = Arc::new(tokio::sync::Notify::new());
        let input_cancelled = Arc::new(AtomicBool::new(false));
        let inner = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("withdrawn-question"))
            .await
            .expect("expected a fake session");
        let session = WithdrawnQuestionSession {
            inner,
            answers: AtomicUsize::new(0),
            input_started: Arc::clone(&input_started),
        };
        let input = WithheldInput {
            started: input_started,
            cancelled: Arc::clone(&input_cancelled),
        };

        tokio::time::timeout(
            Duration::from_millis(100),
            super::run_with_host(
                &session,
                TurnRequest::new("withdrawn-question-turn", "Which branch?"),
                true,
                &RecordingBroker::default(),
                &input,
            ),
        )
        .await
        .expect("expected a withdrawn question to end the turn while keyboard input is pending")
        .expect("expected the withdrawn question turn to finish");

        assert_eq!(
            session.answers.load(Ordering::SeqCst),
            0,
            "the withdrawn round must not send an answer after its prompt stops"
        );
        assert!(
            input_cancelled.load(Ordering::Acquire),
            "expected the withdrawn round to cancel its pending keyboard prompt"
        );
    }

    /// A server that resolves a round while rejecting the host's just-completed answer.
    struct ExpiredAnswerSession {
        inner: Box<dyn Session>,
        question: tokio::sync::Mutex<Option<(InteractionId, EventSink)>>,
    }

    #[async_trait::async_trait]
    impl Session for ExpiredAnswerSession {
        fn state(&self) -> &SessionState {
            self.inner.state()
        }

        async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
            let session_id = self.ids().session_id;
            let (sink, events) = EventSink::new(
                session_id.clone(),
                request.turn_id.clone(),
                request.attempt,
                Arc::new(SystemClock),
                3,
            );
            let question = question_request("expired-answer", session_id);
            let interaction_id = question.interaction.id.clone();
            *self.question.lock().await = Some((interaction_id, sink.clone()));
            sink.emit(EventKind::QuestionAsked { request: question })
                .await?;
            Ok(TurnStream::accepted(
                request.turn_id,
                request.attempt,
                "expired-answer-turn",
                events,
            ))
        }

        async fn respond(&self, response: PermissionResponse) -> Result<()> {
            self.inner.respond(response).await
        }

        async fn answer(&self, response: QuestionResponse) -> Result<()> {
            let Some((interaction_id, sink)) = self.question.lock().await.take() else {
                return Err(Error::Protocol {
                    expected: String::from("a question round this session is still waiting on"),
                    received: response.interaction_id.to_string(),
                });
            };
            sink.emit(EventKind::QuestionResolved {
                interaction_id,
                outcome: QuestionOutcome::Expired,
            })
            .await?;
            sink.complete().await?;
            Err(Error::Protocol {
                expected: String::from("a question round whose deadline has not expired"),
                received: response.interaction_id.to_string(),
            })
        }

        async fn cancel(&self, reason: CancelReason) -> Result<()> {
            self.inner.cancel(reason).await
        }

        async fn close(&self, reason: CloseReason) -> Result<()> {
            self.inner.close(reason).await
        }
    }

    #[tokio::test]
    async fn a_stale_question_answer_waits_for_its_resolution_instead_of_failing_the_turn() {
        let inner = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("expired-answer"))
            .await
            .expect("expected a fake session");
        let session = ExpiredAnswerSession {
            inner,
            question: tokio::sync::Mutex::new(None),
        };

        super::run_with_host(
            &session,
            TurnRequest::new("expired-answer-turn", "Which branch?"),
            true,
            &RecordingBroker::default(),
            &NobodyTyping,
        )
        .await
        .expect("expected the question resolution to settle the stale answer refusal");
    }

    #[test]
    fn only_a_settled_prompts_documented_refusals_are_deferred() {
        let question = super::PromptIdentity::Question(InteractionId::new("question"));
        for expected in [
            "a question round this session is still waiting on",
            "a question round whose deadline has not expired",
            "an active turn that owns this question round",
        ] {
            assert!(
                super::is_stale_prompt_error(
                    &question,
                    &Error::Protocol {
                        expected: String::from(expected),
                        received: String::from("question"),
                    },
                ),
                "expected the settled question refusal {expected:?} to be deferred"
            );
        }
        assert!(super::is_stale_prompt_error(
            &question,
            &Error::Closed {
                subject: "question"
            }
        ));

        let approval = super::PromptIdentity::Approval(InteractionId::new("approval"));
        for expected in [
            "an approval this session is still waiting on",
            "an approval whose deadline has not expired",
            "an active turn that owns this approval",
        ] {
            assert!(
                super::is_stale_prompt_error(
                    &approval,
                    &Error::Protocol {
                        expected: String::from(expected),
                        received: String::from("approval"),
                    },
                ),
                "expected the settled approval refusal {expected:?} to be deferred"
            );
        }
        assert!(super::is_stale_prompt_error(
            &approval,
            &Error::Closed {
                subject: "approval"
            }
        ));

        assert!(
            !super::is_stale_prompt_error(
                &question,
                &Error::Protocol {
                    expected: String::from("a malformed answer"),
                    received: String::from("question"),
                },
            ),
            "an unrelated submission failure must remain fatal"
        );
    }

    /// A server that withdraws an approval while the broker is waiting for terminal input.
    struct WithdrawnApprovalSession {
        inner: Box<dyn Session>,
        broker_started: Arc<tokio::sync::Notify>,
        responses: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Session for WithdrawnApprovalSession {
        fn state(&self) -> &SessionState {
            self.inner.state()
        }

        async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
            let session_id = self.ids().session_id;
            let (sink, events) = EventSink::new(
                session_id.clone(),
                request.turn_id.clone(),
                request.attempt,
                Arc::new(SystemClock),
                3,
            );
            let approval = PermissionRequest::new(
                Interaction::new(
                    InteractionId::new("withdrawn-approval"),
                    InteractionKind::Permission,
                    session_id,
                    SystemTime::now() + Duration::from_secs(60),
                ),
                ActivityKind::Command,
                "change a file",
                vec![PermissionOption::new("deny", PermissionEffect::Reject)],
            );
            let interaction_id = approval.interaction.id.clone();
            sink.emit(EventKind::ApprovalRequested { request: approval })
                .await?;
            let broker_started = Arc::clone(&self.broker_started);
            let resolving = sink.clone();
            tokio::spawn(async move {
                broker_started.notified().await;
                let _ = resolving
                    .emit(EventKind::ApprovalResolved {
                        interaction_id,
                        decision: ApprovalDecision::unresolved(
                            "withdrawn",
                            DecisionSource::Cancelled,
                        ),
                    })
                    .await;
                let _ = resolving.complete().await;
            });
            Ok(TurnStream::accepted(
                request.turn_id,
                request.attempt,
                "withdrawn-approval-turn",
                events,
            ))
        }

        async fn respond(&self, _response: PermissionResponse) -> Result<()> {
            self.responses.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn cancel(&self, reason: CancelReason) -> Result<()> {
            self.inner.cancel(reason).await
        }

        async fn close(&self, reason: CloseReason) -> Result<()> {
            self.inner.close(reason).await
        }
    }

    /// A broker with a terminal line held back forever.
    struct WithheldBroker {
        started: Arc<tokio::sync::Notify>,
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait::async_trait]
    impl PermissionBroker for WithheldBroker {
        async fn decide(
            &self,
            _request: &mango_external_agents::PermissionRequest,
        ) -> BrokerDecision {
            self.started.notify_one();
            let _cancelled = PromptCancellation(Arc::clone(&self.cancelled));
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_withdrawn_approval_stops_its_broker_prompt_and_finishes_the_turn() {
        let broker_started = Arc::new(tokio::sync::Notify::new());
        let broker_cancelled = Arc::new(AtomicBool::new(false));
        let inner = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("withdrawn-approval"))
            .await
            .expect("expected a fake session");
        let session = WithdrawnApprovalSession {
            inner,
            broker_started: Arc::clone(&broker_started),
            responses: AtomicUsize::new(0),
        };
        let broker = WithheldBroker {
            started: broker_started,
            cancelled: Arc::clone(&broker_cancelled),
        };

        tokio::time::timeout(
            Duration::from_millis(100),
            super::run_with_host(
                &session,
                TurnRequest::new("withdrawn-approval-turn", "change a file"),
                true,
                &broker,
                &NobodyTyping,
            ),
        )
        .await
        .expect("expected a withdrawn approval to end the turn while broker input is pending")
        .expect("expected the withdrawn approval turn to finish");

        assert!(
            broker_cancelled.load(Ordering::Acquire),
            "expected the withdrawn approval to cancel its pending broker prompt"
        );
        assert_eq!(
            session.responses.load(Ordering::SeqCst),
            0,
            "the withdrawn approval must not send a stale response"
        );
    }

    /// A server that expires an approval as the host sends its response.
    struct ExpiredApprovalSession {
        inner: Box<dyn Session>,
        approval: tokio::sync::Mutex<Option<(InteractionId, EventSink)>>,
    }

    #[async_trait::async_trait]
    impl Session for ExpiredApprovalSession {
        fn state(&self) -> &SessionState {
            self.inner.state()
        }

        async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
            let session_id = self.ids().session_id;
            let (sink, events) = EventSink::new(
                session_id.clone(),
                request.turn_id.clone(),
                request.attempt,
                Arc::new(SystemClock),
                3,
            );
            let approval = PermissionRequest::new(
                Interaction::new(
                    InteractionId::new("expired-approval"),
                    InteractionKind::Permission,
                    session_id,
                    SystemTime::now() + Duration::from_secs(60),
                ),
                ActivityKind::Command,
                "change a file",
                vec![PermissionOption::new("deny", PermissionEffect::Reject)],
            );
            let interaction_id = approval.interaction.id.clone();
            *self.approval.lock().await = Some((interaction_id, sink.clone()));
            sink.emit(EventKind::ApprovalRequested { request: approval })
                .await?;
            Ok(TurnStream::accepted(
                request.turn_id,
                request.attempt,
                "expired-approval-turn",
                events,
            ))
        }

        async fn respond(&self, response: PermissionResponse) -> Result<()> {
            let Some((interaction_id, sink)) = self.approval.lock().await.take() else {
                return Err(Error::Protocol {
                    expected: String::from("an approval this session is still waiting on"),
                    received: response.interaction_id.to_string(),
                });
            };
            sink.emit(EventKind::ApprovalResolved {
                interaction_id,
                decision: ApprovalDecision::unresolved("expired", DecisionSource::Expired),
            })
            .await?;
            sink.complete().await?;
            Err(Error::Protocol {
                expected: String::from("an approval whose deadline has not expired"),
                received: response.interaction_id.to_string(),
            })
        }

        async fn cancel(&self, reason: CancelReason) -> Result<()> {
            self.inner.cancel(reason).await
        }

        async fn close(&self, reason: CloseReason) -> Result<()> {
            self.inner.close(reason).await
        }
    }

    #[tokio::test]
    async fn a_stale_approval_response_waits_for_its_resolution_instead_of_failing_the_turn() {
        let inner = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("expired-approval"))
            .await
            .expect("expected a fake session");
        let session = ExpiredApprovalSession {
            inner,
            approval: tokio::sync::Mutex::new(None),
        };

        super::run_with_host(
            &session,
            TurnRequest::new("expired-approval-turn", "change a file"),
            true,
            &RecordingBroker::default(),
            &NobodyTyping,
        )
        .await
        .expect("expected the approval resolution to settle the stale response refusal");
    }

    /// A server that withdraws its first question, then asks another and waits for its answer.
    struct NextQuestionSession {
        inner: Box<dyn Session>,
        input_started: Arc<tokio::sync::Notify>,
        second: tokio::sync::Mutex<Option<(InteractionId, EventSink)>>,
        answers: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Session for NextQuestionSession {
        fn state(&self) -> &SessionState {
            self.inner.state()
        }

        async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
            let session_id = self.ids().session_id;
            let (sink, events) = EventSink::new(
                session_id.clone(),
                request.turn_id.clone(),
                request.attempt,
                Arc::new(SystemClock),
                4,
            );
            let first = question_request("first-question", session_id.clone());
            let second = question_request("second-question", session_id);
            let second_id = second.interaction.id.clone();
            *self.second.lock().await = Some((second_id.clone(), sink.clone()));
            sink.emit(EventKind::QuestionAsked { request: first })
                .await?;

            let input_started = Arc::clone(&self.input_started);
            let resolving = sink.clone();
            tokio::spawn(async move {
                input_started.notified().await;
                let _ = resolving
                    .emit(EventKind::QuestionResolved {
                        interaction_id: InteractionId::new("first-question"),
                        outcome: QuestionOutcome::Expired,
                    })
                    .await;
                let _ = resolving
                    .emit(EventKind::QuestionAsked { request: second })
                    .await;
            });

            Ok(TurnStream::accepted(
                request.turn_id,
                request.attempt,
                "next-question-turn",
                events,
            ))
        }

        async fn respond(&self, response: PermissionResponse) -> Result<()> {
            self.inner.respond(response).await
        }

        async fn answer(&self, response: QuestionResponse) -> Result<()> {
            let Some((interaction_id, sink)) = self.second.lock().await.take() else {
                return Err(Error::Protocol {
                    expected: String::from("the second question's answer"),
                    received: String::from("an answer after the question ended"),
                });
            };
            if response.interaction_id != interaction_id {
                return Err(Error::Protocol {
                    expected: String::from("the second question's answer"),
                    received: String::from("the withdrawn first question's answer"),
                });
            }
            self.answers.fetch_add(1, Ordering::SeqCst);
            sink.emit(EventKind::QuestionResolved {
                interaction_id,
                outcome: QuestionOutcome::Answered {
                    answers: response.answers,
                },
            })
            .await?;
            sink.complete().await
        }

        async fn cancel(&self, reason: CancelReason) -> Result<()> {
            self.inner.cancel(reason).await
        }

        async fn close(&self, reason: CloseReason) -> Result<()> {
            self.inner.close(reason).await
        }
    }

    /// The first line never arrives, while the line for the next prompt is ready once the first
    /// prompt has been withdrawn.
    struct WithdrawnThenTyping {
        input_started: Arc<tokio::sync::Notify>,
        first_cancelled: Arc<AtomicBool>,
        reads: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl crate::ask::QuestionInput for WithdrawnThenTyping {
        async fn read_line(&self, _prompt: &str) -> Option<String> {
            match self.reads.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    self.input_started.notify_one();
                    let _cancelled = PromptCancellation(Arc::clone(&self.first_cancelled));
                    std::future::pending().await
                }
                1 => {
                    assert!(
                        self.first_cancelled.load(Ordering::Acquire),
                        "the next prompt began before the old input had been cancelled"
                    );
                    Some(String::from("second answer"))
                }
                _ => panic!("expected one prompt for each question round"),
            }
        }
    }

    #[tokio::test]
    async fn a_withdrawn_prompt_cannot_take_the_next_rounds_answer() {
        let input_started = Arc::new(tokio::sync::Notify::new());
        let first_cancelled = Arc::new(AtomicBool::new(false));
        let inner = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("next-question"))
            .await
            .expect("expected a fake session");
        let session = NextQuestionSession {
            inner,
            input_started: Arc::clone(&input_started),
            second: tokio::sync::Mutex::new(None),
            answers: AtomicUsize::new(0),
        };
        let input = WithdrawnThenTyping {
            input_started,
            first_cancelled: Arc::clone(&first_cancelled),
            reads: AtomicUsize::new(0),
        };

        tokio::time::timeout(
            Duration::from_millis(100),
            super::run_with_host(
                &session,
                TurnRequest::new("next-question-turn", "Which branch?"),
                true,
                &RecordingBroker::default(),
                &input,
            ),
        )
        .await
        .expect("expected the next question to remain answerable after the first was withdrawn")
        .expect("expected the second question to finish the turn");

        assert!(
            first_cancelled.load(Ordering::Acquire),
            "expected the first prompt to stop before the second answer was read"
        );
        assert_eq!(
            session.answers.load(Ordering::SeqCst),
            1,
            "expected exactly the second question to receive an answer"
        );
    }

    fn question_request(id: &str, session_id: mango_external_agents::SessionId) -> QuestionRequest {
        QuestionRequest::new(
            Interaction::new(
                InteractionId::new(id),
                InteractionKind::Question,
                session_id,
                SystemTime::now() + Duration::from_secs(60),
            ),
            vec![
                Question::new(
                    QuestionId::new("branch"),
                    "Which branch?",
                    QuestionForm::FreeText { placeholder: None },
                )
                .required(),
            ],
        )
    }

    #[tokio::test]
    async fn json_turn_finishes_and_closes_the_session() {
        let session = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("json-test"))
            .await
            .expect("fake session");
        super::run_with_format(
            session.as_ref(),
            TurnRequest::new("json-turn", "hello"),
            true,
        )
        .await
        .expect("JSON turn completes");
        assert!(matches!(
            session
                .start_turn(TurnRequest::new("closed", "hello"))
                .await,
            Err(Error::Closed { subject: "session" })
        ));
    }

    #[tokio::test]
    async fn approval_events_consult_the_terminal_broker_before_answering() {
        let session = FakeHarness::new()
            .open_session(&host(), OpenSession::new("approval-test"))
            .await
            .expect("fake session");
        let broker = mango_external_agents::testing::RecordingBroker::new(
            mango_external_agents::BrokerDecision::Allow,
        );
        super::run_with_broker(
            session.as_ref(),
            TurnRequest::new("approval-turn", "hello"),
            true,
            &broker,
        )
        .await
        .expect("approved turn completes");
        assert_eq!(
            broker.requests().len(),
            1,
            "expected the event consumer to ask its broker once before responding"
        );
    }

    struct VendorFailureSession {
        inner: Box<dyn Session>,
        closed: AtomicBool,
    }

    #[async_trait::async_trait]
    impl Session for VendorFailureSession {
        fn state(&self) -> &SessionState {
            self.inner.state()
        }

        async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
            let (sink, events) = EventSink::new(
                self.ids().session_id.clone(),
                request.turn_id.clone(),
                request.attempt,
                Arc::new(SystemClock),
                1,
            );
            sink.fail(VendorError::new(
                ErrorCode::from_static("scripted-vendor-failure"),
                "the scripted vendor failure",
            ))
            .await?;
            Ok(TurnStream::accepted(
                request.turn_id,
                request.attempt,
                "failed-turn",
                events,
            ))
        }

        async fn respond(&self, response: PermissionResponse) -> Result<()> {
            self.inner.respond(response).await
        }

        async fn cancel(&self, reason: CancelReason) -> Result<()> {
            self.inner.cancel(reason).await
        }

        async fn close(&self, reason: CloseReason) -> Result<()> {
            self.closed.store(true, Ordering::Release);
            self.inner.close(reason).await
        }
    }

    #[tokio::test]
    async fn returns_a_vendor_failure_event_as_an_error_and_closes_the_session() {
        let inner = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("mea-test"))
            .await
            .expect("expected a fake session");
        let session = VendorFailureSession {
            inner,
            closed: AtomicBool::new(false),
        };

        let error = run(
            &session,
            TurnRequest::new("mea-turn-1", "this turn must fail"),
        )
        .await
        .expect_err("expected the scripted vendor failure");

        assert!(
            matches!(
                &error,
                Error::Vendor(error) if error.code.as_str() == "scripted-vendor-failure"
            ),
            "expected the vendor error event, received {error}"
        );
        assert!(
            session.closed.load(Ordering::Acquire),
            "expected a failed turn to close the session"
        );
    }

    struct StartFailureSession {
        inner: Box<dyn Session>,
        closed: AtomicBool,
    }

    #[async_trait::async_trait]
    impl Session for StartFailureSession {
        fn state(&self) -> &SessionState {
            self.inner.state()
        }

        async fn start_turn(&self, _request: TurnRequest) -> Result<TurnStream> {
            Err(Error::Protocol {
                expected: String::from("a turn to start"),
                received: String::from("the scripted start failure"),
            })
        }

        async fn respond(&self, response: PermissionResponse) -> Result<()> {
            self.inner.respond(response).await
        }

        async fn cancel(&self, reason: CancelReason) -> Result<()> {
            self.inner.cancel(reason).await
        }

        async fn close(&self, reason: CloseReason) -> Result<()> {
            self.closed.store(true, Ordering::Release);
            self.inner.close(reason).await
        }
    }

    #[tokio::test]
    async fn closes_the_session_when_starting_the_turn_fails() {
        let inner = FakeHarness::new()
            .without_approvals()
            .open_session(&host(), OpenSession::new("mea-test"))
            .await
            .expect("expected a fake session");
        let session = StartFailureSession {
            inner,
            closed: AtomicBool::new(false),
        };

        let error = run(
            &session,
            TurnRequest::new("mea-turn-1", "this start must fail"),
        )
        .await
        .expect_err("expected the scripted start failure");

        assert!(
            matches!(&error, Error::Protocol { expected, received }
                if expected == "a turn to start" && received == "the scripted start failure"),
            "expected the typed start error, received {error:?}"
        );
        assert!(
            session.closed.load(Ordering::Acquire),
            "expected a failed start to close the session"
        );
    }
}
