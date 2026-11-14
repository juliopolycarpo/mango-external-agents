//! The WebSocket link shared by every transport that speaks it.
//!
//! A dialled `ws://` URL and a Unix socket carrying the same upgraded connection differ only in
//! the stream under the handshake, so the framing, the cap and the disconnect rules live here once.
//! Neither transport module reimplements them: the [`Link`] halves are generic over the stream.

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::error::{CapacityError, ProtocolError};
use tokio_tungstenite::tungstenite::{Bytes, Message};

use crate::error::{Error, Result};
use crate::link::{Link, LinkReceiver, LinkSender};
use crate::process::LineLimits;
use crate::redact;

/// The link for an already-upgraded socket over any stream.
///
/// `peer` is repeated in every link error, so it is redacted here: a caller may pass a URL.
pub(crate) fn from_socket<S>(socket: WebSocketStream<S>, peer: &str, limits: LineLimits) -> Link
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let peer = peer_label(peer);
    let (sink, stream) = socket.split();
    Link::new(
        Box::new(SocketSender {
            sink,
            peer: peer.clone(),
        }),
        Box::new(SocketReceiver {
            stream,
            peer,
            max_bytes: limits.max_line_bytes,
        }),
    )
}

/// A socket failure as a structured summary, from the variant rather than from its text.
///
/// `WsError`'s own `Display` repeats the URL it was dialling, and the URL is the host's — it can
/// carry a password in its userinfo or a token in its query string. The variant is the half that
/// says what to do about it, and an HTTP status and an [`std::io::ErrorKind`] are both bounded
/// enumerations the peer cannot choose the shape of.
pub(crate) fn ws_reason(error: &WsError) -> String {
    match error {
        WsError::ConnectionClosed => String::from("a closed connection"),
        WsError::AlreadyClosed => String::from("a connection that was already closed"),
        WsError::Io(io) => format!("a socket failure ({:?})", io.kind()),
        WsError::Tls(_) => String::from("a TLS failure"),
        WsError::Url(_) => String::from("a URL this transport cannot dial"),
        WsError::Http(response) => format!("HTTP {}", response.status()),
        WsError::HttpFormat(_) => String::from("a malformed HTTP response"),
        WsError::Capacity(_) => String::from("a frame past this link's cap"),
        WsError::Protocol(_) => String::from("a frame the WebSocket protocol does not allow"),
        WsError::WriteBufferFull(_) => String::from("a full write buffer"),
        WsError::Utf8(_) => String::from("a text frame that is not UTF-8"),
        WsError::AttackAttempt => String::from("a handshake this transport refused"),
    }
}

/// The endpoint as it may be repeated in every error a host logs.
///
/// A URL configured as `wss://svc:s3cr3t@agent.internal/ws` otherwise carries its password into
/// the `Display` of each link failure, and the password is the half nobody needs to diagnose one.
pub(crate) fn peer_label(url: &str) -> String {
    redact::message_text(url)
}

struct SocketSender<S> {
    sink: SplitSink<WebSocketStream<S>, Message>,
    peer: String,
}

#[async_trait::async_trait]
impl<S> LinkSender for SocketSender<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn send(&mut self, message: String) -> Result<()> {
        self.sink
            .send(Message::Text(message.into()))
            .await
            .map_err(|error| Error::Link {
                peer: self.peer.clone(),
                message: ws_reason(&error),
            })
    }

    async fn close(&mut self) -> Result<()> {
        // A peer that already hung up is not a failure to close: the conversation is over either
        // way, and reporting one would fail an ordinary shutdown.
        let _ = self.sink.close().await;
        Ok(())
    }
}

struct SocketReceiver<S> {
    stream: SplitStream<WebSocketStream<S>>,
    peer: String,
    max_bytes: usize,
}

impl<S> SocketReceiver<S> {
    /// What [`LinkReceiver::recv`] promises: more than the library will assemble is refused by
    /// name rather than returned.
    fn bounded(&self, message: String) -> Result<Option<String>> {
        if message.len() > self.max_bytes {
            return Err(Error::LimitExceeded {
                subject: "bytes of one websocket message",
                limit: self.max_bytes,
                received: message.len(),
            });
        }
        Ok(Some(message))
    }
}

#[async_trait::async_trait]
impl<S> LinkReceiver for SocketReceiver<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
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
                // The socket caught the cap before the message was assembled. Reported the same
                // way the check below reports it, so a host sees one shape whichever layer noticed.
                Err(WsError::Capacity(CapacityError::MessageTooLong { size, max_size })) => {
                    return Err(Error::LimitExceeded {
                        subject: "bytes of one websocket message",
                        limit: max_size,
                        received: size,
                    });
                }
                Err(error) => {
                    return Err(Error::Link {
                        peer: self.peer.clone(),
                        message: ws_reason(&error),
                    });
                }
            };
            match message {
                Message::Text(text) => return self.bounded(text.to_string()),
                // A dialect that frames its JSON as binary is still sending text; a stray byte is
                // replaced rather than ending a turn that is otherwise going fine.
                Message::Binary(bytes) => return self.bounded(decode(&bytes)),
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
