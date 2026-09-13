//! The client side of the connection: the driving model, the handlers, and the shared turn state.
//!
//! # Why the connection is driven from a task
//!
//! The official crate's connection is a scope, not an object:
//! `Client.builder()…connect_with(transport, async |cx| …)` runs the dispatch loop for exactly as
//! long as that closure, and the closure is the only place a [`ConnectionTo`] exists. A
//! [`Session`](mango_external_agents::Session) outlives any one call, so the closure cannot *be* the
//! session — instead the closure hands a clone of the connection out through a channel and then
//! parks on a shutdown signal, and the whole thing runs on one spawned task per session. Returning
//! from the closure is what closes the connection, which is why [`ConnectionHandle::shutdown`] is the
//! only thing that ends it.
//!
//! This works because 2.1.0's connection is `Send + Sync`: `ConnectTo` is `Send + 'static` and its
//! future is `Send`, so nothing here needs a `LocalSet` or a thread of its own, and a
//! `ConnectionTo<Agent>` sits inside a `Session` trait object directly.
//!
//! # Why handlers hold the dispatch loop
//!
//! The loop runs one handler to completion before the next message, so a notification handler that
//! awaits [`EventSink::emit`] on a full channel stops the agent being read — which is exactly the
//! backpressure the core's bounded turn channel exists to apply. The permission handler is the
//! opposite case: it must *not* wait for an answer, because the answer arrives through
//! [`Session::respond`](mango_external_agents::Session::respond) on another task. It parks the
//! agent's [`Responder`] in a map and returns, and answering is a synchronous send.
//!
//! No lock in this module is held across an await. [`Reducer`] is pure, so the events for one frame
//! are computed under the guard and emitted after it drops.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use agent_client_protocol::schema::v1::{
    RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder};
use mango_external_agents::event::{EventKind, SessionId};
use mango_external_agents::permission::{
    ApprovalDecision, DecisionSource, PermissionBroker, PermissionLevel, PermissionResponse,
    broker_response,
};
use mango_external_agents::session::CancelReason;
use mango_external_agents::{
    Clock, Error, EventSink, HostContext, ProcessControl, Result, VendorError,
};

use crate::error::vendor_error;
use crate::permission;
use crate::reducer::Reducer;
use crate::transport::LaunchedAgent;

/// Whether a host's answer reached the agent, or arrived after the question was already settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Answered {
    /// It reached the agent.
    Sent,
    /// The question was already settled, so nothing was sent.
    AlreadyResolved,
}

/// The sink and the configuration one running turn is served under.
///
/// Cloned out from under its lock before anything is emitted, so a handler parked on a full channel
/// is not holding the lock that `cancel`, `close` and `respond` need in order to get out of it.
#[derive(Clone)]
pub(crate) struct TurnHandle {
    pub(crate) sink: EventSink,
    /// What the agent may do for this turn, which is what decides a standing refusal.
    pub(crate) level: PermissionLevel,
}

/// Everything the dispatch loop's handlers and the session's own methods share.
pub(crate) struct SessionState {
    session_id: SessionId,
    clock: Arc<dyn Clock>,
    broker: Option<Arc<dyn PermissionBroker>>,
    /// How long an approval stays answerable, carried on every request the host sees.
    approval_timeout: Duration,
    turn: Mutex<Option<TurnHandle>>,
    /// Why the running turn was cancelled, when somebody said.
    ///
    /// ACP answers a cancelled `session/prompt` with `stop_reason: cancelled` and no reason of its
    /// own, so the reason a host gave has to be remembered between the notification going out and
    /// the response coming back. Flattening it would report a shutdown or a withdrawn consent as
    /// "you stopped this turn".
    cancel_reason: Mutex<Option<CancelReason>>,
    /// Touched only by the notification handler, which the dispatch loop runs one at a time.
    reducer: Mutex<Reducer>,
    /// Questions the agent is waiting on, keyed by the JSON-RPC id the answer routes back to.
    pending: Mutex<HashMap<String, Responder<RequestPermissionResponse>>>,
}

impl std::fmt::Debug for SessionState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionState")
            .field("session_id", &self.session_id)
            .field("has_turn", &self.turn().is_some())
            .field("pending_approvals", &self.pending_count())
            .finish_non_exhaustive()
    }
}

impl SessionState {
    pub(crate) fn new(session_id: SessionId, host: &HostContext) -> Self {
        Self {
            session_id,
            clock: Arc::clone(host.clock()),
            broker: host.broker().cloned(),
            approval_timeout: host.limits().request_timeout,
            turn: Mutex::new(None),
            cancel_reason: Mutex::new(None),
            reducer: Mutex::new(Reducer::new()),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Opens a turn, refusing a second one.
    ///
    /// ACP v1 has one `session/prompt` in flight per session: the response *is* the turn's end, so a
    /// second prompt would produce two turns racing for one stream of updates with no field on the
    /// wire to tell them apart. Refused rather than queued, because a host that thinks it queued a
    /// turn and a library that silently serialised them disagree about what has been sent.
    pub(crate) fn begin_turn(&self, handle: TurnHandle) -> Result<()> {
        let mut turn = self.lock_turn();
        if turn.is_some() {
            return Err(Error::Protocol {
                expected: String::from(
                    "no turn in flight: ACP v1 runs one session/prompt at a time",
                ),
                received: String::from("a turn that has not ended"),
            });
        }
        *turn = Some(handle);
        *self.lock_reducer() = Reducer::new();
        *self.lock_cancel_reason() = None;
        Ok(())
    }

    /// Records why the running turn is being cancelled.
    pub(crate) fn record_cancel_reason(&self, reason: CancelReason) {
        // First writer wins: a shutdown that arrives after a user's stop button is the second thing
        // to happen to a turn that is already ending, and the reason a person triggered is the one
        // worth reporting.
        self.lock_cancel_reason().get_or_insert(reason);
    }

    /// The reason a cancel recorded, if one did.
    pub(crate) fn take_cancel_reason(&self) -> Option<CancelReason> {
        self.lock_cancel_reason().take()
    }

    /// The running turn, if there is one.
    pub(crate) fn turn(&self) -> Option<TurnHandle> {
        self.lock_turn().clone()
    }

    /// Ends the turn and hands back its sink, so the caller can emit outside the lock.
    pub(crate) fn end_turn(&self) -> Option<TurnHandle> {
        self.lock_turn().take()
    }

    /// The events one frame produces, computed under the guard because the reducer is pure.
    fn reduce(&self, notification: SessionNotification) -> Vec<EventKind> {
        self.lock_reducer().update(notification.update)
    }

    /// The events that close out a turn: the other half of an open reasoning block.
    pub(crate) fn finish_reducing(&self) -> Vec<EventKind> {
        self.lock_reducer().finish()
    }

    /// Withdraws every question still waiting.
    ///
    /// Called wherever a turn ends, not only on `close`, because a question cannot outlive the turn it
    /// belongs to: its answer would be emitted into a sink that is already finished, and the agent
    /// would be told "allow" about a turn it stopped running. Dropping the responders instead would
    /// say nothing at all — the SDK's drop guard only answers batch requests — so each is answered
    /// `Cancelled`, which is ACP's own outcome for a question that was withdrawn rather than refused.
    ///
    /// Taken out from under the lock in one statement, so nothing is held while the answers go out.
    pub(crate) fn withdraw_pending(&self) {
        let pending: Vec<Responder<RequestPermissionResponse>> =
            self.lock_pending().drain().map(|(_, held)| held).collect();
        for responder in pending {
            let _ = responder.respond(permission::cancelled());
        }
    }

    /// Answers one question the host decided.
    ///
    /// A question nothing is waiting on is [`Answered::AlreadyResolved`] rather than a failure. Every
    /// way of reaching that state is a race a host can legitimately lose: the level's own standing
    /// refusal or a [`PermissionBroker`] answered before the host read the event, the turn ended
    /// under it, or two of the host's own tasks answered the same prompt. None of those is a host
    /// mistake, and failing the second caller would be the same bug
    /// [`Session::close`](mango_external_agents::Session::close) is documented not to have.
    ///
    /// What does *not* happen is a second answer reaching the agent. The decision the agent acted on
    /// is the one already sent, so the audit trail keeps that one rather than the one that lost.
    ///
    /// # Errors
    ///
    /// Whatever the transport reported while sending the answer.
    pub(crate) fn answer(&self, response: &PermissionResponse) -> Result<Answered> {
        let responder = self.lock_pending().remove(&response.request_id);
        let Some(responder) = responder else {
            return Ok(Answered::AlreadyResolved);
        };
        responder
            .respond(permission::selected(&response.option_id))
            .map_err(|error| Error::Vendor(vendor_error("session/request_permission", &error)))?;
        Ok(Answered::Sent)
    }

    fn pending_count(&self) -> usize {
        self.lock_pending().len()
    }

    fn lock_turn(&self) -> std::sync::MutexGuard<'_, Option<TurnHandle>> {
        self.turn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_reducer(&self) -> std::sync::MutexGuard<'_, Reducer> {
        self.reducer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_cancel_reason(&self) -> std::sync::MutexGuard<'_, Option<CancelReason>> {
        self.cancel_reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_pending(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, Responder<RequestPermissionResponse>>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> SystemTime {
        self.clock.now()
    }
}

/// One `session/update` notification, reduced and emitted.
///
/// Returns `Ok(())` even when the host has gone: the dispatch loop's contract is that a handler
/// error ends the whole connection, and a dropped turn stream is not a reason to tear down a session
/// the host may still be using. The turn is ended instead, so the next frame is dropped cheaply.
async fn on_session_update(state: &Arc<SessionState>, notification: SessionNotification) {
    // Cloned out from under its lock before the first emit: a handler parked on a full channel must
    // not be holding the lock that `cancel` and `close` need to unpark it.
    let Some(turn) = state.turn() else {
        return;
    };
    for kind in state.reduce(notification) {
        if turn.sink.emit(kind).await.is_err() {
            state.end_turn();
            return;
        }
    }
}

/// One `session/request_permission` request, brokered.
///
/// Never waits for a person: the dispatch loop is held for as long as this returns nothing, and the
/// answer arrives on another task. What it does before parking the question is ask the two things
/// that can answer without one:
///
/// 1. **The level.** Under [`PermissionLevel::ReadOnly`] the answer is a refusal, every time. That
///    is the one level the library can reach by answering, because refusing grants nothing — and it
///    is the host's own standing instruction rather than a decision the library made. There is
///    deliberately no counterpart for [`PermissionLevel::FullAccess`]: allowing on an agent's behalf
///    is the one thing nothing here may do, which is why that level needs the agent's own mode
///    instead (see [`crate::profile::matrix`]).
/// 2. **The broker.** A host policy, if the host installed one.
///
/// Otherwise the question reaches the host as an event and the turn waits.
async fn on_request_permission(
    state: &Arc<SessionState>,
    request: RequestPermissionRequest,
    responder: Responder<RequestPermissionResponse>,
) -> agent_client_protocol::Result<()> {
    let id = responder.id().to_string();
    let Some(turn) = state.turn() else {
        // No turn: nobody is reading, and an unanswered request would hold the agent forever.
        return responder.respond(permission::cancelled());
    };

    let question =
        permission::request_from(&request, id.clone(), state.now() + state.approval_timeout);
    let question = match question.normalized() {
        Ok(question) => question,
        // A question nobody could render — no options, or an id that cannot survive bounding — is
        // withdrawn on its own rather than ending the turn it belongs to.
        Err(_) => return responder.respond(permission::cancelled()),
    };

    // Decided before the host is told, so a policy the host already installed does not race the
    // interface the host would otherwise render. The dispatch loop is held while the broker thinks,
    // which is correct: the agent is waiting on this question either way.
    //
    // A `match` rather than `Option::or`, whose argument is evaluated either way: a read-only
    // session that already refused must not also consult the broker, or a host reading its own
    // policy's log would find a question the policy never decided.
    let decided = match standing_refusal(turn.level, &question) {
        Some(refusal) => Some(refusal),
        None => broker_response(state.broker.as_ref(), &question).await,
    };

    // An undecided question is parked *before* it is emitted. A host reading its own stream can
    // answer the instant it sees the event, and an entry inserted afterwards would lose that race
    // and refuse the answer as one nothing is waiting on.
    let held = match decided.is_some() {
        true => Some(responder),
        false => {
            state.lock_pending().insert(id.clone(), responder);
            None
        }
    };

    if turn
        .sink
        .emit(EventKind::ApprovalRequested {
            request: question.clone(),
        })
        .await
        .is_err()
    {
        state.end_turn();
        // Whichever side still holds the responder answers: a host answer can no longer arrive.
        let orphan = held.or_else(|| state.lock_pending().remove(&id));
        return match orphan {
            Some(responder) => responder.respond(permission::cancelled()),
            None => Ok(()),
        };
    }

    if let (Some(decision), Some(responder)) = (decided, held) {
        responder.respond(permission::selected(&decision.option_id))?;
        let _ = turn
            .sink
            .emit(EventKind::ApprovalResolved {
                request_id: question.id,
                decision: ApprovalDecision {
                    option_id: decision.option_id,
                    source: decision.source,
                },
            })
            .await;
    }
    Ok(())
}

/// The refusal a read-only session owes every question, when the agent offered one.
///
/// `None` when the level allows acting, and also when the agent offered no way to refuse — there is
/// nothing to answer with, so the question goes to a person, which is the same thing
/// [`broker_response`] does in that case.
fn standing_refusal(
    level: PermissionLevel,
    question: &mango_external_agents::PermissionRequest,
) -> Option<PermissionResponse> {
    if level != PermissionLevel::ReadOnly {
        return None;
    }
    Some(
        question
            .deny()
            .ok()?
            .with_source(DecisionSource::AutoReview),
    )
}

/// A live connection to one agent, and the child behind it.
pub(crate) struct ConnectionHandle {
    connection: ConnectionTo<Agent>,
    control: Arc<dyn ProcessControl>,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    driver: Mutex<Option<tokio::task::JoinHandle<agent_client_protocol::Result<()>>>>,
}

impl std::fmt::Debug for ConnectionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionHandle")
            .field("pid", &self.control.pid())
            .finish_non_exhaustive()
    }
}

/// Starts the dispatch loop for one agent and hands back the connection it opened.
///
/// # Errors
///
/// [`Error::Link`] when the connection ended before it produced a handle, carrying the child's
/// stderr tail — which is what an agent that printed a usage message and exited looks like from
/// here.
pub(crate) async fn drive(
    launched: LaunchedAgent,
    state: Arc<SessionState>,
    client_name: String,
) -> Result<ConnectionHandle> {
    let LaunchedAgent { transport, control } = launched;
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let notifications = Arc::clone(&state);
    let approvals = Arc::clone(&state);
    let driver = tokio::spawn(
        Client
            .builder()
            .name(client_name)
            .on_receive_notification(
                async move |notification: SessionNotification, _cx| {
                    on_session_update(&notifications, notification).await;
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                async move |request: RequestPermissionRequest, responder, _cx| {
                    on_request_permission(&approvals, request, responder).await
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
                // The closure *is* the connection's lifetime, so it hands a clone out and parks.
                // Returning here shuts the dispatch loop down, which is why only `shutdown` does.
                let _ = ready_tx.send(connection.clone());
                let _ = shutdown_rx.await;
                Ok(())
            }),
    );

    let Ok(connection) = ready_rx.await else {
        // The closure never ran, so the transport failed first. The child's own stderr is the only
        // thing that can say why.
        let message = match driver.await {
            Ok(Err(error)) => error.to_string(),
            Ok(Ok(())) => String::from("the connection closed before it opened"),
            Err(error) => error.to_string(),
        };
        return Err(Error::Link {
            peer: String::from("ACP agent"),
            message: with_stderr(&message, control.as_ref()),
        });
    };

    Ok(ConnectionHandle {
        connection,
        control,
        shutdown: Mutex::new(Some(shutdown_tx)),
        driver: Mutex::new(Some(driver)),
    })
}

impl ConnectionHandle {
    /// The connection, for a request a session method sends.
    ///
    /// Calls from a session method run outside the dispatch loop, which is the condition
    /// `SentRequest::block_task` documents: awaiting a response there is safe, and awaiting one
    /// inside a handler would deadlock the loop that has to route it.
    pub(crate) fn connection(&self) -> &ConnectionTo<Agent> {
        &self.connection
    }

    /// The child, for diagnostics and for ending it.
    pub(crate) fn control(&self) -> &Arc<dyn ProcessControl> {
        &self.control
    }

    /// Ends the dispatch loop and the child. Idempotent.
    ///
    /// Closing the connection first is what lets a well-behaved agent see its stdin end and exit on
    /// its own; the kill is the escalation for one that does not, and it is the launcher's own —
    /// this only asks, with the reason.
    pub(crate) async fn shutdown(&self, reason: mango_external_agents::CancelReason) {
        let signal = self
            .shutdown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(signal) = signal {
            let _ = signal.send(());
        }
        let driver = self
            .driver
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(driver) = driver {
            // Bounded: an agent that never releases the transport must not hold a close open.
            let _ = tokio::time::timeout(SHUTDOWN_GRACE, driver).await;
        }
        let _ = self.control.kill(reason).await;
    }
}

/// How long the dispatch loop is given to wind down before the child is ended anyway.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Sends one request under the host's own deadline and maps whatever came back.
///
/// Every request this harness sends goes through here **except** `session/prompt`, which is a turn
/// and may legitimately take as long as the agent needs. The rest are handshakes and bookkeeping: an
/// agent that accepted the pipe and never answered `initialize` would otherwise hold `open_session`
/// open for the life of the process, which looks to a host exactly like a hung machine rather than a
/// misbehaving agent. `Limits::request_timeout` is the number the host already set for this.
///
/// # Errors
///
/// [`Error::Timeout`] when the deadline passed, [`Error::Link`] when the transport went away — with
/// the child's own stderr, which is the only thing that can explain an agent that exited mid-call —
/// and otherwise whatever [`request_error`](crate::error::request_error) made of the agent's answer.
pub(crate) async fn send<Request>(
    connection: &ConnectionHandle,
    profile: &crate::profile::AcpProfile,
    timeout: Duration,
    method: &'static str,
    request: Request,
) -> Result<Request::Response>
where
    Request: agent_client_protocol::JsonRpcRequest,
    Request::Response: Send,
{
    let sent = connection.connection().send_request(request);
    let answered = tokio::time::timeout(timeout, sent.block_task()).await;
    let Ok(answered) = answered else {
        return Err(Error::Timeout {
            operation: format!("{method} on ACP agent {}", profile.id),
            after: timeout,
        });
    };
    answered.map_err(|error| {
        if agent_client_protocol::is_incoming_transport_closed(&error) {
            return Error::Link {
                peer: format!("ACP agent {}", profile.id),
                message: with_stderr(&error.message, connection.control().as_ref()),
            };
        }
        crate::error::request_error(method, &error, &profile.login_text())
    })
}

/// A message with the child's stderr appended, when it wrote any.
///
/// The tail arrives already redacted from [`StderrTail`](mango_external_agents::StderrTail) — this
/// only decides whether there is anything worth appending.
pub(crate) fn with_stderr(message: &str, control: &dyn ProcessControl) -> String {
    let tail = control.stderr_tail();
    let tail = tail.trim();
    if tail.is_empty() {
        return message.to_owned();
    }
    format!("{message}; the agent's stderr: {tail}")
}

/// The failure a turn ends with when the agent's link went away under it.
pub(crate) fn link_failure(message: String) -> VendorError {
    VendorError::new(
        mango_external_agents::ErrorCode::from_static("acp-link-closed"),
        message,
    )
}
