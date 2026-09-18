//! A JSON-RPC peer over a [`Link`], in both directions.
//!
//! "Both directions" is the requirement that shaped it. These vendors do not merely stream at a
//! client — they *ask the client things* and block until they get an answer: Codex's approval
//! requests, ACP's `session/request_permission`. A codec that only turned messages into events
//! could never reply, which is why a harness is semantic rather than a reducer, and why
//! correlating request ids is the library's job rather than each harness's.
//!
//! Vendor-neutral on purpose: two copies of one wire format would drift apart, and the drift would
//! show up as a hung turn rather than as a failing test. What each vendor calls itself enters only
//! through [`ClientOptions::peer_name`], which appears in messages a person may read.
//!
//! Framing, byte caps and process teardown all belong to the transport underneath; this speaks the
//! protocol on top of them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex as StdMutex, PoisonError};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::error::{Error, ErrorCode, Result, VendorError, jsonrpc_code_is_retryable};
use crate::host::{CancelToken, Limits};
use crate::link::{Link, LinkSender};

/// One request's id, exactly as it arrived.
///
/// Kept as raw JSON rather than normalised to a string, and that is load-bearing rather than tidy:
/// ids may be strings or numbers, and some agents number their requests from zero. Replying to
/// request `0` with `"0"` is a different id, so the peer never matches the answer to the question
/// and blocks forever — which presents as a turn that renders an approval, accepts a click, and
/// then simply never finishes.
#[derive(Clone, PartialEq, Eq)]
pub struct RequestId(Value);

impl std::fmt::Debug for RequestId {
    /// Reports the JSON id type without logging the peer-provided identifier.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestId")
            .field("json_type", &json_value_type(&self.0))
            .finish()
    }
}

impl RequestId {
    /// Wraps an id as it arrived.
    pub fn new(raw: Value) -> Self {
        Self(raw)
    }

    /// The id as JSON, for echoing back.
    pub fn as_json(&self) -> &Value {
        &self.0
    }

    /// The id as a map key, where the JSON type no longer matters.
    ///
    /// Used on this side only: a string `"1"` and a number `1` name the same pending call, and a
    /// peer that answers with the other spelling is answering the right question.
    pub fn key(&self) -> String {
        match &self.0 {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        }
    }
}

impl std::fmt::Display for RequestId {
    /// Names the JSON type, never the id itself.
    ///
    /// A peer picks its own ids, so a string id is peer-controlled text that a host writing
    /// `{id}` into a log would carry across a diagnostic boundary. Correlation happens through
    /// [`key`](Self::key) and [`as_json`](Self::as_json), which stay exact.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a {} request id", json_value_type(&self.0))
    }
}

/// A JSON-RPC error body.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    /// The peer's code.
    pub code: i64,
    /// The peer's message.
    pub message: String,
    /// Whatever else the peer attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Debug for JsonRpcError {
    /// Reports error structure without logging peer-provided text or JSON payloads.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JsonRpcError")
            .field("code", &self.code)
            .field("message_bytes", &self.message.len())
            .field("data_type", &self.data.as_ref().map(json_value_type))
            .finish()
    }
}

impl JsonRpcError {
    /// The failure this represents, with the peer's structure kept.
    pub fn into_vendor_error(self, code: ErrorCode, request_id: Option<String>) -> VendorError {
        let retryable = jsonrpc_code_is_retryable(self.code);
        let mut error =
            VendorError::new(code, self.message).with_vendor_code(self.code.to_string(), retryable);
        error.request_id = request_id;
        error
    }
}

/// What a handler answers a peer's question with.
#[derive(Clone, PartialEq, Eq)]
pub enum ServerRequestOutcome {
    /// An answer.
    Answer(Value),
    /// A refusal, in the protocol's own shape.
    Failure(JsonRpcError),
}

impl std::fmt::Debug for ServerRequestOutcome {
    /// Reports reply shape without logging the peer answer or error payload.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Answer(value) => formatter
                .debug_struct("Answer")
                .field("result_type", &json_value_type(value))
                .finish(),
            Self::Failure(error) => formatter.debug_tuple("Failure").field(error).finish(),
        }
    }
}

/// Why the peer's read side stopped without this client closing it first.
#[derive(Clone, PartialEq, Eq)]
pub enum PeerTermination {
    /// The peer closed its output.
    Exited,
    /// Reading the peer failed.
    LinkFailed(String),
    /// Peer work filled the bounded handoff queue before the handler could consume it.
    NotificationByteBackpressure {
        /// The encoded byte budget for queued and in-flight peer callbacks.
        limit: usize,
    },
    /// The event-count handoff budget was exhausted.
    NotificationBackpressure {
        /// The number of peer messages the client can retain while the handler is busy.
        limit: usize,
    },
}

impl std::fmt::Debug for PeerTermination {
    /// Reports why the peer stopped without logging its unstructured link failure.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited => formatter.write_str("Exited"),
            Self::LinkFailed(error) => formatter
                .debug_struct("LinkFailed")
                .field("message_bytes", &error.len())
                .finish(),
            Self::NotificationByteBackpressure { limit } => formatter
                .debug_struct("NotificationByteBackpressure")
                .field("limit", limit)
                .finish(),
            Self::NotificationBackpressure { limit } => formatter
                .debug_struct("NotificationBackpressure")
                .field("limit", limit)
                .finish(),
        }
    }
}

impl std::fmt::Display for PeerTermination {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited => formatter.write_str("the peer exited"),
            Self::LinkFailed(error) => write!(
                formatter,
                "the peer link failed with an unstructured error ({} bytes)",
                error.len()
            ),
            Self::NotificationByteBackpressure { limit } => write!(
                formatter,
                "the peer exceeded the queued callback payload budget ({limit} bytes)"
            ),
            Self::NotificationBackpressure { limit } => write!(
                formatter,
                "the peer sent more messages than the client could retain while its handler was busy (limit {limit})"
            ),
        }
    }
}

fn json_value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// What the harness above does with what the peer said.
#[async_trait::async_trait]
pub trait PeerHandler: Send + Sync {
    /// The peer announced something.
    ///
    /// Dispatched in arrival order and awaited, so a handler that pushes into a full event sink
    /// applies backpressure to the peer instead of letting events pile up here.
    async fn on_notification(&self, method: String, params: Value);

    /// The peer asked something and is waiting.
    ///
    /// Answered on a task of its own, so a question that waits for a person does not stop the
    /// stream of events arriving meanwhile. A handler that cannot decide should refuse rather than
    /// never return: an unanswered request blocks the vendor on a reply that never comes, which
    /// presents as a hung turn rather than as the failure it is.
    async fn on_request(
        &self,
        method: String,
        params: Value,
        id: RequestId,
    ) -> ServerRequestOutcome;

    /// The peer's read side ended unexpectedly.
    ///
    /// A handler can release state that only a complete vendor turn would otherwise clear. This
    /// callback is never made for [`Client::close`], whose caller already owns that shutdown.
    async fn on_terminated(&self, _termination: PeerTermination) {}
}

/// How this client speaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientOptions {
    /// The peer as a person would name it, such as `Codex app-server`.
    ///
    /// Host-authored text. It reaches a person through the host's own copy, never through an
    /// [`Error`] this client returns.
    pub peer_name: String,
    /// The vendor prefix of the [`ErrorCode`] minted when the peer answers with an error frame.
    ///
    /// `&'static str` is the provenance marker, the same one [`ErrorCode::from_static`] carries: a
    /// prefix is written by the harness that compiles against this client, never derived from
    /// [`peer_name`](ClientOptions::peer_name), which a host fills in and which may carry its own
    /// text. A code is diagnostic — `Display` writes it — so what it may contain is decided here,
    /// where it is made.
    ///
    /// What the bound does not do is make a literal safe by itself. It rules out runtime data,
    /// not a deliberate one: `env!("SOMETHING")` is also `&'static str`, and a lowercase label
    /// passes the shape check `ErrorCode`'s `Display` applies. So this is the same obligation
    /// [`ErrorCode::from_static`] places on every harness that names its own codes — write the
    /// vendor's name, `codex` or `claude`, and nothing a person would not want in a log line.
    /// Sealing it here without sealing that constructor would move the obligation, not remove it.
    pub code_prefix: &'static str,
    /// Whether to write the `"jsonrpc": "2.0"` member.
    ///
    /// Not every dialect this library drives writes it, and a peer that validates strictly will
    /// refuse a frame carrying a member its own schema does not have.
    pub include_version_header: bool,
    /// How long a request waits before it is a failure.
    pub request_timeout: Duration,
    /// How many of the peer's own questions may be in flight at once.
    ///
    /// A question from the peer is answered on a task of its own, so a person deciding on an
    /// approval does not stop the events a turn is rendering meanwhile. Nothing else bounds how
    /// many of those tasks exist: the line cap bounds each frame's size, never the number of
    /// them, so a peer writing request frames as fast as the pipe allows would spawn one task per
    /// frame. Past this many, the next question is refused rather than spawned.
    pub max_in_flight_requests: usize,
    /// How many peer messages that need the handler can wait while responses keep settling.
    pub max_pending_notifications: usize,
    /// Maximum outbound RPCs awaiting a response.
    pub max_pending_requests: usize,
    /// Maximum encoded bytes held by queued or in-flight peer callbacks.
    pub max_pending_bytes: usize,
    /// Deadline for each shutdown stage and callback drain.
    pub shutdown_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            peer_name: String::from("external agent"),
            code_prefix: "peer",
            include_version_header: true,
            request_timeout: Duration::from_secs(120),
            max_in_flight_requests: 256,
            max_pending_notifications: 256,
            max_pending_requests: 64,
            max_pending_bytes: 8 * 1024 * 1024,
            shutdown_timeout: Duration::from_millis(200),
        }
    }
}

impl ClientOptions {
    /// Names the peer.
    pub fn new(peer_name: impl Into<String>) -> Self {
        Self {
            peer_name: peer_name.into(),
            ..Self::default()
        }
    }

    /// Prefixes this client's error codes with a vendor name the harness knows at compile time.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::ClientOptions;
    ///
    /// let options = ClientOptions::new("Codex app-server").with_code_prefix("codex");
    /// assert_eq!(options.code_prefix, "codex");
    /// ```
    #[must_use]
    pub fn with_code_prefix(mut self, code_prefix: &'static str) -> Self {
        self.code_prefix = code_prefix;
        self
    }

    /// Omits the `"jsonrpc"` member, for a dialect that does not write one.
    #[must_use]
    pub fn without_version_header(mut self) -> Self {
        self.include_version_header = false;
        self
    }

    /// Answers at most this many of the peer's questions at once.
    #[must_use]
    pub fn with_max_in_flight_requests(mut self, max_in_flight_requests: usize) -> Self {
        self.max_in_flight_requests = max_in_flight_requests;
        self
    }

    /// Buffers at most this many peer messages while responses continue through the pump.
    #[must_use]
    pub fn with_max_pending_notifications(mut self, max_pending_notifications: usize) -> Self {
        self.max_pending_notifications = max_pending_notifications;
        self
    }

    /// Waits this long for an answer.
    #[must_use]
    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
        self
    }

    /// Takes every bound this client has from the host's own.
    ///
    /// A harness holds a [`HostContext`](crate::HostContext), not a `Duration`, so without this
    /// each one would have to remember to thread `host.limits().request_timeout` through by hand —
    /// and the one that forgot would quietly wait two minutes on a host that asked for ten
    /// seconds.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::ClientOptions;
    /// use mango_external_agents::Limits;
    /// use std::time::Duration;
    ///
    /// let limits = Limits {
    ///     request_timeout: Duration::from_secs(10),
    ///     ..Limits::default()
    /// };
    /// let options = ClientOptions::new("Codex app-server").with_limits(&limits);
    ///
    /// assert_eq!(options.request_timeout, Duration::from_secs(10));
    /// ```
    #[must_use]
    pub fn with_limits(mut self, limits: &Limits) -> Self {
        self.request_timeout = limits.request_timeout;
        self.max_pending_requests = limits.max_pending_requests;
        self.max_in_flight_requests = limits.max_pending_requests;
        self.max_pending_notifications = limits.turn_channel_capacity;
        self.max_pending_bytes = limits.turn_buffer_bytes;
        self.shutdown_timeout = limits.shutdown_timeout;
        self
    }
}

/// A JSON-RPC client that also answers.
pub struct Client {
    state: Arc<ClientState>,
    pump: Mutex<Option<JoinHandle<()>>>,
}

struct ClientState {
    sender: Mutex<Box<dyn LinkSender>>,
    pending: Mutex<HashMap<String, oneshot::Sender<std::result::Result<Value, JsonRpcError>>>>,
    /// The connection's runtime also owns cleanup when a request is dropped on another thread.
    runtime: tokio::runtime::Handle,
    options: ClientOptions,
    next_id: AtomicU64,
    closed: AtomicBool,
    shutdown: CancelToken,
    /// The peer's questions currently being answered, so they can be counted and taken down.
    in_flight: StdMutex<JoinSet<()>>,
    notifications: StdMutex<Option<JoinHandle<()>>>,
    peer_bytes: Arc<tokio::sync::Semaphore>,
}

/// Removes correlation state when a request future is dropped at any await.
struct PendingCall {
    state: Arc<ClientState>,
    id: String,
}

impl Drop for PendingCall {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.state.pending.try_lock() {
            pending.remove(&self.id);
            return;
        }
        // A host can drop this future outside the connection's runtime. Keep cleanup on the
        // runtime that owns the pump rather than retaining an abandoned correlation until the
        // next request or close. The host keeps that runtime alive through session shutdown.
        let state = Arc::clone(&self.state);
        let id = self.id.clone();
        self.state.runtime.spawn(async move {
            state.pending.lock().await.remove(&id);
        });
    }
}

impl Client {
    /// Starts speaking, pumping the link on a task of its own.
    ///
    /// The pump stops when the peer goes away, when [`Client::close`] is called, or when the
    /// client is dropped.
    pub fn connect(link: Link, handler: Arc<dyn PeerHandler>, options: ClientOptions) -> Self {
        let (sender, receiver) = link.split();
        let notification_capacity = options.max_pending_notifications.max(1);
        let peer_bytes = Arc::new(tokio::sync::Semaphore::new(
            options
                .max_pending_bytes
                .min(tokio::sync::Semaphore::MAX_PERMITS),
        ));
        let state = Arc::new(ClientState {
            sender: Mutex::new(sender),
            pending: Mutex::new(HashMap::new()),
            runtime: tokio::runtime::Handle::current(),
            options,
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            shutdown: CancelToken::new(),
            in_flight: StdMutex::new(JoinSet::new()),
            notifications: StdMutex::new(None),
            peer_bytes,
        });
        let (notifications, notification_receiver) = mpsc::channel(notification_capacity);
        *state
            .notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(tokio::spawn(peer_work_pump(
            Arc::clone(&state),
            notification_receiver,
            Arc::clone(&handler),
        )));
        let pump = tokio::spawn(pump(Arc::clone(&state), receiver, handler, notifications));
        Self {
            state,
            pump: Mutex::new(Some(pump)),
        }
    }

    /// Calls a method and waits for its answer.
    ///
    /// # Errors
    ///
    /// [`Error::Vendor`] when the peer answered with an error frame, [`Error::Timeout`] when it
    /// did not answer in time, [`Error::Link`] when the link failed, and [`Error::Protocol`] when
    /// the answer did not deserialise into `R`.
    pub async fn request<P, R>(&self, method: &str, params: P) -> Result<R>
    where
        P: Serialize + Send,
        R: DeserializeOwned,
    {
        self.request_with_timeout(method, params, self.state.options.request_timeout)
            .await
    }

    /// Calls a method with a deadline of its own.
    ///
    /// # Errors
    ///
    /// As [`Client::request`].
    pub async fn request_with_timeout<P, R>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
    ) -> Result<R>
    where
        P: Serialize + Send,
        R: DeserializeOwned,
    {
        let answer = tokio::time::timeout(timeout, self.call(method, params, timeout))
            .await
            .map_err(|_| Error::Timeout {
                operation: String::from("a JSON-RPC request"),
                after: timeout,
            })??;
        serde_json::from_value(answer).map_err(|error| Error::Protocol {
            expected: String::from("a JSON-RPC result"),
            received: error.to_string(),
        })
    }

    /// Tells the peer something, expecting no answer.
    ///
    /// # Errors
    ///
    /// [`Error::Link`] when the link failed. A notification to a closed client is dropped rather
    /// than refused: there is nothing to correlate and nobody waiting.
    pub async fn notify<P>(&self, method: &str, params: P) -> Result<()>
    where
        P: Serialize + Send,
    {
        if self.is_closed() {
            return Ok(());
        }
        let frame = self.state.frame(None, method, params)?;
        self.state.write(frame).await
    }

    /// Stops the pump, fails every call still waiting, and closes this side of the link.
    ///
    /// The process or socket underneath is the caller's to reap: this client did not open it.
    /// Idempotent.
    ///
    /// # Errors
    ///
    /// [`Error::Link`] when closing the link itself failed.
    pub async fn close(&self) -> Result<()> {
        // Stored before the drain rather than inside it, and that order is the contract `call`
        // reads: a caller that finds the map open has, by that fact, arrived before this store,
        // and the drain below cannot run until that caller's entry is in the map.
        self.state.closed.store(true, Ordering::Release);
        self.state.shutdown.cancel();
        self.state.fail_pending(self.state.closed_error()).await;
        self.state.stop_notifications().await;
        // The peer's own questions are answered on tasks of their own, and one waiting for a
        // person would otherwise outlive the client that spawned it — holding its share of the
        // state for the rest of the process, and replying into a link that is already gone. The
        // shutdown flag has already reached them, so this is the moment they need to write the
        // refusal, and it happens before the link is closed under them.
        self.state.drain_in_flight().await;

        let closed = tokio::time::timeout(self.state.options.shutdown_timeout, async {
            self.state.sender.lock().await.close().await
        })
        .await
        .map_err(|_| Error::Timeout {
            operation: String::from("JSON-RPC link shutdown"),
            after: self.state.options.shutdown_timeout,
        })
        .and_then(std::convert::identity);
        if let Some(pump) = self.pump.lock().await.take() {
            // Taken down rather than waited out: the shutdown flag is only read between messages,
            // so a pump parked inside a handler — which is where a host that stopped reading its
            // turn stream parks it — would never reach the flag, and closing would never return.
            pump.abort();
            let _ = pump.await;
        }
        closed
    }

    /// Whether this client has been closed or the peer has gone.
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Acquire)
    }

    /// The peer as a person would name it.
    pub fn peer_name(&self) -> &str {
        &self.state.options.peer_name
    }

    async fn call<P>(&self, method: &str, params: P, timeout: Duration) -> Result<Value>
    where
        P: Serialize + Send,
    {
        if self.is_closed() {
            return Err(Error::Closed { subject: "link" });
        }
        let id = self
            .state
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        // Built before the map is touched: a frame this refuses would otherwise leave a waiter
        // behind for a caller that is returning the failure here.
        let frame = self
            .state
            .frame(Some(Value::String(id.clone())), method, params)?;

        let (answer, waiting) = oneshot::channel();
        {
            let mut pending = self.state.pending.lock().await;
            // Read again, holding the map, because the check above is not the same instant as this
            // insert. `close` stores the flag and then drains under this lock; a call checks under
            // this lock and then inserts. If the read here says open, the store had not landed, and
            // the drain that follows it needs the lock this call holds until the entry is in — so
            // it sees the entry. If it says closed, the call returns. Nothing else can happen, and
            // in particular no entry can land after the drain that was supposed to fail it, which
            // is a caller waiting out its whole `request_timeout` for a link that is already gone.
            if self.state.closed.load(Ordering::Acquire) {
                return Err(Error::Closed { subject: "link" });
            }
            pending.retain(|_, answer| !answer.is_closed());
            if pending.len() >= self.state.options.max_pending_requests {
                return Err(Error::LimitExceeded {
                    subject: "pending JSON-RPC requests",
                    limit: self.state.options.max_pending_requests,
                    received: pending.len().saturating_add(1),
                });
            }
            pending.insert(id.clone(), answer);
        }
        let _pending = PendingCall {
            state: Arc::clone(&self.state),
            id: id.clone(),
        };

        if let Err(error) = self.state.write(frame).await {
            // The entry goes before the error leaves: nothing is waiting on this call — the
            // failure is returning here — so an orphan left behind would be failed later by a
            // close or a dying pump, against a caller that had already given up.
            self.state.pending.lock().await.remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(timeout, waiting).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(failure))) => Err(Error::Vendor(failure.into_vendor_error(
                ErrorCode::new(format!("{}-call-failed", self.state.options.code_prefix)),
                Some(id),
            ))),
            Ok(Err(_)) => Err(Error::Link {
                peer: self.state.options.peer_name.clone(),
                message: String::from("a peer that went away before answering a JSON-RPC request"),
            }),
            Err(_) => {
                self.state.pending.lock().await.remove(&id);
                Err(Error::Timeout {
                    operation: String::from("a JSON-RPC request"),
                    after: timeout,
                })
            }
        }
    }
}

impl Drop for Client {
    /// Best effort, and nothing is left waiting by the time it runs.
    ///
    /// [`Client::close`] is the supported shutdown: it fails every call still waiting, lets the
    /// answers in flight write their refusals, and closes the link. This runs when nobody called
    /// it, and there is deliberately no drain here — a caller inside [`Client::request`] holds a
    /// borrow of this client for the life of its future, so no pending call can outlive this, and
    /// a map the last owner is dropping has nothing to fail.
    ///
    /// What is left is the end of the link. The pump and the answers in flight are taken down, and
    /// the sender goes with the last handle to this client's state — which on a child's stdin
    /// is the end-of-input a print-mode vendor waits for.
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Release);
        self.state.shutdown.cancel();
        self.state.abort_in_flight();
        self.state.abort_notifications();
        if let Ok(mut pump) = self.pump.try_lock()
            && let Some(pump) = pump.take()
        {
            pump.abort();
        }
    }
}

impl ClientState {
    fn frame<P: Serialize>(&self, id: Option<Value>, method: &str, params: P) -> Result<String> {
        let mut frame = Map::new();
        if self.options.include_version_header {
            frame.insert(String::from("jsonrpc"), json!("2.0"));
        }
        if let Some(id) = id {
            frame.insert(String::from("id"), id);
        }
        frame.insert(String::from("method"), json!(method));

        let params = serde_json::to_value(params).map_err(|error| Error::Protocol {
            expected: String::from("serialisable JSON-RPC params"),
            received: error.to_string(),
        })?;
        // A dialect that validates strictly refuses a `params: null` it never declared, so an
        // absent parameter is an absent member.
        if !params.is_null() {
            frame.insert(String::from("params"), params);
        }
        Ok(Value::Object(frame).to_string())
    }

    /// What a call still waiting is failed with once this side has stopped speaking.
    fn closed_error(&self) -> JsonRpcError {
        JsonRpcError {
            code: -32000,
            message: format!("the {} connection was closed", self.options.peer_name),
            data: None,
        }
    }

    /// Lets every answer still being composed write its refusal, then takes down what is left.
    ///
    /// Each one is already racing the shutdown flag, so this is a frame's worth of work rather
    /// than a wait on whoever was being asked. The grace is a bound on a slow link, not on a slow
    /// person.
    async fn drain_in_flight(&self) {
        let mut in_flight = std::mem::take(
            &mut *self
                .in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let drained = tokio::time::timeout(self.options.shutdown_timeout, async {
            while in_flight.join_next().await.is_some() {}
        })
        .await;
        if drained.is_err() {
            in_flight.abort_all();
        }
    }

    /// Takes down every answer still being composed, for a caller that cannot wait.
    fn abort_in_flight(&self) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .abort_all();
    }

    async fn stop_notifications(&self) {
        let handle = self
            .notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Lets already queued peer work reach the handler before reporting that the peer vanished.
    ///
    /// A complete activity frame followed by EOF is still observable output. Dropping the sender
    /// first gives the worker that finite tail; the grace keeps a host that stopped reading from
    /// turning peer teardown into an unbounded wait.
    async fn drain_notifications(&self) {
        let handle = self
            .notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        let Some(mut handle) = handle else {
            return;
        };
        if tokio::time::timeout(self.options.shutdown_timeout, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await;
        }
    }

    fn abort_notifications(&self) {
        if let Some(handle) = self
            .notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            handle.abort();
        }
    }

    /// Answers one of the peer's questions on a task this client owns, if there is room.
    ///
    /// The count and the spawn happen under one lock rather than two. Only the pump dispatches
    /// today, so a gap between checking and spawning would hold — until the day anything else
    /// answers a question, and then the bound this exists to enforce is off by however many
    /// dispatchers raced through it.
    fn spawn_answer(
        state: &Arc<Self>,
        handler: &Arc<dyn PeerHandler>,
        method: String,
        params: Value,
        raw_id: Value,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> bool {
        let mut in_flight = state
            .in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The answers already given are reaped first, so the count is what is still being decided
        // rather than everything that was ever asked.
        while in_flight.try_join_next().is_some() {}
        if in_flight.len() >= state.options.max_in_flight_requests {
            return false;
        }
        let owned = Arc::clone(state);
        let handler = Arc::clone(handler);
        in_flight.spawn(async move {
            let _permit = permit;
            answer(&owned, &handler, method, params, raw_id).await;
        });
        true
    }

    async fn write(&self, frame: String) -> Result<()> {
        tokio::time::timeout(self.options.request_timeout, async {
            self.sender.lock().await.send(frame).await
        })
        .await
        .map_err(|_| Error::Timeout {
            operation: String::from("JSON-RPC frame write"),
            after: self.options.request_timeout,
        })?
    }

    async fn fail_pending(&self, failure: JsonRpcError) {
        let waiting: Vec<_> = self.pending.lock().await.drain().collect();
        for (_, answer) in waiting {
            let _ = answer.send(Err(failure.clone()));
        }
    }

    async fn settle(&self, id: &str, outcome: std::result::Result<Value, JsonRpcError>) {
        if let Some(answer) = self.pending.lock().await.remove(id) {
            let _ = answer.send(outcome);
        }
    }
}

async fn pump(
    state: Arc<ClientState>,
    mut receiver: Box<dyn crate::link::LinkReceiver>,
    handler: Arc<dyn PeerHandler>,
    notifications: mpsc::Sender<BudgetedWork>,
) {
    loop {
        let message = tokio::select! {
            biased;
            () = state.shutdown.cancelled() => break,
            message = receiver.recv() => message,
        };

        match message {
            Ok(Some(message)) => {
                if let Err(termination) = dispatch(&state, &notifications, message).await {
                    state.closed.store(true, Ordering::Release);
                    state
                        .fail_pending(JsonRpcError {
                            code: -32000,
                            message: format!(
                                "the {} notification queue reached its limit",
                                state.options.peer_name
                            ),
                            data: None,
                        })
                        .await;
                    state.stop_notifications().await;
                    state.shutdown.cancel();
                    state.drain_in_flight().await;
                    handler.on_terminated(termination).await;
                    break;
                }
            }
            Ok(None) => {
                state.closed.store(true, Ordering::Release);
                state
                    .fail_pending(JsonRpcError {
                        code: -32000,
                        message: format!("the {} exited", state.options.peer_name),
                        data: None,
                    })
                    .await;
                // Closing this sender lets the ordered worker deliver every frame it already
                // owns, including an activity immediately before EOF, before it exits.
                drop(notifications);
                state.drain_notifications().await;
                state.shutdown.cancel();
                state.drain_in_flight().await;
                handler.on_terminated(PeerTermination::Exited).await;
                break;
            }
            Err(error) => {
                let termination = PeerTermination::LinkFailed(error.to_string());
                state.closed.store(true, Ordering::Release);
                state
                    .fail_pending(JsonRpcError {
                        code: -32000,
                        message: format!("the {} link failed: {error}", state.options.peer_name),
                        data: None,
                    })
                    .await;
                drop(notifications);
                state.drain_notifications().await;
                state.shutdown.cancel();
                state.drain_in_flight().await;
                handler.on_terminated(termination).await;
                break;
            }
        }
    }
}

async fn dispatch(
    state: &Arc<ClientState>,
    notifications: &mpsc::Sender<BudgetedWork>,
    message: String,
) -> std::result::Result<(), PeerTermination> {
    // Not every line on a peer's output is a frame. Dropping an unparseable one keeps a stray
    // diagnostic from killing a live turn.
    let Ok(Value::Object(frame)) = serde_json::from_str::<Value>(&message) else {
        return Ok(());
    };

    let id = frame.get("id").cloned();
    let method = frame
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let params = frame.get("params").cloned().unwrap_or(Value::Null);

    if let Some(method) = method {
        let bytes = u32::try_from(message.len()).map_err(|_| {
            PeerTermination::NotificationByteBackpressure {
                limit: state.options.max_pending_bytes,
            }
        })?;
        let permit = Arc::clone(&state.peer_bytes)
            .try_acquire_many_owned(bytes)
            .map_err(|_| PeerTermination::NotificationByteBackpressure {
                limit: state.options.max_pending_bytes,
            })?;
        let work = match id {
            Some(id) => PeerWork::Request { method, params, id },
            None => PeerWork::Notification { method, params },
        };
        return notifications
            .try_send(BudgetedWork {
                work,
                _permit: permit,
            })
            .map_err(|_| PeerTermination::NotificationBackpressure {
                limit: state.options.max_pending_notifications.max(1),
            });
    }

    if let Some(id) = id {
        let outcome = match frame.get("error") {
            Some(error) => Err(
                serde_json::from_value(error.clone()).unwrap_or(JsonRpcError {
                    code: -32603,
                    message: error.to_string(),
                    data: None,
                }),
            ),
            None => Ok(frame.get("result").cloned().unwrap_or(Value::Null)),
        };
        state.settle(&RequestId::new(id).key(), outcome).await;
    }
    Ok(())
}

struct BudgetedWork {
    work: PeerWork,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

enum PeerWork {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        method: String,
        params: Value,
        id: Value,
    },
}

async fn peer_work_pump(
    state: Arc<ClientState>,
    mut notifications: mpsc::Receiver<BudgetedWork>,
    handler: Arc<dyn PeerHandler>,
) {
    while let Some(work) = notifications.recv().await {
        let BudgetedWork { work, _permit } = work;
        match work {
            PeerWork::Notification { method, params } => {
                handler.on_notification(method, params).await
            }
            PeerWork::Request { method, params, id } => {
                // A question that waits for a person must not stop later events after it has
                // reached the head of the queue. Count the task before starting it so a peer
                // cannot use request frames to create unbounded work.
                if !ClientState::spawn_answer(&state, &handler, method, params, id.clone(), _permit)
                {
                    let refusal = JsonRpcError {
                        code: -32000,
                        message: format!(
                            "expected at most {} questions in flight, received one more",
                            state.options.max_in_flight_requests
                        ),
                        data: None,
                    };
                    write_reply(&state, id, ServerRequestOutcome::Failure(refusal)).await;
                }
            }
        }
    }
}

async fn answer(
    state: &Arc<ClientState>,
    handler: &Arc<dyn PeerHandler>,
    method: String,
    params: Value,
    raw_id: Value,
) {
    let id = RequestId::new(raw_id.clone());
    // A question the peer is blocked on has to be answered even when this side is shutting down:
    // leaving it unanswered leaves the vendor waiting, and leaving the task alive leaks it.
    let outcome = tokio::select! {
        outcome = handler.on_request(method, params, id) => outcome,
        () = state.shutdown.cancelled() => ServerRequestOutcome::Failure(state.closed_error()),
    };
    write_reply(state, raw_id, outcome).await;
}

/// Writes one reply frame for a question the peer asked.
async fn write_reply(state: &Arc<ClientState>, raw_id: Value, outcome: ServerRequestOutcome) {
    let mut frame = Map::new();
    if state.options.include_version_header {
        frame.insert(String::from("jsonrpc"), json!("2.0"));
    }
    // The id goes back exactly as it arrived: a peer that numbered its request will not match a
    // stringified answer to it.
    frame.insert(String::from("id"), raw_id);
    match outcome {
        ServerRequestOutcome::Answer(result) => {
            frame.insert(String::from("result"), result);
        }
        ServerRequestOutcome::Failure(failure) => {
            frame.insert(
                String::from("error"),
                serde_json::to_value(failure).unwrap_or(Value::Null),
            );
        }
    }
    // The peer may have died between its question and this reply; the pump reports that.
    let _ = state.write(Value::Object(frame).to_string()).await;
}

#[cfg(test)]
mod tests {
    use super::{
        Client, ClientOptions, JsonRpcError, PeerHandler, PeerTermination, RequestId,
        ServerRequestOutcome,
    };
    use crate::error::Error;
    use crate::testing::ScriptedLink;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use tokio::sync::{Mutex, Notify};

    /// A handler that records what it was told and answers questions from a script.
    struct RecordingHandler {
        notifications: Mutex<Vec<(String, Value)>>,
        terminations: Mutex<Vec<PeerTermination>>,
        answer: Option<ServerRequestOutcome>,
    }

    impl RecordingHandler {
        fn arc(answer: Option<ServerRequestOutcome>) -> Arc<Self> {
            Arc::new(Self {
                notifications: Mutex::new(Vec::new()),
                terminations: Mutex::new(Vec::new()),
                answer,
            })
        }
    }

    #[async_trait::async_trait]
    impl PeerHandler for RecordingHandler {
        async fn on_notification(&self, method: String, params: Value) {
            self.notifications.lock().await.push((method, params));
        }

        async fn on_request(
            &self,
            _method: String,
            _params: Value,
            id: RequestId,
        ) -> ServerRequestOutcome {
            self.answer
                .clone()
                .unwrap_or_else(|| ServerRequestOutcome::Answer(json!({ "echoed": id.key() })))
        }

        async fn on_terminated(&self, termination: PeerTermination) {
            self.terminations.lock().await.push(termination);
        }
    }

    fn client(link: ScriptedLink, handler: Arc<RecordingHandler>) -> Client {
        Client::connect(
            link.into_link(),
            handler,
            ClientOptions::new("Codex app-server").with_request_timeout(Duration::from_secs(5)),
        )
    }

    /// `Display` is the spelling a host reaches for when it logs `{id}`, so it carries the same
    /// guarantee `Debug` does. `key()` stays exact: it is how this side correlates the answer.
    #[test]
    fn displaying_a_request_id_names_its_type_rather_than_the_peers_value() {
        let id = RequestId::new(json!("request-id-secret"));

        assert_eq!(id.to_string(), "a string request id");
        assert_eq!(id.key(), "request-id-secret");
        assert_eq!(
            RequestId::new(json!(7)).to_string(),
            "a number request id",
            "expected the numeric spelling to be named as such"
        );
    }

    #[test]
    fn jsonrpc_debug_omits_raw_peer_payloads() {
        let id = RequestId::new(json!("request-id-secret"));
        let failure = JsonRpcError {
            code: -32001,
            message: String::from("peer-message-secret"),
            data: Some(json!({ "token": "peer-data-secret" })),
        };
        let answer = ServerRequestOutcome::Answer(json!({ "answer": "reply-secret" }));
        let termination = PeerTermination::LinkFailed(String::from("link-detail-secret"));

        for rendered in [
            format!("{id:?}"),
            format!("{id}"),
            format!("{failure:?}"),
            format!("{answer:?}"),
            format!("{termination:?}"),
        ] {
            for secret in [
                "request-id-secret",
                "peer-message-secret",
                "peer-data-secret",
                "reply-secret",
                "link-detail-secret",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "expected no peer payload in diagnostics, received {rendered}"
                );
            }
        }
    }

    /// A frame that cannot be built must not leave a waiter behind: the map is what `close` and a
    /// dying pump drain, so an orphan is failed later against a caller that gave up here.
    #[tokio::test]
    async fn params_that_cannot_be_written_leave_no_waiter_behind() {
        struct Unwritable;

        impl serde::Serialize for Unwritable {
            fn serialize<S: serde::Serializer>(
                &self,
                _serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("these params cannot be written"))
            }
        }

        let link = ScriptedLink::new();
        let client = client(link.clone(), RecordingHandler::arc(None));

        let refused = client.request::<_, Value>("thread/start", Unwritable).await;
        assert!(
            matches!(refused, Err(Error::Protocol { .. })),
            "expected the params to be refused, received {refused:?}"
        );

        let waiting = client.state.pending.lock().await.len();
        assert_eq!(
            waiting, 0,
            "expected nothing left waiting, received {waiting}"
        );
        assert!(link.sent().is_empty(), "received {:?}", link.sent());
    }

    #[tokio::test]
    async fn abandoned_request_releases_its_pending_entry() {
        let link = ScriptedLink::new();
        let client = Arc::new(client(link.clone(), RecordingHandler::arc(None)));
        let pending = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<_, Value>("work", json!({})).await })
        };
        link.wait_for_sent(1).await;
        pending.abort();
        assert!(pending.await.expect_err("aborted request").is_cancelled());
        assert_eq!(
            client.state.pending.lock().await.len(),
            0,
            "expected abandoned RPC to release its pending entry"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn outgoing_requests_respect_the_host_pending_budget() {
        let link = ScriptedLink::new();
        let limits = crate::Limits {
            max_pending_requests: 1,
            ..crate::Limits::default()
        };
        let client = Arc::new(Client::connect(
            link.clone().into_link(),
            RecordingHandler::arc(None),
            ClientOptions::new("peer").with_limits(&limits),
        ));
        let first = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<_, Value>("first", json!({})).await })
        };
        link.wait_for_sent(1).await;
        let second = tokio::time::timeout(
            Duration::from_secs(1),
            client.request::<_, Value>("second", json!({})),
        )
        .await;
        assert!(
            matches!(second, Ok(Err(Error::LimitExceeded { .. }))),
            "expected pending-budget refusal before submission, received {second:?}"
        );
        assert_eq!(link.sent().len(), 1);
        first.abort();
        let _ = first.await;
    }

    #[tokio::test]
    async fn queued_peer_payloads_have_a_byte_budget_that_releases_with_the_work() {
        let frame = String::from(r#"{"method":"delta","params":{"text":"payload"}}"#);
        let options = ClientOptions {
            max_pending_bytes: frame.len(),
            ..ClientOptions::default()
        };
        let link = ScriptedLink::new();
        let client = Client::connect(link.into_link(), RecordingHandler::arc(None), options);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
        super::dispatch(&client.state, &sender, frame.clone())
            .await
            .expect("first payload");
        let refused = super::dispatch(&client.state, &sender, frame.clone()).await;
        assert!(
            matches!(
                refused,
                Err(super::PeerTermination::NotificationByteBackpressure { .. })
            ),
            "expected byte-pressure refusal even with free event slots, received {refused:?}"
        );
        // Responses still take the direct correlation path when callback payload storage is full.
        super::dispatch(
            &client.state,
            &sender,
            String::from(r#"{"id":"missing","result":{}}"#),
        )
        .await
        .expect("response bypasses callback budget");
        drop(receiver.recv().await.expect("queued work"));
        super::dispatch(&client.state, &sender, frame)
            .await
            .expect("released bytes can be reused");
        client.close().await.expect("close");
    }

    #[test]
    fn host_limits_cover_rpc_requests_callbacks_and_shutdown() {
        let limits = crate::Limits {
            max_pending_requests: 3,
            turn_buffer_bytes: 1234,
            turn_channel_capacity: 7,
            shutdown_timeout: Duration::from_secs(9),
            ..crate::Limits::default()
        };
        let options = ClientOptions::default().with_limits(&limits);
        assert_eq!(options.max_pending_requests, 3);
        assert_eq!(options.max_in_flight_requests, 3);
        assert_eq!(options.max_pending_notifications, 7);
        assert_eq!(options.max_pending_bytes, 1234);
        assert_eq!(options.shutdown_timeout, Duration::from_secs(9));
    }

    /// The window this closes is a preemption between `call` reading the closed flag and inserting
    /// its waiter, which no test can schedule. Holding the map reproduces the same ordering
    /// deterministically: the caller is past its check and not yet inserted when a close lands its
    /// flag and queues behind it. Without the check under the lock the caller inserts anyway and
    /// is failed by the drain that follows — an answer that names the peer for a refusal this side
    /// made, and on the real timeline no drain follows at all and the caller waits out its whole
    /// `request_timeout`.
    #[tokio::test]
    async fn a_call_that_reaches_the_map_behind_a_close_is_refused_by_this_side() {
        let link = ScriptedLink::new();
        let client = Arc::new(client(link.clone(), RecordingHandler::arc(None)));

        let held = client.state.pending.lock().await;

        let calling = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<_, Value>("thread/start", json!({})).await })
        };
        tokio::task::yield_now().await;

        let closing = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.close().await })
        };
        tokio::task::yield_now().await;

        assert!(
            client.is_closed(),
            "expected the close to have landed its flag while the caller waits for the map"
        );
        drop(held);

        let refused = calling.await.expect("expected the caller to finish");
        assert!(
            matches!(refused, Err(Error::Closed { subject: "link" })),
            "expected this side to refuse the call, received {refused:?}"
        );
        closing
            .await
            .expect("expected the close to finish")
            .expect("expected a clean close");
    }

    #[tokio::test]
    async fn a_request_is_answered_by_the_response_that_names_it() {
        let link = ScriptedLink::new();
        link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":{"threadId":"t-1"}}"#);
        let client = client(link.clone(), RecordingHandler::arc(None));

        let answer: Value = client
            .request("thread/start", json!({}))
            .await
            .expect("expected an answer");
        assert_eq!(answer, json!({ "threadId": "t-1" }));

        let sent = link.sent();
        assert_eq!(sent.len(), 1, "received {sent:?}");
        assert!(sent[0].contains(r#""method":"thread/start""#));
        assert!(sent[0].contains(r#""jsonrpc":"2.0""#));
    }

    #[tokio::test]
    async fn answers_arriving_out_of_order_still_reach_the_right_caller() {
        let link = ScriptedLink::new();
        let client = Arc::new(client(link.clone(), RecordingHandler::arc(None)));

        let first = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<_, Value>("first", json!({})).await })
        };
        let second = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<_, Value>("second", json!({})).await })
        };
        link.wait_for_sent(2).await;

        // The second question is answered first.
        link.push_line(r#"{"jsonrpc":"2.0","id":"2","result":"second-answer"}"#);
        link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"first-answer"}"#);

        assert_eq!(
            second
                .await
                .expect("expected the task to finish")
                .expect("expected an answer"),
            json!("second-answer")
        );
        assert_eq!(
            first
                .await
                .expect("expected the task to finish")
                .expect("expected an answer"),
            json!("first-answer")
        );
    }

    #[tokio::test]
    async fn an_id_the_peer_numbered_is_echoed_as_a_number() {
        // Some agents number their server-to-client requests from zero. Answering `0` with `"0"`
        // is a different id: the peer never matches the reply and stays blocked, which reads as a
        // turn that hangs after an approval is clicked.
        let link = ScriptedLink::new();
        link.push_line(r#"{"jsonrpc":"2.0","id":0,"method":"session/request_permission"}"#);
        let client = client(link.clone(), RecordingHandler::arc(None));

        link.wait_for_sent(1).await;
        let reply: Value =
            serde_json::from_str(&link.sent()[0]).expect("expected the reply to be a frame");
        assert_eq!(reply["id"], json!(0), "received {reply}");
        assert_eq!(reply["result"], json!({ "echoed": "0" }));

        client.close().await.expect("expected a clean close");
    }

    #[tokio::test]
    async fn a_notification_reaches_the_handler_in_arrival_order() {
        let link = ScriptedLink::new();
        let handler = RecordingHandler::arc(None);
        let client = client(link.clone(), Arc::clone(&handler));

        link.push_line(r#"{"jsonrpc":"2.0","method":"item/started","params":{"n":1}}"#);
        link.push_line(r#"{"jsonrpc":"2.0","method":"item/completed","params":{"n":2}}"#);
        link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":null}"#);
        client
            .request::<_, Value>("ping", json!({}))
            .await
            .expect("expected an answer");

        let seen = handler.notifications.lock().await;
        assert_eq!(
            seen.iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            vec!["item/started", "item/completed"]
        );
    }

    #[tokio::test]
    async fn an_error_frame_keeps_the_peers_own_code_and_retryability() {
        let link = ScriptedLink::new();
        link.push_line(
            r#"{"jsonrpc":"2.0","id":"1","error":{"code":-32601,"message":"no such method"}}"#,
        );
        let client = client(link.clone(), RecordingHandler::arc(None));

        let error = client
            .request::<_, Value>("nope", json!({}))
            .await
            .expect_err("expected a failure, received an answer");
        let Error::Vendor(vendor) = error else {
            panic!("expected a vendor failure, received {error:?}");
        };
        assert_eq!(vendor.message, "no such method");
        assert_eq!(vendor.vendor_code.as_deref(), Some("-32601"));
        assert!(
            !vendor.retryable,
            "expected a reserved code to be non-retryable"
        );
    }

    #[tokio::test]
    async fn a_call_whose_write_never_lands_fails_without_orphaning_itself() {
        let link = ScriptedLink::new();
        link.fail_sends("EPIPE: the vendor process is gone");
        let client = client(link.clone(), RecordingHandler::arc(None));

        let error = client
            .request::<_, Value>("thread/start", json!({}))
            .await
            .expect_err("expected a failure, received an answer");
        assert!(
            error.to_string().contains("EPIPE"),
            "expected the write failure, received {error}"
        );

        // Closing fails everything still pending. A call whose write failed is not pending — it
        // already failed — so there is nothing here for a second failure to land on.
        client.close().await.ok();
    }

    #[tokio::test]
    async fn a_peer_that_exits_fails_every_call_still_waiting() {
        let link = ScriptedLink::new();
        let client = Arc::new(client(link.clone(), RecordingHandler::arc(None)));

        let waiting = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<_, Value>("thread/start", json!({})).await })
        };
        link.wait_for_sent(1).await;
        link.end();

        let error = waiting
            .await
            .expect("expected the task to finish")
            .expect_err("expected a failure, received an answer");
        assert!(
            matches!(&error, Error::Vendor(vendor) if vendor.message.contains("exited")),
            "expected the raw vendor failure to retain the exit detail, received {error:?}"
        );
        assert!(
            error.to_string().contains("vendor failure"),
            "expected a safe vendor diagnostic, received {error}"
        );
        assert!(client.is_closed());
    }

    #[tokio::test]
    async fn a_peer_exit_reaches_the_handler_when_no_call_is_waiting() {
        let link = ScriptedLink::new();
        let handler = RecordingHandler::arc(None);
        let client = client(link.clone(), Arc::clone(&handler));

        link.end();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !handler.terminations.lock().await.is_empty() {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected the pump to report the exit");

        assert_eq!(
            *handler.terminations.lock().await,
            vec![PeerTermination::Exited]
        );
        assert!(client.is_closed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_call_that_is_never_answered_ends_at_its_deadline() {
        let link = ScriptedLink::new();
        let client = client(link.clone(), RecordingHandler::arc(None));

        let error = client
            .request_with_timeout::<_, Value>("thread/start", json!({}), Duration::from_secs(30))
            .await
            .expect_err("expected a timeout, received an answer");
        assert!(
            matches!(error, Error::Timeout { .. }),
            "expected a timeout, received {error:?}"
        );
    }

    /// Client labels identify a peer to the caller but can be host-authored text, so they must not
    /// cross a timeout diagnostic alongside the exact method sent on the wire.
    #[tokio::test(start_paused = true)]
    async fn caller_defined_peer_and_method_names_stay_out_of_timeout_diagnostics() {
        let link = ScriptedLink::new();
        let client = Client::connect(
            link.into_link(),
            RecordingHandler::arc(None),
            ClientOptions::new("peer credential=peer-secret"),
        );

        let error = client
            .request_with_timeout::<_, Value>(
                "method credential=method-secret",
                json!({}),
                Duration::from_secs(30),
            )
            .await
            .expect_err("expected a timeout, received an answer");

        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("peer-secret") && !rendered.contains("method-secret"),
                "expected no caller-defined labels in diagnostics, received {rendered:?}"
            );
        }
    }

    /// A peer label is host-authored text, so the error code minted when the peer answers with an
    /// error frame must come from the trusted prefix rather than from that label.
    #[tokio::test]
    async fn a_caller_defined_peer_label_does_not_become_an_error_code() {
        let link = ScriptedLink::new();
        link.push_line(r#"{"jsonrpc":"2.0","id":"1","error":{"code":-32603,"message":"refused"}}"#);
        let client = Client::connect(
            link.into_link(),
            RecordingHandler::arc(None),
            ClientOptions::new("tenant-secret"),
        );

        let error = client
            .request::<_, Value>("ping", json!({}))
            .await
            .expect_err("expected the error frame to fail the call");

        let Error::Vendor(vendor) = &error else {
            panic!("expected a vendor error, received {error:?}");
        };
        assert_eq!(vendor.code.as_str(), "peer-call-failed");
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("tenant-secret"),
                "expected no caller-defined label in diagnostics, received {rendered:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_unparseable_line_is_dropped_rather_than_killing_the_turn() {
        let link = ScriptedLink::new();
        link.push_line("Warning: your terminal does not support colour");
        link.push_line("[]");
        link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"still here"}"#);
        let client = client(link.clone(), RecordingHandler::arc(None));

        assert_eq!(
            client
                .request::<_, Value>("ping", json!({}))
                .await
                .expect("expected an answer"),
            json!("still here")
        );
    }

    #[tokio::test]
    async fn a_dialect_that_writes_no_version_header_does_not_get_one() {
        let link = ScriptedLink::new();
        let client = Client::connect(
            link.clone().into_link(),
            RecordingHandler::arc(None),
            ClientOptions::new("Codex app-server").without_version_header(),
        );

        client
            .notify("turn/interrupt", json!({ "turnId": "t-1" }))
            .await
            .expect("expected the notification to be sent");

        let sent = link.sent();
        assert_eq!(sent.len(), 1);
        assert!(
            !sent[0].contains("jsonrpc"),
            "expected no version header, received {}",
            sent[0]
        );
    }

    #[tokio::test]
    async fn an_absent_parameter_is_an_absent_member_rather_than_a_null() {
        let link = ScriptedLink::new();
        let client = client(link.clone(), RecordingHandler::arc(None));

        client
            .notify("account/rateLimits/read", ())
            .await
            .expect("expected the notification to be sent");

        assert!(
            !link.sent()[0].contains("params"),
            "expected no params member, received {}",
            link.sent()[0]
        );
    }

    #[tokio::test]
    async fn a_handler_can_refuse_a_question_rather_than_leaving_the_peer_blocked() {
        let link = ScriptedLink::new();
        link.push_line(r#"{"jsonrpc":"2.0","id":7,"method":"fs/write_text_file"}"#);
        let client = client(
            link.clone(),
            RecordingHandler::arc(Some(ServerRequestOutcome::Failure(JsonRpcError {
                code: -32601,
                message: String::from("this host executes no vendor tool call"),
                data: None,
            }))),
        );

        link.wait_for_sent(1).await;
        let reply: Value =
            serde_json::from_str(&link.sent()[0]).expect("expected the reply to be a frame");
        assert_eq!(reply["id"], json!(7));
        assert_eq!(reply["error"]["code"], json!(-32601));

        client.close().await.expect("expected a clean close");
    }

    #[tokio::test]
    async fn a_closed_client_refuses_a_call_and_drops_a_notification() {
        let link = ScriptedLink::new();
        let client = client(link.clone(), RecordingHandler::arc(None));
        client.close().await.expect("expected a clean close");

        let error = client
            .request::<_, Value>("thread/start", json!({}))
            .await
            .expect_err("expected a refusal, received an answer");
        assert!(
            matches!(error, Error::Closed { subject: "link" }),
            "expected a closed link, received {error:?}"
        );
        client
            .notify("ping", json!({}))
            .await
            .expect("expected a dropped notification to be fine");
        assert!(link.sent().is_empty());
    }

    /// A host that stopped reading its turn stream. A bounded channel with no room left parks the
    /// handler exactly like this, for as long as the host takes to read again.
    ///
    /// Its questions park too, which is what an approval waiting for a person looks like. The flag
    /// records whether the future answering one was ever let go of.
    struct StalledHandler {
        entered: Notify,
        asked: Notify,
        released: Arc<AtomicBool>,
    }

    impl StalledHandler {
        fn arc() -> Arc<Self> {
            Arc::new(Self {
                entered: Notify::new(),
                asked: Notify::new(),
                released: Arc::new(AtomicBool::new(false)),
            })
        }
    }

    /// Sets its flag when the future holding it is dropped or finishes.
    struct ReleaseOnDrop(Arc<AtomicBool>);

    impl Drop for ReleaseOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[async_trait::async_trait]
    impl PeerHandler for StalledHandler {
        async fn on_notification(&self, _method: String, _params: Value) {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }

        async fn on_request(
            &self,
            _method: String,
            _params: Value,
            _id: RequestId,
        ) -> ServerRequestOutcome {
            let _release = ReleaseOnDrop(Arc::clone(&self.released));
            self.asked.notify_one();
            std::future::pending::<()>().await;
            unreachable!("the handler never answers")
        }
    }

    #[tokio::test]
    async fn closing_returns_while_the_handler_is_still_parked() {
        let link = ScriptedLink::new();
        let handler = StalledHandler::arc();
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server"),
        );

        link.push_line(r#"{"jsonrpc":"2.0","method":"item/started","params":{}}"#);
        handler.entered.notified().await;

        // The shutdown flag is read between messages only, so a pump parked in the handler never
        // reaches it: waiting the pump out here would wait for a message that is not coming.
        tokio::time::timeout(Duration::from_secs(5), client.close())
            .await
            .expect("expected close to return while the handler was parked")
            .expect("expected a clean close");
    }

    #[tokio::test]
    async fn a_full_notification_queue_fails_closed_without_parking_the_response_pump() {
        let link = ScriptedLink::new();
        let handler = StalledHandler::arc();
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server").with_max_pending_notifications(1),
        );

        // The worker owns the first notification and then parks in the handler. The second fits
        // the handoff queue; the third must fail the connection instead of blocking the one task
        // that still has to read responses.
        link.push_line(r#"{"jsonrpc":"2.0","method":"item/started","params":{}}"#);
        handler.entered.notified().await;
        link.push_line(r#"{"jsonrpc":"2.0","method":"item/updated","params":{}}"#);
        link.push_line(r#"{"jsonrpc":"2.0","method":"item/completed","params":{}}"#);

        tokio::time::timeout(Duration::from_secs(1), async {
            while !client.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected bounded notification backpressure to close the client");
    }

    /// Peer requests are ordered behind earlier notifications, even though their answers run on
    /// independent tasks once they reach the head of that queue.
    #[tokio::test]
    async fn a_peer_request_cannot_overtake_a_stalled_notification() {
        let link = ScriptedLink::new();
        let handler = StalledHandler::arc();
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server").with_max_pending_notifications(2),
        );

        link.push_line(r#"{"jsonrpc":"2.0","method":"item/started","params":{}}"#);
        handler.entered.notified().await;
        link.push_line(
            r#"{"jsonrpc":"2.0","id":"approval-1","method":"session/request_permission"}"#,
        );

        assert!(
            tokio::time::timeout(Duration::from_millis(50), handler.asked.notified())
                .await
                .is_err(),
            "expected the request to wait behind the stalled notification"
        );

        client.close().await.expect("expected a clean close");
    }

    /// A question the peer asked is answered on a task of its own. Closing has to take that task
    /// with it, or an approval nobody answered outlives the session that raised it — holding its
    /// share of the client for the rest of the process.
    #[tokio::test]
    async fn closing_takes_down_a_question_nobody_answered() {
        let link = ScriptedLink::new();
        let handler = StalledHandler::arc();
        let released = Arc::clone(&handler.released);
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server"),
        );

        link.push_line(r#"{"jsonrpc":"2.0","id":"1","method":"session/request_permission"}"#);
        handler.asked.notified().await;
        assert!(
            !released.load(Ordering::Acquire),
            "expected the question to still be waiting"
        );

        client.close().await.expect("expected a clean close");
        assert!(
            released.load(Ordering::Acquire),
            "expected the answer task to be let go of by the time the client closed"
        );

        // And the peer is told, rather than left waiting on a link that is already gone.
        let sent = link.sent();
        assert_eq!(sent.len(), 1, "received {sent:?}");
        let refusal: Value =
            serde_json::from_str(&sent[0]).expect("expected the refusal to be a frame");
        assert_eq!(refusal["id"], json!("1"), "received {refusal}");
        assert_eq!(refusal["error"]["code"], json!(-32000));
    }

    #[tokio::test]
    async fn peer_exit_releases_unanswered_questions_without_explicit_close() {
        let link = ScriptedLink::new();
        let handler = StalledHandler::arc();
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server"),
        );
        link.push_line(r#"{"jsonrpc":"2.0","id":"1","method":"session/request_permission"}"#);
        handler.asked.notified().await;
        link.end();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !handler.released.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected peer EOF to release the unanswered handler without explicit close");
        client.close().await.expect("expected idempotent close");
    }

    /// The line cap bounds how big one frame is, never how many arrive. Without a count, a peer
    /// writing questions as fast as the pipe allows spawns one task per frame.
    #[tokio::test]
    async fn a_peer_asking_faster_than_anyone_answers_is_refused_rather_than_spawned() {
        let link = ScriptedLink::new();
        let handler = StalledHandler::arc();
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server").with_max_in_flight_requests(1),
        );

        link.push_line(r#"{"jsonrpc":"2.0","id":"1","method":"session/request_permission"}"#);
        handler.asked.notified().await;
        link.push_line(r#"{"jsonrpc":"2.0","id":"2","method":"session/request_permission"}"#);
        // Bounded: without the cap nothing is ever written, and a test that waits forever reports
        // nothing at all.
        tokio::time::timeout(Duration::from_secs(5), link.wait_for_sent(1))
            .await
            .expect("expected the question past the cap to be refused");

        // The parked question wrote nothing; the one past the cap was refused by name.
        let sent = link.sent();
        assert_eq!(sent.len(), 1, "received {sent:?}");
        let refusal: Value =
            serde_json::from_str(&sent[0]).expect("expected the refusal to be a frame");
        assert_eq!(refusal["id"], json!("2"), "received {refusal}");
        assert_eq!(refusal["error"]["code"], json!(-32000));
        assert!(
            refusal["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("at most 1")),
            "expected the cap to be named, received {refusal}"
        );

        client.close().await.expect("expected a clean close");
    }

    /// A request future can be dropped somewhere no runtime is entered.
    ///
    /// A host that holds an in-flight `Client::request` in a struct, or that returns from
    /// `block_on` still owning one, drops it on a plain thread. `PendingCall::drop` has to clean
    /// its correlation entry from there without `tokio::spawn`, which panics off a runtime — and a
    /// panic raised inside `Drop` during an unwind aborts the process.
    #[tokio::test]
    async fn dropping_a_request_off_a_runtime_cleans_up_without_panicking() {
        let link = ScriptedLink::new();
        let client = Client::connect(
            link.clone().into_link(),
            RecordingHandler::arc(None),
            ClientOptions::default(),
        );
        let mut request = Box::pin(client.request::<_, Value>("test/held", json!({})));
        std::future::poll_fn(|cx| {
            assert!(
                request.as_mut().poll(cx).is_pending(),
                "expected the submitted request to wait for its peer"
            );
            std::task::Poll::Ready(())
        })
        .await;
        link.wait_for_sent(1).await;
        let state = Arc::clone(&client.state);
        // Held for the whole drop, so the guard's `try_lock` fails and it has to take the
        // deferred path. Without this the fast path succeeds and the test proves nothing.
        let held = state.pending.lock().await;
        assert_eq!(held.len(), 1, "expected one submitted correlation entry");
        std::thread::scope(|scope| {
            scope
                .spawn(move || drop(request))
                .join()
                .expect("expected dropping a request off a runtime not to panic");
        });
        drop(held);
        let cleaned = tokio::time::timeout(Duration::from_secs(1), async {
            while !state.pending.lock().await.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            cleaned.is_ok(),
            "expected abandoned request correlation to be removed without another call or close; received {} pending entries",
            state.pending.lock().await.len()
        );
    }
}
