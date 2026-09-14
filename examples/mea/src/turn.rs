//! Drives one `mea turn` lifecycle after a harness has opened its session.

use std::time::Duration;

use mango_external_agents::event::EventKind;
use mango_external_agents::{
    BrokerDecision, CancelReason, CloseReason, Error, PermissionBroker, Result, Session,
    TurnRequest, TurnStream,
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
    let outcome = async {
        let mut stream = session.start_turn(request).await?;
        match tokio::time::timeout(
            TURN_DEADLINE,
            print_turn(session, &mut stream, json, broker),
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
) -> Result<()> {
    while let Some(event) = turn.recv().await {
        if json {
            println!("{}", serde_json::json!(event));
        }
        match &event.kind {
            EventKind::TextDelta { text } if !json => print!("{text}"),
            EventKind::ApprovalRequested { request } => {
                if !json {
                    println!("{}", serde_json::json!(event.kind));
                }
                let response = match broker.decide(request).await {
                    BrokerDecision::Allow => request.allow()?,
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use mango_external_agents::testing::{FakeHarness, FakeLauncher};
    use mango_external_agents::{
        CancelReason, CloseReason, Error, ErrorCode, EventSink, Harness, HostContext, OpenSession,
        PermissionResponse, Result, Session, SessionInfo, SystemClock, TurnRequest, TurnStream,
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
        fn info(&self) -> &SessionInfo {
            self.inner.info()
        }

        async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
            let (sink, events) = EventSink::new(
                self.info().ids.session_id.clone(),
                request.turn_id.clone(),
                Arc::new(SystemClock),
                1,
            );
            sink.fail(VendorError::new(
                ErrorCode::from_static("scripted-vendor-failure"),
                "the scripted vendor failure",
            ))
            .await?;
            Ok(TurnStream {
                turn_id: request.turn_id,
                native_turn_id: String::from("failed-turn"),
                events,
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
        fn info(&self) -> &SessionInfo {
            self.inner.info()
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
            error.to_string().contains("the scripted start failure"),
            "expected the start error, received {error}"
        );
        assert!(
            session.closed.load(Ordering::Acquire),
            "expected a failed start to close the session"
        );
    }
}
