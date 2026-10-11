//! The requests this client makes of an agent: how each is named, admitted, queued and answered.
//!
//! Two kinds, with different bounds:
//!
//! - A request that is not a prompt is a handshake or bookkeeping. It takes one of
//!   `Limits::max_pending_requests` permits, has `Limits::request_timeout` to be answered in, and
//!   ends the connection when it is given up without an answer.
//! - `session/prompt` is a turn. It has no deadline and takes no permit: a session runs one turn
//!   at a time, which is its own bound.
//!
//! Both ask for their answer behind the notifications the agent sent before it.

use std::marker::PhantomData;
use std::sync::{Arc, Mutex, PoisonError};

use agent_client_protocol_schema::v1::{
    AGENT_METHOD_NAMES, CloseSessionRequest, CloseSessionResponse, Error as AcpError,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    SetSessionModeRequest, SetSessionModeResponse,
};
use mango_external_agents::jsonrpc::{CallFailureCause, JsonRpcError, Reply, RequestOptions};
use mango_external_agents::{Dispatch, Error, Result};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::ConnectionHandle;
use crate::transport::Overflow;

/// A request this client sends an agent, with the method it goes out under and what answers it.
///
/// The method names come from the schema crate's own table, so a wire name is spelled once, by
/// the crate that defines the protocol.
///
/// ```ignore
/// assert_eq!(<InitializeRequest as AgentRequest>::METHOD, "initialize");
/// ```
pub(crate) trait AgentRequest: serde::Serialize {
    /// What the agent answers with.
    type Response: DeserializeOwned;
    /// The JSON-RPC method.
    const METHOD: &'static str;
}

macro_rules! agent_request {
    ($($request:ty => $response:ty, $method:ident;)+) => {
        $(impl AgentRequest for $request {
            type Response = $response;
            const METHOD: &'static str = AGENT_METHOD_NAMES.$method;
        })+
    };
}

agent_request! {
    InitializeRequest => InitializeResponse, initialize;
    NewSessionRequest => NewSessionResponse, session_new;
    LoadSessionRequest => LoadSessionResponse, session_load;
    ListSessionsRequest => ListSessionsResponse, session_list;
    SetSessionModeRequest => SetSessionModeResponse, session_set_mode;
    SetSessionConfigOptionRequest => SetSessionConfigOptionResponse, session_set_config_option;
    CloseSessionRequest => CloseSessionResponse, session_close;
    PromptRequest => PromptResponse, session_prompt;
}

/// What [`RequestAdmission`] counts, as a host reads it in a refusal.
pub(super) const OUTSTANDING_REQUESTS: &str = "outstanding ACP requests";

/// Admission in front of the connection's requests that are not prompts.
///
/// The JSON-RPC peer has a budget of its own for calls awaiting an answer, applied when a
/// frame's turn to be written comes. This one is applied when the request is made, so a caller
/// is refused before anything is queued, and it closes the moment teardown begins.
pub(super) struct RequestAdmission {
    permits: tokio::sync::Semaphore,
    limit: usize,
    closed: Mutex<bool>,
}

impl RequestAdmission {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            // A host may set a huge count to mean "no cap"; a semaphore panics above
            // `MAX_PERMITS`, and this runs after the child is launched. `limit` itself stays as
            // the host set it, for the refusal message.
            permits: tokio::sync::Semaphore::new(limit.min(tokio::sync::Semaphore::MAX_PERMITS)),
            limit,
            closed: Mutex::new(false),
        }
    }

    /// Runs `send` holding a permit, unless admission is closed or every permit is out.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] once [`Self::close`] was called, and [`Error::LimitExceeded`] naming the
    /// limit when it is reached. Both are marked [`Dispatch::NotSubmitted`]: `send` was not
    /// run, so nothing of the request exists anywhere.
    ///
    /// ```ignore
    /// let (permit, queued) = admission.submit(|| client.submit_request(method, params, options))?;
    /// ```
    pub(super) fn submit<T>(
        &self,
        send: impl FnOnce() -> T,
    ) -> Result<(tokio::sync::SemaphorePermit<'_>, T)> {
        let closed = self.closed.lock().unwrap_or_else(PoisonError::into_inner);
        if *closed {
            return Err(Error::Closed {
                subject: "ACP connection",
            }
            .with_dispatch(Dispatch::NotSubmitted));
        }
        let permit = self.permits.try_acquire().map_err(|_| {
            Error::LimitExceeded {
                subject: OUTSTANDING_REQUESTS,
                limit: self.limit,
                received: self.limit.saturating_add(1),
            }
            .with_dispatch(Dispatch::NotSubmitted)
        })?;
        let sent = send();
        drop(closed);
        Ok((permit, sent))
    }

    /// Refuses every later request. Serialised with [`Self::submit`].
    pub(super) fn close(&self) {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner) = true;
    }
}

/// Starts owned teardown if a request is given up without an answer.
///
/// A request the agent never answered leaves the connection in a state this client cannot
/// reason about: the agent may still be working on it, and its late answer has no owner. The
/// connection is ended instead of reused.
pub(super) struct RequestAbandonment {
    connection: Arc<ConnectionHandle>,
    armed: bool,
    /// Why the connection is ended when the request is given up.
    reason: mango_external_agents::CancelReason,
}

impl RequestAbandonment {
    fn new(connection: Arc<ConnectionHandle>) -> Self {
        Self {
            connection,
            armed: true,
            reason: mango_external_agents::CancelReason::Shutdown,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RequestAbandonment {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.connection.begin_shutdown(self.reason);
    }
}

impl<Response> SubmittedRequest<'_, Response> {
    /// Ends the connection for `reason` if this request is given up, where it would otherwise
    /// end it as a shutdown.
    ///
    /// For the one request sent on somebody's behalf while they are already ending the
    /// session: giving up `session/close` starts the teardown, and the first reason a teardown
    /// is given is the one the child is stopped for.
    ///
    /// ```ignore
    /// let sent = submit(&connection, CloseSessionRequest::new(id))?.abandoned_for(reason);
    /// ```
    #[must_use]
    pub(crate) fn abandoned_for(mut self, reason: mango_external_agents::CancelReason) -> Self {
        self.abandonment.reason = reason;
        self
    }
}

/// A request admitted and queued, with cleanup owned until its answer arrives.
///
/// The fields drop in the order they are declared, and that order matters: letting the reply go
/// first tells the agent the request was given up (`$/cancel_request`, which the peer writes for
/// a request it had sent) while the connection is still open, and only then does the abandonment
/// begin to close it.
pub(crate) struct SubmittedRequest<'a, Response> {
    reply: Reply,
    _permit: tokio::sync::SemaphorePermit<'a>,
    abandonment: RequestAbandonment,
    method: &'static str,
    response: PhantomData<fn() -> Response>,
}

/// Queues one request synchronously, so a caller can bind a catalog revision or a lifecycle claim
/// to the moment it entered the connection's queue.
///
/// # Errors
///
/// [`Error::Closed`] once teardown began, [`Error::LimitExceeded`] when
/// `Limits::max_pending_requests` requests are already outstanding or the frame is larger than
/// the outgoing budget, and the closed-link or overflow failure
/// [`ConnectionHandle::link_closed_under`] gives when the connection had already ended.
///
/// ```ignore
/// let sent = submit(&connection, SetSessionModeRequest::new(session_id, "plan"))?;
/// let response = await_sent(&connection, &profile, sent).await?;
/// ```
pub(crate) fn submit<Request>(
    connection: &Arc<ConnectionHandle>,
    request: Request,
) -> Result<SubmittedRequest<'_, Request::Response>>
where
    Request: AgentRequest,
{
    let options = RequestOptions::new()
        .after_earlier_notifications()
        // Counted by `RequestAdmission` above instead: its permit is back before the caller
        // continues, where the peer's own count is released a moment after the answer.
        .outside_pending_budget();
    let (permit, queued) = connection.requests.submit(|| {
        connection
            .client
            .submit_request(Request::METHOD, request, options)
    })?;
    let queued = queued.map_err(|error| connection.refused(Request::METHOD, error))?;
    let (_written, reply) = queued.into_parts();
    Ok(SubmittedRequest {
        reply,
        _permit: permit,
        abandonment: RequestAbandonment::new(Arc::clone(connection)),
        method: Request::METHOD,
        response: PhantomData,
    })
}

/// Awaits a request queued with [`submit`] and maps whatever came back.
///
/// # Errors
///
/// - [`Error::Timeout`] naming the method when `Limits::request_timeout` passed. The request is
///   given up, which ends the connection.
/// - [`Error::LimitExceeded`] when a queue budget ended the connection under the request.
/// - [`Error::Vendor`] under `acp-link-closed`, with the child's redacted stderr, when the
///   connection ended any other way. An opening call returns no session or control handle, so
///   this is the only path that preserves the tail for a host to inspect.
/// - Whatever [`request_error`](crate::error::request_error) made of the agent's own refusal.
/// - [`Error::Protocol`] when the answer is not the method's result.
///
/// ```ignore
/// let response = await_sent(&connection, &profile, sent).await?;
/// ```
pub(crate) async fn await_sent<Response>(
    connection: &ConnectionHandle,
    profile: &crate::profile::AcpProfile,
    mut submitted: SubmittedRequest<'_, Response>,
) -> Result<Response>
where
    Response: DeserializeOwned,
{
    let method = submitted.method;
    // By reference: a caller that drops this future drops `submitted` whole, in field order.
    let answered = (&mut submitted.reply).await;
    let failure = match answered {
        Ok(result) => {
            submitted.abandonment.disarm();
            return decode(method, result);
        }
        Err(failure) => failure,
    };
    match failure.cause() {
        CallFailureCause::Peer(refusal) => {
            submitted.abandonment.disarm();
            Err(crate::error::request_error(
                method,
                &agent_error(refusal),
                &profile.login_text(),
            ))
        }
        CallFailureCause::TimedOut { after, .. } => Err(Error::Timeout {
            operation: format!("{method} on an ACP agent"),
            after: *after,
        }),
        // The connection is over, so there is nothing left for an abandonment to end.
        CallFailureCause::Ended(_) => {
            submitted.abandonment.disarm();
            Err(connection.link_closed_under(method))
        }
        // An unwritten frame or a stopped handler says nothing about the connection, and a
        // request with no answer to come must not leave it in use.
        _ => Err(connection.link_closed_under(method)),
    }
}

/// Sends one request under the host's own deadline and maps whatever came back.
///
/// Every request this harness sends goes through here or through [`submit`] **except**
/// `session/prompt`, which is a turn and may legitimately take as long as the agent needs. The
/// rest are handshakes and bookkeeping: an agent that accepted the pipe and never answered
/// `initialize` would otherwise hold `open_session` open for the life of the process, which looks
/// to a host exactly like a hung machine rather than a misbehaving agent.
/// `Limits::request_timeout` is the number the host already set for this.
///
/// # Errors
///
/// As [`submit`] and [`await_sent`].
///
/// ```ignore
/// let page = send(&connection, &profile, ListSessionsRequest::new()).await?;
/// ```
pub(crate) async fn send<Request>(
    connection: &Arc<ConnectionHandle>,
    profile: &crate::profile::AcpProfile,
    request: Request,
) -> Result<Request::Response>
where
    Request: AgentRequest,
{
    let sent = submit(connection, request)?;
    await_sent(connection, profile, sent).await
}

/// Why a `session/prompt` has no answer.
#[derive(Debug)]
pub(crate) enum PromptFailure {
    /// The agent answered with an error of its own, or with a result that is not a prompt's.
    Agent(AcpError),
    /// The connection went away, or the prompt never reached it.
    LinkClosed,
}

/// A `session/prompt` in flight.
///
/// Dropping it gives the prompt up: a frame still queued is never written, and one the agent
/// may have read is followed by `$/cancel_request`.
pub(crate) struct PromptCall {
    /// `None` when the connection had already ended and nothing was queued.
    reply: Option<Reply>,
}

impl PromptCall {
    /// The agent's answer, after every `session/update` it sent ahead of it was handled.
    ///
    /// ```ignore
    /// let outcome = connection.submit_prompt(request)?.answer().await;
    /// ```
    pub(crate) async fn answer(self) -> std::result::Result<PromptResponse, PromptFailure> {
        let Some(reply) = self.reply else {
            return Err(PromptFailure::LinkClosed);
        };
        let failure = match reply.await {
            Ok(result) => {
                return decode(PromptRequest::METHOD, result).map_err(|error| {
                    let mut unreadable = AcpError::internal_error();
                    unreadable.message = error.to_string();
                    PromptFailure::Agent(unreadable)
                });
            }
            Err(failure) => failure,
        };
        match failure.cause() {
            CallFailureCause::Peer(refusal) => Err(PromptFailure::Agent(agent_error(refusal))),
            _ => Err(PromptFailure::LinkClosed),
        }
    }
}

/// Queues `session/prompt` behind every frame queued before it.
///
/// A prompt has no deadline and stands outside the request budget, in both directions: it is
/// admitted with every request permit out, and requests are admitted while it runs.
///
/// A connection that had already ended is not an error here. The call that comes back fails as
/// a closed link when it is awaited, so the turn's own stream reports it, as it reports a link
/// that closed a moment later.
///
/// # Errors
///
/// [`Error::LimitExceeded`] with [`Dispatch::NotSubmitted`] when the frame is larger than the
/// outgoing budget, naming the frame's size: nothing was queued and the connection stays usable.
/// Any other refusal that queued nothing on a live connection, the same way, in the words of
/// [`ConnectionHandle::not_queued`].
///
/// ```ignore
/// let prompt = submit_prompt(&connection, PromptRequest::new(session_id, blocks))?;
/// ```
pub(crate) fn submit_prompt(
    connection: &ConnectionHandle,
    request: PromptRequest,
) -> Result<PromptCall> {
    let options = RequestOptions::new()
        .without_deadline()
        .outside_pending_budget()
        .after_earlier_notifications();
    let error = match connection
        .client
        .submit_request(PromptRequest::METHOD, request, options)
    {
        Ok(queued) => {
            let (_written, reply) = queued.into_parts();
            return Ok(PromptCall { reply: Some(reply) });
        }
        Err(error) => error,
    };
    if let Some(overflow) = Overflow::from_refused_frame(&error) {
        return Err(overflow.error().with_dispatch(Dispatch::NotSubmitted));
    }
    let link_gone = matches!(error.cause(), Error::Closed { .. } | Error::Link { .. });
    if link_gone || connection.client.ended().is_some() {
        return Ok(PromptCall { reply: None });
    }
    Err(connection.not_queued(error))
}

/// The agent's own error frame, in the schema's type.
///
/// A code outside the range the schema can hold is clamped to the nearest end of it: the code is
/// reported to a host as a number, and no ACP or JSON-RPC code is that large.
fn agent_error(refusal: &JsonRpcError) -> AcpError {
    let code = i32::try_from(refusal.code).unwrap_or(match refusal.code.is_negative() {
        true => i32::MIN,
        false => i32::MAX,
    });
    let mut error = AcpError::new(code, refusal.message.clone());
    error.data.clone_from(&refusal.data);
    error
}

/// Reads a result as `Response`.
///
/// ACP lets an agent answer a request that returns nothing with `null`, where the schema's
/// response types are objects whose members are all optional. A `null` result is read as that
/// empty object.
fn decode<Response: DeserializeOwned>(method: &str, result: Value) -> Result<Response> {
    let result = match result {
        Value::Null => Value::Object(serde_json::Map::new()),
        other => other,
    };
    // The decoder's own text quotes the value it could not read, which is the agent's, and this
    // error's `Display` writes `received` verbatim. Only the kind of mismatch is kept.
    serde_json::from_value(result).map_err(|error| Error::Protocol {
        expected: format!("a {method} result"),
        received: format!("a result with a {:?} error", error.classify()),
    })
}

#[cfg(test)]
mod tests {
    use super::{AgentRequest, RequestAdmission, agent_error, decode};
    use agent_client_protocol_schema::v1::{
        CloseSessionRequest, InitializeRequest, ListSessionsRequest, LoadSessionRequest,
        NewSessionRequest, PromptRequest, PromptResponse, SetSessionConfigOptionRequest,
        SetSessionModeRequest, SetSessionModeResponse,
    };
    use mango_external_agents::Error;
    use mango_external_agents::jsonrpc::JsonRpcError;

    #[test]
    fn every_request_goes_out_under_its_acp_method() {
        let received = [
            InitializeRequest::METHOD,
            NewSessionRequest::METHOD,
            LoadSessionRequest::METHOD,
            ListSessionsRequest::METHOD,
            SetSessionModeRequest::METHOD,
            SetSessionConfigOptionRequest::METHOD,
            CloseSessionRequest::METHOD,
            PromptRequest::METHOD,
        ];
        let expected = [
            "initialize",
            "session/new",
            "session/load",
            "session/list",
            "session/set_mode",
            "session/set_config_option",
            "session/close",
            "session/prompt",
        ];
        assert_eq!(
            received, expected,
            "expected the wire methods: {expected:?} | received: {received:?}"
        );
    }

    #[test]
    fn generic_request_admission_is_bounded_and_releases_after_completion() {
        let admission = RequestAdmission::new(1);
        let (permit, ()) = admission.submit(|| ()).expect("expected the first permit");
        let refused = admission
            .submit(|| ())
            .expect_err("expected the second request refused");
        assert!(
            matches!(
                refused.cause(),
                Error::LimitExceeded {
                    subject: "outstanding ACP requests",
                    limit: 1,
                    received: 2
                }
            ) && refused.dispatch() == mango_external_agents::Dispatch::NotSubmitted,
            "expected LimitExceeded {{ outstanding ACP requests, 1, 2 }} marked NotSubmitted | received: {refused:?} marked {:?}",
            refused.dispatch()
        );
        drop(permit);
        assert!(
            admission.submit(|| ()).is_ok(),
            "expected a request admitted once the permit is back | received: a refusal"
        );
    }

    #[test]
    fn a_closed_admission_refuses_with_the_connection_as_its_subject() {
        let admission = RequestAdmission::new(4);
        admission.close();
        let refused = admission
            .submit(|| ())
            .expect_err("expected a closed admission to refuse");
        assert!(
            matches!(
                refused.cause(),
                Error::Closed {
                    subject: "ACP connection"
                }
            ) && refused.dispatch() == mango_external_agents::Dispatch::NotSubmitted,
            "expected Closed {{ subject: \"ACP connection\" }} marked NotSubmitted | received: {refused:?} marked {:?}",
            refused.dispatch()
        );
    }

    #[test]
    fn an_admission_with_an_unbounded_count_is_built_without_panicking() {
        let admission = RequestAdmission::new(usize::MAX);
        assert!(
            admission.submit(|| ()).is_ok(),
            "expected a request admitted under a cap of usize::MAX | received: a refusal"
        );
    }

    #[test]
    fn an_agent_error_keeps_its_code_message_and_data() {
        let refusal = JsonRpcError {
            code: -31_004,
            message: String::from("no workspace"),
            data: Some(serde_json::json!({ "path": "/srv" })),
        };
        let error = agent_error(&refusal);
        let received = (i32::from(error.code), error.message.as_str(), error.data);
        let expected = (
            -31_004,
            "no workspace",
            Some(serde_json::json!({ "path": "/srv" })),
        );
        assert_eq!(
            received, expected,
            "expected (code, message, data): {expected:?} | received: {received:?}"
        );
    }

    #[test]
    fn a_code_the_schema_cannot_hold_is_clamped_to_its_range() {
        for (code, expected) in [(i64::MIN, i32::MIN), (i64::MAX, i32::MAX)] {
            let refusal = JsonRpcError {
                code,
                message: String::new(),
                data: None,
            };
            let received = i32::from(agent_error(&refusal).code);
            assert_eq!(
                received, expected,
                "expected the code for {code}: {expected} | received: {received}"
            );
        }
    }

    #[test]
    fn a_null_result_reads_as_an_empty_response_where_the_schema_allows_it() {
        let read: mango_external_agents::Result<SetSessionModeResponse> =
            decode("session/set_mode", serde_json::Value::Null);
        assert!(
            read.is_ok(),
            "expected a null session/set_mode result read as an empty response | received: {read:?}"
        );
    }

    #[test]
    fn an_unreadable_result_does_not_quote_the_agents_value() {
        let read: mango_external_agents::Result<PromptResponse> = decode(
            "session/prompt",
            serde_json::json!({ "stopReason": "Authorization: Bearer top-secret-token" }),
        );
        let shown = format!(
            "{:?} {}",
            read,
            read.as_ref().expect_err("expected an error")
        );
        assert!(
            !shown.contains("top-secret-token"),
            "expected the agent's value kept out of the error | received: {shown}"
        );
    }

    #[test]
    fn a_result_that_is_not_the_methods_is_a_protocol_error_naming_the_method() {
        let read: mango_external_agents::Result<PromptResponse> =
            decode("session/prompt", serde_json::json!({ "stopReason": 7 }));
        let Err(Error::Protocol { expected, received }) = &read else {
            panic!("expected Error::Protocol | received: {read:?}");
        };
        assert_eq!(
            (expected.as_str(), received.as_str()),
            ("a session/prompt result", "a result with a Data error"),
            "expected the protocol error to name the method and the kind of mismatch | received: ({expected:?}, {received:?})"
        );
    }
}
