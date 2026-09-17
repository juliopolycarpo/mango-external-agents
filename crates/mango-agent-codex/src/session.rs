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
use mango_external_agents::interaction::InteractionId;
use mango_external_agents::jsonrpc::{
    Client, JsonRpcError, PeerHandler, PeerTermination, RequestId, ServerRequestOutcome,
};
use mango_external_agents::operation::{AttemptId, Dispatch, OperationRef};
use mango_external_agents::permission::{
    ApprovalDecision, DecisionSource, PermissionResponse, broker_response,
};
use mango_external_agents::process::ProcessControl;
use mango_external_agents::session::{
    ATTACHMENT_MAX_BYTES, AccountUsage, CancelReason, CloseReason, NativeSession, ReviewRequest,
    Session, SessionPage, SessionQuery, Steer, SteerOutcome, SteerRejection, TURN_MAX_ATTACHMENTS,
    TurnRequest,
};
use mango_external_agents::state::{SessionState, SessionStatus};
use mango_external_agents::stream::{EventSink, ReviewStream, TurnStream};
use serde_json::Value;
use tokio::sync::{Mutex, oneshot};

use crate::approvals::{self, PendingApproval};
use crate::protocol::approvals::{ApprovalDecisionValue, ApprovalResponse, ServerRequest};
use crate::protocol::method;
use crate::protocol::notifications::Notification;
use crate::protocol::requests::{
    RateLimitsReadResponse, ReviewStartParams, ReviewStartResponse, ReviewTarget, ThreadListParams,
    ThreadListResponse, TurnHandle, TurnInterruptParams, TurnStartParams, TurnStartResponse,
    TurnSteerParams, TurnSteerResponse, UserInput, empty_params,
};
use crate::reducer::{self, Outcome};

/// A turn was asked for while one was already running.
///
/// The app-server takes `turn/start` on a live turn as a steer — its own documentation says
/// `turnTrigger` is "ignored when this request steers an already-active turn". A host that meant a
/// new turn would receive a stream that never gets a `turn/completed` of its own, so this is
/// refused before the call is made. Retryable: the running turn will end.
pub const TURN_ALREADY_RUNNING: ErrorCode = ErrorCode::from_static("codex-turn-already-running");

/// The session or the connection would not take a call.
pub const CALL_FAILED: ErrorCode = ErrorCode::from_static("codex-call-failed");

/// How long a closing session offers a turn its terminal before dropping the stream.
///
/// A bound on a slow reader, not on a person: a host that is merely behind sees its turn end
/// normally, and a host that has stopped reading sees the stream close instead.
const TERMINAL_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// The client bounds peer questions at 256, so an equal cap retains every plausible early
/// resolution while refusing an unbounded stream of unrelated resolution notices.
const MAX_EARLY_RESOLUTIONS: usize = 256;

/// Native ids retained after terminal frames so a delayed old frame cannot attach while the next
/// start still waits for its response.
const RECENT_COMPLETED_TURNS: usize = 16;

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
    turn: Mutex<Option<ActiveTurn>>,
    recent_completed_turns: Mutex<VecDeque<String>>,
    pending: Mutex<HashMap<InteractionId, PendingEntry>>,
    /// Whether this connection can accept more work.
    ///
    /// A start request that times out may already be running at the vendor, so its active slot
    /// stays occupied. A terminated connection is stronger: nothing can safely use this session
    /// again, even after that slot has drained.
    shutting_down: AtomicBool,
    /// Wakes the session-owned process reaper after an unexpected link termination.
    terminated: mango_external_agents::CancelToken,
    /// Questions the server stopped waiting on before this side had registered them.
    ///
    /// A `serverRequest/resolved` is read off the same pipe as the request it resolves, and the
    /// task composing the answer runs beside the pump rather than inside it — so the release can
    /// win the race against the registration. Without this the waiter it was meant to free would
    /// sit out the whole approval deadline before answering a question nobody is asking.
    ///
    /// Emptied with the questions themselves, because that is their lifetime: a turn that ends
    /// settles everything it was waiting on.
    resolved_early: Mutex<HashMap<String, Arc<()>>>,
}

impl Shared {
    /// The state one connection's handler and session share, before a thread exists.
    pub(crate) fn new(host: HostContext, session_id: mango_external_agents::SessionId) -> Self {
        Self {
            host,
            session_id,
            thread_id: std::sync::OnceLock::new(),
            control: std::sync::OnceLock::new(),
            turn: Mutex::new(None),
            recent_completed_turns: Mutex::new(VecDeque::new()),
            pending: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
            terminated: mango_external_agents::CancelToken::new(),
            resolved_early: Mutex::new(HashMap::new()),
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

    /// Snapshots the active turn's owner and native id so notification routing remains bound to
    /// the start attempt even while its response is setting the native id.
    async fn active_turn_route(&self) -> Option<ActiveTurnRoute> {
        self.turn
            .lock()
            .await
            .as_ref()
            .map(|active| ActiveTurnRoute {
                owner: Arc::clone(&active.owner),
                turn_id: active.turn_id.clone(),
                attempt: active.attempt,
                native_turn_id: active.native_turn_id.clone(),
            })
    }

    /// Whether the start attempt identified by `route` still owns the active turn slot.
    async fn owns_active_turn(&self, route: &ActiveTurnRoute) -> bool {
        self.turn
            .lock()
            .await
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&active.owner, &route.owner))
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
    /// Native reviews have their own lifecycle and do not accept user steering.
    is_review: bool,
    /// A cancel arrived before the server supplied the id its interrupt needs.
    cancel_before_start: bool,
    /// Why this turn is being stopped, when somebody here asked for it.
    ///
    /// The server reports only that a turn was interrupted. Who asked, and why, is this side's
    /// knowledge — and "you stopped this turn" is a lie for a shutdown.
    cancel_reason: Option<CancelReason>,
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

/// How a waiting question was settled.
enum Answer {
    /// Somebody chose.
    Chosen {
        decision: ApprovalDecisionValue,
        option_id: String,
        source: DecisionSource,
        /// Whether the caller that settled this question already recorded its audit event.
        reported: bool,
    },
    /// The server stopped waiting on its own, so nothing needs sending.
    ResolvedByTheServer,
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
    /// The sink is taken out from under the lock before it is used, and that is the whole point of
    /// the extra statement. The turn channel is bounded: a host that stops reading parks this
    /// `emit`, and parking while holding the turn lock would park every other caller of it —
    /// `cancel`, `close`, the next `start_turn` — on a host that is never coming back. A close
    /// that cannot return is worse than a turn that cannot be fed.
    ///
    /// A turn whose host has dropped the stream stays installed, and the event is dropped
    /// instead. A closed sink refuses immediately and without parking, so there is nothing to save
    /// by forgetting the turn — and forgetting it would open the trap the running-turn guard
    /// exists to close: the vendor's turn runs on whether or not anybody is listening, and a free
    /// slot would let the next `turn/start` out to be read as a steer of it. `turn/completed`
    /// ends this turn, as it ends every other.
    ///
    /// A refusal is also not always a host that left. The core refuses an event it cannot bound —
    /// a vendor id past the length limit — and losing the turn over one unrenderable event would
    /// close a host's stream with no terminal at all.
    async fn emit(&self, kind: EventKind) {
        let sink = {
            let turn = self.turn.lock().await;
            turn.as_ref().map(|active| active.sink.clone())
        };
        let Some(sink) = sink else {
            return;
        };
        let _ = sink.emit(kind).await;
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
            .filter(|active| Arc::ptr_eq(&active.owner, owner));
        let Some(active) = active else {
            return Ok(None);
        };
        if active.announced {
            return Ok(None);
        }
        active.announced = true;
        active.native_turn_id = native_turn_id.clone();
        Ok(Some((active.sink.clone(), native_turn_id)))
    }

    /// Puts one event on the stream that still belongs to this start attempt.
    async fn emit_for(&self, route: &ActiveTurnRoute, kind: EventKind) {
        let sink = {
            let turn = self.turn.lock().await;
            turn.as_ref()
                .filter(|active| Arc::ptr_eq(&active.owner, &route.owner))
                .map(|active| active.sink.clone())
        };
        let Some(sink) = sink else {
            return;
        };
        let _ = sink.emit(kind).await;
    }

    /// Ends the running turn, if there is one.
    async fn finish(&self, outcome: Outcome) {
        let Outcome::Finish {
            events,
            cancelled,
            failure,
        } = outcome
        else {
            return;
        };
        let Some(active) = self.turn.lock().await.take() else {
            return;
        };
        for event in events {
            if active.sink.emit(event).await.is_err() {
                return;
            }
        }
        let _ = match (failure, cancelled) {
            (Some(failure), _) => active.sink.fail(failure).await,
            // The reason this side recorded wins over the server's bare "interrupted": a shutdown
            // is not a person pressing stop, and the host is entitled to know which it was.
            (None, Some(reason)) => {
                active
                    .sink
                    .cancel(active.cancel_reason.unwrap_or(reason))
                    .await
            }
            (None, None) => active.sink.complete().await,
        };
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
        let active = {
            let mut turn = self.turn.lock().await;
            if turn
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(&active.owner, &route.owner))
            {
                turn.take()
            } else {
                None
            }
        };
        let Some(active) = active else {
            return;
        };
        self.remember_completed(
            completed_native_turn_id
                .filter(|native_turn_id| !native_turn_id.is_empty())
                .unwrap_or(&route.native_turn_id),
        )
        .await;
        for event in events {
            if active.sink.emit(event).await.is_err() {
                return;
            }
        }
        let _ = match (failure, cancelled) {
            (Some(failure), _) => active.sink.fail(failure).await,
            (None, Some(reason)) => {
                active
                    .sink
                    .cancel(active.cancel_reason.unwrap_or(reason))
                    .await
            }
            (None, None) => active.sink.complete().await,
        };
    }

    /// Ends a turn because the host is shutting the session down.
    async fn cancel_active(&self, reason: CancelReason) {
        let Some(active) = self.turn.lock().await.take() else {
            return;
        };
        let _ = tokio::time::timeout(TERMINAL_GRACE, active.sink.cancel_on_close(reason)).await;
    }

    /// Fails the active stream after the peer disappears without a terminal notification.
    async fn connection_terminated(&self, termination: PeerTermination) {
        if !self.stop_new_work() {
            return;
        }
        self.release_pending(DecisionSource::Cancelled).await;
        let Some(active) = self.turn.lock().await.take() else {
            self.terminated.cancel();
            return;
        };
        let mut message =
            format!("Codex app-server connection ended while the turn was active: {termination}");
        if let Some(control) = self.control.get() {
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
        let _ = tokio::time::timeout(TERMINAL_GRACE, active.sink.fail(failure)).await;
    }

    /// Fails the active turn under a bound and wakes the reaper after an unrecoverable protocol
    /// error. The connection cannot safely carry later work once routing has been lost.
    async fn poison(&self, failure: VendorError) {
        if !self.stop_new_work() {
            return;
        }
        self.release_pending(DecisionSource::Cancelled).await;
        if let Some(active) = self.turn.lock().await.take() {
            let _ = tokio::time::timeout(TERMINAL_GRACE, active.sink.fail(failure)).await;
        }
        self.terminated.cancel();
    }

    /// Settles every question the server is still waiting on.
    ///
    /// Called when a turn is cancelled or a session closes. The waiting handler tasks answer with
    /// a refusal, which is what lets the server's own turn end instead of blocking forever.
    async fn release_pending(&self, source: DecisionSource) {
        let waiting: Vec<PendingEntry> =
            self.pending.lock().await.drain().map(|(_, e)| e).collect();
        let deadline = tokio::time::Instant::now() + TERMINAL_GRACE;
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
            let Some(remaining) = deadline.checked_duration_since(tokio::time::Instant::now())
            else {
                return;
            };
            // A full host stream must not stop close, cancellation, or EOF recovery from
            // releasing every vendor request above. All audits share this one grace period.
            let _ = tokio::time::timeout(remaining, self.emit_for(&route, event)).await;
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

#[async_trait::async_trait]
impl PeerHandler for CodexHandler {
    async fn on_notification(&self, method: String, params: Value) {
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

        let outcome = reducer::reduce_for_active_turn(
            &notification,
            self.shared.thread_id(),
            Some(active_route.native_turn_id.as_str()),
            self.shared.host.now(),
        );
        match outcome {
            Outcome::Emit(events) => {
                for event in events {
                    if notification.requires_native_turn_match() {
                        self.shared.emit_for(&active_route, event).await;
                    } else {
                        self.shared.emit(event).await;
                    }
                }
            }
            Outcome::Finish { .. } => {
                // Whatever the server was still asking is moot: its turn is over, and a question
                // belonging to a finished turn is one nobody will be shown.
                self.shared.release_pending(DecisionSource::Cancelled).await;
                self.shared
                    .finish_for(&active_route, notification.turn_id(), outcome)
                    .await;
            }
            Outcome::Poison { failure } => {
                if !self.shared.stop_new_work() {
                    return;
                }
                self.shared.release_pending(DecisionSource::Cancelled).await;
                self.shared
                    .finish(Outcome::Finish {
                        events: Vec::new(),
                        cancelled: None,
                        failure: Some(failure),
                    })
                    .await;
            }
            Outcome::Ignore => {}
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

        let Some(route) = self.shared.active_turn_route().await else {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32602,
                message: String::from("expected an active turn for an approval request"),
                data: None,
            });
        };
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

        // One read, reused in `decide`: a second `host.now()` call to build the deadline would let
        // a host clock that moved backward between the two reads extend the monotonic approval
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

        match self.decide(pending, &id, route, now).await {
            Some(decision) => ServerRequestOutcome::Answer(
                serde_json::to_value(ApprovalResponse { decision })
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            ),
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
    /// Puts one question to whoever answers, and waits.
    ///
    /// Returns `None` when the server resolved the question itself while this was waiting.
    async fn decide(
        &self,
        pending: PendingApproval,
        id: &RequestId,
        route: ActiveTurnRoute,
        now: SystemTime,
    ) -> Option<ApprovalDecisionValue> {
        let request = pending.request.clone();
        let request_id = request.id().clone();
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

        // And the server may already have stopped waiting, in a race this side cannot see from
        // the outside: the release is read off the same pipe and runs beside this task.
        if let Some(early_route) = self.shared.resolved_early.lock().await.remove(&id.key()) {
            if Arc::ptr_eq(&early_route, &route.owner) {
                let mut pending_entries = self.shared.pending.lock().await;
                if pending_entries
                    .get(&request_id)
                    .is_some_and(|entry| Arc::ptr_eq(&entry.route.owner, &route.owner))
                {
                    pending_entries.remove(&request_id);
                }
                return None;
            }
            self.shared
                .resolved_early
                .lock()
                .await
                .insert(id.key(), early_route);
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

        // The stream deliberately backpressures the app-server. The deadline was created before
        // this await, so a full stream may delay its reply but cannot restart the decision window.
        self.shared
            .emit_for(
                &route,
                EventKind::ApprovalRequested {
                    request: bounded.clone(),
                },
            )
            .await;

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
                    let applied_option_id = decision.option_id();
                    let audited = match pending.option(applied_option_id) {
                        Some(option) => ApprovalDecision::from_option(option, response.source),
                        None => ApprovalDecision::unresolved(applied_option_id, response.source),
                    };
                    self.resolved(&route, &request_id, audited).await;
                    return Some(decision);
                }
            }
        }

        // Three ways this ends: somebody chooses, the deadline the request already carries passes,
        // or the host is going away. All three answer the server; none of them grants anything.
        let settled = tokio::select! {
            biased;
            () = self.shared.host.cancel().cancelled() => None,
            answer = waiting => answer.ok(),
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
                self.resolved(
                    &route,
                    &request_id,
                    ApprovalDecision::unresolved(decision.option_id(), source),
                )
                .await;
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
    ) -> Option<ApprovalDecisionValue> {
        match answer {
            Answer::Chosen {
                decision,
                option_id,
                source,
                reported,
            } => {
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
    ) -> ApprovalDecisionValue {
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

    async fn resolved(
        &self,
        route: &ActiveTurnRoute,
        request_id: &InteractionId,
        decision: ApprovalDecision,
    ) {
        self.shared
            .emit_for(
                route,
                EventKind::ApprovalResolved {
                    interaction_id: request_id.clone(),
                    decision,
                },
            )
            .await;
    }

    /// Releases the question the server says it is no longer waiting on.
    async fn release_resolved(&self, request_key: &str, route: &ActiveTurnRoute) -> bool {
        let mut pending = self.shared.pending.lock().await;
        let Some(id) = pending
            .iter()
            .find(|(_, entry)| {
                entry.request_key == request_key && Arc::ptr_eq(&entry.route.owner, &route.owner)
            })
            .map(|(id, _)| id.clone())
        else {
            // Nothing registered under it yet. Either this is somebody else's question, or the
            // task that will register it has not reached the map — and it checks here for exactly
            // this, rather than waiting out a deadline on a question already withdrawn.
            drop(pending);
            let mut early = self.shared.resolved_early.lock().await;
            if let Some(existing_route) = early.get(request_key) {
                return Arc::ptr_eq(existing_route, &route.owner);
            }
            if early.len() == MAX_EARLY_RESOLUTIONS {
                return false;
            }
            early.insert(request_key.to_owned(), Arc::clone(&route.owner));
            return true;
        };
        if pending
            .get(&id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.route.owner, &route.owner))
            && let Some(entry) = pending.remove(&id)
        {
            let _ = entry.answer.send(Answer::ResolvedByTheServer);
        }
        true
    }
}

impl CodexSession {
    /// A session over a connection that is already pumping and already has a thread.
    pub(crate) fn new(
        state: SessionState,
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
    ) -> Self {
        let _ = shared.control.set(Arc::clone(&control));
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
                    // Published before the wind-down, so a subscriber sees the transition rather
                    // than only its result. Monotonic, so a `close` racing this cannot walk it back.
                    watcher_state.set_status(SessionStatus::Closing);
                    if watcher_shared.stop_new_work() {
                        watcher_shared
                            .release_pending(DecisionSource::Cancelled)
                            .await;
                        watcher_shared.cancel_active(CancelReason::Shutdown).await;
                    }
                    let _ = watcher_client.close().await;
                }
                () = terminated.cancelled() => {
                    // The connection is already gone; `connection_terminated` or `poison` failed
                    // the active turn before cancelling this token.
                    watcher_state.set_status(SessionStatus::Closing);
                }
            }
            let _ = watcher_control.kill(CancelReason::Shutdown).await;
            // The child is released, so nothing more can happen on this session whichever branch
            // ran. Reported even when the kill failed: no later operation can make progress.
            watcher_state.set_status(SessionStatus::Closed);
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
        if self.closed.load(Ordering::Acquire) || self.shared.is_shutting_down() {
            return Err(Error::Closed { subject: "session" }.with_dispatch(Dispatch::NotSubmitted));
        }

        let (sink, events) = EventSink::new(
            self.shared.session_id.clone(),
            turn_id.clone(),
            attempt,
            Arc::clone(self.shared.host.clock()),
            self.shared.host.limits().turn_channel_capacity,
        );
        let owner = Arc::new(());
        let generation;

        {
            let mut turn = self.shared.turn.lock().await;
            if turn.is_some() {
                // Refused under the lock, so two callers racing cannot both find it free. The
                // app-server would take the second as a steer of the first.
                return Err(Error::Vendor(
                    VendorError::new(
                        TURN_ALREADY_RUNNING,
                        "expected no turn to be running, received a request to start one while \
                         another is live; the app-server would take it as a steer",
                    )
                    .with_vendor_code("turn-active", true),
                )
                .with_dispatch(Dispatch::NotSubmitted));
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
                is_review: rpc_method == method::REVIEW_START,
                cancel_before_start: false,
                cancel_reason: None,
            });
            let mut state = self.configuration.lock().await;
            state.next_generation += 1;
            generation = state.next_generation;
        }

        let answer: Result<R> = self.client.request(rpc_method, params).await;
        let (handle, extra) = match answer {
            Ok(answer) => turn_of(answer),
            Err(error) => {
                if !start_was_explicitly_refused(&error) {
                    // A local transport, decoding, or timeout failure says nothing about whether
                    // the app-server accepted the request. Keeping the slot occupied prevents
                    // the next start from becoming a steer of a vendor turn whose handle never
                    // reached this client.
                    return Err(error.with_dispatch(Dispatch::AcceptanceUnknown));
                }
                // The turn never started, so the stream it would have written to is closed here
                // rather than left for a `turn/completed` that will never come. Nothing to
                // interrupt either. The slot may already belong to a later start if the server
                // announced this turn's completion before returning this error.
                let mut turn = self.shared.turn.lock().await;
                if turn
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(&active.owner, &owner))
                {
                    turn.take();
                }
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

        let cancel_before_start = {
            let mut turn = self.shared.turn.lock().await;
            match turn.as_mut() {
                Some(active) if Arc::ptr_eq(&active.owner, &owner) => {
                    active.native_turn_id.clone_from(&native_turn_id);
                    active.cancel_before_start
                }
                _ => false,
            }
        };

        // A cancel that beat this answer could not name the turn. It kept the slot occupied so no
        // later `turn/start` could become a steer; now this call has the id needed to stop it.
        if cancel_before_start {
            self.interrupt(handle.id.clone()).await;
        }

        Ok((
            TurnStream::accepted(turn_id, attempt, native_turn_id, events),
            extra,
        ))
    }

    /// Stops one vendor turn, best effort.
    ///
    /// Used where nothing can be done about a failure: the turn is already out of this session's
    /// hands, and the alternative to a call that might not land is no call at all.
    async fn interrupt(&self, turn_id: String) {
        let _: Result<Value> = self
            .client
            .request(
                method::TURN_INTERRUPT,
                TurnInterruptParams {
                    thread_id: self.shared.thread_id().to_owned(),
                    turn_id,
                },
            )
            .await;
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

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        let Some((route, thread_id, turn_id, pending_start)) = ({
            let mut turn = self.shared.turn.lock().await;
            turn.as_mut().map(|active| {
                active.cancel_reason = Some(reason);
                let pending_start = active.native_turn_id.is_empty();
                if pending_start {
                    active.cancel_before_start = true;
                }
                (
                    ActiveTurnRoute {
                        owner: Arc::clone(&active.owner),
                        turn_id: active.turn_id.clone(),
                        attempt: active.attempt,
                        native_turn_id: active.native_turn_id.clone(),
                    },
                    self.shared.thread_id().to_owned(),
                    active.native_turn_id.clone(),
                    pending_start,
                )
            })
        }) else {
            // No turn to stop. Not an error: a host that cancels twice, or cancels a turn that
            // just ended, has asked for a state that already holds.
            return Ok(());
        };

        // Every question the server is waiting on is refused first. Interrupting a turn whose
        // approval is still blocking would leave the server waiting on a prompt for a turn that is
        // being stopped.
        self.shared.release_pending(DecisionSource::Cancelled).await;

        // Completion and the next start may race with approval cleanup. A cancel belongs only to
        // the start attempt it observed, so it becomes a no-op once that owner no longer holds
        // the slot rather than interrupting a replacement turn or surfacing its no-active error.
        if !self.shared.owns_active_turn(&route).await {
            return Ok(());
        }

        if pending_start {
            // The turn's own handle had not come back when the slot was read, so there was nothing
            // to name in an interrupt. Keep the slot occupied until `begin` receives the handle
            // and interrupts it, or the start fails. The stream has not reached the caller yet;
            // the vendor's completion supplies its one terminal after the interrupt.
            return Ok(());
        }

        let interrupt: Result<Value> = self
            .client
            .request(
                method::TURN_INTERRUPT,
                TurnInterruptParams { thread_id, turn_id },
            )
            .await;
        match interrupt {
            Ok(_) => Ok(()),
            Err(error) if self.shared.owns_active_turn(&route).await => Err(error),
            // The owner ended while the interrupt was in flight. Its requested end has already
            // happened, and an error from the former turn must not be reported for a replacement.
            Err(_) => Ok(()),
        }
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.state.set_status(SessionStatus::Closing);
        self.shutdown_watcher.abort();
        self.shared.stop_new_work();
        self.shared.release_pending(DecisionSource::Cancelled).await;
        // The turn ends here rather than on a notification: the connection is about to go, and a
        // host holding a stream that will never terminate is worse than one told why it stopped.
        // Nothing is interrupted here: the child is about to be killed, which stops every turn on
        // it more thoroughly than a call could.
        self.shared.cancel_active(CancelReason::from(reason)).await;

        let closed = self.client.close().await;
        let killed = self.control.kill(CancelReason::from(reason)).await;
        self.state.set_status(SessionStatus::Closed);
        closed.and(killed)
    }

    async fn steer(&self, steer: Steer) -> Result<SteerOutcome> {
        let running = {
            let turn = self.shared.turn.lock().await;
            turn.as_ref().map(|active| {
                (
                    active.turn_id.clone(),
                    active.native_turn_id.clone(),
                    active.is_review,
                )
            })
        };
        let Some((turn_id, native_turn_id, is_review)) = running else {
            return Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted,
            });
        };
        if is_review {
            return Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnNotSteerable,
            });
        }
        if turn_id != steer.turn_id || native_turn_id != steer.native_turn_id {
            // The turn the host meant is not the one running. Steering the live one instead would
            // put the input on a turn nobody addressed.
            return Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted,
            });
        }

        let params = TurnSteerParams {
            thread_id: self.shared.thread_id().to_owned(),
            input: vec![UserInput::text(steer.input)],
            expected_turn_id: steer.native_turn_id,
        };
        match self
            .client
            .request::<_, TurnSteerResponse>(method::TURN_STEER, params)
            .await
        {
            Ok(_) => Ok(SteerOutcome::Accepted),
            Err(Error::Vendor(error)) if is_no_active_turn(&error) => Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted,
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
        let params = ThreadListParams {
            cursor: query.cursor,
            limit: query.limit,
            cwd: query
                .workspace_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
        };
        let page: ThreadListResponse = self.client.request(method::THREAD_LIST, params).await?;
        Ok(SessionPage {
            sessions: page
                .data
                .into_iter()
                .map(|thread| NativeSession {
                    native_session_id: thread.id,
                    // The vendor has no title of its own for most threads; `preview` is the first
                    // user message, which is what its own picker shows.
                    title: thread.name.filter(|name| !name.is_empty()),
                    preview: Some(thread.preview).filter(|preview| !preview.is_empty()),
                    workspace_path: thread.cwd,
                    updated_at: thread.updated_at.and_then(|seconds| {
                        u64::try_from(seconds).ok().and_then(|seconds| {
                            std::time::SystemTime::UNIX_EPOCH
                                .checked_add(std::time::Duration::from_secs(seconds))
                        })
                    }),
                })
                .collect(),
            next_cursor: page.next_cursor,
            truncated: false,
        })
    }

    async fn refresh_account_usage(&self) -> Result<AccountUsage> {
        let response: RateLimitsReadResponse = self
            .client
            .request(method::ACCOUNT_RATE_LIMITS_READ, empty_params())
            .await?;
        Ok(AccountUsage {
            // Absence is unknown, never an empty quota: a server that answered without a snapshot
            // has not told us the account is unmetered.
            limits: response.rate_limits.as_ref().map(|snapshot| {
                crate::rate_limits::to_account_limits(snapshot, self.shared.host.now())
            }),
        })
    }
}

impl Drop for CodexSession {
    fn drop(&mut self) {
        self.shutdown_watcher.abort();
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
    use std::sync::Arc;

    use mango_external_agents::Attachment;
    use mango_external_agents::error::VendorError;

    use mango_external_agents::HostContext;
    use mango_external_agents::event::{EventKind, TurnId};
    use mango_external_agents::jsonrpc::PeerTermination;
    use mango_external_agents::session::{
        ATTACHMENT_MAX_BYTES, AttachmentKind, TURN_MAX_ATTACHMENTS, TurnRequest,
    };
    use mango_external_agents::stream::EventSink;
    use mango_external_agents::testing::FakeLauncher;

    use super::{
        ActiveTurn, CodexHandler, MAX_EARLY_RESOLUTIONS, Shared, base64, data_url,
        supports_image_mime_type,
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

    /// Installs a turn the way `begin` does, and hands back the stream a host would hold.
    async fn running(
        shared: &Shared,
        native_turn_id: &str,
    ) -> (TurnId, mango_external_agents::stream::TurnStream) {
        let turn_id = TurnId::new("turn-1");
        let attempt = mango_external_agents::operation::AttemptId::default();
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
            is_review: false,
            cancel_before_start: false,
            cancel_reason: None,
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

    /// The trap the running-turn guard exists to close. A host that drops its stream stops
    /// reading; it does not stop the vendor, whose turn runs on. Giving up the slot here would let
    /// the next `turn/start` out, and the app-server reads that as a steer of the live turn.
    #[tokio::test]
    async fn an_event_no_host_will_read_does_not_give_up_the_running_turn() {
        let shared = shared();
        let (_turn_id, stream) = running(&shared, "vendor-turn-1").await;
        drop(stream);

        shared
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
            shared.resolved_early.lock().await.is_empty(),
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
            let mut early = shared.resolved_early.lock().await;
            for index in 0..MAX_EARLY_RESOLUTIONS {
                early.insert(index.to_string(), Arc::new(()));
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
}
