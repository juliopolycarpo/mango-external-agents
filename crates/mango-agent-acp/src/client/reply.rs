//! The answer owed to one `session/request_permission`, held until somebody decides it.
//!
//! The JSON-RPC peer asks [`PeerHandler::on_request`](mango_external_agents::jsonrpc::PeerHandler::on_request)
//! for the answer to each of the agent's questions and writes whatever that call returns. A
//! permission is decided elsewhere and later: by the level's standing refusal, a broker, the host,
//! a deadline or a cancel. [`PermissionReply`] is the half those deciders hold, and
//! [`ParkedQuestion`] the half the handler's call waits on.
//!
//! Everything that settles a question goes through [`PermissionReply::respond`], so this module is
//! the one place that knows how an answer reaches the peer.

use agent_client_protocol_schema::v1::{Error as AcpError, RequestPermissionResponse};
use mango_external_agents::jsonrpc::ServerRequestOutcome;
use tokio::sync::oneshot;

use crate::permission;

/// The right to answer one permission request, once.
pub(super) struct PermissionReply {
    answer: oneshot::Sender<ServerRequestOutcome>,
}

/// The peer's side of a parked question: resolves to the answer the peer writes.
pub(super) struct ParkedQuestion {
    answered: oneshot::Receiver<ServerRequestOutcome>,
}

impl PermissionReply {
    /// A reply and the question waiting on it.
    ///
    /// ```ignore
    /// let (reply, question) = PermissionReply::parked();
    /// reply.respond(permission::cancelled())?;
    /// let outcome = question.answer().await;
    /// ```
    pub(super) fn parked() -> (Self, ParkedQuestion) {
        let (answer, answered) = oneshot::channel();
        (Self { answer }, ParkedQuestion { answered })
    }

    /// Answers the agent's question with `response`.
    ///
    /// # Errors
    ///
    /// A JSON-RPC internal error when the response does not serialise, or when the connection
    /// stopped waiting for this answer because it has ended. Each names what was expected and
    /// what was found.
    ///
    /// ```ignore
    /// reply.respond(permission::selected("reject"))?;
    /// ```
    pub(super) fn respond(self, response: RequestPermissionResponse) -> Result<(), AcpError> {
        let result = serde_json::to_value(response).map_err(|error| {
            internal_error(format!(
                "expected a serialisable session/request_permission response | received: {error}"
            ))
        })?;
        self.answer
            .send(ServerRequestOutcome::Answer(result))
            .map_err(|_| {
                internal_error(String::from(
                    "expected an ACP connection still waiting for this answer | received: one that has ended",
                ))
            })
    }
}

impl ParkedQuestion {
    /// The answer to write, once one was given.
    ///
    /// A reply dropped without an answer reads as ACP's `cancelled` outcome: every path that
    /// settles a question answers it, so a drop means the question was let go, and an agent left
    /// without any answer would wait on it for the rest of its turn.
    pub(super) async fn answer(self) -> ServerRequestOutcome {
        match self.answered.await {
            Ok(outcome) => outcome,
            Err(_) => withdrawn(),
        }
    }
}

/// ACP's `cancelled` outcome, as the answer to a question nobody decided.
fn withdrawn() -> ServerRequestOutcome {
    match serde_json::to_value(permission::cancelled()) {
        Ok(result) => ServerRequestOutcome::Answer(result),
        Err(error) => ServerRequestOutcome::Failure(mango_external_agents::jsonrpc::JsonRpcError {
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
    use super::PermissionReply;
    use crate::permission;
    use mango_external_agents::jsonrpc::ServerRequestOutcome;

    /// The `outcome.outcome` member of an answer, or what the outcome was instead.
    fn outcome_of(outcome: &ServerRequestOutcome) -> String {
        match outcome {
            ServerRequestOutcome::Answer(result) => result
                .pointer("/outcome/outcome")
                .and_then(serde_json::Value::as_str)
                .map_or_else(
                    || format!("an answer without an outcome: {result}"),
                    String::from,
                ),
            ServerRequestOutcome::Failure(error) => format!("a failure with code {}", error.code),
        }
    }

    #[tokio::test]
    async fn an_answer_given_to_the_reply_is_what_the_question_resolves_to() {
        let (reply, question) = PermissionReply::parked();
        reply
            .respond(permission::selected("reject"))
            .expect("expected the answer to be accepted");

        let outcome = question.answer().await;
        let received = match &outcome {
            ServerRequestOutcome::Answer(result) => result
                .pointer("/outcome/optionId")
                .and_then(serde_json::Value::as_str)
                .map(String::from),
            ServerRequestOutcome::Failure(_) => None,
        };
        let expected = Some(String::from("reject"));
        assert_eq!(
            (outcome_of(&outcome).as_str(), &received),
            ("selected", &expected),
            "expected (outcome, option id): (\"selected\", {expected:?}) | received: ({:?}, {received:?})",
            outcome_of(&outcome)
        );
    }

    #[tokio::test]
    async fn a_reply_dropped_unanswered_reads_as_the_cancelled_outcome() {
        let (reply, question) = PermissionReply::parked();
        drop(reply);

        let received = outcome_of(&question.answer().await);
        assert_eq!(
            received, "cancelled",
            "expected the outcome of a dropped reply: \"cancelled\" | received: {received:?}"
        );
    }

    #[tokio::test]
    async fn an_answer_nobody_waits_for_is_an_error_naming_the_ended_connection() {
        let (reply, question) = PermissionReply::parked();
        drop(question);

        let received = reply
            .respond(permission::cancelled())
            .expect_err("expected the answer to be refused");
        assert!(
            received.message.contains("received: one that has ended"),
            "expected an error naming the ended connection | received: {:?}",
            received.message
        );
    }
}
