//! The launcher port: the host spawns, the library speaks.
//!
//! The library never spawns on its own initiative. A host implements [`ProcessLauncher`] and
//! decides the sandbox (job objects, process groups, bwrap, a container), the window flags and the
//! kill sequence; it receives a [`LaunchSpec`] and returns a [`ManagedProcess`]. Hosts without a
//! spawner of their own take `TokioLauncher` from the `launcher-tokio` feature.
//!
//! The three halves of a process are owned separately — stdin, stdout and control — so a JSON-RPC
//! pump can read on one task while a caller writes on another, without a lock around the whole
//! child.
//!
//! Framing is the library's, not the host's: a launcher hands over byte chunks and [`LineStream`]
//! turns them into lines under a cap. A vendor that prints a 100 MB line is a bug, not a request
//! to allocate 100 MB.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use crate::error::{Error, Result};
use crate::redact;
use crate::session::CancelReason;

#[cfg(test)]
mod shutdown_tests;

/// What the library asks a host's launcher for.
///
/// `cwd` and `env` are already decided: the directory is the one the host authorised, and the
/// environment is the positive allowlist built from the host's [`EnvSource`](crate::EnvSource)
/// and the harness's documented keys. A launcher that overwrote either would be widening an
/// authorisation the library was given, not granted.
#[derive(Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    /// The program and its arguments. The first element is the executable.
    pub argv: Vec<String>,
    /// The working directory the host authorised.
    pub cwd: PathBuf,
    /// Exactly what the child's environment must be.
    pub env: BTreeMap<String, String>,
    /// Whether the child needs a writable stdin.
    pub stdin: bool,
    /// Whether to suppress a console window on Windows.
    pub hide_window: bool,
}

impl LaunchSpec {
    /// The executable, when the argv is not empty.
    pub fn program(&self) -> Option<&str> {
        self.argv.first().map(String::as_str)
    }
}

impl std::fmt::Debug for LaunchSpec {
    /// Shows the launch shape without exposing host-provided arguments, paths, or environment
    /// values. Callers that need those values already own the spec they constructed.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaunchSpec")
            .field("program", &self.program().map(redact::program_name))
            .field("argument_count", &self.argv.len().saturating_sub(1))
            .field("environment_keys", &self.env.keys().collect::<Vec<_>>())
            .field("stdin", &self.stdin)
            .field("hide_window", &self.hide_window)
            .finish()
    }
}

/// A child process, in three separately owned halves.
pub struct ManagedProcess {
    /// The child's stdout, as byte chunks in arrival order.
    pub stdout: Box<dyn ByteSource>,
    /// The child's stdin, when [`LaunchSpec::stdin`] asked for one.
    pub stdin: Option<Box<dyn ByteSink>>,
    /// Lifetime and diagnostics, shareable across tasks.
    pub control: Arc<dyn ProcessControl>,
}

/// Bytes arriving from a child, in order.
///
/// Chunks rather than an `AsyncRead`: a host on tokio, on smol, on blocking threads or on a
/// recorded fixture can all answer this without adopting one runtime's IO traits.
#[async_trait::async_trait]
pub trait ByteSource: Send {
    /// The next chunk, or `None` once the stream has ended.
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>>;
}

/// Bytes going to a child.
#[async_trait::async_trait]
pub trait ByteSink: Send {
    /// Writes every byte, or fails.
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()>;

    /// Closes the child's input.
    ///
    /// A persistent JSON-RPC peer never wants this — the pipe *is* the session. A print-mode
    /// vendor does: `claude -p --input-format stream-json` reads until end of input, so a turn
    /// that never closes stdin is a process that never finishes. Idempotent, so a cancel racing a
    /// normal finish cannot double-end the stream.
    async fn close(&mut self) -> Result<()>;
}

/// A child's lifetime and its diagnostics.
#[async_trait::async_trait]
pub trait ProcessControl: Send + Sync {
    /// The operating system's id for the child, when the launcher knows one.
    fn pid(&self) -> Option<u32>;

    /// Whatever the child wrote to stderr, bounded and credential-redacted.
    fn stderr_tail(&self) -> String;

    /// Waits for the child to exit.
    async fn wait(&self) -> Result<ExitStatus>;

    /// Requests the platform's graceful user interrupt, when supported by the host.
    ///
    /// Delivery does not prove the vendor stopped or saved resumable state. For example,
    /// a Unix launcher can deliver SIGINT; a Windows host may provide a console-specific port.
    async fn interrupt(&self, _reason: CancelReason) -> Result<InterruptOutcome> {
        Ok(InterruptOutcome::Unsupported)
    }

    /// Ends the child and everything it started.
    ///
    /// The escalation is the launcher's: the library asks once, with the reason, and a launcher
    /// that knows its platform decides what "end it" means there.
    async fn kill(&self, reason: CancelReason) -> Result<()>;
}

/// Whether the host can deliver a graceful process interrupt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum InterruptOutcome {
    /// The interrupt was delivered; completion still requires waiting for exit.
    Delivered,
    /// No signal was sent because the process exited or tree termination already started.
    NotDelivered,
    /// No graceful interrupt is available on this launcher.
    Unsupported,
}

/// How process shutdown completed, after the child was reaped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopOutcome {
    /// The child exited after the graceful interrupt and before escalation.
    Interrupted,
    /// Process-tree termination was required, or graceful interrupt was unavailable.
    Terminated,
}

/// Interrupts, waits for the host's grace, then terminates and reaps if needed.
///
/// Every host call has a deadline. A timeout reports incomplete cleanup rather than success.
/// The host's `kill` implementation owns containment and process-tree termination.
///
/// # Example
///
/// ```no_run
/// # async fn example(control: &dyn mango_external_agents::ProcessControl) {
/// use mango_external_agents::{CancelReason, process::stop_process};
/// let outcome = stop_process(control, CancelReason::Requested,
///     std::time::Duration::from_secs(2)).await;
/// # }
/// ```
pub async fn stop_process(
    control: &dyn ProcessControl,
    reason: CancelReason,
    grace: std::time::Duration,
) -> Result<StopOutcome> {
    stop_process_bounded(control, reason, grace, grace.saturating_mul(3)).await
}

/// Uses the host's distinct graceful-interrupt and shutdown-stage deadlines.
///
/// For example, a harness calls `stop_process_with_limits(control, reason, host.limits())`.
/// Tree cleanup runs even after the leader exits because its helpers may still hold the workspace.
pub async fn stop_process_with_limits(
    control: &dyn ProcessControl,
    reason: CancelReason,
    limits: &crate::Limits,
) -> Result<StopOutcome> {
    stop_process_bounded(control, reason, limits.kill_grace, limits.shutdown_timeout).await
}

async fn stop_process_bounded(
    control: &dyn ProcessControl,
    reason: CancelReason,
    grace: std::time::Duration,
    shutdown: std::time::Duration,
) -> Result<StopOutcome> {
    let interrupt = tokio::time::timeout(grace, control.interrupt(reason)).await;
    let interrupted = matches!(interrupt, Ok(Ok(InterruptOutcome::Delivered)))
        && matches!(tokio::time::timeout(grace, control.wait()).await, Ok(Ok(_)));
    tokio::time::timeout(shutdown, control.kill(reason))
        .await
        .map_err(|_| Error::Timeout {
            operation: String::from("process-tree termination"),
            after: shutdown,
        })??;
    tokio::time::timeout(shutdown, control.wait())
        .await
        .map_err(|_| Error::Timeout {
            operation: String::from("process reaping"),
            after: shutdown,
        })??;
    Ok(if interrupted {
        StopOutcome::Interrupted
    } else {
        StopOutcome::Terminated
    })
}

/// How a child ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ExitStatus {
    /// The exit code, when it exited of its own accord.
    pub code: Option<i32>,
    /// The signal that ended it, on platforms that have them.
    pub signal: Option<i32>,
}

impl ExitStatus {
    /// Whether the child exited successfully and unsignalled.
    pub fn success(&self) -> bool {
        self.code == Some(0) && self.signal.is_none()
    }
}

/// Spawns children on the host's terms.
#[async_trait::async_trait]
pub trait ProcessLauncher: Send + Sync {
    /// Starts one child, or explains why it could not.
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess>;
}

/// The last bytes a child wrote to stderr, bounded and redacted on the way out.
///
/// Shared between the launcher filling it and the control handle reading it. The unredacted bytes
/// never leave this type.
#[derive(Clone)]
pub struct StderrTail {
    buffer: Arc<Mutex<Vec<u8>>>,
    max_bytes: usize,
}

/// How much stderr is kept for diagnostics.
pub const DEFAULT_STDERR_TAIL_BYTES: usize = 16 * 1024;

impl Default for StderrTail {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_STDERR_TAIL_BYTES)
    }
}

impl StderrTail {
    /// A tail that keeps at most `max_bytes` of the most recent output.
    pub fn with_capacity(max_bytes: usize) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(Vec::new())),
            max_bytes,
        }
    }

    /// Appends what the child just wrote, dropping the oldest bytes past the cap.
    ///
    /// What is dropped is rounded up to a whole line. Cutting the buffer at whatever byte the cap
    /// landed on leaves the tail beginning mid-word, and a tail that begins mid-word is a tail
    /// [`read`](Self::read) cannot redact: the scanner needs the name in front of the `=` to know
    /// the value after it is a secret. Losing a partial first line costs a diagnostic nobody could
    /// read anyway.
    pub fn push(&self, chunk: &[u8]) {
        let mut buffer = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
        buffer.extend_from_slice(chunk);
        if buffer.len() <= self.max_bytes {
            return;
        }
        let overflow = buffer.len() - self.max_bytes;
        buffer.drain(..overflow);
        match buffer.iter().position(|byte| matches!(byte, b'\n' | b'\r')) {
            Some(boundary) => {
                buffer.drain(..=boundary);
            }
            // Nothing retained starts a line, so the whole window is the middle of one — a single
            // line longer than the cap. The middle of a line is exactly what cannot be redacted,
            // and a diagnostic is not worth a token.
            None => buffer.clear(),
        }
    }

    /// The tail, redacted for credential-shaped text.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::StderrTail;
    ///
    /// let tail = StderrTail::with_capacity(1024);
    /// tail.push(b"Authorization: Bearer sk-live-42");
    /// assert_eq!(tail.read(), "Authorization: Bearer [REDACTED]");
    /// ```
    pub fn read(&self) -> String {
        let buffer = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
        redact::stderr_text(&String::from_utf8_lossy(&buffer))
    }
}

impl std::fmt::Debug for StderrTail {
    /// Reports capacity and occupancy without turning retained stderr into a byte-array dump.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let buffer = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
        formatter
            .debug_struct("StderrTail")
            .field("max_bytes", &self.max_bytes)
            .field("buffered_bytes", &buffer.len())
            .finish()
    }
}

/// How much output the library will hold while looking for a newline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineLimits {
    /// The longest single line the library will assemble.
    pub max_line_bytes: usize,
    /// The most unread output the library will hold at once.
    pub max_buffered_bytes: usize,
}

impl Default for LineLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: 1024 * 1024,
            max_buffered_bytes: 2 * 1024 * 1024,
        }
    }
}

/// Newline-delimited records over a [`ByteSource`], under a cap.
///
/// Overflow is a typed error rather than a truncated line: a half-read JSON frame parsed as if it
/// were whole is worse than a link that says it gave up.
pub struct LineStream {
    source: Box<dyn ByteSource>,
    limits: LineLimits,
    buffer: Vec<u8>,
    pending: VecDeque<String>,
    queued_bytes: usize,
    ended: bool,
}

impl LineStream {
    /// Frames `source` by lines.
    pub fn new(source: Box<dyn ByteSource>, limits: LineLimits) -> Self {
        Self {
            source,
            limits,
            buffer: Vec::new(),
            pending: VecDeque::new(),
            queued_bytes: 0,
            ended: false,
        }
    }

    /// The next line without its terminator, or `None` at end of stream.
    ///
    /// A trailing `\r` is dropped, so a vendor writing CRLF on Windows reads the same as one
    /// writing LF. Invalid UTF-8 is replaced rather than refused: a vendor's stray byte should not
    /// end a turn that is otherwise going fine.
    ///
    /// # Errors
    ///
    /// [`Error::LimitExceeded`] when one line, or the unread output as a whole, passes its cap.
    pub async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(line) = self.pending.pop_front() {
                self.queued_bytes = self.queued_bytes.saturating_sub(line.len());
                return Ok(Some(line));
            }
            if self.ended {
                return Ok(None);
            }
            match self.source.next_chunk().await? {
                Some(chunk) => self.absorb(&chunk)?,
                None => self.finish()?,
            }
        }
    }

    fn absorb(&mut self, chunk: &[u8]) -> Result<()> {
        self.buffer.extend_from_slice(chunk);
        let buffered = self.buffer.len() + self.queued_bytes;
        if buffered > self.limits.max_buffered_bytes {
            return Err(Error::LimitExceeded {
                subject: "bytes of unread vendor output",
                limit: self.limits.max_buffered_bytes,
                received: buffered,
            });
        }

        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut record: Vec<u8> = self.buffer.drain(..=newline).collect();
            record.pop();
            if record.last() == Some(&b'\r') {
                record.pop();
            }
            self.queue(record)?;
        }

        if self.buffer.len() > self.limits.max_line_bytes {
            return Err(Error::LimitExceeded {
                subject: "bytes of one vendor output line",
                limit: self.limits.max_line_bytes,
                received: self.buffer.len(),
            });
        }
        Ok(())
    }

    /// A vendor that exits without a final newline still said something.
    fn finish(&mut self) -> Result<()> {
        self.ended = true;
        if self.buffer.is_empty() {
            return Ok(());
        }
        let record = std::mem::take(&mut self.buffer);
        self.queue(record)
    }

    fn queue(&mut self, record: Vec<u8>) -> Result<()> {
        if record.len() > self.limits.max_line_bytes {
            return Err(Error::LimitExceeded {
                subject: "bytes of one vendor output line",
                limit: self.limits.max_line_bytes,
                received: record.len(),
            });
        }
        let line = String::from_utf8_lossy(&record).into_owned();
        self.queued_bytes += line.len();
        self.pending.push_back(line);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{ByteSource, Error, LaunchSpec, LineLimits, LineStream, Result, StderrTail};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A source that hands out exactly the chunks a test scripted, in order.
    struct ScriptedSource {
        chunks: Vec<Vec<u8>>,
    }

    impl ScriptedSource {
        fn boxed<const N: usize>(chunks: [&str; N]) -> Box<dyn ByteSource> {
            Box::new(Self {
                chunks: chunks
                    .into_iter()
                    .rev()
                    .map(|chunk| chunk.as_bytes().to_vec())
                    .collect(),
            })
        }
    }

    #[async_trait::async_trait]
    impl ByteSource for ScriptedSource {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(self.chunks.pop())
        }
    }

    #[test]
    fn launch_spec_debug_keeps_credential_values_out_of_diagnostics() {
        let spec = LaunchSpec {
            argv: vec![
                String::from("vendor"),
                String::from("--api-key"),
                String::from("argv-secret"),
                String::from("--endpoint=https://user:url-secret@agent.internal"),
            ],
            cwd: PathBuf::from("/workspace"),
            env: BTreeMap::from([(String::from("VENDOR_API_KEY"), String::from("env-secret"))]),
            stdin: true,
            hide_window: false,
        };

        let rendered = format!("{spec:?}");
        for secret in ["argv-secret", "url-secret", "env-secret"] {
            assert!(
                !rendered.contains(secret),
                "expected no credential in the launch diagnostic, received {rendered}"
            );
        }
        assert!(
            rendered.contains("custom executable"),
            "expected a safe program summary, received {rendered}"
        );
        assert!(
            rendered.contains("VENDOR_API_KEY"),
            "expected the environment key, received {rendered}"
        );
    }

    #[test]
    fn stderr_tail_debug_never_dumps_its_raw_buffer() {
        let tail = StderrTail::with_capacity(1024);
        tail.push(b"Authorization: Bearer raw-buffer-secret");

        let rendered = format!("{tail:?}");
        assert!(
            !rendered.contains("raw-buffer-secret"),
            "expected no raw stderr bytes, received {rendered}"
        );
        assert!(
            !rendered.contains("buffer:"),
            "expected no raw stderr buffer dump, received {rendered}"
        );
        assert!(
            rendered.contains("max_bytes: 1024"),
            "expected capacity context, received {rendered}"
        );
    }

    #[test]
    fn a_stderr_tail_redacts_multiline_credentials_across_chunks() {
        let tail = StderrTail::with_capacity(1024);
        tail.push(b"request failed\nAuthorization: Bea");
        tail.push(b"rer chunked-bearer-secret\nredis://app:chunked-");
        tail.push(b"url-secret@db.internal/main\n");

        let rendered = tail.read();
        for secret in ["chunked-bearer-secret", "chunked-url-secret"] {
            assert!(
                !rendered.contains(secret),
                "expected no credential in the stderr tail, received {rendered}"
            );
        }
        assert!(
            rendered.contains("Authorization: Bearer [REDACTED]")
                && rendered.contains("redis://app:[REDACTED]@db.internal/main"),
            "expected redacted multiline context, received {rendered}"
        );

        let debug = format!("{tail:?}");
        assert!(
            !debug.contains("chunked-bearer-secret") && !debug.contains("chunked-url-secret"),
            "expected no raw chunks in debug output, received {debug}"
        );
    }

    fn stream<const N: usize>(chunks: [&str; N], limits: LineLimits) -> LineStream {
        LineStream::new(ScriptedSource::boxed(chunks), limits)
    }

    #[tokio::test]
    async fn holds_a_partial_line_across_chunk_boundaries() {
        let mut lines = stream(["first", " line\nsecond\n"], LineLimits::default());

        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("first line"))
        );
        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("second"))
        );
        assert_eq!(lines.next_line().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn yields_a_final_line_that_never_got_its_newline() {
        let mut lines = stream(["only\nlast"], LineLimits::default());

        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("only"))
        );
        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("last"))
        );
        assert_eq!(lines.next_line().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn drops_a_carriage_return_before_the_newline() {
        let mut lines = stream(["windows\r\nline\r\n"], LineLimits::default());

        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("windows"))
        );
        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("line"))
        );
    }

    #[tokio::test]
    async fn refuses_a_line_past_its_byte_cap() {
        let mut lines = stream(
            ["xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n"],
            LineLimits {
                max_line_bytes: 16,
                ..LineLimits::default()
            },
        );

        let error = lines
            .next_line()
            .await
            .expect_err("expected a refusal, received a line");
        assert!(
            matches!(
                error,
                Error::LimitExceeded {
                    subject: "bytes of one vendor output line",
                    limit: 16,
                    received: 32
                }
            ),
            "expected a line-limit refusal, received {error:?}"
        );
    }

    #[tokio::test]
    async fn refuses_unread_output_past_the_buffer_cap() {
        let mut lines = stream(
            ["one\ntwo\nthree\n"],
            LineLimits {
                max_line_bytes: 32,
                max_buffered_bytes: 8,
            },
        );

        let error = lines
            .next_line()
            .await
            .expect_err("expected a refusal, received a line");
        assert!(
            matches!(
                error,
                Error::LimitExceeded {
                    subject: "bytes of unread vendor output",
                    limit: 8,
                    ..
                }
            ),
            "expected a buffer-limit refusal, received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_line_that_never_ends_is_refused_before_it_is_complete() {
        let mut lines = stream(
            ["yyyyyyyyyyyyyyyyyyyy"],
            LineLimits {
                max_line_bytes: 8,
                max_buffered_bytes: 1024,
            },
        );

        let error = lines
            .next_line()
            .await
            .expect_err("expected a refusal, received a line");
        assert!(
            matches!(error, Error::LimitExceeded { limit: 8, .. }),
            "expected a line-limit refusal, received {error:?}"
        );
    }

    #[tokio::test]
    async fn replaces_invalid_utf8_rather_than_ending_the_turn() {
        struct RawSource(Option<Vec<u8>>);

        #[async_trait::async_trait]
        impl ByteSource for RawSource {
            async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
                Ok(self.0.take())
            }
        }

        let mut lines = LineStream::new(
            Box::new(RawSource(Some(vec![b'a', 0xff, b'b', b'\n']))),
            LineLimits::default(),
        );
        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("a\u{fffd}b"))
        );
    }

    /// A tail cut at an arbitrary byte starts mid-name, and a scanner that cannot see the name
    /// cannot redact the value. The cut lands on a line boundary so that never happens, and when
    /// there is no boundary to land on there is nothing safe to keep.
    #[test]
    fn a_tail_cut_by_the_cap_never_starts_mid_line() {
        // 62 bytes into a 35-byte tail: the cut lands eight bytes into the second line, right
        // after `API_KEY=`, leaving the value with nothing in front of it to identify it.
        let tail = StderrTail::with_capacity(35);
        tail.push(b"dropped by the cap\n");
        tail.push(b"API_KEY=another-secret\n");
        tail.push(b"ordinary diagnostic\n");

        let read = tail.read();
        assert!(
            !read.contains("another-secret"),
            "expected no orphaned secret, received {read:?}"
        );
        assert_eq!(read, "ordinary diagnostic\n");
    }

    #[test]
    fn a_single_line_longer_than_the_cap_leaves_nothing_rather_than_a_fragment() {
        let tail = StderrTail::with_capacity(16);
        tail.push(b"OPENAI_API_KEY=sk-proj-0123456789abcdef");

        assert_eq!(
            tail.read(),
            "",
            "expected no fragment of an unredactable line"
        );
    }

    #[test]
    fn a_stderr_tail_keeps_the_last_bytes_and_redacts_them() {
        let tail = StderrTail::with_capacity(32);
        tail.push(b"dropped by the cap\n");
        tail.push(b"API_KEY=another-secret\n");

        let read = tail.read();
        assert!(
            !read.contains("another-secret"),
            "expected no secret, received {read:?}"
        );
        assert_eq!(read, "API_KEY=[REDACTED]\n");
        assert!(
            read.len() <= 32,
            "expected at most 32 bytes, received {}",
            read.len()
        );
    }
}
