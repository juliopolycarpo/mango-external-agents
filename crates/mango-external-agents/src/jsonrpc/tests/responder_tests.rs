//! The peer's questions handed to the handler in order, each with the `Responder` that answers
//! it: on the spot, later from another task, past the cap, too large, or not at all.

use super::outbox_tests::{limits, options, ordered_options, sent, until, within};
use super::*;
use crate::jsonrpc::{ConnectionEnd, Responder, WireOptions, Written};

/// What the fake does with a question, by the method the peer names.
const PARK: &str = "park";
const WAIT_THEN_PARK: &str = "wait-then-park";
const LARGE: &str = "large";
const LET_GO: &str = "let-go";
const ASK_BACK: &str = "ask-back";
const ASK_PLAIN: &str = "ask-plain";
const CLOSE_INSIDE: &str = "close-inside";

/// A handler that takes questions with their responders. It keeps the ones named `park`,
/// waits at a gate before keeping `wait-then-park`, answers `large` with more than fits and
/// then with less, drops `let-go`, and answers anything else `-32601` on the spot.
struct ParkingHandler {
    /// Every call made, in order: `handoff:<method>`, `request:<method>`, `past-cap:<method>`,
    /// `notification:<method>`.
    calls: std::sync::Mutex<Vec<String>>,
    parked: std::sync::Mutex<Vec<Responder>>,
    gate: tokio::sync::Semaphore,
    /// What `answer` said to the answer that was too large.
    too_large: std::sync::Mutex<Vec<(bool, String)>>,
    terminations: std::sync::Mutex<Vec<PeerTermination>>,
    /// The client this handles for, for the question that asks the peer something back.
    client: std::sync::OnceLock<std::sync::Weak<Client>>,
    /// What an ordered request and a hold came to when asked for inside a hand-off.
    asked_back: std::sync::Mutex<Vec<(String, bool)>>,
}

impl ParkingHandler {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            client: std::sync::OnceLock::new(),
            asked_back: std::sync::Mutex::new(Vec::new()),
            calls: std::sync::Mutex::new(Vec::new()),
            parked: std::sync::Mutex::new(Vec::new()),
            gate: tokio::sync::Semaphore::new(0),
            too_large: std::sync::Mutex::new(Vec::new()),
            terminations: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn record(&self, call: String) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn parked(&self) -> usize {
        self.parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// The responder parked first, taken out.
    fn take(&self) -> Responder {
        self.parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(0)
    }

    fn park(&self, responder: Responder) {
        self.parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(responder);
    }
}

fn method_not_found() -> ServerRequestOutcome {
    ServerRequestOutcome::Failure(JsonRpcError {
        code: -32601,
        message: String::from("Method not found"),
        data: None,
    })
}

#[async_trait::async_trait]
impl PeerHandler for ParkingHandler {
    async fn on_notification(&self, method: String, _params: Value) {
        self.record(format!("notification:{method}"));
    }

    async fn on_request(
        &self,
        method: String,
        _params: Value,
        _id: RequestId,
    ) -> ServerRequestOutcome {
        self.record(format!("request:{method}"));
        ServerRequestOutcome::Answer(json!("from on_request"))
    }

    async fn on_request_handoff(&self, method: String, _params: Value, responder: Responder) {
        self.record(format!("handoff:{method}"));
        match method.as_str() {
            PARK => self.park(responder),
            WAIT_THEN_PARK => {
                if let Ok(permit) = self.gate.acquire().await {
                    permit.forget();
                }
                self.park(responder);
            }
            LARGE => {
                let large = ServerRequestOutcome::Answer(json!("x".repeat(400)));
                let unanswered = responder
                    .answer(large)
                    .expect_err("expected the large answer refused");
                self.too_large
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((unanswered.is_too_large(), unanswered.error().to_string()));
                let (responder, _error) = unanswered.into_parts();
                let _ = responder.answer(ServerRequestOutcome::Answer(json!("small")));
            }
            LET_GO => drop(responder),
            ASK_PLAIN => {
                let client = self
                    .client
                    .get()
                    .and_then(std::sync::Weak::upgrade)
                    .expect("expected the client still there");
                let got = client.request::<_, Value>("back", json!({})).await;
                let _ = responder.answer(ServerRequestOutcome::Answer(json!({ "got": got.ok() })));
            }
            CLOSE_INSIDE => {
                let client = self
                    .client
                    .get()
                    .and_then(std::sync::Weak::upgrade)
                    .expect("expected the client still there");
                self.record(String::from("closing"));
                let _ = client.close().await;
                self.record(String::from("close returned"));
                drop(responder);
            }
            ASK_BACK => {
                let client = self
                    .client
                    .get()
                    .and_then(std::sync::Weak::upgrade)
                    .expect("expected the client still there");
                let asked = client
                    .request_with::<_, Value>("back", json!({}), ordered_options())
                    .await;
                let holding = client.hold_cancellation().await.is_holding();
                let queued = client.submit_request("back", json!({}), ordered_options());
                let refusal = match (&asked, queued.as_ref().map(|_| ())) {
                    (
                        Err(Error::HostConfiguration { received, .. }),
                        Err(Error::HostConfiguration { .. }),
                    ) => received.clone(),
                    other => format!("{other:?}"),
                };
                self.asked_back
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((refusal, holding));
                let _ = responder.answer(ServerRequestOutcome::Answer(json!("asked")));
            }
            _ => {
                let _ = responder.answer(method_not_found());
            }
        }
    }

    fn on_request_past_cap(
        &self,
        method: &str,
        _params: &Value,
        _id: &RequestId,
    ) -> Option<ServerRequestOutcome> {
        self.record(format!("past-cap:{method}"));
        (method != "refuse")
            .then(|| ServerRequestOutcome::Answer(json!({ "outcome": "cancelled" })))
    }

    async fn on_terminated(&self, termination: PeerTermination) {
        self.terminations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(termination);
    }
}

/// A client over a link whose sends of frames naming `hold` wait at a gate.
struct Connected {
    link: ScriptedLink,
    gate: Arc<tokio::sync::Semaphore>,
    handler: Arc<ParkingHandler>,
    client: Arc<Client>,
}

fn connect(options: ClientOptions, wire: WireOptions) -> Connected {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let handler = ParkingHandler::new();
    let client = Arc::new(Client::connect_with(
        crate::link::Link::new(
            Box::new(GatedSender {
                inner: sender,
                method: "hold",
                gate: Arc::clone(&gate),
            }),
            receiver,
        ),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options,
        wire,
    ));
    let _ = handler.client.set(Arc::downgrade(&client));
    Connected {
        link,
        gate,
        handler,
        client,
    }
}

fn handoff() -> WireOptions {
    WireOptions::new().with_request_handoff()
}

fn question(id: u64, method: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": {} }).to_string()
}

/// Each frame on the wire, as the method it names or the id it answers.
fn wire(link: &ScriptedLink) -> Vec<String> {
    link.sent()
        .iter()
        .map(|frame| {
            let frame: Value = serde_json::from_str(frame).expect("expected a JSON frame");
            match frame["method"].as_str() {
                Some(method) => method.to_owned(),
                None if frame.get("result").is_some() => format!("result:{}", frame["id"]),
                None => format!("error:{}:{}", frame["id"], frame["error"]["code"]),
            }
        })
        .collect()
}

/// Has the writer inside a send, so everything queued next waits behind it.
async fn hold_the_link(connected: &Connected) -> Written {
    let held = connected
        .client
        .submit_notification("hold", json!({}))
        .expect("expected the held frame queued");
    until("the writer to take the held frame", || held.started()).await;
    held
}

fn slots(client: &Client) -> usize {
    client.state.handoff_slots.available_permits()
}

/// A client that was not asked hands nothing over: the question goes to `on_request` on a task
/// of its own, as it always did.
#[tokio::test]
async fn a_question_is_not_handed_over_by_a_client_that_was_not_asked_to() {
    let connected = connect(options(), WireOptions::new());
    connected.link.push_line(question(1, PARK));

    sent(&connected.link, 1).await;
    let observed = (
        connected.handler.calls(),
        connected.link.sent(),
        connected.handler.parked(),
    );
    let expected = (
        vec![String::from("request:park")],
        vec![json!({ "jsonrpc": "2.0", "id": 1, "result": "from on_request" }).to_string()],
        0,
    );
    assert_eq!(
        observed, expected,
        "expected the question answered by on_request and nothing handed over: {expected:?} | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// A question kept by the handler is answered later, from another task, and the answer is in
/// the queue by the time `answer` returns: a notification queued straight after it reaches
/// the peer after it, even with the link busy the whole time.
#[tokio::test]
async fn an_answer_given_later_is_queued_ahead_of_what_is_queued_after_it() {
    let connected = connect(options(), handoff());
    connected.link.push_line(question(9001, PARK));
    until("the question to be parked", || {
        connected.handler.parked() == 1
    })
    .await;
    let _held = hold_the_link(&connected).await;

    let responder = connected.handler.take();
    let client = Arc::clone(&connected.client);
    let (answered, cancelled) = within(
        "the answering task",
        tokio::spawn(async move {
            // Back to back, with nothing awaited between them.
            let answered = responder.answer(ServerRequestOutcome::Answer(json!("cancelled")));
            let cancelled = client.submit_notification("session/cancel", json!({}));
            (answered, cancelled)
        }),
    )
    .await
    .expect("expected the answering task to finish");
    let answered = answered.expect("expected the answer queued");
    let cancelled = cancelled.expect("expected the notification queued");
    connected.gate.add_permits(1);
    within("the answer written", answered)
        .await
        .expect("expected the answer written");
    within("the notification written", cancelled)
        .await
        .expect("expected the notification written");

    let written = wire(&connected.link);
    assert_eq!(
        written,
        vec!["hold", "result:9001", "session/cancel"],
        "expected the answer ahead of the notification queued after it: [hold, result:9001, session/cancel] | received {written:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// Answers given on the spot are frames in the outbound queue like any other. With the peer
/// not reading and the queue bounded, the answer that does not fit ends the connection with
/// the outbound budget, where it would otherwise wait out a write deadline each.
#[tokio::test]
async fn answers_given_on_the_spot_count_against_the_outbound_queue() {
    let connected = connect(
        options(),
        limits(usize::MAX, usize::MAX, 3).with_request_handoff(),
    );
    let _held = hold_the_link(&connected).await;
    for id in 1..=3 {
        connected.link.push_line(question(id, "unknown"));
    }

    let termination = within("the termination", async {
        loop {
            let seen = connected
                .handler
                .terminations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .first()
                .cloned();
            if let Some(termination) = seen {
                break termination;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(
        matches!(
            termination,
            PeerTermination::OutboundBackpressure {
                limit: 3,
                received: 4,
                ..
            }
        ),
        "expected the third answer to pass a queue of three with the held frame in it: OutboundBackpressure {{ limit: 3, received: 4, .. }} | received {termination:?}"
    );
    connected.gate.add_permits(1);
    let _ = connected.client.close().await;
}

/// A response asked for after earlier peer work waits for a question read before it to be
/// handed over, as it waits for a notification: it is delivered once the hand-off call has
/// returned, and not while the handler is still inside it.
#[tokio::test]
async fn an_ordered_response_waits_for_the_question_before_it_to_be_handed_over() {
    let connected = connect(options(), handoff());
    let (written, reply) = connected
        .client
        .submit_request("session/prompt", json!({}), ordered_options())
        .expect("expected the request queued")
        .into_parts();
    within("the request written", written)
        .await
        .expect("expected the request written");
    connected.link.push_line(question(7, WAIT_THEN_PARK));
    connected
        .link
        .push_line(r#"{"jsonrpc":"2.0","id":"1","result":"done"}"#);
    until("the response to be queued behind the hand-off", || {
        connected.handler.calls() == ["handoff:wait-then-park"]
            && connected
                .client
                .state
                .queued_answers
                .load(Ordering::Acquire)
                == 1
    })
    .await;
    let mut reply = Box::pin(reply);
    let early =
        std::future::poll_fn(|cx| std::task::Poll::Ready(reply.as_mut().poll(cx).is_ready())).await;
    let observed = (early, connected.handler.parked());
    assert_eq!(
        observed,
        (false, 0),
        "expected the response held back while the question is being handed over: (false, 0) | received {observed:?}"
    );

    connected.handler.gate.add_permits(1);
    let answer = within("the reply", reply).await;
    let observed = (answer.ok(), connected.handler.parked());
    assert_eq!(
        observed,
        (Some(json!("done")), 1),
        "expected the response once the question was parked: (Some(\"done\"), 1) | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// A question past the cap is not handed over. The handler is asked on the spot what to
/// answer, and may answer with a result; saying nothing leaves this client's own refusal.
/// The question that holds the place keeps it, and the next one is handed over once the
/// place is free.
#[tokio::test]
async fn a_question_past_the_cap_is_answered_on_the_spot_by_the_handler() {
    let connected = connect(options().with_max_in_flight_requests(1), handoff());
    connected.link.push_line(question(1, PARK));
    connected.link.push_line(question(2, "ask"));
    connected.link.push_line(question(3, "refuse"));

    sent(&connected.link, 2).await;
    let observed = (
        connected.handler.calls(),
        connected.link.sent(),
        connected.handler.parked(),
        slots(&connected.client),
    );
    let expected = (
        vec![
            String::from("handoff:park"),
            String::from("past-cap:ask"),
            String::from("past-cap:refuse"),
        ],
        vec![
            json!({ "jsonrpc": "2.0", "id": 2, "result": { "outcome": "cancelled" } }).to_string(),
            json!({
                "jsonrpc": "2.0",
                "id": 3,
                "error": {
                    "code": -32000,
                    "message": "expected at most 1 questions in flight, received one more",
                },
            })
            .to_string(),
        ],
        1,
        0,
    );
    assert_eq!(
        observed, expected,
        "expected the first question kept and the two past the cap answered at once, by the handler and by default: {expected:?} | received {observed:?}"
    );

    let answered = connected
        .handler
        .take()
        .answer(ServerRequestOutcome::Answer(json!("first")));
    within(
        "the first answer written",
        answered.expect("expected the answer queued"),
    )
    .await
    .expect("expected the answer written");
    connected.link.push_line(question(4, PARK));
    until("the next question to be parked", || {
        connected.handler.parked() == 1
    })
    .await;
    let calls = connected.handler.calls();
    assert_eq!(
        calls.last().map(String::as_str),
        Some("handoff:park"),
        "expected the question after the place was freed handed over: handoff:park | received {calls:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// An answer larger than a frame may be is refused where it is given. The responder comes
/// back, the connection is as it was, and a smaller answer through it reaches the peer.
#[tokio::test]
async fn an_answer_too_large_hands_the_responder_back_for_a_smaller_one() {
    let connected = connect(
        options(),
        limits(200, usize::MAX, usize::MAX).with_request_handoff(),
    );
    connected.link.push_line(question(5, LARGE));

    sent(&connected.link, 1).await;
    let refusals = connected
        .handler
        .too_large
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let named_the_bound = refusals
        .first()
        .is_some_and(|(_, error)| error.contains("bytes of one outgoing JSON-RPC frame"));
    let observed = (
        refusals.first().map(|(too_large, _)| *too_large),
        named_the_bound,
        connected.link.sent(),
        connected.client.is_closed(),
        slots(&connected.client),
    );
    let expected = (
        Some(true),
        true,
        vec![json!({ "jsonrpc": "2.0", "id": 5, "result": "small" }).to_string()],
        false,
        256,
    );
    assert_eq!(
        observed, expected,
        "expected the large answer refused for its size alone and the small one written: {expected:?} | received {observed:?} ({refusals:?})"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// A responder dropped unanswered tells the peer, so the peer does not wait on the question
/// for good, and gives back the place and the bytes the question held.
#[tokio::test]
async fn a_responder_dropped_unanswered_tells_the_peer_and_frees_what_it_held() {
    let connected = connect(options().with_max_in_flight_requests(1), handoff());
    let bytes = connected.client.state.peer_bytes.available_permits();
    connected.link.push_line(question(6, LET_GO));

    sent(&connected.link, 1).await;
    until("what the question held to be given back", || {
        connected.client.state.peer_bytes.available_permits() == bytes
    })
    .await;
    let observed = (connected.link.sent(), slots(&connected.client));
    let expected = (
        vec![
            json!({
                "jsonrpc": "2.0",
                "id": 6,
                "error": { "code": -32603, "message": "the request was let go without an answer" },
            })
            .to_string(),
        ],
        1,
    );
    assert_eq!(
        observed, expected,
        "expected the peer told and the one place free again: {expected:?} | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// A responder can be dropped by a thread that is on no runtime. The peer is told from there
/// all the same, and nothing panics.
#[test]
fn a_responder_dropped_outside_any_runtime_still_tells_the_peer() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .expect("expected a runtime to build");
    let (connected, responder) = runtime.block_on(async {
        let connected = connect(options(), handoff());
        connected.link.push_line(question(8, PARK));
        until("the question to be parked", || {
            connected.handler.parked() == 1
        })
        .await;
        let responder = connected.handler.take();
        (connected, responder)
    });
    drop(responder);
    let written = runtime.block_on(async {
        sent(&connected.link, 1).await;
        let written = wire(&connected.link);
        connected
            .client
            .close()
            .await
            .expect("expected a clean close");
        written
    });
    assert_eq!(
        written,
        vec!["error:8:-32603"],
        "expected the peer told from outside the runtime: [error:8:-32603] | received {written:?}"
    );
}

/// Once the connection has ended there is nobody to answer: `answer` says so and hands the
/// responder back, the refusal is not one about size, and dropping the responder sends nothing.
#[tokio::test]
async fn a_responder_answered_after_the_connection_ended_is_refused_and_says_nothing() {
    let connected = connect(options(), handoff());
    connected.link.push_line(question(4, PARK));
    until("the question to be parked", || {
        connected.handler.parked() == 1
    })
    .await;
    let responder = connected.handler.take();
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");

    let unanswered = responder
        .answer(ServerRequestOutcome::Answer(json!("late")))
        .expect_err("expected the answer refused after the close");
    let observed = (
        matches!(unanswered.error(), Error::Closed { .. }),
        unanswered.is_too_large(),
    );
    drop(unanswered);
    let observed = (
        observed,
        connected.link.sent().len(),
        connected.client.state.outbox.aside(),
    );
    assert_eq!(
        observed,
        ((true, false), 0, 0),
        "expected the answer refused as closed, and nothing written or queued for it: ((true, false), 0, 0) | received {observed:?}"
    );
}

/// A handler that only knows `on_request` still works on a client that hands questions over:
/// each is put to it in order and answered with what it returns, and an answer too large to
/// send is replaced by the refusal a client without the option sends.
#[tokio::test]
async fn a_handler_without_the_hand_off_is_asked_through_on_request() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(Some(ServerRequestOutcome::Answer(json!("y".repeat(400)))));
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        limits(200, usize::MAX, usize::MAX).with_request_handoff(),
    );
    link.push_line(question(3, "ask"));

    sent(&link, 1).await;
    let written = link.sent();
    let expected = vec![
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "error": { "code": -32603, "message": "the answer was too large to send" },
        })
        .to_string(),
    ];
    assert_eq!(
        written, expected,
        "expected on_request asked and its oversized answer replaced by the refusal: {expected:?} | received {written:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// The same handler with an answer that fits: `on_request` is asked, once, and what it
/// returns is what the peer gets, with the question's place given back.
#[tokio::test]
async fn a_handler_without_the_hand_off_has_its_answer_written() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(Some(ServerRequestOutcome::Answer(json!("yes"))));
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        handoff(),
    );
    link.push_line(question(3, "ask"));
    link.push_line(json!({ "jsonrpc": "2.0", "method": "after", "params": {} }).to_string());

    handler.wait_for_notifications(1).await;
    sent(&link, 1).await;
    let observed = (link.sent(), slots(&client), client.state.outbox.held());
    let expected = (
        vec![json!({ "jsonrpc": "2.0", "id": 3, "result": "yes" }).to_string()],
        256,
        (0, 0),
    );
    assert_eq!(
        observed, expected,
        "expected the answer on_request returned written once, and its place back: {expected:?} | received {observed:?}"
    );
    client.close().await.expect("expected a clean close");
}

/// Questions and notifications reach the handler in the order the peer sent them, one call at
/// a time, whatever the handler does with each question.
#[tokio::test]
async fn questions_and_notifications_reach_the_handler_in_the_order_they_were_sent() {
    let connected = connect(options(), handoff());
    connected
        .link
        .push_line(json!({ "jsonrpc": "2.0", "method": "first", "params": {} }).to_string());
    connected.link.push_line(question(1, PARK));
    connected.link.push_line(question(2, "unknown"));
    connected
        .link
        .push_line(json!({ "jsonrpc": "2.0", "method": "last", "params": {} }).to_string());

    until("every call to be made", || {
        connected.handler.calls().len() == 4
    })
    .await;
    sent(&connected.link, 1).await;
    let observed = (connected.handler.calls(), wire(&connected.link));
    let expected = (
        vec![
            String::from("notification:first"),
            String::from("handoff:park"),
            String::from("handoff:unknown"),
            String::from("notification:last"),
        ],
        vec![String::from("error:2:-32601")],
    );
    assert_eq!(
        observed, expected,
        "expected the calls in wire order and the one answer given: {expected:?} | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// A request whose answer is delivered in order cannot be awaited from inside a hand-off: the
/// task that would deliver it is the one awaiting the hand-off. It is refused before anything
/// is written, as it is inside a notification, and no hold is to be had there either.
#[tokio::test]
async fn an_ordered_request_made_inside_a_hand_off_is_refused_before_it_is_written() {
    let connected = connect(options(), handoff());
    connected.link.push_line(question(1, ASK_BACK));

    sent(&connected.link, 1).await;
    let asked_back = connected
        .handler
        .asked_back
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let observed = (asked_back, wire(&connected.link));
    let expected = (
        vec![(
            String::from(
                "one made inside on_notification or on_request_handoff, whose return its answer would wait for",
            ),
            false,
        )],
        vec![String::from("result:1")],
    );
    assert_eq!(
        observed, expected,
        "expected the ordered request refused by name, no hold granted, and only the answer written: {expected:?} | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// Refuses every frame that carries a result whole, the way a transport refuses a frame it
/// will not carry: no byte accepted, the link as good as it was.
struct RefuseResults {
    inner: Box<dyn crate::link::LinkSender>,
}

#[async_trait::async_trait]
impl crate::link::LinkSender for RefuseResults {
    async fn send(&mut self, message: String) -> crate::error::Result<()> {
        if message.contains("\"result\"") {
            return Err(Error::LimitExceeded {
                subject: "local outgoing frame bytes",
                limit: 1,
                received: message.len(),
            }
            .with_dispatch(crate::Dispatch::NotSubmitted));
        }
        self.inner.send(message).await
    }

    async fn close(&mut self) -> crate::error::Result<()> {
        self.inner.close().await
    }
}

async fn termination_of(handler: &ParkingHandler) -> PeerTermination {
    within("the termination", async {
        loop {
            let seen = handler
                .terminations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .first()
                .cloned();
            if let Some(termination) = seen {
                break termination;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

/// An answer that was queued and then not written ends the connection, whether or not anyone
/// kept its `Written`: the peer would otherwise wait for good on a question this side
/// believes it has answered. A link that refuses a frame whole fails a notification alone.
#[tokio::test]
async fn an_answer_the_link_would_not_take_ends_the_connection() {
    let link = ScriptedLink::new();
    let (sender, receiver) = link.clone().into_link().split();
    let handler = ParkingHandler::new();
    let client = Client::connect_with(
        crate::link::Link::new(Box::new(RefuseResults { inner: sender }), receiver),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        handoff(),
    );
    link.push_line(question(1, PARK));
    until("the question to be parked", || handler.parked() == 1).await;
    // Its write is not waited for, as a handler answering on the spot would not.
    drop(
        handler
            .take()
            .answer(ServerRequestOutcome::Answer(json!("lost")))
            .expect("expected the answer queued"),
    );

    let termination = termination_of(&handler).await;
    let observed = (&termination, link.sent().len());
    assert!(
        matches!(&observed, (PeerTermination::LinkFailed(cause), 0)
            if cause.contains("local outgoing frame bytes")),
        "expected the unwritten answer to end the connection, naming the refusal: (LinkFailed naming the link's refusal, 0) | received {observed:?}"
    );
    let _ = client.close().await;
}

/// A question whose id is as large as a frame may be cannot be answered in any form: neither
/// the answer nor the refusal that would replace it fits. The connection ends as a failed
/// link naming the bound, as it does on a client that does not hand questions over.
#[tokio::test]
async fn a_question_no_reply_to_fits_ends_the_connection() {
    let link = ScriptedLink::new();
    let handler = RecordingHandler::arc(Some(ServerRequestOutcome::Answer(json!("yes"))));
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
        limits(120, usize::MAX, usize::MAX).with_request_handoff(),
    );
    link.push_line(
        json!({ "jsonrpc": "2.0", "id": "i".repeat(200), "method": "ask", "params": {} })
            .to_string(),
    );

    let termination = super::outbox_tests::one_termination(&handler).await;
    let observed = (&termination, link.sent().len(), client.state.outbox.held());
    assert!(
        matches!(&observed, (PeerTermination::LinkFailed(cause), 0, (0, 0))
            if cause.contains("bytes of one outgoing JSON-RPC frame")),
        "expected the connection ended naming the frame bound, with nothing written or queued: (LinkFailed naming the bound, 0, (0, 0)) | received {observed:?}"
    );
    let _ = client.close().await;
}

/// A question still queued when the connection ends is not handed over: it could not be
/// answered, and a handler would only find that out by trying. The one whose hand-off was in
/// progress goes on, and its answer is refused as closed.
#[tokio::test]
async fn a_question_still_queued_when_the_connection_ended_is_not_handed_over() {
    // The drain that follows the peer's exit waits this long for the call in progress, which
    // has to outlast a slow machine getting from the end to the gate below.
    let mut patient = options();
    patient.shutdown_timeout = Duration::from_secs(5);
    let connected = connect(patient, handoff());
    connected.link.push_line(question(1, WAIT_THEN_PARK));
    connected.link.push_line(question(2, PARK));
    until("the first hand-off to begin", || {
        connected.handler.calls() == ["handoff:wait-then-park"]
    })
    .await;
    connected.link.end();
    until("the end to be noticed", || connected.client.is_closed()).await;
    connected.handler.gate.add_permits(1);

    let termination = termination_of(&connected.handler).await;
    let refused = connected
        .handler
        .take()
        .answer(ServerRequestOutcome::Answer(json!("late")))
        .expect_err("expected the answer refused after the end");
    let observed = (
        matches!(termination, PeerTermination::Exited),
        connected.handler.calls(),
        refused.is_too_large(),
        connected.link.sent().len(),
    );
    let expected = (true, vec![String::from("handoff:wait-then-park")], false, 0);
    assert_eq!(
        observed,
        expected,
        "expected the exit reported, the queued question not handed over, and nothing written: {expected:?} | received {observed:?} ({:?})",
        refused.error()
    );
}

/// A hand-off cut short by a close drops its responder with the call. Nothing is written for
/// it, and the place it held is free.
#[tokio::test]
async fn a_hand_off_cut_short_by_a_close_says_nothing_and_frees_its_place() {
    let connected = connect(options().with_max_in_flight_requests(1), handoff());
    connected.link.push_line(question(1, WAIT_THEN_PARK));
    until("the hand-off to begin", || {
        connected.handler.calls() == ["handoff:wait-then-park"]
    })
    .await;
    let before = slots(&connected.client);
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");

    until("the place to be given back", || {
        slots(&connected.client) == 1
    })
    .await;
    let observed = (
        before,
        connected.link.sent().len(),
        connected.client.state.outbox.held(),
        connected.handler.parked(),
    );
    assert_eq!(
        observed,
        (0, 0, (0, 0), 0),
        "expected the place held during the call, and nothing written, queued or parked after the close: (0, 0, (0, 0), 0) | received {observed:?}"
    );
}

/// An answer that finds the queue full comes back like any refused answer, and it is not a
/// refusal about its size. The connection ends with the outbound budget, and the responder
/// dropped afterwards adds nothing.
#[tokio::test]
async fn an_answer_that_finds_the_queue_full_ends_the_connection_and_says_no_more() {
    let connected = connect(
        options(),
        limits(usize::MAX, usize::MAX, 1).with_request_handoff(),
    );
    connected.link.push_line(question(1, PARK));
    until("the question to be parked", || {
        connected.handler.parked() == 1
    })
    .await;
    let _held = hold_the_link(&connected).await;

    let unanswered = connected
        .handler
        .take()
        .answer(ServerRequestOutcome::Answer(json!("no room")))
        .expect_err("expected the answer refused by a full queue");
    let refusal = (
        matches!(unanswered.error().cause(), Error::LimitExceeded { .. }),
        unanswered.is_too_large(),
    );
    drop(unanswered);
    let termination = termination_of(&connected.handler).await;
    connected.gate.add_permits(1);
    let _ = connected.client.close().await;
    let observed = (
        refusal,
        matches!(
            termination,
            PeerTermination::OutboundBackpressure {
                limit: 1,
                received: 2,
                ..
            }
        ),
        wire(&connected.link)
            .iter()
            .filter(|frame| frame.as_str() != "hold")
            .count(),
    );
    assert_eq!(
        observed,
        ((true, false), true, 0),
        "expected a limit refusal that is not about size, the outbound budget as the end, and no frame for the question: ((true, false), true, 0) | received {observed:?} ({termination:?})"
    );
}

/// Questions inside a batch are handed over in the order the array gives them, among the
/// notifications beside them.
#[tokio::test]
async fn questions_in_a_batch_are_handed_over_in_order() {
    let connected = connect(options(), handoff().with_batch_decoding());
    connected.link.push_line(
        json!([
            { "jsonrpc": "2.0", "method": "first", "params": {} },
            { "jsonrpc": "2.0", "id": 1, "method": PARK, "params": {} },
            { "jsonrpc": "2.0", "id": 2, "method": "unknown", "params": {} },
            { "jsonrpc": "2.0", "method": "last", "params": {} },
        ])
        .to_string(),
    );

    until("every call to be made", || {
        connected.handler.calls().len() == 4
    })
    .await;
    sent(&connected.link, 1).await;
    let observed = (connected.handler.calls(), wire(&connected.link));
    let expected = (
        vec![
            String::from("notification:first"),
            String::from("handoff:park"),
            String::from("handoff:unknown"),
            String::from("notification:last"),
        ],
        vec![String::from("error:2:-32601")],
    );
    assert_eq!(
        observed, expected,
        "expected the batch's members handled in order and the one answer given: {expected:?} | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// An answer gives back what the question held as it is queued, bytes and place both, and it
/// can be given by a thread that is on no runtime.
#[test]
fn an_answer_given_outside_any_runtime_is_written_and_frees_what_was_held() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_time()
        .build()
        .expect("expected a runtime to build");
    let (connected, responder, bytes) = runtime.block_on(async {
        let connected = connect(options().with_max_in_flight_requests(1), handoff());
        let bytes = connected.client.state.peer_bytes.available_permits();
        connected.link.push_line(question(8, PARK));
        until("the question to be parked", || {
            connected.handler.parked() == 1
        })
        .await;
        let responder = connected.handler.take();
        (connected, responder, bytes)
    });
    let held = (
        slots(&connected.client),
        connected.client.state.peer_bytes.available_permits() < bytes,
    );
    let answered = responder.answer(ServerRequestOutcome::Answer(json!("from a thread")));
    let freed = (
        slots(&connected.client),
        connected.client.state.peer_bytes.available_permits() == bytes,
    );
    let written = runtime.block_on(async {
        within(
            "the answer written",
            answered.expect("expected the answer queued"),
        )
        .await
        .expect("expected the answer written");
        let written = wire(&connected.link);
        connected
            .client
            .close()
            .await
            .expect("expected a clean close");
        written
    });
    let observed = (held, freed, written);
    let expected = ((0, true), (1, true), vec![String::from("result:8")]);
    assert_eq!(
        observed, expected,
        "expected the place and the bytes held until the answer was queued, and the answer written: {expected:?} | received {observed:?}"
    );
}

/// With no question allowed in flight, every question is past the cap and none is handed over.
#[tokio::test]
async fn a_cap_of_zero_hands_nothing_over() {
    let connected = connect(options().with_max_in_flight_requests(0), handoff());
    connected.link.push_line(question(1, PARK));

    sent(&connected.link, 1).await;
    let observed = (connected.handler.calls(), wire(&connected.link));
    let expected = (
        vec![String::from("past-cap:park")],
        vec![String::from("result:1")],
    );
    assert_eq!(
        observed, expected,
        "expected the question answered on the spot and never handed over: {expected:?} | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// A responder dropped unanswered is a frame in the outbound queue like the answer it stands
/// for. With the peer not reading and the queue full, the drop ends the connection with the
/// outbound budget instead of queueing beside it.
#[tokio::test]
async fn a_responder_dropped_into_a_full_queue_ends_the_connection() {
    let connected = connect(
        options(),
        limits(usize::MAX, usize::MAX, 1).with_request_handoff(),
    );
    let _held = hold_the_link(&connected).await;
    connected.link.push_line(question(1, LET_GO));

    let termination = termination_of(&connected.handler).await;
    let observed = (
        matches!(
            termination,
            PeerTermination::OutboundBackpressure {
                limit: 1,
                received: 2,
                ..
            }
        ),
        connected.client.state.outbox.aside(),
    );
    assert_eq!(
        observed,
        (true, 0),
        "expected the drop's refusal counted against a queue of one, and nothing queued beside it: (true, 0) | received {observed:?} ({termination:?})"
    );
    connected.gate.add_permits(1);
    let _ = connected.client.close().await;
}

/// A responder dropped when nothing can be said for its question ends the connection, so the
/// peer is not left waiting on one that looks well: an id as large as a frame may be, which no
/// reply fits, and a queue bounded at nothing, which takes no reply.
#[tokio::test]
async fn a_responder_dropped_with_no_way_to_say_so_ends_the_connection() {
    for (wire_options, id, named) in [
        (
            limits(120, usize::MAX, usize::MAX),
            json!("i".repeat(200)),
            "bytes of one outgoing JSON-RPC frame",
        ),
        (
            limits(usize::MAX, usize::MAX, 0),
            json!(1),
            "outgoing JSON-RPC frames awaiting their write",
        ),
    ] {
        let connected = connect(options(), wire_options.with_request_handoff());
        connected.link.push_line(
            json!({ "jsonrpc": "2.0", "id": id, "method": LET_GO, "params": {} }).to_string(),
        );

        let termination = termination_of(&connected.handler).await;
        let observed = (&termination, connected.link.sent().len());
        assert!(
            matches!(&observed, (PeerTermination::LinkFailed(cause), 0) if cause.contains(named)),
            "expected the connection ended naming what took no reply: (LinkFailed naming {named:?}, 0) | received {observed:?}"
        );
        let _ = connected.client.close().await;
    }
}

/// Ordered calls given up, more of them than the reserve for ordered responses holds, on one
/// connection that stays up. A call given up has no further use for its place, whether its
/// request was written or was still waiting behind a busy link, so the next ordered call
/// finds one.
#[tokio::test]
async fn ordered_calls_given_up_give_their_places_back() {
    for written_first in [true, false] {
        let connected = connect(options(), WireOptions::new());
        let held = if written_first {
            None
        } else {
            Some(hold_the_link(&connected).await)
        };
        for _ in 0..70 {
            let submitted =
                connected
                    .client
                    .submit_request("session/prompt", json!({}), ordered_options());
            let (written, reply) = match submitted {
                Ok(submitted) => submitted.into_parts(),
                Err(error) => panic!(
                    "expected every ordered call admitted after the one before it was given up (written first: {written_first}): Ok | received {error:?}"
                ),
            };
            if written_first {
                within("the request written", written)
                    .await
                    .expect("expected the request written");
            }
            drop(reply);
        }
        let places = connected.client.state.ordered_places.available_permits();
        assert_eq!(
            places, 64,
            "expected every place back once its call was given up (written first: {written_first}): 64 | received {places}"
        );
        connected.gate.add_permits(1);
        drop(held);
        connected
            .client
            .close()
            .await
            .expect("expected a clean close");
    }
}

/// A handler that answers no questions at all, by either path.
struct SilentHandler;

#[async_trait::async_trait]
impl PeerHandler for SilentHandler {
    async fn on_notification(&self, _method: String, _params: Value) {}
}

/// A handler that writes neither way of answering refuses every question as a method it does
/// not have, whether or not the client hands questions over.
#[tokio::test]
async fn a_handler_that_answers_nothing_refuses_every_question_as_not_found() {
    for wire_options in [WireOptions::new(), handoff()] {
        let link = ScriptedLink::new();
        let client = Client::connect_with(
            link.clone().into_link(),
            Arc::new(SilentHandler) as Arc<dyn PeerHandler>,
            options(),
            wire_options,
        );
        link.push_line(question(1, "anything"));

        sent(&link, 1).await;
        let written = link.sent();
        let expected = vec![
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": -32601, "message": "Method not found" },
            })
            .to_string(),
        ];
        assert_eq!(
            written, expected,
            "expected the question refused as a method not found: {expected:?} | received {written:?}"
        );
        client.close().await.expect("expected a clean close");
    }
}

/// Takes every question handed over and keeps it, and answers the ones past the cap itself,
/// noting which was which by the number the peer gave it.
struct RacingHandler {
    parked: std::sync::Mutex<Vec<Responder>>,
    handed: std::sync::Mutex<Vec<u64>>,
    past_cap: std::sync::Mutex<Vec<u64>>,
}

impl RacingHandler {
    fn seen(&self, id: u64) -> bool {
        let handed = self
            .handed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&id);
        handed
            || self
                .past_cap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&id)
    }

    fn take(&self) -> Option<Responder> {
        self.parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop()
    }
}

#[async_trait::async_trait]
impl PeerHandler for RacingHandler {
    async fn on_notification(&self, _method: String, _params: Value) {}

    async fn on_request_handoff(&self, _method: String, _params: Value, responder: Responder) {
        let id = responder.id().as_json().as_u64().unwrap_or(u64::MAX);
        // Parked before it is noted: a test that has seen the question finds its responder.
        self.parked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(responder);
        self.handed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id);
    }

    fn on_request_past_cap(
        &self,
        _method: &str,
        _params: &Value,
        id: &RequestId,
    ) -> Option<ServerRequestOutcome> {
        self.past_cap
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(id.as_json().as_u64().unwrap_or(u64::MAX));
        Some(ServerRequestOutcome::Answer(json!("past the cap")))
    }
}

/// Answers the question when the peer gave it an even number, and lets it go otherwise.
fn settle(responder: Responder) {
    if responder
        .id()
        .as_json()
        .as_u64()
        .unwrap_or(0)
        .is_multiple_of(2)
    {
        let _ = responder.answer(ServerRequestOutcome::Answer(json!("answered")));
    }
}

/// One place for a question in flight, and a second thread that answers or drops the question
/// holding it while the worker looks for a place for the next one. Round after round, with a
/// quarter of the rounds given to each order and a quarter left to race. Every question gets
/// exactly one reply, of the kind that fits what became of it; the place is never given back
/// twice or kept; and nothing the questions held is left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_place_freed_from_another_thread_is_given_back_once_whatever_the_worker_is_doing() {
    const ROUNDS: u64 = 240;
    let link = ScriptedLink::new();
    let handler = Arc::new(RacingHandler {
        parked: std::sync::Mutex::new(Vec::new()),
        handed: std::sync::Mutex::new(Vec::new()),
        past_cap: std::sync::Mutex::new(Vec::new()),
    });
    let client = Client::connect_with(
        link.clone().into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options().with_max_in_flight_requests(1),
        handoff(),
    );
    let bytes = client.state.peer_bytes.available_permits();
    for id in 0..ROUNDS {
        let ask = || link.push_line(question(id, "ask"));
        let outstanding = handler.take();
        match id % 4 {
            // The place is free before the question is asked: it is handed over.
            0 | 2 => {
                if let Some(responder) = outstanding {
                    within(
                        "the settling thread",
                        tokio::task::spawn_blocking(|| settle(responder)),
                    )
                    .await
                    .expect("expected the settling thread to finish");
                }
                ask();
                until("the question to be seen", || handler.seen(id)).await;
            }
            // The place is still taken when the question is asked: it is past the cap.
            1 => {
                ask();
                until("the question to be seen", || handler.seen(id)).await;
                if let Some(responder) = outstanding {
                    settle(responder);
                }
            }
            // Both at once.
            _ => {
                let settling =
                    outstanding.map(|responder| tokio::task::spawn_blocking(|| settle(responder)));
                ask();
                until("the question to be seen", || handler.seen(id)).await;
                if let Some(settling) = settling {
                    within("the settling thread", settling)
                        .await
                        .expect("expected the settling thread to finish");
                }
            }
        }
    }
    if let Some(responder) = handler.take() {
        settle(responder);
    }
    sent(
        &link,
        usize::try_from(ROUNDS).expect("expected the rounds to fit"),
    )
    .await;
    until("what the questions held to be given back", || {
        client.state.peer_bytes.available_permits() == bytes && client.state.outbox.held() == (0, 0)
    })
    .await;

    let handed = handler
        .handed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let past_cap = handler
        .past_cap
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let written: Vec<Value> = link
        .sent()
        .iter()
        .map(|frame| serde_json::from_str(frame).expect("expected a JSON frame"))
        .collect();
    for id in 0..ROUNDS {
        let replies: Vec<&Value> = written.iter().filter(|frame| frame["id"] == id).collect();
        let was = (handed.contains(&id), past_cap.contains(&id));
        let expected = match (was, id % 2) {
            ((true, false), 0) => json!({ "jsonrpc": "2.0", "id": id, "result": "answered" }),
            ((true, false), _) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": "the request was let go without an answer" },
            }),
            _ => json!({ "jsonrpc": "2.0", "id": id, "result": "past the cap" }),
        };
        assert!(
            (was.0 != was.1) && replies == [&expected],
            "expected question {id} either handed over or past the cap, and its one reply to match: (one of the two, [{expected}]) | received (handed {}, past the cap {}, {replies:?})",
            was.0,
            was.1
        );
    }
    let quarter = usize::try_from(ROUNDS / 4).expect("expected the rounds to fit");
    let observed = (
        client.state.handoff_slots.available_permits(),
        handed.len() >= 2 * quarter,
        past_cap.len() >= quarter - 1,
        client.state.outbox.aside(),
    );
    assert_eq!(
        observed,
        (1, true, true, 0),
        "expected the one place back exactly once, and both ends to have occurred: (1, true, true, 0) | received {observed:?} ({} handed over, {} past the cap)",
        handed.len(),
        past_cap.len()
    );
    client.close().await.expect("expected a clean close");
}

/// An answer given in the moment a close has sealed the queue and not yet raised its flag is
/// refused as closed and comes back with its responder. It is not taken for written, nothing
/// is written for it, and the responder dropped afterwards adds nothing.
#[tokio::test]
async fn an_answer_given_as_the_connection_closes_is_refused_and_not_lost_in_silence() {
    let connected = connect(options(), handoff());
    connected.link.push_line(question(1, PARK));
    until("the question to be parked", || {
        connected.handler.parked() == 1
    })
    .await;
    let seen = Arc::new(std::sync::Mutex::new(None));
    {
        let responder = connected.handler.take();
        let seen = Arc::clone(&seen);
        let client = Arc::clone(&connected.client);
        *connected
            .client
            .state
            .after_seal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            let answered = responder.answer(ServerRequestOutcome::Answer(json!("too late")));
            let outcome = match &answered {
                Ok(_) => String::from("queued"),
                Err(unanswered) => format!(
                    "refused: closed {}, too large {}",
                    matches!(unanswered.error(), Error::Closed { .. }),
                    unanswered.is_too_large()
                ),
            };
            *seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                Some((client.is_closed(), outcome));
        }));
    }
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");

    let seen = seen
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let observed = (
        seen,
        connected.link.sent().len(),
        connected.client.state.outbox.held(),
        connected.client.state.outbox.aside(),
    );
    let expected = (
        Some((false, String::from("refused: closed true, too large false"))),
        0,
        (0, 0),
        0,
    );
    assert_eq!(
        observed, expected,
        "expected the answer in the window refused as closed, with the flag still down, and nothing written or queued: {expected:?} | received {observed:?}"
    );
}

/// An answer that waits in the queue until its deadline has run out was never written, and
/// nobody has to be awaiting its write for that to matter: the connection ends.
#[tokio::test(start_paused = true)]
async fn an_answer_whose_deadline_runs_out_in_the_queue_ends_the_connection() {
    let connected = connect(
        options().with_request_timeout(Duration::from_millis(50)),
        handoff(),
    );
    connected.link.push_line(question(1, PARK));
    until("the question to be parked", || {
        connected.handler.parked() == 1
    })
    .await;
    // The link is busy with nothing that has a deadline of its own: the test holds it.
    let link_taken = connected.client.state.sender.lock().await;
    drop(
        connected
            .handler
            .take()
            .answer(ServerRequestOutcome::Answer(json!("stale")))
            .expect("expected the answer queued"),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(link_taken);

    // By the machine's clock: the runtime's is paused and does not move while this yields.
    until("the handler to be told", || {
        !connected
            .handler
            .terminations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    })
    .await;
    let termination = termination_of(&connected.handler).await;
    let observed = (&termination, connected.link.sent().len());
    assert!(
        matches!(&observed, (PeerTermination::LinkFailed(_), 0)),
        "expected the answer that outlived its deadline to end the connection unwritten: (LinkFailed, 0) | received {observed:?}"
    );
    let _ = connected.client.close().await;
}

/// A close awaited from inside a hand-off ends the call at that await, as it does inside a
/// notification: the close does not wait for the call it was made from. The responder goes
/// with the call and says nothing.
#[tokio::test]
async fn a_close_awaited_inside_a_hand_off_ends_the_call_there() {
    let connected = connect(options().with_max_in_flight_requests(1), handoff());
    connected.link.push_line(question(1, CLOSE_INSIDE));

    until("the call to be over and its place back", || {
        connected.client.is_closed() && slots(&connected.client) == 1
    })
    .await;
    let observed = (
        connected.handler.calls(),
        connected.link.sent().len(),
        connected.client.ended(),
    );
    let expected = (
        vec![
            String::from("handoff:close-inside"),
            String::from("closing"),
        ],
        0,
        Some(ConnectionEnd::Closed),
    );
    assert_eq!(
        observed, expected,
        "expected the call cut at its close, nothing written, and the close on record: {expected:?} | received {observed:?}"
    );
    let _ = connected.client.close().await;
}

/// A request whose answer the reader delivers itself can be awaited inside a hand-off: nothing
/// it waits for is behind the call.
#[tokio::test]
async fn a_request_delivered_by_the_reader_can_be_awaited_inside_a_hand_off() {
    let connected = connect(options(), handoff());
    connected.link.push_line(question(5, ASK_PLAIN));
    sent(&connected.link, 1).await;
    connected
        .link
        .push_line(r#"{"jsonrpc":"2.0","id":"1","result":"pong"}"#);

    sent(&connected.link, 2).await;
    let written = connected.link.sent();
    let observed = (wire(&connected.link), written.last().cloned());
    let expected = (
        vec![String::from("back"), String::from("result:5")],
        Some(json!({ "jsonrpc": "2.0", "id": 5, "result": { "got": "pong" } }).to_string()),
    );
    assert_eq!(
        observed, expected,
        "expected the request made inside the hand-off answered, and the question answered with it: {expected:?} | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// Questions read while a hand-off is in progress wait in the queue the notifications wait
/// in, and count against the same budget. A peer that asks past it ends the connection as it
/// would with notifications, and none of the questions behind the call is handed over.
#[tokio::test]
async fn a_flood_of_questions_behind_a_hand_off_ends_the_connection_at_the_budget() {
    let connected = connect(options().with_max_pending_notifications(4), handoff());
    connected.link.push_line(question(0, WAIT_THEN_PARK));
    until("the hand-off to begin", || {
        connected.handler.calls() == ["handoff:wait-then-park"]
    })
    .await;
    for id in 1..=5 {
        connected.link.push_line(question(id, "unknown"));
    }

    let termination = termination_of(&connected.handler).await;
    let observed = (
        &termination,
        connected.handler.calls(),
        connected.link.sent().len(),
    );
    assert!(
        matches!(
            &observed,
            (
                PeerTermination::NotificationBackpressure {
                    limit: 4,
                    received: 5,
                    ..
                },
                calls,
                0
            ) if calls == &["handoff:wait-then-park"]
        ),
        "expected the fifth question behind the call to pass a budget of four, with none of them handed over or answered: (NotificationBackpressure {{ limit: 4, received: 5 }}, [handoff:wait-then-park], 0) | received {observed:?}"
    );
    let _ = connected.client.close().await;
}

/// Two questions the peer gave the same id are two questions: each has its own responder and
/// its own place, and each answer goes out under the id it came with.
#[tokio::test]
async fn questions_with_the_same_id_are_answered_each_on_its_own() {
    let connected = connect(options().with_max_in_flight_requests(2), handoff());
    connected.link.push_line(question(7, PARK));
    connected.link.push_line(question(7, PARK));
    until("both questions to be parked", || {
        connected.handler.parked() == 2
    })
    .await;
    let taken = slots(&connected.client);
    for answer in ["first", "second"] {
        let written = connected
            .handler
            .take()
            .answer(ServerRequestOutcome::Answer(json!(answer)))
            .expect("expected the answer queued");
        within("the answer written", written)
            .await
            .expect("expected the answer written");
    }

    let observed = (taken, connected.link.sent(), slots(&connected.client));
    let expected = (
        0,
        vec![
            json!({ "jsonrpc": "2.0", "id": 7, "result": "first" }).to_string(),
            json!({ "jsonrpc": "2.0", "id": 7, "result": "second" }).to_string(),
        ],
        2,
    );
    assert_eq!(
        observed, expected,
        "expected a place for each question, one answer each in the order given, and both places back: {expected:?} | received {observed:?}"
    );
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
}

/// A handler that says, through a channel a thread can wait on, that it was told of an end.
struct ToldHandler {
    told: std::sync::Mutex<std::sync::mpsc::Sender<&'static str>>,
}

#[async_trait::async_trait]
impl PeerHandler for ToldHandler {
    async fn on_notification(&self, _method: String, _params: Value) {}

    async fn on_terminated(&self, _termination: PeerTermination) {
        let _ = self
            .told
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .send("told of a termination");
    }
}

/// A write that fails in the moment a close is on record and its flag is not yet up reaches
/// the reader as a write failure. The close owns that end: the reader lets the failure go,
/// the handler is told of no termination, and the client reports the close it was given.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_failure_that_lands_on_a_close_is_not_reported_as_a_termination() {
    let link = ScriptedLink::new();
    let (tell, told) = std::sync::mpsc::channel();
    let handler = Arc::new(ToldHandler {
        told: std::sync::Mutex::new(tell.clone()),
    });
    let client = Client::connect(
        link.into_link(),
        Arc::clone(&handler) as Arc<dyn PeerHandler>,
        options(),
    );
    *client
        .state
        .after_write_failure_ignored
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
        let _ = tell.send("let go by the reader");
    }));
    let seen = Arc::new(std::sync::Mutex::new(None));
    {
        let state = Arc::clone(&client.state);
        let seen = Arc::clone(&seen);
        *client
            .state
            .after_seal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(move || {
            // What a writer that read the flag a moment ago does next.
            state.signal_write_failure(&state.write_timeout());
            // Held here, the flag still down, until the reader has dealt with it one way or
            // the other.
            let dealt_with = told.recv_timeout(Duration::from_secs(5)).ok();
            *seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = dealt_with;
        }));
    }
    client.close().await.expect("expected a clean close");

    let observed = (
        *seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        client.ended(),
    );
    assert_eq!(
        observed,
        (Some("let go by the reader"), Some(ConnectionEnd::Closed)),
        "expected the reader to let the failure go under the close on record: (Some(\"let go by the reader\"), Some(Closed)) | received {observed:?}"
    );
}

/// A refused answer is an error that can be returned as one: it says the answer was not
/// queued and why, gives the reason as its source, and can cross threads.
#[tokio::test]
async fn a_refused_answer_is_an_error_naming_its_reason() {
    fn crosses_threads<T: std::error::Error + Send + Sync + 'static>(_: &T) {}

    let connected = connect(options(), handoff());
    connected.link.push_line(question(1, PARK));
    until("the question to be parked", || {
        connected.handler.parked() == 1
    })
    .await;
    let responder = connected.handler.take();
    connected
        .client
        .close()
        .await
        .expect("expected a clean close");
    let unanswered = responder
        .answer(ServerRequestOutcome::Answer(json!("late")))
        .expect_err("expected the answer refused after the close");
    crosses_threads(&unanswered);

    let source = std::error::Error::source(&unanswered)
        .and_then(|source| source.downcast_ref::<Error>())
        .map(ToString::to_string);
    let observed = (unanswered.to_string(), source);
    let expected = (
        format!(
            "the answer to a peer's question was not queued: {}",
            unanswered.error()
        ),
        Some(unanswered.error().to_string()),
    );
    assert_eq!(
        observed, expected,
        "expected the refusal worded with its reason, and the reason as its source: {expected:?} | received {observed:?}"
    );
}
