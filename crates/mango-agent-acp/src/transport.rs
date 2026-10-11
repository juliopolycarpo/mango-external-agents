//! The `acp` transport: a host-owned child process, framed as a message link.
//!
//! The host owns the process, so the argv goes through
//! [`ProcessLauncher`](mango_external_agents::ProcessLauncher) and the library only ever receives
//! the pipes.
//!
//! [`ManagedProcess`] supplies a [`ByteSource`](mango_external_agents::ByteSource) and [`ByteSink`].
//! [`LineStream`] frames the child's stdout under the host's line limits, and [`frame`] hands both
//! halves back as one [`Link`]: one JSON-RPC message per line, each way. The repository's own
//! JSON-RPC peer ([`mango_external_agents::jsonrpc::Client`]) speaks over that link and enforces
//! the queue budgets in both directions; the private `budget` module derives them from the host's
//! [`Limits`](mango_external_agents::Limits). Pressure ends the connection, so native cleanup
//! can progress independently of a stalled physical writer or protocol handler.
//!
//! The `Http` arm of [`AcpSpec`] is not implemented in 0.1. See the crate docs for why.

mod budget;
pub(crate) use budget::{Overflow, client_options, wire_options};
use mango_external_agents::process::{ByteSink, LineStream};
use mango_external_agents::{
    AcpSpec, CancelReason, Error, HostContext, LaunchSpec, Link, LinkReceiver, LinkSender,
    ManagedProcess, ProcessCleanupGuard, Result,
};

/// A launched agent: the link its JSON-RPC connection rides, and the child behind it.
///
/// The two are returned together because a connection alone cannot say why it ended. When the
/// agent exits mid-turn the link reports a closed stream and nothing else; the child's
/// [`stderr_tail`](mango_external_agents::ProcessControl::stderr_tail) is what turns that into a
/// diagnostic, and its [`kill`](mango_external_agents::ProcessControl::kill) is what ends an agent
/// that ignored a closed stdin.
pub struct LaunchedAgent {
    /// One JSON-RPC message per line in each direction, for
    /// [`jsonrpc::Client::connect_with`](mango_external_agents::jsonrpc::Client::connect_with).
    pub link: Link,
    /// The child's lifetime and diagnostics.
    pub control: std::sync::Arc<dyn mango_external_agents::ProcessControl>,
    /// The host's outgoing byte budget, which the connection bounds its frames and queue by.
    pub(crate) outbound_buffer_bytes: usize,
}

impl std::fmt::Debug for LaunchedAgent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchedAgent")
            .field("pid", &self.control.pid())
            .finish_non_exhaustive()
    }
}

/// Spawns `spec` through the host's launcher and frames its pipes as a message link.
///
/// `vendor_environment_keys` comes from the harness's own descriptor: the child receives the base
/// allowlist plus those keys and nothing else.
///
/// # Errors
///
/// [`Error::UnsupportedTransport`] for [`AcpSpec::Http`], which 0.1 does not carry;
/// [`Error::HostConfiguration`] for an empty argv; [`Error::Launch`] when the host's launcher
/// refused; [`Error::Link`] when the launcher returned a child with no stdin, after that child has
/// been stopped; and [`Error::CleanupRequired`], carrying the child's control, when stopping it
/// failed.
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
        // Unreachable today: this crate builds only `AcpSpec::ChildPipes`, and `AcpSpec::Http`
        // has no constructor here. `HarnessId::new("acp")` names the transport generically since
        // `connect` is not handed a profile to name more specifically.
        return Err(Error::UnsupportedTransport {
            harness: mango_external_agents::HarnessId::new("acp")
                .expect("expected \"acp\" to be a valid harness id"),
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
    let control = std::sync::Arc::clone(&process.control);
    match frame(process, host) {
        Ok(launched) => Ok(launched),
        // `frame` refuses a child it cannot write to, and it consumes the process. The child is
        // live and the host owns it only through this control, so it is stopped here, within the
        // host's bounds, rather than left running behind an error. If that fails, the host gets
        // the control back in `Error::CleanupRequired`, as `docs/lifecycle.md` promises.
        Err(error) => {
            ProcessCleanupGuard::new(control, *host.limits(), CancelReason::Shutdown)
                .finish()
                .await?;
            Err(error)
        }
    }
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
        message: String::from("a child without a writable stdin"),
    })?;

    Ok(LaunchedAgent {
        link: Link::new(
            Box::new(LineSink { stdin }),
            Box::new(LineSource {
                lines: LineStream::new(stdout, host.limits().line),
            }),
        ),
        control,
        outbound_buffer_bytes: host.outbound_buffer_bytes(),
    })
}

/// Writes one JSON-RPC message per line into the child's stdin.
struct LineSink {
    stdin: Box<dyn ByteSink>,
}

#[async_trait::async_trait]
impl LinkSender for LineSink {
    async fn send(&mut self, mut message: String) -> Result<()> {
        // One write, so a frame can never be split across two lines by another writer.
        message.push('\n');
        self.stdin.write_all(message.as_bytes()).await
    }

    async fn close(&mut self) -> Result<()> {
        self.stdin.close().await
    }
}

/// Reads the child's stdout one line at a time, under the host's line caps.
///
/// A line over the cap is an error rather than a truncated frame: half a JSON frame parsed as a
/// whole one is worse than a link that says it gave up.
struct LineSource {
    lines: LineStream,
}

#[async_trait::async_trait]
impl LinkReceiver for LineSource {
    async fn recv(&mut self) -> Result<Option<String>> {
        // The reader takes a line, hands it to the connection's queue and comes straight back
        // here, and a line already buffered is ready without waiting. A burst an agent writes in
        // one go would then be counted whole against the queue budgets before the handler had
        // run once. Yielding before each line lets the handler take the one just queued, so the
        // budgets bound what waits for the client and not how much an agent may say. Nothing has
        // been read yet at this point, so a caller that stops waiting here loses no line.
        tokio::task::yield_now().await;
        self.lines.next_line().await
    }
}

#[cfg(test)]
mod tests;
