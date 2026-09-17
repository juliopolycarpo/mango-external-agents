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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

use agent_client_protocol::schema::v1::{
    CancelNotification, RequestPermissionRequest, RequestPermissionResponse, SessionNotification,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Responder};
use mango_external_agents::approval::ApprovalDeadline;
use mango_external_agents::event::{EventKind, SessionId};
use mango_external_agents::permission::{
    ApprovalDecision, DecisionSource, PermissionBroker, PermissionLevel, PermissionResponse,
    broker_response,
};
use mango_external_agents::session::{CancelReason, Configuration};
use mango_external_agents::{
    Clock, Error, EventSink, HostContext, ProcessControl, Result, VendorError,
};

use crate::approval_events::ApprovalEvents;
use crate::error::vendor_error;
use crate::permission;
use crate::reducer::Reducer;
use crate::transport::LaunchedAgent;

/// Whether a host's answer reached the agent, or arrived after the question was already settled.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    pub(crate) level: Option<PermissionLevel>,
    /// Which turn this is, so only its own owner can end it.
    ///
    /// Without it, `end_turn` takes whatever handle is present — and a `session/prompt` task that
    /// answered late would end, and emit the terminal of, a turn that is not its own.
    generation: u64,
    /// Set by whoever emitted this turn's terminal.
    ///
    /// A handler holding a clone can be parked mid-frame while `close` emits the terminal on another
    /// task; resuming afterwards would put the rest of that frame's events *after* the terminal,
    /// which the core's conformance suite refuses. Checked before every emit.
    finished: Arc<AtomicBool>,
    pub(crate) approvals: Arc<ApprovalEvents>,
}

/// One agent request waiting for either the harness or the host to answer it.
///
/// A request remains harness-owned while a standing refusal or broker decision is in progress. Only
/// an undecided request becomes host-answerable, immediately before its event is emitted.
struct PendingApproval {
    responder: Responder<RequestPermissionResponse>,
    host_answerable: bool,
    question: mango_external_agents::PermissionRequest,
    announced: bool,
    deadline: ApprovalDeadline,
    expiry_option: Option<String>,
    connection: ConnectionTo<Agent>,
    cancel: CancelNotification,
    turn: TurnHandle,
    /// Dropping a resolved question stops its timer task.
    _timer_done: tokio::sync::oneshot::Sender<()>,
}

impl PendingApproval {
    /// Register only a question that is answerable or has already reached its deadline.
    fn announce(&mut self) {
        if self.announced {
            return;
        }
        self.announced = true;
        self.turn.approvals.push(EventKind::ApprovalRequested {
            request: self.question.clone(),
        });
    }

    /// Publish a successful decision before prompt completion can flush the terminal.
    fn respond_with_resolution(
        self,
        response: RequestPermissionResponse,
        option_id: String,
        source: DecisionSource,
    ) -> agent_client_protocol::Result<()> {
        let event = EventKind::ApprovalResolved {
            request_id: self.question.id.clone(),
            decision: ApprovalDecision { option_id, source },
        };
        self.turn
            .approvals
            .record_response(event, || self.responder.respond(response))
    }
}

impl TurnHandle {
    /// Whether this turn's terminal has already gone out.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Claims the right to emit this turn's terminal, once.
    pub(crate) fn finish(&self) -> bool {
        !self.finished.swap(true, Ordering::AcqRel)
    }
}

/// Everything the dispatch loop's handlers and the session's own methods share.
pub(crate) struct SessionState {
    session_id: SessionId,
    clock: Arc<dyn Clock>,
    broker: Option<Arc<dyn PermissionBroker>>,
    /// The host limits used to form every approval deadline.
    limits: mango_external_agents::Limits,
    turn: Mutex<Option<TurnHandle>>,
    /// The explicit settings the next turn inherits.
    ///
    /// `None` on either permission axis leaves the vendor's own setting in force. Turning that
    /// absence into `ReadOnly` would alter an agent merely because a host omitted an override.
    configuration: Mutex<Configuration>,
    /// Serialises merging, accepting, and recording one turn's configuration.
    ///
    /// The prompt slot alone is not enough: a second caller could read the old inherited settings
    /// before the first one accepts a turn, then overwrite the first caller's new setting after it
    /// finishes. This guard covers that synchronous read-modify-write sequence and is always
    /// dropped before an event can await a host.
    turn_start: Mutex<()>,
    /// Stamped onto each turn so a late `session/prompt` task can prove a handle is its own.
    generations: AtomicU64,
    /// Why the running turn was cancelled, when somebody said.
    ///
    /// ACP answers a cancelled `session/prompt` with `stop_reason: cancelled` and no reason of its
    /// own, so the reason a host gave has to be remembered between the notification going out and
    /// the response coming back. Flattening it would report a shutdown or a withdrawn consent as
    /// "you stopped this turn".
    cancel_reason: Mutex<Option<CancelReason>>,
    /// Touched only by the notification handler, which the dispatch loop runs one at a time.
    reducer: Mutex<Reducer>,
    /// Questions the agent is waiting on, keyed by [`SessionState::mint_approval_id`]'s id.
    ///
    /// Not the JSON-RPC id `session/request_permission` arrived on: that id is the peer's to choose
    /// and a peer is free to reuse it once the request it named is no longer outstanding — expired,
    /// answered, or withdrawn. A key drawn from the wire would let a host answer that was already in
    /// flight for the first request land on a second, unrelated one that reused the same id, because
    /// nothing here distinguishes "the question this answer was written for" from "whatever question
    /// currently sits at this id". Minting the key ourselves, once per question and never reused,
    /// closes that off: a stale answer's key can only ever name the question it was issued for.
    pending: Mutex<HashMap<String, PendingApproval>>,
    /// Source of [`Self::mint_approval_id`]'s ids. Separate from `generations`, which stamps turns.
    next_approval: AtomicU64,
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
    pub(crate) fn new(
        session_id: SessionId,
        host: &HostContext,
        configuration: Configuration,
    ) -> Self {
        Self {
            session_id,
            clock: Arc::clone(host.clock()),
            broker: host.broker().cloned(),
            limits: *host.limits(),
            turn: Mutex::new(None),
            configuration: Mutex::new(configuration),
            turn_start: Mutex::new(()),
            generations: AtomicU64::new(0),
            cancel_reason: Mutex::new(None),
            reducer: Mutex::new(Reducer::new()),
            pending: Mutex::new(HashMap::new()),
            next_approval: AtomicU64::new(0),
        }
    }

    /// A host-facing approval id this session has never handed out before.
    ///
    /// See the note on [`Self::pending`] for why this cannot be the JSON-RPC id instead.
    fn mint_approval_id(&self) -> String {
        format!(
            "approval-{}",
            self.next_approval.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Opens a turn, refusing a second one.
    ///
    /// ACP v1 has one `session/prompt` in flight per session: the response *is* the turn's end, so a
    /// second prompt would produce two turns racing for one stream of updates with no field on the
    /// wire to tell them apart. Refused rather than queued, because a host that thinks it queued a
    /// turn and a library that silently serialised them disagree about what has been sent.
    /// Returns the turn's own handle, which its `session/prompt` task keeps in order to end it.
    pub(crate) fn begin_turn(
        &self,
        sink: EventSink,
        level: Option<PermissionLevel>,
    ) -> Result<TurnHandle> {
        let mut turn = self.lock_turn();
        if turn.is_some() {
            return Err(Error::Protocol {
                expected: String::from(
                    "no turn in flight: ACP v1 runs one session/prompt at a time",
                ),
                received: String::from("a turn that has not ended"),
            });
        }
        // A question belongs to a turn, and a turn that is starting has none. Anything still parked
        // here outlived the turn it was asked under and would otherwise be answerable during this one.
        //
        // Drained before the new handle is published, and under the same guard. Published first, a
        // `session/request_permission` arriving in between would read the *new* turn out of the slot,
        // park itself, and then be withdrawn by a drain meant for its predecessor. The permission
        // handler's first act is to take this guard, so holding it here closes the window; the
        // answers themselves go out after it drops.
        let stale = self.take_pending_responders();
        let handle = TurnHandle {
            sink,
            level,
            generation: self.generations.fetch_add(1, Ordering::Relaxed),
            finished: Arc::new(AtomicBool::new(false)),
            approvals: Arc::new(ApprovalEvents::default()),
        };
        *turn = Some(handle.clone());
        *self.lock_reducer() = Reducer::new();
        *self.lock_cancel_reason() = None;
        drop(turn);
        for responder in stale {
            let _ = responder.respond(permission::cancelled());
        }
        Ok(handle)
    }

    /// The explicit settings a later turn inherits.
    pub(crate) fn configuration(&self) -> Configuration {
        self.lock_configuration().clone()
    }

    /// Records settings only after their turn won ACP's single prompt slot.
    pub(crate) fn accept_configuration(&self, configuration: Configuration) {
        *self.lock_configuration() = configuration;
    }

    /// Guards one synchronous configuration merge and prompt-slot claim.
    pub(crate) fn lock_turn_start(&self) -> std::sync::MutexGuard<'_, ()> {
        self.turn_start
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Begins cancellation and withdraws every question that was already parked.
    ///
    /// The reason guard covers registration and every selected answer, so a late permission request
    /// cannot be parked after this drains and an already parked request cannot be allowed after this
    /// call starts. The first reason wins because it is the action a host initiated first.
    pub(crate) fn begin_cancellation(&self, reason: CancelReason) -> bool {
        let turn = self.lock_turn();
        if turn.is_none() {
            return false;
        }
        let mut cancelling = self.lock_cancel_reason();
        cancelling.get_or_insert(reason);
        let pending = self.take_pending_responders();
        drop(cancelling);
        drop(turn);
        for responder in pending {
            let _ = responder.respond(permission::cancelled());
        }
        true
    }

    /// Whether the current prompt has been cancelled before its wire request was sent.
    pub(crate) fn is_cancelling(&self) -> bool {
        self.lock_cancel_reason().is_some()
    }

    /// The running turn, if there is one.
    pub(crate) fn turn(&self) -> Option<TurnHandle> {
        self.lock_turn().clone()
    }

    /// Whether it is still safe to submit this handle's ACP prompt.
    pub(crate) fn can_submit_prompt(&self, handle: &TurnHandle) -> bool {
        self.lock_turn()
            .as_ref()
            .is_some_and(|current| current.generation == handle.generation)
    }

    /// Ends whatever turn is running, whoever it belongs to.
    ///
    /// For `close`, which ends the session and therefore every turn in it.
    pub(crate) fn end_turn(&self) -> Option<TurnHandle> {
        self.lock_turn().take()
    }

    /// Ends this turn, and only this turn.
    ///
    /// What a `session/prompt` task calls. Its own turn may already have been ended by a `close`, and
    /// a *later* turn may have started in the meantime — so an unconditional take would let a task
    /// that answered late end, and emit the terminal of, a conversation that is not its own.
    pub(crate) fn end_turn_matching(
        &self,
        handle: &TurnHandle,
    ) -> Option<(TurnHandle, Option<CancelReason>, Vec<EventKind>)> {
        let (turn, reason, pending, closing) = {
            let mut active = self.lock_turn();
            if active.as_ref()?.generation != handle.generation {
                return None;
            }
            let turn = active.take()?;
            // These are all owned by the current turn. Capture them before releasing the slot: a
            // newly started prompt clears the reducer and may park approvals of its own.
            let reason = self.lock_cancel_reason().take();
            let pending = self.take_pending_responders();
            let closing = self.lock_reducer().finish();
            (turn, reason, pending, closing)
        };
        for responder in pending {
            let _ = responder.respond(permission::cancelled());
        }
        Some((turn, reason, closing))
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
        for responder in self.take_pending_responders() {
            let _ = responder.respond(permission::cancelled());
        }
    }

    /// Empties the question map, handing back the responders each one still owes an answer.
    ///
    /// The four places a turn can end all owe the same debt, and each of them takes the responders
    /// out in one statement so nothing is held while the answers go out.
    fn take_pending_responders(&self) -> Vec<Responder<RequestPermissionResponse>> {
        self.lock_pending()
            .drain()
            .map(|(_, held)| held.responder)
            .collect()
    }

    /// Parks an agent question unless cancellation already won its race.
    ///
    /// Cancellation holds the same guard before draining pending responders. A request that arrives
    /// after `session/cancel` must receive ACP's `Cancelled` outcome rather than wait forever for a
    /// host event that cancellation made obsolete.
    fn park_pending(
        &self,
        id: String,
        pending: PendingApproval,
    ) -> agent_client_protocol::Result<bool> {
        let cancelling = self.lock_cancel_reason();
        if cancelling.is_some() {
            drop(cancelling);
            pending.responder.respond(permission::cancelled())?;
            return Ok(false);
        }
        self.lock_pending().insert(id, pending);
        Ok(true)
    }

    /// Publishes a broker-settled request, making an undecided one answerable by the host.
    pub(crate) fn announce_pending(&self, id: &str, host_answerable: bool) -> bool {
        let cancelling = self.lock_cancel_reason();
        if cancelling.is_some() {
            return false;
        }
        let mut pending = self.lock_pending();
        let Some(pending) = pending.get_mut(id) else {
            return false;
        };
        pending.host_answerable = host_answerable;
        pending.announce();
        true
    }

    /// Sends one pending answer, withdrawing it when cancellation got there first.
    ///
    /// The cancellation guard stays held through the synchronous JSON-RPC response so a decision
    /// cannot allow work after `Session::cancel` has begun.
    pub(crate) fn respond_pending(
        &self,
        id: &str,
        requested: RequestPermissionResponse,
        source: DecisionSource,
    ) -> agent_client_protocol::Result<Answered> {
        let mut cancelling = self.lock_cancel_reason();
        let pending = self.lock_pending().remove(id);
        let Some(mut pending) = pending else {
            return Ok(Answered::AlreadyResolved);
        };
        if cancelling.is_some() {
            pending.responder.respond(permission::cancelled())?;
            return Ok(Answered::AlreadyResolved);
        }
        if !pending.deadline.is_elapsed() {
            if let agent_client_protocol::schema::v1::RequestPermissionOutcome::Selected(outcome) =
                &requested.outcome
            {
                let option_id = outcome.option_id.to_string();
                pending.respond_with_resolution(requested, option_id, source)?;
                return Ok(Answered::Sent);
            }
            pending.responder.respond(requested)?;
            return Ok(Answered::Sent);
        }

        pending.announce();
        let Some(option_id) = pending.expiry_option.as_ref() else {
            cancelling.get_or_insert(CancelReason::Timeout);
            self.withdraw_pending();
            let sent = pending.connection.send_notification(pending.cancel);
            pending.responder.respond(permission::cancelled())?;
            sent?;
            return Ok(Answered::AlreadyResolved);
        };
        let outcome = permission::selected(option_id);
        let option_id = option_id.clone();
        pending.respond_with_resolution(outcome, option_id, DecisionSource::Expired)?;
        Ok(Answered::AlreadyResolved)
    }

    /// Withdraws one pending request when its turn cannot continue.
    pub(crate) fn withdraw_pending_by_id(&self, id: &str) -> agent_client_protocol::Result<()> {
        let pending = self.lock_pending().remove(id);
        match pending {
            Some(pending) => pending.responder.respond(permission::cancelled()),
            None => Ok(()),
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
    /// Nor does an answer reach the agent once the turn it belongs to has finished. A host's
    /// `respond` can win the race against a `close` that has already claimed the terminal, and
    /// forwarding its choice then would let an allowing option out during teardown. The question is
    /// withdrawn instead, which is what the turn's end owed it anyway.
    ///
    /// Like the matching guard in [`on_request_permission`], this one has no test: reaching it needs a
    /// `respond` to land between `close`'s `finish` and its `withdraw_pending`, two adjacent statements
    /// with no await between them, and `close` has torn the transport down by the time it returns. It
    /// is two lines of defence in depth, not a covered path.
    ///
    /// # Errors
    ///
    /// Whatever the transport reported while sending the answer.
    pub(crate) fn answer(&self, response: &PermissionResponse) -> Result<Answered> {
        {
            let pending = self.lock_pending();
            let Some(pending) = pending
                .get(&response.request_id)
                .filter(|pending| pending.host_answerable)
            else {
                return Ok(Answered::AlreadyResolved);
            };
            if !pending
                .question
                .options
                .iter()
                .any(|option| option.id == response.option_id)
            {
                return Err(Error::Protocol {
                    // The count, never the ids: an option id is the agent's own string, and
                    // `Display` writes this shape verbatim.
                    expected: format!(
                        "one of the {} approval option ids this question offered",
                        pending.question.options.len()
                    ),
                    received: response.option_id.clone(),
                });
            }
        }
        let alive = self.turn().is_some_and(|turn| !turn.is_finished());
        let outcome = match alive {
            true => permission::selected(&response.option_id),
            false => permission::cancelled(),
        };
        let answered = self
            .respond_pending(&response.request_id, outcome, response.source)
            .map_err(|error| Error::Vendor(vendor_error("session/request_permission", &error)))?;
        match alive {
            true => Ok(answered),
            false => Ok(Answered::AlreadyResolved),
        }
    }

    fn pending_count(&self) -> usize {
        self.lock_pending().len()
    }

    fn lock_turn(&self) -> std::sync::MutexGuard<'_, Option<TurnHandle>> {
        self.turn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_configuration(&self) -> std::sync::MutexGuard<'_, Configuration> {
        self.configuration
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_reducer(&self) -> std::sync::MutexGuard<'_, Reducer> {
        self.reducer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_cancel_reason(&self) -> std::sync::MutexGuard<'_, Option<CancelReason>> {
        self.cancel_reason
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, PendingApproval>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn now(&self) -> SystemTime {
        self.clock.now()
    }
}

/// One `session/update` notification, reduced and emitted.
///
/// Never fails the handler, and — deliberately — never ends the turn. A host that dropped its
/// `TurnStream` has closed the sink, not finished the `session/prompt` that is still in flight, so
/// freeing the turn slot here would let a second prompt onto a wire that has no way to tell two turns
/// apart, and would hand this turn's completion task a *later* turn's handle to terminate. The slot is
/// the prompt's to release; a closed sink simply makes every later frame fail fast.
async fn on_session_update(state: &Arc<SessionState>, notification: SessionNotification) {
    // Cloned out from under its lock before the first emit: a handler parked on a full channel must
    // not be holding the lock that `cancel` and `close` need to unpark it.
    let Some(turn) = state.turn() else {
        return;
    };
    for kind in state.reduce(notification) {
        // Re-checked each time round: this handler can be parked on a full channel while `close`
        // emits the terminal on another task, and resuming would put the rest of this frame after it.
        if turn.is_finished() || turn.sink.emit(kind).await.is_err() {
            return;
        }
    }
}

/// One `session/request_permission` request, brokered.
///
/// Never waits for a person: the dispatch loop is held for as long as this returns nothing, and the
/// answer arrives on another task. It first parks the question under the cancellation guard, then
/// asks the two things that can answer without a person:
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
    connection: ConnectionTo<Agent>,
) -> agent_client_protocol::Result<()> {
    let id = state.mint_approval_id();
    let Some(turn) = state.turn() else {
        // No turn: nobody is reading, and an unanswered request would hold the agent forever.
        return responder.respond(permission::cancelled());
    };
    if turn.is_finished() {
        // The turn's terminal has gone out — a `close` is in progress, and its `session/close`
        // handshake is the window this arrives in. Withdrawn rather than parked or decided: nothing
        // may grant a permission while the session is being torn down, and a responder parked now
        // would be left waiting when the transport goes.
        return responder.respond(permission::cancelled());
    }

    // One read, reused below: a second `state.now()` call to translate `expires_at` back into a
    // deadline would let a host clock that moved backward between the two reads extend the
    // monotonic approval window past what the wire advertised.
    let now = state.now();
    let Ok(expires_at) = state.limits.approval_expires_at(now) else {
        return responder.respond(permission::cancelled());
    };
    let question = permission::request_from(&request, id.clone(), expires_at);
    let question = match question.normalized() {
        Ok(question) => question,
        // A question nobody could render — no options, or an id that cannot survive bounding — is
        // withdrawn on its own rather than ending the turn it belongs to.
        Err(_) => return responder.respond(permission::cancelled()),
    };

    let Some(deadline) = ApprovalDeadline::new(question.expires_at, now) else {
        return responder.respond(permission::cancelled());
    };
    let (timer_done, done) = tokio::sync::oneshot::channel();
    let pending = PendingApproval {
        responder,
        host_answerable: false,
        question: question.clone(),
        announced: false,
        deadline,
        // The same choice the standing refusal makes, through the core's own preference order:
        // the one-time refusal when the agent offered one, the standing refusal otherwise. Matching
        // only `RejectOnce` here would leave an agent that offers `reject_always` alone with no
        // expiry answer, and the branch below cancels the whole turn when there is none.
        expiry_option: question.deny().ok().map(|refusal| refusal.option_id),
        connection,
        cancel: CancelNotification::new(request.session_id),
        turn: turn.clone(),
        _timer_done: timer_done,
    };
    if !state.park_pending(id.clone(), pending)? {
        return Ok(());
    }

    expire_pending(state, &turn, id.clone(), deadline, done);

    // Decided before the host is told, so a policy the host already installed does not race the
    // interface the host would otherwise render. The dispatch loop is held while the broker thinks,
    // which is correct: the agent is waiting on this question either way.
    //
    // The level decides *whether* the broker is asked at all, and that is the whole point. A
    // read-only session must never reach `broker_response`, because a policy answering `Allow` there
    // becomes an allowing option id on the wire — so the one level that exists to grant nothing would
    // grant. When a read-only session cannot refuse (the agent offered no refusing option) the answer
    // is nobody's but a person's, which is what leaving `decided` empty arranges.
    let decided = match turn.level {
        Some(PermissionLevel::ReadOnly) => standing_refusal(&question),
        Some(PermissionLevel::Default | PermissionLevel::FullAccess) | None => deadline
            .run(broker_response(state.broker.as_ref(), &question))
            .await
            .flatten(),
    };

    state.announce_pending(&id, decided.is_none());
    if turn.approvals.flush(&turn.sink).await.is_err() {
        return state.withdraw_pending_by_id(&id);
    }
    if let Some(decision) = decided {
        state.respond_pending(
            &id,
            permission::selected(&decision.option_id),
            decision.source,
        )?;
        let _ = turn.approvals.flush(&turn.sink).await;
    }
    Ok(())
}

/// Starts enforcement before broker deliberation or event backpressure can park the handler.
fn expire_pending(
    state: &Arc<SessionState>,
    turn: &TurnHandle,
    id: String,
    deadline: ApprovalDeadline,
    done: tokio::sync::oneshot::Receiver<()>,
) {
    let state = Arc::downgrade(state);
    let turn = turn.clone();
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = done => return,
            () = deadline.wait() => {},
        }
        let Some(state) = state.upgrade() else {
            return;
        };
        // respond_pending registers the resolution before its response can complete the prompt.
        let _ = state.respond_pending(&id, permission::cancelled(), DecisionSource::Expired);
        let _ = turn.approvals.flush(&turn.sink).await;
    });
}

/// The refusal a read-only session owes a question, when the agent offered a way to refuse.
///
/// `None` when it did not: there is nothing to answer with, so the question goes to a person. The
/// caller must **not** fall back to the broker in that case — a policy answering `Allow` would put an
/// allowing option id on the wire, and read-only exists precisely so that cannot happen. See
/// [`on_request_permission`].
fn standing_refusal(
    question: &mango_external_agents::PermissionRequest,
) -> Option<PermissionResponse> {
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
/// [`Error::Link`] when the connection ended before it produced a handle, naming which of the
/// three shapes it was: a transport that failed, a connection that closed, or a task that did not
/// finish. The child's own stderr — which is what an agent that printed a usage message and exited
/// looks like from here — stays on [`StderrTail`](mango_external_agents::StderrTail), reachable
/// through the [`ProcessControl`] the host holds, because `Error::Link`'s summary is written
/// verbatim by `Display`.
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
                async move |request: RequestPermissionRequest, responder, cx| {
                    on_request_permission(&approvals, request, responder, cx).await
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
        // The closure never ran, so the transport failed first. `Error::Link`'s summary is written
        // verbatim by `Display`, so it says which of the three shapes this was and nothing the
        // driver or the child put into words; the redacted stderr tail stays on `StderrTail`,
        // where a host that wants it reads it.
        let message = match driver.await {
            Ok(Err(_)) => "a transport that failed before the connection opened",
            Ok(Ok(())) => "a connection that closed before it opened",
            Err(_) => "a connection task that did not finish",
        };
        return Err(Error::Link {
            peer: String::from("ACP agent"),
            message: String::from(message),
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
/// [`Error::Timeout`] when the deadline passed, [`Error::Link`] naming the call the transport
/// closed under, and otherwise whatever [`request_error`](crate::error::request_error) made of the
/// agent's answer. The child's own stderr — the only thing that can explain an agent that exited
/// mid-call — stays on [`StderrTail`](mango_external_agents::StderrTail) rather than on the error,
/// and the turn's own failure event still carries it.
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
                message: format!("a transport that closed under {method}"),
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mango_external_agents::testing::FakeLauncher;
    use mango_external_agents::{Configuration, EventSink, HostContext, TurnId};

    use super::{CancelReason, PermissionLevel, SessionId, SessionState};

    fn state() -> (SessionState, HostContext) {
        let host = HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd(std::env::temp_dir())
            .client_info("acp-client-tests", "0.1.0")
            .build()
            .expect("expected a host context");
        let state = SessionState::new(SessionId::new("session-1"), &host, Configuration::default());
        (state, host)
    }

    fn sink(host: &HostContext, turn_id: &str) -> EventSink {
        let (sink, _events) = EventSink::new(
            SessionId::new("session-1"),
            TurnId::new(turn_id),
            Arc::clone(host.clock()),
            1,
        );
        sink
    }

    /// The cancellation reason belongs to the prompt that was cancelled, even when another prompt
    /// acquires the slot immediately after its predecessor finishes.
    #[test]
    fn ending_a_turn_captures_its_cancellation_before_the_next_turn_starts() {
        let (state, host) = state();
        let first = state
            .begin_turn(sink(&host, "turn-1"), Some(PermissionLevel::Default))
            .expect("expected the first turn");
        assert!(state.begin_cancellation(CancelReason::ConsentRevoked));

        let (_, reason, _) = state
            .end_turn_matching(&first)
            .expect("expected the first turn to still own the slot");
        state
            .begin_turn(sink(&host, "turn-2"), Some(PermissionLevel::Default))
            .expect("expected the second turn to acquire the released slot");

        assert_eq!(reason, Some(CancelReason::ConsentRevoked));
    }

    /// A close can end a turn after its handle exists but before the ACP prompt is written. That
    /// detached handle must never submit work after the close won the prompt slot.
    #[test]
    fn an_ended_handle_cannot_submit_a_prompt() {
        let (state, host) = state();
        let first = state
            .begin_turn(sink(&host, "turn-1"), Some(PermissionLevel::Default))
            .expect("expected the first turn");
        state.end_turn();

        assert!(
            !state.can_submit_prompt(&first),
            "an ended handle must not retain authority to submit a prompt"
        );
    }
}
