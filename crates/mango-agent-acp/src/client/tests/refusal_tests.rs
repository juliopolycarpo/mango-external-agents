//! Isolated synchronous answer refusal, before any global transport termination.

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
    assert!(
        matches!(&kinds[2], EventKind::ApprovalResolved { interaction_id, decision }
        if interaction_id.as_str() == "1" && decision.source == DecisionSource::Cancelled)
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
    client.close().await.expect("close after assertions");
}
