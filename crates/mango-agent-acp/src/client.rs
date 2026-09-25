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
//! # Why handlers leave the dispatch loop promptly
//!
//! The loop runs one handler to completion before the next message. [`EventSink::emit`] therefore
//! only reserves bounded transcript capacity and returns; an overflow owns a terminal outcome
//! instead of parking ACP dispatch behind a slow host. Permission callbacks likewise park their
//! responder and move broker deliberation to a bounded task, because an answer may arrive through
//! [`Session::respond`](mango_external_agents::Session::respond) on another task.
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
use mango_external_agents::configuration::{
    Configuration, ConfigurationCatalog, ConfigurationState,
};
use mango_external_agents::event::{EventKind, SessionId};
use mango_external_agents::permission::{
    ApprovalDecision, DecisionSource, PermissionBroker, PermissionLevel, PermissionResponse,
    broker_response,
};
use mango_external_agents::session::CancelReason;
use mango_external_agents::{
    Clock, Error, EventSink, HostContext, Limits, ProcessCleanupGuard, ProcessControl, Result,
    VendorError, process::stop_process_with_limits,
};

use crate::approval_events::ApprovalEvents;
use crate::error::vendor_error;
use crate::permission;
use crate::reducer::{Reducer, SessionFact};
use crate::transport::LaunchedAgent;

#[cfg(test)]
struct DriveStartupHold {
    control: Arc<dyn ProcessControl>,
    release: tokio::sync::oneshot::Receiver<()>,
    reached: tokio::sync::oneshot::Sender<()>,
}

#[cfg(test)]
static DRIVE_STARTUP_HOLD: Mutex<Option<DriveStartupHold>> = Mutex::new(None);

/// Holds the next driver before it creates the connection task.
///
/// The unit test uses this to cancel `drive` in the window where a launched child has no
/// `ConnectionHandle` yet.
#[cfg(test)]
fn hold_next_drive_startup(
    control: Arc<dyn ProcessControl>,
) -> (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (release, release_rx) = tokio::sync::oneshot::channel();
    let (reached_tx, reached) = tokio::sync::oneshot::channel();
    *DRIVE_STARTUP_HOLD
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(DriveStartupHold {
        control,
        release: release_rx,
        reached: reached_tx,
    });
    (release, reached)
}

#[cfg(test)]
async fn wait_for_drive_startup_hold(child: &DriveShutdownGuard) {
    let hold = {
        let mut pending = DRIVE_STARTUP_HOLD
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if pending
            .as_ref()
            .is_some_and(|hold| Arc::ptr_eq(&hold.control, child.control()))
        {
            pending.take()
        } else {
            None
        }
    };
    let Some(DriveStartupHold {
        control: _,
        release,
        reached,
    }) = hold
    else {
        return;
    };
    let _ = reached.send(());
    let _ = release.await;
}

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
    /// Set by whoever emitted this turn's terminal.
    ///
    /// A handler holding a clone can be parked mid-frame while `close` emits the terminal on another
    /// task; resuming afterwards would put the rest of that frame's events *after* the terminal,
    /// which the core's conformance suite refuses. Checked before every emit.
    finished: Arc<AtomicBool>,
    /// Wakes the prompt owner when native cancellation starts.
    pub(crate) cancellation: mango_external_agents::CancelToken,
    /// A notification error belongs to this generation, never a later prompt.
    pub(crate) cancel_failure: Arc<Mutex<Option<String>>>,
    pub(crate) approvals: Arc<ApprovalEvents>,
}

/// Settles the questions a cancellation owes, however its own answer turns out.
///
/// The expiry path answers one question and notifies the agent, and either call can fail on a
/// connection the peer has already dropped. Both are `?`, so the withdrawal cannot be the
/// statement after them: the one case that needs it most is the one that never reaches it. As a
/// guard it runs on the early return too, and the obligation survives a later edit that moves the
/// lines around. There is no test behind this, and there cannot be one yet — the SDK only builds a
/// `Responder` inside a live connection, so the failure it guards against needs a dead one.
struct WithdrawOnDrop<'a>(&'a SessionState);

impl Drop for WithdrawOnDrop<'_> {
    fn drop(&mut self) {
        self.0.withdraw_pending();
    }
}

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
        if self.announced {
            self.turn.approvals.push(EventKind::ApprovalResolved {
                interaction_id: self.question.id().clone(),
                decision: ApprovalDecision::unresolved(
                    WITHDRAWN_OPTION_ID,
                    DecisionSource::Cancelled,
                ),
            });
        }
        let _ = self.responder.respond(permission::cancelled());
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
    fn respond_with_resolution(
        self,
        response: RequestPermissionResponse,
        option_id: String,
        source: DecisionSource,
    ) -> agent_client_protocol::Result<()> {
        let decision = self
            .question
            .options
            .iter()
            .find(|option| option.id == option_id)
            .map(|option| ApprovalDecision::from_option(option, source))
            .unwrap_or_else(|| ApprovalDecision::unresolved(option_id, source));
        let event = EventKind::ApprovalResolved {
            interaction_id: self.question.id().clone(),
            decision,
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
    /// Bumped on every frame the agent sends and every change to the pending questions, so the
    /// idle deadline restarts on progress and re-reads whether a question is still open.
    activity: tokio::sync::watch::Sender<u64>,
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
            configuration: Mutex::new(configuration),
            catalog_revision: Mutex::new(0),
            turn_start: Mutex::new(()),
            generations: AtomicU64::new(0),
            cancel_reason: Mutex::new(None),
            reducer: Mutex::new(Reducer::new()),
            pending: Mutex::new(HashMap::new()),
            next_approval: AtomicU64::new(0),
            activity: tokio::sync::watch::channel(0).0,
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
                    if self.pending_count() == 0 {
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
            cancellation: mango_external_agents::CancelToken::new(),
            cancel_failure: Arc::new(Mutex::new(None)),
            approvals: Arc::new(ApprovalEvents::default()),
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
        drop(cancelling);
        turn.cancellation.cancel();
        for pending in pending {
            pending.withdraw();
        }
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

    /// Ends this turn, and only this turn.
    ///
    /// What a `session/prompt` task calls. Its own turn may already have been ended by a `close`, and
    /// a *later* turn may have started in the meantime — so an unconditional take would let a task
    /// that answered late end, and emit the terminal of, a conversation that is not its own.
    ///
    /// Hands back the turn's own reducer rather than its closing events: what the calls the agent
    /// left running end as depends on the terminal, which the caller decides after this returns.
    pub(crate) fn prepare_terminal_matching(
        &self,
        handle: &TurnHandle,
    ) -> Option<(TurnHandle, Option<CancelReason>, Reducer)> {
        let (turn, reason, pending, closing) = {
            let active = self.lock_turn();
            if active.as_ref()?.generation != handle.generation {
                return None;
            }
            let turn = active.clone()?;
            // These are all owned by the current turn. Capture them before releasing the slot: a
            // newly started prompt clears the reducer and may park approvals of its own.
            let reason = self.lock_cancel_reason().take();
            let pending = self.take_pending();
            let closing = std::mem::take(&mut *self.lock_reducer());
            (turn, reason, pending, closing)
        };
        for pending in pending {
            pending.withdraw();
        }
        Some((turn, reason, closing))
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

    /// The events and session facts one frame produces, computed under the guard because the
    /// reducer is pure.
    fn reduce(&self, notification: SessionNotification) -> (Vec<EventKind>, Vec<SessionFact>) {
        // Tokio's clock rather than the host's: this instant only spaces a running call's updates,
        // and a runtime with paused time has to see that spacing move with it.
        self.lock_reducer()
            .update_at(notification.update, tokio::time::Instant::now().into_std())
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
    /// would be told "allow" about a turn it stopped running. Dropping the responders instead would
    /// say nothing at all — the SDK's drop guard only answers batch requests — so each is answered
    /// `Cancelled`, which is ACP's own outcome for a question that was withdrawn rather than refused.
    ///
    /// Taken out from under the lock in one statement, so nothing is held while the answers go out.
    pub(crate) fn withdraw_pending(&self) {
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
        let mut parked = self.lock_pending();
        if parked.len() >= self.limits.max_pending_requests {
            drop(parked);
            pending.responder.respond(permission::cancelled())?;
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
        self.touch();
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
            // Structural rather than ordered: both calls below leave through `?`, and a question
            // stranded in the map holds a responder the agent is still waiting on and a slot
            // against `max_pending_requests` for the rest of the session. A guard settles the
            // debt on every exit, so no later rearrangement of these two lines can strand it.
            let _debts = WithdrawOnDrop(self);
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
    ///
    /// Sibling of [`SessionState::withdraw_pending`]; see `WithdrawOnDrop` for why the sweeping
    /// form is reached from a guard rather than from a statement.
    pub(crate) fn withdraw_pending_by_id(&self, id: &str) -> agent_client_protocol::Result<()> {
        let pending = self.lock_pending().remove(id);
        match pending {
            Some(pending) => {
                self.touch();
                pending.responder.respond(permission::cancelled())
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

/// One `session/update` notification, reduced and emitted.
///
/// Never fails the handler. A closed or overflowed sink wakes the prompt owner, which cancels native
/// work before it releases this generation; freeing the slot here would let a second prompt onto a
/// wire that has no way to tell two turns apart. The nonblocking sink makes later frames fail fast.
async fn on_session_update(state: &Arc<SessionState>, notification: SessionNotification) {
    // Capture the current owner before reducing session facts. Facts may arrive between turns;
    // their publication must not attach turn events to a newly admitted generation.
    let turn = state.turn();
    state.touch();
    let (events, facts) = state.reduce(notification);
    // Published before the turn events: a session fact is true the moment the agent announced it,
    // not only once a host has read every event that preceded it on this turn's own stream.
    for fact in facts {
        state.apply_fact(fact);
    }
    let Some(turn) = turn else {
        return;
    };
    for kind in events {
        // Re-checked each time round: close can commit the terminal after reduction, and no later
        // event may follow it.
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
    let question = permission::request_from(
        &request,
        id.clone(),
        state.session_id.clone(),
        turn.sink.operation(),
        expires_at,
    );
    let question = match question.normalized() {
        Ok(question) => question,
        // A question nobody could render — no options, or an id that cannot survive bounding — is
        // withdrawn on its own rather than ending the turn it belongs to.
        Err(_) => return responder.respond(permission::cancelled()),
    };

    let Some(deadline) = ApprovalDeadline::new(question.expires_at(), now) else {
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

    resolve_pending(Arc::clone(state), turn, id, question, deadline);
    Ok(())
}

/// Resolves one parked permission away from ACP's serialized dispatch loop.
///
/// The pending-map admission cap limits these tasks. `ApprovalDeadline::run` bounds policy work,
/// so a broker that never decides cannot keep an orphan task after the request expires.
fn resolve_pending(
    state: Arc<SessionState>,
    turn: TurnHandle,
    id: String,
    question: mango_external_agents::PermissionRequest,
    deadline: ApprovalDeadline,
) {
    tokio::spawn(async move {
        let decided = match turn.level {
            Some(PermissionLevel::ReadOnly) => standing_refusal(&question),
            Some(PermissionLevel::Default | PermissionLevel::FullAccess) | None => deadline
                .run(broker_response(state.broker.as_ref(), &question))
                .await
                .flatten(),
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

/// A live connection to one agent, and the child behind it.
pub(crate) struct ConnectionHandle {
    connection: ConnectionTo<Agent>,
    control: Arc<dyn ProcessControl>,
    /// The one owner shared with lifecycle watchers that may also need to reap the child.
    child_reaper: ChildReaper,
    shutdown: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    driver: Mutex<Option<tokio::task::JoinHandle<agent_client_protocol::Result<()>>>>,
    /// Host-owned bounds for tearing down the dispatch loop and child process.
    limits: mango_external_agents::Limits,
    /// Fires once the dispatch loop is over, whichever way it ended.
    ///
    /// Separate from `driver` because both are needed at once: `shutdown` takes the join handle to
    /// bound its own wind-down, and the session's lifecycle watcher has to observe the same event
    /// without competing for it.
    driver_done: mango_external_agents::CancelToken,
    /// Filled by the bounded transport when a queue budget failed the connection.
    overflow: crate::transport::OverflowSlot,
    shutdown_started: AtomicBool,
    shutdown_complete: mango_external_agents::CancelToken,
    /// The one teardown outcome every close waiter observes. `Error` is intentionally reduced to
    /// its diagnostic-safe summary here because the core error is not cloneable and a second close
    /// must still report the first cleanup failure rather than inventing success.
    shutdown_result: Mutex<Option<std::result::Result<(), String>>>,
    /// The runtime the connection was driven on. Shutdown is spawned through it, so a request
    /// or scope guard dropped on a thread without a runtime still ends the child.
    runtime: tokio::runtime::Handle,
    /// How many shutdown tasks `begin_shutdown` started; the single-flight guard keeps it at one.
    #[cfg(test)]
    shutdown_runs: std::sync::atomic::AtomicUsize,
    /// Bounds non-turn ACP requests before they enter the SDK's unbounded pending-request queue.
    /// A prompt has its own single-turn admission in `SessionState` and does not use this permit.
    requests: RequestAdmission,
}

/// Admission in front of ACP's unbounded SDK request queue.
struct RequestAdmission {
    permits: tokio::sync::Semaphore,
    limit: usize,
    state: Mutex<RequestAdmissionState>,
}

#[derive(Default)]
struct RequestAdmissionState {
    closed: bool,
}

impl RequestAdmission {
    fn new(limit: usize) -> Self {
        Self {
            permits: tokio::sync::Semaphore::new(limit),
            limit,
            state: Mutex::new(RequestAdmissionState::default()),
        }
    }

    fn submit<T>(&self, send: impl FnOnce() -> T) -> Result<(tokio::sync::SemaphorePermit<'_>, T)> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.closed {
            return Err(Error::Closed {
                subject: "ACP connection",
            });
        }
        let permit = self
            .permits
            .try_acquire()
            .map_err(|_| Error::LimitExceeded {
                subject: "outstanding ACP requests",
                limit: self.limit,
                received: self.limit.saturating_add(1),
            })?;
        let sent = send();
        drop(state);
        Ok((permit, sent))
    }

    fn close(&self) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
    }
}

/// Starts owned teardown if a generic request leaves ACP's pending-reply map without a response.
struct RequestAbandonment {
    connection: Arc<ConnectionHandle>,
    armed: bool,
}

impl RequestAbandonment {
    fn new(connection: Arc<ConnectionHandle>) -> Self {
        Self {
            connection,
            armed: true,
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
        // ACP 2.1 only sends a cancellation notification when a sent request is dropped; it keeps
        // that request's reply slot until a response or EOF. Close admission synchronously, then
        // let the connection's owned shutdown task force EOF and release the SDK's pending map.
        self.connection
            .begin_shutdown(mango_external_agents::CancelReason::Shutdown);
    }
}

/// Reaps one child once without letting cancellation abandon its launched kill task.
#[derive(Clone)]
pub(crate) struct ChildReaper {
    control: Arc<dyn ProcessControl>,
    claimed: Arc<AtomicBool>,
    limits: Limits,
    result: Arc<Mutex<Option<std::result::Result<(), String>>>>,
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
                let outcome = stop_process_with_limits(control.as_ref(), reason, &limits).await;
                *result.lock().unwrap_or_else(PoisonError::into_inner) =
                    Some(outcome.map(|_| ()).map_err(|error| error.to_string()));
            });
        }
        self.done.cancelled().await;
        self.result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| Err(String::from("ACP child cleanup ended without an outcome")))
            .map_err(|message| Error::CleanupRequired {
                control: Arc::clone(&self.control),
                source: Box::new(Error::Vendor(link_failure(message))),
            })
    }
}

/// Signals every competing reaper even if an injected process control panics.
struct ReapCompletion(mango_external_agents::CancelToken);

impl Drop for ReapCompletion {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// Reaps a child if cancellation drops `drive` before it can return a connection handle.
pub(crate) struct DriveShutdownGuard {
    control: Arc<dyn ProcessControl>,
    cleanup: Option<ProcessCleanupGuard>,
}

impl DriveShutdownGuard {
    /// Claims a launched child before the asynchronous connection driver is first polled.
    pub(crate) fn from_launched(launched: &LaunchedAgent, limits: Limits) -> Self {
        let control = Arc::clone(&launched.control);
        Self {
            cleanup: Some(ProcessCleanupGuard::new(
                Arc::clone(&control),
                limits,
                mango_external_agents::CancelReason::Shutdown,
            )),
            control,
        }
    }

    fn disarm(&mut self) -> Arc<dyn ProcessControl> {
        self.cleanup
            .take()
            .expect("drive guard owns the child until a connection handle exists")
            .into_control()
    }

    /// Completes pre-handle cleanup before `drive` reports a connection-start failure.
    async fn finish(mut self) -> Result<()> {
        self.cleanup
            .take()
            .expect("drive guard owns the child until cleanup completes")
            .finish()
            .await
            .map(|_| ())
    }

    #[cfg(test)]
    fn control(&self) -> &Arc<dyn ProcessControl> {
        &self.control
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

/// Starts the dispatch loop for one agent and hands back the connection it opened.
///
/// # Errors
///
/// [`Error::Vendor`] when the connection ended before it produced a handle, under
/// `acp-link-closed`, carrying which of the three shapes it was and the child's redacted stderr
/// tail — which is what an agent that printed a usage message and exited looks like from here.
///
/// A [`VendorError`] rather than an [`Error::Link`], because the tail has to reach a host and this
/// is the only path where it cannot reach one any other way: no `Session` and no
/// [`ProcessControl`] are returned, so [`StderrTail`](mango_external_agents::StderrTail) is out of
/// reach. `VendorError::message` is the field that exists for vendor text a host reads and
/// `Display` never writes, which is exactly the shape this needs; `Error::Link`'s summary is
/// written verbatim and could not carry it.
///
/// The one exception is a transport budget: when the agent overflows a message or byte budget
/// before the connection opens, this returns the recorded [`Error::LimitExceeded`], naming the
/// limit and the received value, instead of `acp-link-closed`.
pub(crate) async fn drive(
    launched: LaunchedAgent,
    state: Arc<SessionState>,
    client_name: String,
    mut child: DriveShutdownGuard,
) -> Result<ConnectionHandle> {
    #[cfg(test)]
    wait_for_drive_startup_hold(&child).await;

    let LaunchedAgent { transport, .. } = launched;
    let overflow = transport.overflow();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let notifications = Arc::clone(&state);
    let approvals = Arc::clone(&state);
    let driver_done = mango_external_agents::CancelToken::new();
    let loop_over = driver_done.clone();
    let connecting = Client
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
        // All supported handlers are installed before connecting. The SDK's v1 default would
        // retain unknown session messages for a future dynamic handler, without a queue cap.
        .on_receive_dispatch(
            async |message: agent_client_protocol::Dispatch, _cx| match message {
                agent_client_protocol::Dispatch::Request(_, responder) => {
                    responder.respond_with_error(agent_client_protocol::Error::method_not_found())
                }
                agent_client_protocol::Dispatch::Notification(_) => Ok(()),
                agent_client_protocol::Dispatch::Response(result, router) => {
                    router.route_with_result(result)
                }
            },
            agent_client_protocol::on_receive_dispatch!(),
        )
        .connect_with(transport, async move |connection: ConnectionTo<Agent>| {
            // The closure *is* the connection's lifetime, so it hands a clone out and parks.
            // Returning here shuts the dispatch loop down, which is why only `shutdown` does.
            let _ = ready_tx.send(connection.clone());
            let _ = shutdown_rx.await;
            Ok(())
        });
    let driver = tokio::spawn(async move {
        let outcome = connecting.await;
        // Signalled whatever the outcome. An agent that exited or a transport that failed ends the
        // loop without anyone having called `close`, and that is precisely the case a session's
        // lifecycle would otherwise never hear about.
        loop_over.cancel();
        outcome
    });

    let Ok(connection) = ready_rx.await else {
        // The closure never ran, so the transport failed first. Nothing is returned from this
        // path — no session, no control handle — so the child's stderr has nowhere else to go, and
        // an agent that exited printing a usage message has said the only useful thing there is.
        // `VendorError::message` is the field for exactly that: vendor text a host reads and
        // `Display` never writes.
        let shape = match driver.await {
            Ok(Err(_)) => "a transport that failed before the connection opened",
            Ok(Ok(())) => "a connection that closed before it opened",
            Err(_) => "a connection task that did not finish",
        };
        let error = match overflow.get() {
            Some(overflow) => overflow.error(),
            None => Error::Vendor(link_failure(with_stderr(shape, child.control.as_ref()))),
        };
        child.finish().await?;
        return Err(error);
    };

    let control = child.disarm();
    let child_reaper = ChildReaper::new(Arc::clone(&control), state.limits);
    Ok(ConnectionHandle {
        connection,
        control,
        child_reaper,
        shutdown: Mutex::new(Some(shutdown_tx)),
        driver: Mutex::new(Some(driver)),
        limits: state.limits,
        driver_done,
        overflow,
        shutdown_started: AtomicBool::new(false),
        shutdown_complete: mango_external_agents::CancelToken::new(),
        shutdown_result: Mutex::new(None),
        // `drive` is async and has already spawned the dispatch loop, so a runtime is current.
        runtime: tokio::runtime::Handle::current(),
        #[cfg(test)]
        shutdown_runs: std::sync::atomic::AtomicUsize::new(0),
        requests: RequestAdmission::new(state.limits.max_pending_requests),
    })
}

impl ConnectionHandle {
    /// Starts bounded shutdown in an owned task so dropping the initiating session future cannot
    /// abandon the child. Later callers join the same completion signal.
    pub(crate) fn begin_shutdown(self: &Arc<Self>, reason: mango_external_agents::CancelReason) {
        // Serialised with `RequestAdmission::submit`, so no request can enter the SDK queue after
        // a caller has begun teardown.
        self.requests.close();
        if self.shutdown_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let connection = Arc::clone(self);
        #[cfg(test)]
        self.shutdown_runs.fetch_add(1, Ordering::AcqRel);
        self.runtime.spawn(async move {
            let result = connection
                .shutdown(reason)
                .await
                .map_err(|error| error.to_string());
            *connection
                .shutdown_result
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(result);
            connection.shutdown_complete.cancel();
        });
    }

    /// Waits for the shutdown task started by [`Self::begin_shutdown`].
    pub(crate) async fn wait_shutdown(&self) -> Result<()> {
        self.shutdown_complete.cancelled().await;
        let result = self
            .shutdown_result
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| {
                Err(String::from(
                    "the ACP shutdown task ended without an outcome",
                ))
            });
        result.map_err(|message| Error::CleanupRequired {
            control: Arc::clone(&self.child_reaper.control),
            source: Box::new(Error::Vendor(link_failure(format!(
                "ACP connection cleanup failed: {message}"
            )))),
        })
    }
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

    /// The shared owner a watcher can use without retaining a connection clone.
    pub(crate) fn child_reaper(&self) -> ChildReaper {
        self.child_reaper.clone()
    }

    /// Fires once the dispatch loop is over, whichever way it ended.
    ///
    /// Not the same event as [`ConnectionTo::incoming_closed`]: a clean EOF closes the incoming
    /// half while the loop stays up, because its outgoing and task actors live for as long as any
    /// [`ConnectionTo`] clone does — and this handle holds one. A transport that *failed* ends the
    /// loop itself, and only this reports that. Anything watching for a dead agent wants both.
    pub(crate) fn driver_done(&self) -> &mango_external_agents::CancelToken {
        &self.driver_done
    }

    /// The transport budget that failed this connection, if one did.
    ///
    /// Set before the SDK reports the closed connection, so a request or turn that observed the
    /// failure can read it. For example, a prompt ended by an oversized frame reports this instead
    /// of a cancellation.
    pub(crate) fn overflow(&self) -> Option<crate::transport::Overflow> {
        self.overflow.get().copied()
    }

    /// Ends the dispatch loop and the child. Idempotent.
    ///
    /// Closing the connection first is what lets a well-behaved agent see its stdin end and exit on
    /// its own; the kill is the escalation for one that does not, and it is the launcher's own —
    /// this only asks, with the reason.
    pub(crate) async fn shutdown(&self, reason: mango_external_agents::CancelReason) -> Result<()> {
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
        if let Some(mut driver) = driver {
            match tokio::time::timeout(self.limits.shutdown_timeout, &mut driver).await {
                Ok(Ok(Ok(()))) => {}
                // The transport commonly reports its own close as an error. The driver has still
                // joined, and process cleanup is the fact that decides whether teardown succeeded.
                Ok(Ok(Err(_)) | Err(_)) => {}
                Err(_) => {
                    driver.abort();
                    let _ = driver.await;
                    // The task has now joined. A successful bounded process cleanup below proves
                    // the session is no longer live, so reporting an incomplete close would leave
                    // the host permanently at `Closing` despite a reaped child.
                }
            }
        }
        self.child_reaper.reap(reason).await
    }
}

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
/// [`Error::Timeout`] when the deadline passed, [`Error::Vendor`] with the child's redacted stderr
/// when the transport closed, and otherwise whatever [`request_error`](crate::error::request_error)
/// made of the agent's answer. An opening call returns no session or control handle, so this is the
/// only path that preserves the tail for a host to inspect.
pub(crate) async fn send<Request>(
    connection: &Arc<ConnectionHandle>,
    profile: &crate::profile::AcpProfile,
    timeout: Duration,
    method: &'static str,
    request: Request,
) -> Result<Request::Response>
where
    Request: agent_client_protocol::JsonRpcRequest,
    Request::Response: Send,
{
    let sent = submit(connection, request)?;
    await_sent(connection, profile, timeout, method, sent).await
}

/// A request admitted before submission, with cleanup owned until its response arrives.
pub(crate) struct SubmittedRequest<'a, Response> {
    sent: agent_client_protocol::SentRequest<Response>,
    _permit: tokio::sync::SemaphorePermit<'a>,
    abandonment: RequestAbandonment,
}

/// Submits synchronously so callers can bind catalog revision and request admission together.
pub(crate) fn submit<Request>(
    connection: &Arc<ConnectionHandle>,
    request: Request,
) -> Result<SubmittedRequest<'_, Request::Response>>
where
    Request: agent_client_protocol::JsonRpcRequest,
{
    let (permit, sent) = connection
        .requests
        .submit(|| connection.connection().send_request(request))?;
    Ok(SubmittedRequest {
        sent,
        _permit: permit,
        abandonment: RequestAbandonment::new(Arc::clone(connection)),
    })
}

/// Awaits an admitted request already submitted under a synchronous lifecycle claim.
pub(crate) async fn await_sent<Response>(
    connection: &ConnectionHandle,
    profile: &crate::profile::AcpProfile,
    timeout: Duration,
    method: &'static str,
    submitted: SubmittedRequest<'_, Response>,
) -> Result<Response>
where
    Response: Send,
{
    let SubmittedRequest {
        sent,
        _permit,
        mut abandonment,
    } = submitted;
    let answered = tokio::time::timeout(timeout, sent.block_task()).await;
    let Ok(answered) = answered else {
        return Err(Error::Timeout {
            operation: format!("{method} on an ACP agent"),
            after: timeout,
        });
    };
    abandonment.disarm();
    answered.map_err(|error| {
        if is_link_closure(&error)
            && let Some(overflow) = connection.overflow()
        {
            return overflow.error();
        }
        if agent_client_protocol::is_incoming_transport_closed(&error) {
            return Error::Vendor(link_failure(with_stderr(
                &format!("a transport that closed under {method}"),
                connection.control().as_ref(),
            )));
        }
        crate::error::request_error(method, &error, &profile.login_text())
    })
}

/// Whether the SDK failed a request because the link went away, rather than the agent answering.
///
/// Two local shapes: the SDK's `incoming transport closed` reason, and its `never received` error
/// when the connection's reply channel was dropped with the request pending. Only these may be
/// explained by a connection-wide transport budget; an agent's own refusal is kept as it came.
/// For example, a `set_config_option` refusal stays a vendor error even after a later overflow.
pub(crate) fn is_link_closure(error: &agent_client_protocol::Error) -> bool {
    if agent_client_protocol::is_incoming_transport_closed(error) {
        return true;
    }
    let Some(detail) = error.data.as_ref().and_then(serde_json::Value::as_str) else {
        return false;
    };
    error.code == agent_client_protocol::ErrorCode::InternalError
        && detail.starts_with("response to `")
        && detail.contains("` never received: ")
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
mod connection_tests;

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use mango_external_agents::testing::{FakeLauncher, FakeProcess};
    use mango_external_agents::{
        AttemptId, ByteSink, ByteSource, CancelReason, Configuration, Error, EventSink, ExitStatus,
        HarnessIdentity, HostContext, LaunchSpec, ManagedProcess, ProcessControl, ProcessLauncher,
        Result, SessionIds, SessionSnapshot, TransportKind, TransportSelection, TurnId,
    };

    use super::{
        DriveShutdownGuard, PermissionLevel, RequestAdmission, SessionId, SessionState, drive,
        hold_next_drive_startup,
    };

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

    /// A process control whose first bounded cleanup fails and whose next one succeeds.
    struct RetryableCleanupControl {
        failed_once: AtomicBool,
        kills: AtomicUsize,
    }

    /// A child stdout that fails before ACP can hand `drive` a connection handle.
    struct FailingStartupSource;

    /// The unused writable half of a deliberately broken startup transport.
    struct InertStartupSink;

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

    #[async_trait::async_trait]
    impl ProcessControl for RetryableCleanupControl {
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
            if self.failed_once.swap(false, Ordering::AcqRel) {
                return Err(Error::Launch {
                    program: String::from("fake ACP agent"),
                    message: String::from("the first cleanup attempt failed"),
                });
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl ByteSource for FailingStartupSource {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
            Err(Error::Link {
                peer: String::from("fake ACP agent"),
                message: String::from("the startup pipe failed"),
            })
        }
    }

    #[async_trait::async_trait]
    impl ByteSink for InertStartupSink {
        async fn write_all(&mut self, _bytes: &[u8]) -> Result<()> {
            Ok(())
        }

        async fn close(&mut self) -> Result<()> {
            Ok(())
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

    /// A generic request must acquire admission before the ACP SDK can queue it, and releasing the
    /// completed request's RAII permit must admit the next caller.
    #[test]
    fn generic_request_admission_is_bounded_and_releases_after_completion() {
        let admission = RequestAdmission::new(1);
        let first = admission
            .submit(|| ())
            .expect("expected the first request permit");
        let refused = admission
            .submit(|| ())
            .expect_err("expected the second outstanding request to be refused");
        assert!(
            matches!(
                refused,
                Error::LimitExceeded {
                    subject: "outstanding ACP requests",
                    limit: 1,
                    received: 2,
                }
            ),
            "received {refused:?}"
        );
        drop(first.0);
        let _next = admission
            .submit(|| ())
            .expect("expected the completed request to release admission");
        admission.close();
        let closed = admission
            .submit(|| ())
            .expect_err("expected closed admission to refuse before queuing");
        assert!(
            matches!(
                closed,
                Error::Closed {
                    subject: "ACP connection"
                }
            ),
            "received {closed:?}"
        );
    }

    /// Cancelling `drive` before its connection task exists still reaps the child it was handed.
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

    #[tokio::test]
    async fn a_cancelled_driver_startup_kills_its_injected_child_once() {
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
        let control: Arc<dyn ProcessControl> = Arc::new(CountingProcessControl {
            inner: Arc::clone(&process.control),
            kills: Arc::clone(&kills),
        });
        process.control = Arc::clone(&control);
        let launched = crate::transport::frame(process, &host).expect("expected framed child");
        let cleanup = DriveShutdownGuard::from_launched(&launched, *host.limits());
        let (_release, reached) = hold_next_drive_startup(Arc::clone(&control));
        let task = tokio::spawn(drive(
            launched,
            Arc::new(state),
            String::from("acp-client-tests"),
            cleanup,
        ));
        reached
            .await
            .expect("expected the driver to stop before connection startup");
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(1), control.wait())
            .await
            .expect("expected cancellation to reap the held-startup child")
            .expect("expected child cleanup to succeed");
        assert_eq!(
            kills.load(Ordering::Acquire),
            1,
            "expected the pre-drive guard to claim cleanup exactly once"
        );
    }

    /// A pre-handle failure has no session owner, so its explicit bounded cleanup must return the
    /// host's control when the first stop attempt fails.
    #[tokio::test]
    async fn a_failed_pre_handle_cleanup_returns_control_for_host_recovery() {
        let concrete = Arc::new(RetryableCleanupControl {
            failed_once: AtomicBool::new(true),
            kills: AtomicUsize::new(0),
        });
        let control: Arc<dyn ProcessControl> = concrete.clone();
        let cleanup = DriveShutdownGuard {
            control: control.clone(),
            cleanup: Some(mango_external_agents::ProcessCleanupGuard::new(
                control.clone(),
                mango_external_agents::Limits::default(),
                CancelReason::Shutdown,
            )),
        };

        let error = cleanup
            .finish()
            .await
            .expect_err("expected the first pre-handle cleanup attempt to fail");
        assert!(
            matches!(error, Error::CleanupRequired { .. }),
            "expected a recoverable cleanup error, received {error:?}"
        );
        let recovered = error
            .cleanup_control()
            .expect("expected the failed drive cleanup to retain its process control");
        mango_external_agents::process::stop_process_with_limits(
            recovered.as_ref(),
            CancelReason::Shutdown,
            &mango_external_agents::Limits::default(),
        )
        .await
        .expect("expected host cleanup retry to reap the child");
        assert!(
            Arc::ptr_eq(&control, &recovered),
            "expected recovery to retain the originally launched control"
        );
        assert_eq!(
            concrete.kills.load(Ordering::Acquire),
            2,
            "expected one failed guard cleanup and one host retry"
        );
    }

    /// A transport that fails before ACP opens a connection has no session owner. `drive` must
    /// explicitly finish bounded cleanup and return its recovery control instead of the link error.
    #[tokio::test]
    async fn a_drive_startup_failure_returns_control_for_host_recovery() {
        let (state, host) = state();
        let concrete = Arc::new(RetryableCleanupControl {
            failed_once: AtomicBool::new(true),
            kills: AtomicUsize::new(0),
        });
        let control: Arc<dyn ProcessControl> = concrete.clone();
        let launched = crate::transport::frame(
            ManagedProcess {
                stdout: Box::new(FailingStartupSource),
                stdin: Some(Box::new(InertStartupSink)),
                control: control.clone(),
            },
            &host,
        )
        .expect("expected a framed startup transport");
        let cleanup = DriveShutdownGuard::from_launched(&launched, *host.limits());

        let error = drive(
            launched,
            Arc::new(state),
            String::from("acp-client-tests"),
            cleanup,
        )
        .await
        .expect_err("expected the startup transport to fail before connection ownership");
        assert!(
            matches!(error, Error::CleanupRequired { .. }),
            "expected cleanup recovery to take precedence, received {error:?}"
        );
        let recovered = error
            .cleanup_control()
            .expect("expected a recovery control after failed startup cleanup");
        mango_external_agents::process::stop_process_with_limits(
            recovered.as_ref(),
            CancelReason::Shutdown,
            host.limits(),
        )
        .await
        .expect("expected host recovery to reap the startup child");
        assert!(
            Arc::ptr_eq(&control, &recovered),
            "expected drive to retain the launched child control"
        );
        assert_eq!(
            concrete.kills.load(Ordering::Acquire),
            2,
            "expected one failed drive cleanup and one host retry"
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
        let cleanup = DriveShutdownGuard::from_launched(&launched, *host.limits());
        let connection = drive(
            launched,
            Arc::new(state),
            String::from("acp-client-tests"),
            cleanup,
        )
        .await
        .expect("expected a connection handle");
        (Arc::new(connection), kills, launcher)
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
        let reaped = tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.live_children() != 0 {
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
                agent_client_protocol::schema::v1::CloseSessionRequest::new(
                    agent_client_protocol::schema::v1::SessionId::new("sess_abandoned"),
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
}
