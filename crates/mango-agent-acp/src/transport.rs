//! The `acp` transport: a host-owned child process, framed as the official crate's [`Lines`].
//!
//! The official `agent-client-protocol` crate ships two carriers of its own — `AcpAgent`, which
//! spawns the agent itself, and `Stdio`, which takes over this process's own stdio. Neither is
//! usable here: the host owns the process, so the argv goes through
//! [`ProcessLauncher`](mango_external_agents::ProcessLauncher) and the library only ever receives
//! the pipes.
//!
//! What it receives them as is why this module builds [`Lines`] rather than the crate's
//! `ByteStreams`: a [`ManagedProcess`] hands over a [`ByteSource`](mango_external_agents::ByteSource)
//! of chunks and a [`ByteSink`],
//! not a `futures::io::AsyncRead`, so there is nothing for `tokio_util::compat` to convert.
//! `Lines` wants a `Stream<Item = io::Result<String>>` and a `Sink<String>`, which is exactly what
//! the core's own [`LineStream`] and [`ByteSink`] are — so the framing, and the cap it reads the
//! vendor under, stay the library's. `ByteStreams` would have framed the pipe itself, with no cap
//! at all.
//!
//! The `Http` arm of [`AcpSpec`] is not implemented in 0.1. See the crate docs for why.

use std::pin::Pin;

use agent_client_protocol::Lines;
use futures::stream::BoxStream;
use mango_external_agents::process::{ByteSink, LineStream};
use mango_external_agents::{AcpSpec, Error, HostContext, LaunchSpec, ManagedProcess, Result};

/// The outgoing half: one JSON-RPC message per line, written to the child's stdin.
type OutgoingLines = Pin<Box<dyn futures::Sink<String, Error = std::io::Error> + Send>>;

/// The incoming half: the child's stdout, framed by the core under the host's line caps.
type IncomingLines = BoxStream<'static, std::io::Result<String>>;

/// A launched agent: the transport the official connection rides, and the child behind it.
///
/// The two are returned together because a connection alone cannot say why it ended. When the
/// agent exits mid-turn the transport reports a closed stream and nothing else; the child's
/// [`stderr_tail`](mango_external_agents::ProcessControl::stderr_tail) is what turns that into a
/// diagnostic, and its [`kill`](mango_external_agents::ProcessControl::kill) is what ends an agent
/// that ignored a closed stdin.
pub struct LaunchedAgent {
    /// The line transport to hand the official client builder.
    pub transport: Lines<OutgoingLines, IncomingLines>,
    /// The child's lifetime and diagnostics.
    pub control: std::sync::Arc<dyn mango_external_agents::ProcessControl>,
}

impl std::fmt::Debug for LaunchedAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchedAgent")
            .field("pid", &self.control.pid())
            .finish_non_exhaustive()
    }
}

/// Spawns `spec` through the host's launcher and frames its pipes for the official client.
///
/// `vendor_environment_keys` comes from the harness's own descriptor: the child receives the base
/// allowlist plus those keys and nothing else.
///
/// # Errors
///
/// [`Error::UnsupportedTransport`] for [`AcpSpec::Http`], which 0.1 does not carry;
/// [`Error::HostConfiguration`] for an empty argv; [`Error::Launch`] when the host's launcher
/// refused; and [`Error::Link`] when the launcher returned a child with no stdin.
///
/// # Example
///
/// ```no_run
/// use mango_external_agents::{AcpSpec, StdioSpec};
///
/// # async fn run(host: &mango_external_agents::HostContext) -> mango_external_agents::Result<()> {
/// let spec = AcpSpec::ChildPipes(StdioSpec::new(["opencode", "acp"]));
/// let launched = mango_agent_acp::transport::connect(host, &spec, &[]).await?;
/// assert!(launched.control.pid().is_some() || launched.control.pid().is_none());
/// # Ok(())
/// # }
/// ```
pub async fn connect(
    host: &HostContext,
    spec: &AcpSpec,
    vendor_environment_keys: &[&str],
) -> Result<LaunchedAgent> {
    let AcpSpec::ChildPipes(stdio) = spec else {
        return Err(Error::UnsupportedTransport {
            harness: mango_external_agents::HarnessKind::Acp(
                mango_external_agents::AcpProfileId::new("acp"),
            ),
            transport: mango_external_agents::TransportKind::Acp,
        });
    };
    if stdio.argv.is_empty() {
        return Err(Error::HostConfiguration {
            expected: "an argv naming the agent's ACP mode",
            received: String::from("an empty argv"),
        });
    }

    let process = host
        .launcher()
        .spawn(LaunchSpec {
            argv: stdio.argv.clone(),
            cwd: host.cwd().to_path_buf(),
            env: host.child_environment(vendor_environment_keys),
            stdin: true,
            hide_window: true,
        })
        .await?;
    frame(process, host)
}

/// Frames an already-spawned child, so a test can drive the transport without a launcher.
///
/// # Errors
///
/// [`Error::Link`] when the child has no stdin: a JSON-RPC peer that cannot be written to is not
/// a peer, and discovering that on the first request would look like a protocol failure.
pub fn frame(process: ManagedProcess, host: &HostContext) -> Result<LaunchedAgent> {
    let ManagedProcess {
        stdout,
        stdin,
        control,
    } = process;
    let stdin = stdin.ok_or_else(|| Error::Link {
        peer: String::from("ACP agent"),
        message: String::from("expected a writable stdin, received a child without one"),
    })?;

    let incoming: IncomingLines =
        Box::pin(incoming_lines(LineStream::new(stdout, host.limits().line)));
    let outgoing: OutgoingLines = Box::pin(outgoing_lines(stdin));

    Ok(LaunchedAgent {
        transport: Lines::new(outgoing, incoming),
        control,
    })
}

/// The child's stdout as lines, with the core's cap failures crossing as IO errors.
///
/// The official connection's only vocabulary for a broken transport is `std::io::Error`, so a line
/// over the cap becomes `InvalidData` rather than being silently truncated: half a JSON frame
/// parsed as a whole one is worse than a link that says it gave up.
fn incoming_lines(source: LineStream) -> impl futures::Stream<Item = std::io::Result<String>> {
    futures::stream::unfold(Some(source), async move |state| {
        let mut source = state?;
        match source.next_line().await {
            Ok(Some(line)) => Some((Ok(line), Some(source))),
            // End of stream: the state is dropped so the stream stays ended if polled again.
            Ok(None) => None,
            Err(error) => Some((Err(as_io_error(error)), None)),
        }
    })
}

/// One JSON-RPC message per line into the child's stdin.
fn outgoing_lines(
    sink: Box<dyn ByteSink>,
) -> impl futures::Sink<String, Error = std::io::Error> + Send {
    futures::sink::unfold(
        sink,
        async move |mut sink: Box<dyn ByteSink>, line: String| {
            let mut bytes = line.into_bytes();
            bytes.push(b'\n');
            sink.write_all(&bytes).await.map_err(as_io_error)?;
            Ok(sink)
        },
    )
}

/// A core failure as the only failure the official transport understands.
fn as_io_error(error: Error) -> std::io::Error {
    let kind = match &error {
        Error::LimitExceeded { .. } => std::io::ErrorKind::InvalidData,
        Error::Closed { .. } => std::io::ErrorKind::BrokenPipe,
        _ => std::io::ErrorKind::Other,
    };
    std::io::Error::new(kind, error.to_string())
}
