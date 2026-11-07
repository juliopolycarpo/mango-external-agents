//! Native answer refusal and permission settlement at the owning turn's terminal boundary.

use super::super::*;
use mango_external_agents::jsonrpc::{Client, ClientOptions, RequestId, Responder, WireOptions};
use mango_external_agents::testing::ScriptedLink;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

/// Retains the real peer's responders without answering or ending their connection.
#[derive(Default)]
struct KeepingQuestions {
    questions: Mutex<Vec<Responder>>,
    handed: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl PeerHandler for KeepingQuestions {
    async fn on_notification(&self, _method: String, _params: serde_json::Value) {}
    async fn on_request(
        &self,
        _method: String,
        _params: serde_json::Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        panic!("expected responder handoff");
    }
    async fn on_request_handoff(
        &self,
        _method: String,
        _params: serde_json::Value,
        responder: Responder,
    ) {
        self.questions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(responder);
        self.handed.notify_one();
    }
}

/// Obtains one actual responder and parks a normalised permission with a controlled deadline.
async fn park(
    state: &SessionState,
    turn: &TurnHandle,
    link: &ScriptedLink,
    handler: &KeepingQuestions,
    id: u32,
    option: &str,
    lifetime: Duration,
) {
    link.push_line(
        serde_json::json!({"jsonrpc":"2.0","id":id,"method":"session/request_permission"})
            .to_string(),
    );
    handler.handed.notified().await;
    let responder = handler
        .questions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop()
        .expect("responder");
    let now = SystemTime::now();
    let request = serde_json::from_value(serde_json::json!({
        "sessionId":"native-1", "toolCall":{"toolCallId":format!("call-{id}")},
        "options":[{"optionId":option,"name":"Reject","kind":"reject_once"}]
    }))
    .expect("permission request");
    let question = permission::request_from(
        &request,
        id.to_string(),
        SessionId::new("session-1"),
        turn.sink.operation(),
        now + lifetime,
    )
    .normalized()
    .expect("normalised permission");
    let (timer_done, _) = tokio::sync::oneshot::channel();
    let (broker_settled, _) = tokio::sync::oneshot::channel();
    let pending = PendingApproval {
        reply: PermissionReply::new(responder),
        host_answerable: true,
        question,
        announced: false,
        deadline: ApprovalDeadline::new(now + lifetime, now).expect("deadline"),
        expiry_option: Some(option.to_owned()),
        cancel: CancelNotification::new(agent_client_protocol_schema::v1::SessionId::new(
            "native-1",
        )),
        turn: turn.clone(),
        _timer_done: timer_done,
        _broker_settled: broker_settled,
    };
    assert!(state.park_pending(id.to_string(), pending).expect("park"));
    assert!(state.announce_pending(&id.to_string(), true));
}

/// A size refusal rejects only one answer. The expiry branch releases that question while a
/// sibling remains parked and answerable; no close, failed writer or connection-end sweep runs.
#[tokio::test(start_paused = true)]
async fn expiry_answer_refusal_keeps_the_connection_and_sibling_owned() {
    let (state, host) = super::state();
    let (sink, mut events) = EventSink::with_limits(
        SessionId::new("session-1"),
        mango_external_agents::TurnId::new("turn-1"),
        mango_external_agents::AttemptId::default(),
        Arc::clone(host.clock()),
        host.limits(),
    );
    let turn = state.begin_turn(sink, None).expect("turn");
    let link = ScriptedLink::new();
    let handler = Arc::new(KeepingQuestions::default());
    let client = Client::connect_with(
        link.clone().into_link(),
        handler.clone(),
        ClientOptions::new("ACP test"),
        WireOptions::new()
            .with_request_handoff()
            .with_max_outbound_frame_bytes(160),
    );
    park(
        &state,
        &turn,
        &link,
        &handler,
        1,
        &"r".repeat(120),
        Duration::from_secs(1),
    )
    .await;
    park(
        &state,
        &turn,
        &link,
        &handler,
        2,
        "reject",
        Duration::from_secs(60),
    )
    .await;
    tokio::time::advance(Duration::from_secs(1)).await;

    assert!(matches!(
        state
            .respond_pending("1", permission::cancelled(), DecisionSource::Expired)
            .expect("expiry"),
        Answered::AlreadyResolved
    ));
    link.wait_for_sent(1).await;
    let sent: serde_json::Value = serde_json::from_str(&link.sent()[0]).expect("response");
    assert_eq!(sent["id"], 1);
    assert_eq!(sent["error"]["code"], -32603);
    assert_eq!(sent["error"]["message"], "the answer was too large to send");
    assert_eq!(state.lock_pending().len(), 1);
    assert!(state.lock_pending().contains_key("2"));
    assert!(client.ended().is_none());
    assert!(!turn.cancellation.is_cancelled());
    assert_eq!(link.sent().len(), 1, "sibling must still be unanswered");

    turn.approvals.flush(&turn.sink).await.expect("events");
    let mut kinds = Vec::new();
    for _ in 0..3 {
        kinds.push(events.recv().await.expect("approval event").kind);
    }
    for (kind, expected_id) in kinds[..2].iter().zip(["1", "2"]) {
        assert!(
            matches!(kind, EventKind::ApprovalRequested { request }
                if request.interaction.id.as_str() == expected_id),
            "expected the initial request for {expected_id}, received {kind:?}"
        );
    }
    let EventKind::ApprovalResolved {
        interaction_id,
        decision,
    } = &kinds[2]
    else {
        panic!("expected the withdrawn resolution, received {:?}", kinds[2]);
    };
    assert_eq!(interaction_id.as_str(), "1");
    assert_eq!(decision.source, DecisionSource::Cancelled);
    assert_eq!(
        decision.option_id,
        super::super::WITHDRAWN_OPTION_ID,
        "expected the withdrawn option, received {}",
        decision.option_id
    );
    assert_eq!(
        decision.effect,
        mango_external_agents::PermissionEffect::Other
    );
    assert_eq!(decision.scope, None);
    assert!(!decision.policy_changing);
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "expected exactly three audit events"
    );
    assert!(matches!(
        state
            .respond_pending("2", permission::selected("reject"), DecisionSource::User)
            .expect("sibling response"),
        Answered::Sent
    ));
    link.wait_for_sent(2).await;
    let sibling: serde_json::Value =
        serde_json::from_str(&link.sent()[1]).expect("sibling response");
    assert_eq!(sibling["id"], 2);
    assert_eq!(sibling["result"]["outcome"]["optionId"], "reject");
    assert!(state.lock_pending().is_empty());
    assert!(client.ended().is_none());
    turn.approvals
        .flush(&turn.sink)
        .await
        .expect("sibling audit");
    let resolved = events.recv().await.expect("sibling resolution").kind;
    assert!(
        matches!(&resolved, EventKind::ApprovalResolved { interaction_id, decision }
            if interaction_id.as_str() == "2" && decision.source == DecisionSource::User
                && decision.option_id == "reject"),
        "expected the sibling's user resolution, received {resolved:?}"
    );
    assert!(
        matches!(
            events.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ),
        "expected no extra sibling audit event"
    );
    assert!(!turn.cancellation.is_cancelled());
    client.close().await.expect("close after assertions");
}

/// Holds one real broker decision until the test advances the turn's finished transition.
struct FinishedTurnBroker {
    entered: tokio::sync::Notify,
    release: tokio::sync::Semaphore,
    decision: mango_external_agents::BrokerDecision,
}

#[async_trait::async_trait]
impl mango_external_agents::PermissionBroker for FinishedTurnBroker {
    async fn decide(
        &self,
        request: &mango_external_agents::PermissionRequest,
    ) -> mango_external_agents::BrokerDecision {
        assert_eq!(
            request.options.len(),
            2,
            "expected the registered allow/refuse choices"
        );
        self.entered.notify_one();
        self.release
            .acquire()
            .await
            .expect("broker release")
            .forget();
        self.decision.clone()
    }
}

#[derive(Clone, Copy, Debug)]
enum FinishedChoiceSource {
    Broker,
    Host,
    Expiry,
    WithdrawOne,
    Cancel,
    WithdrawAll,
}

/// Exercises actual permission registration/reply at the production finish-before-drain boundary.
async fn finished_permission_choice(source: FinishedChoiceSource) {
    use mango_external_agents::{BrokerDecision, Configuration, HostContext, SessionId};
    if matches!(
        source,
        FinishedChoiceSource::Cancel | FinishedChoiceSource::WithdrawAll
    ) {
        // A helper OS thread must run before its bounded notification wait can expire.
        tokio::time::resume();
    }
    let broker = Arc::new(FinishedTurnBroker {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        decision: if matches!(source, FinishedChoiceSource::Broker) {
            BrokerDecision::Allow
        } else {
            BrokerDecision::Ask
        },
    });
    let host = HostContext::builder()
        .launcher(Arc::new(mango_external_agents::testing::FakeLauncher::new()))
        .cwd(std::env::temp_dir())
        .client_info("finished-permission", "0.0.0")
        .broker(broker.clone())
        .limits(mango_external_agents::Limits {
            approval_timeout: Duration::from_secs(
                if matches!(source, FinishedChoiceSource::Expiry) {
                    1
                } else {
                    60
                },
            ),
            ..mango_external_agents::Limits::default()
        })
        .build()
        .expect("host");
    let state = Arc::new(SessionState::new(
        SessionId::new("session-1"),
        &host,
        Configuration::default(),
        super::core_state(),
    ));
    let (sink, mut events) = EventSink::with_limits(
        SessionId::new("session-1"),
        mango_external_agents::TurnId::new("turn-1"),
        mango_external_agents::AttemptId::default(),
        Arc::clone(host.clock()),
        host.limits(),
    );
    let turn = state.begin_turn(sink, None).expect("turn");
    let link = ScriptedLink::new();
    let handler = Arc::new(KeepingQuestions::default());
    let client = Client::connect_with(
        link.clone().into_link(),
        handler.clone(),
        ClientOptions::new("ACP finished-turn test"),
        WireOptions::new().with_request_handoff(),
    );
    link.push_line(
        serde_json::json!({"jsonrpc":"2.0","id":91,"method":"session/request_permission"})
            .to_string(),
    );
    tokio::time::timeout(Duration::from_secs(5), handler.handed.notified())
        .await
        .expect("bounded responder handoff");
    let responder = handler
        .questions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop()
        .expect("responder");
    let request=serde_json::from_value(serde_json::json!({
        "sessionId":"session-1","toolCall":{"toolCallId":"early-finish","title":"Run tests"},
        "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},{"optionId":"reject","name":"Reject","kind":"reject_once"}]
    })).expect("permission request");
    on_request_permission(&state, request, PermissionReply::new(responder))
        .await
        .expect("actual permission registration");
    tokio::time::timeout(Duration::from_secs(5), broker.entered.notified())
        .await
        .expect("registered request reaches broker");
    assert_eq!(state.pending_count(), 1);
    let host_choice = if !matches!(source, FinishedChoiceSource::Broker) {
        broker.release.add_permits(1);
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("bounded host announcement")
            .expect("announcement");
        let EventKind::ApprovalRequested { request } = event.kind else {
            panic!("expected permission announcement");
        };
        Some(request.allow().expect("allowing host choice"))
    } else {
        None
    };
    assert_eq!(
        state.pending_count(),
        1,
        "the real registered request remains live"
    );
    assert!(
        link.sent().is_empty(),
        "no permission has settled before finish"
    );
    assert!(
        !state
            .lock_pending()
            .values()
            .next()
            .expect("registered permission")
            .deadline
            .is_elapsed(),
        "finish must precede the request deadline"
    );
    let mut withdrawal = None;
    let mut prepared = false;
    if matches!(
        source,
        FinishedChoiceSource::Cancel | FinishedChoiceSource::WithdrawAll
    ) {
        let entered = Arc::new(tokio::sync::Notify::new());
        let (release, release_rx) = std::sync::mpsc::channel();
        *turn
            .withdrawal_hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(WithdrawalHold {
            entered: entered.clone(),
            release: release_rx,
        });
        let withdrawing = state.clone();
        let task = std::thread::spawn(move || {
            if matches!(source, FinishedChoiceSource::Cancel) {
                assert!(withdrawing.begin_cancellation(CancelReason::Requested));
            } else {
                withdrawing.withdraw_pending();
            }
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .expect("withdrawal removed the actual request before audit registration");
        assert_eq!(state.pending_count(), 0);
        withdrawal = Some((release, task));
    }
    // This is the real transition used immediately before prepare_terminal_matching in the
    // prompt owner. Hold its continuation here; do not manufacture or remove a pending entry.
    assert!(turn.finish());
    if withdrawal.is_some() {
        let guarded = match state.cancel_reason.try_lock() {
            Ok(guard) => {
                drop(guard);
                false
            }
            Err(std::sync::TryLockError::WouldBlock) => true,
            Err(error) => panic!("unexpected poisoned cancellation guard: {error}"),
        };
        if !guarded {
            // The baseline has surrendered audit ownership: commit the real terminal while its
            // withdrawal remains synchronously paused. The candidate retains the actual guard.
            assert!(state.prepare_terminal_matching(&turn).is_some());
            turn.approvals
                .flush(&turn.sink)
                .await
                .expect("baseline audit drain");
            if matches!(source, FinishedChoiceSource::Cancel) {
                turn.sink
                    .cancel(CancelReason::Requested)
                    .await
                    .expect("cancel terminal");
            } else {
                turn.sink.complete().await.expect("completed terminal");
            }
            prepared = true;
        }
        let (release, task) = withdrawal.take().expect("withdrawal task");
        release.send(()).expect("resume real audit registration");
        task.join().expect("withdrawal task joined");
    }
    assert!(turn.is_finished());
    match source {
        FinishedChoiceSource::Broker => broker.release.add_permits(1),
        FinishedChoiceSource::Host => {
            let _ = state.answer(host_choice.as_ref().expect("host choice"));
        }
        FinishedChoiceSource::Expiry => tokio::time::advance(Duration::from_secs(1)).await,
        FinishedChoiceSource::WithdrawOne => {
            state
                .withdraw_pending_by_id(
                    host_choice
                        .as_ref()
                        .expect("announced choice")
                        .interaction_id
                        .as_str(),
                )
                .expect("withdraw one");
        }
        FinishedChoiceSource::Cancel | FinishedChoiceSource::WithdrawAll => {}
    }
    tokio::time::timeout(Duration::from_secs(5), link.wait_for_sent(1))
        .await
        .expect("bounded native settlement");
    let wire: serde_json::Value =
        serde_json::from_str(&link.sent()[0]).expect("native JSON-RPC answer");
    // Complete the real drain/flush continuation before asserting the observed wrong outcome.
    if !prepared {
        assert!(state.prepare_terminal_matching(&turn).is_some());
        turn.approvals
            .flush(&turn.sink)
            .await
            .expect("approval audit flush");
        if matches!(source, FinishedChoiceSource::Cancel) {
            turn.sink
                .cancel(CancelReason::Requested)
                .await
                .expect("cancel terminal commitment");
        } else {
            turn.sink.complete().await.expect("terminal commitment");
        }
    }
    let remaining = tokio::time::timeout(Duration::from_secs(5), async {
        let mut remaining = Vec::new();
        while let Some(event) = events.recv().await {
            remaining.push(event.kind);
        }
        remaining
    })
    .await
    .expect("bounded terminal stream");
    assert!(
        matches!(
            remaining.last(),
            Some(
                EventKind::Completed
                    | EventKind::Cancelled {
                        reason: CancelReason::Requested
                    }
            )
        ),
        "selected terminal committed"
    );
    client.close().await.expect("close after observation");
    assert_eq!(
        wire["result"]["outcome"]["outcome"], "cancelled",
        "a finished turn must withdraw a late permission choice; source={source:?}, wire={wire:?}"
    );
    if matches!(
        source,
        FinishedChoiceSource::Host
            | FinishedChoiceSource::Expiry
            | FinishedChoiceSource::WithdrawOne
            | FinishedChoiceSource::Cancel
            | FinishedChoiceSource::WithdrawAll
    ) {
        let resolutions: Vec<_> = remaining
            .iter()
            .filter_map(|event| match event {
                EventKind::ApprovalResolved { decision, .. } => Some(decision),
                _ => None,
            })
            .collect();
        assert_eq!(
            resolutions.len(),
            1,
            "an already announced finished-turn permission must resolve once; source={source:?}, events={remaining:?}"
        );
        assert_eq!(resolutions[0].source, DecisionSource::Cancelled);
    }
    if matches!(source, FinishedChoiceSource::Broker) {
        assert_eq!(
            remaining.len(),
            1,
            "no announcement after the terminal claim"
        );
    }
    assert_eq!(state.pending_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn late_finished_broker_choice_is_withdrawn() {
    finished_permission_choice(FinishedChoiceSource::Broker).await;
}
#[tokio::test(start_paused = true)]
async fn late_finished_host_choice_is_withdrawn() {
    finished_permission_choice(FinishedChoiceSource::Host).await;
}
#[tokio::test(start_paused = true)]
async fn late_finished_expiry_choice_is_withdrawn() {
    finished_permission_choice(FinishedChoiceSource::Expiry).await;
}

/// Holds the request's injected wall-clock read after its first finished check, before parking.
struct RegistrationClock {
    armed: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

impl mango_external_agents::Clock for RegistrationClock {
    fn now(&self) -> SystemTime {
        if self.armed.swap(false, std::sync::atomic::Ordering::AcqRel) {
            self.entered.notify_one();
            self.release
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .recv_timeout(Duration::from_secs(5))
                .expect("bounded registration release");
        }
        SystemTime::now()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_finished_registration_is_withdrawn_before_parking() {
    use mango_external_agents::{BrokerDecision, Configuration, HostContext, SessionId};
    let (release, wait) = std::sync::mpsc::channel();
    let clock = Arc::new(RegistrationClock {
        armed: std::sync::atomic::AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: Mutex::new(wait),
    });
    let broker = Arc::new(FinishedTurnBroker {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        decision: BrokerDecision::Allow,
    });
    let host = HostContext::builder()
        .launcher(Arc::new(mango_external_agents::testing::FakeLauncher::new()))
        .cwd(std::env::temp_dir())
        .client_info("finished-registration", "0.0.0")
        .clock(clock.clone())
        .broker(broker)
        .build()
        .expect("host");
    let state = Arc::new(SessionState::new(
        SessionId::new("session-1"),
        &host,
        Configuration::default(),
        super::core_state(),
    ));
    let (sink, mut events) = EventSink::with_limits(
        SessionId::new("session-1"),
        mango_external_agents::TurnId::new("turn-1"),
        mango_external_agents::AttemptId::default(),
        Arc::clone(host.clock()),
        host.limits(),
    );
    let turn = state.begin_turn(sink, None).expect("turn");
    let link = ScriptedLink::new();
    let handler = Arc::new(KeepingQuestions::default());
    let client = Client::connect_with(
        link.clone().into_link(),
        handler.clone(),
        ClientOptions::new("ACP registration test"),
        WireOptions::new().with_request_handoff(),
    );
    link.push_line(
        serde_json::json!({"jsonrpc":"2.0","id":92,"method":"session/request_permission"})
            .to_string(),
    );
    tokio::time::timeout(Duration::from_secs(5), handler.handed.notified())
        .await
        .expect("bounded handoff");
    let responder = handler
        .questions
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .pop()
        .expect("actual responder");
    let request = serde_json::from_value(serde_json::json!({
        "sessionId":"session-1", "toolCall":{"toolCallId":"registration-race"},
        "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},
                   {"optionId":"reject","name":"Reject","kind":"reject_once"}]
    }))
    .expect("permission request");
    clock
        .armed
        .store(true, std::sync::atomic::Ordering::Release);
    let registration_state = state.clone();
    let registration = tokio::spawn(async move {
        on_request_permission(
            &registration_state,
            request,
            PermissionReply::new(responder),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), clock.entered.notified())
        .await
        .expect("request passed its early finished check");
    assert_eq!(state.pending_count(), 0);
    assert!(turn.finish());
    release.send(()).expect("resume real request registration");
    tokio::time::timeout(Duration::from_secs(5), registration)
        .await
        .expect("bounded registration")
        .expect("registration task")
        .expect("registration result");
    let pending_after_finish = state.pending_count();
    // Finish cleanup on both implementations before asserting the observed registration gap.
    assert!(state.prepare_terminal_matching(&turn).is_some());
    turn.approvals.flush(&turn.sink).await.expect("audit flush");
    turn.sink.complete().await.expect("terminal");
    tokio::time::timeout(Duration::from_secs(5), link.wait_for_sent(1))
        .await
        .expect("native reply");
    let wire: serde_json::Value = serde_json::from_str(&link.sent()[0]).expect("wire");
    let remaining = tokio::time::timeout(Duration::from_secs(5), async {
        let mut remaining = Vec::new();
        while let Some(event) = events.recv().await {
            remaining.push(event.kind);
        }
        remaining
    })
    .await
    .expect("terminal stream");
    client.close().await.expect("close after observation");
    assert_eq!(wire["result"]["outcome"]["outcome"], "cancelled");
    assert!(
        matches!(remaining.last(), Some(EventKind::Completed)),
        "terminal committed"
    );
    assert_eq!(
        pending_after_finish, 0,
        "a request must not park after its turn finishes"
    );
    assert_eq!(
        remaining.len(),
        1,
        "an unannounced withdrawal adds no audit row"
    );
}

/// An ACP peer that asks permission but lets the test refuse its prompt before the broker answers.
#[derive(Default)]
struct EarlyRefusalAgent {
    prompt_id: Mutex<Option<serde_json::Value>>,
    permission_reply: Mutex<Option<serde_json::Value>>,
    replied: tokio::sync::Notify,
}

impl EarlyRefusalAgent {
    fn answer(&self, line: &str) -> Vec<String> {
        let message: serde_json::Value = serde_json::from_str(line).expect("native JSON-RPC");
        if message["method"] == "session/prompt" {
            *self
                .prompt_id
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(message["id"].clone());
            return vec![
                serde_json::json!({
                    "jsonrpc":"2.0", "id":93, "method":"session/request_permission",
                    "params":{"sessionId":"session-1","toolCall":{"toolCallId":"early-refusal"},
                        "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"},
                                   {"optionId":"reject","name":"Reject","kind":"reject_once"}]}
                })
                .to_string(),
            ];
        }
        if message["id"] == 93 && message.get("result").is_some() {
            *self
                .permission_reply
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(message);
            self.replied.notify_one();
        }
        Vec::new()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_finished_actual_prompt_refusal_withdraws_broker_allow() {
    use mango_external_agents::testing::{Announcer, FakeLauncher, FakeProcess};
    use mango_external_agents::{
        BrokerDecision, CloseReason, Configuration, HostContext, LaunchSpec, ProcessLauncher,
        Session, SessionId, TurnRequest, VendorInfo,
    };
    let peer = Arc::new(EarlyRefusalAgent::default());
    let agent = Announcer::new();
    let launcher = FakeLauncher::new();
    let answering = peer.clone();
    launcher.push(
        FakeProcess::responding(move |line| answering.answer(line)).announcing(agent.clone()),
    );
    let broker = Arc::new(FinishedTurnBroker {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
        decision: BrokerDecision::Allow,
    });
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("early-refusal", "0.0.0")
        .broker(broker.clone())
        .limits(mango_external_agents::Limits {
            approval_timeout: Duration::from_secs(60),
            ..mango_external_agents::Limits::default()
        })
        .build()
        .expect("host");
    let core = super::core_state();
    let state = Arc::new(SessionState::new(
        SessionId::new("session-1"),
        &host,
        Configuration::default(),
        core.clone(),
    ));
    let process = launcher
        .spawn(LaunchSpec {
            argv: vec![String::from("fake-acp")],
            cwd: host.cwd().to_path_buf(),
            env: Default::default(),
            stdin: true,
            hide_window: true,
        })
        .await
        .expect("fake child");
    let framed = crate::transport::frame(process, &host).expect("framed fake child");
    let connection = Arc::new(drive(framed, state.clone()));
    let session = crate::session::AcpSession::new(
        Arc::new(crate::AcpProfile::custom(
            "fake",
            ["fake-acp"],
            VendorInfo {
                company: "Nobody",
                terms_url: "https://example.invalid/terms",
                privacy_url: "https://example.invalid/privacy",
                skills_are_slash_commands: false,
            },
        )),
        host,
        state.clone(),
        core,
        connection,
        agent_client_protocol_schema::v1::SessionId::new("session-1"),
        agent_client_protocol_schema::v1::AgentCapabilities::default(),
        None,
    );
    let mut stream = session
        .start_turn(TurnRequest::new("turn-1", "run tests"))
        .await
        .expect("actual prompt owner");
    tokio::time::timeout(Duration::from_secs(5), broker.entered.notified())
        .await
        .expect("actual permission broker");
    let turn = state.turn().expect("running prompt");
    assert_eq!(state.pending_count(), 1);
    let (held, held_rx) = tokio::sync::oneshot::channel();
    let (release, release_rx) = std::sync::mpsc::channel();
    let held_state = state.clone();
    let blocker = std::thread::spawn(move || {
        let _turn = held_state.lock_turn();
        held.send(()).expect("turn lock acquired");
        release_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("bounded turn lock release");
    });
    tokio::time::timeout(Duration::from_secs(5), held_rx)
        .await
        .expect("bounded lock acquisition")
        .expect("held");
    let prompt_id = peer
        .prompt_id
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .expect("native prompt id");
    agent.announce(serde_json::json!({"jsonrpc":"2.0","id":prompt_id,"error":{"code":-31000,"message":"model busy"}}).to_string());
    // Observe the production prompt owner's real claim while its next production lock is held.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !turn.is_finished() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual prompt owner claims terminal before pending drain");
    assert_eq!(state.pending_count(), 1);
    assert!(
        !turn.sink.is_terminal(),
        "claim precedes committed terminal"
    );
    broker.release.add_permits(1);
    let reply_observed =
        tokio::time::timeout(Duration::from_secs(5), peer.replied.notified()).await;
    // Always release the production lock and finish native cleanup before asserting the wire.
    release.send(()).expect("release terminal drain");
    blocker.join().expect("lock owner joined");
    let events = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = stream.recv().await {
            events.push(event.kind);
        }
        events
    })
    .await
    .expect("bounded actual terminal stream");
    session
        .close(CloseReason::Requested)
        .await
        .expect("owned connection cleanup");
    reply_observed.expect("permission reply before terminal drain");
    let wire = peer
        .permission_reply
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
        .expect("actual native permission reply");
    assert!(
        matches!(events.last(), Some(EventKind::Error { .. })),
        "agent refusal commits Failed: {events:?}"
    );
    assert_eq!(
        wire["result"]["outcome"]["outcome"], "cancelled",
        "actual early-refused prompt must not grant a late broker choice: {wire:?}"
    );
    assert!(
        !events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalRequested { .. } | EventKind::ApprovalResolved { .. }
        )),
        "unannounced request remains unannounced: {events:?}"
    );
    assert_eq!(state.pending_count(), 0);
}

#[tokio::test(start_paused = true)]
async fn late_finished_single_withdrawal_resolves_its_announcement() {
    finished_permission_choice(FinishedChoiceSource::WithdrawOne).await;
}
#[tokio::test(start_paused = true)]
async fn late_finished_explicit_cancel_keeps_withdrawal_owned_until_audited() {
    finished_permission_choice(FinishedChoiceSource::Cancel).await;
}
#[tokio::test(start_paused = true)]
async fn late_finished_bulk_withdrawal_keeps_ownership_until_audited() {
    finished_permission_choice(FinishedChoiceSource::WithdrawAll).await;
}
