//! The client side of the connection: the driving model, the handlers, and the shared turn state.
//!
//! # How the connection is driven
//!
//! The connection is the repository's own JSON-RPC peer,
//! [`jsonrpc::Client`](mango_external_agents::jsonrpc::Client), over the line-framed link
//! [`transport::frame`](crate::transport::frame) builds. The peer reads on a task of its own,
//! and hands each `session/update` and each of the agent's questions to [`AcpHandler`] in arrival
//! order on a second task, a question with the responder that answers it. A question is answered
//! there or parked; none is waited on. A [`ConnectionHandle`] owns the peer and
//! the child behind it; [`ConnectionHandle::shutdown`] is the one place both end.
//!
//! Every request this side makes asks for its answer behind the notifications that arrived before
//! it. A `session/prompt` answer therefore reaches the prompt's owner only after the last
//! `session/update` of the turn was reduced and emitted, and a `session/set_config_option` answer
//! only after a `config_option_update` the agent sent ahead of it.
//!
//! # Why handlers return promptly
//!
//! The peer awaits one notification call before the next. [`EventSink::emit`] therefore only
//! reserves bounded transcript capacity and returns; an overflow owns a terminal outcome instead
//! of parking the connection behind a slow host. A permission request likewise parks its reply
//! and moves broker deliberation to a bounded task, because an answer may arrive through
//! [`Session::respond`](mango_external_agents::Session::respond) on another task.
//!
//! No lock in this module is held across an await. [`Reducer`] is pure, so the events for one frame
//! are computed under the guard and emitted after it drops.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::SystemTime;

use agent_client_protocol_schema::v1::{
    AGENT_METHOD_NAMES, CLIENT_METHOD_NAMES, CancelNotification, RequestPermissionRequest,
    RequestPermissionResponse, SessionNotification, SessionUpdate,
};
use mango_external_agents::approval::ApprovalDeadline;
use mango_external_agents::configuration::{
    Configuration, ConfigurationCatalog, ConfigurationState,
};
use mango_external_agents::event::{ActivityStatus, EventKind, SessionId};
use mango_external_agents::jsonrpc::{self, CancellationHold, PeerHandler, ServerRequestOutcome};
use mango_external_agents::permission::{
    ApprovalDecision, DecisionSource, PermissionBroker, PermissionLevel, PermissionResponse,
    broker_response,
};
use mango_external_agents::session::CancelReason;
use mango_external_agents::{
    Clock, Error, EventSink, HostContext, Limits, ProcessControl, Result, VendorError,
    process::stop_process_with_limits,
};

use crate::approval_events::ApprovalEvents;
use crate::error::vendor_error;
use crate::permission;
use crate::reducer::{Reducer, SessionFact};
use crate::transport::{LaunchedAgent, Overflow};

mod reply;
mod requests;

use reply::PermissionReply;
use requests::RequestAdmission;
pub(crate) use requests::{AgentRequest, PromptFailure, await_sent, send, submit, submit_prompt};

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
    ///
    /// Also this harness's own stand-in for a per-turn vendor handle: see
    /// [`AcpSession::start_turn`](crate::session::AcpSession::start_turn)'s doc comment on
    /// `native_turn_id` for why ACP has no better one to offer.
    pub(crate) generation: u64,
    /// Set by whoever claims this turn's terminal.
    ///
    /// A handler holding a clone can be parked mid-frame while `close` emits the terminal on another
    /// task; resuming afterwards would put the rest of that frame's events *after* the terminal,
    /// which the core's conformance suite refuses. Checked before every emit.
    finished: Arc<AtomicBool>,
    /// Serializes permission settlement with the terminal claim and remembers the first drain.
    permission_settlement: Arc<Mutex<PermissionSettlement>>,
    /// Orders a refused update's terminal against prompt and close publication. A refusal can
    /// publish first while the prompt retains responsibility for native cleanup and slot release.
    terminal_publication: Arc<tokio::sync::Mutex<()>>,
    /// Set once this turn's host was told the agent sent an update that could not be read.
    unreadable_update_noticed: Arc<AtomicBool>,
    /// Wakes the prompt owner when native cancellation starts.
    pub(crate) cancellation: mango_external_agents::CancelToken,
    /// A notification error belongs to this generation, never a later prompt.
    pub(crate) cancel_failure: Arc<Mutex<Option<String>>>,
    pub(crate) approvals: Arc<ApprovalEvents>,
    #[cfg(test)]
    withdrawal_hold: Arc<Mutex<Option<WithdrawalHold>>>,
}

#[derive(PartialEq, Eq)]
enum PermissionSettlement {
    Open,
    Sealed,
    Drained,
}

#[derive(PartialEq, Eq)]
enum TerminalPublisher {
    TurnOwner,
    RefusedEvent,
}

/// One-use synchronous pre-audit pause representing a withdrawal thread being descheduled.
#[cfg(test)]
struct WithdrawalHold {
    entered: Arc<tokio::sync::Notify>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
struct UpdateAdmissionHold {
    entered: tokio::sync::oneshot::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

/// The ACP session a connection is bound to, and the one a failed load abandoned.
#[derive(Debug, Default)]
struct NativeBinding {
    bound: Option<agent_client_protocol_schema::v1::SessionId>,
    abandoned: Option<agent_client_protocol_schema::v1::SessionId>,
    generation: u64,
    pending: Vec<(agent_client_protocol_schema::v1::SessionId, SessionFact)>,
    pending_bytes: usize,
    overflow: Option<(&'static str, usize, usize)>,
}

/// Gives a finished turn's slot back, on a connection that may already have ended.
///
/// A turn that ended with an answer did not wait for any teardown, so the connection can have
/// ended behind it (the agent answered and exited) with the lifecycle watcher not yet run.
/// Released then, the slot could be taken by a `start_turn` that would run on a dead
/// connection and report its end, where the session should refuse it as closed. Admission is
/// closed first in that case, which is what the watcher is about to do.
///
/// ```ignore
/// release_turn(&state, &connection, &handle);
/// ```
pub(crate) fn release_turn(
    state: &SessionState,
    connection: &ConnectionHandle,
    handle: &TurnHandle,
) {
    if connection.ended().is_cancelled() {
        state.close_turn_admission();
    }
    state.release_turn_matching(handle);
}

/// How many tool-call ids are kept from one terminal for [`SessionState::is_late_for_a_closed_call`].
///
/// A turn that ends with more calls than this still running keeps the first ones only; the
/// rest are treated as any other frame.
const CLOSED_CALLS_KEPT: usize = 256;

/// The option id a withdrawal reports, for a decision nobody chose.
///
/// Not one of the request's own options, and deliberately so: naming one would tell an audit trail
/// that somebody picked it. `PermissionEffect::Other` on the decision says the same thing in the
/// field a policy layer reads.
const WITHDRAWN_OPTION_ID: &str = "withdrawn";

/// One agent request waiting for either the harness or the host to answer it.
///
/// A request remains harness-owned while a standing refusal or broker decision is in progress. Only
/// an undecided request becomes host-answerable, immediately before its event is emitted.
struct PendingApproval {
    reply: PermissionReply,
    host_answerable: bool,
    question: mango_external_agents::PermissionRequest,
    announced: bool,
    deadline: ApprovalDeadline,
    expiry_option: Option<String>,
    cancel: CancelNotification,
    turn: TurnHandle,
    /// Dropping a resolved question stops its timer task.
    _timer_done: tokio::sync::oneshot::Sender<()>,
    /// Dropping a settled question ends the broker call deliberating on it.
    ///
    /// Every way a question leaves the pending map consumes it, so this drops exactly when the
    /// question is answered, withdrawn, expired or swept by a cancel or a close.
    _broker_settled: tokio::sync::oneshot::Sender<()>,
}

impl PendingApproval {
    /// Registers the host-facing announcement once the question becomes answerable.
    fn announce(&mut self) {
        if self.announced {
            return;
        }
        self.announced = true;
        self.turn.approvals.push(EventKind::ApprovalRequested {
            request: self.question.clone(),
        });
    }

    /// Takes the question back, telling both sides it will not be answered.
    ///
    /// The agent hears ACP's own `Cancelled` outcome. The host hears
    /// [`EventKind::ApprovalResolved`] with [`DecisionSource::Cancelled`] — but only if it was ever
    /// told about the question, because a resolution for a prompt nobody rendered is a row a host
    /// has nothing to close. Without it a host that *did* render one keeps a dialog that never
    /// closes: the agent is answered, the turn ends, and nothing ever says the ask is over.
    ///
    /// Registered rather than emitted, on the same queue every other resolution uses, so it goes
    /// out ahead of the turn's terminal. A withdrawal that loses that race stays in the queue and
    /// is dropped with it, which is the right outcome: a resolution after the terminal is the one
    /// thing worse than no resolution.
    fn withdraw(self) {
        #[cfg(test)]
        {
            let hold = self
                .turn
                .withdrawal_hold
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(hold) = hold {
                hold.entered.notify_one();
                hold.release
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .expect("bounded withdrawal release");
            }
        }
        if self.announced {
            self.turn.approvals.push(EventKind::ApprovalResolved {
                interaction_id: self.question.id().clone(),
                decision: ApprovalDecision::unresolved(
                    WITHDRAWN_OPTION_ID,
                    DecisionSource::Cancelled,
                ),
            });
        }
        let _ = self.reply.respond(permission::cancelled());
    }

    /// Publish a successful decision before prompt completion can flush the terminal.
    ///
    /// `option_id` always names one of `self.question`'s own options: the host's own choice is
    /// validated against them in [`SessionState::answer`], and every automatic path — the standing
    /// refusal, the broker, the expiry fallback — is built from
    /// [`PermissionRequest::allow`](mango_external_agents::PermissionRequest::allow) or
    /// [`PermissionRequest::deny`](mango_external_agents::PermissionRequest::deny), which can only
    /// return one of them. The fallback to [`ApprovalDecision::unresolved`] is defence in depth for
    /// an id that somehow is not among them, not a path this harness's own callers can reach.
    ///
    /// A connection that no longer takes the answer has ended under the question. Nothing was
    /// decided as far as the agent knows, so the host hears what a withdrawal says, when it was
    /// ever told about the question, and the answer reads as one that came too late:
    /// [`Answered::AlreadyResolved`]. It is not an error of the agent's, and not one a host
    /// could act on.
    fn respond_with_resolution(
        self,
        response: RequestPermissionResponse,
        option_id: String,
        source: DecisionSource,
    ) -> Answered {
        let Self {
            reply,
            question,
            announced,
            turn,
            ..
        } = self;
        let decision = question
            .options
            .iter()
            .find(|option| option.id == option_id)
            .map(|option| ApprovalDecision::from_option(option, source))
            .unwrap_or_else(|| ApprovalDecision::unresolved(option_id, source));
        let event = EventKind::ApprovalResolved {
            interaction_id: question.id().clone(),
            decision,
        };
        let taken = turn
            .approvals
            .record_response(event, || reply.respond(response));
        if taken.is_ok() {
            return Answered::Sent;
        }
        if announced {
            turn.approvals.push(withdrawn_resolution(&question));
        }
        Answered::AlreadyResolved
    }

    /// Gives the agent `response` with no resolution of its own to publish, which is how a
    /// question is let go.
    ///
    /// A connection that does not take the answer has ended under the question, exactly as in
    /// [`respond_with_resolution`](Self::respond_with_resolution): the host's question is
    /// closed as withdrawn when it was shown one, and the answer reads as too late.
    fn respond_unresolved(self, response: RequestPermissionResponse) -> Answered {
        let Self {
            reply,
            question,
            announced,
            turn,
            ..
        } = self;
        if reply.respond(response).is_ok() {
            return Answered::Sent;
        }
        if announced {
            turn.approvals.push(withdrawn_resolution(&question));
        }
        Answered::AlreadyResolved
    }
}

/// What a host is told when `question` will not be answered: resolved, by nobody, as cancelled.
fn withdrawn_resolution(question: &mango_external_agents::PermissionRequest) -> EventKind {
    EventKind::ApprovalResolved {
        interaction_id: question.id().clone(),
        decision: ApprovalDecision::unresolved(WITHDRAWN_OPTION_ID, DecisionSource::Cancelled),
    }
}

impl TurnHandle {
    /// Whether this turn's terminal has already been claimed.
    pub(crate) fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }

    /// Claims the turn owner's terminal path and slot release, once.
    ///
    /// A refused update that already started draining may still publish the terminal first.
    pub(crate) fn finish(&self) -> bool {
        let mut settlement = self
            .permission_settlement
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *settlement == PermissionSettlement::Open {
            *settlement = PermissionSettlement::Sealed;
        }
        !self.finished.swap(true, Ordering::AcqRel)
    }

    /// Claims cleanup, then waits for an earlier refused update to publish its terminal.
    ///
    /// The caller must finish any connection shutdown wait before taking this guard. A
    /// notification whose refusal is waiting for the same guard may hold off that shutdown.
    pub(crate) async fn claim_terminal(&self) -> Option<tokio::sync::MutexGuard<'_, ()>> {
        if !self.finish() {
            return None;
        }
        Some(self.terminal_publication.lock().await)
    }
}

/// Everything the connection's handlers and the session's own methods share.
pub(crate) struct SessionState {
    session_id: SessionId,
    /// The core's own live session state, so a fact the reducer surfaces — the command catalog so
    /// far — can be published the moment it arrives rather than waiting for a turn to end. Cheap to
    /// clone: every clone shares one picture with [`crate::session::AcpSession`]'s own handle.
    core_state: mango_external_agents::SessionState,
    clock: Arc<dyn Clock>,
    broker: Option<Arc<dyn PermissionBroker>>,
    /// The host limits used to form every approval deadline.
    limits: mango_external_agents::Limits,
    turn: Mutex<Option<TurnHandle>>,
    /// Wakes a lifecycle watcher once the running turn has written its terminal and left the slot.
    turn_released: tokio::sync::Notify,
    /// Set by the connection-loss watcher: no further turn may take the slot on a dead connection.
    admission_closed: AtomicBool,
    /// Set by the lifecycle watcher on every wake: the connection is being wound down without a
    /// `close`, so configuration is refused even when it touches no wire request.
    peer_ended: AtomicBool,
    /// The explicit settings the next turn inherits.
    ///
    /// `None` on either permission axis leaves the vendor's own setting in force. Turning that
    /// absence into `ReadOnly` would alter an agent merely because a host omitted an override.
    configuration: Mutex<Configuration>,
    /// Orders complete configuration catalogs from requests and notifications.
    ///
    /// A `config_option_update` can arrive before the response to `session/set_config_option` that
    /// caused it. The later notification is authoritative, so a stale response must not replace it.
    catalog_revision: Mutex<u64>,
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
    /// Touched only by the notification handler, which the peer runs one call at a time.
    reducer: Mutex<Reducer>,
    /// The tool calls the last terminal of this session closed while the agent still had them
    /// running, by the reducer's key. See [`SessionState::is_late_for_a_closed_call`].
    closed_calls: Mutex<std::collections::BTreeSet<String>>,
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
    #[cfg(test)]
    permission_session_hold: Mutex<Option<commit_hold_tests::PermissionTurnHold>>,
    #[cfg(test)]
    update_admission_hold: Mutex<Option<UpdateAdmissionHold>>,
    #[cfg(test)]
    permission_closed_call_hold: Mutex<Option<commit_hold_tests::PermissionTurnHold>>,
    /// Bumped on every frame the agent sends and every change to the pending questions, so the
    /// idle deadline restarts on progress and re-reads whether a question is still open.
    activity: tokio::sync::watch::Sender<u64>,
    /// Which ACP session this connection serves, and which one it gave up on.
    ///
    /// Each session owns its own child, so a frame naming any other id is not this session's to
    /// act on. See [`Self::serves`].
    native_session: Mutex<NativeBinding>,
    /// The connection these handlers serve, once it exists.
    ///
    /// Weak: the peer holds its handler, which holds this state, and a strong handle back would
    /// keep a dropped session's connection alive. Unset in a state no connection was driven for.
    peer: OnceLock<Weak<jsonrpc::Client>>,
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
        core_state: mango_external_agents::SessionState,
    ) -> Self {
        Self {
            session_id,
            core_state,
            clock: Arc::clone(host.clock()),
            broker: host.broker().cloned(),
            limits: *host.limits(),
            turn: Mutex::new(None),
            turn_released: tokio::sync::Notify::new(),
            admission_closed: AtomicBool::new(false),
            peer_ended: AtomicBool::new(false),
            configuration: Mutex::new(configuration),
            catalog_revision: Mutex::new(0),
            turn_start: Mutex::new(()),
            generations: AtomicU64::new(0),
            cancel_reason: Mutex::new(None),
            reducer: Mutex::new(Reducer::new()),
            closed_calls: Mutex::new(std::collections::BTreeSet::new()),
            pending: Mutex::new(HashMap::new()),
            next_approval: AtomicU64::new(0),
            #[cfg(test)]
            permission_session_hold: Mutex::new(None),
            #[cfg(test)]
            update_admission_hold: Mutex::new(None),
            #[cfg(test)]
            permission_closed_call_hold: Mutex::new(None),
            activity: tokio::sync::watch::channel(0).0,
            native_session: Mutex::new(NativeBinding::default()),
            peer: OnceLock::new(),
        }
    }

    /// Names the connection this state's handlers serve. The first one stays.
    fn attach_peer(&self, peer: &Arc<jsonrpc::Client>) {
        let _ = self.peer.set(Arc::downgrade(peer));
    }

    /// Queues `session/cancel` on this state's connection, behind every frame queued before it.
    ///
    /// # Errors
    ///
    /// A JSON-RPC internal error naming what was expected when the connection is gone or would
    /// not take the frame.
    fn notify_cancel(
        &self,
        notification: CancelNotification,
    ) -> agent_client_protocol_schema::v1::Result<()> {
        let queued = self
            .peer
            .get()
            .and_then(Weak::upgrade)
            .ok_or(Error::Closed { subject: "link" })
            .and_then(|peer| {
                peer.submit_notification(AGENT_METHOD_NAMES.session_cancel, notification)
            });
        queued.map(drop).map_err(|error| {
            let mut refused = agent_client_protocol_schema::v1::Error::internal_error();
            refused.message = format!(
                "expected an ACP connection that could carry session/cancel | received: {error}"
            );
            refused
        })
    }

    /// Keeps the notification call this is made from whole until the returned hold is dropped,
    /// for the stretch that takes a turn's reducer and questions and commits its terminal.
    ///
    /// The JSON-RPC peer stops the handler's task without draining its queue when the agent
    /// passes an inbound budget and when the connection is closed, and it cancels the call in
    /// progress at the await it had reached. Cut between taking what the turn owes and committing
    /// the failure, the turn would be ended later by its prompt owner with a different cause and
    /// without the closes of what it left running. Under a hold those ends wait for the stretch,
    /// up to `shutdown_timeout`.
    ///
    /// Ask before the stretch, never inside it. When the cancellation has already been decided
    /// this does not return: the call ends here with nothing taken. It holds nothing, and returns
    /// at once, on a task that is not the peer's notification call or in a state no connection
    /// was driven for.
    ///
    /// ```ignore
    /// let hold = state.hold_terminal_commit().await;
    /// // take, emit, commit
    /// drop(hold);
    /// ```
    async fn hold_terminal_commit(&self) -> Option<CancellationHold> {
        let peer = self.peer.get().and_then(Weak::upgrade)?;
        Some(peer.hold_cancellation().await)
    }

    /// Binds this connection to one ACP session: the one being loaded, or the one opened.
    pub(crate) fn bind_native_session(&self, id: agent_client_protocol_schema::v1::SessionId) {
        let mut binding = self.lock_native_session();
        binding.bound = Some(id.clone());
        for (session_id, fact) in std::mem::take(&mut binding.pending) {
            if session_id == id {
                self.apply_fact(fact);
            }
        }
        binding.pending_bytes = 0;
    }

    pub(crate) fn opening_overflow(&self) -> Option<Error> {
        self.lock_native_session()
            .overflow
            .take()
            .map(|(subject, limit, received)| Error::LimitExceeded {
                subject,
                limit,
                received,
            })
    }

    /// Gives up on the session a failed `session/load` named, before a fallback opens a new one.
    ///
    /// Updates admitted under that binding are invalidated. Its replayed command and configuration
    /// catalogs are cleared; new updates are held until the fallback names its session, which may
    /// reuse the same id.
    ///
    /// ```ignore
    /// state.bind_native_session(loading.clone());
    /// // session/load refused, falling back:
    /// state.abandon_native_session();
    /// ```
    pub(crate) fn abandon_native_session(&self) {
        let mut binding = self.lock_native_session();
        binding.abandoned = binding.bound.take();
        binding.generation = binding.generation.wrapping_add(1);
        binding.pending.clear();
        binding.pending_bytes = 0;
        binding.overflow = None;
        let mut revision = self.lock_catalog_revision();
        *revision = revision.wrapping_add(1);
        self.core_state.update(|snapshot| {
            snapshot.commands = Vec::new();
            snapshot.catalog = ConfigurationCatalog::empty();
            snapshot.configuration.observed = Configuration::unknown();
        });
    }

    fn lock_native_session(&self) -> std::sync::MutexGuard<'_, NativeBinding> {
        self.native_session
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether a frame naming `id` belongs to this session.
    ///
    /// Once bound, only for the bound id. Before that, for any id except one a failed load
    /// abandoned: a fallback's `session/new` has not named its session yet.
    ///
    /// ACP can send session-scoped traffic before the opening response. Updates retain bounded
    /// facts until the returned id is known; permission requests are answered `Cancelled`.
    fn serves(&self, id: &agent_client_protocol_schema::v1::SessionId) -> bool {
        let binding = self.lock_native_session();
        match &binding.bound {
            Some(bound) => bound == id,
            None => binding.abandoned.as_ref() != Some(id),
        }
    }

    /// Restarts idle accounting: the agent said something, or a question opened or closed.
    fn touch(&self) {
        self.activity
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    /// Resolves once the running turn has spent `idle` with no frame from the agent and no
    /// question waiting on an answer.
    ///
    /// A pending question pauses the deadline rather than consuming it: the approval has its own
    /// deadline (`Limits::approval_timeout`), and waiting for a person is not the agent going
    /// quiet. When that question is answered, withdrawn or expires, the deadline restarts from
    /// that moment, so a turn whose approval lapsed stops waiting one idle period later.
    ///
    /// ```ignore
    /// tokio::select! {
    ///     () = state.idle_expired(limits.idle_timeout) => { /* cancel with Timeout */ }
    ///     outcome = &mut prompt => { /* the turn ended on its own */ }
    /// }
    /// ```
    pub(crate) async fn idle_expired(&self, idle: std::time::Duration) {
        let mut changes = self.activity.subscribe();
        loop {
            if self.pending_count() > 0 {
                if changes.changed().await.is_err() {
                    return std::future::pending().await;
                }
                continue;
            }
            tokio::select! {
                () = tokio::time::sleep(idle) => {
                    // `select!` picks among ready branches at random, so the sleep can win on the
                    // same tick a frame arrived. A change it has not consumed is progress: loop, and
                    // the change branch restarts the deadline.
                    if self.pending_count() == 0 && !changes.has_changed().unwrap_or(false) {
                        return;
                    }
                }
                changed = changes.changed() => {
                    if changed.is_err() {
                        return std::future::pending().await;
                    }
                }
            }
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
        if self.admission_closed.load(Ordering::Acquire) {
            return Err(Error::Closed { subject: "session" });
        }
        if turn.is_some() {
            return Err(Error::Busy);
        }
        // A question belongs to a turn, and a turn that is starting has none. Anything still parked
        // here outlived the turn it was asked under and would otherwise be answerable during this one.
        //
        // Drained before the new handle is published, and under the same guard. Published first, a
        // `session/request_permission` arriving in between would read the *new* turn out of the slot,
        // park itself, and then be withdrawn by a drain meant for its predecessor. The permission
        // handler's first act is to take this guard, so holding it here closes the window; the
        // answers themselves go out after it drops.
        let stale = self.take_pending();
        let handle = TurnHandle {
            sink,
            level,
            generation: self.generations.fetch_add(1, Ordering::Relaxed),
            finished: Arc::new(AtomicBool::new(false)),
            permission_settlement: Arc::new(Mutex::new(PermissionSettlement::Open)),
            terminal_publication: Arc::new(tokio::sync::Mutex::new(())),
            unreadable_update_noticed: Arc::new(AtomicBool::new(false)),
            cancellation: mango_external_agents::CancelToken::new(),
            cancel_failure: Arc::new(Mutex::new(None)),
            approvals: Arc::new(ApprovalEvents::default()),
            #[cfg(test)]
            withdrawal_hold: Arc::new(Mutex::new(None)),
        };
        *turn = Some(handle.clone());
        *self.lock_reducer() = Reducer::new();
        *self.lock_cancel_reason() = None;
        drop(turn);
        for pending in stale {
            pending.withdraw();
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

    /// The current complete catalog and its notification ordering token.
    pub(crate) fn catalog_snapshot(&self) -> (ConfigurationCatalog, u64) {
        let revision = self.lock_catalog_revision();
        (self.core_state.snapshot().catalog.clone(), *revision)
    }

    /// Submits a synchronous request while holding its catalog-ordering baseline.
    pub(crate) fn with_catalog_revision<ResultValue>(
        &self,
        submit: impl FnOnce(u64) -> ResultValue,
    ) -> ResultValue {
        let revision = self.lock_catalog_revision();
        submit(*revision)
    }

    /// Publishes a request response unless a later catalog notification already won the order.
    pub(crate) fn publish_response_configuration(
        &self,
        response_revision: u64,
        response_catalog: ConfigurationCatalog,
        requested: Configuration,
        accepted: Configuration,
    ) -> ConfigurationState {
        self.with_response_catalog(response_revision, response_catalog, |catalog| {
            let state = ConfigurationState::new(
                requested,
                accepted,
                crate::session::configuration_from_catalog(&catalog),
            );
            self.core_state.update(|snapshot| {
                snapshot.catalog = catalog;
                snapshot.configuration = state.clone();
            });
            state
        })
    }

    /// Publishes an opening catalog only when no newer session notification has won.
    pub(crate) fn publish_lifecycle_catalog(
        &self,
        response_revision: u64,
        response_catalog: ConfigurationCatalog,
    ) {
        self.with_response_catalog(response_revision, response_catalog, |catalog| {
            let observed = crate::session::configuration_from_catalog(&catalog);
            self.core_state.update(|snapshot| {
                snapshot.catalog = catalog;
                snapshot.configuration.observed = observed;
            });
        });
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
        let turn = self.lock_turn().clone();
        let Some(turn) = turn else {
            return false;
        };
        let mut cancelling = self.lock_cancel_reason();
        cancelling.get_or_insert(reason);
        let pending = self.take_pending();
        for pending in pending {
            pending.withdraw();
        }
        // An answered permission can itself complete the prompt. Keep terminal draining behind
        // this guard until every removed request has registered its audit, then wake the owner.
        drop(cancelling);
        turn.cancellation.cancel();
        true
    }

    /// Whether the current prompt has been cancelled before its wire request was sent.
    pub(crate) fn is_cancelling(&self) -> bool {
        self.lock_cancel_reason().is_some()
    }

    /// Records a failed cancellation write for the prompt owner to publish on its owned stream.
    pub(crate) fn record_cancel_failure(&self, handle: &TurnHandle, message: impl Into<String>) {
        *handle
            .cancel_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(message.into());
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

    /// Takes the matched turn's cancellation reason and any reducer it still owes.
    ///
    /// What a `session/prompt` task calls. Its own turn may already have been ended by a `close`, and
    /// a *later* turn may have started in the meantime — so an unconditional take would let a task
    /// that answered late end, and emit the terminal of, a conversation that is not its own.
    ///
    /// Hands back the reducer rather than its closing events because the caller decides their
    /// status. If a refused update already took the reducer, only the cancellation reason remains.
    pub(crate) fn prepare_terminal_matching(
        &self,
        handle: &TurnHandle,
    ) -> Option<(TurnHandle, Option<CancelReason>, Reducer)> {
        self.prepare_terminal(handle, TerminalPublisher::TurnOwner)
    }

    fn prepare_terminal(
        &self,
        handle: &TurnHandle,
        publisher: TerminalPublisher,
    ) -> Option<(TurnHandle, Option<CancelReason>, Reducer)> {
        let active = self.lock_turn();
        let turn = active.as_ref()?;
        if turn.generation != handle.generation {
            return None;
        }
        // These are all owned by the current turn. Capture them before releasing the slot: a
        // newly started prompt clears the reducer and may park approvals of its own.
        let mut cancelling = self.lock_cancel_reason();
        let mut settlement = turn
            .permission_settlement
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if publisher == TerminalPublisher::RefusedEvent && turn.is_finished() {
            return None;
        }
        let drained = *settlement == PermissionSettlement::Drained;
        *settlement = PermissionSettlement::Drained;
        drop(settlement);
        let reason = if publisher == TerminalPublisher::TurnOwner {
            cancelling.take()
        } else {
            // The refusal publishes failure. The first cancellation reason still belongs to
            // the owner that will perform native cleanup.
            None
        };
        if drained {
            // A refusal already recorded the calls it closed. Preserve those ids for the next
            // turn, which uses them to distinguish a late frame from its own work.
            return Some((turn.clone(), reason, Reducer::new()));
        }
        for pending in self.take_pending() {
            pending.withdraw();
        }
        // A second terminal drain must see the audit before it finds the pending map empty.
        drop(cancelling);
        let closing = std::mem::take(&mut *self.lock_reducer());
        self.remember_closed_calls(&closing);
        Some((turn.clone(), reason, closing))
    }

    /// Keeps the ids of the calls `closing` still holds open, in place of the ones kept at the
    /// terminal before.
    ///
    /// Replaced at every terminal, so the set is only ever the last turn's leftovers, and at
    /// most [`CLOSED_CALLS_KEPT`] of them.
    fn remember_closed_calls(&self, closing: &Reducer) {
        *self.lock_closed_calls() = closing
            .open_call_ids()
            .take(CLOSED_CALLS_KEPT)
            .map(String::from)
            .collect();
    }

    /// Whether `update` is a late frame for a call the previous turn's terminal closed.
    ///
    /// ACP updates carry no turn id, so a frame cannot in general be given to the turn it was
    /// meant for. One case is certain: the agent left a call running when its turn ended, this
    /// side closed that call with the turn's terminal, and a `tool_call_update` for the same
    /// id arrives after the next turn began. Reduced there, it would open the call again on a
    /// turn that never ran it. Such a frame is dropped, with nothing said: it is the tail of a
    /// turn the host already saw end, not a fault of the turn now running, and a notice on
    /// that turn would point at the wrong one.
    ///
    /// A `tool_call` that starts a call under a remembered id is the agent using the id anew,
    /// so the id is forgotten and the frame is the running turn's. Anything else, a text chunk
    /// or a call the previous turn never saw, cannot be told apart and is reduced as it comes.
    ///
    /// ```ignore
    /// if state.is_late_for_a_closed_call(&notification.update) { return; }
    /// ```
    fn is_late_for_a_closed_call(&self, update: &SessionUpdate) -> bool {
        match update {
            SessionUpdate::ToolCallUpdate(late) => self.closed_call(&late.tool_call_id),
            SessionUpdate::ToolCall(started) => {
                self.lock_closed_calls()
                    .remove(&crate::reducer::call_key(&started.tool_call_id));
                false
            }
            _ => false,
        }
    }

    /// Whether the previous turn's terminal closed the call the agent names `id`.
    ///
    /// For example, a `session/request_permission` about such a call is the previous turn's
    /// question arriving late, and is withdrawn instead of shown on the turn now running.
    fn closed_call(&self, id: &agent_client_protocol_schema::v1::ToolCallId) -> bool {
        self.lock_closed_calls()
            .contains(&crate::reducer::call_key(id))
    }

    fn lock_closed_calls(&self) -> std::sync::MutexGuard<'_, std::collections::BTreeSet<String>> {
        self.closed_calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Releases an already-terminal generation after its stream outcome is committed.
    pub(crate) fn release_turn_matching(&self, handle: &TurnHandle) {
        let mut active = self.lock_turn();
        if active
            .as_ref()
            .is_some_and(|current| current.generation == handle.generation)
        {
            active.take();
            drop(active);
            self.turn_released.notify_waiters();
        }
    }

    /// Refuses every later turn, for a watcher that saw the connection die.
    ///
    /// Taken under the slot's own guard, so a start either installed its turn first — and the
    /// watcher then waits for that turn's terminal — or observes the refusal.
    ///
    /// ```ignore
    /// state.close_turn_admission();
    /// assert!(state.begin_turn(sink, None).is_err());
    /// ```
    pub(crate) fn close_turn_admission(&self) {
        let _turn = self.lock_turn();
        self.admission_closed.store(true, Ordering::Release);
    }

    /// Records that the lifecycle watcher is ending this session without a `close`.
    ///
    /// A separate mark from the close state on purpose: `close` keeps sole ownership of the shared
    /// close result, and this only lets configuration refuse work on a session that has ended.
    ///
    /// ```ignore
    /// state.mark_peer_ended();
    /// assert!(state.has_peer_ended());
    /// ```
    pub(crate) fn mark_peer_ended(&self) {
        self.peer_ended.store(true, Ordering::Release);
    }

    /// Whether the lifecycle watcher has begun ending this session.
    pub(crate) fn has_peer_ended(&self) -> bool {
        self.peer_ended.load(Ordering::Acquire)
    }

    /// Waits until no turn holds the prompt slot.
    ///
    /// A turn leaves the slot only after its terminal is committed, so a watcher that awaits this
    /// can publish `Closed` knowing nothing more will reach the turn's stream.
    ///
    /// ```ignore
    /// state.wait_for_no_turn().await;
    /// session_state.set_status(SessionStatus::Closed);
    /// ```
    pub(crate) async fn wait_for_no_turn(&self) {
        loop {
            let released = self.turn_released.notified();
            if self.lock_turn().is_none() {
                return;
            }
            released.await;
        }
    }

    /// The events and session facts one frame produces for `turn`, computed under the reducer guard.
    ///
    /// `turn` is the owner the caller read before getting here, and it may have ended since: its
    /// terminal is claimed before its slot is released, and the slot is released before a later
    /// turn resets the reducer. A frame for a turn that already claimed its terminal has nowhere to
    /// go (the caller drops its events), and reducing it would leave a call open in a reducer the
    /// next turn then owns, to be closed there as a call its host never saw start. So such a frame
    /// contributes only its session facts. The claim is read under the reducer's own guard, which
    /// is what the next turn's reset waits for, so a turn that has replaced `turn` is always seen
    /// as ended here. Late-call routing also runs under this guard, so an update cannot forget
    /// a remembered call ID after the turn that received it ended.
    fn reduce(
        &self,
        turn: &TurnHandle,
        notification: SessionNotification,
    ) -> (Vec<EventKind>, Vec<SessionFact>) {
        let mut reducer = self.lock_reducer();
        if turn.is_finished() || turn.sink.is_terminal() {
            return (Vec::new(), Reducer::session_facts(notification.update));
        }
        if self.is_late_for_a_closed_call(&notification.update) {
            return (Vec::new(), Vec::new());
        }
        if is_progress(&notification.update) {
            self.touch();
        }
        // Tokio's clock rather than the host's: this instant only spaces a running call's updates,
        // and a runtime with paused time has to see that spacing move with it.
        reducer.update_at(notification.update, tokio::time::Instant::now().into_std())
    }

    /// How many tool calls the turn reducer holds open, for the tests that prove a turnless frame
    /// did not reach it.
    #[cfg(test)]
    fn open_call_count(&self) -> usize {
        self.lock_reducer().open_calls_len()
    }

    /// Publishes a session-scoped fact the reducer surfaced.
    ///
    /// Applied to the core's own live state rather than folded into the turn stream — see
    /// [`SessionFact`]'s own doc comment for why — so a host reading
    /// [`SessionState::subscribe`](mango_external_agents::SessionState::subscribe) sees a
    /// re-announced catalog without having to watch every turn for it.
    fn apply_fact(&self, fact: SessionFact) {
        match fact {
            SessionFact::Commands(commands) => self
                .core_state
                .set_commands(mango_external_agents::event::normalized_catalog(commands)),
            SessionFact::ConfigurationOptions(options) => {
                let catalog = crate::session::catalog_from_options(&options);
                let observed = crate::session::configuration_from_catalog(&catalog);
                let mut revision = self.lock_catalog_revision();
                *revision = revision.wrapping_add(1);
                self.core_state.update(|snapshot| {
                    snapshot.catalog = catalog;
                    snapshot.configuration.observed = observed;
                });
            }
        }
    }

    /// Withdraws every question still waiting.
    ///
    /// Called wherever a turn ends, not only on `close`, because a question cannot outlive the turn it
    /// belongs to: its answer would be emitted into a sink that is already finished, and the agent
    /// would be told "allow" about a turn it stopped running. Dropping the replies instead would
    /// leave the agent to whatever a dropped reply falls back to, so each is answered
    /// `Cancelled`, which is ACP's own outcome for a question that was withdrawn rather than refused.
    ///
    /// Holds the cancellation guard until every removed request has registered its audit, so a
    /// terminal drain cannot pass between removal and registration. No asynchronous I/O is held.
    pub(crate) fn withdraw_pending(&self) {
        let _cancelling = self.lock_cancel_reason();
        for pending in self.take_pending() {
            pending.withdraw();
        }
    }

    /// Empties the question map, handing back every request that still owes an answer.
    ///
    /// The four places a turn can end all owe the same debt, and each of them takes the requests
    /// out in one statement so nothing is held while the answers go out.
    fn take_pending(&self) -> Vec<PendingApproval> {
        let taken: Vec<PendingApproval> =
            self.lock_pending().drain().map(|(_, held)| held).collect();
        if !taken.is_empty() {
            self.touch();
        }
        taken
    }

    /// Parks an agent question unless cancellation already won its race.
    ///
    /// Cancellation holds the same guard before draining pending replies. A request that arrives
    /// after `session/cancel` must receive ACP's `Cancelled` outcome rather than wait forever for a
    /// host event that cancellation made obsolete.
    fn park_pending(
        &self,
        id: String,
        pending: PendingApproval,
    ) -> agent_client_protocol_schema::v1::Result<bool> {
        let cancelling = self.lock_cancel_reason();
        if cancelling.is_some() {
            drop(cancelling);
            pending.reply.respond(permission::cancelled())?;
            return Ok(false);
        }
        let mut parked = self.lock_pending();
        let settlement = pending.turn.permission_settlement.clone();
        let settlement = settlement.lock().unwrap_or_else(PoisonError::into_inner);
        if *settlement != PermissionSettlement::Open
            || pending.turn.sink.is_terminal()
            || parked.len() >= self.limits.max_pending_requests
        {
            drop(parked);
            pending.reply.respond(permission::cancelled())?;
            return Ok(false);
        }
        parked.insert(id, pending);
        drop(parked);
        self.touch();
        Ok(true)
    }

    /// Publishes a broker-settled request, making an undecided one answerable by the host.
    pub(crate) fn announce_pending(&self, id: &str, host_answerable: bool) -> bool {
        let cancelling = self.lock_cancel_reason();
        if cancelling.is_some() {
            return false;
        }
        let mut parked = self.lock_pending();
        let Some(pending) = parked.get_mut(id) else {
            return false;
        };
        let settlement = pending.turn.permission_settlement.clone();
        let settlement = settlement.lock().unwrap_or_else(PoisonError::into_inner);
        if *settlement != PermissionSettlement::Open || pending.turn.sink.is_terminal() {
            let pending = parked
                .remove(id)
                .expect("the registered permission remains locked");
            self.touch();
            pending.withdraw();
            return false;
        }
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
    ) -> agent_client_protocol_schema::v1::Result<Answered> {
        let mut cancelling = self.lock_cancel_reason();
        let pending = self.lock_pending().remove(id);
        let Some(mut pending) = pending else {
            return Ok(Answered::AlreadyResolved);
        };
        self.touch();
        let settlement = pending.turn.permission_settlement.clone();
        let settlement = settlement.lock().unwrap_or_else(PoisonError::into_inner);
        if cancelling.is_some()
            || *settlement != PermissionSettlement::Open
            || pending.turn.sink.is_terminal()
        {
            pending.withdraw();
            return Ok(Answered::AlreadyResolved);
        }
        if !pending.deadline.is_elapsed() {
            if let agent_client_protocol_schema::v1::RequestPermissionOutcome::Selected(outcome) =
                &requested.outcome
            {
                let option_id = outcome.option_id.to_string();
                return Ok(pending.respond_with_resolution(requested, option_id, source));
            }
            return Ok(pending.respond_unresolved(requested));
        }

        pending.announce();
        let Some(option_id) = pending.expiry_option.as_ref() else {
            cancelling.get_or_insert(CancelReason::Timeout);
            // Released before the turn's cancellation is triggered: the prompt owner it wakes, and
            // `begin_cancellation`, take this same lock. The reason is already recorded, so a
            // late request or answer sees a cancelling turn whether or not the guard is held.
            let turn = pending.turn.clone();
            let notification = pending.cancel.clone();
            pending.withdraw();
            drop(settlement);
            drop(cancelling);
            self.cancel_expired(turn, notification);
            return Ok(Answered::AlreadyResolved);
        };
        let outcome = permission::selected(option_id);
        let option_id = option_id.clone();
        pending.respond_with_resolution(outcome, option_id, DecisionSource::Expired);
        Ok(Answered::AlreadyResolved)
    }

    /// Stops the turn whose question ran out of time with no way to refuse it.
    ///
    /// The caller has already withdrawn the question under its settlement guard. Its siblings are
    /// withdrawn too, and the agent is sent `session/cancel`,
    /// under the gate a turn starts under. That gate is what ties the cancel to this turn: the
    /// caller let go of the cancel-reason lock before coming here, so the turn can have ended
    /// and another begun in between, and a `session/cancel` sent then would stop a turn nobody
    /// timed out. A question whose turn is no longer the running one needs no further cancel.
    ///
    /// A `session/cancel` the connection does not take is recorded on the turn, as a host's own
    /// cancel records it, so the turn's owner reports it and tears the connection down instead
    /// of waiting out the kill grace. It is not an error of the caller's: the question was
    /// withdrawn either way.
    ///
    /// ```ignore
    /// state.cancel_expired(turn, notification);
    /// ```
    fn cancel_expired(&self, turn: TurnHandle, notification: CancelNotification) {
        let _starting = self.lock_turn_start();
        // Every resolution is queued before anything reaches the agent that could end its
        // prompt: an agent that honours `session/cancel` on the spot can finish on another
        // worker, and a resolution registered after the terminal is dropped. Nothing is chosen
        // for the agent; it hears ACP's own `Cancelled` outcome.
        if !self.can_submit_prompt(&turn) {
            return;
        }
        self.withdraw_pending();
        // The agent may ignore `session/cancel`; this starts the kill-grace fallback that
        // reaps it, the same as a host `cancel` does.
        turn.cancellation.cancel();
        if self.notify_cancel(notification).is_err() {
            self.record_cancel_failure(&turn, "ACP session/cancel could not be queued");
        }
    }

    /// Withdraws one pending request when its turn cannot continue.
    ///
    /// Sibling of [`SessionState::withdraw_pending`], for one request instead of the whole map.
    pub(crate) fn withdraw_pending_by_id(
        &self,
        id: &str,
    ) -> agent_client_protocol_schema::v1::Result<()> {
        let _cancelling = self.lock_cancel_reason();
        let pending = self.lock_pending().remove(id);
        match pending {
            Some(pending) => {
                self.touch();
                pending.withdraw();
                Ok(())
            }
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
    /// The pending request's own turn also serializes the final native response with its terminal
    /// claim, covering broker and deadline decisions as well as host answers.
    ///
    /// # Errors
    ///
    /// Whatever the transport reported while sending the answer.
    pub(crate) fn answer(&self, response: &PermissionResponse) -> Result<Answered> {
        {
            let pending = self.lock_pending();
            let Some(pending) = pending
                .get(response.interaction_id.as_str())
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
            .respond_pending(response.interaction_id.as_str(), outcome, response.source)
            .map_err(|error| {
                Error::Vendor(vendor_error(
                    CLIENT_METHOD_NAMES.session_request_permission,
                    &error,
                ))
            })?;
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

    fn lock_catalog_revision(&self) -> std::sync::MutexGuard<'_, u64> {
        self.catalog_revision
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn with_response_catalog<ResultValue>(
        &self,
        response_revision: u64,
        response_catalog: ConfigurationCatalog,
        update: impl FnOnce(ConfigurationCatalog) -> ResultValue,
    ) -> ResultValue {
        let mut revision = self.lock_catalog_revision();
        let catalog = if *revision == response_revision {
            *revision = revision.wrapping_add(1);
            response_catalog
        } else {
            self.core_state.snapshot().catalog.clone()
        };
        update(catalog)
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

/// Whether `update` is the agent making progress on the turn, and so restarts idle accounting.
///
/// Every update is, except a `notice`. A notice is a live aside for the person at the host: it
/// reaches the turn stream as [`EventKind::Notice`], and it says nothing about whether the agent
/// is still working. An agent stuck in a retry loop that announces each attempt would otherwise
/// hold a turn open past the host's idle deadline for as long as it kept announcing. For example,
/// a `notice` frame every second does not restart idle accounting, while a compaction update does:
/// compacting is work.
fn is_progress(update: &SessionUpdate) -> bool {
    !matches!(update, SessionUpdate::Notice(_))
}

/// One `session/update` notification, reduced and emitted.
///
/// Never fails the handler. A closed or overflowed sink wakes the prompt owner, which cancels native
/// work before it releases this generation; freeing the slot here would let a second prompt onto a
/// wire that has no way to tell two turns apart. The nonblocking sink makes later frames fail fast.
async fn on_session_update(state: &Arc<SessionState>, notification: SessionNotification) {
    // Read ownership under the binding lock, before a bind can hand the session to its first turn.
    // A fallback may reopen the abandoned id, so its new-generation updates wait for the bind.
    let (generation, turn) = {
        let binding = state.lock_native_session();
        if binding
            .bound
            .as_ref()
            .is_some_and(|id| id != &notification.session_id)
        {
            return;
        }
        (binding.generation, state.turn())
    };
    #[cfg(test)]
    if let Some(hold) = state
        .update_admission_hold
        .lock()
        .expect("expected update gate")
        .take()
    {
        let _ = hold.entered.send(());
        hold.release
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("expected update gate release within 5s");
    }
    let events = {
        let mut binding = state.lock_native_session();
        if binding.generation != generation
            || binding
                .bound
                .as_ref()
                .is_some_and(|id| id != &notification.session_id)
        {
            return;
        }
        if binding.bound.is_none() && turn.is_none() {
            // The frame is bounded before parsing; use its encoded size and the host's queued
            // message and byte budgets to bound retained opening facts as well.
            let bytes = serde_json::to_vec(&notification).map_or(usize::MAX, |frame| frame.len());
            let facts = Reducer::session_facts(notification.update);
            if !facts.is_empty() {
                let count_limit = state
                    .limits
                    .turn_channel_capacity
                    .max(state.limits.max_pending_requests);
                let received_count = binding.pending.len().saturating_add(facts.len());
                let received_bytes = binding.pending_bytes.saturating_add(bytes);
                if received_count > count_limit {
                    binding.overflow.get_or_insert((
                        "opening session facts",
                        count_limit,
                        received_count,
                    ));
                } else if received_bytes > state.limits.turn_buffer_bytes {
                    binding.overflow.get_or_insert((
                        "bytes of opening session facts",
                        state.limits.turn_buffer_bytes,
                        received_bytes,
                    ));
                } else {
                    binding.pending_bytes = received_bytes;
                    binding.pending.extend(
                        facts
                            .into_iter()
                            .map(|fact| (notification.session_id.clone(), fact)),
                    );
                }
            }
            return;
        }
        let Some(turn) = turn.as_ref() else {
            if is_progress(&notification.update) {
                state.touch();
            }
            for fact in Reducer::session_facts(notification.update) {
                state.apply_fact(fact);
            }
            return;
        };
        let (events, facts) = state.reduce(turn, notification);
        for fact in facts {
            state.apply_fact(fact);
        }
        events
    };
    let Some(turn) = turn else {
        return;
    };
    for kind in events {
        // Re-checked each time round: close can commit the terminal after reduction, and no later
        // event may follow it.
        if turn.is_finished() {
            return;
        }
        if let Err(error) = turn.sink.emit(kind).await {
            report_refused_event(state, &turn, &error).await;
            return;
        }
    }
}

/// What a `session/update` this client cannot read still does: the turn's host is told once.
///
/// There is no request to answer, and one frame in a shape this build does not know is not the
/// turn's end, so the frame itself is dropped, as it always was.
/// Dropped silently, it left a host with an empty turn and nothing to say why, so the first one
/// in a turn is announced as a warning notice, in this crate's own words and with nothing of
/// the frame in it. Later ones in the same turn say nothing more.
///
/// It is not progress: an update kind no schema knows does not restart the idle deadline, any
/// more than a notice does. A turn made only of such frames still ends at that deadline, and
/// the notice is what tells the host why it was empty.
///
/// It does not go through the refused-event path: that one fails the turn, which is right for
/// an event the core cannot publish and too much for a frame that was only skipped.
///
/// ```ignore
/// on_unreadable_update(&state, generation, turn).await;
/// ```
async fn on_unreadable_update(
    state: &Arc<SessionState>,
    generation: u64,
    turn: Option<TurnHandle>,
) {
    #[cfg(test)]
    if let Some(hold) = state
        .update_admission_hold
        .lock()
        .expect("expected update gate")
        .take()
    {
        let _ = hold.entered.send(());
        hold.release
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("expected update gate release within 5s");
    }
    if state.lock_native_session().generation != generation {
        return;
    }
    let Some(turn) = turn else {
        return;
    };
    if turn.is_finished() || turn.unreadable_update_noticed.swap(true, Ordering::AcqRel) {
        return;
    }
    let Some(notice) = EventKind::notice(
        mango_external_agents::NoticeSeverity::Warning,
        "The agent sent an update this client could not read",
        Some(
            "Updates in a shape this client does not know are skipped. The turn continues without them.",
        ),
    ) else {
        return;
    };
    if let Err(error) = turn.sink.emit(notice).await {
        report_refused_event(state, &turn, &error).await;
    }
}

/// Fails the turn when the core refused to publish one of its events, so the host is told.
///
/// Only a value the core cannot make safe (an agent's call id that is blank or longer than the
/// bound) is reported: it means a part of the turn can never reach the host, and dropping it in
/// silence leaves a host rendering a turn that is missing something. Every other refusal is an
/// ordinary end of the turn's stream (a closed or already-terminal sink) or was already turned
/// into a failure by the sink itself (an overflow), so it stays silent. An already-terminal turn
/// keeps its outcome, because a terminal commits once.
///
/// The failure is committed on the sink, so the prompt owner sees the terminal and stops native
/// work before it releases the generation. That owner adds nothing to a terminal it did not write,
/// so what the turn still owes is settled here first, in the order the prompt owner would have
/// used: the questions still waiting are withdrawn and their resolutions flushed, the calls,
/// reasoning and plan the agent left running are closed as failed where the payload budget
/// permits, and then the reserved terminal commits.
async fn report_refused_event(state: &SessionState, turn: &TurnHandle, error: &Error) {
    if !matches!(error, Error::InvalidVendorValue { .. }) || turn.sink.is_terminal() {
        return;
    }
    // From taking what the turn owes to committing its terminal in one piece: see
    // `hold_terminal_commit`. Nothing below waits on the agent or on the host's reader.
    let _hold = state.hold_terminal_commit().await;
    let _publication = turn.terminal_publication.lock().await;
    if turn.sink.is_terminal() {
        return;
    }
    let Some((_, _, closing)) = state.prepare_terminal(turn, TerminalPublisher::RefusedEvent)
    else {
        return;
    };
    let _ = turn.approvals.flush(&turn.sink).await;
    crate::session::emit_terminal_activity_events(&turn.sink, closing, ActivityStatus::Failed)
        .await;
    let _ = turn
        .sink
        .fail(VendorError::new(
            mango_external_agents::ErrorCode::from_static("acp-refused-event"),
            format!("the ACP agent sent an event the core refused to publish: {error}"),
        ))
        .await;
}

/// One `session/request_permission` request, brokered.
///
/// Never waits for a person: it parks the question and returns, and the answer arrives on another
/// task. It first parks the question under the cancellation guard, then
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
    reply: PermissionReply,
) -> agent_client_protocol_schema::v1::Result<()> {
    // Capture the turn before checking the native session and closed calls. A handoff
    // with no turn cannot attach to the first prompt after binding. If a captured turn
    // finishes during either check, its settlement seal still refuses the request.
    let turn = state.turn();
    if !state.serves(&request.session_id) {
        // Another session's question. Refused rather than shown: this host's answer would grant
        // work in a conversation it never opened.
        return reply.respond(permission::cancelled());
    }
    #[cfg(test)]
    commit_hold_tests::PermissionTurnHold::wait_if_armed(&state.permission_session_hold);
    if state.closed_call(&request.tool_call.tool_call_id) {
        // About a call the previous turn's terminal closed: that turn's question, arriving
        // after it ended. Withdrawn, as it would have been had it arrived a moment earlier.
        return reply.respond(permission::cancelled());
    }
    #[cfg(test)]
    commit_hold_tests::PermissionTurnHold::wait_if_armed(&state.permission_closed_call_hold);
    let id = state.mint_approval_id();
    let Some(turn) = turn else {
        // No turn: nobody is reading, and an unanswered request would hold the agent forever.
        return reply.respond(permission::cancelled());
    };
    if turn.is_finished() || turn.sink.is_terminal() {
        // The turn's terminal has been claimed or committed by the stream. A close handshake or
        // prompt completion may still be draining requests. Withdraw rather than park or decide:
        // nothing may grant a permission while the session is being torn down, and a reply parked now
        // would be left waiting when the connection goes.
        return reply.respond(permission::cancelled());
    }

    // One read, reused below: a second `state.now()` call to translate `expires_at` back into a
    // deadline would let a host clock that moved backward between the two reads extend the
    // monotonic approval window past what the wire advertised.
    let now = state.now();
    let Ok(expires_at) = state.limits.approval_expires_at(now) else {
        return reply.respond(permission::cancelled());
    };
    let question = permission::request_from(
        &request,
        id.clone(),
        state.session_id.clone(),
        turn.sink.operation(),
        expires_at,
    );
    let question = match question.normalized() {
        Ok(question) => question,
        // A question the core refuses to normalize is withdrawn on its own rather than ending
        // the turn it belongs to.
        Err(_) => return reply.respond(permission::cancelled()),
    };

    let Some(deadline) = ApprovalDeadline::new(question.expires_at(), now) else {
        return reply.respond(permission::cancelled());
    };
    let (timer_done, done) = tokio::sync::oneshot::channel();
    let (broker_settled, settled) = tokio::sync::oneshot::channel();
    let pending = PendingApproval {
        reply,
        host_answerable: false,
        question: question.clone(),
        announced: false,
        deadline,
        // The same choice the standing refusal makes, through the core's own preference order:
        // the one-time refusal when the agent offered one, the standing refusal otherwise. Matching
        // only `RejectOnce` here would leave an agent that offers `reject_always` alone with no
        // expiry answer, and the branch below cancels the whole turn when there is none.
        expiry_option: question.deny().ok().map(|refusal| refusal.option_id),
        cancel: CancelNotification::new(request.session_id),
        turn: turn.clone(),
        _timer_done: timer_done,
        _broker_settled: broker_settled,
    };
    if !state.park_pending(id.clone(), pending)? {
        return Ok(());
    }

    expire_pending(state, &turn, id.clone(), deadline, done);

    resolve_pending(Arc::clone(state), turn, id, question, deadline, settled);
    Ok(())
}

/// Resolves one parked permission away from the task that received it.
///
/// The broker call lives exactly as long as its question is parked. `settled` resolves when the
/// pending map lets go of the question, however it does — answered, withdrawn, expired, cancelled or
/// swept by a close — and the call is dropped at that point rather than left to run out its
/// approval deadline. That is what ties these tasks to `max_pending_requests`: an entry in the map
/// is the only thing that keeps one alive. `ApprovalDeadline::run` still bounds a broker that
/// never decides, for a question nothing else settles.
///
/// A host's [`PermissionBroker`](mango_external_agents::PermissionBroker) therefore sees its
/// `decide` future dropped, the same as it already does when the deadline passes.
fn resolve_pending(
    state: Arc<SessionState>,
    turn: TurnHandle,
    id: String,
    question: mango_external_agents::PermissionRequest,
    deadline: ApprovalDeadline,
    settled: tokio::sync::oneshot::Receiver<()>,
) {
    tokio::spawn(async move {
        let decided = match turn.level {
            Some(PermissionLevel::ReadOnly) => standing_refusal(&question),
            Some(PermissionLevel::Default | PermissionLevel::FullAccess) | None => {
                let deliberation = deadline.run(broker_response(state.broker.as_ref(), &question));
                tokio::select! {
                    biased;
                    // Only ever a dropped sender: nothing is sent on this channel.
                    _ = settled => return,
                    decided = deliberation => decided.flatten(),
                }
            }
        };
        if !state.announce_pending(&id, decided.is_none()) {
            return;
        }
        if turn.approvals.flush(&turn.sink).await.is_err() {
            let _ = state.withdraw_pending_by_id(&id);
            return;
        }
        if let Some(decision) = decided {
            let _ = state.respond_pending(
                &id,
                permission::selected(&decision.option_id),
                decision.source,
            );
            let _ = turn.approvals.flush(&turn.sink).await;
        }
    });
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

/// What the JSON-RPC peer hands the agent's traffic to.
///
/// - `session/update` is reduced and emitted, one call at a time, in arrival order.
/// - `session/request_permission` is brokered: its reply is parked and the call returns.
/// - Any other request is refused with JSON-RPC `-32601`, which is what the capabilities this
///   client advertises promise: no filesystem, no terminal, nothing else an agent may ask for.
/// - Any other notification is dropped without an answer.
struct AcpHandler {
    state: Arc<SessionState>,
    /// Cancelled once the connection ended under this side, for whoever watches it.
    ended: mango_external_agents::CancelToken,
}

#[async_trait::async_trait]
impl PeerHandler for AcpHandler {
    async fn on_notification(&self, method: String, params: serde_json::Value) {
        if method != CLIENT_METHOD_NAMES.session_update {
            return;
        }
        // Capture the owner before decoding, including for envelopes whose update is unknown.
        let (generation, turn, foreign) = {
            let binding = self.state.lock_native_session();
            let id = params.get("sessionId").and_then(serde_json::Value::as_str);
            let foreign = id
                .zip(binding.bound.as_ref())
                .is_some_and(|(id, bound)| id != bound.0.as_ref());
            (binding.generation, self.state.turn(), foreign)
        };
        let Ok(notification) = serde_json::from_value::<SessionNotification>(params) else {
            if !foreign {
                on_unreadable_update(&self.state, generation, turn).await;
            }
            return;
        };
        on_session_update(&self.state, notification).await;
    }

    /// Runs on the notification worker, in wire order: a permission is parked, or settled,
    /// before anything the agent sent after it is handled and before a later response reaches
    /// its caller. Nothing here waits for a person.
    async fn on_request_handoff(
        &self,
        method: String,
        params: serde_json::Value,
        responder: jsonrpc::Responder,
    ) {
        if method != CLIENT_METHOD_NAMES.session_request_permission {
            reply::refuse(
                responder,
                agent_client_protocol_schema::v1::Error::method_not_found(),
            );
            return;
        }
        let Ok(request) = serde_json::from_value::<RequestPermissionRequest>(params) else {
            // No `data`: the decoder's description quotes the value it could not read, and
            // nothing an agent sent is echoed back onto the wire.
            reply::refuse(
                responder,
                agent_client_protocol_schema::v1::Error::invalid_params(),
            );
            return;
        };
        // An answer the connection did not take is the connection ending under the question,
        // which its owner hears on its own.
        let _ = on_request_permission(&self.state, request, PermissionReply::new(responder)).await;
    }

    fn on_request_past_cap(
        &self,
        method: &str,
        _params: &serde_json::Value,
        _id: &jsonrpc::RequestId,
    ) -> Option<ServerRequestOutcome> {
        Some(reply::past_cap(
            method == CLIENT_METHOD_NAMES.session_request_permission,
        ))
    }

    async fn on_terminated(&self, _termination: jsonrpc::PeerTermination) {
        // Only a signal: whoever owns the connection reads why from `Client::ended` and tears it
        // down from its own task. Closing from here would wait on this very call.
        self.ended.cancel();
    }
}

/// A live connection to one agent, and the child behind it.
pub(crate) struct ConnectionHandle {
    /// The JSON-RPC peer. Shared with nothing that outlives this handle: the handlers reach it
    /// through a weak reference.
    client: Arc<jsonrpc::Client>,
    control: Arc<dyn ProcessControl>,
    /// The one owner shared with lifecycle watchers that may also need to reap the child.
    child_reaper: ChildReaper,
    /// Host-owned bounds for tearing down the connection and child process.
    limits: mango_external_agents::Limits,
    /// Fires once the connection is over, whichever way it ended: the agent exited, the link
    /// failed, a budget was passed, this handle shut it down, or this handle was dropped.
    ended: mango_external_agents::CancelToken,
    shutdown_started: AtomicBool,
    shutdown_complete: mango_external_agents::CancelToken,
    /// The typed outcome replayed through core `Error: Clone` to every close waiter.
    shutdown_result: Mutex<Option<Result<()>>>,
    /// Which request passed its deadline, when that is why this side ended the connection.
    timed_out: std::sync::OnceLock<String>,
    /// The runtime the connection was driven on. Shutdown is spawned through it, so a request
    /// or scope guard dropped on a thread without a runtime still ends the child.
    runtime: tokio::runtime::Handle,
    /// How many shutdown tasks `begin_shutdown` started; the single-flight guard keeps it at one.
    #[cfg(test)]
    shutdown_runs: std::sync::atomic::AtomicUsize,
    /// Bounds the requests that are not prompts. A prompt has its own single-turn admission in
    /// `SessionState` and does not use this permit.
    requests: RequestAdmission,
}

/// Reaps one child once without letting cancellation abandon its launched kill task.
#[derive(Clone)]
pub(crate) struct ChildReaper {
    control: Arc<dyn ProcessControl>,
    claimed: Arc<AtomicBool>,
    limits: Limits,
    result: Arc<Mutex<Option<Result<()>>>>,
    done: mango_external_agents::CancelToken,
}

impl ChildReaper {
    fn new(control: Arc<dyn ProcessControl>, limits: Limits) -> Self {
        Self {
            control,
            limits,
            result: Arc::new(Mutex::new(None)),
            claimed: Arc::new(AtomicBool::new(false)),
            done: mango_external_agents::CancelToken::new(),
        }
    }

    /// Starts bounded cleanup that outlives a caller cancelled while awaiting it.
    pub(crate) async fn reap(&self, reason: mango_external_agents::CancelReason) -> Result<()> {
        if !self.claimed.swap(true, Ordering::AcqRel) {
            let control = Arc::clone(&self.control);
            let done = self.done.clone();
            let limits = self.limits;
            let result = Arc::clone(&self.result);
            tokio::spawn(async move {
                let _complete = ReapCompletion(done);
                // On a task of its own, so a host control that panics is told apart from this
                // reaper being lost: the outcome below names the host's control, not the agent.
                let stopping = Arc::clone(&control);
                let outcome = tokio::spawn(async move {
                    stop_process_with_limits(stopping.as_ref(), reason, &limits).await
                })
                .await
                .map_or_else(
                    |joined| Err(Error::Vendor(link_failure(control_stop_lost(&joined)))),
                    |stopped| stopped.map(|_| ()),
                )
                .map_err(|source| Error::CleanupRequired {
                    // This control owns the launched child. Keep any nested cleanup handle in
                    // its original typed source instead of substituting it for this child.
                    control,
                    source: Box::new(source),
                });
                *result.lock().unwrap_or_else(PoisonError::into_inner) = Some(outcome);
            });
        }
        self.done.cancelled().await;
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| {
                Err(Error::CleanupRequired {
                    control: Arc::clone(&self.control),
                    source: Box::new(Error::Vendor(link_failure(String::from(
                        "ACP child cleanup ended without an outcome",
                    )))),
                })
            })
    }
}

/// What a stop task that never answered says about itself: the host's `ProcessControl` panicked
/// or its task was cancelled. The agent is not the party that failed.
///
/// The message rides the `acp-link-closed` code like every other cleanup failure.
///
/// ```ignore
/// let message = control_stop_lost(&join_error);
/// assert!(message.ends_with("expected the host's process stop to return | received: it panicked"));
/// ```
fn control_stop_lost(joined: &tokio::task::JoinError) -> String {
    let shape = if joined.is_panic() {
        "panicked"
    } else {
        "was cancelled"
    };
    format!(
        "the host's process control failed: expected the host's process stop to return | received: it {shape}"
    )
}

/// Signals every competing reaper even if an injected process control panics.
struct ReapCompletion(mango_external_agents::CancelToken);

impl Drop for ReapCompletion {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Owns a short-lived connection until its request completes or the caller cancels it.
///
/// Listing has no session handle that could close the child later. This guard makes cancellation of
/// that picker request take the same shutdown path as an explicit close.
pub(crate) struct ConnectionShutdownGuard {
    connection: Option<Arc<ConnectionHandle>>,
}

impl ConnectionShutdownGuard {
    /// Starts a scope that ends the connection when it leaves the async call.
    pub(crate) fn new(connection: Arc<ConnectionHandle>) -> Self {
        Self {
            connection: Some(connection),
        }
    }

    /// The live connection while the scope remains active.
    pub(crate) fn connection(&self) -> &Arc<ConnectionHandle> {
        self.connection
            .as_ref()
            .expect("connection exists until an explicit shutdown completes")
    }

    /// Ends the child before the normal scope exit.
    pub(crate) async fn shutdown(
        &mut self,
        reason: mango_external_agents::CancelReason,
    ) -> Result<()> {
        let Some(connection) = self.connection.as_ref() else {
            return Ok(());
        };
        // A dropped request can already have claimed shutdown through `RequestAbandonment`.
        // Always join that owned operation instead of running a second direct cleanup that could
        // race its driver and obscure the outcome from this explicit scope owner.
        connection.begin_shutdown(reason);
        let result = connection.wait_shutdown().await;
        self.connection.take();
        result
    }

    /// Hands ownership to a session that will close the connection later.
    pub(crate) fn release(mut self) -> Arc<ConnectionHandle> {
        self.connection
            .take()
            .expect("connection exists until ownership moves to a session")
    }
}

impl Drop for ConnectionShutdownGuard {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else {
            return;
        };
        // `begin_shutdown` spawns on the connection's own runtime, so this is safe from a plain
        // thread too. Drop has no caller to receive a cleanup failure. Explicit owners use `shutdown`, which
        // preserves its result for them instead.
        connection.begin_shutdown(mango_external_agents::CancelReason::Shutdown);
    }
}

impl std::fmt::Debug for ConnectionHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionHandle")
            .field("pid", &self.control.pid())
            .finish_non_exhaustive()
    }
}

impl Drop for ConnectionHandle {
    /// Wakes whoever watches this connection when the handle goes without a shutdown.
    ///
    /// Dropping the peer takes its tasks down without telling its handler, so the handler's own
    /// signal never comes on this path. The session's watcher holds the child's reaper and no
    /// handle, and this is what lets it reap a session that was dropped while idle.
    fn drop(&mut self) {
        self.ended.cancel();
    }
}

/// Starts the connection to one launched agent and hands back the handle that owns it.
///
/// Nothing here waits: the peer starts reading and writing on tasks of its own, and an agent
/// that exits or floods before it answers anything fails the first request made of it, naming
/// why. Must be called on the runtime the connection is to live on.
///
/// ```ignore
/// let launched = transport::connect(host, &spec, keys).await?;
/// let connection = Arc::new(client::drive(launched, Arc::clone(&state)));
/// ```
pub(crate) fn drive(launched: LaunchedAgent, state: Arc<SessionState>) -> ConnectionHandle {
    let LaunchedAgent {
        link,
        control,
        outbound_buffer_bytes,
    } = launched;
    let limits = state.limits;
    let ended = mango_external_agents::CancelToken::new();
    let handler = Arc::new(AcpHandler {
        state: Arc::clone(&state),
        ended: ended.clone(),
    });
    let client = Arc::new(jsonrpc::Client::connect_with(
        link,
        handler,
        crate::transport::client_options(&limits),
        crate::transport::wire_options(&limits, outbound_buffer_bytes),
    ));
    state.attach_peer(&client);
    ConnectionHandle {
        client,
        child_reaper: ChildReaper::new(Arc::clone(&control), limits),
        control,
        limits,
        ended,
        shutdown_started: AtomicBool::new(false),
        shutdown_complete: mango_external_agents::CancelToken::new(),
        shutdown_result: Mutex::new(None),
        timed_out: std::sync::OnceLock::new(),
        runtime: tokio::runtime::Handle::current(),
        #[cfg(test)]
        shutdown_runs: std::sync::atomic::AtomicUsize::new(0),
        requests: RequestAdmission::new(limits.max_pending_requests),
    }
}

impl ConnectionHandle {
    /// Starts bounded shutdown in an owned task so dropping the initiating session future cannot
    /// abandon the child. Later callers join the same completion signal.
    pub(crate) fn begin_shutdown(self: &Arc<Self>, reason: mango_external_agents::CancelReason) {
        // Serialised with `RequestAdmission::submit`, so no request can be queued after a caller
        // has begun teardown.
        self.requests.close();
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let connection = Arc::clone(self);
        #[cfg(test)]
        self.shutdown_runs.fetch_add(1, Ordering::AcqRel);
        self.runtime.spawn(async move {
            let result = connection.shutdown(reason).await;
            *connection
                .shutdown_result
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(result);
            connection.shutdown_complete.cancel();
        });
    }

    /// Records that `method` got no answer within `after`, which is about to end the connection.
    ///
    /// The first one is kept: it is the one that ended it. A turn that was running on the
    /// connection reads it through [`Self::timed_out`] to say why its link closed. For
    /// example, `record_timeout("session/list", Duration::from_secs(120))` reads back as
    /// `session/list got no answer within 120s`.
    pub(super) fn record_timeout(&self, method: &str, after: std::time::Duration) {
        let _ = self
            .timed_out
            .set(format!("{method} got no answer within {after:?}"));
    }

    /// The request whose deadline ended this connection, if one did.
    ///
    /// A method name of this crate's and a duration: nothing of the agent's, and bounded.
    ///
    /// ```ignore
    /// let cause = connection.timed_out();
    /// ```
    pub(crate) fn timed_out(&self) -> Option<&str> {
        self.timed_out.get().map(String::as_str)
    }

    /// Waits for the shutdown task started by [`Self::begin_shutdown`].
    pub(crate) async fn wait_shutdown(&self) -> Result<()> {
        self.shutdown_complete.cancelled().await;
        self.shutdown_result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| {
                Err(Error::CleanupRequired {
                    control: Arc::clone(&self.child_reaper.control),
                    source: Box::new(Error::Vendor(link_failure(String::from(
                        "the ACP shutdown task ended without an outcome",
                    )))),
                })
            })
    }

    /// The child, for diagnostics and for ending it.
    pub(crate) fn control(&self) -> &Arc<dyn ProcessControl> {
        &self.control
    }

    /// The shared owner a watcher can use without retaining a connection clone.
    pub(crate) fn child_reaper(&self) -> ChildReaper {
        self.child_reaper.clone()
    }

    /// Fires once the connection is over, whichever way it ended.
    ///
    /// One signal for every end: the agent closing its output, a failed link, a budget passed in
    /// either direction, this handle's own shutdown, and the handle being dropped. Anything
    /// watching for a dead agent waits on this.
    pub(crate) fn ended(&self) -> &mango_external_agents::CancelToken {
        &self.ended
    }

    /// The queue budget that ended this connection, if one did.
    ///
    /// On record before any request is failed with it, so a request or turn that observed the
    /// failure can read it. For example, a prompt ended by an oversized frame from the agent
    /// reports this instead of a cancellation. The first cause of an end is kept, so a close that
    /// follows an overflow does not replace it.
    pub(crate) fn overflow(&self) -> Option<Overflow> {
        match self.client.ended()? {
            jsonrpc::ConnectionEnd::Peer(termination) => Overflow::from_termination(&termination),
            _ => None,
        }
    }

    /// Queues `session/cancel` for `session_id`, behind every frame queued before it.
    ///
    /// A notification, so there is no answer to wait for: the agent reports the outcome on the
    /// prompt's own response. Synchronous, which is what keeps a cancel behind the prompt it
    /// stops when both are queued under one guard.
    ///
    /// # Errors
    ///
    /// Whatever the connection refused the frame with: it is closed, its link is unusable, or
    /// its outgoing queue is full.
    ///
    /// ```ignore
    /// connection.cancel_prompt(native_session_id.clone())?;
    /// ```
    pub(crate) fn cancel_prompt(
        &self,
        session_id: agent_client_protocol_schema::v1::SessionId,
    ) -> Result<()> {
        self.client
            .submit_notification(
                AGENT_METHOD_NAMES.session_cancel,
                CancelNotification::new(session_id),
            )
            .map(drop)
    }

    /// The failure of a request the connection ended under, or refused because it had ended.
    ///
    /// The budget that ended the connection when one did, and otherwise `acp-link-closed` naming
    /// the method, with the child's redacted stderr. For example, an agent that printed a usage
    /// message and exited under `initialize` yields
    /// `a transport that closed under initialize; the agent's stderr: usage: …`.
    fn link_closed_under(&self, method: &str) -> Error {
        if let Some(overflow) = self.overflow() {
            return overflow.error();
        }
        Error::Vendor(link_failure(with_stderr(
            &format!("a transport that closed under {method}"),
            self.control.as_ref(),
        )))
    }

    /// What a request the connection would not queue is reported as.
    ///
    /// - A frame over the outgoing budget is that budget, marked not submitted: the refusal is of
    ///   this request alone.
    /// - On a connection that has ended, the same failure a request in flight would have had,
    ///   marked not submitted too: nothing of this request was queued.
    /// - Anything else as [`not_queued`](Self::not_queued) words it.
    fn refused(&self, method: &str, error: Error) -> Error {
        if let Some(overflow) = Overflow::from_refused_frame(&error) {
            return overflow
                .error()
                .with_dispatch(mango_external_agents::Dispatch::NotSubmitted);
        }
        if self.client.ended().is_some() {
            return self
                .link_closed_under(method)
                .with_dispatch(mango_external_agents::Dispatch::NotSubmitted);
        }
        self.not_queued(error)
    }

    /// A refusal that queued nothing on a live connection, in this crate's own words and
    /// marked not submitted.
    ///
    /// With the frame's size and the connection's end already ruled out, the one count the
    /// peer can still refuse a request for is its reserve of places for answers awaiting
    /// delivery. That reserve is sized from `Limits::max_pending_requests`, so it is reported
    /// as that limit, the one a host set, and not under the peer's name for it. For example,
    /// with 64 requests allowed: `expected at most 64 outstanding ACP requests, received 65`.
    pub(super) fn not_queued(&self, error: Error) -> Error {
        let reworded = match error.cause() {
            Error::LimitExceeded { .. } => Error::LimitExceeded {
                subject: requests::OUTSTANDING_REQUESTS,
                limit: self.limits.max_pending_requests,
                received: self.limits.max_pending_requests.saturating_add(1),
            },
            _ => error,
        };
        reworded.with_dispatch(mango_external_agents::Dispatch::NotSubmitted)
    }

    /// Ends the connection and the child. Idempotent.
    ///
    /// Closing the connection first is what lets a well-behaved agent see its stdin end and exit on
    /// its own; the kill is the escalation for one that does not, and it is the launcher's own —
    /// this only asks, with the reason.
    ///
    /// The close is given `shutdown_timeout`. The peer's own bound is twice that, for a
    /// connection that has both answers to write and frames queued; past the first one the close
    /// is given up, which leaves the link unusable and the child to the reaper. A close that
    /// reports an error is not a failed teardown either: a blocked writer makes it report one
    /// after the child is gone, and process cleanup is the fact that decides.
    pub(crate) async fn shutdown(&self, reason: mango_external_agents::CancelReason) -> Result<()> {
        let _ = tokio::time::timeout(
            crate::deadlines::connection_close_bound(&self.limits),
            self.client.close(),
        )
        .await;
        self.ended.cancel();
        self.child_reaper.reap(reason).await
    }
}

/// A message with the child's stderr appended, when it wrote any.
///
/// The tail is redacted here, whatever the control did: one from the library's own launchers
/// arrives redacted from [`StderrTail`](mango_external_agents::StderrTail), but a host's own
/// `ProcessControl` is only asked to redact, and this text goes into an error the host logs. A
/// tail is read once, here, so it is read for well-known tokens too (`redact::stderr_tail`). The
/// second pass never reveals anything and may redact more.
pub(crate) fn with_stderr(message: &str, control: &dyn ProcessControl) -> String {
    let tail = mango_external_agents::redact::stderr_tail(&control.stderr_tail());
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

/// The failure a turn ends with when a transport budget failed the agent's link under it.
///
/// Mirrors the core's `stream-overflow`: the code names the cause and the message is the typed
/// [`Error::LimitExceeded`] text. For example, a 50-message batch under an 8-message cap ends the
/// turn with `acp-transport-overflow` and
/// `expected at most 8 JSON-RPC messages queued from the ACP agent, received 50`.
pub(crate) fn overflow_failure(overflow: crate::transport::Overflow) -> VendorError {
    VendorError::new(
        mango_external_agents::ErrorCode::from_static("acp-transport-overflow"),
        overflow.error().to_string(),
    )
}

#[cfg(test)]
mod commit_hold_tests;

#[cfg(test)]
mod tests {
    mod refusal_tests;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use mango_external_agents::testing::{FakeLauncher, FakeProcess};
    use mango_external_agents::{
        AttemptId, CancelReason, Configuration, Error, EventSink, ExitStatus, HarnessIdentity,
        HostContext, LaunchSpec, ProcessControl, ProcessLauncher, Result, SessionIds,
        SessionSnapshot, TransportKind, TransportSelection, TurnId,
    };

    use super::{PermissionLevel, SessionId, SessionState, drive};

    /// A process control that records how many cleanup claims reach the injected child.
    struct CountingProcessControl {
        inner: Arc<dyn ProcessControl>,
        kills: Arc<AtomicUsize>,
    }

    /// A named process control whose kill waits until the test releases it.
    struct HeldKillProcessControl {
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        completed: AtomicBool,
    }

    #[async_trait::async_trait]
    impl ProcessControl for HeldKillProcessControl {
        fn pid(&self) -> Option<u32> {
            None
        }

        fn stderr_tail(&self) -> String {
            String::new()
        }

        async fn wait(&self) -> Result<ExitStatus> {
            Ok(ExitStatus::default())
        }

        async fn kill(&self, _reason: CancelReason) -> Result<()> {
            if let Some(entered) = self
                .entered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
            {
                let _ = entered.send(());
            }
            let release = self
                .release
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(release) = release {
                let _ = release.await;
            }
            self.completed.store(true, Ordering::Release);
            Ok(())
        }
    }

    /// A named process control whose stop panics, the way a buggy host control would.
    struct PanickingStopControl;

    #[async_trait::async_trait]
    impl ProcessControl for PanickingStopControl {
        fn pid(&self) -> Option<u32> {
            None
        }

        fn stderr_tail(&self) -> String {
            String::new()
        }

        async fn wait(&self) -> Result<ExitStatus> {
            Ok(ExitStatus::default())
        }

        async fn kill(&self, _reason: CancelReason) -> Result<()> {
            panic!("test control stop panicked");
        }
    }

    #[async_trait::async_trait]
    impl ProcessControl for CountingProcessControl {
        fn pid(&self) -> Option<u32> {
            self.inner.pid()
        }

        fn stderr_tail(&self) -> String {
            self.inner.stderr_tail()
        }

        async fn wait(&self) -> Result<ExitStatus> {
            self.inner.wait().await
        }

        async fn kill(&self, reason: CancelReason) -> Result<()> {
            self.kills.fetch_add(1, Ordering::AcqRel);
            self.inner.kill(reason).await
        }
    }

    /// A bare core session state, for the connection-level state under test to publish facts into.
    fn core_state() -> mango_external_agents::SessionState {
        mango_external_agents::SessionState::new(
            std::sync::Arc::new(mango_external_agents::SystemClock),
            SessionSnapshot::opening(
                SessionIds {
                    session_id: SessionId::new("session-1"),
                    native_session_id: String::from("native-1"),
                },
                HarnessIdentity::claude(),
                TransportSelection::new(None, TransportKind::Acp),
                std::time::SystemTime::UNIX_EPOCH,
            ),
        )
    }

    fn state() -> (SessionState, HostContext) {
        let host = HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd(std::env::temp_dir())
            .client_info("acp-client-tests", "0.1.0")
            .build()
            .expect("expected a host context");
        let state = SessionState::new(
            SessionId::new("session-1"),
            &host,
            Configuration::default(),
            core_state(),
        );
        (state, host)
    }

    fn sink(host: &HostContext, turn_id: &str) -> EventSink {
        let (sink, _events) = EventSink::new(
            SessionId::new("session-1"),
            TurnId::new(turn_id),
            AttemptId::default(),
            Arc::clone(host.clock()),
            1,
        );
        sink
    }

    /// An agent that exits before the connection opens has said its only useful thing on stderr.
    ///
    /// Nothing is returned from that path — no session and no `ProcessControl` — so `StderrTail`
    /// is out of reach and the tail has to travel on the error. `VendorError::message` is the
    /// field for vendor text a host reads and `Display` never writes, which is why this path
    /// answers with one instead of an `Error::Link` whose summary is written verbatim.
    #[test]
    fn an_agent_that_never_opened_carries_its_stderr_where_a_host_can_read_it() {
        use mango_external_agents::{CancelReason, ExitStatus, ProcessControl, Result};

        /// A control that only ever reports a tail.
        struct ExitedAgent;

        #[async_trait::async_trait]
        impl ProcessControl for ExitedAgent {
            fn pid(&self) -> Option<u32> {
                None
            }

            fn stderr_tail(&self) -> String {
                String::from("error: unknown flag --acp")
            }

            async fn wait(&self) -> Result<ExitStatus> {
                Ok(ExitStatus::default())
            }

            async fn kill(&self, _reason: CancelReason) -> Result<()> {
                Ok(())
            }
        }

        let error = mango_external_agents::Error::Vendor(super::link_failure(super::with_stderr(
            "a connection that closed before it opened",
            &ExitedAgent,
        )));

        let mango_external_agents::Error::Vendor(vendor) = &error else {
            panic!("expected a vendor failure, received {error:?}");
        };
        assert_eq!(vendor.code.as_str(), "acp-link-closed");
        assert!(
            vendor.message.contains("unknown flag --acp"),
            "expected the agent's own stderr on the field a host reads, received {}",
            vendor.message
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("unknown flag"),
                "expected the tail to stay off the diagnostic, received {rendered}"
            );
        }
    }

    /// A host's own `ProcessControl`, whose tail is whatever the child wrote: the trait asks an
    /// implementor to redact it and nothing checks that it did.
    struct UnredactedTailControl {
        tail: &'static str,
    }

    #[async_trait::async_trait]
    impl mango_external_agents::ProcessControl for UnredactedTailControl {
        fn pid(&self) -> Option<u32> {
            None
        }

        fn stderr_tail(&self) -> String {
            self.tail.to_owned()
        }

        async fn wait(&self) -> mango_external_agents::Result<mango_external_agents::ExitStatus> {
            Ok(mango_external_agents::ExitStatus::default())
        }

        async fn kill(
            &self,
            _reason: mango_external_agents::CancelReason,
        ) -> mango_external_agents::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_link_failure_redacts_a_stderr_tail_the_host_did_not() {
        let control = UnredactedTailControl {
            tail: "fatal: API_KEY=sk-host-unredacted\nAuthorization: Bearer host-bearer-secret",
        };

        let message = super::with_stderr("the agent exited", &control);

        for secret in ["sk-host-unredacted", "host-bearer-secret"] {
            assert!(
                !message.contains(secret),
                "expected a tail the host did not redact to be redacted before it is reported | received {message:?}"
            );
        }
        assert!(
            message.contains("the agent's stderr: fatal: API_KEY=[REDACTED]"),
            "expected the redacted stderr in the message | received {message:?}"
        );
    }

    #[test]
    fn a_link_failure_hides_a_bare_token_in_a_stderr_tail_the_host_did_not_redact() {
        const TOKEN: &str = concat!("gh", "p_0123456789abcdefghijklmnopqrstuvwxyz");
        let control = UnredactedTailControl {
            tail: concat!(
                "fatal: push rejected for gh",
                "p_0123456789abcdefghijklmnopqrstuvwxyz",
                " by the remote"
            ),
        };

        let message = super::with_stderr("the agent exited", &control);

        assert!(
            !message.contains(TOKEN)
                && message.contains(
                    "the agent's stderr: fatal: push rejected for [REDACTED] by the remote"
                ),
            "expected a bare token in the tail hidden in the message | received {message:?}"
        );
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
            .prepare_terminal_matching(&first)
            .expect("expected the first turn to still own the slot");
        state.release_turn_matching(&first);
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
        state
            .prepare_terminal_matching(&first)
            .expect("expected the installed handle to end");
        state.release_turn_matching(&first);

        assert!(
            !state.can_submit_prompt(&first),
            "an ended handle must not retain authority to submit a prompt"
        );
    }

    #[test]
    fn an_overflow_failure_names_the_code_the_limit_and_the_received_count() {
        let overflow = crate::transport::Overflow::incoming_messages(8, 50);
        let failure = super::overflow_failure(overflow);
        assert_eq!(failure.code.as_str(), "acp-transport-overflow");
        assert_eq!(
            failure.message,
            "expected at most 8 JSON-RPC messages queued from the ACP agent, received 50",
            "expected the typed limit text, received {:?}",
            failure.message
        );
    }

    /// Cancelling a shutdown waiter cannot cancel the child kill it already submitted.
    #[tokio::test]
    async fn a_cancelled_reaper_wait_keeps_the_submitted_kill_running() {
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let held = Arc::new(HeldKillProcessControl {
            entered: Mutex::new(Some(entered_tx)),
            release: Mutex::new(Some(release_rx)),
            completed: AtomicBool::new(false),
        });
        let control: Arc<dyn ProcessControl> = held.clone();
        let reaper = super::ChildReaper::new(control, mango_external_agents::Limits::default());
        let task = tokio::spawn({
            let reaper = reaper.clone();
            async move { reaper.reap(CancelReason::Shutdown).await }
        });
        entered.await.expect("expected the kill task to start");
        task.abort();
        let _ = task.await;
        release
            .send(())
            .expect("expected the held kill to remain owned");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !held.completed.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected the detached kill to complete after its waiter was cancelled");
    }

    /// A host control that panics in its stop is reported as the host's control failing, under the
    /// same `acp-link-closed` code, and still hands the control back for reconciliation.
    #[tokio::test]
    async fn a_panicking_control_stop_names_the_host_control_under_the_link_code() {
        let control: Arc<dyn ProcessControl> = Arc::new(PanickingStopControl);
        let reaper = super::ChildReaper::new(control, mango_external_agents::Limits::default());

        let error =
            tokio::time::timeout(Duration::from_secs(5), reaper.reap(CancelReason::Shutdown))
                .await
                .expect(
                    "expected reap: settled after a panicking control | received: still pending",
                )
                .expect_err("expected reap result: Err(CleanupRequired) | received: Ok");

        assert!(
            error.cleanup_control().is_some(),
            "expected a cleanup-required error carrying the host's control | received: {error:?}"
        );
        let Error::CleanupRequired { source, .. } = &error else {
            panic!("expected Error::CleanupRequired | received: {error:?}");
        };
        let Error::Vendor(vendor) = source.as_ref() else {
            panic!("expected the cause: Error::Vendor | received: {source:?}");
        };
        assert_eq!(
            vendor.code.as_str(),
            "acp-link-closed",
            "expected the vendor code: acp-link-closed | received: {}",
            vendor.code.as_str()
        );
        assert!(
            vendor.message.contains("the host's process control")
                && vendor.message.contains("panicked"),
            "expected a message naming the host's process control and `panicked` | received: {}",
            vendor.message
        );
    }

    /// A stop task the runtime lost, by a panic or by an abort, is told as the host's control
    /// failing, never as the agent failing.
    #[tokio::test]
    async fn a_lost_control_stop_names_the_host_control_and_how_it_was_lost() {
        let panicked = tokio::spawn(async { panic!("test control stop panicked") });
        let panic_error = panicked
            .await
            .expect_err("expected the panicking task to fail its join");
        let aborted = tokio::spawn(std::future::pending::<()>());
        aborted.abort();
        let abort_error = aborted
            .await
            .expect_err("expected the aborted task to fail its join");

        for (joined, shape) in [(panic_error, "panicked"), (abort_error, "was cancelled")] {
            let message = super::control_stop_lost(&joined);
            let expected =
                format!("expected the host's process stop to return | received: it {shape}");
            assert!(
                message.starts_with("the host's process control") && message.ends_with(&expected),
                "expected a message from `the host's process control` ending in `{expected}` | received: {message}"
            );
        }
    }

    /// A live connection over a silent fake child whose kills are counted.
    async fn counted_connection() -> (Arc<super::ConnectionHandle>, Arc<AtomicUsize>, FakeLauncher)
    {
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let (state, host) = state();
        let mut process = launcher
            .spawn(LaunchSpec {
                argv: vec![String::from("fake-acp")],
                cwd: host.cwd().to_path_buf(),
                env: Default::default(),
                stdin: true,
                hide_window: true,
            })
            .await
            .expect("expected the fake child to launch");
        let kills = Arc::new(AtomicUsize::new(0));
        process.control = Arc::new(CountingProcessControl {
            inner: Arc::clone(&process.control),
            kills: Arc::clone(&kills),
        });
        let launched = crate::transport::frame(process, &host).expect("expected framed child");
        let connection = drive(launched, Arc::new(state));
        (Arc::new(connection), kills, launcher)
    }

    /// A named host control that refuses cleanup with a typed, already-owned cause.
    struct TypedStopFailure {
        error: Error,
        kills: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ProcessControl for TypedStopFailure {
        fn pid(&self) -> Option<u32> {
            None
        }
        fn stderr_tail(&self) -> String {
            String::new()
        }
        async fn wait(&self) -> Result<ExitStatus> {
            Ok(ExitStatus::default())
        }
        async fn kill(&self, _reason: CancelReason) -> Result<()> {
            self.kills.fetch_add(1, Ordering::AcqRel);
            Err(self.error.clone())
        }
    }

    /// Waiters retain the launched child's control and the complete nested cleanup evidence.
    #[tokio::test]
    async fn shared_teardown_replays_typed_cause_dispatch_and_original_control() {
        let (connection, _, _launcher) = counted_connection().await;
        let mut connection = Arc::try_unwrap(connection).expect("sole connection owner");
        let original = Arc::clone(&connection.control);
        let control = Arc::new(TypedStopFailure {
            error: Error::CleanupRequired {
                control: Arc::clone(&original),
                source: Box::new(
                    Error::Protocol {
                        expected: String::from("the injected cleanup proof"),
                        received: String::from("unconfirmed"),
                    }
                    .with_dispatch(mango_external_agents::Dispatch::Accepted),
                ),
            },
            kills: AtomicUsize::new(0),
        });
        let injected: Arc<dyn ProcessControl> = control.clone();
        connection.control = Arc::clone(&injected);
        connection.child_reaper = super::ChildReaper::new(control.clone(), connection.limits);
        let reaper = connection.child_reaper.clone();
        let connection = Arc::new(connection);
        connection.begin_shutdown(CancelReason::Shutdown);
        let (first, second, third) = tokio::join!(
            reaper.reap(CancelReason::Requested),
            connection.wait_shutdown(),
            connection.wait_shutdown(),
        );
        let kills = control.kills.load(Ordering::Acquire);
        original
            .kill(CancelReason::Shutdown)
            .await
            .expect("release fake child before replay assertions");
        for result in [first, second, third] {
            let error = result.expect_err("incomplete cleanup");
            assert_eq!(error.dispatch(), mango_external_agents::Dispatch::Accepted);
            assert!(Arc::ptr_eq(
                &error.cleanup_control().expect("retained original control"),
                &injected
            ));
            let nested = match &error {
                Error::CleanupRequired { source, .. } => {
                    source.cleanup_control().expect("nested cleanup evidence")
                }
                received => {
                    panic!("expected launched-child cleanup ownership, received {received:?}")
                }
            };
            assert!(Arc::ptr_eq(&nested, &original));
            assert!(
                matches!(error.cause(), Error::Protocol { expected, received }
                if expected == "the injected cleanup proof" && received == "unconfirmed")
            );
        }
        assert_eq!(kills, 1);
    }

    /// The child reaper is its own single-flight owner: two reaps, even concurrent, kill once and
    /// both observe the one outcome.
    #[tokio::test]
    async fn two_reaps_of_one_child_kill_it_once() {
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let (_, host) = state();
        let process = launcher
            .spawn(LaunchSpec {
                argv: vec![String::from("fake-acp")],
                cwd: host.cwd().to_path_buf(),
                env: Default::default(),
                stdin: true,
                hide_window: true,
            })
            .await
            .expect("expected the fake child to launch");
        let kills = Arc::new(AtomicUsize::new(0));
        let reaper = super::ChildReaper::new(
            Arc::new(CountingProcessControl {
                inner: process.control,
                kills: Arc::clone(&kills),
            }),
            *host.limits(),
        );
        let (first, second) = tokio::join!(
            reaper.reap(CancelReason::Shutdown),
            reaper.reap(CancelReason::Requested)
        );
        first.expect("expected the first reap to succeed");
        second.expect("expected the joined reap to succeed");
        let kills = kills.load(Ordering::Acquire);
        assert_eq!(kills, 1, "expected kills: 1 | received: {kills}");
    }

    /// `begin_shutdown` is its own single-flight owner, independently of the child reaper: a
    /// second call joins the first shutdown task rather than running another.
    #[tokio::test]
    async fn two_shutdown_requests_run_one_shutdown_task() {
        let (connection, kills, launcher) = counted_connection().await;
        connection.begin_shutdown(CancelReason::Shutdown);
        connection.begin_shutdown(CancelReason::Requested);
        connection
            .wait_shutdown()
            .await
            .expect("expected the shared shutdown to succeed");
        let runs = connection.shutdown_runs.load(Ordering::Acquire);
        assert_eq!(runs, 1, "expected shutdown task runs: 1 | received: {runs}");
        let kills = kills.load(Ordering::Acquire);
        assert_eq!(kills, 1, "expected kills: 1 | received: {kills}");
        assert_eq!(launcher.live_children(), 0, "expected live children: 0");
    }

    /// A shutdown guard dropped on a thread with no runtime still ends its child through the
    /// runtime it was created on.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_shutdown_guard_dropped_outside_a_runtime_reaps_its_child() {
        let (connection, kills, launcher) = counted_connection().await;
        let guard = super::ConnectionShutdownGuard::new(connection);
        std::thread::spawn(move || drop(guard))
            .join()
            .expect("expected the dropping thread to finish");
        // Both, not the child alone: closing the connection ends the fake child by closing its
        // stdin, a moment before the reaper asks the launcher to stop it.
        let reaped = tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.live_children() != 0 || kills.load(Ordering::Acquire) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            reaped.is_ok(),
            "expected live children: 0 after a drop outside a runtime | received: {}",
            launcher.live_children()
        );
        let kills = kills.load(Ordering::Acquire);
        assert_eq!(kills, 1, "expected kills: 1 | received: {kills}");
    }

    /// With no turn in the slot there is nothing left to settle, so the watcher is not held.
    #[tokio::test]
    async fn waiting_for_no_turn_returns_at_once_when_the_slot_is_empty() {
        let (state, _) = state();
        tokio::time::timeout(Duration::from_secs(1), state.wait_for_no_turn())
            .await
            .expect("expected wait_for_no_turn: immediate with no turn | received: still pending");
    }

    /// An in-flight request dropped on a thread with no runtime must not panic its owner and must
    /// still start the connection's shutdown, through the runtime the connection was driven on.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_request_abandoned_outside_a_runtime_still_shuts_the_connection_down() {
        let (connection, kills, launcher) = counted_connection().await;
        let dropped = std::thread::scope(|scope| {
            let submitted = super::submit(
                &connection,
                agent_client_protocol_schema::v1::CloseSessionRequest::new(
                    agent_client_protocol_schema::v1::SessionId::new("sess_abandoned"),
                ),
            )
            .expect("expected the request to be admitted");
            scope
                .spawn(move || drop(submitted))
                .join()
                .map_err(|_| "the dropping thread panicked")
        });
        assert!(
            dropped.is_ok(),
            "expected an off-runtime drop: no panic | received: {dropped:?}"
        );
        tokio::time::timeout(Duration::from_secs(5), connection.wait_shutdown())
            .await
            .expect("expected the abandoned request to have started shutdown")
            .expect("expected the shutdown to succeed");
        let kills = kills.load(Ordering::Acquire);
        assert_eq!(kills, 1, "expected kills: 1 | received: {kills}");
        assert_eq!(launcher.live_children(), 0, "expected live children: 0");
    }

    /// After the watcher closes admission, no turn can take the slot on the dead connection.
    #[tokio::test]
    async fn a_closed_turn_admission_refuses_the_next_turn() {
        let (state, host) = state();
        state.close_turn_admission();
        let refused = state.begin_turn(sink(&host, "turn-late"), None);
        assert!(
            matches!(refused, Err(Error::Closed { .. })),
            "expected begin_turn after admission closed: Err(Closed) | received: {:?}",
            refused.map(|_| "an installed turn")
        );
    }

    /// A slot the prompt task never releases turns into a typed timeout naming what was held,
    /// rather than an `Ok` that would let the session look settled.
    /// The binding follows a fallback resume: frames for the session being loaded are this
    /// session's, frames for it once the load is abandoned are not — even before the fresh
    /// session names itself — and once that one binds, only it is.
    #[test]
    fn a_fallback_refuses_the_abandoned_session_until_and_after_the_new_one_binds() {
        use agent_client_protocol_schema::v1::SessionId as AcpSessionId;
        let (state, _host) = state();
        let loading = AcpSessionId::new("sess_loading");
        let fresh = AcpSessionId::new("sess_fresh");
        assert!(
            state.serves(&loading),
            "expected an unbound session to accept any id"
        );

        state.bind_native_session(loading.clone());
        assert!(state.serves(&loading));
        assert!(
            !state.serves(&fresh),
            "expected only the loading id while bound"
        );

        state.abandon_native_session();
        assert!(
            !state.serves(&loading),
            "expected the abandoned id refused while the fallback opens"
        );
        assert!(
            state.serves(&fresh),
            "expected the fallback's own early frames accepted before it binds"
        );

        state.bind_native_session(fresh.clone());
        assert!(state.serves(&fresh));
        assert!(
            !state.serves(&loading),
            "expected the abandoned id still refused"
        );
    }

    /// A frame that arrives on the same timer tick as the idle deadline is progress, not silence.
    ///
    /// The frame's task sleeps a shade less than the deadline, so both timers fall due on one
    /// tick and the frame's runs first; the idle wait is then polled with its sleep and its change
    /// both ready. `select!` picks between ready branches at random, so the case repeats until an
    /// unguarded sleep would have won at least once.
    #[tokio::test(start_paused = true)]
    async fn a_frame_on_the_idle_deadline_tick_restarts_the_deadline() {
        let idle = Duration::from_secs(5);
        for round in 0..32 {
            let (state, _host) = state();
            let state = Arc::new(state);
            let frame = tokio::spawn({
                let state = Arc::clone(&state);
                async move {
                    tokio::time::sleep(idle - Duration::from_micros(500)).await;
                    state.touch();
                }
            });
            let waiter = tokio::spawn({
                let state = Arc::clone(&state);
                async move { state.idle_expired(idle).await }
            });
            tokio::time::sleep(idle + Duration::from_millis(1)).await;
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert!(
                frame.is_finished(),
                "expected the frame to have arrived in round {round}"
            );
            assert!(
                !waiter.is_finished(),
                "expected idle state: waiting after a frame on the deadline tick | received: expired, in round {round}"
            );
            waiter.abort();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreleased_turn_slot_is_a_typed_timeout() {
        let (state, host) = state();
        let _held = state
            .begin_turn(sink(&host, "turn-held"), None)
            .expect("expected the turn to take the slot");
        let bound = Duration::from_secs(3);
        let error = crate::session::wait_for_turn_release(&state, bound)
            .await
            .expect_err("expected a held slot: Err(Timeout) | received: Ok");
        assert!(
            matches!(&error, Error::Timeout { operation, after }
                if operation.contains("turn slot release") && *after == bound),
            "expected Timeout naming the turn slot release after {bound:?} | received: {error:?}"
        );
    }

    /// Frames delivered while no turn exists: a `session/load` replay, or a frame between turns.
    mod replay {
        use std::sync::Arc;

        use mango_external_agents::{AttemptId, EventSink, TurnId};

        use super::{SessionId, state};

        /// One `session/update` notification for `session-1`, composed as the agent's JSON.
        pub(super) fn notification(
            update: serde_json::Value,
        ) -> agent_client_protocol_schema::v1::SessionNotification {
            serde_json::from_value(serde_json::json!({
                "sessionId": "session-1",
                "update": update,
            }))
            .expect("expected the test frame to be a v1 session notification")
        }

        pub(super) fn running_call(
            index: usize,
        ) -> agent_client_protocol_schema::v1::SessionNotification {
            notification(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": format!("call-{index}"),
                "title": "Run tests",
                "kind": "execute",
                "status": "in_progress",
                "content": [{ "type": "content", "content": { "type": "text", "text": "output" } }],
            }))
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn delayed_unreadable_update_keeps_its_admitted_turn() {
            for replacing in [false, true] {
                let (state, host) = state();
                let state = Arc::new(state);
                state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                    "session-1",
                ));
                let old = replacing.then(|| {
                    state
                        .begin_turn(super::sink(&host, "old"), None)
                        .expect("expected old turn")
                });
                let (entered, observed) = tokio::sync::oneshot::channel();
                let (release, released) = std::sync::mpsc::channel();
                *state.update_admission_hold.lock().expect("expected gate") =
                    Some(super::super::UpdateAdmissionHold {
                        entered,
                        release: released,
                    });
                let task = tokio::spawn({
                    let state = Arc::clone(&state);
                    async move {
                        let handler = super::super::AcpHandler {
                            state,
                            ended: mango_external_agents::CancelToken::new(),
                        };
                        mango_external_agents::jsonrpc::PeerHandler::on_notification(
                            &handler,
                            "session/update".to_owned(),
                            serde_json::json!({"sessionId":"session-1", "update": 7}),
                        )
                        .await;
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(3), observed)
                    .await
                    .expect("expected admission within 3s")
                    .expect("expected observer");
                if let Some(old) = &old {
                    state.release_turn_matching(old);
                }
                let next = state
                    .begin_turn(super::sink(&host, "next"), None)
                    .expect("expected next turn");
                release.send(()).expect("expected release");
                task.await.expect("expected handler completion");
                let noticed = next
                    .unreadable_update_noticed
                    .load(std::sync::atomic::Ordering::Acquire);
                assert!(
                    !noticed,
                    "expected replacement warning available (replacing={replacing}): true | received: {}",
                    !noticed
                );
            }
        }

        #[tokio::test]
        async fn unidentifiable_unreadable_update_warns_its_turn() {
            let (state, host) = state();
            let state = Arc::new(state);
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                "session-1",
            ));
            let turn = state
                .begin_turn(super::sink(&host, "turn"), None)
                .expect("expected turn");
            let handler = super::super::AcpHandler {
                state,
                ended: mango_external_agents::CancelToken::new(),
            };
            mango_external_agents::jsonrpc::PeerHandler::on_notification(
                &handler,
                "session/update".to_owned(),
                serde_json::json!({"update": 7}),
            )
            .await;
            let noticed = turn
                .unreadable_update_noticed
                .load(std::sync::atomic::Ordering::Acquire);
            assert!(
                noticed,
                "expected missing-ID warning consumed: true | received: {noticed}"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn opening_update_does_not_enter_first_turn_after_admission() {
            let (state, host) = state();
            let state = Arc::new(state);
            let (entered, observed) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            *state.update_admission_hold.lock().expect("expected gate") =
                Some(super::super::UpdateAdmissionHold {
                    entered,
                    release: released,
                });
            let task = tokio::spawn({
                let state = Arc::clone(&state);
                async move {
                    crate::client::on_session_update(&state, running_call(0)).await;
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(3), observed)
                .await
                .expect("expected update admission within 3s")
                .expect("expected gate observer");
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                "session-1",
            ));
            state
                .begin_turn(super::sink(&host, "first"), None)
                .expect("expected first turn");
            release.send(()).expect("expected gate release");
            task.await.expect("expected handler completion");
            let calls = state.open_call_count();
            assert_eq!(
                calls, 0,
                "expected opening update's first-turn open calls: 0 | received: {calls}"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn abandoned_load_update_does_not_restore_cleared_facts() {
            let (state, _) = state();
            let state = Arc::new(state);
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                "session-1",
            ));
            let (entered, observed) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            *state.update_admission_hold.lock().expect("expected gate") =
                Some(super::super::UpdateAdmissionHold {
                    entered,
                    release: released,
                });
            let task = tokio::spawn({
                let state = Arc::clone(&state);
                async move {
                    crate::client::on_session_update(&state, notification(serde_json::json!({
                    "sessionUpdate": "available_commands_update", "availableCommands": [{"name":"stale", "description":"stale"}]
                }))).await;
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(3), observed)
                .await
                .expect("expected update admission within 3s")
                .expect("expected gate observer");
            state.abandon_native_session();
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new("fresh"));
            release.send(()).expect("expected gate release");
            task.await.expect("expected handler completion");
            let snapshot = state.core_state.snapshot();
            let commands: Vec<_> = snapshot
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect();
            assert!(
                commands.is_empty(),
                "expected fresh commands: [] | received: {commands:?}"
            );
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn abandoned_load_update_cannot_restore_facts_when_fallback_reuses_id() {
            let (state, _) = state();
            let state = Arc::new(state);
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                "session-1",
            ));
            let (entered, observed) = tokio::sync::oneshot::channel();
            let (release, released) = std::sync::mpsc::channel();
            *state.update_admission_hold.lock().expect("expected gate") =
                Some(super::super::UpdateAdmissionHold {
                    entered,
                    release: released,
                });
            let task = tokio::spawn({
                let state = Arc::clone(&state);
                async move {
                    crate::client::on_session_update(&state, notification(serde_json::json!({
                    "sessionUpdate": "available_commands_update", "availableCommands": [{"name":"stale", "description":"stale"}]
                }))).await;
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(3), observed)
                .await
                .expect("expected update admission within 3s")
                .expect("expected gate observer");
            state.abandon_native_session();
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                "session-1",
            ));
            release.send(()).expect("expected gate release");
            task.await.expect("expected handler completion");
            let snapshot = state.core_state.snapshot();
            let commands: Vec<_> = snapshot
                .commands
                .iter()
                .map(|command| command.name.as_str())
                .collect();
            assert!(
                commands.is_empty(),
                "expected fresh commands: [] | received: {commands:?}"
            );
        }

        #[tokio::test]
        async fn opening_facts_respect_count_and_byte_budgets() {
            let host = mango_external_agents::HostContext::builder()
                .launcher(Arc::new(mango_external_agents::testing::FakeLauncher::new()))
                .cwd(std::env::temp_dir())
                .client_info("bounds-test", "1")
                .limits(mango_external_agents::Limits {
                    turn_channel_capacity: 1,
                    max_pending_requests: 1,
                    turn_buffer_bytes: 1024,
                    ..Default::default()
                })
                .build()
                .expect("expected host");
            let state = Arc::new(super::super::SessionState::new(
                super::SessionId::new("session-1"),
                &host,
                mango_external_agents::Configuration::default(),
                super::core_state(),
            ));
            for name in ["first", "second"] {
                crate::client::on_session_update(&state, notification(serde_json::json!({
                    "sessionUpdate": "available_commands_update", "availableCommands": [{"name": name, "description": "command"}]
                }))).await;
            }
            let binding = state.lock_native_session();
            let count = binding.pending.len();
            let bytes = binding.pending_bytes;
            assert_eq!(
                count, 1,
                "expected retained opening facts: 1 | received: {count}"
            );
            assert!(
                bytes <= 1024,
                "expected retained opening bytes <= 1024 | received: {bytes}"
            );
            drop(binding);
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                "session-1",
            ));
            let overflow = state.opening_overflow();
            assert!(
                matches!(
                    overflow,
                    Some(mango_external_agents::Error::LimitExceeded {
                        subject: "opening session facts",
                        limit: 1,
                        received: 2
                    })
                ),
                "expected opening fact limit: 1, received: 2 | received: {overflow:?}"
            );
        }

        #[tokio::test]
        async fn opening_facts_over_byte_budget_fail_opening() {
            let host = mango_external_agents::HostContext::builder()
                .launcher(Arc::new(mango_external_agents::testing::FakeLauncher::new()))
                .cwd(std::env::temp_dir())
                .client_info("byte-bound-test", "1")
                .limits(mango_external_agents::Limits {
                    turn_buffer_bytes: 1024,
                    ..Default::default()
                })
                .build()
                .expect("expected host");
            let state = Arc::new(super::super::SessionState::new(
                super::SessionId::new("session-1"),
                &host,
                mango_external_agents::Configuration::default(),
                super::core_state(),
            ));
            let frame = notification(
                serde_json::json!({"sessionUpdate":"available_commands_update",
                "availableCommands":[{"name":"large", "description":"x".repeat(1024)}]}),
            );
            let size = serde_json::to_vec(&frame)
                .expect("expected encodable frame")
                .len();
            crate::client::on_session_update(&state, frame).await;
            assert!(
                state.lock_native_session().pending.is_empty(),
                "expected no retained oversized fact"
            );
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                "session-1",
            ));
            let overflow = state.opening_overflow();
            assert!(
                matches!(overflow, Some(mango_external_agents::Error::LimitExceeded { subject: "bytes of opening session facts", limit: 1024, received }) if received == size),
                "expected opening byte limit: 1024, received: {size} | received: {overflow:?}"
            );
        }

        /// `session/load` replays a whole history while no turn exists. Those frames build events that
        /// are dropped, so they must not also leave a call open in the reducer until the next turn.
        #[tokio::test]
        async fn replayed_tool_calls_leave_no_open_call_when_no_turn_exists() {
            const REPLAYED: usize = 50;
            let (state, _) = state();
            let state = Arc::new(state);
            for index in 0..REPLAYED {
                crate::client::on_session_update(&state, running_call(index)).await;
            }
            let open = state.open_call_count();
            assert_eq!(
                open, 0,
                "expected open calls after turnless replay: 0 | received: {open}"
            );
        }

        /// Skipping the transcript must not skip what the replay says about the session itself.
        #[tokio::test]
        async fn replayed_session_facts_are_still_applied_when_no_turn_exists() {
            let (state, _) = state();
            let state = Arc::new(state);
            state.bind_native_session(agent_client_protocol_schema::v1::SessionId::new(
                "session-1",
            ));
            crate::client::on_session_update(&state, running_call(0)).await;
            crate::client::on_session_update(
                &state,
                notification(serde_json::json!({
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": [{ "name": "review", "description": "Review the diff" }],
                })),
            )
            .await;
            crate::client::on_session_update(
                &state,
                notification(serde_json::json!({
                    "sessionUpdate": "config_option_update",
                    "configOptions": [{
                        "id": "model",
                        "name": "Model",
                        "type": "select",
                        "currentValue": "fast",
                        "options": [
                            { "value": "fast", "name": "Fast" },
                            { "value": "deep", "name": "Deep" }
                        ],
                    }],
                })),
            )
            .await;
            let snapshot = state.core_state.snapshot();
            let commands: Vec<&str> = snapshot.commands.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                commands,
                ["review"],
                "expected replayed commands: [review] | received: {commands:?}"
            );
            let options = snapshot.catalog.options().len();
            assert_eq!(
                options, 1,
                "expected replayed configuration options: 1 | received: {options}"
            );
        }

        /// The turn events one session emits for a repeat of call 0 after `replayed` turnless frames.
        async fn events_after_replay(replayed: usize) -> Vec<String> {
            use mango_external_agents::event::EventKind;

            let (state, host) = state();
            let state = Arc::new(state);
            for index in 0..replayed {
                crate::client::on_session_update(&state, running_call(index)).await;
            }
            let (sink, mut events) = EventSink::new(
                SessionId::new("session-1"),
                TurnId::new("turn-1"),
                AttemptId::default(),
                Arc::clone(host.clock()),
                16,
            );
            let turn = state
                .begin_turn(sink, None)
                .expect("expected the turn to be admitted");
            crate::client::on_session_update(&state, running_call(0)).await;
            let closing = state.prepare_terminal_matching(&turn);
            let closed = closing.map_or(0, |(_, _, mut reducer)| reducer.finish().len());
            let mut kinds = Vec::new();
            while let Ok(event) = events.try_recv() {
                kinds.push(match event.kind {
                    EventKind::ActivityStarted { .. } => String::from("started"),
                    other => format!("{other:?}"),
                });
            }
            kinds.push(format!("owed-closings:{closed}"));
            kinds
        }

        /// A turn that starts after a replay reduces its own frames exactly as it would have on a
        /// session that never replayed anything: the replay's calls are neither open nor finished in it.
        #[tokio::test]
        async fn a_turn_after_replay_reduces_as_on_a_fresh_session() {
            let fresh = events_after_replay(0).await;
            let replayed = events_after_replay(50).await;
            assert_eq!(
                fresh,
                ["started", "owed-closings:1"],
                "expected a fresh turn to start one call and owe its close | received: {fresh:?}"
            );
            assert_eq!(
                replayed, fresh,
                "expected a turn after replay to match a fresh session: {fresh:?} | received: {replayed:?}"
            );
        }
    }

    /// An event the core refuses to publish reaches the host as the turn's failure, not as silence.
    mod refused {
        use std::sync::Arc;

        use mango_external_agents::{
            AttemptId, EventKind, EventReceiver, EventSink, TerminalStatus, TurnId,
        };

        use super::replay::{notification, running_call};
        use super::{SessionId, SessionState, state};

        fn call_named(id: &str) -> agent_client_protocol_schema::v1::SessionNotification {
            notification(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": "Run tests",
                "kind": "execute",
                "status": "in_progress",
            }))
        }

        /// A state with a turn running on it, and the receiver the host reads that turn from.
        fn running_turn() -> (Arc<SessionState>, EventSink, EventReceiver) {
            let (state, host) = state();
            let state = Arc::new(state);
            let (sink, events) = EventSink::new(
                SessionId::new("session-1"),
                TurnId::new("turn-1"),
                AttemptId::default(),
                Arc::clone(host.clock()),
                16,
            );
            state
                .begin_turn(sink.clone(), None)
                .expect("expected the turn to be admitted");
            (state, sink, events)
        }

        /// The failure code the turn ended with, or what it is doing instead.
        fn outcome(events: &EventReceiver) -> String {
            match events.terminal_status() {
                Some(TerminalStatus::Failed { code }) => code.as_str().to_owned(),
                Some(other) => format!("{other:?}"),
                None => String::from("still running"),
            }
        }

        #[tokio::test]
        async fn a_call_id_the_core_refuses_fails_the_turn_with_a_named_error() {
            let (state, _sink, mut events) = running_turn();
            crate::client::on_session_update(&state, call_named(&"c".repeat(129))).await;
            let outcome = outcome(&events);
            assert_eq!(
                outcome, "acp-refused-event",
                "expected the turn to fail with acp-refused-event | received: {outcome}"
            );
            let mut message = None;
            while let Ok(event) = events.try_recv() {
                if let EventKind::Error { error } = event.kind {
                    message = Some(error.message);
                }
            }
            let message = message.unwrap_or_default();
            assert!(
                message.contains("activity call id"),
                "expected the failure to name the refused activity call id | received: {message:?}"
            );
        }

        /// What the turn still owes when a refused event ends it is settled before the terminal, so
        /// the host's transcript never ends with a call running, a plan open or a thought unended.
        #[tokio::test]
        async fn a_refusal_closes_what_the_turn_left_running_before_the_terminal() {
            use mango_external_agents::ActivityStatus;

            let (state, _sink, mut events) = running_turn();
            for frame in [
                running_call(0),
                notification(serde_json::json!({
                    "sessionUpdate": "plan",
                    "entries": [{ "content": "build", "priority": "high", "status": "pending" }]
                })),
                notification(serde_json::json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "thinking" }
                })),
                call_named(&"c".repeat(129)),
            ] {
                crate::client::on_session_update(&state, frame).await;
            }
            let mut sequence = Vec::new();
            while let Ok(event) = events.try_recv() {
                sequence.push(match event.kind {
                    EventKind::ActivityStarted { call_id, .. } => format!("started {call_id}"),
                    EventKind::ActivityCompleted { call_id, result } => {
                        format!("completed {call_id} {:?}", result.status)
                    }
                    EventKind::ReasoningEnded => String::from("reasoning ended"),
                    EventKind::Error { error } => format!("error {}", error.code.as_str()),
                    _ => continue,
                });
            }
            let failed = format!("{:?}", ActivityStatus::Failed);
            let mut closed = vec![
                format!("completed call-0 {failed}"),
                format!("completed acp:plan {:?}", ActivityStatus::Completed),
                String::from("reasoning ended"),
            ];
            let (last, before) = sequence.split_last().expect("expected events");
            let mut owed: Vec<String> = before
                .iter()
                .filter(|entry| !entry.starts_with("started"))
                .cloned()
                .collect();
            owed.sort();
            closed.sort();
            assert!(
                last == "error acp-refused-event" && owed == closed,
                "expected the running call, plan and thought closed before acp-refused-event | received {sequence:?}"
            );
        }

        #[tokio::test]
        async fn a_blank_call_id_fails_the_turn_too() {
            let (state, _sink, events) = running_turn();
            crate::client::on_session_update(&state, call_named("   ")).await;
            let outcome = outcome(&events);
            assert_eq!(
                outcome, "acp-refused-event",
                "expected the turn to fail with acp-refused-event | received: {outcome}"
            );
        }

        /// Frames an ordinary agent sends do not fail a turn, including an id at the limit.
        #[tokio::test]
        async fn ordinary_and_at_the_limit_call_ids_leave_the_turn_running() {
            let (state, _sink, events) = running_turn();
            crate::client::on_session_update(&state, running_call(0)).await;
            crate::client::on_session_update(&state, call_named(&"c".repeat(128))).await;
            let outcome = outcome(&events);
            assert_eq!(
                outcome, "still running",
                "expected the turn still running | received: {outcome}"
            );
        }

        /// Only a value the core cannot make safe is reported; a closed stream or an overflow is not.
        #[tokio::test]
        async fn only_an_invalid_vendor_value_is_reported() {
            let (state, _sink, events) = running_turn();
            let turn = state.turn().expect("expected the turn to be running");
            for error in [
                mango_external_agents::Error::Closed {
                    subject: "turn stream",
                },
                mango_external_agents::Error::LimitExceeded {
                    subject: "queued turn events",
                    limit: 1,
                    received: 2,
                },
            ] {
                crate::client::report_refused_event(&state, &turn, &error).await;
                let outcome = outcome(&events);
                assert_eq!(
                    outcome, "still running",
                    "expected {error:?} not to fail the turn | received: {outcome}"
                );
            }
            let invalid = mango_external_agents::Error::InvalidVendorValue {
                field: "activity call id",
                received: String::from("x"),
            };
            crate::client::report_refused_event(&state, &turn, &invalid).await;
            let outcome = outcome(&events);
            assert_eq!(
                outcome, "acp-refused-event",
                "expected {invalid:?} to fail the turn | received: {outcome}"
            );
        }

        /// A turn that already ended keeps its outcome: a late refused frame is the ordinary race.
        #[tokio::test]
        async fn a_refused_frame_after_the_terminal_does_not_replace_it() {
            let (state, sink, events) = running_turn();
            sink.complete()
                .await
                .expect("expected the turn to complete");
            crate::client::on_session_update(&state, call_named(&"c".repeat(129))).await;
            let outcome = outcome(&events);
            assert_eq!(
                outcome, "Completed",
                "expected the terminal to stay Completed | received: {outcome}"
            );
        }
    }

    /// A frame read for a turn that has since ended must not reach the reducer of the turn that
    /// replaced it: the read of the owner and the reduction are two steps, and a turn can end and
    /// another begin between them.
    mod stale {
        use std::sync::Arc;

        use mango_external_agents::{AttemptId, EventSink, TurnId};

        use super::replay::{notification, running_call};
        use super::{SessionId, SessionState, state};
        use crate::client::TurnHandle;

        fn sink(host: &mango_external_agents::HostContext, turn_id: &str) -> EventSink {
            EventSink::new(
                SessionId::new("session-1"),
                TurnId::new(turn_id),
                AttemptId::default(),
                Arc::clone(host.clock()),
                16,
            )
            .0
        }

        /// A state whose first turn has ended and a second one begun, with the first turn's handle
        /// as a handler that had already read it would still hold it.
        fn second_turn_after_first() -> (Arc<SessionState>, TurnHandle) {
            let (state, host) = state();
            let state = Arc::new(state);
            let first = state
                .begin_turn(sink(&host, "turn-1"), None)
                .expect("expected the first turn to be admitted");
            assert!(first.finish(), "expected the first turn's terminal claim");
            let _ = state.prepare_terminal_matching(&first);
            state.release_turn_matching(&first);
            state
                .begin_turn(sink(&host, "turn-2"), None)
                .expect("expected the second turn to be admitted");
            (state, first)
        }

        #[test]
        fn a_frame_for_an_ended_turn_leaves_no_call_in_the_next_turns_reducer() {
            let (state, first) = second_turn_after_first();
            let _ = state.reduce(&first, running_call(0));
            let open = state.open_call_count();
            assert_eq!(
                open, 0,
                "expected open calls in the next turn after a stale frame: 0 | received: {open}"
            );
            let second = state
                .turn()
                .expect("expected the second turn to be running");
            let owed = state
                .prepare_terminal_matching(&second)
                .map(|(_, _, mut reducer)| reducer.finish());
            assert_eq!(
                owed,
                Some(Vec::new()),
                "expected the next turn to owe no call closing | received: {owed:?}"
            );
        }

        /// What the agent said about the session itself is still true whichever turn it was read for.
        #[test]
        fn a_frame_for_an_ended_turn_still_yields_its_session_facts() {
            let (state, first) = second_turn_after_first();
            let (events, facts) = state.reduce(
                &first,
                notification(serde_json::json!({
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": [{ "name": "review", "description": "Review" }],
                })),
            );
            assert!(
                events.is_empty() && facts.len() == 1,
                "expected no events and one session fact | received: {events:?} and {facts:?}"
            );
        }

        /// The turn that owns the slot reduces its own frames as before.
        #[test]
        fn a_frame_for_the_running_turn_is_reduced() {
            let (state, _first) = second_turn_after_first();
            let current = state
                .turn()
                .expect("expected the second turn to be running");
            let (events, _) = state.reduce(&current, running_call(0));
            let open = state.open_call_count();
            assert!(
                events.len() == 1 && open == 1,
                "expected one event and one open call | received: {events:?} and {open}"
            );
        }
    }
}
