//! Submission certainty, retry advice and cleanup require separate host decisions.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use hub_host::testing::{FakeHubApi, FakeVendorSession, HubCallKind, ReconcileAnswer, TurnAnswer};
use hub_host::{Commit, HubApi, HubStatus, Reconciliation, Settled, Stop, Supervisor};
use mango_external_agents::{
    CancelReason, CloseReason, Dispatch, Error, ErrorCode, ExitStatus, PermissionResponse,
    ProcessControl, RecoveryAction, Result, Session, SessionState, SessionStatus, SystemClock,
    TerminalStatus, TurnId, TurnRequest, TurnStream, VendorError,
};

/// A live fake that refuses every start and counts calls before returning the error.
///
/// Used as `RefusingSession::new(Error::LimitExceeded { .. })` to observe retries independently of
/// whether the vendor accepted any work.
#[derive(Clone)]
struct RefusingSession {
    inner: FakeVendorSession,
    error: Error,
    attempts: Arc<AtomicUsize>,
}

/// Busy is transient, and waiting still uses the host's backoff.
#[tokio::test(start_paused = true)]
async fn busy_waits_before_replaying_under_a_new_attempt() {
    let session = FakeVendorSession::new().answering([TurnAnswer::NotSubmitted]);
    let hub = Arc::new(FakeHubApi::new());
    let stop = Arc::new(Stop::new());
    let mut supervisor = common::supervisor(&session, &hub, &stop);
    let started = tokio::time::Instant::now();
    let settled = supervisor.run(TurnRequest::new("busy", "hello")).await;
    assert_eq!(tokio::time::Instant::now() - started, common::BASE_DELAY);
    assert_eq!(
        settled.expect("expected retry to settle"),
        Settled::Committed {
            terminal: TerminalStatus::Completed,
            commit: Commit::Recorded,
        }
    );
    assert_eq!(hub.count(HubCallKind::Reserve), 2);
    assert_eq!(hub.count(HubCallKind::Withdraw), 1);
}

/// No process behind this handle can be declared reaped without the caller inspecting it.
struct UnreapedControl;

#[async_trait::async_trait]
impl ProcessControl for UnreapedControl {
    fn pid(&self) -> Option<u32> {
        None
    }
    fn stderr_tail(&self) -> String {
        String::new()
    }
    async fn wait(&self) -> Result<ExitStatus> {
        Err(Error::Closed {
            subject: "unreaped fake",
        })
    }
    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        Ok(())
    }
}

/// Nonretryable does not turn link, timeout, vendor or cleanup failures into caller refusals.
#[tokio::test(start_paused = true)]
async fn recovery_failures_return_their_cause_and_cleanup_without_replay() {
    let control: Arc<dyn ProcessControl> = Arc::new(UnreapedControl);
    let failures = [
        Error::Link {
            peer: "fake".into(),
            message: "a closed pipe".into(),
        },
        Error::Timeout {
            operation: "turn/start".into(),
            after: Duration::from_secs(1),
        },
        Error::Vendor(VendorError::new(
            ErrorCode::from_static("vendor-refusal"),
            "refused",
        )),
        Error::CleanupRequired {
            control: Arc::clone(&control),
            source: Box::new(Error::Busy),
        },
    ];
    for failure in failures {
        let session = RefusingSession::new(failure);
        let hub = Arc::new(FakeHubApi::new());
        let mut supervisor = Supervisor::new(
            Box::new(session.clone()),
            Arc::clone(&hub) as Arc<dyn HubApi>,
            common::policy(),
            Arc::new(Stop::new()),
            Arc::new(SystemClock),
        );
        let error = supervisor
            .run(TurnRequest::new("recovery", "hello"))
            .await
            .expect_err("expected recovery failure unchanged");
        assert_eq!(session.attempts(), 1);
        assert_eq!(hub.count(HubCallKind::Withdraw), 1);
        assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
        assert_eq!(error.cause().to_string(), session.error.cause().to_string());
        if let Some(retained) = session.error.cleanup_control() {
            assert!(Arc::ptr_eq(
                &retained,
                &error.cleanup_control().expect("expected retained control")
            ));
            assert!(retained.wait().await.is_err());
            retained
                .kill(CancelReason::Shutdown)
                .await
                .expect("expected fake cleanup request");
        }
    }
}

/// Retry advice cannot reopen a session that the harness has closed.
#[tokio::test(start_paused = true)]
async fn a_closed_session_does_not_retry_even_busy() {
    let session = RefusingSession::new(Error::Busy);
    session.state().set_status(SessionStatus::Closed);
    let hub = Arc::new(FakeHubApi::new());
    let mut supervisor = Supervisor::new(
        Box::new(session.clone()),
        Arc::clone(&hub) as Arc<dyn HubApi>,
        common::policy(),
        Arc::new(Stop::new()),
        Arc::new(SystemClock),
    );
    let error = supervisor
        .run(TurnRequest::new("closed", "hello"))
        .await
        .expect_err("expected closed-session recovery");
    assert!(matches!(error.cause(), Error::Busy));
    assert_eq!(session.attempts(), 1);
    assert_eq!(hub.count(HubCallKind::Withdraw), 1);
}

/// Failed cleanup must stay owned even when submission cannot be proved absent.
#[tokio::test(start_paused = true)]
async fn cleanup_during_uncertain_start_returns_control_without_replay_or_withdrawal() {
    for dispatch in [Dispatch::Accepted, Dispatch::AcceptanceUnknown] {
        let control: Arc<dyn ProcessControl> = Arc::new(UnreapedControl);
        let mut session = RefusingSession::new(Error::CleanupRequired {
            control: Arc::clone(&control),
            source: Box::new(Error::Timeout {
                operation: "native cleanup".into(),
                after: Duration::from_secs(1),
            }),
        });
        session.error = session.error.with_dispatch(dispatch);
        let hub = Arc::new(FakeHubApi::new());
        let stop = Arc::new(Stop::new());
        let mut supervisor = Supervisor::new(
            Box::new(session.clone()),
            Arc::clone(&hub) as Arc<dyn HubApi>,
            common::policy(),
            Arc::clone(&stop),
            Arc::new(SystemClock),
        );
        let mut running = tokio::spawn(async move {
            let result = supervisor.run(TurnRequest::new("cleanup", "hello")).await;
            (supervisor, result)
        });
        let result = match tokio::time::timeout(Duration::from_millis(100), &mut running).await {
            Ok(result) => result,
            Err(_) => {
                stop.stop(CancelReason::Shutdown);
                running.await
            }
        }
        .expect("expected observation task to finish");
        let (supervisor, result) = result;
        let error = result.expect_err("expected cleanup control to reach the caller");
        assert_eq!(error.dispatch(), dispatch);
        assert!(matches!(error.cause(), Error::Timeout { .. }));
        assert!(Arc::ptr_eq(
            &control,
            &error.cleanup_control().expect("expected cleanup handle")
        ));
        assert_eq!(session.attempts(), 1);
        assert_eq!(hub.count(HubCallKind::Withdraw), 0);
        assert_eq!(hub.count(HubCallKind::Reconcile), 0);
        assert_eq!(
            supervisor
                .record(&TurnId::new("cleanup"))
                .expect("expected recovery record")
                .action(),
            if dispatch == Dispatch::Accepted {
                RecoveryAction::Observe
            } else {
                RecoveryAction::Reconcile
            }
        );
    }
}

/// A vendor acknowledgement outranks a later control-plane answer claiming absence.
#[tokio::test(start_paused = true)]
async fn an_accepted_start_failure_cannot_be_replayed_after_a_contradictory_hub_answer() {
    let mut session = RefusingSession::new(Error::Vendor(
        VendorError::new(ErrorCode::from_static("vendor-failed"), "failed")
            .with_vendor_code("-31000", true),
    ));
    session.error = session.error.with_dispatch(Dispatch::Accepted);
    let hub = Arc::new(FakeHubApi::new().reconciling([ReconcileAnswer::Answer(
        Reconciliation::Answered(HubStatus::NeverArrived),
    )]));
    let stop = Arc::new(Stop::new());
    let mut supervisor = Supervisor::new(
        Box::new(session.clone()),
        Arc::clone(&hub) as Arc<dyn HubApi>,
        common::policy(),
        Arc::clone(&stop),
        Arc::new(SystemClock),
    );
    let mut running =
        tokio::spawn(async move { supervisor.run(TurnRequest::new("accepted", "hello")).await });
    let result = match tokio::time::timeout(Duration::from_millis(100), &mut running).await {
        Ok(result) => result,
        Err(_) => {
            stop.stop(CancelReason::Shutdown);
            running.await
        }
    }
    .expect("expected observation task to finish");
    assert_eq!(session.attempts(), 1);
    let error = result.expect_err("expected contradictory absence to be refused");
    assert!(matches!(error.cause(), Error::HostConfiguration { .. }));
    assert_eq!(hub.count(HubCallKind::Withdraw), 0);
    assert_eq!(hub.count(HubCallKind::Reconcile), 1);
}

impl RefusingSession {
    /// Creates a refusal with native proof that no prompt was submitted.
    fn new(error: Error) -> Self {
        Self {
            inner: FakeVendorSession::new(),
            error: error.with_dispatch(Dispatch::NotSubmitted),
            attempts: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Counts every start call, including refused ones.
    fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl Session for RefusingSession {
    fn state(&self) -> &SessionState {
        self.inner.state()
    }

    async fn start_turn(&self, _request: TurnRequest) -> Result<TurnStream> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        Err(self.error.clone())
    }

    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        self.inner.respond(response).await
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        self.inner.cancel(reason).await
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        self.inner.close(reason).await
    }
}

/// Observe the running loop for 100 ms, then cancel it cleanly if it keeps retrying.
/// The attempt assertion fails on the old loop rather than leaving an unbounded background task.
#[tokio::test(start_paused = true)]
async fn an_oversized_prompt_is_refused_once_and_remembered() {
    let session = RefusingSession::new(Error::LimitExceeded {
        subject: "bytes of turn input",
        limit: 4,
        received: 5,
    });
    let hub = Arc::new(FakeHubApi::new());
    let stop = Arc::new(Stop::new());
    let mut supervisor = Supervisor::new(
        Box::new(session.clone()),
        Arc::clone(&hub) as Arc<dyn HubApi>,
        common::policy(),
        Arc::clone(&stop),
        Arc::new(SystemClock),
    );
    let request = TurnRequest::new("oversized", "12345");
    let mut running = tokio::spawn({
        let request = request.clone();
        async move {
            let settled = supervisor.run(request).await;
            (supervisor, settled)
        }
    });
    let completed = match tokio::time::timeout(Duration::from_millis(100), &mut running).await {
        Ok(completed) => completed,
        Err(_) => {
            stop.stop(CancelReason::Shutdown);
            running.await
        }
    };
    let (mut supervisor, settled) = completed.expect("expected the observation task to finish");
    assert_eq!(session.attempts(), 1, "expected one deterministic refusal");
    assert_eq!(hub.count(HubCallKind::Withdraw), 1);
    let refused = Settled::Refused {
        reason: session.error.to_string(),
    };
    assert_eq!(settled.expect("expected a settled refusal"), refused);
    assert_eq!(
        supervisor
            .run(request)
            .await
            .expect("expected the remembered refusal"),
        refused
    );
    assert_eq!(session.attempts(), 1);
    assert_eq!(hub.count(HubCallKind::Reserve), 1);
    assert_eq!(hub.count(HubCallKind::Withdraw), 1);
    assert!(session.snapshot().status.is_usable());
}
