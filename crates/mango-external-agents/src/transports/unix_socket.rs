//! A Unix socket a vendor server already listens on, upgraded to a WebSocket.
//!
//! Behind the `unix-socket` feature and Unix only. The host names the socket; the library dials it
//! and speaks one JSON-RPC message per WebSocket text frame over the upgraded connection, with the
//! framing and caps of [`websocket`](super::websocket). Nothing here starts, stops or signals the
//! server: it is the host's, and other clients may be attached to it.
//!
//! There is no credential on this path. A server that answers the upgrade with `401` or `403`
//! wants a token, and the library refuses that as host configuration instead of presenting one.

use std::path::Path;
use std::sync::Arc;

use tokio::net::UnixStream;
use tokio::sync::watch;
use tokio_tungstenite::client_async_with_config;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::socket::{self, ws_reason};
use crate::CancelReason;
use crate::error::{Error, Result};
use crate::host::Limits;
use crate::link::{Link, LinkReceiver, LinkSender};
use crate::process::{ExitStatus, ProcessControl};
use crate::transport::UnixSocketSpec;

/// What a link error says about the peer. Never the path: a host's directory layout can name a
/// user or a project.
const PEER: &str = "Unix socket app-server";

/// The longest path `sockaddr_un` holds, less its terminating NUL.
///
/// The kernel field is 108 bytes on Linux and 104 on the BSDs and macOS. A longer path would fail
/// the connect with an `InvalidInput` that names neither number.
#[cfg(any(target_os = "linux", target_os = "android"))]
const MAX_PATH_BYTES: usize = 107;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const MAX_PATH_BYTES: usize = 103;

/// The request target and `Host` of the upgrade. Both are fixed: the socket is the address, and a
/// server that listens on one has no virtual hosts to choose between.
const UPGRADE_URL: &str = "ws://localhost/";

/// A connection to a server the host owns: a link, and the handle that reports when it ended.
pub struct UnixSocketTransport {
    /// Messages in and out, one WebSocket text frame each.
    pub link: Link,
    /// The attachment, for waiting until it ends. Its `kill` signals nothing; closing the link is
    /// what ends the attachment. See [`AttachedPeer`].
    pub control: Arc<dyn ProcessControl>,
}

impl std::fmt::Debug for UnixSocketTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("UnixSocketTransport")
            .finish_non_exhaustive()
    }
}

/// Dials `spec`, upgrades the connection and returns the link, bounded by the host's own limits.
///
/// Connecting and the HTTP upgrade together are bounded by [`Limits::request_timeout`]. The
/// message cap is the host's stdio line cap, [`LineLimits::max_line_bytes`](crate::LineLimits),
/// applied to the socket and to every assembled message.
///
/// # Errors
///
/// - [`Error::HostConfiguration`] for a path that is not absolute or does not fit a socket address,
///   and for a server that answers the upgrade with `401` or `403`: it wants a token, which this
///   library never presents.
/// - [`Error::Timeout`] when the connect and upgrade do not finish within the bound.
/// - [`Error::Link`] when nothing listens at the path or the upgrade failed otherwise.
pub async fn open(spec: &UnixSocketSpec, limits: &Limits) -> Result<UnixSocketTransport> {
    check_path(&spec.path)?;
    let upgraded = tokio::time::timeout(limits.request_timeout, upgrade(&spec.path, limits))
        .await
        .map_err(|_| Error::Timeout {
            operation: String::from("Unix socket connect and upgrade"),
            after: limits.request_timeout,
        })??;
    Ok(attach(upgraded, limits))
}

async fn upgrade(
    path: &Path,
    limits: &Limits,
) -> Result<tokio_tungstenite::WebSocketStream<UnixStream>> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|error| Error::Link {
            peer: String::from(PEER),
            message: format!("a socket failure ({:?})", error.kind()),
        })?;
    let request = UPGRADE_URL
        .into_client_request()
        .map_err(|error| refusal(&error))?;
    // Both caps, as `websocket::dial` sets them: a peer that never finishes a frame would
    // otherwise buffer the socket library's 16 MiB first.
    let config = WebSocketConfig::default()
        .max_message_size(Some(limits.line.max_line_bytes))
        .max_frame_size(Some(limits.line.max_line_bytes));
    let (socket, _) = client_async_with_config(request, stream, Some(config))
        .await
        .map_err(|error| refusal(&error))?;
    Ok(socket)
}

/// An upgrade that did not complete, as the host should read it.
fn refusal(error: &WsError) -> Error {
    if let WsError::Http(response) = error {
        let status = response.status();
        if status == 401 || status == 403 {
            return Error::HostConfiguration {
                expected: "a Unix socket that upgrades without a credential, because this library never presents one",
                received: format!("HTTP {status} from the upgrade"),
            };
        }
    }
    Error::Link {
        peer: String::from(PEER),
        message: ws_reason(error),
    }
}

/// Refuses a path the dial could not honour, before anything is connected.
///
/// The received text is a shape and a length, never the path itself.
fn check_path(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::HostConfiguration {
            expected: "an absolute Unix socket path",
            received: format!("a relative path of {} bytes", path.as_os_str().len()),
        });
    }
    let length = path.as_os_str().len();
    if length > MAX_PATH_BYTES {
        return Err(Error::HostConfiguration {
            expected: "a Unix socket path that fits a socket address",
            received: format!("a path of {length} bytes, longer than {MAX_PATH_BYTES}"),
        });
    }
    Ok(())
}

/// Builds the link over the upgraded stream, tied to a signal that says when the attachment ended.
fn attach(
    upgraded: tokio_tungstenite::WebSocketStream<UnixStream>,
    limits: &Limits,
) -> UnixSocketTransport {
    let (ended, observer) = watch::channel(false);
    let ended = Arc::new(ended);
    let (sender, receiver) = socket::from_socket(upgraded, PEER, limits.line).split();
    UnixSocketTransport {
        link: Link::new(
            Box::new(EndsOnClose {
                inner: sender,
                ended: Arc::clone(&ended),
            }),
            Box::new(EndsOnEof {
                inner: receiver,
                ended,
            }),
        ),
        control: Arc::new(AttachedPeer { ended: observer }),
    }
}

/// The control for a server this library attached to but does not own.
///
/// It answers the questions the harness's teardown asks of a child, honestly for a peer that is not
/// one: there is no process id, no stderr, and no signal is ever sent. [`kill`](Self::kill)
/// does nothing, because ending the *attachment* is the link's close, which the harness has done
/// by the time it asks. `wait` resolves when the link ended, whether this side closed
/// it or the server went away, so a teardown reports the same fact it would for a reaped child.
///
/// The server keeps running afterwards, with whatever conversation state it held.
#[derive(Debug)]
pub struct AttachedPeer {
    ended: watch::Receiver<bool>,
}

#[async_trait::async_trait]
impl ProcessControl for AttachedPeer {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        String::new()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        let mut ended = self.ended.clone();
        // An error means every sender is gone, which is the link ending as well.
        let _ = ended.wait_for(|ended| *ended).await;
        Ok(ExitStatus::default())
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        Ok(())
    }
}

/// Marks the attachment ended when this side closes the connection.
struct EndsOnClose {
    inner: Box<dyn LinkSender>,
    ended: Arc<watch::Sender<bool>>,
}

#[async_trait::async_trait]
impl LinkSender for EndsOnClose {
    async fn send(&mut self, message: String) -> Result<()> {
        self.inner.send(message).await
    }

    async fn close(&mut self) -> Result<()> {
        let closed = self.inner.close().await;
        self.ended.send_replace(true);
        closed
    }
}

/// Marks the attachment ended when the server closes it or the read fails.
struct EndsOnEof {
    inner: Box<dyn LinkReceiver>,
    ended: Arc<watch::Sender<bool>>,
}

#[async_trait::async_trait]
impl LinkReceiver for EndsOnEof {
    async fn recv(&mut self) -> Result<Option<String>> {
        let received = self.inner.recv().await;
        if !matches!(received, Ok(Some(_))) {
            self.ended.send_replace(true);
        }
        received
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use tokio::net::UnixListener;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
    use tokio_tungstenite::tungstenite::http::StatusCode;
    use tokio_tungstenite::{accept_async, accept_hdr_async};

    use super::open;
    use crate::error::Error;
    use crate::host::Limits;
    use crate::transport::UnixSocketSpec;

    /// What a [`FakeUnixSocketServer`] does with the one connection it accepts.
    #[derive(Clone, Copy)]
    enum Behaviour {
        /// Upgrades, pings once, then answers every text frame with `echo:<frame>`.
        Echo,
        /// Upgrades, then drops the connection without a closing handshake.
        HangUpAfterUpgrade,
        /// Answers the upgrade with this HTTP status instead of `101`.
        RefuseUpgrade(StatusCode),
        /// Accepts the connection and never answers the upgrade.
        Silent,
    }

    /// What the upgrade request carried, as the server saw it.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct SeenUpgrade {
        target: String,
        host: Option<String>,
        has_authorization: bool,
    }

    /// An in-process peer on a real Unix socket, standing in for `codex app-server --listen unix://`.
    struct FakeUnixSocketServer {
        /// Held so the socket's directory outlives the test.
        _directory: tempfile::TempDir,
        path: PathBuf,
        seen: Arc<Mutex<Option<SeenUpgrade>>>,
    }

    impl FakeUnixSocketServer {
        fn start(behaviour: Behaviour) -> Self {
            // A short, private directory: a socket path is capped near 100 bytes.
            let directory = tempfile::Builder::new()
                .prefix("mea-")
                .tempdir()
                .expect("expected a temporary directory");
            let path = directory.path().join("s.sock");
            let listener = UnixListener::bind(&path).expect("expected a listener");
            let seen = Arc::new(Mutex::new(None));
            let recorded = Arc::clone(&seen);
            tokio::spawn(async move {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                if matches!(behaviour, Behaviour::Silent) {
                    // Holds the connection open without a byte, long past any test deadline.
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    drop(stream);
                    return;
                }
                // The `Err` type is the socket library's own callback contract.
                #[allow(clippy::result_large_err)]
                let upgrade = |request: &Request, response: Response| {
                    *recorded.lock().unwrap_or_else(PoisonError::into_inner) = Some(SeenUpgrade {
                        target: request.uri().to_string(),
                        host: request
                            .headers()
                            .get("host")
                            .and_then(|value| value.to_str().ok())
                            .map(str::to_owned),
                        has_authorization: request.headers().contains_key("authorization"),
                    });
                    if let Behaviour::RefuseUpgrade(status) = behaviour {
                        let mut refusal = ErrorResponse::new(None);
                        *refusal.status_mut() = status;
                        return Err(refusal);
                    }
                    Ok(response)
                };
                let Ok(mut socket) = accept_hdr_async(stream, upgrade).await else {
                    return;
                };
                match behaviour {
                    Behaviour::Echo => {
                        socket.send(Message::Ping(Vec::new().into())).await.ok();
                        while let Some(Ok(message)) = socket.next().await {
                            match message {
                                Message::Text(text) => {
                                    let echoed = Message::Text(format!("echo:{text}").into());
                                    if socket.send(echoed).await.is_err() {
                                        return;
                                    }
                                }
                                Message::Close(_) => return,
                                _ => {}
                            }
                        }
                    }
                    Behaviour::HangUpAfterUpgrade => drop(socket),
                    Behaviour::RefuseUpgrade(_) | Behaviour::Silent => {}
                }
            });
            Self {
                _directory: directory,
                path,
                seen,
            }
        }

        fn spec(&self) -> UnixSocketSpec {
            UnixSocketSpec::new(&self.path)
        }

        fn seen_upgrade(&self) -> Option<SeenUpgrade> {
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }
    }

    fn short_limits() -> Limits {
        Limits {
            request_timeout: Duration::from_millis(200),
            ..Limits::default()
        }
    }

    #[tokio::test]
    async fn dials_upgrades_and_round_trips_a_message_skipping_keepalives() {
        let server = FakeUnixSocketServer::start(Behaviour::Echo);
        let transport = open(&server.spec(), &Limits::default())
            .await
            .expect("expected the dial to land");
        let (mut sender, mut receiver) = transport.link.split();

        sender
            .send(String::from(r#"{"method":"initialize"}"#))
            .await
            .expect("expected the send to land");
        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from(r#"echo:{"method":"initialize"}"#))
        );
    }

    /// The real server accepts any target and host; the fixed pair is what this transport promises.
    #[tokio::test]
    async fn the_upgrade_names_a_fixed_target_and_carries_no_credential() {
        let server = FakeUnixSocketServer::start(Behaviour::Echo);
        let transport = open(&server.spec(), &Limits::default())
            .await
            .expect("expected the dial to land");
        let (mut sender, mut receiver) = transport.link.split();
        sender.send(String::from("x")).await.expect("expected send");
        receiver.recv().await.expect("expected the echo");

        assert_eq!(
            server.seen_upgrade(),
            Some(SeenUpgrade {
                target: String::from("/"),
                host: Some(String::from("localhost")),
                has_authorization: false,
            }),
            "expected a bare upgrade to / on localhost"
        );
    }

    #[tokio::test]
    async fn the_attachment_is_pending_until_this_side_closes_the_link() {
        let server = FakeUnixSocketServer::start(Behaviour::Echo);
        let transport = open(&server.spec(), &Limits::default())
            .await
            .expect("expected the dial to land");
        let control = Arc::clone(&transport.control);
        let (mut sender, _receiver) = transport.link.split();

        assert_eq!(
            control.pid(),
            None,
            "expected an attached peer to have no pid"
        );
        assert_eq!(control.stderr_tail(), "");
        let pending = tokio::time::timeout(Duration::from_millis(50), control.wait()).await;
        assert!(
            pending.is_err(),
            "expected wait to stay pending on a live link, received {pending:?}"
        );

        sender.close().await.expect("expected the close to land");
        tokio::time::timeout(Duration::from_secs(2), control.wait())
            .await
            .expect("expected wait to resolve once the link closed")
            .expect("expected a status");
    }

    #[tokio::test]
    async fn a_server_that_hangs_up_ends_the_link_and_the_attachment() {
        let server = FakeUnixSocketServer::start(Behaviour::HangUpAfterUpgrade);
        let transport = open(&server.spec(), &Limits::default())
            .await
            .expect("expected the dial to land");
        let control = Arc::clone(&transport.control);
        let (_sender, mut receiver) = transport.link.split();

        assert_eq!(receiver.recv().await.expect("expected the end"), None);
        tokio::time::timeout(Duration::from_secs(2), control.wait())
            .await
            .expect("expected wait to resolve once the server went away")
            .expect("expected a status");
    }

    #[tokio::test]
    async fn killing_an_attached_peer_signals_nothing_and_reports_no_failure() {
        let server = FakeUnixSocketServer::start(Behaviour::Echo);
        let transport = open(&server.spec(), &Limits::default())
            .await
            .expect("expected the dial to land");

        transport
            .control
            .kill(crate::CancelReason::Shutdown)
            .await
            .expect("expected kill on an attached peer to succeed without effect");
        let (mut sender, mut receiver) = transport.link.split();
        sender
            .send(String::from("still"))
            .await
            .expect("expected send");
        assert_eq!(
            receiver.recv().await.expect("expected the echo"),
            Some(String::from("echo:still")),
            "expected the connection to survive a kill: the server is not this library's to end"
        );
    }

    #[tokio::test]
    async fn a_message_past_the_hosts_cap_is_refused_by_name() {
        let server = FakeUnixSocketServer::start(Behaviour::Echo);
        let limits = Limits {
            line: crate::process::LineLimits {
                max_line_bytes: 64,
                max_buffered_bytes: 128,
            },
            ..Limits::default()
        };
        let transport = open(&server.spec(), &limits)
            .await
            .expect("expected the dial to land");
        let (mut sender, mut receiver) = transport.link.split();
        sender.send("x".repeat(100)).await.expect("expected send");

        let error = receiver
            .recv()
            .await
            .expect_err("expected a refusal, received a message");
        assert!(
            matches!(
                error,
                Error::LimitExceeded {
                    subject: "bytes of one websocket message",
                    limit: 64,
                    received: 105,
                }
            ),
            "expected the cap to be named, received {error:?}"
        );
    }

    /// A server that wants a token is refused as the host's configuration, and the refusal carries
    /// neither a token nor the path.
    #[tokio::test]
    async fn an_upgrade_that_wants_a_token_is_host_configuration() {
        for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
            let server = FakeUnixSocketServer::start(Behaviour::RefuseUpgrade(status));
            let error = open(&server.spec(), &Limits::default())
                .await
                .expect_err("expected a refusal, received a link");
            assert!(
                matches!(&error, Error::HostConfiguration { received, .. }
                    if received == &format!("HTTP {status} from the upgrade")),
                "expected host configuration naming HTTP {status}, received {error:?}"
            );
            assert!(
                !error.to_string().contains(&*server.path.to_string_lossy()),
                "expected no socket path in the refusal, received {error}"
            );
        }
    }

    #[tokio::test]
    async fn any_other_refused_upgrade_is_a_link_failure_naming_the_status() {
        let server = FakeUnixSocketServer::start(Behaviour::RefuseUpgrade(
            StatusCode::INTERNAL_SERVER_ERROR,
        ));
        let error = open(&server.spec(), &Limits::default())
            .await
            .expect_err("expected a refusal, received a link");
        assert!(
            matches!(&error, Error::Link { message, .. } if message == "HTTP 500 Internal Server Error"),
            "expected a link failure naming HTTP 500, received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_server_that_never_answers_the_upgrade_times_out() {
        let server = FakeUnixSocketServer::start(Behaviour::Silent);
        let error = open(&server.spec(), &short_limits())
            .await
            .expect_err("expected a timeout, received a link");
        assert!(
            matches!(&error, Error::Timeout { after, .. } if *after == Duration::from_millis(200)),
            "expected a timeout after 200ms, received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_socket_nobody_listens_on_is_a_link_failure_without_the_path() {
        let directory = tempfile::Builder::new()
            .prefix("mea-")
            .tempdir()
            .expect("expected a temporary directory");
        let absent = directory.path().join("absent.sock");

        let error = open(&UnixSocketSpec::new(&absent), &Limits::default())
            .await
            .expect_err("expected a refusal, received a link");
        assert!(
            matches!(&error, Error::Link { message, .. } if message == "a socket failure (NotFound)"),
            "expected an io error kind, received {error:?}"
        );
        assert!(
            !error.to_string().contains(&*absent.to_string_lossy()),
            "expected no socket path in the failure, received {error}"
        );
    }

    #[tokio::test]
    async fn a_relative_path_is_refused_before_anything_is_dialled() {
        let error = open(
            &UnixSocketSpec::new("run/app-server.sock"),
            &Limits::default(),
        )
        .await
        .expect_err("expected a refusal, received a link");
        assert!(
            matches!(&error, Error::HostConfiguration { expected, received }
                if *expected == "an absolute Unix socket path"
                    && received == "a relative path of 19 bytes"),
            "expected a relative path of 19 bytes, received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_path_past_the_socket_address_is_refused_with_its_length() {
        let long = format!("/{}", "a".repeat(super::MAX_PATH_BYTES));
        let error = open(&UnixSocketSpec::new(&long), &Limits::default())
            .await
            .expect_err("expected a refusal, received a link");
        let expected = format!(
            "a path of {} bytes, longer than {}",
            long.len(),
            super::MAX_PATH_BYTES
        );
        assert!(
            matches!(&error, Error::HostConfiguration { received, .. } if received == &expected),
            "expected {expected:?}, received {error:?}"
        );
        assert!(
            !error.to_string().contains("aaaa"),
            "expected no path in the refusal, received {error}"
        );
    }

    /// A plain `accept_async` peer completes the handshake, so the fake above is not what makes it
    /// work: the dial speaks the standard upgrade.
    #[tokio::test]
    async fn the_dial_completes_a_standard_server_handshake() {
        let directory = tempfile::Builder::new()
            .prefix("mea-")
            .tempdir()
            .expect("expected a temporary directory");
        let path = directory.path().join("s.sock");
        let listener = UnixListener::bind(&path).expect("expected a listener");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("expected a connection");
            accept_async(stream).await.is_ok()
        });

        open(&UnixSocketSpec::new(&path), &Limits::default())
            .await
            .expect("expected the dial to land");
        assert!(
            server.await.expect("expected the server task"),
            "expected a standard handshake"
        );
    }
}
