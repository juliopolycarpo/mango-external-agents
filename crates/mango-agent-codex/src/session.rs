//! One live `codex app-server` connection, driven as a [`Session`].
//!
//! The app-server is one long-lived process per session, and it talks back: an approval stops the
//! server until this client answers. That is why the handler below waits rather than returning,
//! and why every path that ends a turn — a refusal, a cancel, a close, a deadline — has to resolve
//! whatever the server is still waiting on. A question nobody answers is a vendor process blocked
//! for the rest of its life.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use mango_external_agents::HostContext;
use mango_external_agents::approval::ApprovalDeadline;
use mango_external_agents::configuration::{
    ConfigurationState, refuse_unsupported_native, refuse_unsupported_reset,
};
use mango_external_agents::error::{Error, ErrorCode, Result, VendorError};
use mango_external_agents::event::{EventKind, SessionId, TurnId};
use mango_external_agents::interaction::{
    Interaction, InteractionId, InteractionKind, Question, QuestionForm, QuestionId,
    QuestionOption, QuestionOptionId, QuestionOutcome, QuestionRequest, QuestionResponse,
    UnsupportedQuestion,
};
use mango_external_agents::jsonrpc::{
    Client, JsonRpcError, PeerHandler, PeerTermination, RequestId, ServerRequestOutcome,
};
use mango_external_agents::operation::{AttemptId, Dispatch, OperationRef};
use mango_external_agents::permission::{
    ApprovalDecision, DecisionSource, PermissionResponse, broker_response,
};
use mango_external_agents::process::{ProcessControl, stop_process_with_limits};
use mango_external_agents::session::{
    ATTACHMENT_MAX_BYTES, AccountUsage, CancelReason, CloseReason, NativeSession, ReviewRequest,
    Session, SessionPage, SessionQuery, Steer, SteerOutcome, SteerRejection, TURN_MAX_ATTACHMENTS,
    TurnRequest,
};
use mango_external_agents::state::{SessionState, SessionStatus};
use mango_external_agents::stream::{EventSink, ReviewStream, TurnStream};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, oneshot, watch};

use crate::approvals::{self, PendingApproval};
use crate::protocol::approvals::{
    McpServerElicitationRequestParams, McpServerElicitationRequestResponse, ServerAnswer,
    ServerRequest, ToolRequestUserInputAnswer, ToolRequestUserInputOption,
    ToolRequestUserInputParams, ToolRequestUserInputQuestion, ToolRequestUserInputResponse,
};
use crate::protocol::method;
use crate::protocol::notifications::{Notification, RateLimitSnapshot};
use crate::protocol::requests::{
    RateLimitsReadResponse, ReviewStartParams, ReviewStartResponse, ReviewTarget, ThreadListParams,
    ThreadListResponse, TurnHandle, TurnInterruptParams, TurnStartParams, TurnStartResponse,
    TurnSteerParams, TurnSteerResponse, UserInput, empty_params,
};
use crate::reducer::{self, Outcome};

/// The session or the connection would not take a call.
///
/// A turn asked for while one is already running is not this code: the app-server takes
/// `turn/start` on a live turn as a steer, so admission refuses it as [`Error::Busy`] before the
/// call is made.
pub const CALL_FAILED: ErrorCode = ErrorCode::from_static("codex-call-failed");

/// Native ids retained after terminal frames so a delayed old frame cannot attach while the next
/// start still waits for its response.
const RECENT_COMPLETED_TURNS: usize = 16;

/// How many superseded native ids one turn remembers for steers that still name them.
const EARLIER_NATIVE_TURN_IDS: usize = 16;

/// One live conversation with a `codex app-server`.
pub struct CodexSession {
    state: SessionState,
    configuration: Mutex<AcceptedConfiguration>,
    shared: Arc<Shared>,
    client: Arc<Client>,
    control: Arc<dyn ProcessControl>,
    closed: AtomicBool,
    /// Stops the vendor if the host cancels its shared lifetime token.
    shutdown_watcher: tokio::task::JoinHandle<()>,
    /// The runtime the session was opened on, so a drop on a thread without one still reaps.
    runtime: tokio::runtime::Handle,
}

/// Accepted defaults and the ordering of successful start attempts.
///
/// Not the core's [`ConfigurationState`]: that type is the three-way requested/accepted/observed
/// split published on [`SessionState`]. This one is this harness's own bookkeeping for merging
/// several turns' explicit axes by which one landed last, which is what
/// [`ConfigurationState::accepted`] is built from once a turn's request has landed.
struct AcceptedConfiguration {
    accepted: mango_external_agents::Configuration,
    next_generation: u64,
    accepted_generation: [u64; 4],
}

impl AcceptedConfiguration {
    fn accept(
        &mut self,
        generation: u64,
        configuration: mango_external_agents::Configuration,
    ) -> mango_external_agents::Configuration {
        let mut changed = mango_external_agents::Configuration::unknown();
        if apply_selected(
            &mut self.accepted.model,
            &mut self.accepted_generation[0],
            generation,
            &configuration.model,
        ) {
            changed.model = configuration.model;
        }
        if apply_selected(
            &mut self.accepted.effort,
            &mut self.accepted_generation[1],
            generation,
            &configuration.effort,
        ) {
            changed.effort = configuration.effort;
        }
        if apply_selected(
            &mut self.accepted.level,
            &mut self.accepted_generation[2],
            generation,
            &configuration.level,
        ) {
            changed.level = configuration.level;
        }
        if apply_selected(
            &mut self.accepted.routing,
            &mut self.accepted_generation[3],
            generation,
            &configuration.routing,
        ) {
            changed.routing = configuration.routing;
        }
        changed
    }
}

/// A later successful request only supersedes fields it explicitly selected.
fn apply_selected<T: Clone>(
    accepted: &mut Option<T>,
    last_generation: &mut u64,
    generation: u64,
    selected: &Option<T>,
) -> bool {
    if selected.is_some() && generation > *last_generation {
        *accepted = selected.clone();
        *last_generation = generation;
        return true;
    }
    false
}

impl std::fmt::Debug for CodexSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexSession")
            .field("ids", &self.state.snapshot().ids)
            .field("pid", &self.control.pid())
            .finish_non_exhaustive()
    }
}

/// What the handler and the session both reach for.
pub(crate) struct Shared {
    host: HostContext,
    session_id: mango_external_agents::SessionId,
    /// The conversation this session subscribed to, once the server has named one.
    ///
    /// Set after `thread/start`, which cannot happen until the connection is already pumping —
    /// the handler exists before the thread does. Until it is set, every conversation-scoped
    /// announcement is somebody else's, which is the right answer: no turn is running yet.
    thread_id: std::sync::OnceLock<String>,
    control: std::sync::OnceLock<Arc<dyn ProcessControl>>,
    client: std::sync::OnceLock<Arc<Client>>,
    turn: Mutex<Option<ActiveTurn>>,
    recent_completed_turns: Mutex<VecDeque<String>>,
    pending: Mutex<HashMap<InteractionId, PendingEntry>>,
    /// Question rounds the server is waiting on.
    ///
    /// Separate from [`Shared::pending`]: a question grants no authority, so it never touches a
    /// [`PermissionBroker`](mango_external_agents::PermissionBroker), and its wire answer takes a
    /// different shape than any approval's.
    pending_questions: Mutex<HashMap<InteractionId, PendingQuestion>>,
    /// Serializes question publication with terminal cleanup.
    ///
    /// A host answer remains in [`Shared::pending_questions`] until its resolution reaches the
    /// stream. A terminal takes this lock before draining that map, so either side publishes the
    /// resolution before `Completed` can claim the stream.
    question_settlements: Mutex<()>,
    /// Whether this connection can accept more work.
    ///
    /// A start request that times out may already be running at the vendor, so its active slot
    /// stays occupied. A terminated connection is stronger: nothing can safely use this session
    /// again, even after that slot has drained.
    shutting_down: AtomicBool,
    /// Wakes the session-owned process reaper after an unexpected link termination.
    terminated: mango_external_agents::CancelToken,
    /// Wakes an abandonment watcher after its owned native turn reaches a terminal outcome, and a
    /// stop worker waiting on a pending start once that start is named or can never be.
    turn_finished: Notify,
    /// Restarts the one active turn's idle period after a native frame or approval transition.
    idle_changes: watch::Sender<u64>,
    /// The first teardown requester owns a detached cleanup worker that survives caller drop.
    teardown_started: AtomicBool,
    teardown_complete: AtomicBool,
    /// The teardown worker's one failure, retained so every close waiter can recover it.
    teardown_error: Mutex<Option<Error>>,
    teardown_done: Notify,
    /// Request-id state for messages whose request task and resolution notification race on the
    /// same connection. Both classifications share one lock so a normal confirmation cannot be
    /// recorded as an early resolution while its answer is being remembered.
    resolution_markers: Mutex<ResolutionMarkers>,
    /// Serializes steers, so each one reads the native turn id the previous steer left behind.
    steering: Mutex<()>,
    /// Notifications held, in arrival order, while a steer's answer is being adopted.
    steer_hold: std::sync::Mutex<SteerHold>,
    /// The account's quota: the last full reading, and what arrived while a read was in flight.
    quota: Mutex<QuotaState>,
    /// Whether an update already asked for the missing baseline.
    baseline_requested: AtomicBool,
    #[cfg(test)]
    resolution_marker_gate: Mutex<Option<Arc<ResolutionMarkerGate>>>,
    #[cfg(test)]
    question_settlement_gate: Mutex<Option<Arc<ResolutionMarkerGate>>>,
}

impl Shared {
    /// The state one connection's handler and session share, before a thread exists.
    pub(crate) fn new(host: HostContext, session_id: mango_external_agents::SessionId) -> Self {
        let (idle_changes, _) = watch::channel(0_u64);
        Self {
            host,
            session_id,
            thread_id: std::sync::OnceLock::new(),
            control: std::sync::OnceLock::new(),
            client: std::sync::OnceLock::new(),
            turn: Mutex::new(None),
            recent_completed_turns: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashMap::new()),
            pending_questions: Mutex::new(HashMap::new()),
            question_settlements: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            terminated: mango_external_agents::CancelToken::new(),
            turn_finished: Notify::new(),
            idle_changes,
            teardown_started: AtomicBool::new(false),
            teardown_complete: AtomicBool::new(false),
            teardown_error: Mutex::new(None),
            teardown_done: Notify::new(),
            resolution_markers: Mutex::new(ResolutionMarkers::default()),
            steering: Mutex::new(()),
            steer_hold: std::sync::Mutex::new(SteerHold::default()),
            quota: Mutex::new(QuotaState::default()),
            baseline_requested: AtomicBool::new(false),
            #[cfg(test)]
            resolution_marker_gate: Mutex::new(None),
            #[cfg(test)]
            question_settlement_gate: Mutex::new(None),
        }
    }

    /// Names the conversation this session follows. Called once, after the server opens it.
    pub(crate) fn adopt_thread(&self, thread_id: String) {
        let _ = self.thread_id.set(thread_id);
    }

    /// The conversation this session follows, or nothing before one exists.
    fn thread_id(&self) -> &str {
        self.thread_id.get().map_or("", String::as_str)
    }

    fn stop_new_work(&self) -> bool {
        !self.shutting_down.swap(true, Ordering::AcqRel)
    }

    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    /// Restarts idle accounting after progress the current native turn made.
    fn signal_idle_change(&self) {
        let next = self.idle_changes.borrow().wrapping_add(1);
        self.idle_changes.send_replace(next);
    }

    /// Whether the owner has an approval or a question round whose own deadline, rather than idle
    /// time, governs it.
    async fn approval_is_pending_for(&self, owner: &Arc<()>) -> bool {
        let approval_pending = self
            .pending
            .lock()
            .await
            .values()
            .any(|entry| Arc::ptr_eq(&entry.route.owner, owner));
        if approval_pending {
            return true;
        }
        self.pending_questions
            .lock()
            .await
            .values()
            .any(|entry| Arc::ptr_eq(&entry.route.owner, owner))
    }

    /// Whether this owner still has a running native turn whose idle deadline may fire.
    async fn owner_is_active(&self, owner: &Arc<()>) -> bool {
        self.turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|active| !active.finishing && Arc::ptr_eq(&active.owner, owner))
    }

    /// Snapshots the active turn's owner and native id so notification routing remains bound to
    /// the start attempt even while its response is setting the native id.
    async fn active_turn_route(&self) -> Option<ActiveTurnRoute> {
        self.turn
            .lock()
            .await
            .as_ref()
            .filter(|active| !active.finishing)
            .map(|active| ActiveTurnRoute {
                owner: Arc::clone(&active.owner),
                turn_id: active.turn_id.clone(),
                attempt: active.attempt,
                native_turn_id: active.native_turn_id.clone(),
            })
    }

    /// Whether an approval can still be admitted for this owned native turn.
    ///
    /// This check shares the turn lock with `cancel_owner`, so once cancellation is latched a
    /// late server request cannot enter the host approval path.
    async fn approval_route_is_admissible(&self, route: &ActiveTurnRoute) -> bool {
        if self.is_shutting_down() || self.host.cancel().is_cancelled() {
            return false;
        }
        self.turn.lock().await.as_ref().is_some_and(|active| {
            !active.finishing
                && active.cancel_reason.is_none()
                && Arc::ptr_eq(&active.owner, &route.owner)
        })
    }

    /// Whether the start attempt identified by `route` still owns the active turn slot.
    async fn owns_active_turn(&self, route: &ActiveTurnRoute) -> bool {
        self.turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|active| !active.finishing && Arc::ptr_eq(&active.owner, &route.owner))
    }

    /// Waits until this owner no longer occupies admission.
    async fn wait_for_turn_end(&self, owner: &Arc<()>) {
        loop {
            let changed = self.turn_finished.notified();
            if !self
                .turn
                .lock()
                .await
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(&active.owner, owner))
            {
                return;
            }
            changed.await;
        }
    }

    /// Waits until this owner's start can be acted on by a stop: its native turn is named, its start
    /// answer can never arrive, or it no longer occupies admission.
    ///
    /// Not bounded here: the caller bounds it. `dispatch_stop` wraps this wait in
    /// `Limits::request_timeout` and marks the start unanswerable on expiry, because the start's
    /// own request deadline only runs while the host keeps polling its `start_turn` future.
    async fn wait_for_start_resolution(&self, owner: &Arc<()>) {
        loop {
            let changed = self.turn_finished.notified();
            if !self.turn.lock().await.as_ref().is_some_and(|active| {
                Arc::ptr_eq(&active.owner, owner)
                    && !active.finishing
                    && active.native_turn_id.is_empty()
                    && !active.start_unanswerable
            }) {
                return;
            }
            changed.await;
        }
    }

    /// Whether this owner is live and not yet interrupted, so naming it — now or later — should
    /// trigger the stop's one interrupt.
    ///
    /// Not gated on the id still being empty: the pump can name the turn between the stop's
    /// empty-id pass and this check, and [`Self::wait_for_named`] returns at once for a named turn.
    async fn awaiting_name(&self, owner: &Arc<()>) -> bool {
        self.turn.lock().await.as_ref().is_some_and(|active| {
            Arc::ptr_eq(&active.owner, owner) && !active.finishing && !active.interrupt_dispatched
        })
    }

    /// Waits until this owner's native turn is named, by a notification or a late answer.
    ///
    /// Never returns while the owner stays unnamed; the caller races it against the turn's end
    /// and its settle deadline.
    async fn wait_for_named(&self, owner: &Arc<()>) {
        loop {
            let changed = self.turn_finished.notified();
            if self.turn.lock().await.as_ref().is_some_and(|active| {
                Arc::ptr_eq(&active.owner, owner) && !active.native_turn_id.is_empty()
            }) {
                return;
            }
            changed.await;
        }
    }

    /// Records that this owner's `turn/start` answer will never be read, so a stop waiting for the
    /// turn's id moves on to its settle deadline instead.
    async fn mark_start_unanswerable(&self, owner: &Arc<()>) {
        if let Some(active) = self
            .turn
            .lock()
            .await
            .as_mut()
            .filter(|active| Arc::ptr_eq(&active.owner, owner))
        {
            active.start_unanswerable = true;
        }
        self.turn_finished.notify_waiters();
    }

    /// Seals the session only if this owner still holds admission at the point an abandoned-turn
    /// reaper must escalate. The turn lock also guards `begin`, so a replacement cannot enter the
    /// gap between this check and the shutdown request.
    async fn seal_owner_for_shutdown(&self, owner: &Arc<()>) -> bool {
        let turn = self.turn.lock().await;
        if !turn
            .as_ref()
            .is_some_and(|active| !active.finishing && Arc::ptr_eq(&active.owner, owner))
        {
            return false;
        }
        self.stop_new_work();
        true
    }

    /// Asks Codex to stop one owned turn without ever naming a replacement turn.
    async fn cancel_owner(
        &self,
        client: &Client,
        owner: Option<&Arc<()>>,
        reason: CancelReason,
    ) -> Result<bool> {
        let Some((route, thread_id, native_turn_id, no_dispatch)) = ({
            let mut turn = self.turn.lock().await;
            turn.as_mut()
                .filter(|active| {
                    !active.finishing && owner.is_none_or(|owner| Arc::ptr_eq(&active.owner, owner))
                })
                .map(|active| {
                    active.cancel_reason.get_or_insert(reason);
                    let pending_start = active.native_turn_id.is_empty();
                    // One native stop per turn: a second interrupt while the first is still
                    // unanswered adds nothing but another request to time out.
                    let already_dispatched = active.interrupt_dispatched;
                    if !pending_start {
                        active.interrupt_dispatched = true;
                    }
                    (
                        ActiveTurnRoute {
                            owner: Arc::clone(&active.owner),
                            turn_id: active.turn_id.clone(),
                            attempt: active.attempt,
                            native_turn_id: active.native_turn_id.clone(),
                        },
                        self.thread_id().to_owned(),
                        active.native_turn_id.clone(),
                        pending_start || already_dispatched,
                    )
                })
        }) else {
            return Ok(false);
        };

        self.release_pending_for(Some(&route.owner), DecisionSource::Cancelled)
            .await;
        if !self.owns_active_turn(&route).await || no_dispatch {
            return Ok(true);
        }

        let interrupt: Result<Value> = client
            .request(
                method::TURN_INTERRUPT,
                TurnInterruptParams {
                    thread_id,
                    turn_id: native_turn_id,
                },
            )
            .await;
        match interrupt {
            Ok(_) => Ok(true),
            Err(error) if self.owns_active_turn(&route).await => Err(error),
            Err(_) => Ok(true),
        }
    }

    async fn recently_completed(&self, native_turn_id: &str) -> bool {
        self.recent_completed_turns
            .lock()
            .await
            .iter()
            .any(|completed| completed == native_turn_id)
    }

    async fn remember_completed(&self, native_turn_id: &str) {
        if native_turn_id.is_empty() {
            return;
        }
        let mut completed = self.recent_completed_turns.lock().await;
        completed.retain(|existing| existing != native_turn_id);
        completed.push_back(native_turn_id.to_owned());
        if completed.len() > RECENT_COMPLETED_TURNS {
            completed.pop_front();
        }
    }
}

/// The session's quota bookkeeping, under one lock so a read's answer and an update cannot
/// interleave between checking and merging.
#[derive(Default)]
struct QuotaState {
    /// The last full `account/rateLimits/read`, merged forward by sparse updates.
    ///
    /// `None` until a read answers: a sparse update alone is not a reading worth showing.
    baseline: Option<RateLimitSnapshot>,
    /// Updates that arrived while a read was in flight, merged in arrival order.
    ///
    /// The notification worker and the task awaiting the read run separately, so these may be
    /// newer than the read's answer and are laid over it when it lands. They belong to reads in
    /// flight only: when the last one fails, they are dropped rather than laid over a later one.
    pending: Option<RateLimitSnapshot>,
    /// Full reads, from the host's refresh or the session's own, not yet answered.
    reads_in_flight: usize,
}

/// How many notifications a steer may hold before the session gives up on the connection.
const STEER_HOLD_LIMIT: usize = 1024;

/// Notifications that arrived while a steer was in flight.
///
/// The steer's answer can name a continuation turn id, and the notification worker is separate
/// from the task awaiting that answer: frames for the continuation can be routed before the id is
/// adopted and would be dropped as another turn's. Holding every notification until the answer
/// is handled, then replaying them in order, makes the transition visible before any of them is
/// routed.
///
/// Held frames are outside the connection's own byte budget, so the hold is bounded by count and
/// by [`Limits::turn_buffer_bytes`](mango_external_agents::Limits::turn_buffer_bytes); exceeding
/// either poisons the session.
#[derive(Default)]
struct SteerHold {
    /// Steers whose answer has not been handled yet. Steers are serialized, but a dropped steer
    /// future can leave its replay running while the next steer begins.
    holders: usize,
    /// Whether a replay task owns the queue. At most one exists, so frames leave in order.
    draining: bool,
    held: VecDeque<(String, Value, usize)>,
    held_bytes: usize,
}

impl SteerHold {
    /// Whether a new frame has to join the queue rather than be routed now.
    fn is_holding(&self) -> bool {
        self.holders > 0 || self.draining
    }
}

/// One steer's claim on the hold; released exactly once, whether or not the steer finishes.
struct SteerHoldGuard {
    shared: Option<Arc<Shared>>,
}

impl SteerHoldGuard {
    fn hold(shared: &Arc<Shared>) -> Self {
        shared
            .steer_hold
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .holders += 1;
        Self {
            shared: Some(Arc::clone(shared)),
        }
    }

    /// Releases this steer's claim and waits for the replay it may have started.
    ///
    /// The replay runs in its own task, so dropping this future — a host's timeout wrapper, a
    /// disconnect — only stops the waiting; the replay still finishes and routing reopens.
    async fn release(mut self) {
        let Some(shared) = self.shared.take() else {
            return;
        };
        if let Some(replay) = release_claim(shared) {
            let _ = replay.await;
        }
    }
}

impl Drop for SteerHoldGuard {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.take() {
            let _ = release_claim(shared);
        }
    }
}

/// Gives up one claim and, when it was the last and nobody is replaying, starts the replay.
fn release_claim(shared: Arc<Shared>) -> Option<tokio::task::JoinHandle<()>> {
    let mut hold = shared
        .steer_hold
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    hold.holders = hold.holders.saturating_sub(1);
    if hold.holders > 0 || hold.draining {
        return None;
    }
    if hold.held.is_empty() {
        return None;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        // No runtime left to route on: the connection is going away with it.
        hold.held.clear();
        hold.held_bytes = 0;
        return None;
    };
    hold.draining = true;
    drop(hold);
    Some(runtime.spawn(replay_steer_hold(shared)))
}

/// Routes the held frames one at a time and reopens live routing only once the queue is empty,
/// under the same lock the worker checks, so no live frame can overtake a held one. A steer that
/// takes a new claim meanwhile leaves the rest queued for its own release.
async fn replay_steer_hold(shared: Arc<Shared>) {
    let handler = CodexHandler {
        shared: Arc::clone(&shared),
    };
    loop {
        let next = {
            let mut hold = shared
                .steer_hold
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let next = if hold.holders > 0 {
                None
            } else {
                hold.held.pop_front()
            };
            match next {
                Some((method, params, bytes)) => {
                    hold.held_bytes = hold.held_bytes.saturating_sub(bytes);
                    (method, params)
                }
                None => {
                    hold.draining = false;
                    return;
                }
            }
        };
        handler.handle_notification(next.0, next.1).await;
    }
}

/// The turn currently running, and the sink its events go to.
struct ActiveTurn {
    /// The particular `begin` call that installed this slot.
    ///
    /// A host turn id may be reused, so ownership is shared only by one start attempt. A delayed
    /// answer must not mutate a later attempt that happens to carry the same id.
    owner: Arc<()>,
    sink: EventSink,
    turn_id: TurnId,
    attempt: AttemptId,
    native_turn_id: String,
    /// Whether [`EventKind::TurnStarted`] has already been put on this attempt's stream.
    ///
    /// Two sides can learn the vendor's own turn id first: the `turn/start` response, or a
    /// notification that names it before that answer arrives — captured fixtures show the second
    /// happening on the ordinary path, not as a rare race. Whichever side wins claims this flag
    /// under the turn lock and announces; the other does nothing, which is what keeps the
    /// announcement to exactly once.
    announced: bool,
    /// Native ids this attempt ran under before a steer continued it under `native_turn_id`.
    ///
    /// A host knows only the id the turn was announced with, so a steer naming any of these still
    /// addresses this attempt. The first entry, that announced id, is kept for the whole turn;
    /// the rest are bounded by [`EARLIER_NATIVE_TURN_IDS`].
    earlier_native_turn_ids: VecDeque<String>,
    /// What this turn already announced, for the announcements that depend on it.
    reducer: crate::turn_reducer::TurnReducer,
    /// Native reviews have their own lifecycle and do not accept user steering.
    is_review: bool,
    /// The `turn/start` answer will never be read, so this turn can never be named or interrupted.
    start_unanswerable: bool,
    /// `turn/interrupt` has been sent for this turn. It is sent at most once.
    interrupt_dispatched: bool,
    /// Why this turn is being stopped, when somebody here asked for it.
    ///
    /// The server reports only that a turn was interrupted. Who asked, and why, is this side's
    /// knowledge — and "you stopped this turn" is a lie for a shutdown.
    cancel_reason: Option<CancelReason>,
    /// Stops the native turn if the library stream owner is dropped before completion.
    abandonment_watcher: Option<tokio::task::JoinHandle<()>>,
    /// Stops a native turn that remains inactive past the host's configured deadline.
    idle_watcher: Option<tokio::task::JoinHandle<()>>,
    /// Owns one durable native-stop worker across every cancellation caller.
    stop: Arc<StopState>,
    /// A terminal is being committed. Admission remains held until it is visible to the host.
    finishing: bool,
}

/// The data needed to commit one terminal without holding the admission lock.
struct TerminalClaim {
    owner: Arc<()>,
    sink: EventSink,
    cancel_reason: Option<CancelReason>,
}

/// How a stopped turn's settle stage ended.
enum Settled {
    /// Codex reported the turn over, or another path released it.
    Ended,
    /// The settle deadline passed with the turn still admitted.
    Expired,
    /// The late interrupt for a newly named turn failed while the turn was live.
    InterruptFailed(Error),
}

/// Completion state for one detached native-stop worker.
struct StopState {
    started: AtomicBool,
    complete: AtomicBool,
    /// The worker's own error, kept whole so a `CleanupRequired` still carries its control.
    failure: Mutex<Option<Error>>,
    done: Notify,
}

impl StopState {
    fn new() -> Self {
        Self {
            started: AtomicBool::new(false),
            complete: AtomicBool::new(false),
            failure: Mutex::new(None),
            done: Notify::new(),
        }
    }

    /// A stop that has nothing left to do, for an owner that no longer holds admission.
    fn finished() -> Self {
        let stop = Self::new();
        stop.started.store(true, Ordering::Release);
        stop.complete.store(true, Ordering::Release);
        stop
    }
}

/// Cleans up admission when the caller drops `start_turn` before it can return a stream owner.
struct StartGuard {
    shared: Arc<Shared>,
    client: Arc<Client>,
    control: Arc<dyn ProcessControl>,
    state: SessionState,
    owner: Arc<()>,
    armed: bool,
    /// The runtime the start began on, so a drop on a thread without one still abandons the owner.
    runtime: tokio::runtime::Handle,
    /// Whether any byte of this attempt's start request may have reached Codex.
    request_written: Arc<AtomicBool>,
}

impl StartGuard {
    fn new(
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
        state: SessionState,
        owner: Arc<()>,
    ) -> Self {
        Self {
            shared,
            client,
            control,
            state,
            owner,
            armed: true,
            // Built inside the async `start_turn`, so a runtime is current.
            runtime: tokio::runtime::Handle::current(),
            request_written: Arc::new(AtomicBool::new(false)),
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StartGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let client = Arc::clone(&self.client);
        let control = Arc::clone(&self.control);
        let state = self.state.clone();
        let owner = Arc::clone(&self.owner);
        // The current runtime when there is one, else the one the start began on: a caller
        // dropping the future from a plain thread must not strand the turn slot.
        let runtime =
            tokio::runtime::Handle::try_current().unwrap_or_else(|_| self.runtime.clone());
        // A start request that never began writing cannot have started a native turn, so its
        // owner is released at once, without a settle wait or a kill.
        if !self.request_written.load(Ordering::Acquire) {
            runtime.spawn(async move {
                shared.release_refused_start(&owner).await;
            });
            return;
        }
        runtime.spawn(async move {
            shared.mark_start_unanswerable(&owner).await;
            CodexSession::abandon_owner(shared, client, control, state, owner).await;
        });
    }
}

/// The identity one notification must retain from parsing through emission or completion.
#[derive(Clone)]
struct ActiveTurnRoute {
    owner: Arc<()>,
    turn_id: TurnId,
    attempt: AttemptId,
    native_turn_id: String,
}

impl ActiveTurnRoute {
    /// The session, turn and attempt an approval raised on this route belongs to.
    fn operation(&self, session_id: SessionId) -> OperationRef {
        OperationRef::new(session_id, self.turn_id.clone(), self.attempt)
    }
}

/// One question the server is waiting on.
struct PendingEntry {
    /// The JSON-RPC id the server will match the answer to, in the shape it arrived.
    request_key: String,
    /// The turn that owns the server request and its host-visible prompt.
    route: ActiveTurnRoute,
    pending: PendingApproval,
    /// The request's original wall-clock expiry, translated once for every later wait.
    deadline: ApprovalDeadline,
    answer: oneshot::Sender<Answer>,
}

/// One round of questions the server is waiting on.
///
/// Not a [`PendingEntry`]: a question grants no authority, so it carries no
/// [`PermissionBroker`](mango_external_agents::PermissionBroker) race and no vendor decision — only
/// the bounded [`QuestionRequest`] a host was shown and the answer channel that settles it.
struct PendingQuestion {
    /// The JSON-RPC id the server will match the answer to, in the shape it arrived.
    request_key: String,
    /// The turn that owns the server request and its host-visible prompt.
    route: ActiveTurnRoute,
    /// The bounded round a host was actually shown.
    request: QuestionRequest,
    /// The request's original wall-clock expiry, translated once for every later wait.
    deadline: ApprovalDeadline,
    /// The sender that wakes the handler when this round settles.
    ///
    /// A host answer stays in this entry after the one-shot receives it. Terminal cleanup can
    /// then publish that accepted outcome before it commits `Completed`, even if the handler has
    /// not had another chance to poll.
    answer: Option<oneshot::Sender<QuestionAnswer>>,
    /// The outcome a host has already accepted but the waiting handler has not yet published.
    settlement: Option<QuestionOutcome>,
    /// Whether the host was shown this round and therefore needs a matching resolution.
    announced: bool,
}

/// An approval answer awaiting the app-server notification that confirms the request ended.
struct AnsweredResolution {
    request_key: String,
    owner: Arc<()>,
}

/// Resolution state owned by the active native turn.
#[derive(Default)]
struct ResolutionMarkers {
    /// Questions the server stopped waiting on before this side registered them.
    early: HashMap<String, Arc<()>>,
    /// Answers this side returned while their matching confirmation is still in flight.
    answered: VecDeque<AnsweredResolution>,
}

#[cfg(test)]
/// A test-only barrier that holds one reply-marker transition while a confirmation queues behind it.
struct ResolutionMarkerGate {
    entered: AtomicBool,
    entered_notice: Notify,
    release: tokio::sync::Semaphore,
}

#[cfg(test)]
impl ResolutionMarkerGate {
    fn closed() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicBool::new(false),
            entered_notice: Notify::new(),
            release: tokio::sync::Semaphore::new(0),
        })
    }

    async fn wait_until_entered(&self) {
        loop {
            let notice = self.entered_notice.notified();
            if self.entered.load(Ordering::Acquire) {
                return;
            }
            notice.await;
        }
    }

    async fn wait_for_release(&self) {
        self.entered.store(true, Ordering::Release);
        self.entered_notice.notify_waiters();
        self.release
            .acquire()
            .await
            .expect("the test gate must stay open")
            .forget();
    }

    fn open(&self) {
        self.release.add_permits(1);
    }
}

/// How a waiting question was settled.
enum Answer {
    /// Somebody chose.
    Chosen {
        decision: ServerAnswer,
        option_id: String,
        source: DecisionSource,
        /// Whether the caller that settled this question already recorded its audit event.
        reported: bool,
    },
    /// The server stopped waiting on its own, so nothing needs sending.
    ResolvedByTheServer,
}

/// How a waiting round of questions was settled.
enum QuestionAnswer {
    /// A host answered through [`Session::answer`].
    Answered(QuestionResponse),
    /// [`Session::answer`] arrived after this round's own deadline had already passed, whether or
    /// not [`CodexHandler::decide_question`]'s own wait had noticed yet.
    Expired,
    /// The turn or session ended before anybody answered.
    ///
    /// Sent by the owner-scoped cancellation [`Shared::release_pending_for`] performs when a turn
    /// ends while a round is still open. A plain deadline never sends this:
    /// [`CodexHandler::decide_question`] notices its own expiry locally instead, the same way
    /// [`CodexHandler::expire`] does for an approval.
    Cancelled {
        /// Whether the caller that settled this round already recorded its audit event.
        ///
        /// The same bit [`Answer::Chosen`] carries, for the same reason: the canceller publishes
        /// the resolution itself so it lands before the turn's terminal, and the waiter it wakes
        /// must not publish an identical second one.
        reported: bool,
    },
    /// The server stopped waiting on its own.
    ResolvedByTheServer {
        /// Whether the withdrawing side already closed the host-visible round.
        reported: bool,
    },
}

/// The first result while the broker and host race under one approval deadline.
enum BrokerWait {
    /// The host or server settled the question before the broker did.
    Answer(Option<Answer>),
    /// The broker completed, or the shared deadline elapsed while it deliberated.
    Broker(Option<Option<PermissionResponse>>),
}

/// Whether a waiting question still belongs to the turn that is resolving it.
enum PendingTake {
    /// The caller owns the question and removed it.
    Taken,
    /// Another path already removed the question.
    Missing,
    /// A newer turn owns the same request id.
    Foreign,
}

impl Shared {
    /// Puts one event on the running turn's stream.
    ///
    /// The bounded sink never waits for a reader. Its receiver's drop is observed separately by
    /// the abandonment watcher, which stops the native turn instead of leaving it unowned.
    async fn emit(&self, kind: EventKind) -> Result<()> {
        let sink = {
            let turn = self.turn.lock().await;
            turn.as_ref()
                .filter(|active| !active.finishing)
                .map(|active| active.sink.clone())
        };
        let Some(sink) = sink else {
            return Ok(());
        };
        sink.emit(kind).await
    }

    /// Wins the race to announce one attempt's acceptance.
    ///
    /// The vendor's own turn id can arrive two ways: as the `turn/start` response, or on a
    /// notification that names it before that response comes back — captured fixtures show the
    /// second happening on the ordinary path, not as a rare race. Whichever caller reaches this
    /// first, under the turn lock, wins: it records the id and takes the sink to announce
    /// through. Every other caller, including a second one from the losing side, receives
    /// `None` — which is what keeps [`EventKind::TurnStarted`] to exactly once per attempt.
    async fn claim_announcement(
        &self,
        owner: &Arc<()>,
        native_turn_id: &str,
    ) -> Result<Option<(EventSink, String)>> {
        let native_turn_id =
            mango_external_agents::normalize::opaque_id(native_turn_id, "native turn id")?;
        let mut turn = self.turn.lock().await;
        let active = turn
            .as_mut()
            .filter(|active| !active.finishing && Arc::ptr_eq(&active.owner, owner));
        let Some(active) = active else {
            return Ok(None);
        };
        if active.announced {
            return Ok(None);
        }
        active.announced = true;
        active.native_turn_id = native_turn_id.clone();
        let sink = active.sink.clone();
        drop(turn);
        // A stop worker waiting for this start can now name the turn it has to interrupt.
        self.turn_finished.notify_waiters();
        Ok(Some((sink, native_turn_id)))
    }

    /// Folds one quota update into the baseline and reports the result to the running turn.
    ///
    /// With no baseline yet, the update is not shown; instead one `account/rateLimits/read` is
    /// asked for in the background, and its full answer becomes the baseline and the reading
    /// reported. The update that asks is older than that answer; any that arrive while a read is
    /// in flight may not be, and are laid over its answer.
    async fn rate_limits_updated(self: &Arc<Self>, update: &RateLimitSnapshot) {
        let merged = {
            let mut quota = self.quota.lock().await;
            if quota.reads_in_flight > 0 {
                quota.pending = Some(match quota.pending.as_ref() {
                    Some(earlier) => crate::rate_limits::merge(earlier, update),
                    None => update.clone(),
                });
            }
            let Some(current) = quota.baseline.as_ref() else {
                let idle = quota.reads_in_flight == 0;
                drop(quota);
                if idle {
                    self.request_baseline();
                }
                return;
            };
            let merged = crate::rate_limits::merge(current, update);
            quota.baseline = Some(merged.clone());
            merged
        };
        self.report_limits(&merged).await;
    }

    /// Records that a full read is about to be sent.
    async fn begin_quota_read(&self) {
        self.quota.lock().await.reads_in_flight += 1;
    }

    /// Settles one full read: its answer, with any update that arrived meanwhile laid over it,
    /// becomes the baseline. A read that returned nothing leaves the baseline alone, and when it
    /// was the last one in flight its pending updates go with it.
    async fn finish_quota_read(
        &self,
        answer: Option<&RateLimitSnapshot>,
    ) -> Option<RateLimitSnapshot> {
        let mut quota = self.quota.lock().await;
        quota.reads_in_flight = quota.reads_in_flight.saturating_sub(1);
        let Some(answer) = answer else {
            if quota.reads_in_flight == 0 {
                quota.pending = None;
            }
            return None;
        };
        let merged = match quota.pending.take() {
            Some(pending) => crate::rate_limits::merge(answer, &pending),
            None => answer.clone(),
        };
        quota.baseline = Some(merged.clone());
        Some(merged)
    }

    /// Asks for the full reading a sparse update could not be merged onto, once at a time.
    fn request_baseline(self: &Arc<Self>) {
        if self.baseline_requested.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(client) = self.client.get().cloned() else {
            self.baseline_requested.store(false, Ordering::Release);
            return;
        };
        let shared = Arc::clone(self);
        // Detached: the notification handler runs on the connection's reader, which has to keep
        // reading for this request to be answered at all.
        tokio::spawn(async move {
            shared.begin_quota_read().await;
            let read = client
                .request::<_, RateLimitsReadResponse>(
                    method::ACCOUNT_RATE_LIMITS_READ,
                    empty_params(),
                )
                .await;
            let answer = read.ok().and_then(|response| response.rate_limits);
            let merged = shared.finish_quota_read(answer.as_ref()).await;
            // A later update may ask again, whether this one answered or not.
            shared.baseline_requested.store(false, Ordering::Release);
            if let Some(merged) = merged {
                shared.report_limits(&merged).await;
            }
        });
    }

    /// Puts one quota reading on the running turn's stream, when a turn is running.
    async fn report_limits(&self, snapshot: &RateLimitSnapshot) {
        let limits = crate::rate_limits::to_account_limits(snapshot, self.host.now());
        let _ = self.emit(EventKind::AccountLimits { limits }).await;
    }

    /// Moves the attempt `owner` still runs from `previous` to the continuation id a steer named.
    ///
    /// The app-server answers `turn/steer` with "the accepted turnId"; when that differs from the
    /// one the steer expected, Codex continues the turn under it, and later frames, steers and the
    /// interrupt must use it. A turn that ended or moved on meanwhile is left alone.
    async fn adopt_continuation(
        &self,
        owner: &Arc<()>,
        previous: &str,
        continued: &str,
    ) -> Result<()> {
        if continued.is_empty() || continued == previous {
            return Ok(());
        }
        let continued =
            mango_external_agents::normalize::opaque_id(continued, "continued native turn id")?;
        let mut turn = self.turn.lock().await;
        let Some(active) = turn.as_mut().filter(|active| {
            !active.finishing
                && Arc::ptr_eq(&active.owner, owner)
                && active.native_turn_id == previous
        }) else {
            return Ok(());
        };
        let previous = std::mem::replace(&mut active.native_turn_id, continued);
        // The first entry is the id the host was given at `TurnStarted` and never learns a
        // replacement for, so it is never evicted; only the ids in between are bounded.
        if active.earlier_native_turn_ids.len() == EARLIER_NATIVE_TURN_IDS {
            active.earlier_native_turn_ids.remove(1);
        }
        active.earlier_native_turn_ids.push_back(previous);
        Ok(())
    }

    /// Reduces one announcement with the memory of the turn `route` still owns.
    ///
    /// A route whose turn has already been replaced or is finishing keeps the stateless reduction:
    /// its events can no longer reach a stream, but a malformed terminal must still poison.
    async fn reduce_for(&self, route: &ActiveTurnRoute, notification: &Notification) -> Outcome {
        let thread_id = self.thread_id();
        let native_turn_id = Some(route.native_turn_id.as_str());
        let observed_at = self.host.now();
        let mut turn = self.turn.lock().await;
        match turn
            .as_mut()
            .filter(|active| !active.finishing && Arc::ptr_eq(&active.owner, &route.owner))
        {
            Some(active) => active.reducer.reduce(
                notification,
                thread_id,
                native_turn_id,
                observed_at,
                tokio::time::Instant::now(),
            ),
            None => reducer::reduce_for_active_turn(
                notification,
                thread_id,
                native_turn_id,
                observed_at,
            ),
        }
    }

    /// Puts one event on the stream that still belongs to this start attempt.
    async fn emit_for(&self, route: &ActiveTurnRoute, kind: EventKind) -> Result<()> {
        let sink = {
            let turn = self.turn.lock().await;
            turn.as_ref()
                .filter(|active| !active.finishing && Arc::ptr_eq(&active.owner, &route.owner))
                .map(|active| active.sink.clone())
        };
        let Some(sink) = sink else {
            return Ok(());
        };
        sink.emit(kind).await
    }

    /// Claims a turn's terminal while retaining admission until it is committed.
    async fn claim_terminal(&self, owner: Option<&Arc<()>>) -> Option<TerminalClaim> {
        let mut turn = self.turn.lock().await;
        let active = turn.as_mut().filter(|active| {
            !active.finishing && owner.is_none_or(|owner| Arc::ptr_eq(&active.owner, owner))
        })?;
        active.finishing = true;
        Some(TerminalClaim {
            owner: Arc::clone(&active.owner),
            sink: active.sink.clone(),
            cancel_reason: active.cancel_reason,
        })
    }

    /// Releases admission only after the terminal claimed above is visible to the host.
    async fn release_terminal(&self, owner: &Arc<()>) {
        self.release_owned_turn(owner, true).await;
    }

    /// Releases a start that the app-server explicitly refused before it became a native turn.
    async fn release_refused_start(&self, owner: &Arc<()>) {
        self.release_owned_turn(owner, false).await;
    }

    /// Removes one owned admission slot, wakes every waiter on that owner and drops every
    /// request-id marker tied to it. Terminal commitment and explicit start refusal are the only
    /// two paths that may free admission without tearing down the whole session.
    async fn release_owned_turn(&self, owner: &Arc<()>, terminal_committed: bool) {
        let watchers = {
            let mut turn = self.turn.lock().await;
            let owns_expected_state = turn.as_ref().is_some_and(|active| {
                Arc::ptr_eq(&active.owner, owner) && active.finishing == terminal_committed
            });
            if !owns_expected_state {
                return;
            }
            let mut markers = self.resolution_markers.lock().await;
            markers
                .early
                .retain(|_, marker_owner| !Arc::ptr_eq(marker_owner, owner));
            markers
                .answered
                .retain(|marker| !Arc::ptr_eq(&marker.owner, owner));
            turn.take().map(|mut active| {
                (
                    active.abandonment_watcher.take(),
                    active.idle_watcher.take(),
                )
            })
        };
        self.turn_finished.notify_waiters();
        if let Some((abandonment, idle)) = watchers {
            if let Some(watcher) = abandonment {
                watcher.abort();
            }
            if let Some(watcher) = idle {
                watcher.abort();
            }
        }
    }

    /// Commits the terminal before releasing the matching admission slot.
    async fn commit_terminal(
        &self,
        claim: TerminalClaim,
        events: Vec<EventKind>,
        cancelled: Option<CancelReason>,
        failure: Option<VendorError>,
    ) {
        for event in events {
            let _ = claim.sink.emit(event).await;
        }
        let _ = match (failure, cancelled) {
            (Some(failure), _) => claim.sink.fail(failure).await,
            // The reason this side recorded wins over the server's bare "interrupted": a shutdown
            // is not a person pressing stop, and the host is entitled to know which it was.
            (None, Some(reason)) => {
                claim
                    .sink
                    .cancel(claim.cancel_reason.unwrap_or(reason))
                    .await
            }
            (None, None) => claim.sink.complete().await,
        };
        self.release_terminal(&claim.owner).await;
    }

    /// Ends the stream that still belongs to this start attempt.
    async fn finish_for(
        &self,
        route: &ActiveTurnRoute,
        completed_native_turn_id: Option<&str>,
        outcome: Outcome,
    ) {
        let Outcome::Finish {
            events,
            cancelled,
            failure,
        } = outcome
        else {
            return;
        };
        let Some(claim) = self.claim_terminal(Some(&route.owner)).await else {
            return;
        };
        self.remember_completed(
            completed_native_turn_id
                .filter(|native_turn_id| !native_turn_id.is_empty())
                .unwrap_or(&route.native_turn_id),
        )
        .await;
        self.commit_terminal(claim, events, cancelled, failure)
            .await;
    }

    /// Ends a turn because the host is shutting the session down.
    async fn cancel_active(&self, reason: CancelReason) {
        let Some(claim) = self.claim_terminal(None).await else {
            return;
        };
        let _ = claim.sink.cancel_on_close(reason).await;
        self.release_terminal(&claim.owner).await;
    }

    /// Fails the active stream after the peer disappears without a terminal notification.
    async fn connection_terminated(&self, termination: PeerTermination) {
        if !self.stop_new_work() {
            return;
        }
        self.release_pending_for(None, DecisionSource::Cancelled)
            .await;
        let Some(claim) = self.claim_terminal(None).await else {
            self.terminated.cancel();
            return;
        };
        let mut message =
            format!("Codex app-server connection ended while the turn was active: {termination}");
        if let Some(control) = self.control.get() {
            // A bounded, read-only probe for the diagnostic, not a reap: the teardown owner still
            // waits and reaps afterwards. It relies on `ProcessControl::wait` being repeatable and
            // cancel-safe, which the core's own `stop_process` already assumes by waiting after an
            // interrupt and again after the kill.
            if let Ok(Ok(status)) =
                tokio::time::timeout(std::time::Duration::from_millis(10), control.wait()).await
            {
                message.push_str(&format!(
                    "; exit code={}, signal={}",
                    status
                        .code
                        .map_or_else(|| String::from("unknown"), |code| code.to_string()),
                    status
                        .signal
                        .map_or_else(|| String::from("none"), |signal| signal.to_string())
                ));
            }
            let stderr = mango_external_agents::redact::stderr_text(&control.stderr_tail());
            if !stderr.is_empty() {
                message.push_str(&format!("; stderr: {stderr}"));
            }
        }
        self.terminated.cancel();
        let failure =
            VendorError::new(CALL_FAILED, message).with_vendor_code("connection-terminated", true);
        let _ = claim.sink.fail(failure).await;
        self.release_terminal(&claim.owner).await;
    }

    /// Fails the active turn under a bound and wakes the reaper after an unrecoverable protocol
    /// error. The connection cannot safely carry later work once routing has been lost.
    async fn poison(&self, failure: VendorError) {
        if !self.stop_new_work() {
            return;
        }
        if let Some(client) = self.client.get() {
            let _ = tokio::time::timeout(
                self.host.limits().kill_grace,
                self.cancel_owner(client, None, CancelReason::Shutdown),
            )
            .await;
        }
        self.release_pending_for(None, DecisionSource::Cancelled)
            .await;
        if let Some(claim) = self.claim_terminal(None).await {
            let _ = claim.sink.fail(failure).await;
            self.release_terminal(&claim.owner).await;
        }
        self.terminated.cancel();
    }

    /// Settles every question the server is still waiting on.
    ///
    /// Called when a turn is cancelled or a session closes. The waiting handler tasks answer with
    /// a refusal, which is what lets the server's own turn end instead of blocking forever.
    async fn release_pending_for(&self, owner: Option<&Arc<()>>, source: DecisionSource) {
        self.release_pending_approvals_for(owner, source).await;
        self.release_pending_questions_for(owner).await;
    }

    /// Settles every authority-bearing approval the server still awaits.
    async fn release_pending_approvals_for(&self, owner: Option<&Arc<()>>, source: DecisionSource) {
        let waiting = {
            let mut pending = self.pending.lock().await;
            let matching: Vec<InteractionId> = pending
                .iter()
                .filter(|(_, entry)| {
                    owner.is_none_or(|owner| Arc::ptr_eq(&entry.route.owner, owner))
                })
                .map(|(id, _)| id.clone())
                .collect();
            matching
                .into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect::<Vec<_>>()
        };
        self.signal_idle_change();
        let mut audits = Vec::with_capacity(waiting.len());
        for entry in waiting {
            let decision = entry.pending.refusal();
            let option_id = decision.option_id().to_owned();
            let _ = entry.answer.send(Answer::Chosen {
                decision,
                option_id: option_id.clone(),
                source,
                reported: true,
            });
            audits.push((
                entry.route,
                EventKind::ApprovalResolved {
                    interaction_id: entry.pending.request.id().clone(),
                    decision: ApprovalDecision::unresolved(option_id, source),
                },
            ));
        }
        for (route, event) in audits {
            let _ = self.emit_for(&route, event).await;
        }
    }

    /// Settles every question round the server is still waiting on.
    ///
    /// This function only ever runs as part of [`Shared::release_pending_for`]. An unanswered
    /// round is cancelled; a round whose host answer was accepted already keeps that outcome so
    /// it reaches the host before the caller commits its terminal.
    async fn release_pending_questions_for(&self, owner: Option<&Arc<()>>) {
        let _settlement = self.question_settlements.lock().await;
        self.release_pending_questions_for_locked(owner).await;
    }

    /// Settles question rounds while the caller already holds [`Shared::question_settlements`].
    async fn release_pending_questions_for_locked(&self, owner: Option<&Arc<()>>) {
        let waiting = {
            let mut pending = self.pending_questions.lock().await;
            let matching: Vec<InteractionId> = pending
                .iter()
                .filter(|(_, entry)| {
                    owner.is_none_or(|owner| Arc::ptr_eq(&entry.route.owner, owner))
                })
                .map(|(id, _)| id.clone())
                .collect();
            matching
                .into_iter()
                .filter_map(|id| pending.remove(&id))
                .collect::<Vec<_>>()
        };
        self.signal_idle_change();
        for entry in waiting {
            let outcome = entry.settlement.unwrap_or(QuestionOutcome::Cancelled);
            if let Some(answer) = entry.answer {
                let _ = answer.send(QuestionAnswer::Cancelled { reported: true });
            }
            if entry.announced {
                let _ = self
                    .emit_for(
                        &entry.route,
                        EventKind::QuestionResolved {
                            interaction_id: entry.request.interaction.id.clone(),
                            outcome,
                        },
                    )
                    .await;
            }
        }
    }

    /// Remembers an answer only until Codex confirms the server request ended.
    ///
    /// The confirmation can race the answer task between its pending-map removal and this call.
    /// In that case it already left an early marker, which this consumes instead of retaining a
    /// second marker. The acknowledgement ledger is bounded even if a future server omits the
    /// confirmation, so a long-running turn cannot retain an unbounded history of answers.
    async fn remember_answered_resolution(&self, request_key: &str, owner: &Arc<()>) {
        let turn = self.turn.lock().await;
        if !turn
            .as_ref()
            .is_some_and(|active| !active.finishing && Arc::ptr_eq(&active.owner, owner))
        {
            return;
        }
        let mut markers = self.resolution_markers.lock().await;
        #[cfg(test)]
        self.wait_for_resolution_marker_gate().await;
        if markers
            .early
            .remove(request_key)
            .is_some_and(|early_owner| Arc::ptr_eq(&early_owner, owner))
        {
            return;
        }
        markers.answered.retain(|marker| {
            marker.request_key != request_key || !Arc::ptr_eq(&marker.owner, owner)
        });
        let capacity = self.resolution_marker_capacity();
        while markers.answered.len() >= capacity {
            markers.answered.pop_front();
        }
        markers.answered.push_back(AnsweredResolution {
            request_key: request_key.to_owned(),
            owner: Arc::clone(owner),
        });
    }

    /// The answer ledger must remain bounded independently of active server requests.
    fn resolution_marker_capacity(&self) -> usize {
        self.host.limits().max_pending_requests.max(1)
    }

    #[cfg(test)]
    /// Holds a reply-marker transition only when a focused race test installed a gate.
    async fn wait_for_resolution_marker_gate(&self) {
        let gate = self.resolution_marker_gate.lock().await.clone();
        if let Some(gate) = gate {
            gate.wait_for_release().await;
        }
    }

    #[cfg(test)]
    /// Holds a question waiter after it accepted an answer and before it publishes the outcome.
    async fn wait_for_question_settlement_gate(&self) {
        let gate = self.question_settlement_gate.lock().await.clone();
        if let Some(gate) = gate {
            gate.wait_for_release().await;
        }
    }

    /// Takes a question only when the same turn that registered it still owns it.
    async fn take_pending_for(
        &self,
        request_id: &InteractionId,
        route: &ActiveTurnRoute,
    ) -> PendingTake {
        let mut pending = self.pending.lock().await;
        let Some(entry) = pending.remove(request_id) else {
            return PendingTake::Missing;
        };
        if Arc::ptr_eq(&entry.route.owner, &route.owner) {
            drop(pending);
            self.signal_idle_change();
            return PendingTake::Taken;
        }
        pending.insert(request_id.clone(), entry);
        PendingTake::Foreign
    }
}

/// What the app-server says, and what this client says back.
pub(crate) struct CodexHandler {
    shared: Arc<Shared>,
}

/// A round's questions, in the core's neutral shape.
///
/// `required` is a round-level fact on the wire (`isBlocking`), not a per-question one, so every
/// question in the round carries the same value.
fn to_questions(params: &ToolRequestUserInputParams) -> Vec<Question> {
    params
        .questions
        .iter()
        .map(|question| to_question(question, params.is_blocking))
        .collect()
}

/// One question, in the core's neutral shape.
///
/// `isOther` is deliberately not read here: the neutral contract has no arm for "one of these, or
/// write your own", so a round that sets it is presented as its declared choices and the extra
/// path is not advertised.
fn to_question(question: &ToolRequestUserInputQuestion, required: bool) -> Question {
    let form = match question
        .options
        .as_ref()
        .filter(|options| !options.is_empty())
    {
        Some(options) => QuestionForm::Choice {
            options: options.iter().map(to_question_option).collect(),
            multi_select: false,
        },
        None => QuestionForm::FreeText { placeholder: None },
    };
    let built = Question::new(
        QuestionId::new(question.id.clone()),
        question.question.clone(),
        form,
    )
    .with_detail(question.header.clone());
    if required { built.required() } else { built }
}

/// One choice, in the core's neutral shape.
///
/// The vendor gives a choice no id of its own — only a label — so the label is what this harness
/// offers as the option's native identity, and what an answer echoes back.
fn to_question_option(option: &ToolRequestUserInputOption) -> QuestionOption {
    let built = QuestionOption::new(QuestionOptionId::new(option.label.clone()))
        .with_label(option.label.clone());
    match option.description.as_deref() {
        Some(description) => built.with_description(description),
        None => built,
    }
}

/// A host's answers, in the shape `item/tool/requestUserInput` takes on the wire.
fn to_wire_answers(response: &QuestionResponse) -> ToolRequestUserInputResponse {
    let answers = response
        .answers
        .iter()
        .map(|answer| {
            let values = match &answer.value {
                mango_external_agents::interaction::AnswerValue::Chosen { option_ids } => {
                    option_ids.iter().map(ToString::to_string).collect()
                }
                mango_external_agents::interaction::AnswerValue::Text { text } => {
                    vec![text.clone()]
                }
                mango_external_agents::interaction::AnswerValue::Declined => Vec::new(),
                // `AnswerValue` is `#[non_exhaustive]`: an arm the core adds later is refused
                // elsewhere (`QuestionRequest::validate` rejects an answer shape a question never
                // declared), so nothing reaches this match that is not one of the three above.
                _ => Vec::new(),
            };
            (
                answer.question_id.to_string(),
                ToolRequestUserInputAnswer { answers: values },
            )
        })
        .collect();
    ToolRequestUserInputResponse { answers }
}

#[async_trait::async_trait]
impl PeerHandler for CodexHandler {
    async fn on_notification(&self, method: String, params: Value) {
        let routed_here = params
            .get("threadId")
            .and_then(Value::as_str)
            .is_some_and(|thread| thread == self.shared.thread_id());
        let live = {
            let mut hold = self
                .shared
                .steer_hold
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if hold.is_holding() {
                // Held frames left the connection's byte budget when this call returns, so the
                // hold counts them against the turn's own.
                let bytes =
                    method.len() + serde_json::to_string(&params).map_or(0, |raw| raw.len());
                let budget = self.shared.host.limits().turn_buffer_bytes;
                if hold.held.len() >= STEER_HOLD_LIMIT
                    || hold.held_bytes.saturating_add(bytes) > budget
                {
                    None
                } else {
                    hold.held_bytes += bytes;
                    hold.held.push_back((method, params, bytes));
                    Some(None)
                }
            } else {
                Some(Some((method, params)))
            }
        };
        match live {
            Some(Some((method, params))) => self.handle_notification(method, params).await,
            Some(None) => {
                // Queued progress for this conversation is still progress: a slow steer answer
                // must not let the idle deadline stop a turn that is working.
                if routed_here {
                    self.shared.signal_idle_change();
                }
            }
            None => {
                self.shared
                    .poison(VendorError::new(
                        reducer::PROTOCOL_ERROR,
                        "expected notifications held during a steer to fit the turn's count and byte budget, received more",
                    ))
                    .await;
            }
        }
    }

    async fn on_request(
        &self,
        method: String,
        params: Value,
        id: RequestId,
    ) -> ServerRequestOutcome {
        let request = ServerRequest::parse(&method, params);

        if let Some(refusal) = request.refusal() {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32601,
                message: refusal.message().to_owned(),
                data: None,
            });
        }

        // Another conversation's approval: a subagent's thread rides this same connection. It is
        // refused rather than put to this session's host, who is not being asked.
        if request
            .thread_id()
            .is_some_and(|thread| thread != self.shared.thread_id())
        {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32602,
                message: format!(
                    "expected an approval for thread {}, received one for another thread",
                    self.shared.thread_id()
                ),
                data: None,
            });
        }

        // An MCP elicitation is an arbitrary JSON-schema form this library does not render — see
        // `UnsupportedQuestion::ArbitraryForm` and the scope note in `docs/contracts.md`. Answered
        // immediately and natively: no pending registration, no broker, no `QuestionAsked` — a
        // form is never put to a host.
        //
        // Answered ahead of the turn-correlation gates below rather than after them. At this pin
        // the `url` branch carries no `turnId` at all, so those gates would answer the one family
        // whose whole point is not receiving a JSON-RPC error with exactly that error, and the
        // server would be told this client is broken rather than that its form was declined.
        if let ServerRequest::McpElicitation(params) = &request {
            return self.decline_elicitation(params, &id).await;
        }

        let Some(route) = self.shared.active_turn_route().await else {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32602,
                message: String::from("expected an active turn for an approval request"),
                data: None,
            });
        };
        if !self.shared.approval_route_is_admissible(&route).await {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32602,
                message: String::from(
                    "expected an active turn accepting approvals, received a stopped turn",
                ),
                data: None,
            });
        }
        let Some(request_turn_id) = request.turn_id().filter(|turn_id| !turn_id.is_empty()) else {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32602,
                message: String::from("expected an approval request with a non-empty turn id"),
                data: None,
            });
        };
        if !route.native_turn_id.is_empty() && request_turn_id != route.native_turn_id {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32602,
                message: format!(
                    "expected an approval for active turn {}, received one for {}",
                    route.native_turn_id, request_turn_id
                ),
                data: None,
            });
        }
        if route.native_turn_id.is_empty() && self.shared.recently_completed(request_turn_id).await
        {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32602,
                message: String::from(
                    "expected an approval for the pending start, received one for a completed turn",
                ),
                data: None,
            });
        }

        // One read, reused below: a second `host.now()` call to build the deadline would let a
        // host clock that moved backward between the two reads extend the monotonic approval
        // window past what `expires_at` advertised.
        let now = self.shared.host.now();
        let expires_at = match self.shared.host.limits().approval_expires_at(now) {
            Ok(expires_at) => expires_at,
            Err(error) => {
                return ServerRequestOutcome::Failure(JsonRpcError {
                    code: -32602,
                    message: error.to_string(),
                    data: None,
                });
            }
        };

        if let ServerRequest::RequestUserInput(params) = &request {
            return match self
                .decide_question(params, &id, route.clone(), now, expires_at)
                .await
            {
                Some(answer) => {
                    self.shared
                        .remember_answered_resolution(&id.key(), &route.owner)
                        .await;
                    ServerRequestOutcome::Answer(
                        serde_json::to_value(answer)
                            .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
                    )
                }
                // The server already stopped waiting, so the frame is discarded on its side.
                None => ServerRequestOutcome::Answer(Value::Object(serde_json::Map::new())),
            };
        }

        let Some(pending) = approvals::to_request(
            &request,
            route.operation(self.shared.session_id.clone()),
            expires_at,
        ) else {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32601,
                message: String::from("expected an approval this client can put to a person"),
                data: None,
            });
        };

        match self.decide(pending, &id, route.clone(), now).await {
            Some(answer) => {
                self.shared
                    .remember_answered_resolution(&id.key(), &route.owner)
                    .await;
                ServerRequestOutcome::Answer(answer.to_wire())
            }
            // The server already stopped waiting, so the frame is discarded on its side. Something
            // has to be returned, and a refusal is the answer that grants nothing.
            None => ServerRequestOutcome::Answer(Value::Object(serde_json::Map::new())),
        }
    }

    async fn on_terminated(&self, termination: PeerTermination) {
        self.shared.connection_terminated(termination).await;
    }
}

impl CodexHandler {
    /// Routes one notification now, outside any steer hold.
    async fn handle_notification(&self, method: String, params: Value) {
        let notification = Notification::parse(&method, params);

        // The server resolved one of its own questions — an interrupt did it, or a policy of its
        // own answered first. Releasing the waiter is what keeps the task composing a reply, and
        // its share of this session, from outliving the question.
        if let Notification::ServerRequestResolved(resolved) = &notification
            && resolved.thread_id == self.shared.thread_id()
        {
            let route = self.shared.active_turn_route().await;
            if let Some(route) = route {
                let released = self
                    .release_resolved(&RequestId::new(resolved.request_id.clone()).key(), &route)
                    .await;
                if !released {
                    self.shared
                            .poison(VendorError::new(
                                reducer::PROTOCOL_ERROR,
                                "expected room for a serverRequest/resolved tombstone, received a full bounded set",
                            ))
                            .await;
                    return;
                }
            }
        }

        // Quota belongs to the account and arrives whether or not a turn is running. It is merged
        // into the session's baseline rather than shown as the partial snapshot it may be.
        if let Notification::RateLimits(update) = &notification {
            self.shared.rate_limits_updated(&update.rate_limits).await;
            return;
        }

        let active_route = self.shared.active_turn_route().await;
        let Some(mut active_route) = active_route else {
            return;
        };
        if active_route.native_turn_id.is_empty()
            && notification.requires_native_turn_match()
            && let Some(turn_id) = notification.turn_id()
            && self.shared.recently_completed(turn_id).await
        {
            return;
        }

        // The vendor's own turn id can arrive on a notification before `turn/start` answers.
        // Whichever side learns it first announces the turn as accepted, exactly once, before
        // anything else reaches the host on this attempt's stream.
        //
        // Gated on `requires_native_turn_match()`, the same test the reducer itself uses to
        // decide whether a family's id can be trusted — which excludes `turn/started` on
        // purpose. Captured review transcripts show its id can differ from `review/start`'s own
        // response and from every later item and completion for the same review; claiming from
        // it here would announce a review under an id nothing else on its stream agrees with.
        if active_route.native_turn_id.is_empty()
            && notification.requires_native_turn_match()
            && let Some(turn_id) = notification.turn_id()
        {
            match self
                .shared
                .claim_announcement(&active_route.owner, turn_id)
                .await
            {
                Ok(Some((sink, native_turn_id))) => {
                    active_route.native_turn_id.clone_from(&native_turn_id);
                    if sink
                        .emit(EventKind::TurnStarted { native_turn_id })
                        .await
                        .is_err()
                    {
                        self.shared
                            .poison(VendorError::new(
                                reducer::PROTOCOL_ERROR,
                                "expected a host stream that accepts an accepted turn announcement",
                            ))
                            .await;
                        return;
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    self.shared
                        .poison(VendorError::new(
                            reducer::PROTOCOL_ERROR,
                            "expected an app-server notification with a usable native turn id",
                        ))
                        .await;
                    return;
                }
            }
        }

        // Idle accounting measures *this* turn's silence. A subagent's thread, a detached review,
        // another turn of this thread and an account-level quota update all ride the same
        // connection, and resetting the deadline for them would let steady foreign traffic keep a
        // genuinely hung turn alive for as long as the connection lasts. Routed, not reduced: a
        // frame can belong to this turn and still render nothing, as `turn/started` and a retrying
        // `error` do, and withholding the reset for those would time out a turn that is working.
        if reducer::routes_to_active_turn(
            &notification,
            self.shared.thread_id(),
            Some(active_route.native_turn_id.as_str()),
        ) {
            self.shared.signal_idle_change();
        }

        let outcome = self.shared.reduce_for(&active_route, &notification).await;
        match outcome {
            Outcome::Emit(events) => {
                for event in events {
                    let emitted = if notification.requires_native_turn_match() {
                        self.shared.emit_for(&active_route, event).await
                    } else {
                        self.shared.emit(event).await
                    };
                    if matches!(emitted, Err(Error::LimitExceeded { .. })) {
                        self.shared
                            .poison(VendorError::new(
                                CALL_FAILED,
                                "expected bounded Codex transcript publication, received overflow",
                            ))
                            .await;
                        return;
                    }
                }
            }
            Outcome::Finish { .. } => {
                // Whatever the server was still asking is moot: its turn is over, and a question
                // belonging to a finished turn is one nobody will be shown.
                //
                // Hold question settlement ownership through both the drain and terminal claim.
                // A request task that already routed to this turn then observes `finishing`
                // before it can register a new host-visible round after this drain.
                let _settlement = self.shared.question_settlements.lock().await;
                self.shared
                    .release_pending_approvals_for(
                        Some(&active_route.owner),
                        DecisionSource::Cancelled,
                    )
                    .await;
                self.shared
                    .release_pending_questions_for_locked(Some(&active_route.owner))
                    .await;
                self.shared
                    .finish_for(&active_route, notification.turn_id(), outcome)
                    .await;
            }
            Outcome::Poison { failure } => {
                self.shared.poison(failure).await;
            }
            Outcome::Ignore => {}
        }
    }

    /// Puts one question to whoever answers, and waits.
    ///
    /// Returns `None` when the server resolved the question itself while this was waiting.
    async fn decide(
        &self,
        pending: PendingApproval,
        id: &RequestId,
        route: ActiveTurnRoute,
        now: SystemTime,
    ) -> Option<ServerAnswer> {
        let request = pending.request.clone();
        let request_id = request.id().clone();
        if !self.shared.approval_route_is_admissible(&route).await {
            return Some(pending.refusal());
        }
        // Translate the request's original wall-clock deadline once. Every later await must share
        // it, otherwise a full event channel or a slow broker would restart the host's timer. `now`
        // is the same read `to_request` stamped `expires_at` from, not a fresh one: a second host
        // clock read here could drift from the first and stretch the window past what
        // `expires_at` advertised.
        let Some(deadline) = ApprovalDeadline::new(request.expires_at(), now) else {
            return Some(pending.refusal());
        };

        // A request the core refuses to bound is a request nothing could render, and refusing it
        // here is better than a prompt nobody sees behind a turn that waits.
        let bounded = match request.clone().normalized() {
            Ok(bounded) => bounded,
            Err(_) => return Some(pending.refusal()),
        };

        // Registered before it is announced, and that order is the whole point. The host learns
        // this question's id from the event, so a host that answers the moment it sees one would
        // otherwise find nothing waiting: `respond` would refuse, the host would have spent its
        // only handle on a protocol error, and the server would stay blocked until the deadline
        // declined on its behalf.
        let (answer, mut waiting) = oneshot::channel();
        {
            let mut pending_entries = self.shared.pending.lock().await;
            if pending_entries.contains_key(&request_id) {
                return Some(pending.refusal());
            }
            if pending_entries.len() >= self.shared.host.limits().max_pending_requests {
                return Some(pending.refusal());
            }
            pending_entries.insert(
                request_id.clone(),
                PendingEntry {
                    request_key: id.key(),
                    route: route.clone(),
                    pending: pending.clone(),
                    deadline,
                    answer,
                },
            );
        }
        // `cancel_owner` latches cancellation under the turn lock before draining pending
        // questions. Re-check after registration so a request that raced that drain cannot be
        // left behind to a fast broker that would otherwise grant it.
        if !self.shared.approval_route_is_admissible(&route).await {
            let removed = self.shared.pending.lock().await.remove(&request_id);
            if removed
                .as_ref()
                .is_some_and(|entry| Arc::ptr_eq(&entry.route.owner, &route.owner))
            {
                let decision = pending.refusal();
                self.resolved(
                    &route,
                    &request_id,
                    ApprovalDecision::unresolved(decision.option_id(), DecisionSource::Cancelled),
                )
                .await;
            }
            self.shared.signal_idle_change();
            return Some(pending.refusal());
        }
        self.shared.signal_idle_change();

        // And the server may already have stopped waiting, in a race this side cannot see from
        // the outside: the release is read off the same pipe and runs beside this task.
        let request_key = id.key();
        let early_route = self
            .shared
            .resolution_markers
            .lock()
            .await
            .early
            .remove(&request_key);
        if let Some(early_route) = early_route
            && Arc::ptr_eq(&early_route, &route.owner)
        {
            let removed = {
                let mut pending_entries = self.shared.pending.lock().await;
                if pending_entries
                    .get(&request_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.route.owner, &route.owner))
                {
                    pending_entries.remove(&request_id);
                    true
                } else {
                    false
                }
            };
            if removed {
                self.shared.signal_idle_change();
            }
            return None;
        }

        // `serverRequest/resolved` can land between consuming an early marker and this point. It
        // takes the pending entry and wakes `waiting`; do not announce a question the server has
        // already withdrawn.
        if !self
            .shared
            .pending
            .lock()
            .await
            .get(&request_id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.route.owner, &route.owner))
        {
            return None;
        }

        // The deadline is created before publication, so bounded publication cannot restart the
        // decision window while the host catches up.
        let emitted = self
            .shared
            .emit_for(
                &route,
                EventKind::ApprovalRequested {
                    request: bounded.clone(),
                },
            )
            .await;
        if matches!(emitted, Err(Error::LimitExceeded { .. })) {
            self.shared
                .poison(VendorError::new(
                    CALL_FAILED,
                    "expected room for a bounded Codex approval, received transcript overflow",
                ))
                .await;
            return Some(pending.refusal());
        }

        let broker = deadline.run(broker_response(self.shared.host.broker(), &bounded));
        tokio::pin!(broker);
        let broker_wait = tokio::select! {
            biased;
            answer = &mut waiting => BrokerWait::Answer(answer.ok()),
            response = &mut broker => BrokerWait::Broker(response),
        };

        match broker_wait {
            BrokerWait::Answer(Some(answer)) => {
                return self
                    .settle_answer(answer, &pending, &route, &request_id)
                    .await;
            }
            BrokerWait::Answer(None) => {
                let PendingTake::Taken = self.shared.take_pending_for(&request_id, &route).await
                else {
                    return None;
                };
                return Some(self.expire(&pending, &route, &request_id).await);
            }
            BrokerWait::Broker(None) => {
                match self.shared.take_pending_for(&request_id, &route).await {
                    PendingTake::Taken => {
                        return Some(self.expire(&pending, &route, &request_id).await);
                    }
                    // `respond` claims the map entry before sending its answer. The expiry race must
                    // wait for that already-accepted result instead of turning it into an empty reply.
                    PendingTake::Missing => {
                        let answer = waiting.await.ok()?;
                        return self
                            .settle_answer(answer, &pending, &route, &request_id)
                            .await;
                    }
                    PendingTake::Foreign => return None,
                }
            }
            BrokerWait::Broker(Some(None)) => {}
            BrokerWait::Broker(Some(Some(response))) => {
                // Only if nobody answered first. Taking the entry back is what says so — a policy that
                // deliberated while a person chose does not get to overrule them.
                if matches!(
                    self.shared.take_pending_for(&request_id, &route).await,
                    PendingTake::Taken
                ) {
                    // A policy answering with an id this question never offered is a policy that
                    // cannot be applied here. Refusing is the answer that grants nothing; sending the
                    // id on would have the server refuse a frame a person already thinks was answered.
                    let decision = pending
                        .decision_for(&response.option_id)
                        .unwrap_or_else(|| pending.refusal());
                    let option_id = decision.option_id().to_owned();
                    return self
                        .settle_answer(
                            Answer::Chosen {
                                decision,
                                option_id,
                                source: response.source,
                                reported: false,
                            },
                            &pending,
                            &route,
                            &request_id,
                        )
                        .await;
                }
            }
        }

        // Three ways this ends: somebody chooses, the deadline the request already carries passes,
        // or the host is going away. All three answer the server; none of them grants anything.
        let settled = tokio::select! {
            biased;
            answer = waiting => answer.ok(),
            () = self.shared.host.cancel().cancelled() => None,
            () = deadline.wait() => None,
        };

        let removed = self.shared.pending.lock().await.remove(&request_id);
        if removed
            .as_ref()
            .is_some_and(|entry| !Arc::ptr_eq(&entry.route.owner, &route.owner))
        {
            if let Some(entry) = removed {
                self.shared
                    .pending
                    .lock()
                    .await
                    .insert(request_id.clone(), entry);
            }
            return None;
        }
        match settled {
            Some(answer) => {
                self.settle_answer(answer, &pending, &route, &request_id)
                    .await
            }
            None => {
                // Nobody chose: the shared deadline elapsed, or the host is going away.
                let decision = pending.refusal();
                let source = if self.shared.host.cancel().is_cancelled() {
                    DecisionSource::Cancelled
                } else {
                    DecisionSource::Expired
                };
                if removed.is_some() {
                    self.resolved(
                        &route,
                        &request_id,
                        ApprovalDecision::unresolved(decision.option_id(), source),
                    )
                    .await;
                }
                Some(decision)
            }
        }
    }

    /// Records the answer that won while the broker was still deliberating.
    async fn settle_answer(
        &self,
        answer: Answer,
        pending: &PendingApproval,
        route: &ActiveTurnRoute,
        request_id: &InteractionId,
    ) -> Option<ServerAnswer> {
        match answer {
            Answer::Chosen {
                decision,
                option_id,
                source,
                reported,
            } => {
                if !self.shared.approval_route_is_admissible(route).await {
                    let refusal = pending.refusal();
                    if !reported {
                        self.resolved(
                            route,
                            request_id,
                            ApprovalDecision::unresolved(
                                refusal.option_id(),
                                DecisionSource::Cancelled,
                            ),
                        )
                        .await;
                    }
                    return Some(refusal);
                }
                if !reported {
                    // A real choice: reported with the reach and risk of the option that won,
                    // when this harness offered one by that id.
                    let audited = match pending.option(&option_id) {
                        Some(option) => ApprovalDecision::from_option(option, source),
                        None => ApprovalDecision::unresolved(option_id, source),
                    };
                    self.resolved(route, request_id, audited).await;
                }
                Some(decision)
            }
            Answer::ResolvedByTheServer => {
                // Nobody here chose; the server simply stopped waiting. `cancel` is a label for
                // the audit trail, not an option a person or a policy picked.
                self.resolved(
                    route,
                    request_id,
                    ApprovalDecision::unresolved("cancel", DecisionSource::Cancelled),
                )
                .await;
                None
            }
        }
    }

    /// Refuses an expired question and records the expiry before replying to the app-server.
    async fn expire(
        &self,
        pending: &PendingApproval,
        route: &ActiveTurnRoute,
        request_id: &InteractionId,
    ) -> ServerAnswer {
        let decision = pending.refusal();
        // The deadline chose this, not a person or a policy — the audit trail says so.
        self.resolved(
            route,
            request_id,
            ApprovalDecision::unresolved(decision.option_id(), DecisionSource::Expired),
        )
        .await;
        decision
    }

    /// Declines an MCP elicitation natively, whether or not it correlates to a turn.
    ///
    /// The wire answer is unconditional: a form this library will not render must never leave the
    /// app-server waiting on a JSON-RPC error, and a missing correlation is not a reason to send
    /// one. The audit event is conditional, because it needs a turn to be reported on — an
    /// elicitation that names another turn, or arrives outside one, is declined and not recorded.
    async fn decline_elicitation(
        &self,
        params: &McpServerElicitationRequestParams,
        id: &RequestId,
    ) -> ServerRequestOutcome {
        if let Some(route) = self.elicitation_route(params).await {
            let _ = self
                .shared
                .emit_for(
                    &route,
                    EventKind::QuestionResolved {
                        interaction_id: InteractionId::new(
                            params
                                .elicitation_id
                                .clone()
                                .unwrap_or_else(|| id.key().to_string()),
                        ),
                        outcome: QuestionOutcome::Refused {
                            reason: UnsupportedQuestion::ArbitraryForm,
                        },
                    },
                )
                .await;
            // No pending entry was ever registered for this answer, so a later
            // `serverRequest/resolved` for it would otherwise find nothing in either map and
            // spend an early-resolution marker on a question already settled.
            self.shared
                .remember_answered_resolution(&id.key(), &route.owner)
                .await;
        }
        ServerRequestOutcome::Answer(
            serde_json::to_value(McpServerElicitationRequestResponse::decline())
                .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
        )
    }

    /// The turn an elicitation's refusal is recorded on, when there is one it belongs to.
    ///
    /// An absent or empty `turnId` is a correlation the server did not offer, not a failed one:
    /// the active turn is the only one the form can belong to, so it is reported there. A
    /// `turnId` that names a different turn is somebody else's, and reports nowhere.
    async fn elicitation_route(
        &self,
        params: &McpServerElicitationRequestParams,
    ) -> Option<ActiveTurnRoute> {
        let route = self.shared.active_turn_route().await?;
        if !self.shared.approval_route_is_admissible(&route).await {
            return None;
        }
        let Some(turn_id) = params
            .turn_id
            .as_deref()
            .filter(|turn_id| !turn_id.is_empty())
        else {
            return Some(route);
        };
        if !route.native_turn_id.is_empty() {
            return (turn_id == route.native_turn_id).then_some(route);
        }
        // The turn has not been told its native id yet, so the only thing that disqualifies this
        // one is naming a turn that already ended.
        let completed = self.shared.recently_completed(turn_id).await;
        (!completed).then_some(route)
    }

    async fn resolved(
        &self,
        route: &ActiveTurnRoute,
        request_id: &InteractionId,
        decision: ApprovalDecision,
    ) {
        self.shared.signal_idle_change();
        let _ = self
            .shared
            .emit_for(
                route,
                EventKind::ApprovalResolved {
                    interaction_id: request_id.clone(),
                    decision,
                },
            )
            .await;
    }

    /// Puts one round of questions to a host, and waits.
    ///
    /// Mirrors [`CodexHandler::decide`] without the broker race: a question grants no authority,
    /// so a [`PermissionBroker`](mango_external_agents::PermissionBroker) is never consulted about
    /// one. Returns `None` when the server resolved the round itself while this was waiting.
    async fn decide_question(
        &self,
        params: &ToolRequestUserInputParams,
        id: &RequestId,
        route: ActiveTurnRoute,
        now: SystemTime,
        expires_at: SystemTime,
    ) -> Option<ToolRequestUserInputResponse> {
        let request_id = InteractionId::new(params.item_id.clone());

        // A round asking for a credential is refused whole, and no part of it is ever put to a
        // host: a password typed into a box labelled "answer" is a password in a host's
        // transcript.
        if params.questions.iter().any(|question| question.is_secret) {
            self.question_resolved(
                &route,
                &request_id,
                QuestionOutcome::Refused {
                    reason: UnsupportedQuestion::SecretCollection,
                },
            )
            .await;
            return Some(ToolRequestUserInputResponse::none());
        }

        if !self.shared.approval_route_is_admissible(&route).await {
            return Some(ToolRequestUserInputResponse::none());
        }
        // Shared with `decide`: every later await must reuse this deadline, or a full event
        // channel would restart the host's timer.
        let Some(deadline) = ApprovalDeadline::new(expires_at, now) else {
            return Some(ToolRequestUserInputResponse::none());
        };

        let interaction = Interaction::new(
            request_id.clone(),
            InteractionKind::Question,
            self.shared.session_id.clone(),
            expires_at,
        )
        .during(route.operation(self.shared.session_id.clone()));
        let request = QuestionRequest::new(interaction, to_questions(params));
        // A round the core refuses to bound is a round nobody can render, refused on its own
        // rather than behind a turn that waits.
        let bounded = match request.normalized() {
            Ok(bounded) => bounded,
            Err(_) => return Some(ToolRequestUserInputResponse::none()),
        };

        // Registered before it is announced, for the same reason as `decide`: a host answering the
        // instant it sees the event must find something waiting.
        let (answer, waiting) = oneshot::channel();
        {
            let mut pending = self.shared.pending_questions.lock().await;
            if pending.contains_key(&request_id) {
                return Some(ToolRequestUserInputResponse::none());
            }
            if pending.len() >= self.shared.host.limits().max_pending_requests {
                return Some(ToolRequestUserInputResponse::none());
            }
            pending.insert(
                request_id.clone(),
                PendingQuestion {
                    request_key: id.key(),
                    route: route.clone(),
                    request: bounded.clone(),
                    deadline,
                    answer: Some(answer),
                    settlement: None,
                    announced: false,
                },
            );
        }
        if !self.shared.approval_route_is_admissible(&route).await {
            let _settlement = self.shared.question_settlements.lock().await;
            let removed = self
                .shared
                .pending_questions
                .lock()
                .await
                .remove(&request_id);
            if let Some(entry) = removed
                && entry.announced
                && Arc::ptr_eq(&entry.route.owner, &route.owner)
            {
                self.question_resolved(
                    &route,
                    &request_id,
                    entry.settlement.unwrap_or(QuestionOutcome::Cancelled),
                )
                .await;
            }
            self.shared.signal_idle_change();
            return Some(ToolRequestUserInputResponse::none());
        }
        self.shared.signal_idle_change();

        // The server may already have stopped waiting, in a race this side cannot see from the
        // outside: the release is read off the same pipe and runs beside this task.
        let request_key = id.key();
        let early_route = self
            .shared
            .resolution_markers
            .lock()
            .await
            .early
            .remove(&request_key);
        if let Some(early_route) = early_route
            && Arc::ptr_eq(&early_route, &route.owner)
        {
            let _settlement = self.shared.question_settlements.lock().await;
            let removed = {
                let mut pending = self.shared.pending_questions.lock().await;
                if pending
                    .get(&request_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.route.owner, &route.owner))
                {
                    pending.remove(&request_id);
                    true
                } else {
                    false
                }
            };
            if removed {
                self.shared.signal_idle_change();
            }
            return None;
        }
        let emitted = {
            // A server withdrawal cannot take this entry after the question reaches the stream
            // but before it is marked announced. That makes the withdrawing side the owner of
            // the matching host resolution instead of leaving a dialog behind.
            let _settlement = self.shared.question_settlements.lock().await;
            if !self.shared.approval_route_is_admissible(&route).await {
                self.shared
                    .pending_questions
                    .lock()
                    .await
                    .remove(&request_id);
                return None;
            }
            if !self
                .shared
                .pending_questions
                .lock()
                .await
                .get(&request_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.route.owner, &route.owner))
            {
                return None;
            }

            let emitted = self
                .shared
                .emit_for(
                    &route,
                    EventKind::QuestionAsked {
                        request: bounded.clone(),
                    },
                )
                .await;
            if emitted.is_ok()
                && let Some(entry) = self
                    .shared
                    .pending_questions
                    .lock()
                    .await
                    .get_mut(&request_id)
                && Arc::ptr_eq(&entry.route.owner, &route.owner)
            {
                entry.announced = true;
            }
            emitted
        };
        if matches!(emitted, Err(Error::LimitExceeded { .. })) {
            self.shared
                .poison(VendorError::new(
                    CALL_FAILED,
                    "expected room for a bounded Codex question, received transcript overflow",
                ))
                .await;
            return Some(ToolRequestUserInputResponse::none());
        }

        // Three ways this ends: somebody answers, the deadline the request already carries
        // passes, or the host is going away. None of them grant anything.
        let settled = tokio::select! {
            biased;
            answer = waiting => answer.ok(),
            () = self.shared.host.cancel().cancelled() => None,
            () = deadline.wait() => None,
        };

        #[cfg(test)]
        self.shared.wait_for_question_settlement_gate().await;

        // Serialise this removal and publication with terminal cleanup. Once either side takes
        // the entry, it also puts its resolution on the stream before the terminal can claim it.
        let _settlement = self.shared.question_settlements.lock().await;
        let removed = self
            .shared
            .pending_questions
            .lock()
            .await
            .remove(&request_id);
        if removed
            .as_ref()
            .is_some_and(|entry| !Arc::ptr_eq(&entry.route.owner, &route.owner))
        {
            if let Some(entry) = removed {
                self.shared
                    .pending_questions
                    .lock()
                    .await
                    .insert(request_id.clone(), entry);
            }
            return None;
        }

        match settled {
            Some(QuestionAnswer::Answered(response)) => {
                if removed.is_some() {
                    self.question_resolved(
                        &route,
                        &request_id,
                        QuestionOutcome::Answered {
                            answers: response.answers.clone(),
                        },
                    )
                    .await;
                }
                Some(to_wire_answers(&response))
            }
            Some(QuestionAnswer::Expired) => {
                if removed.is_some() {
                    self.question_resolved(&route, &request_id, QuestionOutcome::Expired)
                        .await;
                }
                Some(ToolRequestUserInputResponse::none())
            }
            Some(QuestionAnswer::Cancelled { reported }) => {
                if !reported {
                    self.question_resolved(&route, &request_id, QuestionOutcome::Cancelled)
                        .await;
                }
                Some(ToolRequestUserInputResponse::none())
            }
            Some(QuestionAnswer::ResolvedByTheServer { reported }) => {
                if !reported {
                    self.question_resolved(&route, &request_id, QuestionOutcome::Cancelled)
                        .await;
                }
                None
            }
            None => {
                // Nobody answered: the oneshot was not ready when the shared deadline elapsed or
                // the host started going away. `select!` above is biased toward `waiting`, so a
                // value that had already landed on the oneshot — an answer, or the `Cancelled`
                // that `release_pending_for` sends ahead of a shutdown — would have won that arm
                // instead; its `reported` bit is what keeps that path from publishing twice.
                //
                // Reported only by the waiter that still owned the round: an empty slot means
                // somebody else already took this round and published for it. A round the host
                // answered in the same window is likewise not this task's to report — saying
                // nothing is better than publishing `Cancelled` over an answer that landed.
                if removed.is_some() {
                    let outcome = if self.shared.host.cancel().is_cancelled() {
                        QuestionOutcome::Cancelled
                    } else {
                        QuestionOutcome::Expired
                    };
                    self.question_resolved(&route, &request_id, outcome).await;
                }
                Some(ToolRequestUserInputResponse::none())
            }
        }
    }

    async fn question_resolved(
        &self,
        route: &ActiveTurnRoute,
        request_id: &InteractionId,
        outcome: QuestionOutcome,
    ) {
        self.shared.signal_idle_change();
        let _ = self
            .shared
            .emit_for(
                route,
                EventKind::QuestionResolved {
                    interaction_id: request_id.clone(),
                    outcome,
                },
            )
            .await;
    }

    /// Releases the question the server says it is no longer waiting on.
    async fn release_resolved(&self, request_key: &str, route: &ActiveTurnRoute) -> bool {
        let entry = {
            let mut pending = self.shared.pending.lock().await;
            let id = pending
                .iter()
                .find(|(_, entry)| {
                    entry.request_key == request_key
                        && Arc::ptr_eq(&entry.route.owner, &route.owner)
                })
                .map(|(id, _)| id.clone());
            id.and_then(|id| pending.remove(&id))
        };
        if let Some(entry) = entry {
            let _ = entry.answer.send(Answer::ResolvedByTheServer);
            self.shared.signal_idle_change();
            return true;
        }

        let question_settlement = self.shared.question_settlements.lock().await;
        let question_entry = {
            let mut pending = self.shared.pending_questions.lock().await;
            let id = pending
                .iter()
                .find(|(_, entry)| {
                    entry.request_key == request_key
                        && Arc::ptr_eq(&entry.route.owner, &route.owner)
                })
                .map(|(id, _)| id.clone());
            id.and_then(|id| pending.remove(&id))
        };
        if let Some(entry) = question_entry {
            if entry.announced {
                let outcome = entry.settlement.unwrap_or(QuestionOutcome::Cancelled);
                self.question_resolved(&entry.route, &entry.request.interaction.id, outcome)
                    .await;
            }
            if let Some(answer) = entry.answer {
                let _ = answer.send(QuestionAnswer::ResolvedByTheServer {
                    reported: entry.announced,
                });
            }
            drop(question_settlement);
            self.shared.signal_idle_change();
            return true;
        }
        drop(question_settlement);

        // Nothing registered or answered under it yet. Either this is somebody else's question,
        // or the task that will register it has not reached the map — and it checks the marker
        // after registration rather than waiting out a deadline on a question already withdrawn.
        // Keeping the turn lock while classifying the marker means terminal cleanup cannot clear
        // this owner and a late notification cannot recreate its marker afterwards.
        let turn = self.shared.turn.lock().await;
        if !turn
            .as_ref()
            .is_some_and(|active| !active.finishing && Arc::ptr_eq(&active.owner, &route.owner))
        {
            return true;
        }
        let mut markers = self.shared.resolution_markers.lock().await;
        if let Some(position) = markers.answered.iter().position(|marker| {
            marker.request_key == request_key && Arc::ptr_eq(&marker.owner, &route.owner)
        }) {
            let _ = markers.answered.remove(position);
            return true;
        }
        if let Some(existing_owner) = markers.early.get(request_key) {
            return Arc::ptr_eq(existing_owner, &route.owner);
        }
        if markers.early.len() >= self.shared.resolution_marker_capacity() {
            return false;
        }
        markers
            .early
            .insert(request_key.to_owned(), Arc::clone(&route.owner));
        true
    }
}

impl CodexSession {
    /// Stops one abandoned stream owner, preserving the session when Codex confirms its terminal.
    async fn abandon_owner(
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
        state: SessionState,
        owner: Arc<()>,
    ) {
        let _ = Self::request_owner_stop(
            shared,
            client,
            control,
            state,
            owner,
            CancelReason::Requested,
        )
        .await;
    }

    /// Claims the one durable stop worker for this owner before any interrupt await begins.
    async fn request_owner_stop(
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
        state: SessionState,
        owner: Arc<()>,
        reason: CancelReason,
    ) -> Arc<StopState> {
        let stop = {
            let turn = shared.turn.lock().await;
            turn.as_ref()
                .filter(|active| !active.finishing && Arc::ptr_eq(&active.owner, &owner))
                .map(|active| Arc::clone(&active.stop))
        };
        let Some(stop) = stop else {
            return Arc::new(StopState::finished());
        };
        if stop
            .started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            let worker_stop = Arc::clone(&stop);
            tokio::spawn(async move {
                if let Err(error) =
                    Self::stop_owner(shared, client, control, state, owner, reason).await
                {
                    *worker_stop.failure.lock().await = Some(error);
                }
                worker_stop.complete.store(true, Ordering::Release);
                worker_stop.done.notify_waiters();
            });
        }
        stop
    }

    /// Waits for a durable native-stop worker without owning its lifetime.
    async fn wait_for_stop(stop: &StopState) -> Result<()> {
        loop {
            let done = stop.done.notified();
            if stop.complete.load(Ordering::Acquire) {
                return match stop.failure.lock().await.as_ref() {
                    Some(error) => Err(error.clone()),
                    None => Ok(()),
                };
            }
            done.await;
        }
    }

    /// Sends this owner's one `turn/interrupt`, first waiting for a pending start to name its turn.
    ///
    /// The first pass records the cancel and refuses the owner's approvals; for a pending start
    /// it can send nothing, because Codex's interrupt must name the turn. The second pass sends
    /// the interrupt once the id is known, or nothing when the start can never be named. Each wait
    /// is bounded by `request_timeout`, never by a shutdown deadline; the start wait is bounded
    /// here as well, because an unpolled start future never reaches its own deadline.
    async fn dispatch_stop(
        shared: &Shared,
        client: &Client,
        owner: &Arc<()>,
        reason: CancelReason,
    ) -> Result<()> {
        shared.cancel_owner(client, Some(owner), reason).await?;
        // The start future belongs to the host, which may keep it alive without polling it; then
        // neither its request deadline nor its drop guard ever resolves the start. Bound the wait
        // here too, from when the stop began, and treat expiry as a lost answer. A notification
        // that names the turn later still gets its interrupt in the settle stage.
        let request_timeout = shared.host.limits().request_timeout;
        if tokio::time::timeout(request_timeout, shared.wait_for_start_resolution(owner))
            .await
            .is_err()
        {
            shared.mark_start_unanswerable(owner).await;
        }
        shared.cancel_owner(client, Some(owner), reason).await?;
        Ok(())
    }

    /// Cancels one owned native turn and escalates only when its terminal does not arrive.
    async fn stop_owner(
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
        state: SessionState,
        owner: Arc<()>,
        reason: CancelReason,
    ) -> Result<()> {
        if !shared.owner_is_active(&owner).await {
            return Ok(());
        }
        let settle = shared.host.limits().cancel_settle_timeout;
        let settled = match Self::dispatch_stop(&shared, &client, &owner, reason).await {
            Ok(()) => Self::settle_stop(&shared, &client, &owner, reason, settle).await,
            Err(error) => Settled::InterruptFailed(error),
        };
        match settled {
            Settled::Ended => Self::after_turn_gone(&shared).await,
            Settled::InterruptFailed(error) => {
                // Codex refused, or never acknowledged, the interrupt while the turn is live.
                if shared.seal_owner_for_shutdown(&owner).await {
                    Self::request_shutdown(Arc::clone(&shared), client, control, state, reason);
                    // A teardown that could not reap the child outranks the interrupt failure:
                    // its `CleanupRequired` carries the control the host needs to reconcile.
                    return Self::wait_for_shutdown(&shared).await.and(Err(error));
                }
                Self::after_turn_gone(&shared).await
            }
            Settled::Expired => {
                if shared.seal_owner_for_shutdown(&owner).await {
                    Self::request_shutdown(Arc::clone(&shared), client, control, state, reason);
                    return Self::wait_for_shutdown(&shared).await;
                }
                Self::after_turn_gone(&shared).await
            }
        }
    }

    /// Waits for a stopped turn to end, bounded by `settle`.
    ///
    /// The stop did all it can on the protocol. The turn stays admitted until Codex reports it
    /// over. A turn that is named but not yet interrupted — a lost start answer named later by a
    /// notification, or named while the stop was between its passes — gets its one interrupt
    /// here, with a fresh settle clock.
    async fn settle_stop(
        shared: &Shared,
        client: &Client,
        owner: &Arc<()>,
        reason: CancelReason,
        settle: std::time::Duration,
    ) -> Settled {
        loop {
            let awaiting_name = shared.awaiting_name(owner).await;
            tokio::select! {
                () = shared.wait_for_turn_end(owner) => return Settled::Ended,
                () = tokio::time::sleep(settle) => return Settled::Expired,
                () = shared.wait_for_named(owner), if awaiting_name => {
                    if let Err(error) = shared.cancel_owner(client, Some(owner), reason).await {
                        return Settled::InterruptFailed(error);
                    }
                }
            }
        }
    }

    /// What a stop reports once its turn left admission without this worker escalating.
    ///
    /// A racing `close` may have ended the turn and then failed to reap the child. The stop waiter
    /// shares that teardown's outcome, so its `CleanupRequired` is not reported as success.
    async fn after_turn_gone(shared: &Shared) -> Result<()> {
        if shared.teardown_started.load(Ordering::Acquire) {
            return Self::wait_for_shutdown(shared).await;
        }
        Ok(())
    }

    /// Closes the connection and reaps its child after native cancellation did not settle it.
    async fn shutdown_session(
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
        state: SessionState,
        reason: CancelReason,
    ) -> Result<()> {
        state.set_status(SessionStatus::Closing);
        shared.stop_new_work();
        let limits = *shared.host.limits();
        let _ = tokio::time::timeout(
            limits.kill_grace,
            shared.cancel_owner(&client, None, reason),
        )
        .await;
        shared
            .release_pending_for(None, DecisionSource::Cancelled)
            .await;
        shared.cancel_active(reason).await;
        let close = tokio::time::timeout(limits.shutdown_timeout, client.close())
            .await
            .map_err(|_| Error::Timeout {
                operation: String::from("Codex app-server connection close"),
                after: limits.shutdown_timeout,
            })
            .and_then(|result| result);
        let stop = stop_process_with_limits(&*control, reason, &limits).await;
        // Reaping the app-server is the terminal lifetime fact. A blocked JSON-RPC writer can
        // make closing its connection report an error after the child is gone, but leaving the
        // observable session `Ready` then invites a host to submit work to a reaped process.
        if stop.is_ok() {
            state.set_status(SessionStatus::Closed);
        }
        match stop {
            Ok(_) => close,
            Err(source) => Err(Error::CleanupRequired {
                control,
                source: Box::new(source),
            }),
        }
    }

    /// Starts exactly one detached teardown worker so an abandoned `close` future cannot orphan
    /// Codex's child. Later callers observe its completion rather than starting a second reaper.
    fn request_shutdown(
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
        state: SessionState,
        reason: CancelReason,
    ) {
        if shared
            .teardown_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let worker_shared = Arc::clone(&shared);
        let worker_control = Arc::clone(&control);
        tokio::spawn(async move {
            // The cleanup runs in a task of its own so a panicking host `ProcessControl` surfaces
            // here as a join error. Without that, the completion below never ran and every close
            // waiter stayed parked forever.
            let worker = tokio::spawn(Self::shutdown_session(
                worker_shared,
                client,
                worker_control,
                state,
                reason,
            ));
            let outcome = match worker.await {
                Ok(outcome) => outcome,
                Err(joined) => Err(teardown_panicked(control, &joined)),
            };
            if let Err(error) = outcome {
                *shared.teardown_error.lock().await = Some(error);
            }
            shared.teardown_complete.store(true, Ordering::Release);
            shared.teardown_done.notify_waiters();
        });
    }

    /// Waits for the one teardown worker and preserves its typed cleanup failure for every caller.
    async fn wait_for_shutdown(shared: &Shared) -> Result<()> {
        loop {
            let done = shared.teardown_done.notified();
            if shared.teardown_complete.load(Ordering::Acquire) {
                let error = shared.teardown_error.lock().await;
                return match error.as_ref() {
                    Some(error) => Err(error.clone()),
                    None => Ok(()),
                };
            }
            done.await;
        }
    }

    /// Starts a watcher after a stream reaches its caller, making stream drop explicit ownership
    /// abandonment rather than an invisible native turn.
    async fn watch_stream_abandonment(&self, owner: Arc<()>, sink: EventSink) {
        let shared = Arc::clone(&self.shared);
        let client = Arc::clone(&self.client);
        let control = Arc::clone(&self.control);
        let state = self.state.clone();
        let abandoned_owner = Arc::clone(&owner);
        let watcher = tokio::spawn(async move {
            tokio::select! {
                () = sink.closed() => {}
                () = sink.terminated() => {}
            }
            tokio::spawn(async move {
                Self::abandon_owner(shared, client, control, state, abandoned_owner).await;
            });
        });
        let idle_shared = Arc::clone(&self.shared);
        let idle_client = Arc::clone(&self.client);
        let idle_control = Arc::clone(&self.control);
        let idle_state = self.state.clone();
        let idle_owner = Arc::clone(&owner);
        let idle_watcher = tokio::spawn(async move {
            Self::watch_idle(
                idle_shared,
                idle_client,
                idle_control,
                idle_state,
                idle_owner,
            )
            .await;
        });
        let mut turn = self.shared.turn.lock().await;
        if let Some(active) = turn
            .as_mut()
            .filter(|active| Arc::ptr_eq(&active.owner, &owner))
        {
            active.abandonment_watcher = Some(watcher);
            active.idle_watcher = Some(idle_watcher);
        } else {
            watcher.abort();
            idle_watcher.abort();
        }
    }

    /// Enforces the host's idle deadline, pausing while an approval owns the turn's wait phase.
    async fn watch_idle(
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
        state: SessionState,
        owner: Arc<()>,
    ) {
        let mut changes = shared.idle_changes.subscribe();
        let idle_timeout = shared.host.limits().idle_timeout;
        loop {
            if !shared.owner_is_active(&owner).await {
                return;
            }
            if shared.approval_is_pending_for(&owner).await {
                if changes.changed().await.is_err() {
                    return;
                }
                continue;
            }
            tokio::select! {
                () = tokio::time::sleep(idle_timeout) => {
                    if shared.owner_is_active(&owner).await
                        && !shared.approval_is_pending_for(&owner).await
                    {
                        let _ = Self::request_owner_stop(
                            shared,
                            client,
                            control,
                            state,
                            owner,
                            CancelReason::Timeout,
                        )
                        .await;
                    }
                    return;
                }
                changed = changes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// A session over a connection that is already pumping and already has a thread.
    pub(crate) fn new(
        state: SessionState,
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
    ) -> Self {
        let _ = shared.control.set(Arc::clone(&control));
        let _ = shared.client.set(Arc::clone(&client));
        let cancel = shared.host.cancel().clone();
        let terminated = shared.terminated.clone();
        let watcher_shared = Arc::clone(&shared);
        let watcher_client = Arc::clone(&client);
        let watcher_control = Arc::clone(&control);
        // Cheap to clone: every clone publishes into the one picture `close` writes to. Without it
        // a watcher-driven teardown left the snapshot saying `Ready` while every later start was
        // refused, so a host watching the subscription never learned the session had ended.
        let watcher_state = state.clone();
        let shutdown_watcher = tokio::spawn(async move {
            tokio::select! {
                () = cancel.cancelled() => {
                    Self::request_shutdown(
                        watcher_shared,
                        watcher_client,
                        watcher_control,
                        watcher_state,
                        CancelReason::Shutdown,
                    );
                }
                () = terminated.cancelled() => {
                    // The connection is already gone; `connection_terminated` or `poison` failed
                    // the active turn before cancelling this token.
                    Self::request_shutdown(
                        watcher_shared,
                        watcher_client,
                        watcher_control,
                        watcher_state,
                        CancelReason::Shutdown,
                    );
                }
            }
        });
        let accepted = state.snapshot().configuration.accepted.clone();
        Self {
            configuration: Mutex::new(AcceptedConfiguration {
                accepted,
                next_generation: 0,
                accepted_generation: [0; 4],
            }),
            state,
            shared,
            client,
            control,
            closed: AtomicBool::new(false),
            shutdown_watcher,
            // `new` already spawned the watcher above, which only succeeds inside a runtime.
            runtime: tokio::runtime::Handle::current(),
        }
    }

    /// The handler that answers this session's connection.
    pub(crate) fn handler(shared: Arc<Shared>) -> Arc<dyn PeerHandler> {
        Arc::new(CodexHandler { shared })
    }

    /// Starts a turn or a review, which are the same thing with different openings.
    async fn begin<P: serde::Serialize + Send, R>(
        &self,
        turn_id: TurnId,
        attempt: AttemptId,
        rpc_method: &str,
        params: P,
        turn_of: impl FnOnce(R) -> (TurnHandle, Option<String>) + Send,
        configuration: Option<mango_external_agents::Configuration>,
    ) -> Result<(TurnStream, Option<String>)>
    where
        R: serde::de::DeserializeOwned,
    {
        let (sink, events) = EventSink::with_limits(
            self.shared.session_id.clone(),
            turn_id.clone(),
            attempt,
            Arc::clone(self.shared.host.clock()),
            self.shared.host.limits(),
        );
        let owner = Arc::new(());
        let generation = {
            let mut state = self.configuration.lock().await;
            state.next_generation += 1;
            state.next_generation
        };

        {
            let mut turn = self.shared.turn.lock().await;
            if self.closed.load(Ordering::Acquire)
                || self.shared.is_shutting_down()
                || self.shared.host.cancel().is_cancelled()
            {
                return Err(
                    Error::Closed { subject: "session" }.with_dispatch(Dispatch::NotSubmitted)
                );
            }
            if turn.is_some() {
                // Refused under the lock, so two callers racing cannot both find it free. The
                // app-server would take the second as a steer of the first.
                return Err(Error::Busy.with_dispatch(Dispatch::NotSubmitted));
            }
            // Installed before the call, because notifications for this turn can arrive before its
            // own response does.
            *turn = Some(ActiveTurn {
                owner: Arc::clone(&owner),
                sink: sink.clone(),
                turn_id: turn_id.clone(),
                attempt,
                native_turn_id: String::new(),
                announced: false,
                earlier_native_turn_ids: VecDeque::new(),
                reducer: crate::turn_reducer::TurnReducer::new(),
                is_review: rpc_method == method::REVIEW_START,
                start_unanswerable: false,
                interrupt_dispatched: false,
                cancel_reason: None,
                abandonment_watcher: None,
                idle_watcher: None,
                stop: Arc::new(StopState::new()),
                finishing: false,
            });
        }
        let mut start_guard = StartGuard::new(
            Arc::clone(&self.shared),
            Arc::clone(&self.client),
            Arc::clone(&self.control),
            self.state.clone(),
            Arc::clone(&owner),
        );

        let request_written = Arc::clone(&start_guard.request_written);
        let answer: Result<R> = self
            .client
            .request_tracking_write(rpc_method, params, &request_written)
            .await;
        let (handle, extra) = match answer {
            Ok(answer) => turn_of(answer),
            Err(error) => {
                if !request_written.load(Ordering::Acquire) {
                    // Refused before any byte reached the link — a closed client, a full
                    // request budget, an unencodable frame. Codex never saw it, so this is a
                    // clean refusal: release the owner and report the start as not submitted.
                    self.shared.release_refused_start(&owner).await;
                    start_guard.disarm();
                    return Err(error.with_dispatch(Dispatch::NotSubmitted));
                }
                if !start_was_explicitly_refused(&error) {
                    // A local transport, decoding, or timeout failure says nothing about whether
                    // the app-server accepted the request. Keeping the slot occupied prevents
                    // the next start from becoming a steer of a vendor turn whose handle never
                    // reached this client.
                    self.shared.mark_start_unanswerable(&owner).await;
                    self.watch_stream_abandonment(Arc::clone(&owner), sink.clone())
                        .await;
                    start_guard.disarm();
                    return Ok((
                        TurnStream::accepted(turn_id, attempt, String::new(), events)
                            .with_dispatch(Dispatch::AcceptanceUnknown),
                        None,
                    ));
                }
                // The turn never started, so the stream it would have written to is closed here
                // rather than left for a `turn/completed` that will never come. Nothing can be
                // interrupted either. Releasing through the shared owner path wakes a
                // cancellation reaper that was already waiting for this admission slot.
                self.shared.release_refused_start(&owner).await;
                start_guard.disarm();
                return Err(error.with_dispatch(Dispatch::Accepted));
            }
        };

        let native_turn_id =
            mango_external_agents::normalize::opaque_id(&handle.id, "native turn id")
                .map_err(|error| error.with_dispatch(Dispatch::Accepted));
        let native_turn_id = match native_turn_id {
            Ok(native_turn_id) => native_turn_id,
            Err(error) => {
                // A positive response that cannot be routed is already vendor work. Tear down
                // the connection so a later request cannot become a steer of that hidden turn.
                let _ = self.close(CloseReason::Shutdown).await;
                return Err(error);
            }
        };

        // The turn-scoped counterpart to opening a session: the vendor accepted this attempt and
        // named its own handle for it. Replacing what used to ride the first turn as a
        // session-wide announcement — that fact is now published on `SessionState` at open time
        // instead.
        //
        // A notification naming this turn's id can already have won this race — captured
        // fixtures show it arriving before this response is even a real race, not a rare one —
        // in which case this call claims nothing and announces nothing.
        let announcement = match self
            .shared
            .claim_announcement(&owner, &native_turn_id)
            .await
        {
            Ok(announcement) => announcement,
            Err(error) => {
                let _ = self.close(CloseReason::Shutdown).await;
                return Err(error.with_dispatch(Dispatch::Accepted));
            }
        };
        if let Some((sink, announced_turn_id)) = announcement
            && let Err(error) = sink
                .emit(EventKind::TurnStarted {
                    native_turn_id: announced_turn_id,
                })
                .await
        {
            let _ = self.close(CloseReason::Shutdown).await;
            return Err(error.with_dispatch(Dispatch::Accepted));
        }

        if let Some(configuration) = configuration {
            let mut state = self.configuration.lock().await;
            let changed = state.accept(generation, configuration);
            if !changed.is_unknown() {
                let mut observed = self.state.snapshot().configuration.observed.clone();
                if changed.model.is_some() {
                    observed.model = None;
                }
                if changed.effort.is_some() {
                    observed.effort = None;
                }
                if changed.level.is_some() {
                    observed.level = None;
                }
                if changed.routing.is_some() {
                    observed.routing = None;
                }
                let accepted = state.accepted.clone();
                self.state.set_configuration(ConfigurationState::new(
                    accepted.clone(),
                    accepted,
                    observed,
                ));
            }
        }

        if let Some(active) = self
            .shared
            .turn
            .lock()
            .await
            .as_mut()
            .filter(|active| Arc::ptr_eq(&active.owner, &owner))
        {
            active.native_turn_id.clone_from(&native_turn_id);
        }
        // A cancel that beat this answer could not name the turn. It kept the slot occupied so no
        // later `turn/start` could become a steer; its stop worker now has the id and sends the
        // one interrupt. Session shutdown and poison release the slot instead: their process
        // teardown, not an interrupt, ends that turn.
        self.shared.turn_finished.notify_waiters();

        self.watch_stream_abandonment(owner, sink).await;
        start_guard.disarm();
        Ok((
            TurnStream::accepted(turn_id, attempt, native_turn_id, events),
            extra,
        ))
    }

    fn input_for(request: &TurnRequest) -> Result<Vec<UserInput>> {
        if request.attachments.len() > TURN_MAX_ATTACHMENTS {
            return Err(Error::LimitExceeded {
                subject: "attachments on one turn",
                limit: TURN_MAX_ATTACHMENTS,
                received: request.attachments.len(),
            });
        }
        let mut input = vec![UserInput::text(request.input.clone())];
        for attachment in &request.attachments {
            if attachment.bytes.len() > ATTACHMENT_MAX_BYTES {
                return Err(Error::LimitExceeded {
                    subject: "bytes in one attachment",
                    limit: ATTACHMENT_MAX_BYTES,
                    received: attachment.bytes.len(),
                });
            }
            if attachment.kind != mango_external_agents::AttachmentKind::Image {
                return Err(Error::Protocol {
                    expected: String::from("an image attachment Codex can send"),
                    received: format!("{:?} attachment {}", attachment.kind, attachment.name),
                });
            }
            if !supports_image_mime_type(&attachment.mime_type) {
                return Err(Error::Protocol {
                    expected: String::from(
                        "an image MIME type Codex accepts: image/png, image/jpeg, image/gif, or image/webp",
                    ),
                    received: attachment.mime_type.clone(),
                });
            }
            input.push(UserInput::Image {
                url: data_url(&attachment.mime_type, &attachment.bytes),
            });
        }
        Ok(input)
    }
}

fn supports_image_mime_type(mime_type: &str) -> bool {
    ["image/png", "image/jpeg", "image/gif", "image/webp"]
        .iter()
        .any(|supported| mime_type.eq_ignore_ascii_case(supported))
}

/// Bytes as the `data:` URL the app-server's image input takes.
fn data_url(mime_type: &str, bytes: &[u8]) -> String {
    format!("data:{mime_type};base64,{}", base64(bytes))
}

/// Standard base64, without a dependency for sixty-four characters.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0_u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let packed = u32::from(block[0]) << 16 | u32::from(block[1]) << 8 | u32::from(block[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                let position = (packed >> (18 - index * 6)) & 0b0011_1111;
                encoded.push(char::from(ALPHABET[position as usize]));
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

#[async_trait::async_trait]
impl Session for CodexSession {
    fn state(&self) -> &SessionState {
        &self.state
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        self.validate_turn_request(&request)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        // Codex already persists its accepted settings. Omission leaves those settings alone.
        let patch = request.configuration.clone().unwrap_or_default();
        // Codex has no "drop my override" semantics on `turn/start`, so a reset is refused
        // explicitly rather than silently dropped into a no-op keep.
        refuse_unsupported_native(&patch)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        refuse_unsupported_reset(&patch)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        crate::configuration::validate_ids(&patch)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        let vendor = crate::permissions::overrides(&patch);
        let configuration = patch.requested();

        let params = TurnStartParams {
            thread_id: self.shared.thread_id().to_owned(),
            input: Self::input_for(&request)
                .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?,
            model: configuration.model.clone(),
            effort: configuration.effort.clone(),
            approval_policy: vendor.approval_policy,
            sandbox_policy: configuration.level.map(|level| {
                crate::permissions::VendorConfiguration::for_pair(
                    level,
                    mango_external_agents::ApprovalRouting::User,
                )
                .sandbox_policy(&self.shared.host.cwd().to_string_lossy())
            }),
            approvals_reviewer: vendor.approvals_reviewer,
        };

        let (stream, _) = self
            .begin::<_, TurnStartResponse>(
                request.turn_id,
                request.attempt,
                method::TURN_START,
                params,
                |response| (response.turn, None),
                Some(configuration),
            )
            .await?;
        Ok(stream)
    }

    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        self.require_capability(mango_external_agents::Capability::InteractiveApprovals)?;
        let route = self
            .shared
            .active_turn_route()
            .await
            .ok_or_else(|| Error::Protocol {
                expected: String::from("an active turn that owns this approval"),
                received: response.interaction_id.to_string(),
            })?;
        let entry = {
            let mut pending = self.shared.pending.lock().await;
            if !pending
                .get(&response.interaction_id)
                .is_some_and(|entry| Arc::ptr_eq(&entry.route.owner, &route.owner))
            {
                None
            } else {
                pending.remove(&response.interaction_id)
            }
        };
        let Some(entry) = entry else {
            return Err(Error::Protocol {
                expected: String::from("an approval this session is still waiting on"),
                received: response.interaction_id.to_string(),
            });
        };
        self.shared.signal_idle_change();
        if entry.deadline.is_elapsed() {
            let decision = entry.pending.refusal();
            let option_id = decision.option_id().to_owned();
            let _ = entry.answer.send(Answer::Chosen {
                decision,
                option_id,
                source: DecisionSource::Expired,
                reported: false,
            });
            return Err(Error::Protocol {
                expected: String::from("an approval whose deadline has not expired"),
                received: response.interaction_id.to_string(),
            });
        }
        let Some(decision) = entry.pending.decision_for(&response.option_id) else {
            // Put back: the server is still waiting, and an id nobody offered is the caller's
            // mistake rather than a reason to leave the vendor blocked.
            let option_id = response.option_id.clone();
            let mut pending = self.shared.pending.lock().await;
            if pending.contains_key(&response.interaction_id) {
                return Err(Error::Protocol {
                    expected: String::from("the original approval slot to remain vacant"),
                    received: response.interaction_id.to_string(),
                });
            }
            pending.insert(response.interaction_id.clone(), entry);
            return Err(Error::Protocol {
                expected: String::from("one of the options this approval offered"),
                received: option_id,
            });
        };
        entry
            .answer
            .send(Answer::Chosen {
                decision,
                option_id: response.option_id,
                source: response.source,
                reported: false,
            })
            .map_err(|_| Error::Closed {
                subject: "approval",
            })
    }

    async fn answer(&self, response: QuestionResponse) -> Result<()> {
        self.require_capability(mango_external_agents::Capability::Questions)?;
        let Some(route) = self.shared.active_turn_route().await else {
            return Err(Error::Protocol {
                expected: String::from("an active turn that owns this question round"),
                received: response.interaction_id.to_string(),
            });
        };
        let current_operation = route.operation(self.shared.session_id.clone());
        let interaction_id = response.interaction_id.clone();

        let settlement = self.shared.question_settlements.lock().await;
        let mut pending = self.shared.pending_questions.lock().await;
        let Some(entry) = pending.get(&interaction_id) else {
            drop(pending);
            return Err(Error::Protocol {
                expected: String::from("a question round this session is still waiting on"),
                received: interaction_id.to_string(),
            });
        };
        // A stale answer for an attempt the host has already replaced is answering work that no
        // longer exists, and applying it would mutate the attempt that replaced it.
        let entry_operation = entry.route.operation(self.shared.session_id.clone());
        if entry_operation.is_superseded_by(&current_operation) {
            drop(pending);
            return Err(Error::Protocol {
                expected: String::from("an answer to the current attempt's question round"),
                received: String::from("an answer naming a superseded attempt"),
            });
        }
        if entry.deadline.is_elapsed() {
            // Keep this accepted settlement until its waiter or terminal cleanup publishes it.
            // Otherwise a completed turn can overtake the expiry and leave its announced round
            // unresolved on the host.
            let entry = pending
                .get_mut(&interaction_id)
                .expect("checked present under the same lock above");
            let Some(answer) = entry.answer.take() else {
                drop(pending);
                return Err(Error::Protocol {
                    expected: String::from("a question round this session is still waiting on"),
                    received: interaction_id.to_string(),
                });
            };
            entry.settlement = Some(QuestionOutcome::Expired);
            if answer.send(QuestionAnswer::Expired).is_err() {
                entry.settlement = None;
                drop(pending);
                return Err(Error::Closed {
                    subject: "question",
                });
            }
            drop(pending);
            self.shared.signal_idle_change();
            return Err(Error::Protocol {
                expected: String::from("a question round whose deadline has not expired"),
                received: interaction_id.to_string(),
            });
        }
        if let Err(error) = entry.request.validate(&response) {
            drop(pending);
            return Err(error);
        }
        let entry = pending
            .get_mut(&interaction_id)
            .expect("checked present under the same lock above");
        let Some(answer) = entry.answer.take() else {
            drop(pending);
            return Err(Error::Protocol {
                expected: String::from("a question round this session is still waiting on"),
                received: interaction_id.to_string(),
            });
        };
        entry.settlement = Some(QuestionOutcome::Answered {
            answers: response.answers.clone(),
        });
        if answer.send(QuestionAnswer::Answered(response)).is_err() {
            entry.settlement = None;
            drop(pending);
            return Err(Error::Closed {
                subject: "question",
            });
        }
        drop(pending);
        drop(settlement);

        self.shared.signal_idle_change();
        Ok(())
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        let Some(route) = self.shared.active_turn_route().await else {
            return Ok(());
        };
        let pending_start = route.native_turn_id.is_empty();
        let owner = route.owner;
        let stop = Self::request_owner_stop(
            Arc::clone(&self.shared),
            Arc::clone(&self.client),
            Arc::clone(&self.control),
            self.state.clone(),
            owner,
            reason,
        )
        .await;
        if pending_start {
            return Ok(());
        }
        Self::wait_for_stop(&stop).await
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Self::wait_for_shutdown(&self.shared).await;
        }
        self.shared.stop_new_work();
        self.shutdown_watcher.abort();
        Self::request_shutdown(
            Arc::clone(&self.shared),
            Arc::clone(&self.client),
            Arc::clone(&self.control),
            self.state.clone(),
            CancelReason::from(reason),
        );
        Self::wait_for_shutdown(&self.shared).await
    }

    async fn steer(&self, steer: Steer) -> Result<SteerOutcome> {
        // Held across the read, the request and the adoption: a second steer issued while the
        // first is in flight must address the id the first one leaves behind.
        let _steering = self.shared.steering.lock().await;
        let running = {
            let turn = self.shared.turn.lock().await;
            turn.as_ref()
                .filter(|active| !active.finishing)
                .map(|active| {
                    (
                        Arc::clone(&active.owner),
                        active.turn_id.clone(),
                        active.native_turn_id.clone(),
                        active.native_turn_id == steer.native_turn_id
                            || active
                                .earlier_native_turn_ids
                                .contains(&steer.native_turn_id),
                        active.is_review,
                    )
                })
        };
        let Some((owner, turn_id, native_turn_id, names_this_turn, is_review)) = running else {
            return Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted,
            });
        };
        if is_review {
            return Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnNotSteerable,
            });
        }
        if turn_id != steer.turn_id || native_turn_id.is_empty() || !names_this_turn {
            // The turn the host meant is not the one running. Steering the live one instead would
            // put the input on a turn nobody addressed.
            return Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted,
            });
        }

        let params = TurnSteerParams {
            thread_id: self.shared.thread_id().to_owned(),
            input: vec![UserInput::text(steer.input)],
            // The id Codex is running now, which a continuation may have moved past the one the
            // host named.
            expected_turn_id: native_turn_id.clone(),
        };
        let hold = SteerHoldGuard::hold(&self.shared);
        let answered = self
            .client
            .request::<_, TurnSteerResponse>(method::TURN_STEER, params)
            .await;
        let adopted = match &answered {
            Ok(response) => {
                self.shared
                    .adopt_continuation(&owner, &native_turn_id, &response.turn_id)
                    .await
            }
            Err(_) => Ok(()),
        };
        hold.release().await;
        match answered {
            Ok(_) => adopted.map(|()| SteerOutcome::Accepted),
            Err(Error::Vendor(error)) if is_no_active_turn(&error) => Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted,
            }),
            Err(Error::Vendor(error)) if is_not_steerable(&error) => Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnNotSteerable,
            }),
            Err(error) => Err(error),
        }
    }

    async fn start_review(&self, request: ReviewRequest) -> Result<ReviewStream> {
        let params = ReviewStartParams {
            thread_id: self.shared.thread_id().to_owned(),
            target: match request.target {
                mango_external_agents::ReviewTarget::UncommittedChanges => {
                    ReviewTarget::UncommittedChanges
                }
                mango_external_agents::ReviewTarget::BaseBranch { branch } => {
                    ReviewTarget::BaseBranch { branch }
                }
                mango_external_agents::ReviewTarget::Commit { sha, title } => {
                    ReviewTarget::Commit { sha, title }
                }
                mango_external_agents::ReviewTarget::Custom { instructions } => {
                    ReviewTarget::Custom { instructions }
                }
                other => {
                    return Err(Error::Protocol {
                        expected: String::from("a review target this harness can spell"),
                        received: format!("{other:?}"),
                    });
                }
            },
        };
        let (turn, review_thread_id) = self
            .begin::<_, ReviewStartResponse>(
                request.turn_id,
                // A review has no attempt of its own on `ReviewRequest`; it does not retry the
                // way a turn does.
                AttemptId::default(),
                method::REVIEW_START,
                params,
                // Only when the server named one. `reviewThreadId` deserialises to an empty
                // string when it is absent, and wrapping that in `Some` would hand the fallback
                // below a value it could never replace — leaving a host told to subscribe to a
                // thread with no name.
                |response| {
                    let named = Some(response.review_thread_id).filter(|id| !id.is_empty());
                    (response.turn, named)
                },
                None,
            )
            .await?;
        Ok(ReviewStream {
            turn,
            // An inline review runs on this session's own thread, which is what `delivery` is left
            // absent to ask for. Reporting whatever the server named lets a host refuse a thread
            // it is not subscribed to rather than stream a review nobody would see.
            review_thread_id: review_thread_id
                .unwrap_or_else(|| self.shared.thread_id().to_owned()),
        })
    }

    async fn list_native_sessions(&self, query: SessionQuery) -> Result<SessionPage> {
        list_threads(&self.client, &self.shared.host, query).await
    }

    async fn refresh_account_usage(&self) -> Result<AccountUsage> {
        // The same bookkeeping as the session's own read: an update that lands while this is in
        // flight is laid over the answer rather than lost to it.
        self.shared.begin_quota_read().await;
        let response = self
            .client
            .request::<_, RateLimitsReadResponse>(method::ACCOUNT_RATE_LIMITS_READ, empty_params())
            .await;
        let answer = response
            .as_ref()
            .ok()
            .and_then(|response| response.rate_limits.clone());
        let merged = self.shared.finish_quota_read(answer.as_ref()).await;
        response?;
        Ok(AccountUsage {
            // Absence is unknown, never an empty quota: a server that answered without a snapshot
            // has not told us the account is unmetered.
            limits: merged.as_ref().map(|snapshot| {
                crate::rate_limits::to_account_limits(snapshot, self.shared.host.now())
            }),
        })
    }
}

/// Lists only conversations in the host's authorized workspace, on a live or probe connection.
pub(crate) async fn list_threads(
    client: &Client,
    host: &HostContext,
    query: SessionQuery,
) -> Result<SessionPage> {
    validate_list_workspace(host, &query)?;
    let cwd = host.absolute_cwd()?;
    let params = ThreadListParams::picker(query.cursor, query.limit, cwd);
    let page: ThreadListResponse = client.request(method::THREAD_LIST, params).await?;
    if let Some(limit) = query.limit
        && page.data.len() > limit
    {
        return Err(Error::LimitExceeded {
            subject: "sessions in a Codex thread/list response",
            limit,
            received: page.data.len(),
        });
    }
    Ok(SessionPage {
        sessions: page
            .data
            .into_iter()
            .filter(|thread| thread.cwd.as_deref() == Some(cwd))
            .map(|thread| NativeSession {
                updated_at: thread.last_used_at().and_then(|seconds| {
                    u64::try_from(seconds).ok().and_then(|seconds| {
                        std::time::SystemTime::UNIX_EPOCH
                            .checked_add(std::time::Duration::from_secs(seconds))
                    })
                }),
                native_session_id: thread.id,
                // A title may be absent; the preview is the first user message when supplied.
                title: thread.name.filter(|name| !name.is_empty()),
                preview: Some(thread.preview).filter(|preview| !preview.is_empty()),
                workspace_path: thread.cwd,
            })
            .collect(),
        next_cursor: page.next_cursor,
        truncated: false,
    })
}

/// Refuses a remote or unrelated workspace before any list request or probe launch.
pub(crate) fn validate_list_workspace(host: &HostContext, query: &SessionQuery) -> Result<()> {
    host.absolute_cwd()?;
    if query
        .workspace_path
        .as_ref()
        .is_some_and(|path| path != host.cwd())
    {
        return Err(Error::HostConfiguration {
            expected: "a session-list workspace equal to the host's authorized directory",
            received: String::from("a different workspace path"),
        });
    }
    Ok(())
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        self.shutdown_watcher.abort();
        self.closed.store(true, Ordering::Release);
        let shared = Arc::clone(&self.shared);
        let client = Arc::clone(&self.client);
        let control = Arc::clone(&self.control);
        let state = self.state.clone();
        // The current runtime when there is one, else the one the session was opened on: a host
        // dropping its last handle from a plain thread must not orphan the app-server.
        let runtime =
            tokio::runtime::Handle::try_current().unwrap_or_else(|_| self.runtime.clone());
        runtime.spawn(async move {
            Self::request_shutdown(shared, client, control, state, CancelReason::Shutdown);
        });
    }
}

/// The typed failure every close waiter receives when the teardown worker did not return.
///
/// The child may still be live, so the host's control rides along for reconciliation.
///
/// ```ignore
/// let error = teardown_panicked(control, &join_error);
/// assert!(error.cleanup_control().is_some());
/// ```
fn teardown_panicked(control: Arc<dyn ProcessControl>, joined: &tokio::task::JoinError) -> Error {
    let shape = if joined.is_panic() {
        "panicked"
    } else {
        "was cancelled"
    };
    Error::CleanupRequired {
        control,
        source: Box::new(Error::Link {
            peer: String::from("codex app-server"),
            message: format!(
                "expected the session teardown worker to return | received: it {shape}"
            ),
        }),
    }
}

/// Whether a steer failed because the turn it named is not the one running.
///
/// The app-server answers `-32600 "no active turn to steer"`. Both halves are checked, because
/// neither is enough on its own: `-32600` is the generic invalid-request code and carries every
/// other malformed steer with it, and a message match alone would take a `-32600` this harness
/// has not seen and report it to a host as a turn that simply finished first.
///
/// A code the vendor later changes falls through to the vendor error, which is the conservative
/// failure: a host is told what the server said rather than told something that did not happen.
fn is_no_active_turn(error: &VendorError) -> bool {
    error.vendor_code.as_deref() == Some(NO_ACTIVE_TURN_CODE)
        && error.message.to_lowercase().contains("no active turn")
}

/// Whether a steer failed because the running turn is a review or a compaction.
///
/// The pinned app-server answers `-32600` with `cannot steer a review turn` or `cannot steer a
/// compact turn`, and attaches `codexErrorInfo: {activeTurnNotSteerable: {turnKind}}` as data. The
/// JSON-RPC client keeps the code and message but not the data, so both of those are checked, on
/// the same terms as [`is_no_active_turn`].
fn is_not_steerable(error: &VendorError) -> bool {
    error.vendor_code.as_deref() == Some(NO_ACTIVE_TURN_CODE)
        && error.message.starts_with("cannot steer a ")
}

/// The JSON-RPC code the app-server refuses a steer with: the generic invalid-request code.
const NO_ACTIVE_TURN_CODE: &str = "-32600";

/// A start is safe to forget only after the app-server sent an ordinary JSON-RPC error frame.
///
/// The client uses `-32000` for its own EOF and close failures, so that reserved server-error
/// spelling remains ambiguous. Conservatively retaining a slot for a vendor that really used it
/// is preferable to steering a turn whose start result was lost.
fn start_was_explicitly_refused(error: &Error) -> bool {
    matches!(
        error,
        Error::Vendor(vendor) if vendor.vendor_code.as_deref() != Some("-32000")
    )
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use mango_external_agents::Attachment;
    use mango_external_agents::error::VendorError;

    use mango_external_agents::event::{EventKind, TurnId};
    use mango_external_agents::jsonrpc::{Client, ClientOptions, PeerTermination};
    use mango_external_agents::process::ProcessLauncher;
    use mango_external_agents::session::{
        ATTACHMENT_MAX_BYTES, AttachmentKind, Session, SessionIds, TURN_MAX_ATTACHMENTS,
        TurnRequest,
    };
    use mango_external_agents::state::{SessionSnapshot, SessionState, TransportSelection};
    use mango_external_agents::stream::EventSink;
    use mango_external_agents::testing::{FakeLauncher, FakeProcess, ScriptedLink};
    use mango_external_agents::transport::TransportKind;
    use mango_external_agents::{
        Capabilities, HarnessIdentity, HostContext, LaunchSpec, Limits, SessionCapabilities,
    };

    use super::{
        ActiveTurn, Answer, CodexHandler, ResolutionMarkerGate, Shared, StopState, base64,
        data_url, supports_image_mime_type,
    };
    use crate::reducer::Outcome;
    use mango_external_agents::jsonrpc::PeerHandler;

    #[test]
    fn accepted_configuration_orders_each_explicit_axis_independently() {
        use mango_external_agents::{ApprovalRouting, Configuration, PermissionLevel};
        let mut state = super::AcceptedConfiguration {
            accepted: Configuration::unknown(),
            next_generation: 0,
            accepted_generation: [0; 4],
        };
        state.accept(2, Configuration::unknown().with_model("new-model"));
        state.accept(
            1,
            Configuration::unknown()
                .with_level(PermissionLevel::FullAccess)
                .with_routing(ApprovalRouting::User),
        );
        assert_eq!(state.accepted.model.as_deref(), Some("new-model"));
        assert_eq!(state.accepted.level, Some(PermissionLevel::FullAccess));
        state.accept(
            3,
            Configuration::unknown()
                .with_level(PermissionLevel::ReadOnly)
                .with_effort("high"),
        );
        state.accept(
            1,
            Configuration::unknown()
                .with_model("old-model")
                .with_level(PermissionLevel::FullAccess)
                .with_effort("low"),
        );
        state.accept(4, Configuration::unknown());
        assert_eq!(state.accepted.model.as_deref(), Some("new-model"));
        assert_eq!(state.accepted.effort.as_deref(), Some("high"));
        assert_eq!(state.accepted.level, Some(PermissionLevel::ReadOnly));
        assert_eq!(state.accepted.routing, Some(ApprovalRouting::User));
    }

    /// A host with a launcher that spawns nothing, for the state these tests drive directly.
    fn shared() -> Arc<Shared> {
        let host = HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd("/workspace")
            .client_info("mango-test", "0.0.1")
            .build()
            .expect("expected a host");
        Arc::new(Shared::new(
            host,
            mango_external_agents::SessionId::new("chat-1"),
        ))
    }

    /// A direct-handler host with a deliberately tight request budget.
    fn shared_with_pending_limit(max_pending_requests: usize) -> Arc<Shared> {
        let host = HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd("/workspace")
            .client_info("mango-test", "0.0.1")
            .limits(Limits {
                max_pending_requests,
                ..Limits::default()
            })
            .build()
            .expect("expected a host");
        Arc::new(Shared::new(
            host,
            mango_external_agents::SessionId::new("chat-1"),
        ))
    }

    /// Builds the production session surface used to submit a question answer in a direct-handler
    /// race test.
    async fn question_session(shared: Arc<Shared>) -> super::CodexSession {
        let snapshot = SessionSnapshot::opening(
            SessionIds {
                session_id: mango_external_agents::SessionId::new("chat-1"),
                native_session_id: String::from("thread-1"),
            },
            HarnessIdentity::codex(),
            TransportSelection::new(None, TransportKind::Stdio),
            shared.host.now(),
        )
        .with_capabilities(SessionCapabilities::new(Capabilities {
            questions: true,
            ..Capabilities::none()
        }));
        let state = SessionState::new(Arc::clone(shared.host.clock()), snapshot);
        let client = Arc::new(Client::connect(
            ScriptedLink::new().into_link(),
            super::CodexSession::handler(Arc::clone(&shared)),
            ClientOptions::new("Codex app-server"),
        ));
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let control = launcher
            .spawn(LaunchSpec {
                argv: vec![String::from("codex")],
                cwd: std::path::PathBuf::from("/workspace"),
                env: BTreeMap::new(),
                stdin: false,
                hide_window: true,
            })
            .await
            .expect("expected a persistent fake app-server")
            .control;
        super::CodexSession::new(state, shared, client, control)
    }

    /// Reproduces the map removal that `Session::respond` performs before the server confirms it.
    async fn answer_waiting_approval(shared: &Shared) {
        loop {
            let entry = {
                let mut pending = shared.pending.lock().await;
                let id = pending.keys().next().cloned();
                id.and_then(|id| pending.remove(&id))
            };
            if let Some(entry) = entry {
                let decision = entry.pending.refusal();
                let option_id = decision.option_id().to_owned();
                assert!(
                    entry
                        .answer
                        .send(Answer::Chosen {
                            decision,
                            option_id,
                            source: mango_external_agents::DecisionSource::User,
                            reported: false,
                        })
                        .is_ok(),
                    "expected the handler to still await the host answer"
                );
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Reproduces the accepted settlement [`Session::answer`] retains until it is published.
    async fn answer_waiting_question(shared: &Shared) {
        loop {
            let response = {
                let _settlement = shared.question_settlements.lock().await;
                let mut pending = shared.pending_questions.lock().await;
                let id = pending.keys().next().cloned();
                id.map(|id| {
                    let entry = pending
                        .get_mut(&id)
                        .expect("expected the pending question id to stay present");
                    let response = mango_external_agents::QuestionResponse::new(
                        entry.request.interaction.id.clone(),
                        vec![mango_external_agents::Answer::new(
                            mango_external_agents::QuestionId::new("note"),
                            mango_external_agents::AnswerValue::text("keep this answer"),
                        )],
                    );
                    assert!(
                        entry.request.validate(&response).is_ok(),
                        "expected the direct answer to satisfy the round Session::answer validates"
                    );
                    let answer = entry
                        .answer
                        .take()
                        .expect("expected the test question to still accept one answer");
                    entry.settlement = Some(mango_external_agents::QuestionOutcome::Answered {
                        answers: response.answers.clone(),
                    });
                    assert!(
                        answer
                            .send(super::QuestionAnswer::Answered(response.clone()))
                            .is_ok(),
                        "expected the handler to still await the host answer"
                    );
                    response
                })
            };
            if let Some(response) = response {
                assert_eq!(
                    response.answers.len(),
                    1,
                    "expected one accepted test answer"
                );
                return;
            }
            tokio::task::yield_now().await;
        }
    }

    /// Sends one approval through the direct handler and answers it like `Session::respond`.
    async fn request_and_answer(shared: &Arc<Shared>, native_turn_id: &str, request_id: u8) {
        let request_shared = Arc::clone(shared);
        let native_turn_id = native_turn_id.to_owned();
        let request = tokio::spawn(async move {
            CodexHandler {
                shared: request_shared,
            }
            .on_request(
                String::from("item/commandExecution/requestApproval"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": native_turn_id,
                    "itemId": format!("item-{request_id}"), "command": "pwd"
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(request_id)),
            )
            .await
        });
        answer_waiting_approval(shared).await;
        let _ = request.await.expect("expected the approval request task");
    }

    /// Installs a turn the way `begin` does, and hands back the stream a host would hold.
    async fn running(
        shared: &Shared,
        native_turn_id: &str,
    ) -> (TurnId, mango_external_agents::stream::TurnStream) {
        running_as(
            shared,
            native_turn_id,
            mango_external_agents::operation::AttemptId::default(),
        )
        .await
    }

    /// The same, for a named dispatch of that turn: what a host's retry installs in the slot.
    async fn running_as(
        shared: &Shared,
        native_turn_id: &str,
        attempt: mango_external_agents::operation::AttemptId,
    ) -> (TurnId, mango_external_agents::stream::TurnStream) {
        let turn_id = TurnId::new("turn-1");
        let (sink, events) = EventSink::new(
            mango_external_agents::SessionId::new("chat-1"),
            turn_id.clone(),
            attempt,
            Arc::clone(shared.host.clock()),
            8,
        );
        *shared.turn.lock().await = Some(ActiveTurn {
            owner: Arc::new(()),
            sink,
            turn_id: turn_id.clone(),
            attempt,
            native_turn_id: native_turn_id.to_owned(),
            // This helper installs a turn already past the point `begin` would have announced it.
            announced: true,
            earlier_native_turn_ids: std::collections::VecDeque::new(),
            reducer: crate::turn_reducer::TurnReducer::new(),
            is_review: false,
            start_unanswerable: false,
            interrupt_dispatched: false,
            cancel_reason: None,
            abandonment_watcher: None,
            idle_watcher: None,
            stop: Arc::new(StopState::new()),
            finishing: false,
        });
        (
            turn_id.clone(),
            mango_external_agents::stream::TurnStream::accepted(
                turn_id,
                attempt,
                native_turn_id.to_owned(),
                events,
            ),
        )
    }

    fn held_delta(delta: &str) -> serde_json::Value {
        serde_json::json!({"threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "m",
                           "delta": delta})
    }

    /// Reads the next answer text off a turn, or fails naming what was expected.
    async fn next_text(stream: &mut mango_external_agents::stream::TurnStream, expected: &str) {
        let text = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while let Some(event) = stream.recv().await {
                if let EventKind::TextDelta { text } = event.kind {
                    return Some(text);
                }
            }
            None
        })
        .await
        .ok()
        .flatten();
        assert_eq!(
            text.as_deref(),
            Some(expected),
            "expected the next answer text on the turn"
        );
    }

    /// A host may drop its steer future while the hold is being replayed, through a timeout
    /// wrapper or a disconnect. The replay must still finish and routing must go live again, or
    /// every later frame — the terminal included — is queued forever.
    #[tokio::test]
    async fn dropping_a_steer_mid_release_still_replays_and_reopens_routing_in_order() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;
        let handler = super::CodexHandler {
            shared: Arc::clone(&shared),
        };
        let guard = super::SteerHoldGuard::hold(&shared);
        handler
            .on_notification(String::from("item/agentMessage/delta"), held_delta("a"))
            .await;

        // The replay needs the turn lock, so holding it here makes the release pend; the
        // timeout then drops the release future mid-way, as a host's wrapper would.
        let turn = shared.turn.lock().await;
        let dropped =
            tokio::time::timeout(std::time::Duration::from_millis(20), guard.release()).await;
        assert!(dropped.is_err(), "expected the release to be cut off");
        drop(turn);

        handler
            .on_notification(String::from("item/agentMessage/delta"), held_delta("b"))
            .await;
        next_text(&mut stream, "a").await;
        next_text(&mut stream, "b").await;
    }

    /// Held frames are outside the connection's own byte budget, so the hold has one of its own.
    #[tokio::test]
    async fn a_steer_hold_past_its_byte_budget_fails_the_session_cleanly() {
        let host = HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd("/workspace")
            .client_info("mango-test", "0.0.1")
            .limits(Limits {
                turn_buffer_bytes: 4096,
                ..Limits::default()
            })
            .build()
            .expect("expected a host");
        let shared = Arc::new(Shared::new(
            host,
            mango_external_agents::SessionId::new("chat-1"),
        ));
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;
        let handler = super::CodexHandler {
            shared: Arc::clone(&shared),
        };
        let _guard = super::SteerHoldGuard::hold(&shared);
        for _ in 0..8 {
            handler
                .on_notification(
                    String::from("item/agentMessage/delta"),
                    held_delta(&"x".repeat(1024)),
                )
                .await;
        }
        assert!(
            shared.is_shutting_down(),
            "expected a hold past its byte budget to fail the session"
        );
    }

    /// A slow steer answer must not let the idle deadline kill a turn whose progress is queued.
    #[tokio::test]
    async fn a_held_frame_for_this_conversation_still_counts_as_progress() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;
        let handler = super::CodexHandler {
            shared: Arc::clone(&shared),
        };
        let changes = shared.idle_changes.subscribe();
        let _guard = super::SteerHoldGuard::hold(&shared);
        handler
            .on_notification(String::from("item/agentMessage/delta"), held_delta("a"))
            .await;
        assert!(
            changes.has_changed().unwrap_or(false),
            "expected a held frame for this conversation to restart the idle deadline"
        );
    }

    /// The trap the running-turn guard exists to close. A host that drops its stream stops
    /// reading; it does not stop the vendor, whose turn runs on. Giving up the slot here would let
    /// the next `turn/start` out, and the app-server reads that as a steer of the live turn.
    #[tokio::test]
    async fn an_event_no_host_will_read_does_not_give_up_the_running_turn() {
        let shared = shared();
        let (_turn_id, stream) = running(&shared, "vendor-turn-1").await;
        drop(stream);

        let _ = shared
            .emit(EventKind::TextDelta {
                text: String::from("nobody is reading this"),
            })
            .await;

        assert!(
            shared.turn.lock().await.is_some(),
            "expected the vendor's turn to still hold the slot"
        );
    }

    #[tokio::test]
    async fn unexpected_exit_reports_status_and_redacted_stderr_when_available() {
        use mango_external_agents::process::{ExitStatus, LaunchSpec, ProcessLauncher};
        use mango_external_agents::testing::FakeProcess;

        let launcher = FakeLauncher::new();
        launcher.push(
            FakeProcess::transcript(Vec::<String>::new())
                .with_exit(ExitStatus {
                    code: Some(23),
                    signal: None,
                })
                .with_stderr("fatal: API_KEY=sk-private-value"),
        );
        let mut child = launcher
            .spawn(LaunchSpec {
                argv: vec![String::from("codex"), String::from("app-server")],
                cwd: "/workspace".into(),
                env: Default::default(),
                stdin: true,
                hide_window: true,
            })
            .await
            .expect("fake process");
        assert!(
            child
                .stdout
                .next_chunk()
                .await
                .expect("stdout EOF")
                .is_none()
        );
        let shared = shared();
        assert!(shared.control.set(child.control).is_ok());
        let (_, mut stream) = running(&shared, "turn-1").await;
        shared.connection_terminated(PeerTermination::Exited).await;
        let event = stream.recv().await.expect("terminal error");
        let EventKind::Error { error } = event.kind else {
            panic!("expected terminal error")
        };
        assert!(
            error.message.contains("code=23"),
            "expected received exit code, got {}",
            error.message
        );
        assert!(
            error.message.contains("fatal:"),
            "expected stderr diagnostic, got {}",
            error.message
        );
        assert!(
            !error.message.contains("sk-private-value"),
            "expected credential-redacted stderr"
        );
    }

    /// A delayed frame from a completed turn must not render into or complete its replacement.
    #[tokio::test]
    async fn stale_native_turn_notifications_do_not_reach_a_reused_turn_slot() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, mut first) = running(&shared, "vendor-turn-1").await;

        handler
            .on_notification(
                String::from("turn/completed"),
                serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {"id": "vendor-turn-1", "status": "completed"}
                }),
            )
            .await;
        assert!(
            matches!(
                first.recv().await.map(|event| event.kind),
                Some(EventKind::Completed)
            ),
            "expected the first turn to complete"
        );

        let (_turn_id, mut second) = running(&shared, "vendor-turn-2").await;
        for (method, params) in [
            (
                "item/agentMessage/delta",
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "item-1",
                    "delta": "stale"
                }),
            ),
            (
                "turn/completed",
                serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {"id": "vendor-turn-1", "status": "completed"}
                }),
            ),
            (
                "turn/completed",
                serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {"id": "vendor-turn-1", "status": 7}
                }),
            ),
        ] {
            handler.on_notification(String::from(method), params).await;
        }

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), second.recv())
                .await
                .is_err(),
            "expected stale events to stay out of the replacement stream"
        );
        assert!(
            shared.turn.lock().await.is_some(),
            "expected the delayed completion not to free the replacement slot"
        );
    }

    /// A malformed terminal that names this turn fails it instead of leaving its stream open.
    #[tokio::test]
    async fn a_malformed_terminal_fails_its_addressed_turn_and_clears_the_slot() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        handler
            .on_notification(
                String::from("turn/completed"),
                serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {"id": "vendor-turn-1", "status": 7}
                }),
            )
            .await;

        let event = stream.recv().await.expect("expected a protocol failure");
        let EventKind::Error { error } = event.kind else {
            panic!("expected a protocol error, received {:?}", event.kind);
        };
        assert_eq!(error.code.as_str(), "codex-protocol-error");
        assert!(
            shared.turn.lock().await.is_none(),
            "expected malformed completion to clear the active slot"
        );
    }

    /// A terminal with no recoverable route must fail the only active stream and poison the
    /// connection. Ignoring it would leave the slot occupied forever.
    #[tokio::test]
    async fn an_unroutable_malformed_terminal_fails_the_active_turn() {
        let shared = shared();
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        handler
            .on_notification(
                String::from("turn/completed"),
                serde_json::json!({"unexpected": true}),
            )
            .await;

        let event = tokio::time::timeout(std::time::Duration::from_millis(10), stream.recv())
            .await
            .expect("expected the malformed terminal to end the stream")
            .expect("expected a protocol error");
        assert!(matches!(event.kind, EventKind::Error { .. }));
        assert!(
            shared.is_shutting_down(),
            "expected an unroutable terminal to poison the connection"
        );
    }

    /// The start response can set the native id after a terminal handler snapshots the empty
    /// pre-response id. That terminal still belongs to this start attempt.
    #[tokio::test]
    async fn a_completion_keeps_its_start_attempt_when_the_response_sets_its_native_id() {
        let shared = shared();
        let (_turn_id, mut stream) = running(&shared, "").await;
        let route = shared
            .active_turn_route()
            .await
            .expect("expected an active turn route");
        {
            let mut turn = shared.turn.lock().await;
            turn.as_mut()
                .expect("expected an active turn")
                .native_turn_id = String::from("vendor-turn-1");
        }

        shared
            .finish_for(
                &route,
                Some("vendor-turn-1"),
                Outcome::Finish {
                    events: Vec::new(),
                    cancelled: None,
                    failure: None,
                },
            )
            .await;

        assert!(
            matches!(
                tokio::time::timeout(std::time::Duration::from_millis(10), stream.recv())
                    .await
                    .ok()
                    .flatten()
                    .map(|event| event.kind),
                Some(EventKind::Completed)
            ),
            "expected the completion to survive the response race"
        );
    }

    /// Completion retains admission until its terminal is committed, even when nobody drains it.
    #[tokio::test]
    async fn completion_commits_its_terminal_before_releasing_admission() {
        let shared = shared();
        let (_turn_id, stream) = running(&shared, "vendor-turn-1").await;
        let route = shared
            .active_turn_route()
            .await
            .expect("expected an active turn route");
        let completed = Arc::clone(&shared);
        let held_completion_log = shared.recent_completed_turns.lock().await;
        let completion = tokio::spawn(async move {
            completed
                .finish_for(
                    &route,
                    Some("vendor-turn-1"),
                    Outcome::Finish {
                        events: Vec::new(),
                        cancelled: None,
                        failure: None,
                    },
                )
                .await;
        });

        for _ in 0..100 {
            if shared
                .turn
                .lock()
                .await
                .as_ref()
                .is_some_and(|active| active.finishing)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            shared.turn.lock().await.is_some(),
            "expected a terminal in progress to retain admission"
        );
        assert!(
            stream.terminal_status().is_none(),
            "expected the held completion to keep the terminal uncommitted"
        );

        drop(held_completion_log);
        completion
            .await
            .expect("expected completion task to finish");
        assert!(
            stream.terminal_status().is_some(),
            "expected terminal status without consuming transcript events"
        );
        assert!(
            shared.turn.lock().await.is_none(),
            "expected admission release after terminal commitment"
        );
    }

    /// A delayed frame for the turn that just ended cannot claim the next start before its
    /// response gives that start a native id.
    #[tokio::test]
    async fn a_recently_completed_native_turn_cannot_claim_a_pending_start() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        shared.remember_completed("vendor-turn-1").await;
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, mut pending_start) = running(&shared, "").await;

        handler
            .on_notification(
                String::from("turn/completed"),
                serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {"id": "vendor-turn-1", "status": "completed"}
                }),
            )
            .await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), pending_start.recv(),)
                .await
                .is_err(),
            "expected the delayed terminal to stay out of the pending start"
        );
        assert!(
            shared.turn.lock().await.is_some(),
            "expected the delayed terminal not to clear the pending start"
        );
    }

    /// An approval from a completed turn on this same thread must never wait for the new turn's
    /// host, even while the server still has its request task alive.
    #[tokio::test]
    async fn a_same_thread_approval_for_another_native_turn_is_refused() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, _stream) = running(&shared, "vendor-turn-2").await;

        let outcome = tokio::time::timeout(
            std::time::Duration::from_millis(10),
            handler.on_request(
                String::from("item/commandExecution/requestApproval"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "item-1",
                    "command": "pwd"
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            ),
        )
        .await
        .expect("expected a stale approval to be refused without waiting");
        assert!(
            matches!(
                outcome,
                mango_external_agents::jsonrpc::ServerRequestOutcome::Failure(_)
            ),
            "expected a stale approval refusal, received {outcome:?}"
        );
        assert!(
            shared.pending.lock().await.is_empty(),
            "expected a stale approval not to register a host-visible prompt"
        );
    }

    /// A request that arrives after cancellation is latched cannot enter a broker that might
    /// automatically allow it; its JSON-RPC outcome is a refusal and no host prompt is retained.
    #[tokio::test]
    async fn a_late_approval_after_cancellation_is_refused_before_broker_admission() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;
        shared
            .turn
            .lock()
            .await
            .as_mut()
            .expect("expected an active turn")
            .cancel_reason = Some(mango_external_agents::CancelReason::Requested);

        let outcome = handler
            .on_request(
                String::from("item/commandExecution/requestApproval"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "item-1",
                    "command": "pwd"
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await;

        assert!(
            matches!(
                outcome,
                mango_external_agents::jsonrpc::ServerRequestOutcome::Failure(_)
            ),
            "expected late approval to be refused instead of allowed, received {outcome:?}"
        );
        assert!(
            shared.pending.lock().await.is_empty(),
            "expected a cancelled owner not to retain a broker-visible approval"
        );
    }

    /// A shutdown that empties the round before the waiter is polled resolves it once.
    ///
    /// `release_pending_for` sends `QuestionAnswer::Cancelled { reported: true }` on the pending
    /// entry's oneshot before this waiter can be polled again, so once the schedule below forces
    /// that ordering the waiting task's `select!` — biased toward `waiting`, not the cancel token
    /// — takes the oneshot's `Cancelled` arm rather than the nobody-answered `None` arm. The
    /// `reported` bit is what keeps that arm from publishing a second time: the canceller already
    /// reported for this round, so the waiter reports nothing.
    ///
    /// The schedule is forced rather than hoped for. `release_pending_for` empties the map before
    /// its first real yield, so on a current-thread runtime the waiting task cannot be polled in
    /// between, and the losing order is the one this test always runs.
    #[tokio::test]
    async fn a_shutdown_that_empties_the_round_first_resolves_it_exactly_once() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;
        let owner = Arc::clone(
            &shared
                .turn
                .lock()
                .await
                .as_ref()
                .expect("expected an active turn")
                .owner,
        );

        let waiting_shared = Arc::clone(&shared);
        let waiter = tokio::spawn(async move {
            CodexHandler {
                shared: waiting_shared,
            }
            .on_request(
                String::from("item/tool/requestUserInput"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "ask-1",
                    "isBlocking": false,
                    "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await
        });
        while shared.pending_questions.lock().await.is_empty() {
            tokio::task::yield_now().await;
        }

        shared.host.cancel().cancel();
        shared
            .release_pending_for(
                Some(&owner),
                mango_external_agents::DecisionSource::Cancelled,
            )
            .await;
        let _ = waiter.await.expect("expected the question task to finish");

        let mut resolutions = 0;
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_millis(10), stream.recv()).await
        {
            if matches!(event.kind, EventKind::QuestionResolved { .. }) {
                resolutions += 1;
            }
        }
        assert_eq!(
            resolutions, 1,
            "expected the shutdown to resolve the round exactly once"
        );
    }

    /// An early approval withdrawal wakes the idle watcher after it removes its pending entry.
    #[tokio::test]
    async fn an_early_approval_withdrawal_wakes_the_idle_watcher() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };

        handler
            .on_notification(
                String::from("serverRequest/resolved"),
                serde_json::json!({"threadId": "thread-1", "requestId": 1}),
            )
            .await;
        let before_request = *shared.idle_changes.borrow();

        let outcome = handler
            .on_request(
                String::from("item/commandExecution/requestApproval"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "item-1",
                    "command": "pwd"
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await;

        assert!(
            matches!(
                outcome,
                mango_external_agents::jsonrpc::ServerRequestOutcome::Answer(_)
            ),
            "expected an early withdrawal to be answered without waiting, received {outcome:?}"
        );
        assert_eq!(
            *shared.idle_changes.borrow(),
            before_request + 2,
            "expected registration and early-entry removal to each wake the idle watcher"
        );
    }

    /// An early question withdrawal wakes the idle watcher after it removes its pending entry.
    #[tokio::test]
    async fn an_early_question_withdrawal_wakes_the_idle_watcher() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };

        handler
            .on_notification(
                String::from("serverRequest/resolved"),
                serde_json::json!({"threadId": "thread-1", "requestId": 1}),
            )
            .await;
        let before_request = *shared.idle_changes.borrow();

        let outcome = handler
            .on_request(
                String::from("item/tool/requestUserInput"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "ask-1",
                    "isBlocking": false,
                    "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await;

        assert!(
            matches!(
                outcome,
                mango_external_agents::jsonrpc::ServerRequestOutcome::Answer(_)
            ),
            "expected an early withdrawal to be answered without waiting, received {outcome:?}"
        );
        assert_eq!(
            *shared.idle_changes.borrow(),
            before_request + 2,
            "expected registration and early-entry removal to each wake the idle watcher"
        );
    }

    /// A server withdrawal after announcement must close the host-visible round too.
    #[tokio::test]
    async fn a_server_withdrawal_resolves_an_announced_question_round() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };

        let request_shared = Arc::clone(&shared);
        let request = tokio::spawn(async move {
            CodexHandler {
                shared: request_shared,
            }
            .on_request(
                String::from("item/tool/requestUserInput"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "ask-1",
                    "isBlocking": false,
                    "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await
        });

        let asked = stream.recv().await.expect("expected an announced question");
        assert!(
            matches!(asked.kind, EventKind::QuestionAsked { .. }),
            "expected a question announcement, received {asked:?}"
        );

        handler
            .on_notification(
                String::from("serverRequest/resolved"),
                serde_json::json!({"threadId": "thread-1", "requestId": 1}),
            )
            .await;
        let _ = request.await.expect("expected the question task to finish");

        let resolved = tokio::time::timeout(std::time::Duration::from_millis(100), stream.recv())
            .await
            .expect("expected the withdrawn question to resolve for the host")
            .expect("expected the question stream to remain open");
        assert!(
            matches!(
                resolved.kind,
                EventKind::QuestionResolved {
                    ref interaction_id,
                    outcome: mango_external_agents::QuestionOutcome::Cancelled,
                } if interaction_id.as_str() == "ask-1"
            ),
            "expected the server withdrawal to cancel the announced question, received {resolved:?}"
        );
    }

    /// An accepted answer wins over a shutdown that becomes ready in the same poll.
    #[tokio::test]
    async fn an_answer_accepted_before_shutdown_is_still_returned_and_reported() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        let request_shared = Arc::clone(&shared);
        let request = tokio::spawn(async move {
            CodexHandler {
                shared: request_shared,
            }
            .on_request(
                String::from("item/tool/requestUserInput"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "ask-1",
                    "isBlocking": false,
                    "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await
        });

        let asked = stream.recv().await.expect("expected an announced question");
        assert!(matches!(asked.kind, EventKind::QuestionAsked { .. }));
        answer_waiting_question(&shared).await;
        // Neither operation above yields after the answer reaches the one-shot channel, so the
        // request task observes both branches as ready on its next poll.
        shared.host.cancel().cancel();

        let mango_external_agents::jsonrpc::ServerRequestOutcome::Answer(answer) =
            request.await.expect("expected the question task to finish")
        else {
            panic!("expected the accepted question answer on the wire");
        };
        assert_eq!(
            answer,
            serde_json::json!({"answers": {"note": {"answers": ["keep this answer"]}}}),
            "expected shutdown not to discard the accepted answer"
        );

        let resolved = stream
            .recv()
            .await
            .expect("expected the accepted answer to resolve for the host");
        assert!(
            matches!(
                resolved.kind,
                EventKind::QuestionResolved {
                    ref interaction_id,
                    outcome: mango_external_agents::QuestionOutcome::Answered { ref answers },
                } if interaction_id.as_str() == "ask-1" && answers.len() == 1
            ),
            "expected the accepted answer to remain the host-visible outcome, received {resolved:?}"
        );
    }

    /// An answer naming a dispatch the host has already replaced cannot mutate its replacement.
    ///
    /// The round is announced while attempt 1 owns the turn; attempt 2 then takes the slot, which
    /// is what a host retrying the same logical turn installs. The answer still names attempt 1's
    /// round, and applying it there would settle a round on behalf of the attempt that replaced
    /// it — the round the host is actually being shown.
    #[tokio::test]
    async fn an_answer_naming_a_superseded_attempt_is_refused() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        let request_shared = Arc::clone(&shared);
        let request = tokio::spawn(async move {
            CodexHandler {
                shared: request_shared,
            }
            .on_request(
                String::from("item/tool/requestUserInput"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "ask-1",
                    "isBlocking": false,
                    "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await
        });
        let asked = stream.recv().await.expect("expected an announced question");
        assert!(matches!(asked.kind, EventKind::QuestionAsked { .. }));

        let (_turn_id, _replacement) = running_as(
            &shared,
            "vendor-turn-2",
            mango_external_agents::operation::AttemptId::new(2),
        )
        .await;

        let session = question_session(Arc::clone(&shared)).await;
        let error = session
            .answer(mango_external_agents::QuestionResponse::new(
                mango_external_agents::InteractionId::new("ask-1"),
                vec![mango_external_agents::Answer::new(
                    mango_external_agents::QuestionId::new("note"),
                    mango_external_agents::AnswerValue::text("answering the attempt that is gone"),
                )],
            ))
            .await
            .expect_err("expected the superseded round's answer to be refused");
        assert!(
            matches!(
                error.cause(),
                mango_external_agents::Error::Protocol { received, .. }
                    if received == "an answer naming a superseded attempt"
            ),
            "expected a refusal naming the superseded attempt, received {error:?}"
        );

        let pending = shared.pending_questions.lock().await;
        let entry = pending
            .get(&mango_external_agents::InteractionId::new("ask-1"))
            .expect("expected the refused round to survive untouched");
        assert!(
            entry.answer.is_some(),
            "expected the refused answer not to consume the round's waiter"
        );
        assert!(
            entry.settlement.is_none(),
            "expected the refused answer not to settle the round, received {:?}",
            entry.settlement
        );
        drop(pending);

        shared
            .release_pending_for(None, mango_external_agents::DecisionSource::Cancelled)
            .await;
        let _ = request.await.expect("expected the question task to finish");
    }

    /// A terminal cannot overtake the resolution of an answer [`Session::answer`] accepted.
    #[tokio::test]
    async fn a_completed_turn_waits_for_a_session_answer_to_resolve() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;
        let gate = ResolutionMarkerGate::closed();
        *shared.question_settlement_gate.lock().await = Some(Arc::clone(&gate));

        let request_shared = Arc::clone(&shared);
        let request = tokio::spawn(async move {
            CodexHandler {
                shared: request_shared,
            }
            .on_request(
                String::from("item/tool/requestUserInput"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "ask-1",
                    "isBlocking": false,
                    "questions": [{"id": "note", "header": "Note", "question": "Anything else?"}],
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await
        });
        let asked = stream.recv().await.expect("expected an announced question");
        assert!(matches!(asked.kind, EventKind::QuestionAsked { .. }));

        let session = question_session(Arc::clone(&shared)).await;
        session
            .answer(mango_external_agents::QuestionResponse::new(
                mango_external_agents::InteractionId::new("ask-1"),
                vec![mango_external_agents::Answer::new(
                    mango_external_agents::QuestionId::new("note"),
                    mango_external_agents::AnswerValue::text("keep this answer"),
                )],
            ))
            .await
            .expect("expected Session::answer to accept the announced round");
        gate.wait_until_entered().await;

        let terminal_shared = Arc::clone(&shared);
        let terminal = tokio::spawn(async move {
            CodexHandler {
                shared: terminal_shared,
            }
            .on_notification(
                String::from("turn/completed"),
                serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {"id": "vendor-turn-1", "status": "completed"}
                }),
            )
            .await;
        });
        terminal
            .await
            .expect("expected terminal cleanup to publish the accepted settlement");
        gate.open();

        let mango_external_agents::jsonrpc::ServerRequestOutcome::Answer(answer) =
            request.await.expect("expected the question task to finish")
        else {
            panic!("expected the accepted question answer on the wire");
        };
        assert_eq!(
            answer,
            serde_json::json!({"answers": {"note": {"answers": ["keep this answer"]}}})
        );
        assert!(
            matches!(
                stream.recv().await.map(|event| event.kind),
                Some(EventKind::QuestionResolved {
                    outcome: mango_external_agents::QuestionOutcome::Answered { .. },
                    ..
                })
            ),
            "expected the accepted answer to resolve before the terminal"
        );
        assert!(
            matches!(
                stream.recv().await.map(|event| event.kind),
                Some(EventKind::Completed)
            ),
            "expected completion after the accepted answer resolution"
        );
    }

    /// Cancellation cleanup owns the approval resolution it already published.
    #[tokio::test]
    async fn a_shutdown_that_empties_an_approval_first_resolves_it_exactly_once() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;
        let owner = Arc::clone(
            &shared
                .turn
                .lock()
                .await
                .as_ref()
                .expect("expected an active turn")
                .owner,
        );

        let waiting_shared = Arc::clone(&shared);
        let waiter = tokio::spawn(async move {
            CodexHandler {
                shared: waiting_shared,
            }
            .on_request(
                String::from("item/commandExecution/requestApproval"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "item-1",
                    "command": "pwd"
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await
        });
        let asked = stream.recv().await.expect("expected an announced approval");
        assert!(matches!(asked.kind, EventKind::ApprovalRequested { .. }));

        shared.host.cancel().cancel();
        shared
            .release_pending_for(
                Some(&owner),
                mango_external_agents::DecisionSource::Cancelled,
            )
            .await;
        let _ = waiter.await.expect("expected the approval task to finish");

        let mut resolutions = 0;
        while let Ok(event) = stream.try_recv() {
            if matches!(event.kind, EventKind::ApprovalResolved { .. }) {
                resolutions += 1;
            }
        }
        assert_eq!(
            resolutions, 1,
            "expected cancellation cleanup to resolve the approval exactly once"
        );
    }

    /// An elicitation with no turn to belong to is still declined on the wire.
    ///
    /// The decline is what stops the app-server waiting; a missing correlation is not a reason to
    /// hand it the JSON-RPC error this family exists to avoid. There is nothing to report it on,
    /// so nothing is reported.
    #[tokio::test]
    async fn an_elicitation_outside_a_turn_is_declined_and_recorded_nowhere() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };

        let outcome = handler
            .on_request(
                String::from("mcpServer/elicitation/request"),
                serde_json::json!({
                    "threadId": "thread-1",
                    "mode": "url",
                    "elicitationId": "elicit-1",
                    "url": "https://example.test/form",
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await;

        let mango_external_agents::jsonrpc::ServerRequestOutcome::Answer(answer) = outcome else {
            panic!("expected a native decline outside a turn, received {outcome:?}");
        };
        assert_eq!(answer, serde_json::json!({"action": "decline"}));
    }

    /// An elicitation naming a turn other than the active one is declined without being recorded.
    #[tokio::test]
    async fn an_elicitation_for_another_turn_is_declined_without_an_event() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        let outcome = handler
            .on_request(
                String::from("mcpServer/elicitation/request"),
                serde_json::json!({
                    "threadId": "thread-1",
                    "turnId": "vendor-turn-2",
                    "elicitationId": "elicit-1",
                    "requestedSchema": {"type": "object"},
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(1)),
            )
            .await;

        let mango_external_agents::jsonrpc::ServerRequestOutcome::Answer(answer) = outcome else {
            panic!("expected a native decline for a foreign turn, received {outcome:?}");
        };
        assert_eq!(answer, serde_json::json!({"action": "decline"}));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), stream.recv())
                .await
                .is_err(),
            "expected another turn's form to be recorded nowhere on this one"
        );
    }

    /// A resolution for a child thread cannot plant an early tombstone that later consumes a
    /// parent approval with the same JSON-RPC request id.
    #[tokio::test]
    async fn a_foreign_resolution_does_not_create_an_early_tombstone() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;

        handler
            .on_notification(
                String::from("serverRequest/resolved"),
                serde_json::json!({"threadId": "child-thread", "requestId": 7}),
            )
            .await;

        assert!(
            shared.resolution_markers.lock().await.early.is_empty(),
            "expected a foreign resolution not to affect this session's approval lifecycle"
        );
    }

    /// A bounded early-resolution set fails the active turn when it fills. Silently dropping a
    /// parent resolution would leave the server request task waiting until its deadline.
    #[tokio::test]
    async fn an_early_resolution_overflow_poisons_the_active_session() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;
        {
            let mut markers = shared.resolution_markers.lock().await;
            for index in 0..shared.host.limits().max_pending_requests {
                markers.early.insert(index.to_string(), Arc::new(()));
            }
        }

        handler
            .on_notification(
                String::from("serverRequest/resolved"),
                serde_json::json!({"threadId": "thread-1", "requestId": 999}),
            )
            .await;

        let event = stream.recv().await.expect("expected a protocol failure");
        assert!(matches!(event.kind, EventKind::Error { .. }));
        assert!(
            shared.is_shutting_down(),
            "expected overflow to poison the session rather than lose a resolution"
        );
    }

    /// The server confirms every host answer with `serverRequest/resolved`. Those confirmations
    /// must not consume the early-resolution budget while a long turn keeps asking questions.
    #[tokio::test]
    async fn normal_resolutions_do_not_accumulate_during_one_long_turn() {
        let shared = shared_with_pending_limit(1);
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;

        for request_id in [0, 1] {
            request_and_answer(&shared, "vendor-turn-1", request_id).await;
            handler
                .on_notification(
                    String::from("serverRequest/resolved"),
                    serde_json::json!({"threadId": "thread-1", "requestId": request_id}),
                )
                .await;
        }

        assert!(
            !shared.is_shutting_down(),
            "expected normal resolution traffic to leave capacity for later approvals"
        );
        assert!(
            shared.resolution_markers.lock().await.early.is_empty(),
            "expected host answers to leave no early-resolution tombstones"
        );
    }

    /// Confirmed answers from a completed turn leave the next turn's bounded request budget free.
    #[tokio::test]
    async fn normal_resolutions_do_not_spend_the_next_turns_budget() {
        let shared = shared_with_pending_limit(1);
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, _first) = running(&shared, "vendor-turn-1").await;

        request_and_answer(&shared, "vendor-turn-1", 0).await;
        handler
            .on_notification(
                String::from("serverRequest/resolved"),
                serde_json::json!({"threadId": "thread-1", "requestId": 0}),
            )
            .await;
        handler
            .on_notification(
                String::from("turn/completed"),
                serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {"id": "vendor-turn-1", "status": "completed"}
                }),
            )
            .await;
        let (_turn_id, _second) = running(&shared, "vendor-turn-2").await;

        request_and_answer(&shared, "vendor-turn-2", 1).await;
        handler
            .on_notification(
                String::from("serverRequest/resolved"),
                serde_json::json!({"threadId": "thread-1", "requestId": 1}),
            )
            .await;

        let markers = shared.resolution_markers.lock().await;
        assert!(
            !shared.is_shutting_down(),
            "expected a completed normal answer not to spend the next turn's request budget"
        );
        assert!(
            markers.early.is_empty() && markers.answered.is_empty(),
            "expected normal resolutions to leave no cross-turn markers"
        );
    }

    /// The answer task and its confirmation run independently. Their shared marker transition
    /// must leave neither an early tombstone nor an acknowledgement behind.
    #[tokio::test]
    async fn an_answer_and_its_confirmation_share_one_marker_transition() {
        let shared = shared_with_pending_limit(1);
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;
        let gate = ResolutionMarkerGate::closed();
        *shared.resolution_marker_gate.lock().await = Some(Arc::clone(&gate));

        let request_shared = Arc::clone(&shared);
        let request = tokio::spawn(async move {
            CodexHandler {
                shared: request_shared,
            }
            .on_request(
                String::from("item/commandExecution/requestApproval"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-1", "itemId": "item-1",
                    "command": "pwd"
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(0)),
            )
            .await
        });
        answer_waiting_approval(&shared).await;
        gate.wait_until_entered().await;

        let resolution_shared = Arc::clone(&shared);
        let resolution = tokio::spawn(async move {
            CodexHandler {
                shared: resolution_shared,
            }
            .on_notification(
                String::from("serverRequest/resolved"),
                serde_json::json!({"threadId": "thread-1", "requestId": 0}),
            )
            .await;
        });
        tokio::task::yield_now().await;
        assert!(
            !resolution.is_finished(),
            "expected the confirmation to queue behind the reply-marker transition"
        );

        gate.open();
        let _ = request.await.expect("expected the approval request task");
        resolution
            .await
            .expect("expected the confirmation task to finish");

        let markers = shared.resolution_markers.lock().await;
        assert!(
            markers.early.is_empty() && markers.answered.is_empty(),
            "expected one atomic transition to consume both sides of the race"
        );
    }

    /// A missing confirmation from a future server build cannot turn the acknowledgement ledger
    /// into unbounded per-turn history.
    #[tokio::test]
    async fn unconfirmed_answers_keep_a_bounded_acknowledgement_ledger() {
        let shared = shared_with_pending_limit(1);
        shared.adopt_thread(String::from("thread-1"));
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;

        for request_id in [0, 1] {
            request_and_answer(&shared, "vendor-turn-1", request_id).await;
        }

        assert_eq!(
            shared.resolution_markers.lock().await.answered.len(),
            1,
            "expected the acknowledgement ledger to retain at most the configured request budget"
        );
        assert!(
            !shared.is_shutting_down(),
            "expected a missing confirmation not to poison the active turn"
        );
    }

    /// Terminal cleanup owns all request-id markers. Otherwise an old marker with a reused id
    /// keeps the mutex while the next handler tries to restore it, deadlocking that approval.
    #[tokio::test(start_paused = true)]
    async fn terminal_cleanup_removes_early_markers_before_a_request_id_is_reused() {
        let shared = shared();
        shared.adopt_thread(String::from("thread-1"));
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, _first) = running(&shared, "vendor-turn-1").await;
        let first_owner = shared
            .active_turn_route()
            .await
            .expect("expected a first route")
            .owner;
        shared
            .resolution_markers
            .lock()
            .await
            .early
            .insert(String::from("0"), Arc::clone(&first_owner));
        handler
            .on_notification(
                String::from("turn/completed"),
                serde_json::json!({
                    "threadId": "thread-1",
                    "turn": {"id": "vendor-turn-1", "status": "completed"}
                }),
            )
            .await;
        let (_turn_id, _second) = running(&shared, "vendor-turn-2").await;

        shared
            .remember_answered_resolution("late-first", &first_owner)
            .await;
        let markers = shared.resolution_markers.lock().await;
        assert!(
            markers.early.is_empty() && markers.answered.is_empty(),
            "expected a released owner not to record a marker after its replacement started"
        );
        drop(markers);

        let request_shared = Arc::clone(&shared);
        let request = tokio::spawn(async move {
            CodexHandler {
                shared: request_shared,
            }
            .on_request(
                String::from("item/commandExecution/requestApproval"),
                serde_json::json!({
                    "threadId": "thread-1", "turnId": "vendor-turn-2", "itemId": "item-2",
                    "command": "pwd"
                }),
                mango_external_agents::jsonrpc::RequestId::new(serde_json::json!(0)),
            )
            .await
        });
        answer_waiting_approval(&shared).await;

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), request)
                .await
                .is_ok(),
            "expected the old marker to be gone before the replacement checks it"
        );
        assert!(
            shared.resolution_markers.lock().await.early.is_empty(),
            "expected terminal cleanup to remove the completed turn's markers"
        );
    }

    /// Closing carries the host's reason before its terminal when both events fit.
    #[tokio::test]
    async fn closing_preserves_the_consent_revocation_reason() {
        let shared = shared();
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        shared
            .cancel_active(mango_external_agents::CancelReason::ConsentRevoked)
            .await;

        assert!(matches!(
            stream.recv().await.expect("cancellation marker").kind,
            EventKind::Cancelled {
                reason: mango_external_agents::CancelReason::ConsentRevoked
            }
        ));

        let event = stream.recv().await.expect("expected an ending");
        assert!(
            event.is_terminal(),
            "expected the marker to be followed by its terminal, received {:?}",
            event.kind
        );
        assert!(
            stream.recv().await.is_none(),
            "expected the stream to end after its terminal"
        );
    }

    /// EOF has no `turn/completed` to reduce, so the connection callback must end the stream.
    #[tokio::test]
    async fn a_terminated_connection_fails_the_active_stream_and_clears_its_slot() {
        let shared = shared();
        let handler = CodexHandler {
            shared: Arc::clone(&shared),
        };
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        handler.on_terminated(PeerTermination::Exited).await;

        let event = stream.recv().await.expect("expected a terminal error");
        let EventKind::Error { error } = event.kind else {
            panic!("expected a terminal error, received {:?}", event.kind);
        };
        assert_eq!(error.code, super::CALL_FAILED);
        assert!(
            shared.turn.lock().await.is_none(),
            "expected the exited connection to free the active slot"
        );
        assert!(
            shared.is_shutting_down(),
            "expected an exited connection to refuse more work"
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_the_active_stream_with_its_reason() {
        let shared = shared();
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        shared
            .cancel_active(mango_external_agents::CancelReason::Shutdown)
            .await;

        assert!(matches!(
            stream.recv().await.map(|event| event.kind),
            Some(EventKind::Cancelled {
                reason: mango_external_agents::CancelReason::Shutdown
            })
        ));
        assert!(matches!(
            stream.recv().await.map(|event| event.kind),
            Some(EventKind::Completed)
        ));
        assert!(shared.turn.lock().await.is_none());
    }

    fn image_attachment(mime_type: &str, bytes: Vec<u8>) -> Attachment {
        Attachment {
            id: String::from("image-1"),
            name: String::from("image"),
            mime_type: String::from(mime_type),
            kind: AttachmentKind::Image,
            bytes,
        }
    }

    #[test]
    fn codex_input_rejects_attachments_before_encoding_them() {
        let too_many = TurnRequest::new("turn-1", "look").with_attachments(
            (0..=TURN_MAX_ATTACHMENTS)
                .map(|index| image_attachment("image/png", vec![index as u8]))
                .collect(),
        );
        assert!(matches!(
            super::CodexSession::input_for(&too_many),
            Err(mango_external_agents::Error::LimitExceeded {
                subject: "attachments on one turn",
                ..
            })
        ));

        let oversized =
            TurnRequest::new("turn-1", "look").with_attachments(vec![image_attachment(
                "image/png",
                vec![0; ATTACHMENT_MAX_BYTES + 1],
            )]);
        assert!(matches!(
            super::CodexSession::input_for(&oversized),
            Err(mango_external_agents::Error::LimitExceeded {
                subject: "bytes in one attachment",
                ..
            })
        ));
    }

    #[test]
    fn codex_input_accepts_the_documented_raster_image_types() {
        for mime_type in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert!(
                supports_image_mime_type(mime_type),
                "expected {mime_type} to be accepted"
            );
        }
        assert!(!supports_image_mime_type("image/svg+xml"));
    }

    /// A steer refused because the turn already finished is a rejection a host can act on; any
    /// other `-32600` is a failure it has to be told about, and the two share a code.
    #[test]
    fn only_the_refusal_the_app_server_spells_out_reads_as_a_turn_that_already_finished() {
        let refusal = VendorError::new(super::CALL_FAILED, "no active turn to steer")
            .with_vendor_code("-32600", false);
        assert!(super::is_no_active_turn(&refusal));

        let other = VendorError::new(super::CALL_FAILED, "expectedTurnId is not a uuid")
            .with_vendor_code("-32600", false);
        assert!(
            !super::is_no_active_turn(&other),
            "expected another invalid request under the same code to stay a failure"
        );

        let unrecognised = VendorError::new(super::CALL_FAILED, "no active turn to steer")
            .with_vendor_code("-31999", false);
        assert!(
            !super::is_no_active_turn(&unrecognised),
            "expected a code this harness has not seen to fall through to the vendor error"
        );
    }

    #[test]
    fn only_an_explicit_non_transport_vendor_refusal_frees_a_started_turn_slot() {
        let refusal = mango_external_agents::Error::Vendor(
            VendorError::new(super::CALL_FAILED, "the request is not allowed")
                .with_vendor_code("-32602", false),
        );
        assert!(super::start_was_explicitly_refused(&refusal));

        for ambiguous in [
            mango_external_agents::Error::Timeout {
                operation: String::from("Codex app-server turn/start"),
                after: std::time::Duration::from_secs(1),
            },
            mango_external_agents::Error::Protocol {
                expected: String::from("a turn result"),
                received: String::from("malformed JSON"),
            },
            mango_external_agents::Error::Vendor(
                VendorError::new(super::CALL_FAILED, "the Codex app-server exited")
                    .with_vendor_code("-32000", true),
            ),
        ] {
            assert!(
                !super::start_was_explicitly_refused(&ambiguous),
                "expected {ambiguous:?} to retain the active slot"
            );
        }
    }

    /// Verified against a real `turn/start`: the app-server accepts an image as a `data:` URL and
    /// the turn completes. The encoding is the part that has to be exactly right.
    #[test]
    fn bytes_become_the_data_url_the_app_server_takes() {
        assert_eq!(data_url("image/png", b"hi"), "data:image/png;base64,aGk=");
    }

    #[test]
    fn base64_pads_every_length_the_way_the_standard_does() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    /// The bytes that break a hand-rolled encoder: the top bit set, and a length that pads.
    #[test]
    fn base64_encodes_bytes_above_the_ascii_range() {
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(base64(&[0xff]), "/w==");
    }

    /// A teardown worker that panicked becomes a cleanup-required error naming the panic, so a
    /// close waiter is told the child may still be live.
    #[tokio::test]
    async fn a_panicked_teardown_worker_is_reported_as_cleanup_required() {
        let joined = tokio::spawn(async { panic!("test teardown worker panicked") })
            .await
            .expect_err("expected the spawned worker to panic");
        let launcher = mango_external_agents::testing::FakeLauncher::new();
        launcher.push(mango_external_agents::testing::FakeProcess::transcript(
            Vec::<String>::new(),
        ));
        let control = mango_external_agents::ProcessLauncher::spawn(
            &launcher,
            mango_external_agents::LaunchSpec {
                argv: vec![String::from("codex")],
                cwd: std::env::temp_dir(),
                env: Default::default(),
                stdin: true,
                hide_window: true,
            },
        )
        .await
        .expect("expected the fake child to launch")
        .control;
        let error = super::teardown_panicked(control, &joined);
        assert!(
            error.cleanup_control().is_some(),
            "expected a cleanup-required error carrying the control | received: {error:?}"
        );
        assert!(
            error.to_string().contains("panicked"),
            "expected an error naming the panic | received: {error}"
        );
    }

    /// On a multi-thread runtime the pump can name a lost start while the stop worker is between
    /// its second, empty-id pass and the settle stage. The settle stage must still send that
    /// turn's one interrupt rather than wait out its deadline and kill the process.
    #[tokio::test(start_paused = true)]
    async fn a_turn_named_before_the_settle_stage_gets_its_one_interrupt() {
        let shared = shared();
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;
        let owner = {
            let mut turn = shared.turn.lock().await;
            let active = turn.as_mut().expect("expected the installed turn");
            active.start_unanswerable = true;
            Arc::clone(&active.owner)
        };
        let link = ScriptedLink::new();
        let client = Client::connect(
            link.clone().into_link(),
            super::CodexSession::handler(Arc::clone(&shared)),
            ClientOptions::new("Codex app-server"),
        );
        let settling = tokio::spawn(async move {
            let _ = super::CodexSession::settle_stop(
                &shared,
                &client,
                &owner,
                mango_external_agents::CancelReason::Requested,
                std::time::Duration::from_secs(60),
            )
            .await;
        });

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let interrupts = link
            .sent()
            .iter()
            .filter(|line| line.contains("\"turn/interrupt\""))
            .count();
        settling.abort();
        assert_eq!(
            interrupts, 1,
            "expected turn/interrupt dispatches: 1 | received: {interrupts}"
        );
    }
}
