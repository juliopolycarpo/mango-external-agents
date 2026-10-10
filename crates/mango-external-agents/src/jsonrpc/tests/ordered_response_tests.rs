//! A request can ask for its answer after the notifications that arrived before it.

use super::*;
use std::sync::{Mutex as StdMutex, OnceLock, PoisonError};

/// Holds each notification at a gate and writes what happened, in order, to one log.
///
/// The requests under test write to the same log when they return, so one list shows whether an
/// answer reached its caller before or after the notifications the peer sent ahead of it. A
/// question from the peer is never answered, the way an approval nobody clicked is not.
struct Turnstile {
    entered: Notify,
    gate: tokio::sync::Semaphore,
    asked: Notify,
    log: StdMutex<Vec<String>>,
    logged: Notify,
}

impl Turnstile {
    fn arc() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            gate: tokio::sync::Semaphore::new(0),
            asked: Notify::new(),
            log: StdMutex::new(Vec::new()),
            logged: Notify::new(),
        })
    }

    /// Waits until a notification is inside the handler, held at the gate. Bounded.
    ///
    /// ```ignore
    /// link.push_line(STARTED);
    /// handler.entered().await;
    /// ```
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("expected a notification to reach the handler within 5s");
    }

    /// Lets `count` held or future notifications through the gate.
    ///
    /// ```ignore
    /// handler.open_for(2);
    /// ```
    fn open_for(&self, count: usize) {
        self.gate.add_permits(count);
    }

    /// Appends one entry to the log.
    ///
    /// ```ignore
    /// handler.note("answer:ok");
    /// ```
    fn note(&self, entry: impl Into<String>) {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(entry.into());
        self.logged.notify_waiters();
    }

    /// Everything logged so far, in order.
    ///
    /// ```ignore
    /// assert!(handler.log().is_empty());
    /// ```
    fn log(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until the log holds `count` entries, then returns it. Bounded, so an entry that
    /// never arrives fails by name instead of hanging.
    ///
    /// ```ignore
    /// handler.open_for(1);
    /// let log = handler.wait_for_log(2).await;
    /// ```
    async fn wait_for_log(&self, count: usize) -> Vec<String> {
        let wait = async {
            loop {
                let logged = self.logged.notified();
                let seen = self.log();
                if seen.len() >= count {
                    return seen;
                }
                logged.await;
            }
        };
        match tokio::time::timeout(Duration::from_secs(5), wait).await {
            Ok(seen) => seen,
            Err(_) => panic!(
                "expected {count} log entries within 5s | received {:?}",
                self.log()
            ),
        }
    }
}

#[async_trait::async_trait]
impl PeerHandler for Turnstile {
    async fn on_notification(&self, method: String, _params: Value) {
        self.entered.notify_one();
        self.gate
            .acquire()
            .await
            .expect("expected the gate to stay open for the whole test")
            .forget();
        self.note(method);
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        self.asked.notify_one();
        std::future::pending::<()>().await;
        unreachable!("the handler never answers")
    }
}

/// What a request under test logs when it returns: the answer, or which failure.
fn describe(outcome: &crate::Result<Value>) -> String {
    match outcome {
        Ok(value) => format!("answer:{value}"),
        Err(Error::Vendor(vendor)) => format!("failed:{}", vendor.message),
        Err(Error::Timeout { .. }) => String::from("failed:timeout"),
        Err(error) => format!("failed:{error}"),
    }
}

fn ordered() -> RequestOptions {
    RequestOptions::new().after_earlier_notifications()
}

fn connect(link: &ScriptedLink, handler: &Arc<Turnstile>, options: ClientOptions) -> Arc<Client> {
    Arc::new(Client::connect(
        link.clone().into_link(),
        Arc::clone(handler) as Arc<dyn PeerHandler>,
        options,
    ))
}

fn options() -> ClientOptions {
    ClientOptions::new("ACP agent")
        .with_request_timeout(Duration::from_secs(5))
        .with_shutdown_timeout_for_tests(Duration::from_secs(30))
}

impl ClientOptions {
    /// A drain grace long enough that a test, not the clock, decides when the drain ends.
    fn with_shutdown_timeout_for_tests(mut self, shutdown_timeout: Duration) -> Self {
        self.shutdown_timeout = shutdown_timeout;
        self
    }
}

/// Makes `method` an ordered request on a task of its own and logs how it returned.
///
/// Returns once the request is on the wire, so the next line a test pushes arrives after it.
async fn ask_ordered(
    client: &Arc<Client>,
    link: &ScriptedLink,
    handler: &Arc<Turnstile>,
    method: &'static str,
    options: RequestOptions,
) -> tokio::task::JoinHandle<crate::Result<Value>> {
    let before = link.sent().len();
    let task = {
        let client = Arc::clone(client);
        let handler = Arc::clone(handler);
        tokio::spawn(async move {
            let outcome = client
                .request_with::<_, Value>(method, json!({}), options)
                .await;
            handler.note(describe(&outcome));
            outcome
        })
    };
    tokio::time::timeout(Duration::from_secs(5), link.wait_for_sent(before + 1))
        .await
        .expect("expected the ordered request on the wire");
    task
}

/// Returns once the reader has dispatched every line pushed before this call.
///
/// The reader takes lines one at a time and settles an ordinary response itself, so the answer to
/// a fresh ordinary request proves everything queued ahead of it has been read. Nothing is slept.
async fn reader_caught_up(client: &Client, link: &ScriptedLink) {
    let before = link.sent().len();
    let peer = async {
        link.wait_for_sent(before + 1).await;
        let frame: Value = serde_json::from_str(&link.sent()[before]).expect("a request frame");
        link.push_line(json!({ "id": frame["id"], "result": "caught-up" }).to_string());
    };
    let (answer, ()) = tokio::join!(client.request::<_, Value>("barrier", json!({})), peer);
    let answer = answer.expect("expected an ordinary request to overtake the held notifications");
    assert_eq!(answer, json!("caught-up"), "received {answer}");
}

/// Waits for the reader to have seen the connection end. Bounded.
async fn connection_ended(client: &Client) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !client.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("expected the reader to notice the connection ending");
}

/// Waits for `count` frames on the wire. Bounded.
async fn sent(link: &ScriptedLink, count: usize) {
    tokio::time::timeout(Duration::from_secs(5), link.wait_for_sent(count))
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected {count} frames written within 5s | received {:?}",
                link.sent()
            )
        });
}

async fn joined(task: tokio::task::JoinHandle<crate::Result<Value>>) -> crate::Result<Value> {
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("expected the ordered request to return within 5s")
        .expect("expected the request task to finish")
}

const STARTED: &str = r#"{"jsonrpc":"2.0","method":"item/started","params":{"n":1}}"#;
const UPDATED: &str = r#"{"jsonrpc":"2.0","method":"item/updated","params":{"n":2}}"#;
const COMPLETED: &str = r#"{"jsonrpc":"2.0","method":"item/completed","params":{"n":3}}"#;
const PONG: &str = r#"{"jsonrpc":"2.0","id":"1","result":"pong"}"#;

#[tokio::test]
async fn an_ordered_response_reaches_its_caller_after_the_notifications_read_before_it() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    link.push_line(UPDATED);
    link.push_line(PONG);
    // An ordinary request still overtakes, and its answer says the reader has read `PONG`.
    reader_caught_up(&client, &link).await;

    let held = handler.log();
    assert!(
        held.is_empty(),
        "expected nothing delivered while the handler held the gate: [] | received {held:?}"
    );

    handler.open_for(2);
    let answer = joined(prompt).await.expect("expected the answer");
    assert_eq!(answer, json!("pong"), "received {answer}");
    let log = handler.log();
    assert_eq!(
        log,
        vec!["item/started", "item/updated", "answer:\"pong\""],
        "expected the answer after both notifications: [item/started, item/updated, answer] | received {log:?}"
    );

    client.close().await.expect("expected a clean close");
}

/// The peer's own question is answered on a task of its own. Only its start is ordered: an
/// approval nobody has clicked must not hold back the answer that ends the turn.
#[tokio::test]
async fn an_ordered_response_does_not_wait_for_a_question_the_peer_asked_earlier() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(r#"{"jsonrpc":"2.0","id":"approval-1","method":"session/request_permission"}"#);
    link.push_line(PONG);

    let answer = joined(prompt).await.expect("expected the answer");
    assert_eq!(answer, json!("pong"), "received {answer}");
    tokio::time::timeout(Duration::from_secs(5), handler.asked.notified())
        .await
        .expect("expected the question ahead of the answer to have reached the handler too");
    assert!(link.sent().len() == 1, "received {:?}", link.sent());

    client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn default_request_options_still_let_a_response_overtake() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let prompt = ask_ordered(&client, &link, &handler, "ping", RequestOptions::new()).await;
    link.push_line(STARTED);
    link.push_line(PONG);

    let answer = joined(prompt).await.expect("expected the answer");
    assert_eq!(answer, json!("pong"), "received {answer}");
    let log = handler.log();
    assert_eq!(
        log,
        vec!["answer:\"pong\""],
        "expected the answer ahead of the held notification: [answer] | received {log:?}"
    );

    client.close().await.expect("expected a clean close");
}

/// The peer's last words are often its answer and then nothing. A response that was read before
/// the end of output must not be reported as an exit because the handler had not caught up.
#[tokio::test]
async fn an_ordered_response_read_before_the_peer_exits_still_reaches_its_caller() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    link.push_line(PONG);
    link.end();
    connection_ended(&client).await;

    let held = handler.log();
    assert!(
        held.is_empty(),
        "expected nothing delivered while the handler held the gate: [] | received {held:?}"
    );

    handler.open_for(1);
    let outcome = joined(prompt).await;
    let log = handler.log();
    assert_eq!(
        log,
        vec!["item/started", "answer:\"pong\""],
        "expected the notification, then the answer read before the exit: [item/started, answer] | received {log:?}"
    );
    assert!(outcome.is_ok(), "received {outcome:?}");
}

#[tokio::test]
async fn an_ordered_request_the_peer_never_answered_fails_after_the_notifications_drain() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    link.end();
    connection_ended(&client).await;

    let held = handler.log();
    assert!(
        held.is_empty(),
        "expected the failure held behind the queued notification: [] | received {held:?}"
    );

    handler.open_for(1);
    let outcome = joined(prompt).await;
    let log = handler.log();
    assert_eq!(
        log,
        vec!["item/started", "failed:the ACP agent exited"],
        "expected the notification, then the exit: [item/started, failed:the ACP agent exited] | received {log:?}"
    );
    assert!(
        matches!(&outcome, Err(Error::Vendor(vendor)) if vendor.vendor_code.as_deref() == Some("-32000")),
        "expected the same -32000 failure an unordered call receives | received {outcome:?}"
    );
}

/// An ordinary request on the same connection is failed at once, as it always was.
#[tokio::test]
async fn an_unordered_request_still_fails_at_once_when_the_peer_exits() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let ordinary = ask_ordered(&client, &link, &handler, "ping", RequestOptions::new()).await;
    link.push_line(STARTED);
    link.end();

    let outcome = joined(ordinary).await;
    let log = handler.log();
    assert_eq!(
        log,
        vec!["failed:the ACP agent exited"],
        "expected the exit ahead of the held notification: [failed:the ACP agent exited] | received {log:?}"
    );
    assert!(outcome.is_err(), "received {outcome:?}");
    handler.open_for(1);
}

#[tokio::test]
async fn an_ordered_response_read_before_the_link_failed_still_reaches_its_caller() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    link.push_line(PONG);
    reader_caught_up(&client, &link).await;
    // The read side stays open; a write that fails is what ends this connection.
    link.fail_sends("EPIPE");
    let refused = client.notify("session/cancel", json!({})).await;
    assert!(
        matches!(refused, Err(Error::Link { .. })),
        "expected the write to fail the link | received {refused:?}"
    );
    connection_ended(&client).await;

    let held = handler.log();
    assert!(
        held.is_empty(),
        "expected nothing delivered while the handler held the gate: [] | received {held:?}"
    );

    handler.open_for(1);
    let outcome = joined(prompt).await;
    let log = handler.log();
    assert_eq!(
        log,
        vec!["item/started", "answer:\"pong\""],
        "expected the notification, then the answer read before the failure: [item/started, answer] | received {log:?}"
    );
    assert!(outcome.is_ok(), "received {outcome:?}");
}

/// The drain is bounded. A handler that never returns cannot keep the caller of an ordered
/// request waiting past the grace: it is told the peer exited, not handed an answer out of order.
#[tokio::test(start_paused = true)]
async fn an_ordered_response_fails_with_the_exit_when_the_drain_runs_out() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let grace = Duration::from_millis(200);
    let client = connect(
        &link,
        &handler,
        ClientOptions::new("ACP agent")
            .with_request_timeout(Duration::from_secs(60))
            .with_shutdown_timeout_for_tests(grace),
    );

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    link.push_line(PONG);
    link.end();
    let began = tokio::time::Instant::now();

    let outcome = joined(prompt).await;
    let log = handler.log();
    assert_eq!(
        log,
        vec!["failed:the ACP agent exited"],
        "expected the exit and no notification past the gate: [failed:the ACP agent exited] | received {log:?}"
    );
    assert!(outcome.is_err(), "received {outcome:?}");
    let waited = began.elapsed();
    assert_eq!(
        waited, grace,
        "expected the caller released when the drain grace ran out: {grace:?} | received {waited:?}"
    );
}

/// Notification pressure must not refuse or drop an answer: one that arrives with the handoff
/// queue and its byte budget both spent still takes its place and is delivered in order.
#[tokio::test]
async fn an_ordered_response_is_queued_when_the_notification_budgets_are_spent() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let mut limited = options().with_max_pending_notifications(1);
    limited.max_pending_bytes = STARTED.len() + UPDATED.len();
    let client = connect(&link, &handler, limited);

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    // The worker holds the first notification at the gate; the second fills the queue of one and
    // the last byte of the payload budget.
    link.push_line(STARTED);
    handler.entered().await;
    link.push_line(UPDATED);
    link.push_line(PONG);
    reader_caught_up(&client, &link).await;
    assert!(
        !client.is_closed(),
        "expected the connection open after an answer met a full queue: open | received closed"
    );

    handler.open_for(2);
    let outcome = joined(prompt).await;
    let log = handler.log();
    assert_eq!(
        log,
        vec!["item/started", "item/updated", "answer:\"pong\""],
        "expected the answer after both notifications: [item/started, item/updated, answer] | received {log:?}"
    );
    assert!(outcome.is_ok(), "received {outcome:?}");

    client.close().await.expect("expected a clean close");
}

/// The deadline keeps running while the answer waits in the queue, and the place it held there
/// stays counted until the handler's task reaches it, so timed-out requests cannot pile answers up
/// behind a handler that is not returning.
#[tokio::test(start_paused = true)]
async fn an_ordered_request_times_out_while_its_answer_waits_and_keeps_its_place_counted() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let mut limited = ClientOptions::new("ACP agent").with_request_timeout(Duration::from_secs(5));
    limited.max_pending_requests = 1;
    let client = connect(&link, &handler, limited);

    let prompt = ask_ordered(
        &client,
        &link,
        &handler,
        "session/prompt",
        ordered().with_timeout(Duration::from_secs(1)),
    )
    .await;
    link.push_line(STARTED);
    link.push_line(PONG);

    let outcome = joined(prompt).await;
    assert!(
        matches!(outcome, Err(Error::Timeout { after, .. }) if after == Duration::from_secs(1)),
        "expected the request to time out behind the held notification: Timeout after 1s | received {outcome:?}"
    );

    // The answer is still queued behind the gate, so its place is still taken.
    let refused = client
        .request_with::<_, Value>("session/prompt", json!({}), ordered())
        .await;
    assert!(
        matches!(
            refused,
            Err(Error::LimitExceeded {
                subject: "ordered JSON-RPC responses awaiting delivery",
                limit: 1,
                received: 2,
            })
        ),
        "expected the queued answer to keep its place: LimitExceeded {{ ordered responses, limit: 1, received: 2 }} | received {refused:?}"
    );
    assert_eq!(link.sent().len(), 1, "received {:?}", link.sent());

    // Once the handler's task has passed the abandoned answer, the place is free again.
    handler.open_for(2);
    link.push_line(UPDATED);
    handler.wait_for_log(3).await;
    let next = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(r#"{"jsonrpc":"2.0","id":"3","result":"second"}"#);
    let answer = joined(next)
        .await
        .expect("expected the freed place to admit a request");
    assert_eq!(answer, json!("second"), "received {answer}");
    let log = handler.log();
    assert_eq!(
        log,
        vec![
            "failed:timeout",
            "item/started",
            "item/updated",
            "answer:\"second\""
        ],
        "expected the abandoned answer delivered to nobody | received {log:?}"
    );

    client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn closing_fails_an_ordered_request_whose_answer_is_still_queued() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    link.push_line(PONG);
    reader_caught_up(&client, &link).await;

    tokio::time::timeout(Duration::from_secs(5), client.close())
        .await
        .expect("expected close to return while an answer waited behind the handler")
        .expect("expected a clean close");

    let outcome = joined(prompt).await;
    let log = handler.log();
    assert_eq!(
        log,
        vec!["failed:the ACP agent connection was closed"],
        "expected the close, not the queued answer: [failed:the ACP agent connection was closed] | received {log:?}"
    );
    assert!(outcome.is_err(), "received {outcome:?}");
}

#[tokio::test]
async fn a_queue_overflow_fails_an_ordered_request_whose_answer_is_still_queued() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options().with_max_pending_notifications(1));

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    handler.entered().await;
    link.push_line(UPDATED);
    link.push_line(PONG);
    // One more than the queue of one holds: the connection ends with the answer still queued.
    link.push_line(COMPLETED);

    let outcome = joined(prompt).await;
    let log = handler.log();
    assert_eq!(
        log,
        vec!["failed:the ACP agent notification queue reached its limit"],
        "expected the overflow, not the queued answer: [failed:the ACP agent notification queue reached its limit] | received {log:?}"
    );
    assert!(outcome.is_err(), "received {outcome:?}");
    assert!(client.is_closed());
}

/// A peer that answers one request twice gets one delivery, and the second copy neither takes a
/// second place in the queue nor disturbs the first.
#[tokio::test]
async fn a_second_response_to_a_queued_ordered_request_is_dropped() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let mut limited = options();
    limited.max_pending_requests = 2;
    let client = connect(&link, &handler, limited);

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"first"}"#);
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","result":"second"}"#);
    link.push_line(r#"{"jsonrpc":"2.0","id":"1","error":{"code":-1,"message":"third"}}"#);
    reader_caught_up(&client, &link).await;

    handler.open_for(1);
    let answer = joined(prompt).await.expect("expected the first answer");
    assert_eq!(
        answer,
        json!("first"),
        "expected the first response to win: \"first\" | received {answer}"
    );
    assert!(!client.is_closed());

    client.close().await.expect("expected a clean close");
}

/// Makes a request from inside the handler and records how it returned.
struct ReentrantHandler {
    client: OnceLock<Arc<Client>>,
    ordered: bool,
    outcomes: StdMutex<Vec<String>>,
    returned: Notify,
}

impl ReentrantHandler {
    fn arc(ordered: bool) -> Arc<Self> {
        Arc::new(Self {
            client: OnceLock::new(),
            ordered,
            outcomes: StdMutex::new(Vec::new()),
            returned: Notify::new(),
        })
    }

    fn connect(self: &Arc<Self>, link: &ScriptedLink) -> Arc<Client> {
        let client = Arc::new(Client::connect(
            link.clone().into_link(),
            Arc::clone(self) as Arc<dyn PeerHandler>,
            ClientOptions::new("ACP agent").with_request_timeout(Duration::from_secs(5)),
        ));
        assert!(self.client.set(Arc::clone(&client)).is_ok());
        client
    }

    async fn ask(&self) -> crate::Result<Value> {
        let client = self.client.get().expect("expected a connected client");
        let options = if self.ordered {
            ordered()
        } else {
            RequestOptions::new()
        };
        let outcome = client
            .request_with::<_, Value>("session/load", json!({}), options)
            .await;
        let described = match &outcome {
            Err(Error::HostConfiguration { .. }) => String::from("refused"),
            other => describe(other),
        };
        self.outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(described);
        self.returned.notify_one();
        outcome
    }

    async fn first_outcome(&self) -> String {
        tokio::time::timeout(Duration::from_secs(60), self.returned.notified())
            .await
            .expect("expected the handler's request to return");
        self.outcomes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .first()
            .cloned()
            .expect("expected one outcome")
    }
}

#[async_trait::async_trait]
impl PeerHandler for ReentrantHandler {
    async fn on_notification(&self, _method: String, _params: Value) {
        let _ = self.ask().await;
    }

    async fn on_request(
        &self,
        _method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        match self.ask().await {
            Ok(value) => ServerRequestOutcome::Answer(value),
            Err(_) => ServerRequestOutcome::Answer(Value::Null),
        }
    }
}

/// The answer would wait for the very call that is waiting for it. Refused before anything is
/// written, so the peer is not left running a request nobody can hear the answer to.
#[tokio::test(start_paused = true)]
async fn an_ordered_request_from_inside_the_notification_handler_is_refused() {
    let link = ScriptedLink::new();
    let handler = ReentrantHandler::arc(true);
    let client = handler.connect(&link);

    link.push_line(STARTED);
    let outcome = handler.first_outcome().await;
    assert_eq!(
        outcome, "refused",
        "expected the request refused as a caller's mistake: refused | received {outcome}"
    );
    let sent = link.sent();
    assert!(
        sent.is_empty(),
        "expected nothing written for a refused request: [] | received {sent:?}"
    );
    assert!(!client.is_closed());

    client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn an_unordered_request_from_inside_the_notification_handler_is_still_answered() {
    let link = ScriptedLink::new();
    let handler = ReentrantHandler::arc(false);
    let client = handler.connect(&link);

    link.push_line(STARTED);
    sent(&link, 1).await;
    link.push_line(PONG);
    let outcome = handler.first_outcome().await;
    assert_eq!(
        outcome, "answer:\"pong\"",
        "expected the handler's ordinary request answered: answer:\"pong\" | received {outcome}"
    );

    client.close().await.expect("expected a clean close");
}

/// A question from the peer is answered on a task of its own, which nothing in the queue waits
/// for, so an ordered request made there is safe and is not mistaken for the hazard.
#[tokio::test]
async fn an_ordered_request_from_inside_a_question_handler_is_answered() {
    let link = ScriptedLink::new();
    let handler = ReentrantHandler::arc(true);
    let client = handler.connect(&link);

    link.push_line(r#"{"jsonrpc":"2.0","id":"q-1","method":"session/request_permission"}"#);
    sent(&link, 1).await;
    link.push_line(PONG);
    let outcome = handler.first_outcome().await;
    assert_eq!(
        outcome, "answer:\"pong\"",
        "expected the question handler's ordered request answered: answer:\"pong\" | received {outcome}"
    );

    client.close().await.expect("expected a clean close");
}

/// Holds a notification at a gate and then panics, the way a handler with a bug does.
struct PanickingHandler {
    entered: Notify,
    gate: tokio::sync::Semaphore,
}

impl PanickingHandler {
    /// Waits until a notification is inside the handler, held at the gate. Bounded.
    ///
    /// ```ignore
    /// link.push_line(STARTED);
    /// handler.entered().await;
    /// ```
    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("expected a notification to reach the handler within 5s");
    }
}

#[async_trait::async_trait]
impl PeerHandler for PanickingHandler {
    async fn on_notification(&self, _method: String, _params: Value) {
        self.entered.notify_one();
        self.gate
            .acquire()
            .await
            .expect("expected the gate to stay open for the whole test")
            .forget();
        panic!("the handler under test panics on purpose");
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

fn panicking_client(link: &ScriptedLink) -> (Arc<PanickingHandler>, Arc<Client>) {
    let handler = Arc::new(PanickingHandler {
        entered: Notify::new(),
        gate: tokio::sync::Semaphore::new(0),
    });
    let client = Arc::new(Client::connect(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("ACP agent").with_request_timeout(Duration::from_secs(60)),
    ));
    (handler, client)
}

fn ask_prompt(client: &Arc<Client>) -> tokio::task::JoinHandle<crate::Result<Value>> {
    let client = Arc::clone(client);
    tokio::spawn(async move {
        client
            .request_with::<_, Value>("session/prompt", json!({}), ordered())
            .await
    })
}

const HANDLER_STOPPED: &str =
    "the ACP agent notification handler stopped before the answer was delivered";

fn assert_failed_because_the_handler_stopped(outcome: &crate::Result<Value>) {
    assert!(
        matches!(outcome, Err(Error::Vendor(vendor)) if vendor.message == HANDLER_STOPPED),
        "expected the caller told the handler stopped: {HANDLER_STOPPED} | received {}",
        describe(outcome)
    );
}

/// The handler's task is the only thing that delivers an ordered answer. When a panic takes it
/// down with the answer queued, the caller is told then, not at the end of its deadline.
#[tokio::test(start_paused = true)]
async fn an_ordered_answer_queued_behind_a_handler_that_panics_fails_at_once() {
    let link = ScriptedLink::new();
    let (handler, client) = panicking_client(&link);

    let prompt = ask_prompt(&client);
    sent(&link, 1).await;
    link.push_line(STARTED);
    handler.entered().await;
    link.push_line(PONG);
    reader_caught_up(&client, &link).await;
    let began = tokio::time::Instant::now();
    handler.gate.add_permits(1);

    let outcome = joined(prompt).await;
    assert_failed_because_the_handler_stopped(&outcome);
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::ZERO,
        "expected no wait for the 60s deadline: 0s | received {waited:?}"
    );
}

/// The same when the answer is read after the panic: there is no queue left to put it in.
#[tokio::test(start_paused = true)]
async fn an_ordered_answer_read_after_the_handler_panicked_fails_at_once() {
    let link = ScriptedLink::new();
    let (handler, client) = panicking_client(&link);

    let prompt = ask_prompt(&client);
    sent(&link, 1).await;
    link.push_line(STARTED);
    handler.entered().await;
    handler.gate.add_permits(1);
    // The worker's task has ended, and its end of the queue with it, once this is true.
    // Counted, not timed: a task that keeps yielding never lets a paused clock advance.
    let worker_ended = || {
        client
            .state
            .notifications
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    };
    let mut yields = 0;
    while !worker_ended() {
        yields += 1;
        assert!(
            yields < 10_000,
            "expected the handler's task to end with its panic: ended | received still running after {yields} yields"
        );
        tokio::task::yield_now().await;
    }
    let began = tokio::time::Instant::now();
    link.push_line(PONG);

    let outcome = joined(prompt).await;
    assert_failed_because_the_handler_stopped(&outcome);
    let waited = began.elapsed();
    assert_eq!(
        waited,
        Duration::ZERO,
        "expected no wait for the 60s deadline: 0s | received {waited:?}"
    );
}

/// Waits until the reader's drain has taken the worker's handle, which is when a close or a drop
/// can no longer reach the worker through it. Counted, so it also holds on a paused clock.
async fn drain_owns_the_worker(client: &Client) {
    let mut yields = 0;
    while client
        .state
        .notifications
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
    {
        yields += 1;
        assert!(
            yields < 10_000,
            "expected the drain to have taken the worker: taken | received still held after {yields} yields"
        );
        tokio::task::yield_now().await;
    }
}

/// Carries frames and never finishes closing, so a `Client::close` spends its whole grace there.
struct NeverCloses(Box<dyn crate::link::LinkSender>);

#[async_trait::async_trait]
impl crate::link::LinkSender for NeverCloses {
    async fn send(&mut self, message: String) -> crate::Result<()> {
        self.0.send(message).await
    }

    async fn close(&mut self) -> crate::Result<()> {
        std::future::pending().await
    }
}

/// The reader drains the queue when the peer exits, and takes the worker's handle to wait on it.
/// A close that lands in that drain must still stop the worker, and first: an answer queued
/// behind a handler that is not returning is failed when the close begins, not after the stages
/// of the close that can each take the shutdown grace.
#[tokio::test(start_paused = true)]
async fn closing_during_the_exit_drain_fails_a_queued_ordered_answer_at_once() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let grace = Duration::from_secs(30);
    let (sender, receiver) = link.clone().into_link().split();
    let client = Arc::new(Client::connect(
        crate::link::Link::new(Box::new(NeverCloses(sender)), receiver),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        ClientOptions::new("ACP agent")
            .with_request_timeout(Duration::from_secs(600))
            .with_shutdown_timeout_for_tests(grace),
    ));

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    let released = tokio::spawn(async move { (prompt.await, tokio::time::Instant::now()) });
    link.push_line(STARTED);
    link.push_line(PONG);
    link.end();
    drain_owns_the_worker(&client).await;

    let began = tokio::time::Instant::now();
    let closed = client.close().await;
    assert!(
        matches!(closed, Err(Error::Timeout { after, .. }) if after == grace),
        "expected the close to spend its grace on the link: Timeout after {grace:?} | received {closed:?}"
    );

    let (outcome, at) = released.await.expect("expected the request task to finish");
    let waited = at - began;
    assert_eq!(
        waited,
        Duration::ZERO,
        "expected the caller released as the close began: 0s | received {waited:?}"
    );
    let log = handler.log();
    assert_eq!(
        log,
        vec!["failed:the ACP agent exited"],
        "expected the exit that began the drain, and no notification past the gate: [failed:the ACP agent exited] | received {log:?}"
    );
    assert!(
        matches!(outcome, Ok(Err(_))),
        "expected the request failed | received {outcome:?}"
    );
}

/// What the default path does with the same close: the notifications still queued when the peer
/// exited are not handled once the client is closed. The drain is for a peer that went away
/// under a host still listening, and a host that closes has stopped.
#[tokio::test]
async fn closing_during_the_exit_drain_cuts_the_queued_notifications() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    link.push_line(STARTED);
    link.push_line(UPDATED);
    link.end();
    handler.entered().await;
    drain_owns_the_worker(&client).await;

    tokio::time::timeout(Duration::from_secs(5), client.close())
        .await
        .expect("expected close to return during the drain")
        .expect("expected a clean close");

    // The reader and the worker each held the handler; both are gone when only this test does.
    let mut yields = 0;
    while Arc::strong_count(&handler) > 1 {
        yields += 1;
        assert!(
            yields < 10_000,
            "expected the handler's task taken down by the close: 1 owner | received {} after {yields} yields",
            Arc::strong_count(&handler)
        );
        tokio::task::yield_now().await;
    }
    handler.open_for(2);
    let log = handler.log();
    assert!(
        log.is_empty(),
        "expected neither queued notification handled after the close: [] | received {log:?}"
    );
}

/// A caller that gives up before the peer answers gives its place back then, and the answer that
/// arrives afterwards finds nobody: it is dropped without taking a place or releasing one twice.
#[tokio::test(start_paused = true)]
async fn an_ordered_request_that_times_out_unanswered_frees_its_place_and_a_late_answer_is_dropped()
{
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let mut limited = ClientOptions::new("ACP agent").with_request_timeout(Duration::from_secs(5));
    limited.max_pending_requests = 1;
    let client = connect(&link, &handler, limited);
    let places = || client.state.ordered_places.available_permits();

    let prompt = ask_ordered(
        &client,
        &link,
        &handler,
        "session/prompt",
        ordered().with_timeout(Duration::from_secs(1)),
    )
    .await;
    let outcome = joined(prompt).await;
    assert!(
        matches!(outcome, Err(Error::Timeout { .. })),
        "expected the unanswered request to time out: Timeout | received {outcome:?}"
    );
    let free = places();
    assert_eq!(
        free, 1,
        "expected the place given back with the timeout: 1 free | received {free}"
    );

    // The one place admits the next request. The late answer to the first is read ahead of the
    // answer to the second, so by the time the second returns the first has been dropped.
    let next = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(PONG);
    link.push_line(r#"{"jsonrpc":"2.0","id":"2","result":"second"}"#);
    let answer = joined(next).await.expect("expected the second answer");
    assert_eq!(
        answer,
        json!("second"),
        "expected the second request's own answer: \"second\" | received {answer}"
    );
    let free = places();
    assert_eq!(
        free, 1,
        "expected one place free after a late answer and a delivered one: 1 free | received {free}"
    );
    assert!(!client.is_closed());

    client.close().await.expect("expected a clean close");
}

#[tokio::test]
async fn an_ordered_request_the_peer_never_answered_fails_after_the_drain_when_the_link_fails() {
    let link = ScriptedLink::new();
    let handler = Turnstile::arc();
    let client = connect(&link, &handler, options());

    let prompt = ask_ordered(&client, &link, &handler, "session/prompt", ordered()).await;
    link.push_line(STARTED);
    handler.entered().await;
    link.fail_sends("EPIPE");
    let refused = client.notify("session/cancel", json!({})).await;
    assert!(
        matches!(refused, Err(Error::Link { .. })),
        "expected the write to fail the link | received {refused:?}"
    );
    connection_ended(&client).await;

    let held = handler.log();
    assert!(
        held.is_empty(),
        "expected the failure held behind the queued notification: [] | received {held:?}"
    );

    handler.open_for(1);
    let outcome = joined(prompt).await;
    let log = handler.log();
    assert!(
        matches!(
            &log[..],
            [first, second] if first == "item/started"
                && second.starts_with("failed:the ACP agent link failed: ")
        ),
        "expected the notification, then the link failure: [item/started, failed:the ACP agent link failed: ...] | received {log:?}"
    );
    assert!(
        matches!(&outcome, Err(Error::Vendor(vendor)) if vendor.vendor_code.as_deref() == Some("-32000")),
        "expected the same -32000 failure an unordered call receives | received {outcome:?}"
    );
}

/// A reply failed because the handler's task stopped says exactly that, and does not claim the
/// connection ended: the client reports no end.
#[tokio::test(start_paused = true)]
async fn a_reply_failed_by_a_stopped_handler_does_not_say_the_connection_ended() {
    use crate::jsonrpc::CallFailureCause;

    let link = ScriptedLink::new();
    let (handler, client) = panicking_client(&link);
    let (written, reply) = client
        .submit_request("session/prompt", json!({}), ordered())
        .expect("expected the request queued")
        .into_parts();
    written.await.expect("expected the request written");
    link.push_line(STARTED);
    handler.entered().await;
    link.push_line(PONG);
    reader_caught_up(&client, &link).await;
    handler.gate.add_permits(1);

    let failure = reply.await.expect_err("expected the reply to fail");
    assert_eq!(
        failure.cause(),
        &CallFailureCause::HandlerStopped,
        "expected the cause to be the stopped handler | received {:?}",
        failure.cause()
    );
    let ended = client.ended();
    assert_eq!(
        ended, None,
        "expected no end on record for a connection that did not end: None | received {ended:?}"
    );
    let error = failure.into_error();
    assert!(
        matches!(&error, Error::Vendor(vendor) if vendor.message == HANDLER_STOPPED),
        "expected the error `request` returns for it: {HANDLER_STOPPED} | received {error:?}"
    );
}
