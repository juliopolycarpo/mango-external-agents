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
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

use crate::error::{Error, ErrorCode, Result, VendorError, jsonrpc_code_is_retryable};
use crate::host::CancelToken;
use crate::link::{Link, LinkSender};

/// One request's id, exactly as it arrived.
///
/// Kept as raw JSON rather than normalised to a string, and that is load-bearing rather than tidy:
/// ids may be strings or numbers, and some agents number their requests from zero. Replying to
/// request `0` with `"0"` is a different id, so the peer never matches the answer to the question
/// and blocks forever — which presents as a turn that renders an approval, accepts a click, and
/// then simply never finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestId(Value);

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
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.key())
    }
}

/// A JSON-RPC error body.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    /// The peer's code.
    pub code: i64,
    /// The peer's message.
    pub message: String,
    /// Whatever else the peer attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerRequestOutcome {
    /// An answer.
    Answer(Value),
    /// A refusal, in the protocol's own shape.
    Failure(JsonRpcError),
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
}

/// How this client speaks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientOptions {
    /// The peer as a person would name it, such as `Codex app-server`.
    pub peer_name: String,
    /// Whether to write the `"jsonrpc": "2.0"` member.
    ///
    /// Not every dialect this library drives writes it, and a peer that validates strictly will
    /// refuse a frame carrying a member its own schema does not have.
    pub include_version_header: bool,
    /// How long a request waits before it is a failure.
    pub request_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self {
            peer_name: String::from("external agent"),
            include_version_header: true,
            request_timeout: Duration::from_secs(120),
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

    /// Omits the `"jsonrpc"` member, for a dialect that does not write one.
    #[must_use]
    pub fn without_version_header(mut self) -> Self {
        self.include_version_header = false;
        self
    }

    /// Waits this long for an answer.
    #[must_use]
    pub fn with_request_timeout(mut self, request_timeout: Duration) -> Self {
        self.request_timeout = request_timeout;
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
    options: ClientOptions,
    next_id: AtomicU64,
    closed: AtomicBool,
    shutdown: CancelToken,
}

impl Client {
    /// Starts speaking, pumping the link on a task of its own.
    ///
    /// The pump stops when the peer goes away, when [`Client::close`] is called, or when the
    /// client is dropped.
    pub fn connect(link: Link, handler: Arc<dyn PeerHandler>, options: ClientOptions) -> Self {
        let (sender, receiver) = link.split();
        let state = Arc::new(ClientState {
            sender: Mutex::new(sender),
            pending: Mutex::new(HashMap::new()),
            options,
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
            shutdown: CancelToken::new(),
        });
        let pump = tokio::spawn(pump(Arc::clone(&state), receiver, handler));
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
        let answer = self.call(method, params, timeout).await?;
        serde_json::from_value(answer).map_err(|error| Error::Protocol {
            expected: format!("a result {method} could answer with"),
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
        self.state.closed.store(true, Ordering::Release);
        self.state.shutdown.cancel();
        self.state
            .fail_pending(JsonRpcError {
                code: -32000,
                message: format!("the {} connection was closed", self.state.options.peer_name),
                data: None,
            })
            .await;

        let closed = self.state.sender.lock().await.close().await;
        if let Some(pump) = self.pump.lock().await.take() {
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
        let (answer, waiting) = oneshot::channel();
        self.state.pending.lock().await.insert(id.clone(), answer);

        let frame = self
            .state
            .frame(Some(Value::String(id.clone())), method, params)?;
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
                ErrorCode::new(format!("{}-call-failed", self.state.slug())),
                Some(id),
            ))),
            Ok(Err(_)) => Err(Error::Link {
                peer: self.state.options.peer_name.clone(),
                message: format!("the peer went away before answering {method}"),
            }),
            Err(_) => {
                self.state.pending.lock().await.remove(&id);
                Err(Error::Timeout {
                    operation: format!("{} {method}", self.state.options.peer_name),
                    after: timeout,
                })
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.state.closed.store(true, Ordering::Release);
        self.state.shutdown.cancel();
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
            expected: format!("serialisable params for {method}"),
            received: error.to_string(),
        })?;
        // A dialect that validates strictly refuses a `params: null` it never declared, so an
        // absent parameter is an absent member.
        if !params.is_null() {
            frame.insert(String::from("params"), params);
        }
        Ok(Value::Object(frame).to_string())
    }

    async fn write(&self, frame: String) -> Result<()> {
        self.sender.lock().await.send(frame).await
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

    /// The peer's name as an error-code prefix.
    fn slug(&self) -> String {
        self.options
            .peer_name
            .to_lowercase()
            .split_whitespace()
            .next()
            .unwrap_or("peer")
            .to_owned()
    }
}

async fn pump(
    state: Arc<ClientState>,
    mut receiver: Box<dyn crate::link::LinkReceiver>,
    handler: Arc<dyn PeerHandler>,
) {
    loop {
        let message = tokio::select! {
            biased;
            () = state.shutdown.cancelled() => break,
            message = receiver.recv() => message,
        };

        match message {
            Ok(Some(message)) => dispatch(&state, &handler, message).await,
            Ok(None) => {
                state.closed.store(true, Ordering::Release);
                state
                    .fail_pending(JsonRpcError {
                        code: -32000,
                        message: format!("the {} exited", state.options.peer_name),
                        data: None,
                    })
                    .await;
                break;
            }
            Err(error) => {
                state.closed.store(true, Ordering::Release);
                state
                    .fail_pending(JsonRpcError {
                        code: -32000,
                        message: format!("the {} link failed: {error}", state.options.peer_name),
                        data: None,
                    })
                    .await;
                break;
            }
        }
    }
}

async fn dispatch(state: &Arc<ClientState>, handler: &Arc<dyn PeerHandler>, message: String) {
    // Not every line on a peer's output is a frame. Dropping an unparseable one keeps a stray
    // diagnostic from killing a live turn.
    let Ok(Value::Object(frame)) = serde_json::from_str::<Value>(&message) else {
        return;
    };

    let id = frame.get("id").cloned();
    let method = frame
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let params = frame.get("params").cloned().unwrap_or(Value::Null);

    match (method, id) {
        (Some(method), None) => handler.on_notification(method, params).await,
        (Some(method), Some(id)) => {
            // On a task of its own: a question that waits for a person must not stop the events
            // arriving meanwhile, which is what a turn renders while the person decides.
            let state = Arc::clone(state);
            let handler = Arc::clone(handler);
            tokio::spawn(async move { answer(&state, &handler, method, params, id).await });
        }
        (None, Some(id)) => {
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
        (None, None) => {}
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
    let outcome = handler.on_request(method, params, id).await;

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
        Client, ClientOptions, JsonRpcError, PeerHandler, RequestId, ServerRequestOutcome,
    };
    use crate::error::Error;
    use crate::testing::ScriptedLink;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;

    /// A handler that records what it was told and answers questions from a script.
    struct RecordingHandler {
        notifications: Mutex<Vec<(String, Value)>>,
        answer: Option<ServerRequestOutcome>,
    }

    impl RecordingHandler {
        fn arc(answer: Option<ServerRequestOutcome>) -> Arc<Self> {
            Arc::new(Self {
                notifications: Mutex::new(Vec::new()),
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
    }

    fn client(link: ScriptedLink, handler: Arc<RecordingHandler>) -> Client {
        Client::connect(
            link.into_link(),
            handler,
            ClientOptions::new("Codex app-server").with_request_timeout(Duration::from_secs(5)),
        )
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
            error.to_string().contains("exited"),
            "expected the peer's exit, received {error}"
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
}
