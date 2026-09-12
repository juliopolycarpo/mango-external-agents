//! A dialled WebSocket endpoint.
//!
//! Experimental, and behind the `websocket` feature: only a vendor that documents a socket
//! endpoint can be driven this way, and the library dials nothing the host did not configure.
//!
//! TLS is `ring`, everywhere and by construction: `deny.toml` bans the alternatives across every
//! target the release builds, and `scripts/check-tls.sh` proves the tree is clean.

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{Bytes, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

use crate::error::{Error, Result};
use crate::link::{Link, LinkReceiver, LinkSender};
use crate::transport::WsSpec;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Dials `spec` and returns the link.
///
/// The bearer token, when the host passed one, is sent as an `Authorization` header on the
/// handshake and nowhere else. The library never reads, stores or derives one: a vendor login is
/// not a source for it, because the library does not handle logins.
///
/// # Errors
///
/// [`Error::Link`] when the URL is unusable, the bearer is not a valid header value, or the
/// handshake failed.
pub async fn dial(spec: &WsSpec) -> Result<Link> {
    let mut request = spec
        .url
        .as_str()
        .into_client_request()
        .map_err(|error| link_error(spec, error))?;

    if let Some(bearer) = &spec.bearer {
        let value =
            HeaderValue::from_str(&format!("Bearer {bearer}")).map_err(|_| Error::Link {
                peer: spec.url.clone(),
                // Never the value: a malformed credential is still a credential.
                message: String::from("expected a bearer token usable as a header value"),
            })?;
        request.headers_mut().insert("Authorization", value);
    }

    let (socket, _) = connect_async(request)
        .await
        .map_err(|error| link_error(spec, error))?;
    Ok(from_socket(socket, spec.url.clone()))
}

/// The link for an already-dialled socket.
///
/// Exposed for a host that dials on its own terms — through a proxy, or onto a listener it opened
/// itself — and for the tests here.
pub fn from_socket(socket: Socket, peer: String) -> Link {
    let (sink, stream) = socket.split();
    Link::new(
        Box::new(SocketSender {
            sink,
            peer: peer.clone(),
        }),
        Box::new(SocketReceiver { stream, peer }),
    )
}

fn link_error(spec: &WsSpec, error: impl std::fmt::Display) -> Error {
    Error::Link {
        peer: spec.url.clone(),
        message: error.to_string(),
    }
}

struct SocketSender {
    sink: SplitSink<Socket, Message>,
    peer: String,
}

#[async_trait::async_trait]
impl LinkSender for SocketSender {
    async fn send(&mut self, message: String) -> Result<()> {
        self.sink
            .send(Message::Text(message.into()))
            .await
            .map_err(|error| Error::Link {
                peer: self.peer.clone(),
                message: error.to_string(),
            })
    }

    async fn close(&mut self) -> Result<()> {
        // A peer that already hung up is not a failure to close: the conversation is over either
        // way, and reporting one would fail an ordinary shutdown.
        let _ = self.sink.close().await;
        Ok(())
    }
}

struct SocketReceiver {
    stream: SplitStream<Socket>,
    peer: String,
}

#[async_trait::async_trait]
impl LinkReceiver for SocketReceiver {
    async fn recv(&mut self) -> Result<Option<String>> {
        loop {
            let Some(message) = self.stream.next().await else {
                return Ok(None);
            };
            let message = match message {
                Ok(message) => message,
                // A peer that vanished is an ended conversation, not a failure to report
                // differently: a vendor process that dies takes its socket with it, and whatever
                // was waiting on an answer is failed by the layer above with a message that says
                // the peer exited.
                Err(error) if is_disconnect(&error) => return Ok(None),
                Err(error) => {
                    return Err(Error::Link {
                        peer: self.peer.clone(),
                        message: error.to_string(),
                    });
                }
            };
            match message {
                Message::Text(text) => return Ok(Some(text.to_string())),
                // A dialect that frames its JSON as binary is still sending text; a stray byte is
                // replaced rather than ending a turn that is otherwise going fine.
                Message::Binary(bytes) => return Ok(Some(decode(&bytes))),
                Message::Close(_) => return Ok(None),
                // Keepalives are the library's business, not the harness's.
                Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            }
        }
    }
}

fn decode(bytes: &Bytes) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Whether this is a peer that went away rather than a peer that misbehaved.
///
/// Half the CLIs that would ever be driven over a socket exit by exiting: the process dies, the
/// connection resets, and no closing handshake happens. Reporting that as a protocol error would
/// make an ordinary shutdown look like a bug.
fn is_disconnect(error: &WsError) -> bool {
    match error {
        WsError::ConnectionClosed
        | WsError::AlreadyClosed
        | WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake) => true,
        WsError::Io(io) => matches!(
            io.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{dial, from_socket};
    use crate::transport::WsSpec;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    /// Accepts one connection, echoes every text frame back with a prefix, then closes.
    async fn echo_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("expected a listener");
        let port = listener.local_addr().expect("expected an address").port();

        tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut socket) = accept_async(stream).await else {
                return;
            };
            socket.send(Message::Ping(Vec::new().into())).await.ok();
            while let Some(Ok(message)) = socket.next().await {
                match message {
                    Message::Text(text) => {
                        if socket
                            .send(Message::Text(format!("echo:{text}").into()))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Message::Close(_) => return,
                    _ => {}
                }
            }
        });

        format!("ws://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn dials_and_round_trips_a_message() {
        let url = echo_server().await;
        let link = dial(&WsSpec::new(url))
            .await
            .expect("expected the dial to land");
        let (mut sender, mut receiver) = link.split();

        sender
            .send(String::from(r#"{"method":"ping"}"#))
            .await
            .expect("expected the send to land");
        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from(r#"echo:{"method":"ping"}"#))
        );
    }

    #[tokio::test]
    async fn a_closed_socket_ends_the_link_rather_than_failing() {
        let url = echo_server().await;
        let link = dial(&WsSpec::new(url))
            .await
            .expect("expected the dial to land");
        let (mut sender, mut receiver) = link.split();

        sender.close().await.expect("expected the close to land");
        assert_eq!(receiver.recv().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn a_peer_that_vanishes_ends_the_link_rather_than_reporting_a_protocol_error() {
        // A vendor process that dies takes its socket with it: no closing handshake, just a reset.
        // That is an ended conversation, and the layer above already says the peer exited.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("expected a listener");
        let port = listener.local_addr().expect("expected an address").port();
        tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                let socket = accept_async(stream).await;
                drop(socket);
            }
        });

        let link = dial(&WsSpec::new(format!("ws://127.0.0.1:{port}")))
            .await
            .expect("expected the dial to land");
        let (_, mut receiver) = link.split();

        assert_eq!(receiver.recv().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn a_dial_to_nowhere_names_the_endpoint_rather_than_panicking() {
        // Port 1 is reserved and nothing is listening on it.
        let error = dial(&WsSpec::new("ws://127.0.0.1:1"))
            .await
            .expect_err("expected a refusal, received a link");
        assert!(
            error.to_string().contains("127.0.0.1:1"),
            "expected the endpoint to be named, received {error}"
        );
    }

    #[tokio::test]
    async fn an_unusable_url_is_refused_before_anything_is_dialled() {
        let error = dial(&WsSpec::new("not a url"))
            .await
            .expect_err("expected a refusal, received a link");
        assert!(
            matches!(error, crate::Error::Link { .. }),
            "expected a link refusal, received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_bearer_that_cannot_be_a_header_is_refused_without_echoing_it() {
        let error = dial(&WsSpec::new("ws://127.0.0.1:1").with_bearer("bad\nvalue"))
            .await
            .expect_err("expected a refusal, received a link");
        assert!(
            !error.to_string().contains("bad"),
            "expected the credential not to be echoed, received {error}"
        );
    }

    #[tokio::test]
    async fn keepalives_never_reach_the_harness_above() {
        // The server pings on connect; the receiver must skip it and answer with the text frame.
        let url = echo_server().await;
        let link = dial(&WsSpec::new(url))
            .await
            .expect("expected the dial to land");
        let (mut sender, mut receiver) = link.split();

        sender
            .send(String::from("hello"))
            .await
            .expect("expected the send to land");
        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from("echo:hello"))
        );
    }

    #[tokio::test]
    async fn a_link_can_be_built_from_a_socket_a_host_dialled_itself() {
        let url = echo_server().await;
        let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .expect("expected the dial to land");
        let (mut sender, mut receiver) = from_socket(socket, url).split();

        sender
            .send(String::from("hi"))
            .await
            .expect("expected the send to land");
        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from("echo:hi"))
        );
    }
}
