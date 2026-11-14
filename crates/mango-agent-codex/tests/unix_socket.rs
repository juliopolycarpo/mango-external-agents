//! The harness attached to an app-server that is already listening on a Unix socket.
//!
//! A named in-process fake listens on a real socket, answers the WebSocket upgrade, and replays
//! the recorded `turn` conversation over text frames, so a full turn crosses the same link the
//! real server's `--listen unix://PATH` would give it. The host's launcher is an empty
//! [`FakeLauncher`]: it has no process to hand out, so any spawn, including a `--version` probe,
//! would fail the test that did not expect one.

#![cfg(all(unix, feature = "unix-socket"))]

// Shared with the replay suite; this binary uses only the transcript's responder.
#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use mango_agent_codex::{CodexHarness, MINIMUM_CODEX_VERSION};
use mango_external_agents::event::EventKind;
use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{
    CancelReason, CancelToken, CloseReason, Dispatch, Error, Harness, HostContext, OpenSession,
    SessionStatus, TransportKind, TurnRequest,
};
use serde_json::{Value, json};
use support::{Transcript, workspace_path};
use tokio::net::UnixListener;
use tokio::sync::Notify;
use tokio_tungstenite::accept_hdr_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;

type Responder = Box<dyn FnMut(&str) -> Vec<String> + Send>;

/// An answer line that makes the fake drop the connection, without a closing handshake, instead of
/// being sent: the server process dying mid-conversation.
const HANG_UP: &str = "<hang up>";

/// An answer line that makes the fake stop reading for [`STALL_FOR`] instead of being sent: a
/// server that is alive but not draining its socket.
const STALL: &str = "<stop reading>";

/// Longer than the teardown bounds the stalled-server test sets, shorter than the test's patience.
const STALL_FOR: Duration = Duration::from_millis(2500);

/// What the fake does with a `turn/interrupt`.
#[derive(Clone, Copy)]
enum Interrupt {
    /// Answers with an empty result, but never ends the turn: the vendor accepted the request and
    /// is slow to act on it.
    AcknowledgedOnly,
    /// Never answers.
    Ignored,
}

/// An app-server stand-in on a Unix socket: upgrades each connection and answers text frames.
struct FakeAppServerSocket {
    /// Held so the socket's directory outlives the test.
    _directory: tempfile::TempDir,
    path: PathBuf,
    /// Upgrades the server completed.
    connections: Arc<AtomicUsize>,
    /// Connections that have ended, from the server's side.
    disconnections: Arc<AtomicUsize>,
    ended: Arc<Notify>,
    /// Every text frame the harness sent, in order.
    received: Arc<Mutex<Vec<String>>>,
}

impl FakeAppServerSocket {
    /// Replays the recorded `turn` conversation.
    fn replaying_turn() -> Self {
        Self::start(Box::new(Transcript::load("turn").responder()), None)
    }

    /// Answers `initialize` as a build that reports `user_agent`, and everything else as `turn`.
    fn reporting_user_agent(user_agent: &'static str) -> Self {
        let mut recorded = Transcript::load("turn").responder();
        Self::start(
            Box::new(move |line| {
                let frame: Value = serde_json::from_str(line).unwrap_or(Value::Null);
                if frame["method"] == "initialize" {
                    return vec![
                        json!({"id": frame["id"], "result": {
                            "userAgent": user_agent,
                            "platformOs": "linux",
                        }})
                        .to_string(),
                    ];
                }
                recorded(line)
            }),
            None,
        )
    }

    /// Replays `turn`, but dies right after answering `turn/start`.
    fn dying_after_turn_start() -> Self {
        let mut recorded = Transcript::load("turn").responder();
        Self::start(
            Box::new(move |line| {
                let mut answers = recorded(line);
                if line.contains("\"turn/start\"") {
                    // Keep what precedes the start answer, drop the rest of the recorded turn,
                    // including its `turn/completed`, and die in the middle of it.
                    let answered = answers
                        .iter()
                        .position(|answer| answer.contains("\"result\""))
                        .map_or(0, |at| at + 1);
                    answers.truncate(answered);
                    answers.push(HANG_UP.to_owned());
                }
                answers
            }),
            None,
        )
    }

    /// Replays `turn` through `thread/start`, then stops reading for [`STALL_FOR`].
    fn stalling_after_thread_start() -> Self {
        let mut recorded = Transcript::load("turn").responder();
        Self::start(
            Box::new(move |line| {
                let mut answers = recorded(line);
                if line.contains("\"thread/start\"") {
                    answers.push(STALL.to_owned());
                }
                answers
            }),
            None,
        )
    }

    /// Replays `turn` up to the answer to `turn/start`, then keeps the turn running for good: the
    /// recorded notifications that would end it are dropped.
    fn holding_a_turn_open(interrupt: Interrupt) -> Self {
        let mut recorded = Transcript::load("turn").responder();
        Self::start(
            Box::new(move |line| {
                let frame: Value = serde_json::from_str(line).unwrap_or(Value::Null);
                if frame["method"] == "turn/interrupt" {
                    return match interrupt {
                        Interrupt::AcknowledgedOnly => {
                            vec![json!({"id": frame["id"], "result": {}}).to_string()]
                        }
                        Interrupt::Ignored => Vec::new(),
                    };
                }
                let mut answers = recorded(line);
                if frame["method"] == "turn/start" {
                    let answered = answers
                        .iter()
                        .position(|answer| answer.contains("\"result\""))
                        .map_or(0, |at| at + 1);
                    answers.truncate(answered);
                }
                answers
            }),
            None,
        )
    }

    /// Upgrades the connection and then never answers a frame.
    fn silent() -> Self {
        Self::start(Box::new(|_| Vec::new()), None)
    }

    /// Answers the upgrade with this status instead of `101`.
    fn refusing_upgrade(status: StatusCode) -> Self {
        Self::start(Box::new(|_| Vec::new()), Some(status))
    }

    fn start(mut respond: Responder, refuse: Option<StatusCode>) -> Self {
        // A short, private directory: a socket path is capped near 100 bytes.
        let directory = tempfile::Builder::new()
            .prefix("mea-")
            .tempdir()
            .expect("expected a temporary directory");
        let path = directory.path().join("app-server.sock");
        let listener = UnixListener::bind(&path).expect("expected a listener");
        let server = Self {
            _directory: directory,
            path,
            connections: Arc::new(AtomicUsize::new(0)),
            disconnections: Arc::new(AtomicUsize::new(0)),
            ended: Arc::new(Notify::new()),
            received: Arc::new(Mutex::new(Vec::new())),
        };
        let connections = Arc::clone(&server.connections);
        let disconnections = Arc::clone(&server.disconnections);
        let ended = Arc::clone(&server.ended);
        let received = Arc::clone(&server.received);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                // The `Err` type is the socket library's own callback contract.
                #[allow(clippy::result_large_err)]
                let upgrade = |_: &Request, response: Response| match refuse {
                    Some(status) => {
                        let mut refusal = ErrorResponse::new(None);
                        *refusal.status_mut() = status;
                        Err(refusal)
                    }
                    None => Ok(response),
                };
                let Ok(mut socket) = accept_hdr_async(stream, upgrade).await else {
                    continue;
                };
                connections.fetch_add(1, Ordering::SeqCst);
                'connection: while let Some(Ok(message)) = socket.next().await {
                    match message {
                        Message::Text(frame) => {
                            received
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .push(frame.to_string());
                            for answer in respond(&frame) {
                                if answer == HANG_UP {
                                    break 'connection;
                                }
                                if answer == STALL {
                                    tokio::time::sleep(STALL_FOR).await;
                                    continue;
                                }
                                if socket.send(Message::Text(answer.into())).await.is_err() {
                                    break;
                                }
                            }
                        }
                        Message::Close(_) => break,
                        _ => {}
                    }
                }
                disconnections.fetch_add(1, Ordering::SeqCst);
                ended.notify_waiters();
            }
        });
        server
    }

    fn harness(&self) -> CodexHarness {
        CodexHarness::new().with_unix_socket(&self.path)
    }

    fn frames(&self) -> Vec<Value> {
        self.received
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|line| serde_json::from_str(line).expect("expected a JSON frame"))
            .collect()
    }

    /// Methods of the frames received so far, in order.
    fn methods(&self) -> Vec<String> {
        self.frames()
            .iter()
            .filter_map(|frame| frame["method"].as_str().map(str::to_owned))
            .collect()
    }

    /// Waits until the server has received a frame calling `method`.
    async fn until_method(&self, method: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.methods().iter().any(|received| received == method) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected a `{method}` frame | received: {:?}",
                self.methods()
            )
        });
    }

    /// Waits until the server has seen `count` connections end.
    async fn until_disconnections(&self, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let ended = self.ended.notified();
                if self.disconnections.load(Ordering::SeqCst) >= count {
                    return;
                }
                ended.await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "expected {count} ended connections | received: {}",
                self.disconnections.load(Ordering::SeqCst)
            )
        });
    }
}

/// A host whose launcher has nothing to hand out, so a spawn is a failure.
fn host_without_processes() -> (HostContext, Arc<FakeLauncher>) {
    host_with_limits(mango_external_agents::Limits {
        request_timeout: Duration::from_secs(5),
        ..mango_external_agents::Limits::default()
    })
}

fn host_with_limits(limits: mango_external_agents::Limits) -> (HostContext, Arc<FakeLauncher>) {
    let launcher = Arc::new(FakeLauncher::new());
    let host = HostContext::builder()
        .launcher(launcher.clone())
        .cwd(workspace_path())
        .client_info("mango-test", "0.0.1")
        .limits(limits)
        .build()
        .expect("expected a host");
    (host, launcher)
}

/// Short bounds, so a teardown that has to give up on the server finishes within a test.
fn quick_teardown_limits() -> mango_external_agents::Limits {
    mango_external_agents::Limits {
        request_timeout: Duration::from_secs(5),
        kill_grace: Duration::from_millis(300),
        shutdown_timeout: Duration::from_secs(1),
        cancel_settle_timeout: Duration::from_millis(500),
        ..mango_external_agents::Limits::default()
    }
}

/// Opens a session on `server` and starts a turn the server never ends.
async fn session_with_a_running_turn(
    server: &FakeAppServerSocket,
    host: &HostContext,
) -> (
    Box<dyn mango_external_agents::Session>,
    mango_external_agents::TurnStream,
) {
    let session = server
        .harness()
        .open_session(host, over_socket("chat-1"))
        .await
        .expect("expected a session");
    let turn = session
        .start_turn(TurnRequest::new("turn-1", "run echo mango"))
        .await
        .expect("expected the turn to be accepted");
    (session, turn)
}

/// The position of `method` among the frames the server received.
fn position_of(methods: &[String], method: &str) -> usize {
    methods
        .iter()
        .position(|received| received == method)
        .unwrap_or_else(|| panic!("expected a `{method}` frame | received: {methods:?}"))
}

fn over_socket(session: &str) -> OpenSession {
    OpenSession::new(session).over_transport(TransportKind::UnixSocket)
}

async fn drain(turn: &mut mango_external_agents::TurnStream) -> Vec<EventKind> {
    let mut events = Vec::new();
    let collected = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            events.push(event.kind);
            if terminal {
                break;
            }
        }
    })
    .await;
    assert!(
        collected.is_ok(),
        "expected the turn to end, received {events:#?}"
    );
    events
}

/// The issue's definition of done: a fake app-server on a Unix socket drives a full turn.
#[tokio::test]
async fn a_full_turn_runs_over_the_unix_socket_without_spawning_anything() {
    let server = FakeAppServerSocket::replaying_turn();
    let (host, launcher) = host_without_processes();

    let session = server
        .harness()
        .open_session(&host, over_socket("chat-1"))
        .await
        .expect("expected a session attached to the socket");
    assert_eq!(
        session.snapshot().transport.effective,
        TransportKind::UnixSocket
    );
    assert_eq!(
        session.ids().native_session_id,
        Transcript::load("turn")
            .thread_id()
            .expect("expected the fixture to open a thread"),
        "expected the thread the server opened"
    );

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "run echo mango"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.first(), Some(EventKind::TurnStarted { .. })),
        "expected the server's acceptance first, received {events:#?}"
    );
    assert!(
        events
            .iter()
            .any(|kind| matches!(kind, EventKind::TextDelta { .. })),
        "expected the answer to stream, received {events:#?}"
    );
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the terminal last, received {events:#?}"
    );

    let methods: Vec<String> = server
        .frames()
        .iter()
        .filter_map(|frame| frame["method"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(
        methods.iter().take(2).collect::<Vec<_>>(),
        ["initialize", "initialized"],
        "expected the handshake first over the socket, received {methods:?}"
    );
    assert!(
        methods.iter().any(|method| method == "turn/start"),
        "expected the turn to reach the server, received {methods:?}"
    );
    assert_eq!(
        launcher.launches().len(),
        0,
        "expected no spawn: the server is the host's"
    );
    assert_eq!(server.connections.load(Ordering::SeqCst), 1);
}

/// Closing ends this session's connection and nothing else: the server keeps listening for the
/// next client, with whatever thread it holds.
#[tokio::test]
async fn closing_the_session_ends_the_connection_and_leaves_the_server_listening() {
    let server = FakeAppServerSocket::replaying_turn();
    let (host, launcher) = host_without_processes();
    let session = server
        .harness()
        .open_session(&host, over_socket("chat-1"))
        .await
        .expect("expected a session");

    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected a close that waits for the attachment to end");
    server.until_disconnections(1).await;

    tokio::net::UnixStream::connect(&server.path)
        .await
        .expect("expected the server to still accept connections after the session closed");
    assert_eq!(launcher.launches().len(), 0, "expected nothing to signal");
}

#[tokio::test]
async fn a_socket_transport_without_a_path_is_host_configuration_before_any_dial() {
    let server = FakeAppServerSocket::replaying_turn();
    let (host, _) = host_without_processes();

    let error = CodexHarness::new()
        .open_session(&host, over_socket("chat-1"))
        .await
        .err()
        .expect("expected a refusal, received a session");
    assert!(
        matches!(error.cause(), Error::HostConfiguration { expected, received }
            if *expected == "a Unix socket path set with CodexHarness::with_unix_socket"
                && received == "none"),
        "expected host configuration naming the missing path, received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
}

/// A kind the harness does not declare is refused by name before anything is dialled, even with a
/// socket path configured.
#[tokio::test]
async fn an_undeclared_transport_is_refused_before_the_socket_is_dialled() {
    let server = FakeAppServerSocket::replaying_turn();
    let (host, _) = host_without_processes();

    let error = server
        .harness()
        .open_session(
            &host,
            OpenSession::new("chat-1").over_transport(TransportKind::WebSocket),
        )
        .await
        .err()
        .expect("expected a refusal, received a session");
    assert!(
        matches!(
            error.cause(),
            Error::UnsupportedTransport {
                transport: TransportKind::WebSocket,
                ..
            }
        ),
        "expected UnsupportedTransport for websocket, received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
}

/// The harness never presents a token: a server that wants one is the host's configuration to fix.
#[tokio::test]
async fn a_server_that_wants_a_token_is_refused_as_host_configuration() {
    let server = FakeAppServerSocket::refusing_upgrade(StatusCode::UNAUTHORIZED);
    let (host, _) = host_without_processes();

    let error = server
        .harness()
        .open_session(&host, over_socket("chat-1"))
        .await
        .err()
        .expect("expected a refusal, received a session");
    assert!(
        matches!(error.cause(), Error::HostConfiguration { received, .. }
            if received == "HTTP 401 Unauthorized from the upgrade"),
        "expected host configuration naming HTTP 401, received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
}

#[tokio::test]
async fn a_socket_nobody_listens_on_is_a_link_failure_that_submitted_nothing() {
    let directory = tempfile::Builder::new()
        .prefix("mea-")
        .tempdir()
        .expect("expected a temporary directory");
    let (host, _) = host_without_processes();

    let error = CodexHarness::new()
        .with_unix_socket(directory.path().join("absent.sock"))
        .open_session(&host, over_socket("chat-1"))
        .await
        .err()
        .expect("expected a refusal, received a session");
    assert!(
        matches!(error.cause(), Error::Link { message, .. }
            if message == "a socket failure (NotFound)"),
        "expected a link failure naming the io error kind, received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
}

/// Nothing is spawned, so the local binary cannot be gated: the server's own `initialize` answer
/// is the version that counts.
#[tokio::test]
async fn an_attached_server_below_the_floor_is_refused_by_its_own_handshake() {
    let server = FakeAppServerSocket::reporting_user_agent("codex_cli_rs/0.100.0 (Linux)");
    let (host, launcher) = host_without_processes();

    let error = server
        .harness()
        .open_session(&host, over_socket("chat-1"))
        .await
        .err()
        .expect("expected a refusal, received a session");
    assert!(
        matches!(error.cause(), Error::VersionGate { found, minimum }
            if found == "0.100.0" && minimum == MINIMUM_CODEX_VERSION),
        "expected the server's version to be gated, received {error:?}"
    );
    assert_eq!(launcher.launches().len(), 0, "expected no local probe");
    // A refused open still ends its connection; the server is left running.
    server.until_disconnections(1).await;
}

/// A server that dies mid-turn is an ended connection, not a hang: the turn fails once, and the
/// message says the connection ended with an unknown exit, since there is no child to ask.
#[tokio::test]
async fn a_server_that_dies_mid_turn_fails_the_turn_once() {
    let server = FakeAppServerSocket::dying_after_turn_start();
    let (host, _) = host_without_processes();
    let session = server
        .harness()
        .open_session(&host, over_socket("chat-1"))
        .await
        .expect("expected a session");

    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "run echo mango"))
        .await
        .expect("expected the turn to be accepted before the server died");
    let events = drain(&mut turn).await;

    let failures: Vec<&EventKind> = events
        .iter()
        .filter(|kind| matches!(kind, EventKind::Error { .. }))
        .collect();
    assert_eq!(
        failures.len(),
        1,
        "expected exactly one terminal failure, received {events:#?}"
    );
    assert!(
        matches!(failures[0], EventKind::Error { error }
            if error.message.contains("connection ended while the turn was active")
                && error.message.contains("exit code=unknown, signal=none")),
        "expected the connection-ended failure with an unknown exit, received {failures:#?}"
    );
}

/// Over stdio, reaping the child is what stops a running turn. A server the host owns cannot be
/// reaped, so the native `turn/interrupt` is the only lever: close sends it before it disconnects.
#[tokio::test]
async fn closing_a_running_turn_asks_the_server_to_interrupt_it_before_disconnecting() {
    let server = FakeAppServerSocket::holding_a_turn_open(Interrupt::AcknowledgedOnly);
    let (host, launcher) = host_with_limits(quick_teardown_limits());
    let (session, mut turn) = session_with_a_running_turn(&server, &host).await;

    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected the close to detach from the server");
    server.until_disconnections(1).await;

    let methods = server.methods();
    assert!(
        position_of(&methods, "turn/start") < position_of(&methods, "turn/interrupt"),
        "expected `turn/interrupt` after `turn/start` and before the disconnect, received {methods:?}"
    );
    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::Cancelled {
                reason: CancelReason::Shutdown
            }
        )),
        "expected the turn to be cancelled with reason Shutdown, received {events:#?}"
    );
    assert_eq!(launcher.launches().len(), 0);
}

/// The residual of a detachable peer: when the server never acknowledges the interrupt, the
/// session still disconnects within its bounds. The turn may keep running in the server.
#[tokio::test]
async fn closing_when_the_server_ignores_the_interrupt_still_disconnects() {
    let server = FakeAppServerSocket::holding_a_turn_open(Interrupt::Ignored);
    let (host, _) = host_with_limits(quick_teardown_limits());
    let (session, _turn) = session_with_a_running_turn(&server, &host).await;

    session
        .close(CloseReason::Shutdown)
        .await
        .expect("expected the close to detach even though the interrupt went unanswered");
    server.until_disconnections(1).await;

    let methods = server.methods();
    assert!(
        methods.iter().any(|method| method == "turn/interrupt"),
        "expected the interrupt to have been sent, received {methods:?}"
    );
}

/// A cancel whose interrupt was accepted but whose turn never ends escalates past
/// `cancel_settle_timeout` to a disconnect: the interrupt went out first, and nothing is signalled.
#[tokio::test]
async fn cancelling_a_turn_the_server_never_ends_escalates_to_a_disconnect() {
    let server = FakeAppServerSocket::holding_a_turn_open(Interrupt::AcknowledgedOnly);
    let (host, launcher) = host_with_limits(quick_teardown_limits());
    let (session, mut turn) = session_with_a_running_turn(&server, &host).await;

    session
        .cancel(CancelReason::Requested)
        .await
        .expect("expected the cancel to settle by detaching");
    server.until_disconnections(1).await;

    let methods = server.methods();
    assert!(
        position_of(&methods, "turn/start") < position_of(&methods, "turn/interrupt"),
        "expected `turn/interrupt` after `turn/start` and before the disconnect, received {methods:?}"
    );
    let events = drain(&mut turn).await;
    assert!(
        events.iter().any(|kind| matches!(
            kind,
            EventKind::Cancelled {
                reason: CancelReason::Requested
            }
        )),
        "expected the turn to be cancelled with reason Requested, received {events:#?}"
    );
    assert_eq!(launcher.launches().len(), 0);
}

/// `kill` on an attached peer signals nothing, so a connection can only end through its link. An
/// `open_session` abandoned while the server holds `initialize` must still drop the connection.
#[tokio::test]
async fn abandoning_an_open_session_ends_the_connection_without_a_kill() {
    let server = FakeAppServerSocket::silent();
    let (host, _) = host_without_processes();
    let harness = server.harness();
    let opening = tokio::spawn(async move {
        harness
            .open_session(&host, over_socket("chat-1"))
            .await
            .map(|_| ())
    });

    server.until_method("initialize").await;
    opening.abort();
    server.until_disconnections(1).await;
}

/// A server that is alive but has stopped reading leaves the session's writer blocked mid-message.
/// The attachment is ended from this side: the session reaches `Closed`, the teardown owes the host
/// no cleanup, and the server sees the connection end. The close may still report that the link's
/// own shutdown timed out, as it does for a child whose pipe is blocked.
#[tokio::test]
async fn closing_while_the_server_has_stopped_reading_still_detaches() {
    let server = FakeAppServerSocket::stalling_after_thread_start();
    let (host, _) = host_with_limits(quick_teardown_limits());
    let session = server
        .harness()
        .open_session(&host, over_socket("chat-1"))
        .await
        .expect("expected a session");

    // A message far past a socket buffer, but within the line cap: the write blocks mid-frame.
    let prompt = "x".repeat(900_000);
    let starting = session.start_turn(TurnRequest::new("turn-1", prompt));
    let closing = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        session.close(CloseReason::Shutdown).await
    };
    let (_, closed) = tokio::join!(starting, closing);

    if let Err(error) = &closed {
        assert!(
            error.cleanup_control().is_none(),
            "expected no cleanup owed to the host for an attachment, received {error:?}"
        );
    }
    assert_eq!(
        session.subscribe().current().status,
        SessionStatus::Closed,
        "expected the attachment to be ended, close returned {closed:?}"
    );
    server.until_disconnections(1).await;
}

/// A listener that accepts a connection and never answers the upgrade: the dial is stuck until its
/// own deadline unless something else ends it.
struct FakeSocketThatNeverUpgrades {
    /// Held so the socket's directory outlives the test.
    _directory: tempfile::TempDir,
    path: PathBuf,
    accepted: Arc<Notify>,
}

impl FakeSocketThatNeverUpgrades {
    fn start() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("mea-")
            .tempdir()
            .expect("expected a temporary directory");
        let path = directory.path().join("app-server.sock");
        let listener = UnixListener::bind(&path).expect("expected a listener");
        let accepted = Arc::new(Notify::new());
        let signal = Arc::clone(&accepted);
        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            signal.notify_one();
            // Holds the connection open, without a byte, past every deadline in the test.
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(stream);
        });
        Self {
            _directory: directory,
            path,
            accepted,
        }
    }
}

/// The host's cancel ends a dial that is waiting on the server, as `Cancelled` and not as the
/// `request_timeout` it would otherwise sit out, and nothing was submitted.
#[tokio::test]
async fn cancelling_the_host_during_the_dial_is_cancelled_not_a_timeout() {
    let server = FakeSocketThatNeverUpgrades::start();
    let cancel = CancelToken::new();
    let host = HostContext::builder()
        .launcher(Arc::new(FakeLauncher::new()))
        .cwd(workspace_path())
        .client_info("mango-test", "0.0.1")
        .cancel(cancel.clone())
        .limits(mango_external_agents::Limits {
            request_timeout: Duration::from_secs(30),
            ..mango_external_agents::Limits::default()
        })
        .build()
        .expect("expected a host");
    let harness = CodexHarness::new().with_unix_socket(&server.path);

    let opening = tokio::spawn(async move {
        harness
            .open_session(&host, over_socket("chat-1"))
            .await
            .map(|_| ())
    });
    server.accepted.notified().await;
    cancel.cancel();

    let error = tokio::time::timeout(Duration::from_secs(2), opening)
        .await
        .expect(
            "expected the open to end within 2s of the cancel, well before request_timeout (30s)",
        )
        .expect("expected the open task to finish")
        .expect_err("expected a refusal, received a session");
    assert!(
        matches!(error.cause(), Error::Cancelled { .. }),
        "expected Cancelled, received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
}

/// A host that was cancelled before the open never dials the server.
#[tokio::test]
async fn a_host_cancelled_before_the_open_never_dials() {
    let server = FakeAppServerSocket::replaying_turn();
    let cancel = CancelToken::new();
    cancel.cancel();
    let host = HostContext::builder()
        .launcher(Arc::new(FakeLauncher::new()))
        .cwd(workspace_path())
        .client_info("mango-test", "0.0.1")
        .cancel(cancel)
        .build()
        .expect("expected a host");

    let error = server
        .harness()
        .open_session(&host, over_socket("chat-1"))
        .await
        .err()
        .expect("expected a refusal, received a session");
    assert!(
        matches!(error.cause(), Error::Cancelled { .. }),
        "expected Cancelled, received {error:?}"
    );
    assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
    assert_eq!(server.connections.load(Ordering::SeqCst), 0);
}
