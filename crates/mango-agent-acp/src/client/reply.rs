//! The answer owed to one `session/request_permission`, held until somebody decides it.
//!
//! The JSON-RPC peer hands each of the agent's questions over with a
//! [`Responder`](mango_external_agents::jsonrpc::Responder): the right to answer it, now or later,
//! from any task. A permission is decided elsewhere and later: by the level's standing refusal, a
//! broker, the host, a deadline or a cancel. [`PermissionReply`] is what those deciders hold.
//!
//! An answer is in the connection's outgoing queue by the time [`PermissionReply::respond`]
//! returns, so an answer and then a `session/cancel` reach the agent in that order, and an answer
//! counts against the outgoing queue like any other frame.
//!
//! Everything that answers one of the agent's requests goes through this module, so it is the one
//! place that knows how an answer reaches the peer.

use agent_client_protocol_schema::v1::{Error as AcpError, RequestPermissionResponse};
use mango_external_agents::jsonrpc::{JsonRpcError, Responder, ServerRequestOutcome};

use crate::permission;

/// The right to answer one permission request, once.
///
/// Dropped without an answer, it gives ACP's `cancelled` outcome: every path that settles a
/// question answers it, so a drop means the question was let go, and an agent left without any
/// answer would wait on it for the rest of its turn.
pub(super) struct PermissionReply {
    /// `None` once the question was answered.
    responder: Option<Responder>,
}

impl PermissionReply {
    /// The reply to the question `responder` answers.
    ///
    /// ```ignore
    /// let reply = PermissionReply::new(responder);
    /// reply.respond(permission::cancelled())?;
    /// ```
    pub(super) fn new(responder: Responder) -> Self {
        Self {
            responder: Some(responder),
        }
    }

    /// Answers the agent's question with `response`, queued before this returns.
    ///
    /// # Errors
    ///
    /// A JSON-RPC internal error when the response does not serialise, or when the connection
    /// did not take the answer because it has ended or its outgoing queue is full. Each names
    /// what was expected and what was found.
    ///
    /// ```ignore
    /// reply.respond(permission::selected("reject"))?;
    /// ```
    pub(super) fn respond(mut self, response: RequestPermissionResponse) -> Result<(), AcpError> {
        let result = serde_json::to_value(response).map_err(|error| {
            internal_error(format!(
                "expected a serialisable session/request_permission response | received: {error}"
            ))
        })?;
        let Some(responder) = self.responder.take() else {
            return Ok(());
        };
        answer(responder, ServerRequestOutcome::Answer(result)).map_err(|error| {
            internal_error(format!(
                "expected an ACP connection that takes this answer | received: {error}"
            ))
        })
    }
}

impl Drop for PermissionReply {
    fn drop(&mut self) {
        if let Some(responder) = self.responder.take() {
            let _ = answer(responder, withdrawn());
        }
    }
}

/// Answers one of the agent's requests with an ACP error, queued before this returns.
///
/// For example, `refuse(responder, Error::method_not_found())` queues
/// `{"error":{"code":-32601,"message":"Method not found"}}` on the id the agent used. An answer
/// the connection does not take, because it has ended or its queue is full, is dropped: the
/// connection's own end is what its owner hears.
pub(super) fn refuse(responder: Responder, error: AcpError) {
    let _ = answer(responder, refusal(error));
}

/// What a question past the connection's cap is answered with, without a responder.
///
/// A permission is withdrawn with ACP's `cancelled` outcome, which an agent already handles;
/// anything else is the method this client does not serve. For example,
/// `past_cap(true)` is `{"outcome":{"outcome":"cancelled"}}`.
pub(super) fn past_cap(is_permission: bool) -> ServerRequestOutcome {
    match is_permission {
        true => withdrawn(),
        false => refusal(AcpError::method_not_found()),
    }
}

/// Queues `outcome` through `responder`, or says why the connection did not take it.
fn answer(
    responder: Responder,
    outcome: ServerRequestOutcome,
) -> mango_external_agents::Result<()> {
    // The write itself is not waited for: the frame is queued, in order, and the connection
    // reports a write that fails as its own end.
    responder.answer_or_refuse(outcome).map(drop)
}

/// An ACP error as the answer to one of the agent's requests.
fn refusal(error: AcpError) -> ServerRequestOutcome {
    ServerRequestOutcome::Failure(JsonRpcError {
        code: i64::from(i32::from(error.code)),
        message: error.message,
        data: error.data,
    })
}

/// ACP's `cancelled` outcome, as the answer to a question nobody decided.
fn withdrawn() -> ServerRequestOutcome {
    match serde_json::to_value(permission::cancelled()) {
        Ok(result) => ServerRequestOutcome::Answer(result),
        Err(error) => ServerRequestOutcome::Failure(JsonRpcError {
            code: -32603,
            message: format!("expected a serialisable cancelled outcome | received: {error}"),
            data: None,
        }),
    }
}

fn internal_error(message: String) -> AcpError {
    let mut error = AcpError::internal_error();
    error.message = message;
    error
}

#[cfg(test)]
mod tests {
    use super::{PermissionReply, past_cap, refuse};
    use crate::permission;
    use agent_client_protocol_schema::v1::Error as AcpError;
    use mango_external_agents::jsonrpc::{
        Client, ClientOptions, PeerHandler, RequestId, Responder, ServerRequestOutcome, WireOptions,
    };
    use mango_external_agents::testing::ScriptedLink;
    use std::sync::{Arc, Mutex, PoisonError};

    /// Keeps every responder it is handed, for the test to answer through.
    #[derive(Default)]
    struct KeepingHandler {
        kept: Mutex<Vec<Responder>>,
        handed: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl PeerHandler for KeepingHandler {
        async fn on_notification(&self, _method: String, _params: serde_json::Value) {}

        async fn on_request(
            &self,
            _method: String,
            _params: serde_json::Value,
            _id: RequestId,
        ) -> ServerRequestOutcome {
            ServerRequestOutcome::Answer(serde_json::Value::Null)
        }

        async fn on_request_handoff(
            &self,
            _method: String,
            _params: serde_json::Value,
            responder: Responder,
        ) {
            self.kept
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(responder);
            self.handed.notify_one();
        }
    }

    /// A connection whose agent has asked one question, and the responder for it.
    async fn asked(id: u32) -> (Client, ScriptedLink, Responder) {
        let link = ScriptedLink::new();
        let handler = Arc::new(KeepingHandler::default());
        let client = Client::connect_with(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("ACP agent"),
            WireOptions::new().with_request_handoff(),
        );
        link.push_line(
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission" })
                .to_string(),
        );
        handler.handed.notified().await;
        let responder = handler
            .kept
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop()
            .expect("expected the question handed over");
        (client, link, responder)
    }

    /// The first frame the connection wrote, once it has.
    async fn first_sent(link: &ScriptedLink) -> serde_json::Value {
        link.wait_for_sent(1).await;
        serde_json::from_str(&link.sent()[0]).expect("expected a JSON frame")
    }

    #[tokio::test]
    async fn an_answer_given_to_the_reply_is_written_on_the_questions_id() {
        let (_client, link, responder) = asked(7).await;
        PermissionReply::new(responder)
            .respond(permission::selected("reject"))
            .expect("expected the answer to be accepted");

        let received = first_sent(&link).await;
        let expected = serde_json::json!({
            "jsonrpc": "2.0", "id": 7,
            "result": { "outcome": { "outcome": "selected", "optionId": "reject" } }
        });
        assert_eq!(
            received, expected,
            "expected the answer on the wire: {expected} | received: {received}"
        );
    }

    #[tokio::test]
    async fn a_reply_dropped_unanswered_writes_the_cancelled_outcome() {
        let (_client, link, responder) = asked(8).await;
        drop(PermissionReply::new(responder));

        let received = first_sent(&link).await;
        let expected = serde_json::json!({
            "jsonrpc": "2.0", "id": 8, "result": { "outcome": { "outcome": "cancelled" } }
        });
        assert_eq!(
            received, expected,
            "expected the answer on the wire: {expected} | received: {received}"
        );
    }

    #[tokio::test]
    async fn an_answer_after_the_connection_closed_is_an_error_naming_it() {
        let (client, link, responder) = asked(9).await;
        client.close().await.expect("expected a clean close");

        let received = PermissionReply::new(responder)
            .respond(permission::cancelled())
            .expect_err("expected the answer to be refused");
        assert!(
            received
                .message
                .starts_with("expected an ACP connection that takes this answer | received: "),
            "expected an error naming the connection that did not take the answer | received: {:?}",
            received.message
        );
        assert_eq!(
            link.sent().len(),
            0,
            "expected nothing written after the close | received: {:?}",
            link.sent()
        );
    }

    #[tokio::test]
    async fn a_refusal_is_written_as_the_acp_error_on_the_requests_id() {
        let (_client, link, responder) = asked(10).await;
        refuse(responder, AcpError::method_not_found());

        let received = first_sent(&link).await;
        let expected = serde_json::json!({
            "jsonrpc": "2.0", "id": 10,
            "error": { "code": -32601, "message": "Method not found" }
        });
        assert_eq!(
            received, expected,
            "expected the refusal on the wire: {expected} | received: {received}"
        );
    }

    #[test]
    fn a_question_past_the_cap_is_withdrawn_or_refused_by_its_method() {
        let describe = |outcome: ServerRequestOutcome| match outcome {
            ServerRequestOutcome::Answer(result) => result.to_string(),
            ServerRequestOutcome::Failure(error) => format!("error {}", error.code),
        };
        let received = (describe(past_cap(true)), describe(past_cap(false)));
        let expected = (
            String::from(r#"{"outcome":{"outcome":"cancelled"}}"#),
            String::from("error -32601"),
        );
        assert_eq!(
            received, expected,
            "expected (a permission, anything else) past the cap: {expected:?} | received: {received:?}"
        );
    }
}
