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
mod stderr_discard;

use stderr_discard::Discard;

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

/// Owns bounded process cleanup until it succeeds or hands the control back to the host.
///
/// A caller creates this after launch and calls [`finish`](Self::finish) when its short-lived
/// operation is done. It captures that operation's Tokio runtime, so dropping it from another
/// thread starts the same bounded cleanup while the captured runtime remains alive. A failed
/// cleanup returns [`Error::CleanupRequired`], retaining the control for host reconciliation
/// rather than losing the only process handle.
///
/// # Example
///
/// ```no_run
/// # async fn example(
/// #     control: std::sync::Arc<dyn mango_external_agents::ProcessControl>,
/// #     limits: mango_external_agents::Limits,
/// # ) -> mango_external_agents::Result<()> {
/// use mango_external_agents::{CancelReason, ProcessCleanupGuard};
///
/// let cleanup = ProcessCleanupGuard::new(control, limits, CancelReason::Shutdown);
/// cleanup.finish().await?;
/// # Ok(())
/// # }
/// ```
pub struct ProcessCleanupGuard {
    control: Arc<dyn ProcessControl>,
    limits: crate::Limits,
    reason: CancelReason,
    runtime: tokio::runtime::Handle,
    worker: Option<tokio::task::JoinHandle<Result<StopOutcome>>>,
    active: bool,
}

impl ProcessCleanupGuard {
    /// Starts owning cleanup for `control`.
    ///
    /// The guard does not interrupt the child until [`finish`](Self::finish) or drop, so callers
    /// may keep using its pipes while they hold the guard.
    #[must_use]
    pub fn new(
        control: Arc<dyn ProcessControl>,
        limits: crate::Limits,
        reason: CancelReason,
    ) -> Self {
        Self {
            control,
            limits,
            reason,
            runtime: tokio::runtime::Handle::current(),
            worker: None,
            active: true,
        }
    }

    /// Waits for bounded cleanup and returns a recoverable error when it did not complete.
    ///
    /// The cleanup worker is started before this await. Cancelling this future therefore detaches
    /// the bounded worker, which retains a clone of the control while the runtime is alive.
    pub async fn finish(mut self) -> Result<StopOutcome> {
        self.begin();
        let Some(worker) = self.worker.take() else {
            self.active = false;
            return Err(self.required(Error::Closed {
                subject: "process cleanup worker",
            }));
        };
        // Dropping an awaited JoinHandle detaches its already-started worker. Mark that fact before
        // awaiting so cancellation of `finish` cannot start a second process-tree cleanup.
        self.active = false;
        match worker.await {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(source)) => Err(self.required(source)),
            Err(_) => Err(self.required(Error::Closed {
                subject: "process cleanup task",
            })),
        }
    }

    /// Transfers the child control to a long-lived owner without starting cleanup.
    #[must_use]
    pub fn into_control(mut self) -> Arc<dyn ProcessControl> {
        self.active = false;
        Arc::clone(&self.control)
    }

    fn required(&self, source: Error) -> Error {
        Error::CleanupRequired {
            control: Arc::clone(&self.control),
            source: Box::new(source),
        }
    }

    fn begin(&mut self) {
        if !self.active || self.worker.is_some() {
            return;
        }
        let control = Arc::clone(&self.control);
        let limits = self.limits;
        let reason = self.reason;
        self.worker = Some(self.runtime.spawn(async move {
            stop_process_with_limits(control.as_ref(), reason, &limits).await
        }));
    }
}

impl Drop for ProcessCleanupGuard {
    fn drop(&mut self) {
        self.begin();
    }
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
    state: Arc<Mutex<TailState>>,
    max_bytes: usize,
}

/// What the tail holds, and whether the next bytes still belong to text it already discarded.
#[derive(Default)]
struct TailState {
    buffer: Vec<u8>,
    /// Set while a cut is unfinished: the bytes that follow are the rest of a line, or the value
    /// of a credential whose name was dropped, and a continuation kept on its own has lost the
    /// name that lets [`redact::stderr_text`] recognise it.
    discard: Option<Discard>,
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
            state: Arc::new(Mutex::new(TailState::default())),
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
    ///
    /// A cut goes on until nothing kept can have lost its name. A line longer than the cap is
    /// dropped whole: once an overflow leaves only the middle of a line, every following byte up
    /// to the next CR or LF is dropped too, however many reads it spans. And a cut that ends where
    /// a credential's value is still awaited (`API_KEY=`, `Authorization:` or a trailing
    /// `Bearer`) also drops the blank lines and the next non-empty line, because the redaction
    /// rules read a value across a line break. Bytes are only ever dropped, never cut before
    /// they are redacted.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::StderrTail;
    ///
    /// let tail = StderrTail::with_capacity(16);
    /// tail.push(b"API_KEY=0123456789abcdef");
    /// tail.push(b"-continued-secret\nnext line\n");
    /// assert_eq!(tail.read(), "next line\n");
    /// ```
    pub fn push(&self, chunk: &[u8]) {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let TailState { buffer, discard } = &mut *guard;
        let chunk = match discard.as_mut().map(|open| open.consume(chunk)) {
            Some(None) => return,
            Some(Some(kept_from)) => {
                *discard = None;
                &chunk[kept_from..]
            }
            None => chunk,
        };
        buffer.extend_from_slice(chunk);
        if buffer.len() <= self.max_bytes {
            return;
        }
        let overflow = buffer.len() - self.max_bytes;
        let boundary = buffer[overflow..]
            .iter()
            .position(|byte| matches!(byte, b'\n' | b'\r'))
            .map(|offset| overflow + offset);
        let Some(boundary) = boundary else {
            // Nothing retained starts a line, so the whole window is the middle of one — a single
            // line longer than the cap. The middle of a line is exactly what cannot be redacted,
            // and a diagnostic is not worth a token. The line goes on until its terminator.
            *discard = Some(Discard::mid_line(buffer));
            buffer.clear();
            return;
        };
        let awaiting = Discard::after_line(&buffer[..boundary]);
        buffer.drain(..=boundary);
        let Some(mut open) = awaiting else {
            return;
        };
        // The dropped line ended awaiting a value, and what is kept begins with it.
        match open.consume(buffer) {
            Some(kept_from) => {
                buffer.drain(..kept_from);
            }
            None => {
                buffer.clear();
                *discard = Some(open);
            }
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
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        redact::stderr_text(&String::from_utf8_lossy(&state.buffer))
    }
}

impl std::fmt::Debug for StderrTail {
    /// Reports capacity and occupancy without turning retained stderr into a byte-array dump.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        formatter
            .debug_struct("StderrTail")
            .field("max_bytes", &self.max_bytes)
            .field("buffered_bytes", &state.buffer.len())
            .finish()
    }
}

/// How much output the library will hold while looking for a newline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineLimits {
    /// The longest single line the library will assemble.
    pub max_line_bytes: usize,
    /// The most unread output the library will hold at once, counted after invalid UTF-8 is
    /// replaced (each invalid sequence becomes a 3-byte U+FFFD, so repair can at most triple a
    /// line).
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
    /// end a turn that is otherwise going fine. The replacement can be longer than the bytes it
    /// stands for, so the repaired line is what counts against `max_buffered_bytes`; `max_line_bytes`
    /// stays a limit on the raw line.
    ///
    /// # Errors
    ///
    /// [`Error::LimitExceeded`] when one line, or the unread output as a whole, passes its cap.
    /// The stream is over after that: later calls return `None` rather than lines that skip the
    /// refused one. Lines from the same read that came before the refused one are not delivered
    /// either; the error is what the call returns, as it always was.
    pub async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(line) = self.pending.pop_front() {
                self.queued_bytes = self.queued_bytes.saturating_sub(line.len());
                return Ok(Some(line));
            }
            if self.ended {
                return Ok(None);
            }
            let outcome = match self.source.next_chunk().await? {
                Some(chunk) => self.absorb(chunk),
                None => self.finish(),
            };
            if let Err(error) = outcome {
                self.abandon();
                return Err(error);
            }
        }
    }

    /// Splits `chunk` and everything held so far into complete lines.
    ///
    /// The held tail never contains a newline, so the scan resumes where the new bytes begin
    /// instead of at the start of a long partial line, and the buffer is compacted once for the
    /// whole chunk rather than once per line.
    fn absorb(&mut self, chunk: Vec<u8>) -> Result<()> {
        let tail = self.buffer.len();
        if tail == 0 {
            self.buffer = chunk;
        } else {
            self.buffer.extend_from_slice(&chunk);
        }
        let buffered = self.buffer.len() + self.queued_bytes;
        if buffered > self.limits.max_buffered_bytes {
            return Err(Error::LimitExceeded {
                subject: "bytes of unread vendor output",
                limit: self.limits.max_buffered_bytes,
                received: buffered,
            });
        }

        let mut consumed = 0;
        while let Some(newline) = self.next_newline(consumed.max(tail)) {
            let mut record = &self.buffer[consumed..newline];
            if let Some(without_return) = record.strip_suffix(b"\r") {
                record = without_return;
            }
            let record = record.to_vec();
            consumed = newline + 1;
            self.queue(record, self.buffer.len() - consumed)?;
        }
        self.buffer.drain(..consumed);

        if self.buffer.len() > self.limits.max_line_bytes {
            return Err(Error::LimitExceeded {
                subject: "bytes of one vendor output line",
                limit: self.limits.max_line_bytes,
                received: self.buffer.len(),
            });
        }
        Ok(())
    }

    /// The index of the first newline at or after `from`.
    fn next_newline(&self, from: usize) -> Option<usize> {
        let found = self.buffer[from..].iter().position(|byte| *byte == b'\n');
        found.map(|offset| from + offset)
    }

    /// A vendor that exits without a final newline still said something.
    fn finish(&mut self) -> Result<()> {
        self.ended = true;
        if self.buffer.is_empty() {
            return Ok(());
        }
        let record = std::mem::take(&mut self.buffer);
        self.queue(record, 0)
    }

    /// Queues one raw line; `remaining` is how many raw bytes are still buffered behind it.
    fn queue(&mut self, record: Vec<u8>, remaining: usize) -> Result<()> {
        if record.len() > self.limits.max_line_bytes {
            return Err(Error::LimitExceeded {
                subject: "bytes of one vendor output line",
                limit: self.limits.max_line_bytes,
                received: record.len(),
            });
        }
        // Repair can triple a line: each invalid sequence becomes a 3-byte U+FFFD. The raw checks
        // above cannot see that, so the decoded size is counted against the unread-output budget
        // too. A valid line keeps its own allocation.
        let line = String::from_utf8(record)
            .unwrap_or_else(|invalid| String::from_utf8_lossy(invalid.as_bytes()).into_owned());
        // What is still buffered behind this record (later lines, an unterminated tail) is held too.
        let unread = self.queued_bytes + line.len() + remaining;
        if unread > self.limits.max_buffered_bytes {
            return Err(Error::LimitExceeded {
                subject: "bytes of unread vendor output",
                limit: self.limits.max_buffered_bytes,
                received: unread,
            });
        }
        self.queued_bytes += line.len();
        self.pending.push_back(line);
        Ok(())
    }

    /// Ends the stream after a limit error, so a caller that asks again reaches the end instead of
    /// lines that skip the one that was refused, and the refused bytes are released.
    fn abandon(&mut self) {
        self.ended = true;
        self.buffer = Vec::new();
        self.pending.clear();
        self.queued_bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ByteSource, DEFAULT_STDERR_TAIL_BYTES, Error, LaunchSpec, LineLimits, LineStream, Result,
        StderrTail,
    };
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

    /// Hands out exactly the raw chunks a test scripted, so a test can put invalid UTF-8 on the
    /// wire, which the `&str`-taking [`ScriptedSource`] cannot.
    struct RawChunkSource {
        chunks: Vec<Vec<u8>>,
    }

    impl RawChunkSource {
        fn stream(chunks: Vec<Vec<u8>>, limits: LineLimits) -> LineStream {
            let chunks = chunks.into_iter().rev().collect();
            LineStream::new(Box::new(Self { chunks }), limits)
        }
    }

    #[async_trait::async_trait]
    impl ByteSource for RawChunkSource {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
            Ok(self.chunks.pop())
        }
    }

    /// `count` invalid bytes and a newline: each byte repairs to a 3-byte U+FFFD.
    fn invalid_line(count: usize) -> Vec<u8> {
        let mut line = vec![0xff; count];
        line.push(b'\n');
        line
    }

    #[tokio::test]
    async fn refuses_a_line_that_only_passes_the_buffer_cap_before_repair() {
        // 60 raw bytes fit both caps (100); repaired they are 180 bytes.
        let mut lines = RawChunkSource::stream(
            vec![invalid_line(60)],
            LineLimits {
                max_line_bytes: 100,
                max_buffered_bytes: 100,
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
                    limit: 100,
                    received: 180,
                }
            ),
            "expected a buffer-limit refusal at 180 repaired bytes, received {error:?}"
        );
    }

    #[tokio::test]
    async fn refuses_several_repaired_lines_from_one_chunk_past_the_buffer_cap() {
        // Four lines of 21 raw bytes are 84 in one chunk, under the cap of 100. The first repairs
        // to 60 bytes with 63 raw bytes of the other three still buffered: 123 held.
        let chunk = (0..4).flat_map(|_| invalid_line(20)).collect();
        let mut lines = RawChunkSource::stream(
            vec![chunk],
            LineLimits {
                max_line_bytes: 100,
                max_buffered_bytes: 100,
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
                    limit: 100,
                    received: 123,
                }
            ),
            "expected a buffer-limit refusal at 123 held bytes, received {error:?}"
        );
    }

    #[tokio::test]
    async fn counts_the_unterminated_tail_beside_a_repaired_line() {
        // One 21-byte line and a 70-byte tail are 91 raw bytes in the chunk, under the cap of 100.
        // The line repairs to 60 bytes and the tail stays buffered: 130 bytes held in all.
        let mut chunk = invalid_line(20);
        chunk.extend(std::iter::repeat_n(b'a', 70));
        let mut lines = RawChunkSource::stream(
            vec![chunk],
            LineLimits {
                max_line_bytes: 100,
                max_buffered_bytes: 100,
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
                    limit: 100,
                    received: 130,
                }
            ),
            "expected a buffer-limit refusal at 130 held bytes, received {error:?}"
        );
    }

    /// Reads every remaining line, or the first error.
    async fn drain(lines: &mut LineStream) -> Result<Vec<String>> {
        let mut all = Vec::new();
        while let Some(line) = lines.next_line().await? {
            all.push(line);
        }
        Ok(all)
    }

    fn strings<const N: usize>(lines: [&str; N]) -> Vec<String> {
        lines.into_iter().map(String::from).collect()
    }

    /// A stream over `chunks` exactly as cut.
    fn cut(chunks: &[&str], limits: LineLimits) -> LineStream {
        let chunks = chunks
            .iter()
            .map(|chunk| chunk.as_bytes().to_vec())
            .collect();
        RawChunkSource::stream(chunks, limits)
    }

    #[tokio::test]
    async fn joins_a_crlf_that_is_split_across_chunks() {
        for chunks in [
            vec!["a\r", "\nb\r\n"],
            vec!["a", "\r", "\n", "b", "\r", "\n"],
            vec!["a\r\nb\r", "\n"],
        ] {
            let mut lines = cut(&chunks, LineLimits::default());
            assert_eq!(
                drain(&mut lines).await.expect("expected lines"),
                strings(["a", "b"]),
                "expected CRLF to end each line however {chunks:?} is cut"
            );
        }
    }

    #[tokio::test]
    async fn a_newline_may_be_the_first_or_last_byte_of_a_chunk() {
        for (chunks, expected) in [
            (vec!["abc", "\ndef\n"], strings(["abc", "def"])),
            (vec!["abc\n", "def\n"], strings(["abc", "def"])),
            (vec!["abc\n", "\n", "def"], strings(["abc", "", "def"])),
            (vec!["\n", "\n\n"], strings(["", "", ""])),
            (vec!["ab", "\n", "cd", "\n"], strings(["ab", "cd"])),
        ] {
            let mut lines = cut(&chunks, LineLimits::default());
            assert_eq!(
                drain(&mut lines).await.expect("expected lines"),
                expected,
                "expected these lines from chunks {chunks:?}"
            );
        }
    }

    #[tokio::test]
    async fn strips_one_carriage_return_and_keeps_an_unterminated_one() {
        // Only the `\r` directly before a `\n` is a terminator; a `\r` at the end of the stream
        // has no newline after it and is data.
        let mut lines = stream(["a\r\r\nb\r"], LineLimits::default());

        assert_eq!(
            drain(&mut lines).await.expect("expected lines"),
            strings(["a\r", "b\r"])
        );
    }

    #[tokio::test]
    async fn a_line_exactly_at_the_cap_is_delivered_however_the_chunks_fall() {
        let limits = LineLimits {
            max_line_bytes: 8,
            max_buffered_bytes: 64,
        };
        for chunks in [
            vec!["12345678\n"],
            vec!["1234", "5678\n"],
            vec!["12345678", "\n"],
            vec!["1", "2", "3", "4", "5", "6", "7", "8", "\n"],
            vec!["12345678\n", "12345678\n"],
            vec!["1234", "5678\n1234", "5678\n"],
            vec!["12345678"],
        ] {
            let mut lines = cut(&chunks, limits);
            let all = drain(&mut lines).await.unwrap_or_else(|error| {
                panic!("expected 8-byte lines from {chunks:?}, received {error:?}")
            });
            assert!(
                !all.is_empty() && all.iter().all(|line| line == "12345678"),
                "expected only 8-byte lines from {chunks:?}, received {all:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_line_one_past_the_cap_is_refused_with_its_length_however_the_chunks_fall() {
        let limits = LineLimits {
            max_line_bytes: 8,
            max_buffered_bytes: 64,
        };
        // Terminated in the same chunk, terminated in a later chunk, and never terminated: the
        // refusal names the same limit and the same received length.
        for chunks in [
            vec!["123456789\n"],
            vec!["1234", "56789\n"],
            vec!["123456789", "\n"],
            vec!["12345678", "9\n"],
            vec!["123456789"],
            vec!["1234", "56789"],
        ] {
            let mut lines = cut(&chunks, limits);
            let error = drain(&mut lines)
                .await
                .expect_err("expected a refusal, received lines");
            assert!(
                matches!(
                    error,
                    Error::LimitExceeded {
                        subject: "bytes of one vendor output line",
                        limit: 8,
                        received: 9,
                    }
                ),
                "expected a 9-byte line refusal from {chunks:?}, received {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_carriage_return_held_at_a_chunk_edge_counts_toward_the_line_cap() {
        // Characterises today's behaviour so it cannot drift: a full line whose `\r` ends one chunk
        // and whose `\n` starts the next is held as 9 bytes, one past the cap, when the first chunk
        // is checked. Cut anywhere else, the same line is delivered.
        let limits = LineLimits {
            max_line_bytes: 8,
            max_buffered_bytes: 64,
        };
        let mut split = cut(&["12345678\r", "\n"], limits);
        let error = drain(&mut split)
            .await
            .expect_err("expected a refusal, received lines");
        assert!(
            matches!(
                error,
                Error::LimitExceeded {
                    subject: "bytes of one vendor output line",
                    limit: 8,
                    received: 9,
                }
            ),
            "expected a 9-byte line refusal, received {error:?}"
        );

        let mut whole = cut(&["12345678\r\n"], limits);
        assert_eq!(
            drain(&mut whole).await.expect("expected the line"),
            strings(["12345678"])
        );
    }

    #[tokio::test]
    async fn a_refusal_ends_the_stream_instead_of_skipping_a_line() {
        // The second line is over the cap. Once `next_line` has said so, a caller that asks again
        // must not be handed the lines around it as if nothing had been lost.
        let mut lines = stream(
            ["ok\n0123456789\nafter\n"],
            LineLimits {
                max_line_bytes: 8,
                max_buffered_bytes: 64,
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
                    limit: 8,
                    received: 10,
                    ..
                }
            ),
            "expected a line-limit refusal, received {error:?}"
        );
        let after = drain(&mut lines).await;
        assert!(
            matches!(after.as_deref(), Ok([])),
            "expected the end of the stream after a refusal, received {after:?}"
        );
    }

    #[tokio::test]
    async fn framing_does_not_depend_on_where_the_chunks_are_cut() {
        // Empty lines, CRLF, invalid UTF-8, multi-byte characters and an unterminated tail.
        let input =
            b"first\r\n\nsec\xffond\n\xe2\x82\xac euro\r\n\r\n{\"k\":1}\nlast \xc3".to_vec();
        let mut whole = RawChunkSource::stream(vec![input.clone()], LineLimits::default());
        let whole = drain(&mut whole).await.expect("expected lines");
        assert_eq!(
            whole,
            strings([
                "first",
                "",
                "sec\u{fffd}ond",
                "\u{20ac} euro",
                "",
                "{\"k\":1}",
                "last \u{fffd}"
            ])
        );
        for size in 1..=input.len() {
            let chunks = input.chunks(size).map(<[u8]>::to_vec).collect();
            let mut lines = RawChunkSource::stream(chunks, LineLimits::default());
            assert_eq!(
                drain(&mut lines).await.expect("expected lines"),
                whole,
                "expected the same lines cut every {size} bytes"
            );
        }
    }

    #[tokio::test]
    async fn delivers_repaired_lines_that_fit_the_buffer_cap_exactly() {
        // Two lines of 10 invalid bytes repair to 30 bytes each: 60, exactly the cap.
        let chunk = (0..2).flat_map(|_| invalid_line(10)).collect();
        let mut lines = RawChunkSource::stream(
            vec![chunk],
            LineLimits {
                max_line_bytes: 100,
                max_buffered_bytes: 60,
            },
        );

        let repaired = "\u{fffd}".repeat(10);
        for _ in 0..2 {
            assert_eq!(
                lines.next_line().await.expect("expected a line"),
                Some(repaired.clone())
            );
        }
        assert_eq!(lines.next_line().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn valid_utf8_at_exactly_both_caps_is_delivered() {
        let mut lines = stream(
            ["0123456789abcde\n"],
            LineLimits {
                max_line_bytes: 15,
                max_buffered_bytes: 16,
            },
        );

        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("0123456789abcde"))
        );
        assert_eq!(lines.next_line().await.expect("expected the end"), None);
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

    /// The credential name sits in the read that overflowed the cap and the value arrives in later
    /// reads. The tail is read between every read, so a fragment of the overlong line fails by name.
    #[test]
    fn an_overflowed_line_stays_discarded_across_later_reads() {
        let tail = StderrTail::with_capacity(16);
        tail.push(b"OPENAI_API_KEY=sk-proj-0123456789abcdef");
        assert_eq!(tail.read(), "", "expected nothing after the overflow");

        for (index, piece) in [&b"secret-suffix"[..], b"-more-secret", b"-and-more"]
            .into_iter()
            .enumerate()
        {
            tail.push(piece);
            let read = tail.read();
            assert_eq!(
                read, "",
                "expected no fragment of the overflowed line after continuation read {index} | received {read:?}"
            );
        }
    }

    /// The defaults: a 16 KiB cap fed the launcher's 16 KiB reads. The name lands in the second
    /// read, which is the window the overflow keeps, and the value arrives in the third.
    #[test]
    fn a_line_past_twice_the_default_cap_never_returns_its_credential_suffix() {
        let tail = StderrTail::default();
        let chunk = DEFAULT_STDERR_TAIL_BYTES;
        let mut line = vec![b'x'; 2 * chunk - 8];
        line.extend_from_slice(b"API_KEY=TOPSECRETVALUE0123456789 more text\n");
        for piece in line.chunks(chunk) {
            tail.push(piece);
        }

        let read = tail.read();
        assert!(
            !read.contains("TOPSECRET") && !read.contains("more text"),
            "expected no credential suffix from the overflowed line | received {read:?}"
        );
    }

    /// Only the rest of the overflowed line is lost: the next line, even in the same read as the
    /// terminator, is kept whole.
    #[test]
    fn the_line_after_an_overflowed_one_is_kept_intact() {
        let tail = StderrTail::with_capacity(48);
        tail.push(b"API_KEY=0123456789012345678901234567890123456789012345678901234567890");
        tail.push(b"still-the-same-secret-line");
        tail.push(b"end-of-secret\nnext: ordinary diagnostic\n");

        let read = tail.read();
        assert_eq!(
            read, "next: ordinary diagnostic\n",
            "expected the following line intact and no overflowed suffix | received {read:?}"
        );
    }

    #[test]
    fn a_crlf_split_across_reads_ends_the_discard_at_the_carriage_return() {
        let tail = StderrTail::with_capacity(32);
        tail.push(b"API_KEY=0123456789012345678901234567890");
        tail.push(b"trailing-secret");
        let read = tail.read();
        assert_eq!(
            read, "",
            "expected no suffix before the terminator arrives | received {read:?}"
        );
        tail.push(b"-more\r");
        tail.push(b"\nnext: ordinary diagnostic\n");

        let read = tail.read();
        assert!(
            !read.contains("trailing-secret"),
            "expected no suffix of the overflowed line | received {read:?}"
        );
        assert!(
            read.ends_with("next: ordinary diagnostic\n"),
            "expected the line after the CRLF intact | received {read:?}"
        );
    }

    #[test]
    fn a_lone_line_feed_or_carriage_return_ends_the_discard() {
        for terminator in ["\n", "\r"] {
            let tail = StderrTail::with_capacity(16);
            tail.push(b"API_KEY=0123456789012345678901234567890");
            tail.push(format!("secret{terminator}ok\n").as_bytes());

            let read = tail.read();
            assert!(
                !read.contains("secret") && read.contains("ok\n"),
                "expected only the overflowed remainder dropped for {terminator:?} | received {read:?}"
            );
        }
    }

    /// The discard belongs to the shared buffer, not to one handle: the launcher fills one clone
    /// while the control handle reads another.
    #[test]
    fn a_clone_shares_the_discard_of_an_overflowed_line() {
        let writer = StderrTail::with_capacity(16);
        let reader = writer.clone();
        writer.push(b"API_KEY=0123456789012345678901234567890");
        reader.push(b"secret-suffix");
        let read = writer.read();
        assert_eq!(
            read, "",
            "expected the writer clone to drop the continuation pushed through the reader clone | received {read:?}"
        );

        writer.push(b"secret-tail\nok\n");
        for (name, tail) in [("writer", &writer), ("reader", &reader)] {
            let read = tail.read();
            assert!(
                !read.contains("secret") && read.contains("ok\n"),
                "expected the {name} clone to drop the overflowed remainder only | received {read:?}"
            );
        }
    }

    #[test]
    fn a_zero_cap_retains_nothing_and_never_leaks() {
        let tail = StderrTail::with_capacity(0);
        tail.push(b"");
        tail.push(b"API_KEY=zero-cap-secret");
        tail.push(b"more-secret\nnext line\n");
        tail.push(b"another API_KEY=second-secret");

        let read = tail.read();
        assert_eq!(
            read, "",
            "expected an empty tail at zero cap | received {read:?}"
        );
    }

    /// A credential whose value is cut by the overflow boundary itself.
    #[test]
    fn a_credential_split_by_the_overflow_boundary_is_never_returned() {
        let tail = StderrTail::with_capacity(24);
        tail.push(b"noise\nAuthorization: Bearer sk-live-");
        tail.push(b"0123456789abcdef");
        tail.push(b"ghijklmnop\nok\n");

        let read = tail.read();
        assert!(
            !read.contains("sk-live")
                && !read.contains("0123456789abcdef")
                && !read.contains("ghijklmnop"),
            "expected no credential fragment | received {read:?}"
        );
    }

    /// The redaction rules read a value across a line break, so a discarded line that ends on the
    /// name or the separator takes the value line with it.
    #[test]
    fn an_overflowed_line_ending_on_a_separator_drops_the_value_on_the_next_line() {
        let tail = StderrTail::with_capacity(16);
        tail.push(b"xxxxxxxxxxxxxxxxxxxxxxxx API_KEY=");
        tail.push(b"\nsk-secret-value\n");

        let read = tail.read();
        assert!(
            !read.contains("sk-secret-value"),
            "expected no value from the line after an overflowed assignment | received {read:?}"
        );
    }

    /// The same shape at the line rounding cut, where the overflow lands inside an earlier line.
    #[test]
    fn a_rounded_cut_ending_on_a_header_drops_the_bearer_line() {
        let tail = StderrTail::with_capacity(30);
        tail.push(b"noise noise Authorization:\n  Bearer sk-live-secret\n");

        let read = tail.read();
        assert!(
            !read.contains("sk-live-secret"),
            "expected no token from the line after a cut header | received {read:?}"
        );
    }

    #[test]
    fn a_value_line_dropped_after_a_cut_is_followed_by_an_intact_diagnostic() {
        let tail = StderrTail::with_capacity(32);
        tail.push(b"noise noise noise noise Authorization:\n");
        tail.push(b"\n  Bearer sk-live-");
        assert_eq!(tail.read(), "", "expected the split token line dropped");
        tail.push(b"secret\nnext: ordinary diagnostic\n");

        let read = tail.read();
        assert_eq!(
            read, "next: ordinary diagnostic\n",
            "expected only the value line dropped | received {read:?}"
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
