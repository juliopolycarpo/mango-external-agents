//! A dialled WebSocket endpoint.
//!
//! Experimental, and behind the `websocket` feature: only a vendor that documents a socket
//! endpoint can be driven this way, and the library dials nothing the host did not configure.
//!
//! TLS is `ring`, everywhere and by construction: `deny.toml` bans the alternatives across every
//! target the release builds, and `scripts/check-tls.sh` proves the tree is clean.

use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Error as WsError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

use super::socket::{self, peer_label, ws_reason};
use crate::error::{Error, Result};
use crate::link::Link;
use crate::process::LineLimits;
use crate::transport::WsSpec;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Dials `spec` and returns the link, bounded by the host's own caps.
///
/// `limits` is the host's — `host.limits().line` — rather than the socket library's. Left to
/// itself, tungstenite accepts a 64 MiB message and a 16 MiB frame, so a host that capped a stdio
/// line at 1 MiB would have been given sixty-four times that allowance the moment the same vendor
/// was reached over a socket.
///
/// The bearer token, when the host passed one, is sent as an `Authorization` header on the
/// handshake and nowhere else. The library never reads, stores or derives one: a vendor login is
/// not a source for it, because the library does not handle logins.
///
/// # Errors
///
/// [`Error::Link`] when the URL is unusable, the bearer is not a valid header value, or the
/// handshake failed.
pub async fn dial(spec: &WsSpec, limits: LineLimits) -> Result<Link> {
    let mut request = spec
        .url
        .as_str()
        .into_client_request()
        .map_err(|error| link_error(spec, &error))?;

    if let Some(bearer) = &spec.bearer {
        let value =
            HeaderValue::from_str(&format!("Bearer {bearer}")).map_err(|_| Error::Link {
                peer: peer_label(&spec.url),
                // Never the value: a malformed credential is still a credential.
                message: String::from("a bearer token unusable as a header value"),
            })?;
        request.headers_mut().insert("Authorization", value);
    }

    // Both caps, not just the message one: a peer that never finishes a frame would otherwise
    // buffer 16 MiB before anything noticed.
    let config = WebSocketConfig::default()
        .max_message_size(Some(limits.max_line_bytes))
        .max_frame_size(Some(limits.max_line_bytes));
    let (socket, _) = connect_async_with_config(request, Some(config), false)
        .await
        .map_err(|error| link_error(spec, &error))?;
    Ok(from_socket(socket, spec.url.clone(), limits))
}

/// The link for an already-dialled socket.
///
/// Exposed for a host that dials on its own terms — through a proxy, or onto a listener it opened
/// itself — and for the tests here. The cap is applied to every assembled message here as well as
/// to the socket, because a host that dialled for itself configured the socket for itself.
pub fn from_socket(socket: Socket, peer: String, limits: LineLimits) -> Link {
    socket::from_socket(socket, &peer, limits)
}

fn link_error(spec: &WsSpec, error: &WsError) -> Error {
    Error::Link {
        peer: peer_label(&spec.url),
        message: ws_reason(error),
    }
}

#[cfg(test)]
mod tests {
    use super::{dial, from_socket, peer_label};
    use crate::process::LineLimits;
    use crate::transport::WsSpec;
    use futures_util::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    /// A webhook-style URL carries its token as a path segment, with no name beside it.
    #[test]
    fn a_bare_token_in_the_url_path_is_not_repeated_in_the_peer_label() {
        let token = format!(
            "{}0123456789abcdefghijklmnopqrstuvwxyz",
            concat!("gh", "p_")
        );
        let label = peer_label(&format!("wss://agent.internal/hooks/{token}/ws"));
        assert_eq!(
            label, "wss://agent.internal/hooks/[REDACTED]/ws",
            "expected the path token out of the peer label | received: {label:?}"
        );
    }

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
        let link = dial(&WsSpec::new(url), LineLimits::default())
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
        let link = dial(&WsSpec::new(url), LineLimits::default())
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

        let link = dial(
            &WsSpec::new(format!("ws://127.0.0.1:{port}")),
            LineLimits::default(),
        )
        .await
        .expect("expected the dial to land");
        let (_, mut receiver) = link.split();

        assert_eq!(receiver.recv().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn a_dial_to_nowhere_retains_the_endpoint_as_data_without_panicking() {
        // Port 1 is reserved and nothing is listening on it.
        let error = dial(&WsSpec::new("ws://127.0.0.1:1"), LineLimits::default())
            .await
            .expect_err("expected a refusal, received a link");
        assert!(
            matches!(&error, crate::Error::Link { peer, .. } if peer.contains("127.0.0.1:1")),
            "expected the raw endpoint data, received {error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("a socket failure (ConnectionRefused)"),
            "expected the io error kind rather than the dialled URL, received {error}"
        );
    }

    #[tokio::test]
    async fn an_unusable_url_is_refused_before_anything_is_dialled() {
        let error = dial(&WsSpec::new("not a url"), LineLimits::default())
            .await
            .expect_err("expected a refusal, received a link");
        assert!(
            matches!(error, crate::Error::Link { .. }),
            "expected a link refusal, received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_bearer_that_cannot_be_a_header_is_refused_without_echoing_it() {
        let error = dial(
            &WsSpec::new("ws://127.0.0.1:1").with_bearer("bad\nvalue"),
            LineLimits::default(),
        )
        .await
        .expect_err("expected a refusal, received a link");
        assert!(
            !error.to_string().contains("bad"),
            "expected the credential not to be echoed, received {error}"
        );
    }

    #[test]
    fn a_bearer_never_reaches_a_debug_line() {
        let spec = WsSpec::new("wss://svc:s3cr3t@agent.internal/ws").with_bearer("sk-live-42");
        let rendered = format!("{spec:?}");

        assert!(
            !rendered.contains("sk-live-42"),
            "expected no bearer, received {rendered}"
        );
        assert!(
            !rendered.contains("s3cr3t"),
            "expected no url password, received {rendered}"
        );
        assert!(
            rendered.contains("endpoint_configured: true") && rendered.contains("secure: true"),
            "expected safe endpoint metadata, received {rendered}"
        );

        // `TransportSpec` derives its own `Debug` from this one, so the same must hold there.
        let wrapped = format!("{:?}", crate::TransportSpec::WebSocket(spec));
        assert!(
            !wrapped.contains("sk-live-42") && !wrapped.contains("s3cr3t"),
            "expected no credential through the wrapper, received {wrapped}"
        );
    }

    #[tokio::test]
    async fn a_url_password_never_reaches_a_link_error() {
        // Port 1 is reserved and nothing is listening on it, so the dial fails and the error is
        // the one a host would log.
        let error = dial(
            &WsSpec::new("ws://svc:s3cr3t@127.0.0.1:1"),
            LineLimits::default(),
        )
        .await
        .expect_err("expected a refusal, received a link");

        assert!(
            !error.to_string().contains("s3cr3t"),
            "expected no url password, received {error}"
        );
        assert!(
            matches!(&error, crate::Error::Link { peer, .. } if peer.contains("127.0.0.1:1")),
            "expected the raw endpoint data, received {error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("a socket failure (ConnectionRefused)"),
            "expected the io error kind rather than the dialled URL, received {error}"
        );
    }

    #[tokio::test]
    async fn keepalives_never_reach_the_harness_above() {
        // The server pings on connect; the receiver must skip it and answer with the text frame.
        let url = echo_server().await;
        let link = dial(&WsSpec::new(url), LineLimits::default())
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

    /// The cap a host set for a stdio line is the cap a socket gets. Without it the socket
    /// library's own 64 MiB allowance applies, and `recv` could never return the refusal its own
    /// documentation promises.
    #[tokio::test]
    async fn a_message_past_the_hosts_cap_is_refused_by_name() {
        let url = echo_server().await;
        let link = dial(
            &WsSpec::new(url),
            LineLimits {
                max_line_bytes: 64,
                max_buffered_bytes: 128,
            },
        )
        .await
        .expect("expected the dial to land");
        let (mut sender, mut receiver) = link.split();

        sender
            .send("x".repeat(100))
            .await
            .expect("expected the send to land");

        let error = receiver
            .recv()
            .await
            .expect_err("expected a refusal, received a message");
        assert!(
            matches!(
                error,
                crate::Error::LimitExceeded {
                    subject: "bytes of one websocket message",
                    limit: 64,
                    received: 105,
                }
            ),
            "expected the cap to be named, received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_link_can_be_built_from_a_socket_a_host_dialled_itself() {
        let url = echo_server().await;
        let (socket, _) = tokio_tungstenite::connect_async(url.as_str())
            .await
            .expect("expected the dial to land");
        let (mut sender, mut receiver) = from_socket(socket, url, LineLimits::default()).split();

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
