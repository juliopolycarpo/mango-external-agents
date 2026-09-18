//! Drives one `mea turn` lifecycle after a harness has opened its session.

use std::time::Duration;

use mango_external_agents::event::EventKind;
use mango_external_agents::{
    ActivityContent, BrokerDecision, CancelReason, CloseReason, Error, PermissionBroker, Result,
    Session, TurnRequest, TurnStream,
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
    run_with_broker(session, request, json, &crate::terminal::TerminalBroker).await
}

async fn run_with_broker(
    session: &dyn Session,
    request: TurnRequest,
    json: bool,
    broker: &dyn PermissionBroker,
) -> Result<()> {
    run_with_host(session, request, json, broker, &crate::ask::TerminalInput).await
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

/// Prints events and lets the host broker answer each approval once.
async fn print_turn(
    session: &dyn Session,
    turn: &mut TurnStream,
    json: bool,
    broker: &dyn PermissionBroker,
    asker: &dyn crate::ask::QuestionInput,
) -> Result<()> {
    while let Some(event) = turn.recv().await {
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
                let answers = crate::ask::answer_round(asker, request).await;
                session.answer(answers).await?;
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
                let response = match broker.decide(request).await {
                    // A question the vendor raised without an allowing option is still a question
                    // that has to be answered. Falling back to the refusal keeps the turn alive
                    // and grants nothing, where propagating would cancel the turn instead.
                    BrokerDecision::Allow => match request.allow() {
                        Ok(allow) => allow,
                        Err(_) => request.deny()?,
                    },
                    BrokerDecision::Deny { .. } | BrokerDecision::Ask => request.deny()?,
                };
                session.respond(response).await?;
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
    }
    Ok(())
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
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use mango_external_agents::testing::{FakeHarness, FakeLauncher};
    use mango_external_agents::{
        CancelReason, CloseReason, Error, ErrorCode, EventSink, Harness, HostContext, OpenSession,
        PermissionResponse, Result, Session, SessionState, SystemClock, TurnRequest, TurnStream,
        VendorError,
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
