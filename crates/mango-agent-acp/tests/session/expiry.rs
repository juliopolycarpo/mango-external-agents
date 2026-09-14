use super::*;

#[tokio::test(start_paused = true)]
async fn an_unanswered_approval_expires_without_host_cancellation() {
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("expiry", "run it"))
        .await
        .expect("expected turn");
    let question = loop {
        let event = turn.recv().await.expect("expected approval request");
        if let EventKind::ApprovalRequested { request } = event.kind {
            break request;
        }
    };
    tokio::time::advance(Duration::from_secs(121)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    let writes = launcher.written();
    assert!(
        writes
            .iter()
            .any(|line| line.contains("\"optionId\":\"reject\"")),
        "expected an expired approval to be rejected without host input, received {writes:?}"
    );
    session
        .respond(question.allow().expect("expected allow option"))
        .await
        .expect("late response is ignored");
    let events = drain(&mut turn).await;
    assert_eq!(events.iter().filter(|event| matches!(event, EventKind::ApprovalResolved { decision, .. } if decision.source == DecisionSource::Expired)).count(), 1,
        "expected exactly one expired resolution, received {events:?}");
    session.close(CloseReason::Shutdown).await.expect("close");
}

#[tokio::test(start_paused = true)]
async fn a_host_allow_at_the_deadline_is_expired_before_the_timer_task_runs() {
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("expiry", "run it"))
        .await
        .expect("turn");
    let question = loop {
        let event = turn.recv().await.expect("approval");
        if let EventKind::ApprovalRequested { request } = event.kind {
            break request;
        }
    };
    tokio::time::advance(Duration::from_secs(120)).await;
    session
        .respond(question.allow().expect("allow option"))
        .await
        .expect("late response");
    let events = drain(&mut turn).await;
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"optionId\":\"reject\"")),
        "expected deadline to replace late Allow with Reject, received {:?}",
        launcher.written()
    );
    assert!(events.iter().any(|event| matches!(event, EventKind::ApprovalResolved { decision, .. } if decision.source == DecisionSource::Expired)), "expected expired resolution, received {events:?}");
    assert!(!events.iter().any(|event| matches!(event, EventKind::ApprovalResolved { decision, .. } if decision.source == DecisionSource::User)), "late user choice must not be reported, received {events:?}");
    session.close(CloseReason::Shutdown).await.expect("close");
}

struct SlowAllowBroker;

#[async_trait::async_trait]
impl mango_external_agents::PermissionBroker for SlowAllowBroker {
    async fn decide(&self, _request: &mango_external_agents::PermissionRequest) -> BrokerDecision {
        tokio::time::sleep(Duration::from_secs(121)).await;
        BrokerDecision::Allow
    }
}

#[tokio::test(start_paused = true)]
async fn broker_deliberation_consumes_the_approval_deadline() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .asking_for_approval(Approval::Once)
            .process(),
    );
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .broker(Arc::new(SlowAllowBroker))
        .build()
        .expect("host");
    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat").with_configuration(permissive()),
        )
        .await
        .expect("session");
    let mut turn = session
        .start_turn(TurnRequest::new("expiry", "run it"))
        .await
        .expect("turn");
    let mut events = Vec::new();
    while let Some(event) = turn.recv().await {
        let terminal = event.is_terminal();
        events.push(event.kind);
        if terminal {
            break;
        }
    }
    assert!(events.iter().any(|event| matches!(event, EventKind::ApprovalResolved { decision, .. } if decision.source == DecisionSource::Expired)), "expected stalled broker to expire, received {events:?}");
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"optionId\":\"allow\"")),
        "expired broker must not allow, received {:?}",
        launcher.written()
    );
    session.close(CloseReason::Shutdown).await.expect("close");
}

#[tokio::test(start_paused = true)]
async fn expiry_withdraws_a_question_that_offers_no_one_time_refusal() {
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::OnlyAllows),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("expiry", "run it"))
        .await
        .expect("turn");
    loop {
        let event = turn.recv().await.expect("approval");
        if matches!(event.kind, EventKind::ApprovalRequested { .. }) {
            break;
        }
    }
    tokio::time::advance(Duration::from_secs(120)).await;
    let events = drain(&mut turn).await;
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"outcome\":\"cancelled\"")),
        "expected withdrawal when the agent offers no refusal, received {:?}",
        launcher.written()
    );
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("session/cancel")),
        "expected expiry to cancel the prompt when no refusal exists, received {:?}",
        launcher.written()
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, EventKind::ApprovalResolved { .. })),
        "withdrawal must not invent a selected vendor option, received {events:?}"
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::Cancelled {
                reason: CancelReason::Timeout
            }
        )),
        "expected the fake agent to cancel its turn after withdrawal, received {events:?}"
    );
    session.close(CloseReason::Shutdown).await.expect("close");
}

#[tokio::test(start_paused = true)]
async fn a_full_event_channel_does_not_postpone_the_wire_deadline() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .asking_for_approval(Approval::Once)
            .process(),
    );
    let broker = Arc::new(RecordingBroker::new(BrokerDecision::Ask));
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            turn_channel_capacity: 1,
            ..Limits::default()
        })
        .broker(broker.clone())
        .build()
        .expect("host");
    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat").with_configuration(permissive()),
        )
        .await
        .expect("session");
    let mut turn = session
        .start_turn(TurnRequest::new("expiry", "run it"))
        .await
        .expect("turn");
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        broker.requests().len(),
        1,
        "expected the permission handler to reach the full channel"
    );
    tokio::time::advance(Duration::from_secs(120)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"optionId\":\"reject\"")),
        "expected expiry to reject on the wire while the host reads nothing, received {:?}",
        launcher.written()
    );
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.as_slice(), [EventKind::SessionStarted { .. }, EventKind::ApprovalRequested { .. }, EventKind::ApprovalResolved { decision, .. }, EventKind::Completed] if decision.source == DecisionSource::Expired),
        "expected request, expired resolution, then terminal after backpressure clears, received {events:?}"
    );
    session.close(CloseReason::Shutdown).await.expect("close");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fast_agent_cannot_complete_before_its_expiry_audit_on_another_worker() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .asking_for_approval(Approval::Once)
            .process(),
    );
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(Limits {
            request_timeout: Duration::from_millis(100),
            ..Limits::default()
        })
        .build()
        .expect("host");
    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat").with_configuration(permissive()),
        )
        .await
        .expect("session");
    let mut turn = session
        .start_turn(TurnRequest::new("expiry", "run it"))
        .await
        .expect("turn");
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.as_slice(), [EventKind::SessionStarted { .. }, EventKind::ApprovalRequested { .. }, EventKind::ApprovalResolved { decision, .. }, EventKind::Completed] if decision.source == DecisionSource::Expired),
        "expected requested < expired < terminal across runtime workers, received {events:?}"
    );
    session.close(CloseReason::Shutdown).await.expect("close");
}

#[derive(Default)]
struct PausedAskBroker {
    entered: mango_external_agents::CancelToken,
    released: mango_external_agents::CancelToken,
}

#[async_trait::async_trait]
impl mango_external_agents::PermissionBroker for PausedAskBroker {
    async fn decide(&self, _request: &mango_external_agents::PermissionRequest) -> BrokerDecision {
        self.entered.cancel();
        self.released.cancelled().await;
        BrokerDecision::Ask
    }
}

#[tokio::test(start_paused = true)]
async fn a_stale_response_cannot_publish_a_question_before_its_broker_is_done() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .with_updates(Vec::new())
            .asking_for_approval(Approval::Once)
            .process(),
    );
    let broker = Arc::new(PausedAskBroker::default());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .broker(broker.clone())
        .build()
        .expect("host");
    let session = AcpHarness::new(profile())
        .open_session(
            &host,
            OpenSession::new("chat").with_configuration(permissive()),
        )
        .await
        .expect("session");
    let mut turn = session
        .start_turn(TurnRequest::new("expiry", "run it"))
        .await
        .expect("turn");
    turn.recv().await.expect("session started");
    broker.entered.cancelled().await;
    session
        .respond(mango_external_agents::PermissionResponse::from_user(
            "stale", "allow",
        ))
        .await
        .expect("stale reply is harmless");
    assert!(
        turn.events.try_recv().is_err(),
        "expected broker-owned question to remain unpublished during an unrelated response"
    );
    broker.released.cancel();
    let event = turn
        .recv()
        .await
        .expect("answerable question after broker returns Ask");
    let EventKind::ApprovalRequested { request } = event.kind else {
        panic!("expected approval, received {:?}", event.kind);
    };
    session
        .respond(request.allow().expect("allow"))
        .await
        .expect("response");
    let events = drain(&mut turn).await;
    assert!(events.iter().any(|event| matches!(event, EventKind::ApprovalResolved { decision, .. } if decision.source == DecisionSource::User)), "expected visible question to accept user answer, received {events:?}");
    session.close(CloseReason::Shutdown).await.expect("close");
}

#[tokio::test]
async fn an_invalid_host_option_leaves_the_approval_answerable() {
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::Once),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("invalid", "run it"))
        .await
        .expect("turn");
    let question = loop {
        let event = turn.recv().await.expect("approval");
        if let EventKind::ApprovalRequested { request } = event.kind {
            break request;
        }
    };
    let response = session
        .respond(mango_external_agents::PermissionResponse::from_user(
            &question.id,
            "  ",
        ))
        .await;
    assert!(
        matches!(response, Err(Error::Protocol { ref received, .. }) if received == "  "),
        "expected an unknown option to be refused as a protocol error, received {response:?}"
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("\"optionId\":\"  \"")),
        "invalid option must not reach the agent"
    );
    session
        .respond(question.allow().expect("valid allow"))
        .await
        .expect("question remains answerable");
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected terminal after valid answer, received {events:?}"
    );
    session.close(CloseReason::Shutdown).await.expect("close");
}

/// A standing refusal is still a refusal, and expiry has to reach for it.
///
/// `PermissionRequest::deny` prefers `reject_once` and falls back to `reject_always`, which is what
/// the read-only standing refusal already does. An expiry path that recognised only `reject_once`
/// read this option set as "nothing here refuses" and cancelled the whole prompt — withdrawing every
/// other question with it — for a question the agent had given it a safe answer to.
#[tokio::test(start_paused = true)]
async fn expiry_refuses_with_a_standing_option_when_the_agent_offers_no_one_time_one() {
    let (session, launcher) = open(
        FakeAcpAgent::new().asking_for_approval(Approval::OnlyStandingRefusal),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("expiry", "run it"))
        .await
        .expect("turn");
    loop {
        let event = turn.recv().await.expect("approval");
        if matches!(event.kind, EventKind::ApprovalRequested { .. }) {
            break;
        }
    }
    tokio::time::advance(Duration::from_secs(120)).await;
    let events = drain(&mut turn).await;

    assert!(
        launcher
            .written()
            .iter()
            .any(|line| line.contains("\"optionId\":\"reject-all\"")),
        "expected the standing refusal to reach the agent, received {:?}",
        launcher.written()
    );
    assert!(
        !launcher
            .written()
            .iter()
            .any(|line| line.contains("session/cancel")),
        "expected one refused question rather than a cancelled prompt, received {:?}",
        launcher.written()
    );
    assert!(
        events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalResolved { decision, .. }
                if decision.option_id == "reject-all" && decision.source == DecisionSource::Expired
        )),
        "expected the expiry to be reported as a refusal, received {events:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            EventKind::Cancelled {
                reason: CancelReason::Timeout
            }
        )),
        "a refusable question must not take the turn down with it, received {events:?}"
    );
    session.close(CloseReason::Shutdown).await.expect("close");
}
