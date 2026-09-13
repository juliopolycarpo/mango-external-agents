//! One live `codex app-server` connection, driven as a [`Session`].
//!
//! The app-server is one long-lived process per session, and it talks back: an approval stops the
//! server until this client answers. That is why the handler below waits rather than returning,
//! and why every path that ends a turn — a refusal, a cancel, a close, a deadline — has to resolve
//! whatever the server is still waiting on. A question nobody answers is a vendor process blocked
//! for the rest of its life.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use mango_external_agents::HostContext;
use mango_external_agents::error::{Error, ErrorCode, Result, VendorError};
use mango_external_agents::event::{EventKind, TurnId};
use mango_external_agents::jsonrpc::{
    Client, JsonRpcError, PeerHandler, RequestId, ServerRequestOutcome,
};
use mango_external_agents::permission::{
    ApprovalDecision, DecisionSource, PermissionResponse, broker_response,
};
use mango_external_agents::process::ProcessControl;
use mango_external_agents::session::{
    AccountUsage, CancelReason, CloseReason, NativeSession, ReviewRequest, Session, SessionInfo,
    SessionPage, SessionQuery, Steer, SteerOutcome, SteerRejection, TurnRequest,
};
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

/// One live conversation with a `codex app-server`.
pub struct CodexSession {
    info: SessionInfo,
    shared: Arc<Shared>,
    client: Arc<Client>,
    control: Arc<dyn ProcessControl>,
    closed: AtomicBool,
    /// Whether the vendor session has already been announced on a turn.
    ///
    /// A vendor session is opened once, and the core has no stream outside a turn to say so on —
    /// so the first turn carries the announcement and the rest do not. Repeating it would tell a
    /// host the conversation had just started, or just been resumed, on a turn where neither
    /// happened.
    ///
    /// Set only once a turn has actually started. A turn the server refuses never reaches its
    /// host, and the announcement it was carrying goes with it.
    announced_session: AtomicBool,
}

impl std::fmt::Debug for CodexSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodexSession")
            .field("ids", &self.info.ids)
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
    turn: Mutex<Option<ActiveTurn>>,
    pending: Mutex<HashMap<String, PendingEntry>>,
    /// Turns given up before the server had even named them.
    ///
    /// A slot emptied by `turn/completed` holds a turn that is over. A slot emptied by a cancel
    /// that arrived before the `turn/start` answer did holds one the vendor is still running, with
    /// nobody on this side holding an id to stop it by — the id is still in flight. Recorded here
    /// so the call that is parked on that answer can stop the turn when it finally lands.
    ///
    /// Self-clearing: only a turn whose handle has not arrived is recorded, and the call waiting
    /// for that handle takes its own entry back whether or not anybody abandoned it.
    abandoned: Mutex<HashSet<TurnId>>,
    /// Questions the server stopped waiting on before this side had registered them.
    ///
    /// A `serverRequest/resolved` is read off the same pipe as the request it resolves, and the
    /// task composing the answer runs beside the pump rather than inside it — so the release can
    /// win the race against the registration. Without this the waiter it was meant to free would
    /// sit out the whole approval deadline before answering a question nobody is asking.
    ///
    /// Emptied with the questions themselves, because that is their lifetime: a turn that ends
    /// settles everything it was waiting on.
    resolved_early: Mutex<HashSet<String>>,
}

impl Shared {
    /// The state one connection's handler and session share, before a thread exists.
    pub(crate) fn new(host: HostContext, session_id: mango_external_agents::SessionId) -> Self {
        Self {
            host,
            session_id,
            thread_id: std::sync::OnceLock::new(),
            turn: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            abandoned: Mutex::new(HashSet::new()),
            resolved_early: Mutex::new(HashSet::new()),
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
}

/// The turn currently running, and the sink its events go to.
struct ActiveTurn {
    sink: EventSink,
    turn_id: TurnId,
    native_turn_id: String,
    /// Why this turn is being stopped, when somebody here asked for it.
    ///
    /// The server reports only that a turn was interrupted. Who asked, and why, is this side's
    /// knowledge — and "you stopped this turn" is a lie for a shutdown.
    cancel_reason: Option<CancelReason>,
}

/// One question the server is waiting on.
struct PendingEntry {
    /// The JSON-RPC id the server will match the answer to, in the shape it arrived.
    request_key: String,
    pending: PendingApproval,
    answer: oneshot::Sender<Answer>,
}

/// How a waiting question was settled.
enum Answer {
    /// Somebody chose.
    Chosen {
        decision: ApprovalDecisionValue,
        option_id: String,
        source: DecisionSource,
    },
    /// The server stopped waiting on its own, so nothing needs sending.
    ResolvedByTheServer,
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

    /// Ends the running turn without waiting on a host that is not reading.
    ///
    /// The turn channel is bounded, and that bound is backpressure on the *vendor* — which is the
    /// right answer while a turn is running and the wrong one while a session is closing. A close
    /// that parked on a full channel would hold a `codex app-server` open for the life of the
    /// process, waiting for a reader that has already walked away.
    ///
    /// So the terminal is offered under a grace, and the sink is dropped either way. A host that
    /// is merely behind gets its `Completed`; a host that has stopped reading gets the end of its
    /// stream, which is the same ending by a different route.
    ///
    /// One event, deliberately, where a turn that ends normally sends two. `EventSink::cancel`
    /// writes a cancellation marker and then a terminal, and a grace that elapsed between them
    /// would leave a host a marker with nothing after it — the one shape the core's contract says
    /// cannot happen. A single `Completed` cannot half-land: either it arrives or the stream
    /// closes, and both are endings a host can read. Nothing is lost by dropping the reason,
    /// because the reason is the argument the host passed to `cancel` or `close` itself.
    async fn abandon(&self) {
        let Some(active) = self.turn.lock().await.take() else {
            return;
        };
        // Its handle has not arrived, so this side has no id to interrupt the vendor's turn by.
        // Whoever is parked on that answer stops it instead.
        if active.native_turn_id.is_empty() {
            self.abandoned.lock().await.insert(active.turn_id.clone());
        }
        let _ = tokio::time::timeout(TERMINAL_GRACE, active.sink.complete()).await;
    }

    /// Settles every question the server is still waiting on.
    ///
    /// Called when a turn is cancelled or a session closes. The waiting handler tasks answer with
    /// a refusal, which is what lets the server's own turn end instead of blocking forever.
    async fn release_pending(&self, source: DecisionSource) {
        self.resolved_early.lock().await.clear();
        let waiting: Vec<PendingEntry> =
            self.pending.lock().await.drain().map(|(_, e)| e).collect();
        for entry in waiting {
            let decision = entry.pending.refusal();
            let option_id = decision.option_id().to_owned();
            let _ = entry.answer.send(Answer::Chosen {
                decision,
                option_id,
                source,
            });
        }
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
        if let Notification::ServerRequestResolved(resolved) = &notification {
            self.release_resolved(&RequestId::new(resolved.request_id.clone()).key())
                .await;
        }

        let outcome = reducer::reduce(
            &notification,
            self.shared.thread_id(),
            self.shared.host.now(),
        );
        match outcome {
            Outcome::Emit(events) => {
                for event in events {
                    self.shared.emit(event).await;
                }
            }
            Outcome::Finish { .. } => {
                // Whatever the server was still asking is moot: its turn is over, and a question
                // belonging to a finished turn is one nobody will be shown.
                self.shared.release_pending(DecisionSource::Cancelled).await;
                self.shared.finish(outcome).await;
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

        let Some(pending) = approvals::to_request(&request, self.shared.host.now()) else {
            return ServerRequestOutcome::Failure(JsonRpcError {
                code: -32601,
                message: String::from("expected an approval this client can put to a person"),
                data: None,
            });
        };

        match self.decide(pending, &id).await {
            Some(decision) => ServerRequestOutcome::Answer(
                serde_json::to_value(ApprovalResponse { decision })
                    .unwrap_or_else(|_| Value::Object(serde_json::Map::new())),
            ),
            // The server already stopped waiting, so the frame is discarded on its side. Something
            // has to be returned, and a refusal is the answer that grants nothing.
            None => ServerRequestOutcome::Answer(Value::Object(serde_json::Map::new())),
        }
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
    ) -> Option<ApprovalDecisionValue> {
        let request = pending.request.clone();

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
        let (answer, waiting) = oneshot::channel();
        self.shared.pending.lock().await.insert(
            request.id.clone(),
            PendingEntry {
                request_key: id.key(),
                pending: pending.clone(),
                answer,
            },
        );

        // And the server may already have stopped waiting, in a race this side cannot see from
        // the outside: the release is read off the same pipe and runs beside this task.
        if self.shared.resolved_early.lock().await.remove(&id.key()) {
            self.shared.pending.lock().await.remove(&request.id);
            return None;
        }

        self.shared
            .emit(EventKind::ApprovalRequested {
                request: bounded.clone(),
            })
            .await;

        if let Some(response) = broker_response(self.shared.host.broker(), &bounded).await {
            // Only if nobody answered first. Taking the entry back is what says so — a policy that
            // deliberated while a person chose does not get to overrule them.
            if self
                .shared
                .pending
                .lock()
                .await
                .remove(&request.id)
                .is_some()
            {
                // A policy answering with an id this question never offered is a policy that
                // cannot be applied here. Refusing is the answer that grants nothing; sending the
                // id on would have the server refuse a frame a person already thinks was answered.
                let decision = pending
                    .decision_for(&response.option_id)
                    .unwrap_or_else(|| pending.refusal());
                let option_id = decision.option_id().to_owned();
                self.resolved(&request.id, &option_id, response.source)
                    .await;
                return Some(decision);
            }
        }

        // Three ways this ends: somebody chooses, the deadline the request already carries passes,
        // or the host is going away. All three answer the server; none of them grants anything.
        let settled = tokio::select! {
            biased;
            () = self.shared.host.cancel().cancelled() => None,
            answer = waiting => answer.ok(),
            () = tokio::time::sleep(approvals::APPROVAL_TIMEOUT) => None,
        };

        self.shared.pending.lock().await.remove(&request.id);
        match settled {
            Some(Answer::Chosen {
                decision,
                option_id,
                source,
            }) => {
                self.resolved(&request.id, &option_id, source).await;
                Some(decision)
            }
            Some(Answer::ResolvedByTheServer) => {
                self.resolved(&request.id, "cancel", DecisionSource::Cancelled)
                    .await;
                None
            }
            None => {
                let decision = pending.refusal();
                self.resolved(&request.id, decision.option_id(), DecisionSource::Expired)
                    .await;
                Some(decision)
            }
        }
    }

    async fn resolved(&self, request_id: &str, option_id: &str, source: DecisionSource) {
        self.shared
            .emit(EventKind::ApprovalResolved {
                request_id: request_id.to_owned(),
                decision: ApprovalDecision {
                    option_id: option_id.to_owned(),
                    source,
                },
            })
            .await;
    }

    /// Releases the question the server says it is no longer waiting on.
    async fn release_resolved(&self, request_key: &str) {
        let mut pending = self.shared.pending.lock().await;
        let Some(id) = pending
            .iter()
            .find(|(_, entry)| entry.request_key == request_key)
            .map(|(id, _)| id.clone())
        else {
            // Nothing registered under it yet. Either this is somebody else's question, or the
            // task that will register it has not reached the map — and it checks here for exactly
            // this, rather than waiting out a deadline on a question already withdrawn.
            drop(pending);
            self.shared
                .resolved_early
                .lock()
                .await
                .insert(request_key.to_owned());
            return;
        };
        if let Some(entry) = pending.remove(&id) {
            let _ = entry.answer.send(Answer::ResolvedByTheServer);
        }
    }
}

impl CodexSession {
    /// A session over a connection that is already pumping and already has a thread.
    pub(crate) fn new(
        info: SessionInfo,
        shared: Arc<Shared>,
        client: Arc<Client>,
        control: Arc<dyn ProcessControl>,
    ) -> Self {
        Self {
            info,
            shared,
            client,
            control,
            closed: AtomicBool::new(false),
            announced_session: AtomicBool::new(false),
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
        rpc_method: &str,
        params: P,
        turn_of: impl FnOnce(R) -> (TurnHandle, Option<String>) + Send,
        announce_session: bool,
    ) -> Result<(TurnStream, Option<String>)>
    where
        R: serde::de::DeserializeOwned,
    {
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed { subject: "session" });
        }

        let (sink, events) = EventSink::new(
            self.shared.session_id.clone(),
            turn_id.clone(),
            Arc::clone(self.shared.host.clock()),
            self.shared.host.limits().turn_channel_capacity,
        );

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
                ));
            }
            // Installed before the call, because notifications for this turn can arrive before its
            // own response does.
            *turn = Some(ActiveTurn {
                sink: sink.clone(),
                turn_id: turn_id.clone(),
                native_turn_id: String::new(),
                cancel_reason: None,
            });
        }

        if announce_session {
            let _ = sink
                .emit(EventKind::SessionStarted {
                    native_session_id: self.info.ids.native_session_id.clone(),
                    resumed: self.info.resumed,
                })
                .await;
        }

        let answer: Result<R> = self.client.request(rpc_method, params).await;
        let (handle, extra) = match answer {
            Ok(answer) => turn_of(answer),
            Err(error) => {
                // The turn never started, so the stream it would have written to is closed here
                // rather than left for a `turn/completed` that will never come.
                self.shared.turn.lock().await.take();
                return Err(error);
            }
        };

        if let Some(active) = self.shared.turn.lock().await.as_mut() {
            active.native_turn_id.clone_from(&handle.id);
        }

        // Somebody ended this turn while its own handle was still on the wire — a cancel with no
        // id to name, or a close. The vendor's turn is running regardless of what this side did
        // with the slot, and this is the first moment anything here can name it.
        if self.shared.abandoned.lock().await.remove(&turn_id) {
            self.interrupt(handle.id.clone()).await;
        }

        Ok((
            TurnStream {
                turn_id,
                native_turn_id: handle.id,
                events,
            },
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

    fn input_for(request: &TurnRequest) -> Vec<UserInput> {
        let mut input = vec![UserInput::text(request.input.clone())];
        // Only what the vendor takes. `UserInput` has an image arm and no general-purpose file
        // arm, so anything else would have to be smuggled in as text — which is a different thing
        // from what the host attached, under a name it did not choose.
        input.extend(
            request
                .attachments
                .iter()
                .filter(|attachment| {
                    attachment.kind == mango_external_agents::AttachmentKind::Image
                })
                .map(|attachment| UserInput::Image {
                    url: data_url(&attachment.mime_type, &attachment.bytes),
                }),
        );
        input
    }
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
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        let configuration = request
            .configuration
            .clone()
            .unwrap_or_else(|| self.info.effective_configuration.clone());

        // A level is a sandbox and an approval policy together, and `turn/start` takes only the
        // policy. Sending half of one would produce a configuration nobody chose, in whichever
        // direction it went: a turn narrowed to read-only would keep the thread's full-access
        // sandbox and lose its prompts along with it, and a turn widened from read-only would ask
        // for permissions its sandbox will not grant. Refused rather than half-applied — a host
        // that wants a different level opens a session at that level.
        if configuration.level != self.info.effective_configuration.level {
            return Err(Error::Protocol {
                expected: format!(
                    "a turn at this session's own permission level, {:?}; the app-server takes a \
                     sandbox on the thread and only an approval policy on a turn",
                    self.info.effective_configuration.level
                ),
                received: format!("{:?}", configuration.level),
            });
        }
        let vendor = crate::permissions::VendorConfiguration::for_pair(
            configuration.level,
            configuration.routing,
        );
        // Read, not taken. The announcement rides the turn's own stream, so a turn the server
        // refuses takes the announcement down with it — and a flag set in advance would mean the
        // host never hears which vendor session it is talking to, or whether it was resumed.
        let announce_session = !self.announced_session.load(Ordering::Acquire);

        let params = TurnStartParams {
            thread_id: self.shared.thread_id().to_owned(),
            input: Self::input_for(&request),
            model: configuration.model.clone(),
            effort: configuration.effort.clone(),
            approval_policy: Some(vendor.approval_policy),
            approvals_reviewer: Some(vendor.approvals_reviewer),
        };

        let (stream, _) = self
            .begin::<_, TurnStartResponse>(
                request.turn_id,
                method::TURN_START,
                params,
                |response| (response.turn, None),
                announce_session,
            )
            .await?;
        self.announced_session.store(true, Ordering::Release);
        Ok(stream)
    }

    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        let entry = self
            .shared
            .pending
            .lock()
            .await
            .remove(&response.request_id);
        let Some(entry) = entry else {
            return Err(Error::Protocol {
                expected: String::from("an approval this session is still waiting on"),
                received: response.request_id,
            });
        };
        let Some(decision) = entry.pending.decision_for(&response.option_id) else {
            // Put back: the server is still waiting, and an id nobody offered is the caller's
            // mistake rather than a reason to leave the vendor blocked.
            let option_id = response.option_id.clone();
            self.shared
                .pending
                .lock()
                .await
                .insert(response.request_id.clone(), entry);
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
            })
            .map_err(|_| Error::Closed {
                subject: "approval",
            })
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        let Some((thread_id, turn_id)) = ({
            let mut turn = self.shared.turn.lock().await;
            turn.as_mut().map(|active| {
                active.cancel_reason = Some(reason);
                (
                    self.shared.thread_id().to_owned(),
                    active.native_turn_id.clone(),
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

        if turn_id.is_empty() {
            // The turn's own handle has not come back yet, so there is nothing to name in an
            // interrupt. The stream is ended here rather than left waiting on a `turn/completed`
            // for a turn the server may not have started.
            self.shared.abandon().await;
            return Ok(());
        }

        let _: Value = self
            .client
            .request(
                method::TURN_INTERRUPT,
                TurnInterruptParams { thread_id, turn_id },
            )
            .await?;
        Ok(())
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.shared.release_pending(DecisionSource::Cancelled).await;
        // The turn ends here rather than on a notification: the connection is about to go, and a
        // host holding a stream that will never terminate is worse than one told why it stopped.
        self.shared.abandon().await;

        let closed = self.client.close().await;
        let killed = self.control.kill(CancelReason::from(reason)).await;
        closed.and(killed)
    }

    async fn steer(&self, steer: Steer) -> Result<SteerOutcome> {
        let running = {
            let turn = self.shared.turn.lock().await;
            turn.as_ref()
                .map(|active| (active.turn_id.clone(), active.native_turn_id.clone()))
        };
        let Some((turn_id, native_turn_id)) = running else {
            return Ok(SteerOutcome::Rejected {
                reason: SteerRejection::TurnAlreadyCompleted,
            });
        };
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
                // The core marks its target enum non-exhaustive so a vendor's own targets — a base
                // branch, a commit — can be added later. Substituting the one target this harness
                // knows would run a review of something nobody asked about and report it as the
                // thing they did.
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
                false,
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mango_external_agents::error::VendorError;

    use mango_external_agents::HostContext;
    use mango_external_agents::event::{EventKind, TurnId};
    use mango_external_agents::stream::EventSink;
    use mango_external_agents::testing::FakeLauncher;

    use super::{ActiveTurn, Shared, base64, data_url};

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
        let (sink, events) = EventSink::new(
            mango_external_agents::SessionId::new("chat-1"),
            turn_id.clone(),
            Arc::clone(shared.host.clock()),
            8,
        );
        *shared.turn.lock().await = Some(ActiveTurn {
            sink,
            turn_id: turn_id.clone(),
            native_turn_id: native_turn_id.to_owned(),
            cancel_reason: None,
        });
        (
            turn_id.clone(),
            mango_external_agents::stream::TurnStream {
                turn_id,
                native_turn_id: native_turn_id.to_owned(),
                events,
            },
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

    /// A turn given up before its own handle arrived leaves nothing on this side to stop it by.
    /// The call still parked on that answer is the only thing that will ever know the id.
    #[tokio::test]
    async fn a_turn_abandoned_before_the_server_named_it_is_left_for_its_own_answer_to_stop() {
        let shared = shared();
        let (turn_id, _stream) = running(&shared, "").await;

        shared.abandon().await;

        assert!(
            shared.abandoned.lock().await.contains(&turn_id),
            "expected the turn to be recorded for the answer still in flight"
        );
    }

    /// The other half: a turn whose handle already arrived was cancelled by name, or belongs to a
    /// session that is killing the process. Recording it would send an interrupt nobody asked for.
    #[tokio::test]
    async fn a_turn_the_server_already_named_is_not_left_for_anybody_to_stop() {
        let shared = shared();
        let (_turn_id, _stream) = running(&shared, "vendor-turn-1").await;

        shared.abandon().await;

        assert!(
            shared.abandoned.lock().await.is_empty(),
            "expected nothing to be recorded for a turn this side can name"
        );
    }

    /// The grace exists so a closing session cannot park on a host that stopped reading, and one
    /// event is what fits through it: a marker with no terminal behind it is the one shape the
    /// core's contract rules out.
    #[tokio::test]
    async fn an_abandoned_turn_ends_with_a_terminal_and_nothing_before_it() {
        let shared = shared();
        let (_turn_id, mut stream) = running(&shared, "vendor-turn-1").await;

        shared.abandon().await;

        let event = stream.recv().await.expect("expected an ending");
        assert!(
            event.is_terminal(),
            "expected the first event to be the terminal, received {:?}",
            event.kind
        );
        assert!(
            stream.recv().await.is_none(),
            "expected the stream to end after its terminal"
        );
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
