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

mod outbox;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex as StdMutex, OnceLock, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};

use crate::error::{Error, ErrorCode, Result, VendorError, jsonrpc_code_is_retryable};
use crate::host::{CancelToken, Limits};
use crate::link::{Link, LinkSender};
use crate::operation::Dispatch;

pub use outbox::{WireOptions, Written};

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

/// Why the connection ended without this client closing it first.
///
/// More reasons may be added, and the reasons that carry numbers may carry more of them, so a
/// `match` on this needs a wildcard arm and a pattern on one of those needs `..`.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PeerTermination {
    /// The peer closed its output.
    Exited,
    /// The link failed: reading or writing the peer failed, a reply to one of its questions could
    /// not be written, or a write was abandoned mid-send (timed out or dropped) with its frame
    /// possibly half on the wire.
    LinkFailed(String),
    /// Peer work filled the bounded handoff queue before the handler could consume it.
    #[non_exhaustive]
    NotificationByteBackpressure {
        /// The encoded byte budget for queued and in-flight peer callbacks.
        limit: usize,
        /// The encoded bytes of the frame the budget had no room left for.
        received: usize,
    },
    /// The event-count handoff budget was exhausted.
    #[non_exhaustive]
    NotificationBackpressure {
        /// The number of peer messages the client can retain while the handler is busy.
        limit: usize,
        /// How many there were with the message that did not fit.
        received: usize,
    },
    /// This side queued more for the peer than [`WireOptions`] allows it to hold, which is a peer
    /// that has stopped reading.
    #[non_exhaustive]
    OutboundBackpressure {
        /// What was counted, unit included, as in [`Error::LimitExceeded`].
        subject: &'static str,
        /// The bound that was passed.
        limit: usize,
        /// What the queue would have held with the frame that did not fit.
        received: usize,
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
            Self::NotificationByteBackpressure { limit, received } => formatter
                .debug_struct("NotificationByteBackpressure")
                .field("limit", limit)
                .field("received", received)
                .finish(),
            Self::NotificationBackpressure { limit, received } => formatter
                .debug_struct("NotificationBackpressure")
                .field("limit", limit)
                .field("received", received)
                .finish(),
            Self::OutboundBackpressure {
                subject,
                limit,
                received,
            } => formatter
                .debug_struct("OutboundBackpressure")
                .field("subject", subject)
                .field("limit", limit)
                .field("received", received)
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
            Self::NotificationByteBackpressure { limit, .. } => write!(
                formatter,
                "the peer exceeded the queued callback payload budget ({limit} bytes)"
            ),
            Self::NotificationBackpressure { limit, .. } => write!(
                formatter,
                "the peer sent more messages than the client could retain while its handler was busy (limit {limit})"
            ),
            Self::OutboundBackpressure {
                subject,
                limit,
                received,
            } => write!(
                formatter,
                "the peer stopped reading: expected at most {limit} {subject}, received {received}"
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
    /// Dispatched in arrival order and awaited on a task apart from the reader, which keeps
    /// reading and hands each frame over through a bounded queue. A handler that is slow to return
    /// therefore does not slow the peer down: later frames wait in that queue, and when its
    /// message or byte budget is spent the connection ends with
    /// [`PeerTermination::NotificationBackpressure`] or
    /// [`PeerTermination::NotificationByteBackpressure`].
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

    /// The peer's link ended unexpectedly, on its read or write side.
    ///
    /// A handler can release state that only a complete vendor turn would otherwise clear. This
    /// callback is never made for [`Client::close`], whose caller already owns that shutdown.
    async fn on_terminated(&self, _termination: PeerTermination) {}

    /// Whether a call to [`on_notification`](Self::on_notification) that is in progress when
    /// the connection is cut short is let finish. `false` unless a handler says otherwise.
    ///
    /// Two ends of a connection stop the handler's task without draining its queue: the peer
    /// passing a budget, and [`Client::close`]. By default the call in progress is cancelled
    /// where it stands, at whichever of its awaits it had reached, which is safe only for a
    /// handler that keeps nothing half-done across an await. A handler that does (one that has
    /// claimed something it releases further down) returns `true` here. The call in progress
    /// then gets up to [`ClientOptions::shutdown_timeout`] to return, and is cancelled only
    /// after that; a host that stopped reading cannot hold the connection's end for longer.
    ///
    /// Either way nothing that is still queued once the end has been noticed is handed to the
    /// handler, and [`on_terminated`](Self::on_terminated) comes after the handler's task has
    /// stopped. A close waits for the call while it waits for the answers this side still owes
    /// the peer, so it takes no longer than [`Client::close`] documents. Dropping the
    /// [`Client`] cannot wait and cancels the call in every case.
    ///
    /// Read once, when the client connects.
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::{PeerHandler, RequestId, ServerRequestOutcome};
    /// use serde_json::Value;
    ///
    /// struct Careful;
    ///
    /// #[async_trait::async_trait]
    /// impl PeerHandler for Careful {
    ///     async fn on_notification(&self, _method: String, _params: Value) {}
    ///
    ///     async fn on_request(&self, _: String, _: Value, _: RequestId) -> ServerRequestOutcome {
    ///         ServerRequestOutcome::Answer(Value::Null)
    ///     }
    ///
    ///     fn finishes_notification_in_progress(&self) -> bool {
    ///         true
    ///     }
    /// }
    ///
    /// assert!(Careful.finishes_notification_in_progress());
    /// ```
    fn finishes_notification_in_progress(&self) -> bool {
        false
    }
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
    /// Maximum outbound RPCs whose response has not been read.
    ///
    /// Also, counted on its own, how many answers requested with
    /// [`RequestOptions::after_earlier_notifications`] may be awaiting delivery, read or not: that
    /// many places are reserved for them in the handoff queue, apart from
    /// [`max_pending_notifications`](ClientOptions::max_pending_notifications). Such an answer
    /// that has been read and waits in that queue is counted by its place alone.
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

/// How one request differs from the connection's defaults.
///
/// Built with its methods and handed to [`Client::request_with`]. A default value asks for exactly
/// what [`Client::request`] does, so a call site names only what it changes.
///
/// # Example
///
/// ```
/// use mango_external_agents::jsonrpc::{RequestDeadline, RequestOptions};
/// use std::time::Duration;
///
/// let options = RequestOptions::new()
///     .with_timeout(Duration::from_secs(30))
///     .after_earlier_notifications();
/// assert_eq!(options.deadline(), RequestDeadline::After(Duration::from_secs(30)));
/// assert!(options.waits_for_earlier_notifications());
/// ```
///
/// Deliberately neither `Clone` nor comparable: an option added later may own something that is
/// neither, and a call site builds these where it uses them.
#[derive(Default)]
pub struct RequestOptions {
    deadline: RequestDeadline,
    after_earlier_notifications: bool,
    outside_pending_budget: bool,
    label: Option<&'static str>,
}

impl std::fmt::Debug for RequestOptions {
    /// Without the request's name, which is host-written text.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequestOptions")
            .field("deadline", &self.deadline)
            .field(
                "after_earlier_notifications",
                &self.after_earlier_notifications,
            )
            .field("outside_pending_budget", &self.outside_pending_budget)
            .field("labelled", &self.label.is_some())
            .finish()
    }
}

/// How long a request waits for its answer.
///
/// # Example
///
/// ```
/// use mango_external_agents::jsonrpc::{RequestDeadline, RequestOptions};
///
/// assert_eq!(RequestOptions::new().deadline(), RequestDeadline::Connection);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum RequestDeadline {
    /// [`ClientOptions::request_timeout`], the connection's own.
    #[default]
    Connection,
    /// This long, whatever the connection's own is.
    After(Duration),
    /// No deadline: the answer is waited for until it comes, the connection ends, or the
    /// caller gives the request up.
    Unbounded,
}

impl RequestOptions {
    /// The connection's defaults: its request timeout, and an answer delivered as soon as it is
    /// read.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::{RequestDeadline, RequestOptions};
    ///
    /// let options = RequestOptions::new();
    /// assert_eq!(options.deadline(), RequestDeadline::Connection);
    /// assert!(!options.waits_for_earlier_notifications());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Waits this long for the answer instead of [`ClientOptions::request_timeout`].
    ///
    /// The write of the request frame keeps the connection's own bound.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::{RequestDeadline, RequestOptions};
    /// use std::time::Duration;
    ///
    /// let options = RequestOptions::new().with_timeout(Duration::from_secs(5));
    /// assert_eq!(options.deadline(), RequestDeadline::After(Duration::from_secs(5)));
    /// ```
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.deadline = RequestDeadline::After(timeout);
        self
    }

    /// Waits for the answer for as long as the connection lasts.
    ///
    /// For a request whose answer is the end of something a person or a model is doing, such as
    /// an ACP `session/prompt`, which no fixed time bounds. The wait ends with the answer, with
    /// the connection, or when the caller gives the request up by dropping it.
    ///
    /// Only the wait for the answer loses its bound. The write of the request frame keeps
    /// [`ClientOptions::request_timeout`], so a peer that stopped reading still fails the call,
    /// and the write it left unfinished ends the connection.
    ///
    /// Such a request still holds a place among [`ClientOptions::max_pending_requests`] for as
    /// long as it waits, unless it also asks for
    /// [`outside_pending_budget`](Self::outside_pending_budget).
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::{RequestDeadline, RequestOptions};
    ///
    /// let options = RequestOptions::new().without_deadline();
    /// assert_eq!(options.deadline(), RequestDeadline::Unbounded);
    /// ```
    #[must_use]
    pub fn without_deadline(mut self) -> Self {
        self.deadline = RequestDeadline::Unbounded;
        self
    }

    /// Does not count this request against [`ClientOptions::max_pending_requests`].
    ///
    /// That budget exists so a peer that stops answering cannot make this side hold calls
    /// without limit. A request the host already bounds some other way (one running turn per
    /// session, say) can stand outside it, so that short requests are not refused while it
    /// runs and it is not refused because of them. It is neither counted nor checked: it is
    /// admitted with the budget full, and requests that do count are admitted as if it were not
    /// there. The host answers for how many of these it makes: nothing in this client bounds
    /// the ones whose answers are delivered as they are read, beyond what [`WireOptions`]
    /// bounds of their frames.
    ///
    /// The places [`after_earlier_notifications`](Self::after_earlier_notifications) reserves
    /// are a budget of their own and still apply: an answer that waits its turn needs a place
    /// to wait in, so at most [`ClientOptions::max_pending_requests`] such requests may be
    /// awaiting delivery at once, inside this budget or outside it.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::RequestOptions;
    ///
    /// let options = RequestOptions::new().outside_pending_budget();
    /// assert!(!options.counts_against_pending_budget());
    /// ```
    #[must_use]
    pub fn outside_pending_budget(mut self) -> Self {
        self.outside_pending_budget = true;
        self
    }

    /// Names this request, for whoever handles its failure.
    ///
    /// The name comes back from [`CallFailure::label`], so code that receives a failed
    /// [`Reply`] can tell which of its requests it was without keeping a table of ids. It is a
    /// `'static` string the host wrote, never the method sent on the wire, and it is not put
    /// into any error's text, nor into the `Debug` form of the failure or of these options.
    ///
    /// Only a request queued with [`Client::submit_request`] has a [`CallFailure`] to carry it.
    /// [`Client::request_with`] returns a plain [`Error`] and the name goes unused there: a
    /// caller that wants the name, or the typed cause, queues the request and awaits its
    /// [`Reply`].
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::RequestOptions;
    ///
    /// let options = RequestOptions::new().labelled("prompt");
    /// assert_eq!(options.label(), Some("prompt"));
    /// ```
    #[must_use]
    pub fn labelled(mut self, label: &'static str) -> Self {
        self.label = Some(label);
        self
    }

    /// Delivers the answer only after [`PeerHandler::on_notification`] has returned for every
    /// notification the peer sent before it.
    ///
    /// By default a response overtakes: the reader hands it to its caller at once, while
    /// notifications read earlier may still be waiting for the handler. A caller that treats the
    /// answer as "everything before this has been seen" (an ACP `session/prompt` result closes a
    /// turn whose `session/update` notifications came first) asks for this instead. The answer
    /// then takes its place in the same queue the notifications wait in and reaches the caller
    /// when the handler's task gets to it.
    ///
    /// What it waits for is the handler returning from each earlier notification. A question the
    /// peer asked earlier ([`PeerHandler::on_request`]) only has to have been started: it is
    /// answered on a task of its own and does not hold the answer back.
    ///
    /// The same order holds when the connection ends. A response that was read before the peer
    /// exited or the link failed still reaches its caller once the notifications ahead of it have
    /// been handled, and a request the peer never answered fails only after that drain, which
    /// [`ClientOptions::shutdown_timeout`] bounds. [`Client::close`] and a queue overflow do not
    /// drain. They fail the call at once, or, when its answer was already queued behind a
    /// notification call that [`PeerHandler::finishes_notification_in_progress`] lets finish,
    /// when that call returns.
    ///
    /// A request [`without_deadline`](Self::without_deadline) waits in that queue for as long
    /// as the handler takes: an `on_notification` that never returns holds its answer back
    /// until the connection ends or the caller gives the request up.
    ///
    /// The request's deadline keeps running while its answer waits in that queue, so a handler
    /// that is slow to return can time the request out with the answer already read. A
    /// request that does so keeps its place in the queue, counted below, until the handler's
    /// task has passed the abandoned answer; one that times out before its answer is read gives
    /// the place back at once.
    ///
    /// At most [`ClientOptions::max_pending_requests`] of these may be awaiting delivery at once,
    /// counted apart from the notification budgets: a burst of notifications cannot refuse or
    /// drop such an answer, and a peer cannot grow the queue with answers nobody asked for. A
    /// queued answer is not charged to [`ClientOptions::max_pending_bytes`] either; what bounds
    /// its size is the line cap of the transport underneath, once per reserved place.
    ///
    /// # Deadlock hazard
    ///
    /// Never await such a request from inside `on_notification` on the same client. The answer
    /// waits for that very call to return, so nothing could ever deliver it. The client refuses
    /// the request with [`Error::HostConfiguration`], before writing anything, instead of letting
    /// it run to its deadline. It can only see a request made on the handler's own task: one made
    /// on a task the handler spawned and then awaits is the same deadlock and runs until the
    /// request times out. A request made from [`PeerHandler::on_request`] is safe, since nothing
    /// in the queue waits for a question to be answered, and so is one made to another client.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::RequestOptions;
    ///
    /// let options = RequestOptions::new().after_earlier_notifications();
    /// assert!(options.waits_for_earlier_notifications());
    /// ```
    #[must_use]
    pub fn after_earlier_notifications(mut self) -> Self {
        self.after_earlier_notifications = true;
        self
    }

    /// The deadline this request asked for.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::{RequestDeadline, RequestOptions};
    ///
    /// assert_eq!(RequestOptions::new().deadline(), RequestDeadline::Connection);
    /// ```
    #[must_use]
    pub fn deadline(&self) -> RequestDeadline {
        self.deadline
    }

    /// Whether the answer waits for the notifications that arrived before it.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::RequestOptions;
    ///
    /// assert!(!RequestOptions::new().waits_for_earlier_notifications());
    /// ```
    #[must_use]
    pub fn waits_for_earlier_notifications(&self) -> bool {
        self.after_earlier_notifications
    }

    /// Whether this request holds a place among [`ClientOptions::max_pending_requests`].
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::RequestOptions;
    ///
    /// assert!(RequestOptions::new().counts_against_pending_budget());
    /// ```
    #[must_use]
    pub fn counts_against_pending_budget(&self) -> bool {
        !self.outside_pending_budget
    }

    /// The name this request was given with [`labelled`](Self::labelled).
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::RequestOptions;
    ///
    /// assert_eq!(RequestOptions::new().label(), None);
    /// ```
    #[must_use]
    pub fn label(&self) -> Option<&'static str> {
        self.label
    }
}

tokio::task_local! {
    /// The client whose `on_notification` the current task is inside, as its state's address.
    ///
    /// Scoped around that one call, so it says nothing once the handler returned or panicked,
    /// and nothing on any other task. A recorded task id would do neither: it outlives a worker
    /// that panicked, and tokio may hand a finished task's id to a new one.
    static HANDLING_NOTIFICATION_FOR: usize;
}

/// A JSON-RPC client that also answers.
pub struct Client {
    state: Arc<ClientState>,
    pump: Mutex<Option<JoinHandle<()>>>,
}

/// How a connection came to its end.
///
/// What [`Client::ended`] returns and what a failed [`Reply`] carries, so a caller can tell a
/// peer that exited from a link that failed from a budget that was passed, without reading text.
/// More ends may be added, so a `match` needs a wildcard arm.
///
/// # Example
///
/// ```
/// use mango_external_agents::jsonrpc::{ConnectionEnd, PeerTermination};
///
/// fn peer_exited(end: &ConnectionEnd) -> bool {
///     matches!(end, ConnectionEnd::Peer(PeerTermination::Exited))
/// }
/// assert!(!peer_exited(&ConnectionEnd::Closed));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConnectionEnd {
    /// This side ended it: [`Client::close`] was called, or the client was dropped.
    Closed,
    /// It ended under this side, for the reason the handler's
    /// [`on_terminated`](PeerHandler::on_terminated) is given: the peer exited, the link failed,
    /// or a budget in either direction was passed.
    Peer(PeerTermination),
}

/// Why a call queued with [`Client::submit_request`] has no answer.
///
/// [`cause`](Self::cause) says which side failed it and how, as a type.
/// [`into_error`](Self::into_error) is the failure as an [`Error`], for a caller that only
/// reports it.
///
/// ```no_run
/// # async fn example(reply: mango_external_agents::jsonrpc::Reply) {
/// use mango_external_agents::jsonrpc::{CallFailureCause, ConnectionEnd, PeerTermination};
///
/// if let Err(failure) = reply.await {
///     match failure.cause() {
///         CallFailureCause::Peer(error) => eprintln!("the peer refused with {}", error.code),
///         CallFailureCause::Ended(ConnectionEnd::Peer(PeerTermination::Exited)) => {
///             eprintln!("the peer exited")
///         }
///         _ => eprintln!("{}", failure.into_error()),
///     }
/// }
/// # }
/// ```
pub struct CallFailure {
    // Boxed so that a reply's `Result` stays the size of its answer.
    parts: Box<(CallFailureCause, Error)>,
    label: Option<&'static str>,
}

/// Which side failed a call, and how.
///
/// More causes may be added, so a `match` on this needs a wildcard arm, and a pattern on
/// [`TimedOut`](Self::TimedOut) needs `..`. What the other causes carry is fixed; anything
/// more a failure comes to know is read from [`CallFailure`] itself. It is not comparable:
/// match on it.
///
/// ```
/// use mango_external_agents::jsonrpc::{CallFailureCause, ConnectionEnd};
///
/// fn worth_a_new_connection(cause: &CallFailureCause) -> bool {
///     matches!(cause, CallFailureCause::Ended(ConnectionEnd::Peer(_)))
/// }
/// assert!(!worth_a_new_connection(&CallFailureCause::Unwritten));
/// ```
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
#[non_exhaustive]
pub enum CallFailureCause {
    /// The peer answered with an error frame. Its code, message and data are here as it sent
    /// them; nothing this side says about itself ever appears in this variant.
    Peer(JsonRpcError),
    /// The connection ended before an answer came. [`Client::ended`] returns the same reason.
    Ended(ConnectionEnd),
    /// The task that runs [`PeerHandler::on_notification`] stopped (a handler panicked), so an
    /// answer that had to wait its turn behind earlier notifications could not be delivered.
    /// Says nothing about the connection.
    HandlerStopped,
    /// No answer came within the deadline the request asked for.
    #[non_exhaustive]
    TimedOut {
        /// The deadline that passed, counted from when the request was queued.
        after: Duration,
    },
    /// The request's frame did not reach the wire whole, so no answer could come. Its
    /// [`Written`] resolves to the reason. A link that failed under the write ends the
    /// connection next.
    Unwritten,
}

impl std::fmt::Debug for CallFailure {
    /// Without the request's name: that is host-written text, and this is an error.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CallFailure")
            .field("cause", &self.parts.0)
            .field("error", &self.parts.1)
            .field("labelled", &self.label.is_some())
            .finish()
    }
}

impl CallFailure {
    /// Which side failed the call, and how.
    ///
    /// ```no_run
    /// # fn example(failure: &mango_external_agents::jsonrpc::CallFailure) {
    /// use mango_external_agents::jsonrpc::CallFailureCause;
    ///
    /// let answered_by_the_peer = matches!(failure.cause(), CallFailureCause::Peer(_));
    /// # let _ = answered_by_the_peer;
    /// # }
    /// ```
    #[must_use]
    pub fn cause(&self) -> &CallFailureCause {
        &self.parts.0
    }

    /// This failure as an [`Error`].
    ///
    /// For the peer's own error, an ended connection, a stopped handler and a deadline it is
    /// the error [`Client::request`] returns for the same failure, text and codes included. For
    /// an unwritten frame it only says so; the write's own error is what [`Written`] resolves
    /// to.
    ///
    /// ```no_run
    /// # fn example(failure: mango_external_agents::jsonrpc::CallFailure) -> mango_external_agents::Error {
    /// failure.into_error()
    /// # }
    /// ```
    #[must_use]
    pub fn into_error(self) -> Error {
        self.parts.1
    }

    /// The name the request was given with [`RequestOptions::labelled`], if it was given one.
    ///
    /// ```no_run
    /// # fn example(failure: &mango_external_agents::jsonrpc::CallFailure) {
    /// if failure.label() == Some("prompt") {
    ///     eprintln!("the prompt failed: {failure}");
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn label(&self) -> Option<&'static str> {
        self.label
    }

    fn new(cause: CallFailureCause, error: Error) -> Self {
        Self {
            parts: Box::new((cause, error)),
            label: None,
        }
    }

    fn labelled(mut self, label: Option<&'static str>) -> Self {
        self.label = label;
        self
    }
}

impl std::fmt::Display for CallFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.parts.1.fmt(formatter)
    }
}

impl std::error::Error for CallFailure {}

impl From<CallFailure> for Error {
    fn from(failure: CallFailure) -> Self {
        failure.parts.1
    }
}

/// What a call that has no answer is settled with: the error body its caller has always been
/// given, and which side it came from.
#[derive(Clone, Debug)]
struct Failed {
    body: JsonRpcError,
    cause: FailedCause,
}

impl Failed {
    /// The reason the handler is told for the end this records: the one on record when the
    /// connection ended under this side, so the handler, [`Client::ended`] and every failed
    /// reply name the same one however the causes raced. Otherwise `found`, what the pump saw.
    fn termination(&self, found: PeerTermination) -> PeerTermination {
        match &self.cause {
            FailedCause::Ended(ConnectionEnd::Peer(recorded)) => recorded.clone(),
            _ => found,
        }
    }
}

#[derive(Clone, Debug)]
enum FailedCause {
    /// The peer's own error frame.
    Peer,
    /// The connection ended.
    Ended(ConnectionEnd),
    /// The notification handler's task stopped under a live connection.
    HandlerStopped,
    /// The request's frame was not written.
    Unwritten,
}

/// What a call is settled with: the peer's result, or why there is none.
type Answer = std::result::Result<Value, Failed>;

/// One call waiting for its answer.
struct PendingAnswer {
    answer: oneshot::Sender<Answer>,
    delivery: Delivery,
    /// Whether it holds a place among `max_pending_requests`.
    counted: bool,
}

/// Drops the entries nobody waits on any more and counts those that hold a place in the budget.
fn counted_pending(pending: &mut HashMap<String, PendingAnswer>) -> usize {
    let mut counted = 0;
    pending.retain(|_, waiting| {
        let live = !waiting.answer.is_closed();
        counted += usize::from(live && waiting.counted);
        live
    });
    counted
}

/// Who hands a response to its caller, and when.
enum Delivery {
    /// The reader, as soon as it has read the response.
    Immediate,
    /// The peer-work task, when it reaches the response in arrival order. The permit is this
    /// call's place in the queue's reserve; it moves into the queue with the response.
    Ordered(OwnedSemaphorePermit),
}

/// A response that was read and now waits in the handoff queue for its turn.
///
/// It owns the caller's end of the call from the moment it is queued, so the call cannot be left
/// waiting whatever becomes of the queue: delivered when the worker reaches it, and failed when
/// it is dropped instead, which is what an aborted worker, a drain that ran out, a handler that
/// panicked and a queue that was already gone all come to.
struct QueuedAnswer {
    state: Arc<ClientState>,
    answer: Option<oneshot::Sender<Answer>>,
    outcome: Option<Answer>,
    /// The place the call reserved, given back once the worker has passed this entry.
    _place: OwnedSemaphorePermit,
}

impl QueuedAnswer {
    /// Hands the response to its caller. One that gave up left nobody to hear it.
    fn deliver(mut self) {
        if let (Some(answer), Some(outcome)) = (self.answer.take(), self.outcome.take()) {
            let _ = answer.send(outcome);
        }
    }
}

impl Drop for QueuedAnswer {
    fn drop(&mut self) {
        // Out of the queue, whichever way it left.
        self.state.queued_answers.fetch_sub(1, Ordering::AcqRel);
        if let Some(answer) = self.answer.take() {
            let _ = answer.send(Err(self.state.undelivered_failure()));
        }
    }
}

/// A request queued without waiting, on its way to the calls awaiting an answer.
///
/// That map sits behind a lock a synchronous caller cannot wait for, so the entry travels with
/// the request's frame and the writer enters it just before the frame goes out.
struct Registration {
    id: String,
    answer: oneshot::Sender<Answer>,
    delivery: Delivery,
    counted: bool,
}

struct ClientState {
    /// The link's sending half. Held for one frame at a time, in the order the outbox decides,
    /// and by `close`.
    sender: Arc<Mutex<Box<dyn LinkSender>>>,
    pending: Mutex<HashMap<String, PendingAnswer>>,
    /// Where every outgoing frame, or its writer's turn, is queued. See [`outbox`].
    outbox: outbox::Outbox,
    /// Takes the outbox's task down with the client. Its join handle is not kept: nothing waits
    /// for it.
    writer_abort: OnceLock<tokio::task::AbortHandle>,
    /// The typed reason, when what ended the connection was this side's own outbound budget.
    outbound_overflow: StdMutex<Option<PeerTermination>>,
    /// Closes running now, and whether any close has got as far as the link. The last one to
    /// leave without that poisons the link.
    closes_in_progress: AtomicUsize,
    close_reached_link: AtomicBool,
    /// Runs once when the writer has taken a queued frame and before it sends, so a test can
    /// place a caller's withdrawal exactly there.
    #[cfg(test)]
    after_write_began: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once when a caller that writes for itself has been told how it gets the link and
    /// before it acts on that, so a test can queue a frame exactly there.
    #[cfg(test)]
    after_turn_decided: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once when the queue has been sealed and before the closed flag goes up, so a test
    /// can queue a frame exactly there.
    #[cfg(test)]
    after_seal: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once when a stop has found the handler's task outside a call and before it cancels
    /// the task, so a test can give the task time to begin one if it wrongly would.
    #[cfg(test)]
    after_no_call_found: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once when the handler's task has taken a notification and before it says it is in
    /// a call, so a test can have a stop land exactly there.
    #[cfg(test)]
    before_call_claimed: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The connection's runtime also owns cleanup when a request is dropped on another thread.
    runtime: tokio::runtime::Handle,
    options: ClientOptions,
    next_id: AtomicU64,
    closed: AtomicBool,
    shutdown: CancelToken,
    /// Why a write to the peer left the link unusable, once one did: a transport failure, a reply
    /// that could not be written, or any frame whose send was abandoned mid-frame (a timeout, a
    /// shorter caller deadline, or a dropped request).
    ///
    /// A reply is written on a task the pump waits for when it tears the connection down, so that
    /// task must never run the teardown itself. It records the cause here and wakes the pump, which
    /// owns termination.
    write_failure: StdMutex<Option<String>>,
    write_failed: Notify,
    /// What ended the connection, recorded before any waiting call is failed with it, so a
    /// response dropped from the handoff queue fails its caller with the same words.
    ended: StdMutex<Option<Failed>>,
    /// Set synchronously, before the sender is released, by a transport failure or abandoned
    /// mid-frame send, so a queued writer refuses instead of using the broken physical link.
    /// The pump ends the connection later, on its own task.
    link_poisoned: AtomicBool,
    /// The peer's questions currently being answered, so they can be counted and taken down.
    in_flight: StdMutex<JoinSet<()>>,
    notifications: StdMutex<Option<JoinHandle<()>>>,
    /// Takes the handler's task down whoever holds its join handle. A drain takes the handle out
    /// of `notifications` to wait on it, and a close or a drop that lands in that drain still
    /// has to stop the task, with whatever its queue holds.
    worker_abort: OnceLock<tokio::task::AbortHandle>,
    /// What the handler said of itself: a notification call in progress is let finish, within
    /// the shutdown grace, when the connection is cut short.
    finishes_notification: bool,
    /// Raised when the handler's task is to stop once the call it is in has returned.
    worker_stopping: AtomicBool,
    /// Cancelled when the handler's task has stopped, however it stopped.
    worker_stopped: CancelToken,
    /// Whether the handler's task is inside `on_notification`, or about to be. Raised before
    /// the task reads `worker_stopping` and read after that flag is raised, so one side always
    /// sees the other: a task found outside a call is not cancelled inside one it went on to
    /// begin.
    in_notification: AtomicBool,
    /// Runs once between the two reads the reader admits a notification on, so a test can place
    /// the worker's dequeue exactly there.
    #[cfg(test)]
    after_queue_length_read: StdMutex<Option<Box<dyn FnOnce() + Send>>>,
    peer_bytes: Arc<Semaphore>,
    /// How many notifications and peer questions may wait in the handoff queue. The channel is
    /// larger than this by the reserve below, so the reader counts what is in it.
    notification_capacity: usize,
    /// How many of the queue's entries are ordered responses, which the count above leaves out.
    /// Only an ordered request ever moves it, so a connection without one pays a read for it.
    queued_answers: AtomicUsize,
    /// Places in the handoff queue reserved for ordered responses. Taken when the call is
    /// admitted and released when the worker has passed the queued response, so neither a
    /// notification burst nor a caller that gave up can crowd one out.
    ordered_places: Arc<Semaphore>,
}

/// Reports a send that never finished.
///
/// A send dropped mid-frame, by its own write deadline, by a caller's shorter deadline or because
/// the caller dropped the future, may have put half a frame on the wire, and every later frame
/// would follow it. Whatever dropped it, this is the one place that notices, so the pump is
/// told. A completed transport failure is handled where the frame is written instead; local
/// refusals do not damage the link.
struct MidSend<'a> {
    state: &'a ClientState,
    finished: bool,
}

impl<'a> MidSend<'a> {
    fn arm(state: &'a ClientState) -> Self {
        Self {
            state,
            finished: false,
        }
    }

    fn finish(&mut self) {
        self.finished = true;
    }
}

impl Drop for MidSend<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Before the sender is released (this guard drops ahead of it): the next writer sees it.
        self.state.link_poisoned.store(true, Ordering::Release);
        self.state.signal_write_failure(&Error::Link {
            peer: self.state.options.peer_name.clone(),
            message: String::from("a JSON-RPC frame write was abandoned mid-send"),
        });
    }
}

/// Leaves the link unusable when the last close in progress ends without any having reached it.
///
/// Whether the close's own grace ran out or its caller dropped it mid-wait, what was queued
/// behind a write that would not finish is refused from then on, not written into a connection
/// its host has walked away from. While another close is still waiting, the link is left to it.
struct PoisonUnlessClosed<'a> {
    state: &'a ClientState,
    reached_the_link: bool,
}

impl<'a> PoisonUnlessClosed<'a> {
    fn begin(state: &'a ClientState) -> Self {
        state.closes_in_progress.fetch_add(1, Ordering::AcqRel);
        Self {
            state,
            reached_the_link: false,
        }
    }
}

impl Drop for PoisonUnlessClosed<'_> {
    fn drop(&mut self) {
        if self.reached_the_link {
            self.state.close_reached_link.store(true, Ordering::Release);
        }
        let last = self.state.closes_in_progress.fetch_sub(1, Ordering::AcqRel) == 1;
        if last && !self.state.close_reached_link.load(Ordering::Acquire) {
            self.state.link_poisoned.store(true, Ordering::Release);
        }
    }
}

/// Says the handler's task has stopped, when it is dropped with that task.
struct WorkerStopped(CancelToken);

impl Drop for WorkerStopped {
    fn drop(&mut self) {
        self.0.cancel();
    }
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

/// A request queued with [`Client::submit_request`].
///
/// The frame is already in the queue, behind every frame queued before it, by the time this
/// exists. It splits into the two things a caller can wait for: the frame's write, and the
/// peer's answer.
///
/// ```no_run
/// # async fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
/// use mango_external_agents::jsonrpc::RequestOptions;
///
/// let submitted =
///     client.submit_request("session/prompt", serde_json::json!({}), RequestOptions::new())?;
/// let (written, reply) = submitted.into_parts();
/// written.await?;
/// let _answer = reply.await?;
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping this gives the request up, and takes it out of the queue if it is still there"]
pub struct SubmittedRequest {
    id: RequestId,
    written: Written,
    reply: Reply,
}

impl std::fmt::Debug for SubmittedRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubmittedRequest")
            .field("frame_bytes", &self.written.frame_bytes())
            .field("write_started", &self.written.started())
            .finish_non_exhaustive()
    }
}

impl SubmittedRequest {
    /// The id the request went out under, for a caller that names the request to the peer later.
    ///
    /// ```no_run
    /// # fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
    /// # use mango_external_agents::jsonrpc::RequestOptions;
    /// let submitted = client.submit_request("ping", serde_json::json!({}), RequestOptions::new())?;
    /// let _id = submitted.id().as_json();
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn id(&self) -> &RequestId {
        &self.id
    }

    /// The size of the request frame as it was queued, in encoded bytes.
    ///
    /// ```no_run
    /// # fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
    /// # use mango_external_agents::jsonrpc::RequestOptions;
    /// let submitted = client.submit_request("ping", serde_json::json!({}), RequestOptions::new())?;
    /// assert!(submitted.frame_bytes() > 0);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn frame_bytes(&self) -> usize {
        self.written.frame_bytes()
    }

    /// Whether the write of the request frame has begun. See [`Written::started`].
    ///
    /// ```no_run
    /// # fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
    /// # use mango_external_agents::jsonrpc::RequestOptions;
    /// let submitted = client.submit_request("ping", serde_json::json!({}), RequestOptions::new())?;
    /// let _possibly_delivered = submitted.write_started();
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn write_started(&self) -> bool {
        self.written.started()
    }

    /// The write of the frame, and the peer's answer, to be waited for apart.
    ///
    /// ```no_run
    /// # async fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
    /// # use mango_external_agents::jsonrpc::RequestOptions;
    /// let (written, reply) = client
    ///     .submit_request("ping", serde_json::json!({}), RequestOptions::new())?
    ///     .into_parts();
    /// written.await?;
    /// let _pong = reply.await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_parts(self) -> (Written, Reply) {
        (self.written, self.reply)
    }
}

/// The peer's answer to a request queued with [`Client::submit_request`].
///
/// Resolves to the answer, still as JSON, or to a [`CallFailure`]: the peer's own error, or what
/// ended the wait on this side, told apart by type. [`CallFailure::into_error`] turns either
/// into an [`Error`]. The deadline the request asked for is counted from when it was queued.
///
/// Dropping it gives the call up. If the request frame is still queued it is taken out and never
/// written; if its write has begun it is left to finish and the answer, when it comes, is
/// dropped. A request whose frame could not be written fails here as well; the reason is what
/// its [`Written`] resolves to.
///
/// ```no_run
/// # async fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
/// # use mango_external_agents::jsonrpc::RequestOptions;
/// let (_written, reply) = client
///     .submit_request("ping", serde_json::json!({}), RequestOptions::new())?
///     .into_parts();
/// let _pong: serde_json::Value = reply.await?;
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping this gives the request up, and takes it out of the queue if it is still there"]
pub struct Reply {
    state: Arc<ClientState>,
    waiting: oneshot::Receiver<Answer>,
    /// How long the wait for the answer may be and when that is over, counted from when the
    /// request was queued. `None` for a request without a deadline.
    deadline: Option<(Duration, tokio::time::Instant)>,
    /// The timer for that, made on the first poll: a request may be queued from a thread that
    /// has no runtime to make one on.
    timer: Option<Pin<Box<tokio::time::Sleep>>>,
    label: Option<&'static str>,
    ctl: Arc<outbox::FrameCtl>,
    /// Takes the call back out from among those awaiting an answer when this is dropped.
    pending: PendingCall,
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Reply")
            .field("write_started", &self.ctl.began())
            .finish_non_exhaustive()
    }
}

impl Future for Reply {
    type Output = std::result::Result<Value, CallFailure>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let label = self.label;
        if let Poll::Ready(answered) = Pin::new(&mut self.waiting).poll(cx) {
            let replied = self.state.replied(answered, &self.pending.id);
            return Poll::Ready(replied.map_err(|failure| failure.labelled(label)));
        }
        // Without a deadline only the answer, the connection's end or the caller ends the wait.
        let Some((timeout, deadline)) = self.deadline else {
            return Poll::Pending;
        };
        let timer = self
            .timer
            .get_or_insert_with(|| Box::pin(tokio::time::sleep_until(deadline)));
        if timer.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        Poll::Ready(Err(CallFailure::new(
            CallFailureCause::TimedOut { after: timeout },
            Error::Timeout {
                operation: String::from("a JSON-RPC request"),
                after: timeout,
            },
        )
        .labelled(label)))
    }
}

impl Drop for Reply {
    fn drop(&mut self) {
        // Nobody will hear the answer, so a frame the writer has not taken is not sent at all.
        // One it has taken is left alone: cutting a write off is what breaks a link.
        self.ctl.withdraw();
    }
}

impl Client {
    /// Starts speaking, pumping the link on a task of its own.
    ///
    /// The pump stops when the peer goes away, when [`Client::close`] is called, or when the
    /// client is dropped.
    pub fn connect(link: Link, handler: Arc<dyn PeerHandler>, options: ClientOptions) -> Self {
        Self::connect_with(link, handler, options, WireOptions::default())
    }

    /// Starts speaking as [`Client::connect`] does, under bounds on what is held for the peer.
    ///
    /// ```no_run
    /// # fn example(
    /// #     link: mango_external_agents::Link,
    /// #     handler: std::sync::Arc<dyn mango_external_agents::jsonrpc::PeerHandler>,
    /// # ) {
    /// use mango_external_agents::jsonrpc::{Client, ClientOptions, WireOptions};
    ///
    /// let wire = WireOptions::new()
    ///     .with_max_outbound_frame_bytes(1024 * 1024)
    ///     .with_max_outbound_queued_bytes(8 * 1024 * 1024);
    /// let _client = Client::connect_with(link, handler, ClientOptions::new("ACP agent"), wire);
    /// # }
    /// ```
    pub fn connect_with(
        link: Link,
        handler: Arc<dyn PeerHandler>,
        options: ClientOptions,
        wire: WireOptions,
    ) -> Self {
        let (sender, receiver) = link.split();
        let (outbox, entries) = outbox::Outbox::new(wire);
        // `mpsc::channel` panics above `Semaphore::MAX_PERMITS`, and a host may set a huge count
        // to mean "no cap".
        let notification_capacity = options
            .max_pending_notifications
            .clamp(1, Semaphore::MAX_PERMITS);
        let ordered_capacity = options.max_pending_requests.min(Semaphore::MAX_PERMITS);
        // One queue carries both kinds so their arrival order is kept; each is admitted against
        // its own count, so the sum is room neither can take from the other. The one more is for
        // the notification the reader can admit while the worker holds a response it has taken
        // out and not yet let go of: without it, the next ordered response could find no room.
        let queue_capacity = handoff_capacity(notification_capacity, ordered_capacity);
        let peer_bytes = Arc::new(Semaphore::new(
            options.max_pending_bytes.min(Semaphore::MAX_PERMITS),
        ));
        let state = Arc::new(ClientState {
            sender: Arc::new(Mutex::new(sender)),
            pending: Mutex::new(HashMap::new()),
            outbox,
            writer_abort: OnceLock::new(),
            outbound_overflow: StdMutex::new(None),
            closes_in_progress: AtomicUsize::new(0),
            close_reached_link: AtomicBool::new(false),
            #[cfg(test)]
            after_write_began: StdMutex::new(None),
            #[cfg(test)]
            after_turn_decided: StdMutex::new(None),
            #[cfg(test)]
            after_seal: StdMutex::new(None),
            #[cfg(test)]
            after_no_call_found: StdMutex::new(None),
            #[cfg(test)]
            before_call_claimed: StdMutex::new(None),
            runtime: tokio::runtime::Handle::current(),
            options,
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            shutdown: CancelToken::new(),
            write_failure: StdMutex::new(None),
            write_failed: Notify::new(),
            ended: StdMutex::new(None),
            link_poisoned: AtomicBool::new(false),
            in_flight: StdMutex::new(JoinSet::new()),
            notifications: StdMutex::new(None),
            worker_abort: OnceLock::new(),
            finishes_notification: handler.finishes_notification_in_progress(),
            worker_stopping: AtomicBool::new(false),
            worker_stopped: CancelToken::new(),
            in_notification: AtomicBool::new(false),
            #[cfg(test)]
            after_queue_length_read: StdMutex::new(None),
            peer_bytes,
            notification_capacity,
            queued_answers: AtomicUsize::new(0),
            ordered_places: Arc::new(Semaphore::new(ordered_capacity)),
        });
        let writer = tokio::spawn(outbox::run(Arc::clone(&state), entries));
        let _ = state.writer_abort.set(writer.abort_handle());
        let (notifications, notification_receiver) = mpsc::channel(queue_capacity);
        // Held by the task's future from the moment it exists, so it is dropped with the task
        // however the task ends: returned, cancelled mid-call, or cancelled before it ever ran.
        let stopped = WorkerStopped(state.worker_stopped.clone());
        let work = peer_work_pump(
            Arc::clone(&state),
            notification_receiver,
            Arc::clone(&handler),
        );
        let worker = tokio::spawn(async move {
            let _stopped = stopped;
            work.await;
        });
        let _ = state.worker_abort.set(worker.abort_handle());
        *state
            .notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(worker);
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
        self.answer_within(method, params, timeout, None, false)
            .await
    }

    /// Calls a method the way `options` describes.
    ///
    /// With default options this is [`Client::request`].
    ///
    /// ```no_run
    /// # async fn example(client: &mango_external_agents::jsonrpc::Client) {
    /// use mango_external_agents::jsonrpc::RequestOptions;
    ///
    /// // The answer arrives after every notification the peer sent ahead of it was handled.
    /// let options = RequestOptions::new().after_earlier_notifications();
    /// let _: mango_external_agents::Result<serde_json::Value> = client
    ///     .request_with("session/prompt", serde_json::json!({}), options)
    ///     .await;
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Client::request`]. A request made with
    /// [`RequestOptions::after_earlier_notifications`] is also refused with
    /// [`Error::LimitExceeded`] when [`ClientOptions::max_pending_requests`] of them already await
    /// delivery, and with [`Error::HostConfiguration`] when it is made from inside
    /// [`PeerHandler::on_notification`] on this client.
    pub async fn request_with<P, R>(
        &self,
        method: &str,
        params: P,
        options: RequestOptions,
    ) -> Result<R>
    where
        P: Serialize + Send,
        R: DeserializeOwned,
    {
        let call = self.call(
            method,
            params,
            self.state.answer_deadline(options.deadline),
            None,
            options.after_earlier_notifications,
            !options.outside_pending_budget,
        );
        serde_json::from_value(call.await?).map_err(|error| Error::Protocol {
            expected: String::from("a JSON-RPC result"),
            received: error.to_string(),
        })
    }

    /// Calls a method like [`Client::request`] and records when its frame starts reaching the
    /// link.
    ///
    /// `write_started` becomes `true` once this call holds the link's writer and begins sending,
    /// so any byte of the request may be out. A caller that drops this future while the flag is
    /// still `false` knows the peer never saw any of the request. The flag marks possible
    /// delivery, not success: it stays raised when the write then fails or the call times out. That is the fact a harness needs
    /// before it can release a start without reconciling a turn the vendor might be running.
    ///
    /// ```no_run
    /// # async fn example(client: &mango_external_agents::jsonrpc::Client) {
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let written = AtomicBool::new(false);
    /// let _: mango_external_agents::Result<serde_json::Value> =
    ///     client.request_tracking_write("ping", serde_json::json!({}), &written).await;
    /// assert!(written.load(Ordering::Acquire));
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Client::request`].
    pub async fn request_tracking_write<P, R>(
        &self,
        method: &str,
        params: P,
        write_started: &AtomicBool,
    ) -> Result<R>
    where
        P: Serialize + Send,
        R: DeserializeOwned,
    {
        let timeout = self.state.options.request_timeout;
        self.answer_within(method, params, timeout, Some(write_started), false)
            .await
    }

    async fn answer_within<P, R>(
        &self,
        method: &str,
        params: P,
        timeout: Duration,
        write_started: Option<&AtomicBool>,
        ordered: bool,
    ) -> Result<R>
    where
        P: Serialize + Send,
        R: DeserializeOwned,
    {
        let answer = self
            .call(method, params, Some(timeout), write_started, ordered, true)
            .await?;
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

    /// Queues a request without waiting, and returns what to wait for.
    ///
    /// Synchronous on purpose. The frame is admitted and placed in the one queue every outgoing
    /// frame goes through before this returns, so frames reach the wire in the order their
    /// callers made these calls: a caller that holds its own lock across a request and the
    /// notification that must follow it (a prompt, then its cancel) gets that order on the wire.
    /// From the first frame queued on a connection, [`Client::request`] and [`Client::notify`]
    /// take their turn in the same queue; a connection that never queues one is unchanged.
    ///
    /// Nothing here waits for the peer, so nothing slows a caller down but the bounds in
    /// [`WireOptions`]: with the default, which bounds nothing, a caller that queues faster than
    /// the peer reads holds every frame it queued.
    ///
    /// # Errors
    ///
    /// Refused here, with nothing queued: [`Error::Closed`]; [`Error::LimitExceeded`] for the
    /// ordered-response reserve; [`Error::HostConfiguration`] for an ordered request from inside
    /// `on_notification`; [`Error::Protocol`] for params that do not serialise;
    /// [`Error::LimitExceeded`] with [`Dispatch::NotSubmitted`]
    /// when the frame is larger than [`WireOptions::with_max_outbound_frame_bytes`] allows or
    /// than the queue could hold even when empty (any frame, under a queue bound of zero), which
    /// refuses this call alone, or when the queue's bounds are passed by what it already holds,
    /// which also ends the connection; and [`Error::Link`] when an earlier write left the link
    /// unusable.
    ///
    /// [`ClientOptions::max_pending_requests`] is applied when the frame's turn to be written
    /// comes, not here: a request past it is not written, its [`Written`] resolves to that
    /// [`Error::LimitExceeded`], and its [`Reply`] fails.
    ///
    /// ```no_run
    /// # async fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
    /// use mango_external_agents::jsonrpc::RequestOptions;
    ///
    /// // Both are queued, in this order, before either is written.
    /// let prompt =
    ///     client.submit_request("session/prompt", serde_json::json!({}), RequestOptions::new())?;
    /// let cancel = client.submit_notification("session/cancel", serde_json::json!({}))?;
    /// let (written, reply) = prompt.into_parts();
    /// written.await?;
    /// cancel.await?;
    /// let _stopped = reply.await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn submit_request<P>(
        &self,
        method: &str,
        params: P,
        options: RequestOptions,
    ) -> Result<SubmittedRequest>
    where
        P: Serialize,
    {
        if self.is_closed() {
            return Err(Error::Closed { subject: "link" });
        }
        self.queue_request(method, params, options)
    }

    /// The rest of [`Client::submit_request`], for a caller that has read the client open.
    fn queue_request<P>(
        &self,
        method: &str,
        params: P,
        options: RequestOptions,
    ) -> Result<SubmittedRequest>
    where
        P: Serialize,
    {
        // Too late is said before anything else is asked of the request: with the ordered
        // reserve full, a request that raced a close is refused for the close.
        if self.state.outbox.is_sealed() {
            return Err(Error::Closed { subject: "link" });
        }
        let ordered = options.after_earlier_notifications;
        if ordered && self.state.handling_own_notification() {
            return Err(reentrant_ordered_request());
        }
        let timeout = self.state.answer_deadline(options.deadline);
        let id = self
            .state
            .next_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string();
        let frame = self
            .state
            .frame(Some(Value::String(id.clone())), method, params)?;
        let delivery = if ordered {
            Delivery::Ordered(self.state.ordered_place()?)
        } else {
            Delivery::Immediate
        };
        let (answer, waiting) = oneshot::channel();
        let registration = Registration {
            id: id.clone(),
            answer,
            delivery,
            counted: !options.outside_pending_budget,
        };
        let ticket = self.state.enqueue(frame, Some(registration))?;
        let ctl = Arc::clone(&ticket.ctl);
        Ok(SubmittedRequest {
            id: RequestId::new(Value::String(id.clone())),
            written: Written::new(ticket, self.state.options.peer_name.clone()),
            reply: Reply {
                state: Arc::clone(&self.state),
                waiting,
                deadline: timeout.map(|timeout| (timeout, deadline_after(timeout))),
                timer: None,
                label: options.label,
                ctl,
                pending: PendingCall {
                    state: Arc::clone(&self.state),
                    id,
                },
            },
        })
    }

    /// Queues a notification without waiting, and returns its write to wait for.
    ///
    /// The synchronous counterpart of [`Client::notify`], with the ordering
    /// [`Client::submit_request`] describes.
    ///
    /// # Errors
    ///
    /// [`Error::Closed`] on a closed client, where [`Client::notify`] drops the notification
    /// silently: a caller that queues has asked what became of the frame. Otherwise the
    /// serialisation, size and link refusals of [`Client::submit_request`].
    ///
    /// ```no_run
    /// # async fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
    /// let written = client.submit_notification("session/cancel", serde_json::json!({}))?;
    /// written.await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn submit_notification<P>(&self, method: &str, params: P) -> Result<Written>
    where
        P: Serialize,
    {
        if self.is_closed() {
            return Err(Error::Closed { subject: "link" });
        }
        self.queue_notification(method, params)
    }

    /// The rest of [`Client::submit_notification`], for a caller that has read the client open.
    fn queue_notification<P>(&self, method: &str, params: P) -> Result<Written>
    where
        P: Serialize,
    {
        let frame = self.state.frame(None, method, params)?;
        let ticket = self.state.enqueue(frame, None)?;
        Ok(Written::new(ticket, self.state.options.peer_name.clone()))
    }

    /// Stops the pump, fails every call still waiting, and closes this side of the link.
    ///
    /// The process or socket underneath is the caller's to reap: this client did not open it.
    /// Idempotent.
    ///
    /// # How long it takes
    ///
    /// At most twice [`ClientOptions::shutdown_timeout`]: once for the answers this side still
    /// owes the peer to be written, and once for what was queued before the close to be written
    /// and the link closed. Both waits are over at once on a link that takes its writes. The
    /// handler's tasks are cancelled, not waited out, which takes no time unless a handler is
    /// blocking its thread without yielding; nothing here can bound that. The one exception is
    /// a notification call in progress whose handler asked for it to finish
    /// ([`PeerHandler::finishes_notification_in_progress`]): it is waited for during the first
    /// of the two waits, so the bound is the same. A caller that wants
    /// a tighter bound puts its own timeout around this future or drops it: that is safe at any
    /// point, and leaves the link as the last paragraph below describes.
    ///
    /// # What becomes of queued frames
    ///
    /// From the moment a close begins the queue takes nothing more: a
    /// [`Client::submit_notification`] or [`Client::submit_request`] that comes after is refused
    /// with [`Error::Closed`] (or with [`Error::Link`], on a link an earlier write had already
    /// left unusable), and nothing of it is queued or written. One that raced the close
    /// either got into the queue ahead of it, and is then a frame queued before the close, or
    /// was refused the same way.
    ///
    /// Of the frames queued before the close, notifications are written before the link
    /// goes. Requests whose turn to be written has not come are not: a request is entered among
    /// the calls awaiting an answer when that turn comes, and a closing client enters none, so
    /// its [`Written`] resolves to [`Error::Closed`] and its [`Reply`] fails with
    /// [`ConnectionEnd::Closed`], as does the reply of one already written. A host that needs a
    /// last request answered waits for its reply and closes afterwards.
    ///
    /// A close that does not reach the link leaves it unusable: nothing still queued is written
    /// afterwards. That is so when the close runs out of `shutdown_timeout` behind a write the
    /// peer never reads, and when the future returned here is dropped before it finishes,
    /// unless another call to `close` is still in progress, which then answers for the link.
    ///
    /// # Errors
    ///
    /// [`Error::Link`] when closing the link itself failed, [`Error::Timeout`] when the link
    /// could not be reached within `shutdown_timeout`.
    pub async fn close(&self) -> Result<()> {
        // Stored before the drain rather than inside it, and that order is the contract `call`
        // reads: a caller that finds the map open has, by that fact, arrived before this store,
        // and the drain below cannot run until that caller's entry is in the map.
        // The cause goes on record first, so whoever sees the flag finds the cause with it.
        let failure = self
            .state
            .ended_with(ConnectionEnd::Closed, self.state.closed_error());
        self.state.mark_closed();
        self.state.shutdown.cancel();
        // A close that does not get as far as the link, because its grace ran out or because its
        // caller stopped waiting, must not leave queued frames to be written behind it.
        // Closes running at once answer for the link together: one that gives up while another
        // is still writing what was queued does not take the link from under it.
        let mut giving_up = PoisonUnlessClosed::begin(&self.state);
        self.state.ask_handler_task_to_stop();
        self.state.fail_pending(failure).await;
        // The peer's own questions are answered on tasks of their own, and one waiting for a
        // person would otherwise outlive the client that spawned it — holding its share of the
        // state for the rest of the process, and replying into a link that is already gone. The
        // shutdown flag has already reached them, so this is the moment they need to write the
        // refusal, and it happens before the link is closed under them.
        //
        // A notification call its handler asked to finish gets the same grace, at the same
        // time: the two waits overlap, so the close takes no longer for it.
        if self.state.finishes_notification {
            self.state.finish_notification_and_drain_in_flight().await;
        } else {
            self.state.stop_notifications().await;
            self.state.drain_in_flight().await;
        }

        let closed = tokio::time::timeout(self.state.options.shutdown_timeout, async {
            // Behind whatever is already queued, as a close has always waited its turn behind
            // the writes ahead of it: a notification queued before the close is written before
            // the link goes, or the close runs out of its grace waiting.
            if let Some(flushed) = self.state.outbox.barrier() {
                let _ = flushed.await;
            }
            self.state.sender.lock().await.close().await
        })
        .await
        .map_err(|_| Error::Timeout {
            operation: String::from("JSON-RPC link shutdown"),
            after: self.state.options.shutdown_timeout,
        });
        giving_up.reached_the_link = closed.is_ok();
        drop(giving_up);
        let closed = closed.and_then(std::convert::identity);
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

    /// How the connection ended, or `None` while nothing has ended it.
    ///
    /// The reason is on record before any call is failed with it, and before a
    /// [`Client::submit_request`] that passed the outbound budget returns its error, so a caller
    /// holding a failure can ask this for the cause. That is a moment before
    /// [`is_closed`](Self::is_closed) turns true. It is the reason the handler's
    /// [`on_terminated`](PeerHandler::on_terminated) is given when the end came from under this
    /// side.
    ///
    /// ```no_run
    /// # fn example(client: &mango_external_agents::jsonrpc::Client) {
    /// use mango_external_agents::jsonrpc::{ConnectionEnd, PeerTermination};
    ///
    /// if let Some(ConnectionEnd::Peer(PeerTermination::Exited)) = client.ended() {
    ///     eprintln!("the peer exited");
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn ended(&self) -> Option<ConnectionEnd> {
        self.state.ended()
    }

    /// The peer as a person would name it.
    pub fn peer_name(&self) -> &str {
        &self.state.options.peer_name
    }

    async fn call<P>(
        &self,
        method: &str,
        params: P,
        timeout: Option<Duration>,
        write_started: Option<&AtomicBool>,
        ordered: bool,
        counted: bool,
    ) -> Result<Value>
    where
        P: Serialize + Send,
    {
        let Some(timeout) = timeout else {
            // No deadline on the answer. The frame's write is bounded where it is made.
            return self
                .call_unbounded(method, params, write_started, ordered, counted)
                .await;
        };
        // The deadline covers the whole call, the wait for the link included.
        tokio::time::timeout(
            timeout,
            self.call_unbounded(method, params, write_started, ordered, counted),
        )
        .await
        .map_err(|_| Error::Timeout {
            operation: String::from("a JSON-RPC request"),
            after: timeout,
        })?
    }

    async fn call_unbounded<P>(
        &self,
        method: &str,
        params: P,
        write_started: Option<&AtomicBool>,
        ordered: bool,
        counted: bool,
    ) -> Result<Value>
    where
        P: Serialize + Send,
    {
        if self.is_closed() {
            return Err(Error::Closed { subject: "link" });
        }
        // An ordered answer is delivered by the task that awaits `on_notification`. Asked for
        // from inside that call, it would wait for the call to return while the call waits for
        // it. Refused before the frame exists, so the peer is never left running the request.
        if ordered && self.state.handling_own_notification() {
            return Err(reentrant_ordered_request());
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
            let holding = counted_pending(&mut pending);
            if counted && holding >= self.state.options.max_pending_requests {
                return Err(Error::LimitExceeded {
                    subject: "pending JSON-RPC requests",
                    limit: self.state.options.max_pending_requests,
                    received: holding.saturating_add(1),
                });
            }
            let delivery = if ordered {
                Delivery::Ordered(self.state.ordered_place()?)
            } else {
                Delivery::Immediate
            };
            pending.insert(
                id.clone(),
                PendingAnswer {
                    answer,
                    delivery,
                    counted,
                },
            );
        }
        let _pending = PendingCall {
            state: Arc::clone(&self.state),
            id: id.clone(),
        };

        if let Err(error) = self.state.write_marking(frame, write_started).await {
            // The entry goes before the error leaves: nothing is waiting on this call — the
            // failure is returning here — so an orphan left behind would be failed later by a
            // close or a dying pump, against a caller that had already given up.
            self.state.pending.lock().await.remove(&id);
            return Err(error);
        }

        // A caller that stops waiting, by its deadline or by being dropped, takes its entry
        // back out as `_pending` goes.
        self.state.answered(waiting.await, &id)
    }
}

impl Drop for Client {
    /// Best effort, and nothing is left waiting by the time it runs.
    ///
    /// [`Client::close`] is the supported shutdown: it fails every call still waiting, lets the
    /// answers in flight write their refusals, and closes the link. This runs when nobody called
    /// it. A caller inside [`Client::request`] holds a borrow of this client for the life of its
    /// future, so none of those can outlive this. A [`Reply`] can, and is failed here: with the
    /// writer if its request is still queued, from the map otherwise.
    ///
    /// What is left is the end of the link. The pump, the writer and the answers in flight are
    /// taken down, frames still queued are dropped unwritten, and the sender goes with the last
    /// handle to this client's state — which on a child's stdin is the end-of-input a print-mode
    /// vendor waits for. A [`Reply`] is such a handle: it has resolved by then, but a host that
    /// keeps one without polling it keeps the sender, and so the child's stdin, open until it
    /// lets the reply go. A host that reaps the child after dropping the client should drop its
    /// replies first, or kill instead of waiting for the child to see end-of-input.
    fn drop(&mut self) {
        let failure = self
            .state
            .ended_with(ConnectionEnd::Closed, self.state.closed_error());
        self.state.mark_closed();
        self.state.shutdown.cancel();
        // Nothing holds the map across a wait, so this misses only against a thread inside it at
        // this instant. That thread may be entering a call, so the map is emptied behind it on
        // the connection's runtime, as a dropped call cleans up after itself.
        if let Ok(mut pending) = self.state.pending.try_lock() {
            for (_, waiting) in pending.drain() {
                let _ = waiting.answer.send(Err(failure.clone()));
            }
        } else {
            let state = Arc::clone(&self.state);
            self.state.runtime.spawn(async move {
                state.fail_pending(failure).await;
            });
        }
        if let Some(writer) = self.state.writer_abort.get() {
            writer.abort();
        }
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
        // Through the abort handle first: a drain may be holding the join handle, and the task
        // has to stop now all the same, not when that drain's grace runs out.
        self.abort_worker();
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

    /// Tells the handler's task to take up nothing more, for a handler that asked to finish
    /// its calls. Raised as soon as the connection is known to be cut short, so that what is
    /// queued from then on is neither started nor delivered.
    fn ask_handler_task_to_stop(&self) {
        if self.finishes_notification {
            self.worker_stopping.store(true, Ordering::SeqCst);
        }
    }

    /// Whether a notification call is in progress that is to be waited for. Read after the
    /// stop was asked for; the task says it is in a call before it looks for a stop. Whichever
    /// the interleaving, a task this finds outside a call will see the stop before it begins
    /// one.
    fn notification_call_to_wait_for(&self) -> bool {
        self.ask_handler_task_to_stop();
        // A call that asks for the stop itself cannot be waited for by the stop.
        let waited_for =
            self.in_notification.load(Ordering::SeqCst) && !self.handling_own_notification();
        #[cfg(test)]
        if !waited_for {
            Self::run_hook(&self.after_no_call_found);
        }
        waited_for
    }

    /// Waits for the notification call in progress to return, for as long as the shutdown
    /// grace, and stops the handler's task.
    async fn wait_for_notification_call(&self) {
        // Whoever gives this wait up, a close that was dropped or timed out by its caller,
        // must not leave the task running a call nobody is waiting for any more.
        struct StopUnlessWaitedOut<'a>(&'a ClientState, bool);
        impl Drop for StopUnlessWaitedOut<'_> {
            fn drop(&mut self) {
                if !self.1 {
                    self.0.abort_worker();
                }
            }
        }
        let mut stop = StopUnlessWaitedOut(self, false);
        // Waited for through the task's own signal and not its join handle: a close and the
        // reader can both be here, and each has to see the task stopped before it goes on.
        let _ = tokio::time::timeout(
            self.options.shutdown_timeout,
            self.worker_stopped.cancelled(),
        )
        .await;
        // Still there: out of grace.
        self.stop_notifications().await;
        self.worker_stopped.cancelled().await;
        stop.1 = true;
    }

    /// Stops the handler's task once the call it is in has returned, or when the shutdown grace
    /// runs out, while the answers this side still owes the peer are written.
    ///
    /// The two waits overlap only when there is a call to wait for. With none, the task is
    /// stopped first, as for a handler that did not ask: an answer task it might still have
    /// spawned is then among those waited for.
    async fn finish_notification_and_drain_in_flight(&self) {
        if self.notification_call_to_wait_for() {
            tokio::join!(self.wait_for_notification_call(), self.drain_in_flight());
        } else {
            self.stop_notifications().await;
            self.worker_stopped.cancelled().await;
            self.drain_in_flight().await;
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
        // A close or a drop that lands here stops the worker through its abort handle, which
        // ends this wait too.
        if tokio::time::timeout(self.options.shutdown_timeout, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await;
        }
    }

    fn abort_worker(&self) {
        if let Some(worker) = self.worker_abort.get() {
            worker.abort();
        }
    }

    fn abort_notifications(&self) {
        self.abort_worker();
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

    /// What a settled call returns to its caller.
    fn answered(
        &self,
        answered: std::result::Result<Answer, oneshot::error::RecvError>,
        id: &str,
    ) -> Result<Value> {
        match answered {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(failure)) => Err(self.call_failed(failure.body, id)),
            Err(_) => Err(self.went_away()),
        }
    }

    /// The same, for a caller that is told which side failed the call.
    fn replied(
        &self,
        answered: std::result::Result<Answer, oneshot::error::RecvError>,
        id: &str,
    ) -> std::result::Result<Value, CallFailure> {
        match answered {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(Failed { body, cause })) => Err(CallFailure::new(
                match cause {
                    FailedCause::Peer => CallFailureCause::Peer(body.clone()),
                    FailedCause::Ended(end) => CallFailureCause::Ended(end),
                    FailedCause::HandlerStopped => CallFailureCause::HandlerStopped,
                    FailedCause::Unwritten => CallFailureCause::Unwritten,
                },
                self.call_failed(body, id),
            )),
            // The entry was dropped unsettled, which is its frame going with a writer that was
            // taken down while the frame was still queued. A dropped client is on record as the
            // end; anything else that stops the writer has only left the frame unwritten.
            Err(_) => Err(match self.ended() {
                Some(end) => CallFailure::new(CallFailureCause::Ended(end), self.went_away()),
                None => CallFailure::new(
                    CallFailureCause::Unwritten,
                    self.call_failed(self.unwritten().body, id),
                ),
            }),
        }
    }

    /// The error a failed call has always returned: the body, under this client's code.
    fn call_failed(&self, body: JsonRpcError, id: &str) -> Error {
        Error::Vendor(body.into_vendor_error(
            ErrorCode::new(format!("{}-call-failed", self.options.code_prefix)),
            Some(id.to_owned()),
        ))
    }

    fn went_away(&self) -> Error {
        Error::Link {
            peer: self.options.peer_name.clone(),
            message: String::from("a peer that went away before answering a JSON-RPC request"),
        }
    }

    /// How the connection ended, once it has.
    fn ended(&self) -> Option<ConnectionEnd> {
        let ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
        match &ended.as_ref()?.cause {
            FailedCause::Ended(end) => Some(end.clone()),
            FailedCause::Peer | FailedCause::HandlerStopped | FailedCause::Unwritten => None,
        }
    }

    /// Queues one frame for the writer, behind everything queued before it.
    ///
    /// Refuses a frame that is too large on its own without touching the connection, and ends
    /// the connection when the queue itself has no room, which is a peer that stopped reading:
    /// the cause is recorded before the error is returned.
    fn enqueue(&self, frame: String, call: Option<Registration>) -> Result<outbox::Ticket> {
        if self.link_poisoned.load(Ordering::Acquire) {
            return Err(self.unusable_link());
        }
        let deadline = deadline_after(self.options.request_timeout);
        self.outbox
            .enqueue(frame, deadline, call)
            .map_err(|refusal| self.refused(refusal))
    }

    /// The error for a frame the outbox would not take, with what a full queue sets off.
    fn refused(&self, refusal: outbox::Refusal) -> Error {
        match refusal {
            outbox::Refusal::FrameTooLarge { limit, received } => Error::LimitExceeded {
                subject: FRAME_BYTES,
                limit,
                received,
            }
            .with_dispatch(Dispatch::NotSubmitted),
            outbox::Refusal::QueueFull {
                subject,
                limit,
                received,
            } => self.outbound_overflow(subject, limit, received),
            outbox::Refusal::NoRoomAtAll {
                subject,
                limit,
                received,
            } => Error::LimitExceeded {
                subject,
                limit,
                received,
            }
            .with_dispatch(Dispatch::NotSubmitted),
            outbox::Refusal::Sealed => Error::Closed { subject: "link" },
            outbox::Refusal::Stopped => self.writer_stopped(),
        }
    }

    /// Marks the connection closed, to the queue and to callers. Every end of a connection
    /// comes through here.
    ///
    /// The flag turns callers away before they build a frame. The seal settles the caller that
    /// read the flag a moment too early: its frame is in the queue ahead of this, to be dealt
    /// with as any frame queued before the end, or it is refused as it is queued. No frame
    /// arrives in the queue behind an end's back.
    ///
    /// The seal goes first. With the flag up and the queue still open, a late frame would be
    /// measured against the queue's bounds, and one that found the queue full would be taken
    /// for a peer that stopped reading and end a connection that is only closing.
    fn mark_closed(&self) {
        self.outbox.seal();
        #[cfg(test)]
        Self::run_hook(&self.after_seal);
        self.closed.store(true, Ordering::Release);
    }

    /// Ends the connection because this side holds more for the peer than it may.
    ///
    /// Callable from synchronous code, so it only records and signals: the cause every waiting
    /// call is failed with, the typed reason the handler is told, and a link no later frame may
    /// use. The pump does the ending, as for any failed write.
    fn outbound_overflow(&self, subject: &'static str, limit: usize, received: usize) -> Error {
        self.link_poisoned.store(true, Ordering::Release);
        let termination = PeerTermination::OutboundBackpressure {
            subject,
            limit,
            received,
        };
        self.ended_with(
            ConnectionEnd::Peer(termination.clone()),
            JsonRpcError {
                code: -32000,
                message: format!(
                    "the {} outbound queue reached its limit",
                    self.options.peer_name
                ),
                data: None,
            },
        );
        if !self.closed.load(Ordering::Acquire) {
            self.outbound_overflow
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert(termination);
            self.write_failed.notify_one();
        }
        Error::LimitExceeded {
            subject,
            limit,
            received,
        }
        .with_dispatch(Dispatch::NotSubmitted)
    }

    fn unusable_link(&self) -> Error {
        Error::Link {
            peer: self.options.peer_name.clone(),
            message: String::from(
                "the link became unusable during an earlier JSON-RPC frame write",
            ),
        }
    }

    fn writer_stopped(&self) -> Error {
        Error::Link {
            peer: self.options.peer_name.clone(),
            message: String::from("a connection whose writer has stopped"),
        }
    }

    fn write_timeout(&self) -> Error {
        Error::Timeout {
            operation: String::from("JSON-RPC frame write"),
            after: self.options.request_timeout,
        }
    }

    /// Enters a queued request among the calls awaiting an answer, as its frame's turn comes.
    ///
    /// Under the same lock and the same checks an asynchronous caller makes for itself: a
    /// request that finds the connection closed is failed here, by this side, and one past the
    /// pending budget is refused. Either way the frame is not written.
    async fn register(&self, registration: Registration) -> Result<String> {
        let Registration {
            id,
            answer,
            delivery,
            counted,
        } = registration;
        let mut pending = self.pending.lock().await;
        if self.closed.load(Ordering::Acquire) {
            drop(pending);
            let _ = answer.send(Err(self.undelivered_failure()));
            return Err(Error::Closed { subject: "link" });
        }
        let holding = counted_pending(&mut pending);
        if counted && holding >= self.options.max_pending_requests {
            let received = holding.saturating_add(1);
            drop(pending);
            let _ = answer.send(Err(self.unwritten()));
            return Err(Error::LimitExceeded {
                subject: "pending JSON-RPC requests",
                limit: self.options.max_pending_requests,
                received,
            }
            .with_dispatch(Dispatch::NotSubmitted));
        }
        pending.insert(
            id.clone(),
            PendingAnswer {
                answer,
                delivery,
                counted,
            },
        );
        Ok(id)
    }

    /// How long a request with this deadline waits for its answer. `None` is no bound.
    fn answer_deadline(&self, deadline: RequestDeadline) -> Option<Duration> {
        match deadline {
            RequestDeadline::Connection => Some(self.options.request_timeout),
            RequestDeadline::After(timeout) => Some(timeout),
            RequestDeadline::Unbounded => None,
        }
    }

    fn unwritten(&self) -> Failed {
        Failed {
            body: JsonRpcError {
                code: -32000,
                message: format!(
                    "the {} request frame was not written",
                    self.options.peer_name
                ),
                data: None,
            },
            cause: FailedCause::Unwritten,
        }
    }

    /// Fails the request whose frame was not written: no answer to it can come. With what ended
    /// the connection, when that is why.
    async fn fail_unwritten(&self, id: &str) {
        let Some(waiting) = self.pending.lock().await.remove(id) else {
            return;
        };
        let failure = self
            .ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| self.unwritten());
        let _ = waiting.answer.send(Err(failure));
    }

    #[cfg(test)]
    fn run_after_write_began(&self) {
        Self::run_hook(&self.after_write_began);
    }

    #[cfg(test)]
    fn run_hook(hook: &StdMutex<Option<Box<dyn FnOnce() + Send>>>) {
        let hook = hook.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// The link, for a caller that writes its own frame. On a connection that has never queued a
    /// frame, by the link's lock alone. On one that has: at once when nothing is ahead of it,
    /// otherwise when the outbox has worked through what was queued first.
    async fn writer_turn(&self) -> Result<outbox::SenderGuard> {
        let turn = self.outbox.turn(&self.sender);
        #[cfg(test)]
        Self::run_hook(&self.after_turn_decided);
        match turn {
            outbox::Turn::Direct(waiting) => {
                // As before there was a queue: wait on the lock, in the lock's own order.
                let sender = Arc::clone(&self.sender).lock_owned().await;
                drop(waiting);
                Ok(sender)
            }
            outbox::Turn::Now(sender) => Ok(sender),
            outbox::Turn::Queued(granted) => granted.await.map_err(|_| self.writer_stopped()),
            outbox::Turn::Stopped => Err(self.writer_stopped()),
        }
    }

    async fn write(&self, frame: String) -> Result<()> {
        self.write_marking(frame, None).await
    }

    /// Writes one frame, setting `started` once this write holds the sender and begins.
    async fn write_marking(&self, frame: String, started: Option<&AtomicBool>) -> Result<()> {
        self.outbox
            .frame_fits(frame.len())
            .map_err(|refusal| self.refused(refusal))?;
        tokio::time::timeout(self.options.request_timeout, async {
            let mut sender = self.writer_turn().await?;
            // Read again holding the sender: the write that held it before may have been
            // abandoned mid-frame or failed while this one waited, and the pump has not
            // necessarily ended the connection yet. `closed` is deliberately not the test:
            // a closing connection still writes the refusals its in-flight questions are owed.
            if self.link_poisoned.load(Ordering::Acquire) {
                return Err(Error::Link {
                    peer: self.options.peer_name.clone(),
                    message: String::from(
                        "the link became unusable during an earlier JSON-RPC frame write",
                    ),
                });
            }
            if let Some(started) = started {
                started.store(true, Ordering::Release);
            }
            // Armed only once this write holds the sender: a caller that merely waited behind
            // another writer has put nothing on the wire, and that writer's own guard reports it.
            let mut midsend = MidSend::arm(self);
            let sent = sender.send(frame).await;
            midsend.finish();
            if let Err(error) = &sent
                && matches!(error.cause(), Error::Link { .. })
            {
                // A completed transport failure can leave the read half open. Poison before
                // releasing the sender so a queued writer cannot race the pump's termination.
                // Local admission and validation refusals leave a healthy transport usable.
                self.link_poisoned.store(true, Ordering::Release);
                self.signal_write_failure(error);
            }
            sent
        })
        .await
        .map_err(|_| Error::Timeout {
            operation: String::from("JSON-RPC frame write"),
            after: self.options.request_timeout,
        })?
    }

    /// What names this client in [`HANDLING_NOTIFICATION_FOR`]. Stable while anything holds the
    /// state, which the task inside the handler does.
    fn address(&self) -> usize {
        std::ptr::from_ref(self).addr()
    }

    /// Whether the caller is inside this client's `on_notification`, on the task that awaits it.
    fn handling_own_notification(&self) -> bool {
        HANDLING_NOTIFICATION_FOR
            .try_with(|client| *client == self.address())
            .unwrap_or(false)
    }

    /// How many notifications and peer questions the handoff channel holds.
    ///
    /// The channel's length less the ordered responses in it. The two are separate reads, and
    /// the worker can take a response out between them: read the length first and the count
    /// second, and a response that was counted in the length is gone from the count, which makes
    /// the peer's share look one larger than it is and ends a healthy connection at the limit.
    /// So the count is read on both sides of the length and the read repeated when it moved.
    /// Only the worker lowers it during a read (this is the reader, the one task that raises
    /// it), so it settles in as many rounds as there were responses.
    ///
    /// What is left is the other direction: the worker has taken a response out and not yet
    /// dropped it, so the length is one short and the count is not. That reads one low, never
    /// more with one worker, and the channel has a slot for the notification it lets through.
    fn peer_work_queued(&self, queue: &mpsc::Sender<QueuedWork>) -> usize {
        let mut responses = self.queued_answers.load(Ordering::Acquire);
        loop {
            let length = queue.max_capacity() - queue.capacity();
            #[cfg(test)]
            self.run_after_queue_length_read();
            let settled = self.queued_answers.load(Ordering::Acquire);
            if settled == responses {
                return peer_share(length, responses);
            }
            responses = settled;
        }
    }

    #[cfg(test)]
    fn run_after_queue_length_read(&self) {
        let hook = self
            .after_queue_length_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// The termination a peer earns by sending more than the handoff queue may hold.
    fn queue_full(&self) -> PeerTermination {
        let limit = self.options.max_pending_notifications.max(1);
        PeerTermination::NotificationBackpressure {
            limit,
            received: limit.saturating_add(1),
        }
    }

    /// What a call still waiting is failed with when the peer overran the handoff queue.
    fn overflow_failure(&self) -> JsonRpcError {
        JsonRpcError {
            code: -32000,
            message: format!(
                "the {} notification queue reached its limit",
                self.options.peer_name
            ),
            data: None,
        }
    }

    /// Takes one of the places the handoff queue reserves for ordered responses.
    fn ordered_place(&self) -> Result<OwnedSemaphorePermit> {
        Arc::clone(&self.ordered_places)
            .try_acquire_owned()
            .map_err(|_| Error::LimitExceeded {
                subject: "ordered JSON-RPC responses awaiting delivery",
                limit: self.options.max_pending_requests,
                received: self.options.max_pending_requests.saturating_add(1),
            })
    }

    /// Records what ended the connection and returns it. The first cause is kept.
    fn ended_with(&self, end: ConnectionEnd, body: JsonRpcError) -> Failed {
        self.ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(Failed {
                body,
                cause: FailedCause::Ended(end),
            })
            .clone()
    }

    /// What a response dropped from the handoff queue fails its caller with.
    ///
    /// Whatever ended the connection, when something did. Otherwise the queue went away under a
    /// live connection, which only a handler that panicked does: the answer was read and can no
    /// longer be delivered in order, so the caller is told now instead of at its deadline.
    fn undelivered_failure(&self) -> Failed {
        self.ended
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| Failed {
                body: JsonRpcError {
                    code: -32000,
                    message: format!(
                        "the {} notification handler stopped before the answer was delivered",
                        self.options.peer_name
                    ),
                    data: None,
                },
                cause: FailedCause::HandlerStopped,
            })
    }

    async fn fail_pending(&self, failure: Failed) {
        let waiting: Vec<_> = self.pending.lock().await.drain().collect();
        for (_, waiting) in waiting {
            let _ = waiting.answer.send(Err(failure.clone()));
        }
    }

    /// Fails the calls the reader settles itself and leaves the ordered ones waiting.
    ///
    /// The first half of ending a connection whose queued work is still to be drained: an
    /// ordered call learns the connection ended only after the notifications read before that
    /// have been handled. One whose response is already queued is no longer in this map at all.
    async fn fail_unordered(&self, failure: &Failed) {
        let mut pending = self.pending.lock().await;
        let (failing, waiting): (Vec<_>, Vec<_>) = pending
            .drain()
            .partition(|(_, waiting)| matches!(waiting.delivery, Delivery::Immediate));
        pending.extend(waiting);
        drop(pending);
        for (_, waiting) in failing {
            let _ = waiting.answer.send(Err(failure.clone()));
        }
    }

    /// Hands a response the reader just read to its caller, or queues it behind the peer work
    /// read before it when the caller asked for that order.
    ///
    /// Never waits on the queue: an ordered call reserved its place when it was admitted, so
    /// there is room whatever the notifications ahead of it have used.
    async fn deliver(
        state: &Arc<Self>,
        queue: &mpsc::Sender<QueuedWork>,
        id: String,
        outcome: Answer,
    ) -> std::result::Result<(), PeerTermination> {
        // A response nobody is waiting for is dropped: the call timed out or was abandoned, or
        // this is a second response to a request whose first is already delivered or queued.
        let Some(PendingAnswer {
            answer, delivery, ..
        }) = state.pending.lock().await.remove(&id)
        else {
            return Ok(());
        };
        let place = match delivery {
            Delivery::Immediate => {
                let _ = answer.send(outcome);
                return Ok(());
            }
            Delivery::Ordered(place) => place,
        };
        state.queued_answers.fetch_add(1, Ordering::AcqRel);
        let queued = QueuedWork::Response(QueuedAnswer {
            state: Arc::clone(state),
            answer: Some(answer),
            outcome: Some(outcome),
            _place: place,
        });
        match queue.try_send(queued) {
            Ok(()) => Ok(()),
            // The worker is gone: the connection is ending, or the handler panicked. The entry
            // came back in the error and fails its caller as it is dropped here.
            Err(mpsc::error::TrySendError::Closed(_)) => Ok(()),
            // Unreachable while the reserve is counted correctly. Fails closed all the same. The
            // cause is recorded before the entry is dropped, so its caller is told the truth.
            Err(mpsc::error::TrySendError::Full(queued)) => {
                state.ended_with(
                    ConnectionEnd::Peer(state.queue_full()),
                    state.overflow_failure(),
                );
                drop(queued);
                Err(state.queue_full())
            }
        }
    }

    /// Records that a write failed and wakes the pump, which ends the connection.
    ///
    /// Only signals: the caller may be an answer task or the peer-work worker, both of which the
    /// pump waits for while it terminates, so terminating from here would wait on itself. Nothing
    /// is recorded once the connection is closing, since the pump is then already on its way out
    /// and a refusal that lands on a link being shut down is expected. The first cause is kept.
    fn signal_write_failure(&self, error: &Error) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        self.write_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(|| error.to_string());
        self.write_failed.notify_one();
    }

    /// The cause of the first write that left the link unusable, if one did.
    fn take_write_failure(&self) -> Option<String> {
        self.write_failure
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }
}

async fn pump(
    state: Arc<ClientState>,
    mut receiver: Box<dyn crate::link::LinkReceiver>,
    handler: Arc<dyn PeerHandler>,
    notifications: mpsc::Sender<QueuedWork>,
) {
    loop {
        let message = tokio::select! {
            biased;
            () = state.shutdown.cancelled() => break,
            // A transport failure, refused reply or abandoned send can leave the read side open.
            // End the connection here, on the one task that owns termination.
            () = state.write_failed.notified() => {
                let overflow = state
                    .outbound_overflow
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take();
                if let Some(termination) = overflow {
                    // The cause was recorded where the budget was passed; this returns it.
                    let failure = state.ended_with(
                        ConnectionEnd::Peer(termination.clone()),
                        state.closed_error(),
                    );
                    let termination = failure.termination(termination);
                    connection_failed(&state, &handler, notifications, termination, failure)
                        .await;
                    break;
                }
                let cause = state
                    .take_write_failure()
                    .unwrap_or_else(|| String::from("a frame could not be written"));
                link_failed(&state, &handler, notifications, cause).await;
                break;
            }
            message = receiver.recv() => message,
        };

        match message {
            Ok(Some(message)) => {
                if let Err(termination) = dispatch(&state, &notifications, message).await {
                    let failure = state.ended_with(
                        ConnectionEnd::Peer(termination.clone()),
                        state.overflow_failure(),
                    );
                    let termination = failure.termination(termination);
                    state.mark_closed();
                    state.ask_handler_task_to_stop();
                    state.fail_pending(failure).await;
                    if state.finishes_notification {
                        // As a close does it: the shutdown flag first, so whatever the call in
                        // progress is waiting on that the flag releases lets it return, and the
                        // two waits at once, so the handler is told within one grace.
                        state.shutdown.cancel();
                        state.finish_notification_and_drain_in_flight().await;
                    } else {
                        state.stop_notifications().await;
                        state.shutdown.cancel();
                        state.drain_in_flight().await;
                    }
                    handler.on_terminated(termination).await;
                    break;
                }
            }
            Ok(None) => {
                let failure = state.ended_with(
                    ConnectionEnd::Peer(PeerTermination::Exited),
                    JsonRpcError {
                        code: -32000,
                        message: format!("the {} exited", state.options.peer_name),
                        data: None,
                    },
                );
                let termination = failure.termination(PeerTermination::Exited);
                state.mark_closed();
                fail_pending_around_drain(&state, notifications, failure).await;
                state.shutdown.cancel();
                state.drain_in_flight().await;
                handler.on_terminated(termination).await;
                break;
            }
            Err(error) => {
                link_failed(&state, &handler, notifications, error.to_string()).await;
                break;
            }
        }
    }
}

/// Ends the connection because its link failed, on either side. Runs on the pump only, once, as
/// the last thing it does.
async fn link_failed(
    state: &Arc<ClientState>,
    handler: &Arc<dyn PeerHandler>,
    notifications: mpsc::Sender<QueuedWork>,
    cause: String,
) {
    let termination = PeerTermination::LinkFailed(cause.clone());
    let failure = state.ended_with(
        ConnectionEnd::Peer(termination.clone()),
        JsonRpcError {
            code: -32000,
            message: format!("the {} link failed: {cause}", state.options.peer_name),
            data: None,
        },
    );
    let termination = failure.termination(termination);
    connection_failed(state, handler, notifications, termination, failure).await;
}

/// Ends a connection that can no longer be written to, telling the handler why.
async fn connection_failed(
    state: &Arc<ClientState>,
    handler: &Arc<dyn PeerHandler>,
    notifications: mpsc::Sender<QueuedWork>,
    termination: PeerTermination,
    failure: Failed,
) {
    state.mark_closed();
    fail_pending_around_drain(state, notifications, failure).await;
    state.shutdown.cancel();
    state.drain_in_flight().await;
    handler.on_terminated(termination).await;
}

/// Fails the calls still waiting on a connection that ended, and lets the peer work already read
/// reach the handler.
///
/// An ordinary call fails at once, as nothing it waits for can still arrive. Closing the queue's
/// sender then lets the worker deliver every frame it already owns, including an activity
/// immediately before EOF and an ordered response read before the end, before it exits. Only
/// then do the ordered calls left over fail: the ones the peer never answered. One whose queued
/// response the bounded drain did not reach was failed as the worker was taken down.
async fn fail_pending_around_drain(
    state: &Arc<ClientState>,
    notifications: mpsc::Sender<QueuedWork>,
    failure: Failed,
) {
    state.fail_unordered(&failure).await;
    drop(notifications);
    state.drain_notifications().await;
    state.fail_pending(failure).await;
}

async fn dispatch(
    state: &Arc<ClientState>,
    notifications: &mpsc::Sender<QueuedWork>,
    message: String,
) -> std::result::Result<(), PeerTermination> {
    // Not every line on a peer's output is a frame. Dropping an unparseable one keeps a stray
    // diagnostic from killing a live turn.
    let Ok(Value::Object(mut frame)) = serde_json::from_str::<Value>(&message) else {
        return Ok(());
    };

    // The frame is owned and read once, so every member is taken out of it rather than cloned: a
    // large diff notification would otherwise exist twice until the clone is dropped.
    let id = frame.remove("id");
    if let Some(Value::String(method)) = frame.remove("method") {
        let params = frame.remove("params").unwrap_or(Value::Null);
        let over_bytes = || PeerTermination::NotificationByteBackpressure {
            limit: state.options.max_pending_bytes,
            received: message.len(),
        };
        let bytes = u32::try_from(message.len()).map_err(|_| over_bytes())?;
        let permit = Arc::clone(&state.peer_bytes)
            .try_acquire_many_owned(bytes)
            .map_err(|_| over_bytes())?;
        // The channel also has room reserved for ordered responses, so its own capacity no
        // longer says when the peer's share is spent.
        if state.peer_work_queued(notifications) >= state.notification_capacity {
            return Err(state.queue_full());
        }
        let work = match id {
            Some(id) => PeerWork::Request { method, params, id },
            None => PeerWork::Notification { method, params },
        };
        return notifications
            .try_send(QueuedWork::Peer {
                work,
                bytes: permit,
            })
            .map_err(|_| state.queue_full());
    }

    if let Some(id) = id {
        let outcome = match frame.remove("error") {
            // Deserialised from a reference so the raw value survives for the fallback, which is
            // only rendered when the body is malformed.
            Some(error) => Err(Failed {
                body: JsonRpcError::deserialize(&error).unwrap_or_else(|_| JsonRpcError {
                    code: -32603,
                    message: error.to_string(),
                    data: None,
                }),
                cause: FailedCause::Peer,
            }),
            None => Ok(frame.remove("result").unwrap_or(Value::Null)),
        };
        return ClientState::deliver(state, notifications, RequestId::new(id).key(), outcome).await;
    }
    Ok(())
}

/// What an ordered request made from inside its own client's `on_notification` is refused with.
fn reentrant_ordered_request() -> Error {
    Error::HostConfiguration {
        expected: "an ordered JSON-RPC request made outside this client's notification handler",
        received: String::from(
            "one made inside on_notification, whose return its answer would wait for",
        ),
    }
}

/// When a wait of `after` that starts now is over.
///
/// A host may set a huge duration to mean "no deadline", and an instant that far out does not
/// exist to add up to.
fn deadline_after(after: Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now.checked_add(after)
        .unwrap_or_else(|| now + Duration::from_secs(60 * 60 * 24 * 365 * 30))
}

/// How many entries the handoff channel has room for.
///
/// Every notification the peer may queue, every place reserved for an ordered response, and one
/// more: the reader can admit one notification too many while the worker holds a response it has
/// taken out and not yet dropped (see [`ClientState::peer_work_queued`]), and without a slot for
/// it the next ordered response could find the channel full. Clamped, since the channel panics
/// above `Semaphore::MAX_PERMITS` and a host may set a huge count to mean "no cap".
fn handoff_capacity(notifications: usize, ordered: usize) -> usize {
    notifications
        .saturating_add(ordered)
        .saturating_add(1)
        .min(Semaphore::MAX_PERMITS)
}

/// The peer's share of a handoff channel holding `length` entries, `responses` of them ordered
/// responses.
fn peer_share(length: usize, responses: usize) -> usize {
    length.saturating_sub(responses)
}

/// One entry of the handoff queue between the reader and the task that runs the handler.
enum QueuedWork {
    /// Something the peer sent that the handler has to see.
    Peer {
        work: PeerWork,
        /// The frame's share of the callback byte budget, held until the handler is done with it.
        bytes: OwnedSemaphorePermit,
    },
    /// A response whose caller asked for it after the peer work read before it.
    Response(QueuedAnswer),
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
    mut notifications: mpsc::Receiver<QueuedWork>,
    handler: Arc<dyn PeerHandler>,
) {
    // Read once: a handler that did not ask to finish its calls pays for none of this.
    let finishes = state.finishes_notification;
    while let Some(queued) = notifications.recv().await {
        // Asked to stop: nothing more is taken up, whatever kind of work it is. An answer
        // that was waiting its turn is failed as the queue is dropped.
        if finishes && state.worker_stopping.load(Ordering::SeqCst) {
            break;
        }
        let (work, _permit) = match queued {
            QueuedWork::Response(answer) => {
                // Everything read before this response has been handed to the handler and, for a
                // notification, awaited.
                answer.deliver();
                continue;
            }
            QueuedWork::Peer { work, bytes } => (work, bytes),
        };
        match work {
            PeerWork::Notification { method, params } => {
                if finishes {
                    #[cfg(test)]
                    ClientState::run_hook(&state.before_call_claimed);
                    // Raised before the stop flag is read, as the stopper raises that flag
                    // before it reads this one.
                    state.in_notification.store(true, Ordering::SeqCst);
                    if state.worker_stopping.load(Ordering::SeqCst) {
                        break;
                    }
                }
                HANDLING_NOTIFICATION_FOR
                    .scope(state.address(), handler.on_notification(method, params))
                    .await;
                if finishes {
                    state.in_notification.store(false, Ordering::SeqCst);
                    // Whoever asked for the stop is waiting for exactly this, and with nothing
                    // queued behind the call nothing else would wake this task to see it.
                    if state.worker_stopping.load(Ordering::SeqCst) {
                        break;
                    }
                }
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
    // A reply that never lands leaves the peer blocked on a question this side believes it has
    // answered, and a timed-out write may have left half a frame on the link. Either way the link
    // is unusable, so the pump is told; it is the only place that ends a connection.
    let id = frame.get("id").cloned().unwrap_or(Value::Null);
    let Err(error) = state.write(Value::Object(frame).to_string()).await else {
        return;
    };
    if !oversized_frame(&error) {
        state.signal_write_failure(&error);
        return;
    }
    // Nothing was written and the link is as good as it was: the answer was only too large for
    // the bound this client sends under. The peer still has to hear something, or it waits on
    // this question for good. The replacement carries the peer's own id, so a peer that chose an
    // id as large as the bound gets no reply that fits at all; that connection cannot answer the
    // question in any form and ends as a failed link, naming the bound.
    let mut refusal = Map::new();
    if state.options.include_version_header {
        refusal.insert(String::from("jsonrpc"), json!("2.0"));
    }
    refusal.insert(String::from("id"), id);
    refusal.insert(
        String::from("error"),
        json!({ "code": -32603, "message": "the answer was too large to send" }),
    );
    if let Err(error) = state.write(Value::Object(refusal).to_string()).await {
        state.signal_write_failure(&error);
    }
}

/// Whether `error` is this client's own refusal of one frame for its size.
fn oversized_frame(error: &Error) -> bool {
    matches!(
        error.cause(),
        Error::LimitExceeded {
            subject: FRAME_BYTES,
            ..
        }
    )
}

/// What the per-frame outbound bound counts, as [`Error::LimitExceeded`] names it.
const FRAME_BYTES: &str = "bytes of one outgoing JSON-RPC frame";

#[cfg(test)]
mod tests {
    mod handler_grace_tests;
    mod handoff_queue_tests;
    mod ordered_response_tests;
    mod outbox_tests;
    mod request_lifecycle_tests;
    mod write_failure_tests;

    use super::{
        Client, ClientOptions, JsonRpcError, PeerHandler, PeerTermination, RequestId,
        RequestOptions, ServerRequestOutcome,
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
        recorded: Notify,
        terminations: Mutex<Vec<PeerTermination>>,
        answer: Option<ServerRequestOutcome>,
    }

    impl RecordingHandler {
        fn arc(answer: Option<ServerRequestOutcome>) -> Arc<Self> {
            Arc::new(Self {
                notifications: Mutex::new(Vec::new()),
                recorded: Notify::new(),
                terminations: Mutex::new(Vec::new()),
                answer,
            })
        }

        /// Waits until the handler has recorded at least `count` notifications, then returns
        /// them in the order it recorded them.
        ///
        /// Notifications are handled on a task apart from the reader, so nothing else a test can
        /// await (a response least of all) says the handler has run. Bounded, so a notification
        /// that never arrives fails by name instead of hanging.
        ///
        /// ```ignore
        /// link.push_line(r#"{"method":"item/started"}"#);
        /// let seen = handler.wait_for_notifications(1).await;
        /// ```
        async fn wait_for_notifications(&self, count: usize) -> Vec<(String, Value)> {
            let wait = async {
                loop {
                    let recorded = self.recorded.notified();
                    let seen = self.notifications.lock().await.clone();
                    if seen.len() >= count {
                        return seen;
                    }
                    recorded.await;
                }
            };
            match tokio::time::timeout(Duration::from_secs(5), wait).await {
                Ok(seen) => seen,
                Err(_) => panic!(
                    "expected {count} notifications handled within 5s | received {:?}",
                    self.notifications.lock().await
                ),
            }
        }
    }

    #[async_trait::async_trait]
    impl PeerHandler for RecordingHandler {
        async fn on_notification(&self, method: String, params: Value) {
            self.notifications.lock().await.push((method, params));
            self.recorded.notify_waiters();
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

        let started = AtomicBool::new(false);
        let refused = client
            .request_tracking_write::<_, Value>("thread/start", Unwritable, &started)
            .await;
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
        assert!(!started.load(Ordering::Acquire));
        link.push_line(r#"{"id":"2","result":"fresh"}"#);
        let answer: String = client
            .request("fresh", json!({}))
            .await
            .expect("expected serialization refusal to leave the link usable");
        assert_eq!(answer, "fresh");
        assert!(!client.is_closed());
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
        client
            .notify("fresh", json!({}))
            .await
            .expect("expected pending admission refusal to preserve the link");
        assert_eq!(link.sent().len(), 2);
        assert!(!client.is_closed());
    }

    /// A host may set a huge count to mean "no cap". The queues behind these limits are semaphore
    /// backed and panic above `Semaphore::MAX_PERMITS`, so they are clamped, not passed through.
    #[tokio::test]
    async fn connecting_under_limits_of_usize_max_does_not_panic() {
        let limits = crate::Limits {
            turn_channel_capacity: usize::MAX,
            turn_buffer_bytes: usize::MAX,
            max_pending_requests: usize::MAX,
            ..crate::Limits::default()
        };
        let link = ScriptedLink::new();
        let connected = tokio::spawn(async move {
            let client = Client::connect(
                link.into_link(),
                RecordingHandler::arc(None),
                ClientOptions::new("peer").with_limits(&limits),
            );
            drop(client);
        })
        .await;
        assert!(
            connected.is_ok(),
            "expected connect under usize::MAX limits: no panic | received {connected:?}"
        );
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

        // No response stands in for "the handler is done": one is settled by the reader and can
        // reach its caller before either notification has been handled.
        let seen = handler.wait_for_notifications(2).await;
        let methods: Vec<&str> = seen.iter().map(|(method, _)| method.as_str()).collect();
        assert_eq!(
            methods,
            vec!["item/started", "item/completed"],
            "expected notifications in arrival order: [item/started, item/completed] | received {methods:?}"
        );

        client.close().await.expect("expected a clean close");
    }

    /// A handler that holds every notification at a gate before recording it, the way a host
    /// that is slow to read its turn stream does, and lets them through once the test opens it.
    struct GatedRecorder {
        entered: Notify,
        gate: tokio::sync::Semaphore,
        notifications: Mutex<Vec<String>>,
        recorded: Notify,
    }

    impl GatedRecorder {
        fn arc() -> Arc<Self> {
            Arc::new(Self {
                entered: Notify::new(),
                gate: tokio::sync::Semaphore::new(0),
                notifications: Mutex::new(Vec::new()),
                recorded: Notify::new(),
            })
        }

        /// Lets `count` held or future notifications through the gate.
        ///
        /// ```ignore
        /// handler.open_for(2);
        /// ```
        fn open_for(&self, count: usize) {
            self.gate.add_permits(count);
        }

        /// The methods recorded so far, in the order they passed the gate.
        ///
        /// ```ignore
        /// assert!(handler.recorded().await.is_empty());
        /// ```
        async fn recorded(&self) -> Vec<String> {
            self.notifications.lock().await.clone()
        }

        /// Waits until `count` notifications have passed the gate, then returns their methods in
        /// order. Bounded, so a notification still held fails by name instead of hanging.
        ///
        /// ```ignore
        /// handler.open_for(2);
        /// let methods = handler.wait_for_recorded(2).await;
        /// ```
        async fn wait_for_recorded(&self, count: usize) -> Vec<String> {
            let wait = async {
                loop {
                    let recorded = self.recorded.notified();
                    let seen = self.recorded().await;
                    if seen.len() >= count {
                        return seen;
                    }
                    recorded.await;
                }
            };
            match tokio::time::timeout(Duration::from_secs(5), wait).await {
                Ok(seen) => seen,
                Err(_) => panic!(
                    "expected {count} notifications past the gate within 5s | received {:?}",
                    self.recorded().await
                ),
            }
        }
    }

    #[async_trait::async_trait]
    impl PeerHandler for GatedRecorder {
        async fn on_notification(&self, method: String, _params: Value) {
            self.entered.notify_one();
            self.gate
                .acquire()
                .await
                .expect("expected the gate to stay open for the whole test")
                .forget();
            self.notifications.lock().await.push(method);
            self.recorded.notify_waiters();
        }

        async fn on_request(
            &self,
            _method: String,
            _params: Value,
            _id: RequestId,
        ) -> ServerRequestOutcome {
            ServerRequestOutcome::Answer(Value::Null)
        }
    }

    /// Arrival order holds among notifications, not between a notification and a response. The
    /// reader settles a response itself and only queues a notification for the handler's task,
    /// so an answer reaches its caller while notifications the peer sent before it still wait.
    #[tokio::test]
    async fn a_response_reaches_its_caller_while_earlier_notifications_wait_on_the_handler() {
        let link = ScriptedLink::new();
        let handler = GatedRecorder::arc();
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server").with_request_timeout(Duration::from_secs(5)),
        );

        // The peer writes two notifications and then the answer, once the question is on the wire.
        let peer = async {
            link.wait_for_sent(1).await;
            link.push_line(r#"{"jsonrpc":"2.0","method":"item/started","params":{"n":1}}"#);
            link.push_line(r#"{"jsonrpc":"2.0","method":"item/completed","params":{"n":2}}"#);
            link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"pong"}"#);
        };
        let (answer, ()) = tokio::join!(client.request::<_, Value>("ping", json!({})), peer);

        let answer = answer.expect("expected the answer while the handler held the gate");
        assert_eq!(answer, json!("pong"), "received {answer}");
        // The handler is inside the first notification and has finished neither.
        tokio::time::timeout(Duration::from_secs(5), handler.entered.notified())
            .await
            .expect("expected the first notification to have reached the handler");
        let held = handler.recorded().await;
        assert!(
            held.is_empty(),
            "expected no notification handled before the gate opened: [] | received {held:?}"
        );

        handler.open_for(2);
        let methods = handler.wait_for_recorded(2).await;
        assert_eq!(
            methods,
            vec!["item/started", "item/completed"],
            "expected notifications in arrival order: [item/started, item/completed] | received {methods:?}"
        );

        client.close().await.expect("expected a clean close");
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

    /// Sends one request and answers it with the given frame, whatever shape the frame has.
    async fn outcome_of(frame: &str) -> crate::error::Result<Value> {
        let link = ScriptedLink::new();
        link.push_line(frame);
        let client = client(link, RecordingHandler::arc(None));
        client.request::<_, Value>("thread/start", json!({})).await
    }

    #[tokio::test]
    async fn a_null_or_malformed_error_member_still_takes_the_error_path() {
        let cases = [
            (r#"{"id":"1","error":null}"#, "null"),
            (r#"{"id":"1","error":"boom"}"#, r#""boom""#),
            (
                r#"{"id":"1","error":{"code":"x"},"result":1}"#,
                r#"{"code":"x"}"#,
            ),
        ];
        for (frame, message) in cases {
            let error = outcome_of(frame).await.expect_err(&format!(
                "expected a failure for {frame}, received an answer"
            ));
            let Error::Vendor(vendor) = error else {
                panic!("expected a vendor failure for {frame}, received {error:?}");
            };
            assert_eq!(vendor.message, message, "frame: {frame}");
            assert_eq!(
                vendor.vendor_code.as_deref(),
                Some("-32603"),
                "expected the internal-error code for {frame}"
            );
        }
    }

    #[tokio::test]
    async fn an_error_member_wins_over_a_result_member() {
        let error = outcome_of(
            r#"{"id":"1","result":{"ok":true},"error":{"code":-32000,"message":"refused"}}"#,
        )
        .await
        .expect_err("expected the error member to win, received an answer");
        assert!(
            matches!(&error, Error::Vendor(vendor) if vendor.message == "refused"),
            "expected the peer's refusal, received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_frame_whose_method_is_not_a_string_is_a_response() {
        let answer = outcome_of(r#"{"id":"1","method":7,"params":{"a":1},"result":{"b":2}}"#)
            .await
            .expect("expected the frame to settle the call");
        assert_eq!(answer, json!({ "b": 2 }));
    }

    #[tokio::test]
    async fn a_response_without_a_result_settles_with_null() {
        let answer = outcome_of(r#"{"id":"1"}"#)
            .await
            .expect("expected the frame to settle the call");
        assert_eq!(answer, Value::Null);
    }

    #[tokio::test]
    async fn a_notification_reaches_the_handler_with_its_params_intact() {
        let link = ScriptedLink::new();
        let handler = RecordingHandler::arc(None);
        let client = client(link.clone(), Arc::clone(&handler));

        link.push_line(
            r#"{"method":"item/patch","params":{"changes":[{"path":"a.rs","kind":null}],"n":1.5}}"#,
        );
        link.push_line(r#"{"method":"item/bare"}"#);

        let seen = handler.wait_for_notifications(2).await;
        let expected = vec![
            (
                String::from("item/patch"),
                json!({ "changes": [{ "path": "a.rs", "kind": null }], "n": 1.5 }),
            ),
            (String::from("item/bare"), Value::Null),
        ];
        assert_eq!(
            seen, expected,
            "expected notifications with their params intact: {expected:?} | received {seen:?}"
        );

        client.close().await.expect("expected a clean close");
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

    /// A tracked request reports its write once the frame is on its way, and not before: a future
    /// dropped unpolled leaves the flag down, one that reached the link raises it.
    #[tokio::test(start_paused = true)]
    async fn a_tracked_request_reports_only_a_write_that_began() {
        let link = ScriptedLink::new();
        let client = Client::connect(
            link.clone().into_link(),
            RecordingHandler::arc(None),
            ClientOptions::new("peer"),
        );

        let unsent = AtomicBool::new(false);
        drop(client.request_tracking_write::<_, Value>("ping", json!({}), &unsent));
        assert!(
            !unsent.load(Ordering::Acquire),
            "expected write_started: false for an unpolled request | received: true"
        );
        assert!(link.sent().is_empty(), "expected nothing on the wire");

        let sent = AtomicBool::new(false);
        let answering = link.clone();
        let (answer, ()) = tokio::join!(
            client.request_tracking_write::<_, Value>("ping", json!({}), &sent),
            async move {
                answering.wait_for_sent(1).await;
                answering.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"pong"}"#);
            }
        );
        assert!(
            sent.load(Ordering::Acquire),
            "expected write_started: true after the frame was sent | received: false"
        );
        assert_eq!(answer.expect("expected the scripted answer"), json!("pong"));
    }

    /// Holds the one send whose frame names `method`, so the writer stays busy for the test.
    struct GatedSender {
        inner: Box<dyn crate::link::LinkSender>,
        method: &'static str,
        gate: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait::async_trait]
    impl crate::link::LinkSender for GatedSender {
        async fn send(&mut self, message: String) -> crate::error::Result<()> {
            if message.contains(self.method) {
                self.gate
                    .acquire()
                    .await
                    .expect("the fake send gate must stay open")
                    .forget();
            }
            self.inner.send(message).await
        }

        async fn close(&mut self) -> crate::error::Result<()> {
            self.inner.close().await
        }
    }

    /// A tracked request waiting for the writer another call holds has written nothing, so its
    /// flag stays down; it rises only once that request takes the writer and sends.
    #[tokio::test(start_paused = true)]
    async fn a_tracked_request_behind_a_busy_writer_reports_no_write() {
        let link = ScriptedLink::new();
        let (sender, receiver) = link.clone().into_link().split();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let client = Arc::new(Client::connect(
            crate::link::Link::new(
                Box::new(GatedSender {
                    inner: sender,
                    method: "hold",
                    gate: Arc::clone(&gate),
                }),
                receiver,
            ),
            RecordingHandler::arc(None),
            ClientOptions::new("peer"),
        ));
        let holding = Arc::clone(&client);
        let held =
            tokio::spawn(async move { holding.request::<_, Value>("hold", json!({})).await });
        tokio::time::sleep(Duration::from_millis(1)).await;

        let flag = AtomicBool::new(false);
        let mut tracked =
            std::pin::pin!(client.request_tracking_write::<_, Value>("ping", json!({}), &flag));
        tokio::select! {
            biased;
            _ = &mut tracked => panic!("expected the tracked request to wait for the writer"),
            () = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
        assert!(
            !flag.load(Ordering::Acquire),
            "expected write_started: false while another call holds the writer | received: true"
        );

        gate.add_permits(1);
        let answering = link.clone();
        let (answer, ()) = tokio::join!(tracked, async move {
            answering.wait_for_sent(2).await;
            answering.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"held"}"#);
            answering.push_line(r#"{"jsonrpc":"2.0","id":"2","result":"pong"}"#);
        });
        assert!(
            flag.load(Ordering::Acquire),
            "expected write_started: true once the request sent | received: false"
        );
        assert_eq!(answer.expect("expected the scripted answer"), json!("pong"));
        let _ = held.await;
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

    /// Waits for `count` terminations to reach the handler and reports the last count it saw.
    async fn terminations_after(
        handler: &RecordingHandler,
        count: usize,
    ) -> std::result::Result<Vec<PeerTermination>, usize> {
        let mut seen = 0;
        // Paused-time tests advance virtual time here, so the horizon covers a 5 s write deadline.
        for _ in 0..2_000 {
            seen = handler.terminations.lock().await.len();
            if seen >= count {
                return Ok(handler.terminations.lock().await.clone());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Err(seen)
    }

    /// A reply the link refused reached nobody, so the connection cannot be trusted with the next
    /// question. The read side is held open, which is the case the pump alone never notices.
    #[tokio::test]
    async fn a_reply_that_cannot_be_written_ends_the_connection_once() {
        let link = ScriptedLink::new();
        let handler = RecordingHandler::arc(None);
        let client = client(link.clone(), Arc::clone(&handler));
        link.fail_sends("EPIPE");
        link.push_line(r#"{"jsonrpc":"2.0","id":7,"method":"item/requestApproval"}"#);

        let terminations = terminations_after(&handler, 1)
            .await
            .unwrap_or_else(|seen| {
                panic!("expected terminations: 1 after a failed reply write | received: {seen}")
            });
        assert!(
            matches!(&terminations[..], [PeerTermination::LinkFailed(cause)] if cause.contains("EPIPE")),
            "expected one LinkFailed naming EPIPE | received {terminations:?}"
        );
        assert!(
            client.is_closed(),
            "expected closed: true after a failed reply write | received: false"
        );
        let later = client
            .request::<_, Value>("thread/start", json!({}))
            .await
            .expect_err("expected a refusal on a connection whose reply failed");
        assert!(
            matches!(later, Error::Closed { .. }),
            "expected a closed-link refusal for the later request | received {later:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let total = handler.terminations.lock().await.len();
        assert_eq!(
            total, 1,
            "expected exactly one termination | received: {total}"
        );
        client.close().await.expect("expected a clean close");
    }

    /// A request already waiting fails as a link failure the moment a reply fails, rather than at
    /// its own deadline.
    #[tokio::test]
    async fn a_failed_reply_fails_the_requests_still_waiting() {
        let link = ScriptedLink::new();
        let handler = RecordingHandler::arc(None);
        let client = Arc::new(client(link.clone(), Arc::clone(&handler)));
        let waiting = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<_, Value>("thread/start", json!({})).await })
        };
        link.wait_for_sent(1).await;
        link.fail_sends("EPIPE");
        link.push_line(r#"{"jsonrpc":"2.0","id":7,"method":"item/requestApproval"}"#);

        let outcome = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("expected the waiting request to fail promptly after the reply failed")
            .expect("expected the request task to finish");
        let error = outcome.expect_err("expected a link failure, received an answer");
        assert!(
            matches!(&error, Error::Vendor(vendor) if vendor.message.contains("link failed")),
            "expected the waiting request to fail with a link failure | received {error:?}"
        );
    }

    /// A handler whose answers are held until the test lets them go, the way an approval waits on
    /// a person. `asked` counts the questions that reached it.
    struct GatedAnswers {
        asked: std::sync::atomic::AtomicUsize,
        terminated: std::sync::atomic::AtomicUsize,
        release: Notify,
    }

    #[async_trait::async_trait]
    impl PeerHandler for GatedAnswers {
        async fn on_notification(&self, _method: String, _params: Value) {}

        async fn on_request(
            &self,
            _method: String,
            _params: Value,
            _id: RequestId,
        ) -> ServerRequestOutcome {
            self.asked.fetch_add(1, Ordering::AcqRel);
            self.release.notified().await;
            ServerRequestOutcome::Answer(json!({}))
        }

        async fn on_terminated(&self, _termination: PeerTermination) {
            self.terminated.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Two replies failing together while a close races them still end in one clean shutdown: the
    /// reply path only signals, so nothing waits on the task that is waiting on it.
    ///
    /// Both questions are held inside the handler until the replies are about to fail, so the
    /// close arrives while answers are in flight. The first reaches the physical sender and
    /// poisons it; the second settles without another write to that broken sender.
    #[tokio::test]
    async fn failed_replies_racing_a_close_neither_deadlock_nor_terminate_twice() {
        let link = ScriptedLink::new();
        let gated = Arc::new(GatedAnswers {
            asked: std::sync::atomic::AtomicUsize::new(0),
            terminated: std::sync::atomic::AtomicUsize::new(0),
            release: Notify::new(),
        });
        let client = Client::connect(
            link.clone().into_link(),
            Arc::clone(&gated) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server").with_request_timeout(Duration::from_secs(5)),
        );
        link.fail_sends("EPIPE");
        link.push_line(r#"{"jsonrpc":"2.0","id":1,"method":"item/requestApproval"}"#);
        link.push_line(r#"{"jsonrpc":"2.0","id":2,"method":"item/requestApproval"}"#);
        let mut asked = 0;
        for _ in 0..400 {
            asked = gated.asked.load(Ordering::Acquire);
            if asked == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            asked, 2,
            "expected both questions held in the handler | received: {asked}"
        );

        gated.release.notify_waiters();
        let closed = tokio::time::timeout(Duration::from_secs(5), client.close())
            .await
            .expect("expected close to return while replies were failing");
        assert!(
            closed.is_ok(),
            "expected a clean close | received {closed:?}"
        );
        assert_eq!(
            link.refused_sends(),
            1,
            "expected only the first reply to reach the broken physical sender | received: {}",
            link.refused_sends()
        );
        assert!(
            client
                .state
                .in_flight
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        let terminated = gated.terminated.load(Ordering::Acquire);
        assert!(
            terminated <= 1,
            "expected at most one termination for a racing close | received: {terminated}"
        );
    }

    /// Two failed replies with nothing else going on are one termination, not two.
    #[tokio::test]
    async fn two_failed_replies_report_one_termination() {
        let link = ScriptedLink::new();
        let handler = RecordingHandler::arc(None);
        let client = client(link.clone(), Arc::clone(&handler));
        link.fail_sends("EPIPE");
        link.push_line(r#"{"jsonrpc":"2.0","id":1,"method":"item/requestApproval"}"#);
        link.push_line(r#"{"jsonrpc":"2.0","id":2,"method":"item/requestApproval"}"#);

        terminations_after(&handler, 1)
            .await
            .unwrap_or_else(|seen| panic!("expected terminations: 1 | received: {seen}"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        let total = handler.terminations.lock().await.len();
        assert_eq!(
            total, 1,
            "expected exactly one termination | received: {total}"
        );
        client.close().await.expect("expected a clean close");
    }

    /// A stdin that never drains: the write is abandoned at its deadline, and a frame that may be
    /// half on the wire is as unusable as one that failed.
    struct StalledReplies {
        inner: Box<dyn crate::link::LinkSender>,
    }

    #[async_trait::async_trait]
    impl crate::link::LinkSender for StalledReplies {
        async fn send(&mut self, message: String) -> crate::error::Result<()> {
            if message.contains("\"result\"") || message.contains("\"error\"") {
                std::future::pending::<()>().await;
            }
            self.inner.send(message).await
        }

        async fn close(&mut self) -> crate::error::Result<()> {
            self.inner.close().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_reply_write_that_times_out_ends_the_connection_like_one_that_failed() {
        let link = ScriptedLink::new();
        let (sender, receiver) = link.clone().into_link().split();
        let handler = RecordingHandler::arc(None);
        let client = Client::connect(
            crate::link::Link::new(Box::new(StalledReplies { inner: sender }), receiver),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server").with_request_timeout(Duration::from_secs(5)),
        );
        link.push_line(r#"{"jsonrpc":"2.0","id":7,"method":"item/requestApproval"}"#);

        let terminations = terminations_after(&handler, 1)
            .await
            .unwrap_or_else(|seen| {
                panic!("expected terminations: 1 after a reply write timeout | received: {seen}")
            });
        assert!(
            matches!(&terminations[..], [PeerTermination::LinkFailed(cause)] if cause.contains("JSON-RPC frame write")),
            "expected one LinkFailed naming the frame write timeout | received {terminations:?}"
        );
        assert!(
            client.is_closed(),
            "expected closed: true | received: false"
        );
        client.close().await.expect("expected a clean close");
    }

    /// A peer that is alive but no longer draining its input: the one send whose frame names
    /// `method` never completes, so the write is abandoned at its deadline with the frame possibly
    /// half on the wire.
    struct StalledMethod {
        inner: Box<dyn crate::link::LinkSender>,
        method: &'static str,
    }

    #[async_trait::async_trait]
    impl crate::link::LinkSender for StalledMethod {
        async fn send(&mut self, message: String) -> crate::error::Result<()> {
            if message.contains(self.method) {
                std::future::pending::<()>().await;
            }
            self.inner.send(message).await
        }

        async fn close(&mut self) -> crate::error::Result<()> {
            self.inner.close().await
        }
    }

    fn stalled_client(handler: Arc<RecordingHandler>, method: &'static str) -> Client {
        let (sender, receiver) = ScriptedLink::new().into_link().split();
        Client::connect(
            crate::link::Link::new(
                Box::new(StalledMethod {
                    inner: sender,
                    method,
                }),
                receiver,
            ),
            handler as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server").with_request_timeout(Duration::from_secs(5)),
        )
    }

    async fn assert_write_timeout_ended_the_connection(
        handler: &RecordingHandler,
        client: &Client,
    ) {
        let terminations = terminations_after(handler, 1).await.unwrap_or_else(|seen| {
            panic!("expected terminations: 1 after a write timeout | received: {seen}")
        });
        assert!(
            matches!(&terminations[..], [PeerTermination::LinkFailed(cause)] if cause.contains("JSON-RPC frame write")),
            "expected one LinkFailed naming the frame write timeout | received {terminations:?}"
        );
        assert!(
            client.is_closed(),
            "expected closed: true after a write timeout | received: false"
        );
    }

    /// A request whose write times out may have left half a frame on the pipe, so the caller gets
    /// its timeout and the connection ends rather than carrying a corrupt stream.
    #[tokio::test(start_paused = true)]
    async fn a_request_write_that_times_out_on_a_stalled_peer_ends_the_connection() {
        let handler = RecordingHandler::arc(None);
        let client = stalled_client(Arc::clone(&handler), "thread/start");

        let error = client
            .request::<_, Value>("thread/start", json!({}))
            .await
            .expect_err("expected the stalled write to time out");
        assert!(
            matches!(error, Error::Timeout { .. }),
            "expected the caller to receive the write timeout | received {error:?}"
        );
        assert_write_timeout_ended_the_connection(&handler, &client).await;
        client.close().await.expect("expected a clean close");
    }

    #[tokio::test(start_paused = true)]
    async fn a_notification_write_that_times_out_on_a_stalled_peer_ends_the_connection() {
        let handler = RecordingHandler::arc(None);
        let client = stalled_client(Arc::clone(&handler), "turn/interrupt");

        let error = client
            .notify("turn/interrupt", json!({}))
            .await
            .expect_err("expected the stalled write to time out");
        assert!(
            matches!(error, Error::Timeout { .. }),
            "expected the caller to receive the write timeout | received {error:?}"
        );
        assert_write_timeout_ended_the_connection(&handler, &client).await;
        client.close().await.expect("expected a clean close");
    }

    /// A caller's own deadline shorter than the write deadline drops the request mid-send. The
    /// frame may be half on the wire, so the connection ends whichever deadline wins.
    #[tokio::test(start_paused = true)]
    async fn a_caller_deadline_shorter_than_the_write_deadline_still_ends_the_connection() {
        let handler = RecordingHandler::arc(None);
        let client = stalled_client(Arc::clone(&handler), "thread/start");

        let error = client
            .request_with_timeout::<_, Value>("thread/start", json!({}), Duration::from_secs(1))
            .await
            .expect_err("expected the caller's deadline to pass");
        assert!(
            matches!(error, Error::Timeout { .. }),
            "expected the caller to receive its own timeout | received {error:?}"
        );
        let terminations = terminations_after(&handler, 1)
            .await
            .unwrap_or_else(|seen| {
                panic!(
                    "expected terminations: 1 after the caller's deadline dropped a send | received: {seen}"
                )
            });
        assert!(
            matches!(&terminations[..], [PeerTermination::LinkFailed(cause)] if cause.contains("JSON-RPC frame write")),
            "expected one LinkFailed naming the abandoned frame write | received {terminations:?}"
        );
        assert!(
            client.is_closed(),
            "expected closed: true after an abandoned send | received: false"
        );
        client.close().await.expect("expected a clean close");
    }

    /// A caller that drops its request future mid-send leaves the same half frame behind.
    #[tokio::test(start_paused = true)]
    async fn a_request_dropped_mid_send_ends_the_connection() {
        let handler = RecordingHandler::arc(None);
        let client = stalled_client(Arc::clone(&handler), "thread/start");

        tokio::select! {
            _ = client.request::<_, Value>("thread/start", json!({})) => {
                panic!("expected the stalled send to still be pending | received an outcome");
            }
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        let terminations = terminations_after(&handler, 1)
            .await
            .unwrap_or_else(|seen| {
                panic!("expected terminations: 1 after a request was dropped mid-send | received: {seen}")
            });
        assert!(
            matches!(&terminations[..], [PeerTermination::LinkFailed(_)]),
            "expected one LinkFailed | received {terminations:?}"
        );
        assert!(
            client.is_closed(),
            "expected closed: true after an abandoned send | received: false"
        );
        client.close().await.expect("expected a clean close");
    }

    /// A successful send leaves the connection usable.
    #[tokio::test]
    async fn a_send_that_returns_leaves_the_connection_open() {
        let link = ScriptedLink::new();
        let handler = RecordingHandler::arc(None);
        let client = client(link.clone(), Arc::clone(&handler));

        client
            .notify("ping", json!({}))
            .await
            .expect("expected the send to land");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let terminations = handler.terminations.lock().await.len();
        assert_eq!(
            terminations, 0,
            "expected terminations: 0 for sends that returned | received: {terminations}"
        );
        assert!(
            !client.is_closed(),
            "expected closed: false for sends that returned | received: true"
        );
        client.close().await.expect("expected a clean close");
    }

    /// A write already waiting for the sender when another is abandoned mid-frame must not send: it
    /// would append a whole frame behind the half one, and the pump ends the connection later than
    /// the sender is released.
    #[tokio::test(start_paused = true)]
    async fn a_write_queued_behind_an_abandoned_send_never_reaches_the_link() {
        let link = ScriptedLink::new();
        let (sender, receiver) = link.clone().into_link().split();
        let handler = RecordingHandler::arc(None);
        let client = Arc::new(Client::connect(
            crate::link::Link::new(
                Box::new(StalledMethod {
                    inner: sender,
                    method: "thread/start",
                }),
                receiver,
            ),
            Arc::clone(&handler) as Arc<dyn PeerHandler>,
            ClientOptions::new("Codex app-server").with_request_timeout(Duration::from_secs(5)),
        ));
        let stalled = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.request::<_, Value>("thread/start", json!({})).await })
        };
        // The stalled send holds the sender before the second write is issued, and the second
        // write's own deadline is later than the stall's, so it is waiting for the sender when the
        // stall is abandoned.
        tokio::time::sleep(Duration::from_secs(1)).await;
        let queued = {
            let client = Arc::clone(&client);
            tokio::spawn(async move { client.notify("ping", json!({})).await })
        };

        let stalled = stalled
            .await
            .expect("expected the stalled request to finish");
        assert!(
            matches!(stalled, Err(Error::Timeout { .. })),
            "expected the stalled request to time out | received {stalled:?}"
        );
        let queued = queued.await.expect("expected the queued write to finish");
        assert!(
            queued.is_err(),
            "expected the queued write to be refused | received {queued:?}"
        );
        assert!(
            link.sent().is_empty(),
            "expected sent: [] behind an abandoned send | received {:?}",
            link.sent()
        );
        assert_write_timeout_ended_the_connection(&handler, &client).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let terminations = handler.terminations.lock().await.len();
        assert_eq!(
            terminations, 1,
            "expected exactly one termination | received: {terminations}"
        );
        client.close().await.expect("expected a clean close");
    }
}
