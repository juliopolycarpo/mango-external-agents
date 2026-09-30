//! A launcher that spawns nothing and replays what a vendor would have said.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

use crate::error::{Error, Result};
use crate::host::CancelToken;
use crate::process::{
    ByteSink, ByteSource, ExitStatus, LaunchSpec, ManagedProcess, ProcessControl, ProcessLauncher,
    StderrTail,
};
use crate::session::CancelReason;

/// What a fake child answers one written line with.
type Responder = Arc<dyn Fn(&str) -> Vec<String> + Send + Sync>;

/// What one fake child does.
///
/// Two shapes cover every dialect the library drives: a transcript, where the child writes a fixed
/// sequence and exits, and a responder, where each line the library writes produces the lines a
/// vendor would have answered with.
#[derive(Clone, Default)]
pub struct FakeProcess {
    stdout: Vec<String>,
    stderr: Vec<u8>,
    exit: ExitStatus,
    responder: Option<Responder>,
    end_stdout_when: Option<CancelToken>,
    announcer: Option<Announcer>,
    stdin_failure: Option<StdinFailure>,
}

/// When and how a fake child's stdin starts refusing writes.
#[derive(Clone, Debug)]
struct StdinFailure {
    /// How many complete lines land before the pipe breaks.
    after_lines: usize,
    message: String,
}

impl StdinFailure {
    fn error(&self) -> Error {
        Error::Link {
            peer: String::from("fake child stdin"),
            message: self.message.clone(),
        }
    }
}

/// A handle that makes a fake child speak without being written to first.
///
/// A responder only answers: it says something because the library asked. The peers this library
/// drives also announce on their own initiative, and a whole class of behaviour — what a session
/// does about traffic that arrives while it is waiting — cannot be reached by answering alone.
/// Lines announced before the child is launched are held and delivered when it starts.
///
/// # Example
///
/// ```
/// use mango_external_agents::testing::{Announcer, FakeLauncher, FakeProcess};
///
/// let announcer = Announcer::new();
/// let launcher = FakeLauncher::new();
/// launcher.push(FakeProcess::responding(|_| Vec::new()).announcing(announcer.clone()));
/// announcer.announce(r#"{"method":"rateLimits","params":{}}"#);
/// ```
#[derive(Clone, Debug, Default)]
pub struct Announcer {
    state: Arc<AnnouncerState>,
}

#[derive(Default)]
struct AnnouncerState {
    /// The child to speak through, once one has been launched.
    child: Mutex<Option<Arc<ChildState>>>,
    /// What was announced before that, in order.
    pending: Mutex<Vec<String>>,
}

impl std::fmt::Debug for AnnouncerState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnnouncerState")
            .field(
                "attached",
                &self
                    .child
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some(),
            )
            .field(
                "pending",
                &self
                    .pending
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len(),
            )
            .finish()
    }
}

impl Announcer {
    /// A handle attached to no child yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Puts one line on the child's stdout, as a peer announcing something would.
    pub fn announce(&self, line: impl Into<String>) {
        let line = line.into();
        let child = self
            .state
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(child) = child else {
            self.state
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line);
            return;
        };
        child.announce([line]);
    }

    fn attach(&self, child: &Arc<ChildState>) {
        *self
            .state
            .child
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Arc::clone(child));
        let pending = std::mem::take(
            &mut *self
                .state
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        child.announce(pending);
    }
}

impl std::fmt::Debug for FakeProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FakeProcess")
            .field("stdout", &self.stdout.len())
            .field("stderr", &self.stderr.len())
            .field("exit", &self.exit)
            .field("responder", &self.responder.is_some())
            .finish()
    }
}

impl FakeProcess {
    /// A child that writes these lines and exits.
    pub fn transcript<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            stdout: lines.into_iter().map(Into::into).collect(),
            ..Self::default()
        }
    }

    /// A child that answers each line written to it.
    ///
    /// Its stdout stays open until stdin is closed or it is killed, which is what a persistent
    /// JSON-RPC peer looks like.
    pub fn responding(responder: impl Fn(&str) -> Vec<String> + Send + Sync + 'static) -> Self {
        Self {
            responder: Some(Arc::new(responder)),
            ..Self::default()
        }
    }

    /// Closes the child's stdout when `signal` is cancelled, while leaving the child alive.
    ///
    /// This models a peer whose output pipe disappears before its process has exited, so callers
    /// can verify that their connection-failure path owns process cleanup.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::CancelToken;
    /// use mango_external_agents::testing::FakeProcess;
    ///
    /// let close_stdout = CancelToken::new();
    /// let process = FakeProcess::responding(|_| Vec::new()).ending_stdout_when(close_stdout);
    /// let _ = process;
    /// ```
    #[must_use]
    pub fn ending_stdout_when(mut self, signal: CancelToken) -> Self {
        self.end_stdout_when = Some(signal);
        self
    }

    /// Lets `announcer` put lines on this child's stdout at any point in its life.
    #[must_use]
    pub fn announcing(mut self, announcer: Announcer) -> Self {
        self.announcer = Some(announcer);
        self
    }

    /// Writes this to stderr before exiting.
    #[must_use]
    pub fn with_stderr(mut self, stderr: impl AsRef<[u8]>) -> Self {
        self.stderr = stderr.as_ref().to_vec();
        self
    }

    /// Makes stdin refuse writes once `lines` complete lines have landed, like a peer whose pipe
    /// broke: the next line is not recorded, no responder answers it, and the write returns
    /// [`Error::Link`] carrying `message`, as does every later one.
    ///
    /// Every other fake stdin answers `Ok`, so nothing else can reach the path where a harness's
    /// write to its vendor fails. That includes a reply to a request the vendor made, which has to
    /// end the link rather than leave the vendor waiting for an answer that will never come.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::testing::FakeProcess;
    ///
    /// // The handshake's first two lines land; the third write hits a broken pipe.
    /// let process = FakeProcess::responding(|_| Vec::new()).failing_stdin_after(2, "EPIPE");
    /// # let _ = process;
    /// ```
    #[must_use]
    pub fn failing_stdin_after(mut self, lines: usize, message: impl Into<String>) -> Self {
        self.stdin_failure = Some(StdinFailure {
            after_lines: lines,
            message: message.into(),
        });
        self
    }

    /// Exits with this status.
    #[must_use]
    pub fn with_exit(mut self, exit: ExitStatus) -> Self {
        self.exit = exit;
        self
    }

    /// Writes these lines before anything the responder produces.
    ///
    /// Spliced in front of whatever was already queued rather than assigned over it: a greeting
    /// chained onto a [`transcript`](Self::transcript) that replaced it would silently throw the
    /// transcript away, which reads as a fixture that stopped replaying rather than as the mistake
    /// it is.
    #[must_use]
    pub fn with_greeting<I, S>(mut self, lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut greeting: Vec<String> = lines.into_iter().map(Into::into).collect();
        greeting.append(&mut self.stdout);
        self.stdout = greeting;
        self
    }
}

/// A [`ProcessLauncher`] that spawns nothing.
///
/// Records what it was asked for — argv, cwd and the environment the allowlist produced — so a
/// test can assert on the command line a harness built and prove that a host's own secret did not
/// reach a child.
///
/// # Example
///
/// ```
/// use mango_external_agents::testing::{FakeLauncher, FakeProcess};
///
/// let launcher = FakeLauncher::new();
/// launcher.push(FakeProcess::transcript(["{\"type\":\"system\"}"]));
/// assert!(launcher.launches().is_empty());
/// ```
#[derive(Clone, Debug, Default)]
pub struct FakeLauncher {
    state: Arc<LauncherState>,
}

#[derive(Debug, Default)]
struct LauncherState {
    queued: Mutex<VecDeque<FakeProcess>>,
    launches: Mutex<Vec<LaunchSpec>>,
    /// Shared with every child, so `written` is one list across all of them.
    written: Arc<Mutex<Vec<String>>>,
    /// Children that have not exited or been killed yet.
    live_children: Arc<AtomicUsize>,
}

impl FakeLauncher {
    /// A launcher with nothing queued. Spawning from it fails, which is the honest answer.
    pub fn new() -> Self {
        Self::default()
    }

    /// A launcher whose one child writes `transcript`, a line at a time, then exits.
    ///
    /// The shape a captured fixture has: newline-delimited JSON, blank lines ignored.
    pub fn scripted(transcript: &str) -> Self {
        let launcher = Self::new();
        launcher.push(FakeProcess::transcript(
            transcript.lines().filter(|line| !line.trim().is_empty()),
        ));
        launcher
    }

    /// Queues the next child.
    pub fn push(&self, process: FakeProcess) {
        self.state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(process);
    }

    /// Every launch, in order.
    pub fn launches(&self) -> Vec<LaunchSpec> {
        self.state
            .launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The most recent launch.
    pub fn last_launch(&self) -> Option<LaunchSpec> {
        self.state
            .launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last()
            .cloned()
    }

    /// Every line the library wrote to a child's stdin, across all of them.
    pub fn written(&self) -> Vec<String> {
        self.state
            .written
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The fake children that are still alive.
    ///
    /// Tests use this to prove a harness reaped a peer after a broken output pipe, rather than
    /// merely dropping the stream handle that exposed the failure.
    /// For example, assert `launcher.live_children() == 0` after session shutdown.
    pub fn live_children(&self) -> usize {
        self.state.live_children.load(Ordering::Acquire)
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for FakeLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let queued = self
            .state
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        let Some(process) = queued else {
            return Err(Error::Launch {
                program: spec.program().unwrap_or_default().to_owned(),
                message: String::from("no fake process queued for this launch"),
            });
        };

        let wants_stdin = spec.stdin;
        self.state
            .launches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(spec);

        let stderr = StderrTail::default();
        stderr.push(&process.stderr);
        let ended = process.responder.is_none();
        if !ended {
            self.state.live_children.fetch_add(1, Ordering::AcqRel);
        }
        let child = Arc::new(ChildState {
            stdout: Mutex::new(process.stdout.into_iter().collect()),
            ended: Mutex::new(ended),
            responder: process.responder,
            end_stdout_when: process.end_stdout_when,
            exit: process.exit,
            stderr,
            written: Arc::clone(&self.state.written),
            live_children: Arc::clone(&self.state.live_children),
            changed: Notify::new(),
        });
        if let Some(announcer) = &process.announcer {
            announcer.attach(&child);
        }
        let stdin_failure = process.stdin_failure;

        Ok(ManagedProcess {
            stdout: Box::new(FakeStdout {
                state: Arc::clone(&child),
            }),
            stdin: wants_stdin.then(|| -> Box<dyn ByteSink> {
                Box::new(FakeStdin {
                    state: Arc::clone(&child),
                    partial: Vec::new(),
                    failure: stdin_failure,
                    lines_landed: 0,
                    broken: false,
                })
            }),
            control: child,
        })
    }
}

struct ChildState {
    stdout: Mutex<VecDeque<String>>,
    ended: Mutex<bool>,
    responder: Option<Responder>,
    end_stdout_when: Option<CancelToken>,
    exit: ExitStatus,
    stderr: StderrTail,
    written: Arc<Mutex<Vec<String>>>,
    live_children: Arc<AtomicUsize>,
    changed: Notify,
}

impl ChildState {
    fn end(&self) {
        let mut ended = self.ended.lock().unwrap_or_else(PoisonError::into_inner);
        if *ended {
            return;
        }
        *ended = true;
        self.live_children.fetch_sub(1, Ordering::AcqRel);
        drop(ended);
        self.changed.notify_waiters();
    }

    fn is_ended(&self) -> bool {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues lines the child writes without having been asked for them.
    fn announce<I: IntoIterator<Item = String>>(&self, lines: I) {
        self.stdout
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(lines);
        self.changed.notify_waiters();
    }
}

#[async_trait::async_trait]
impl ProcessControl for ChildState {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        self.stderr.read()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        loop {
            let changed = self.changed.notified();
            if self.is_ended() {
                return Ok(self.exit);
            }
            changed.await;
        }
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        self.end();
        Ok(())
    }
}

struct FakeStdout {
    state: Arc<ChildState>,
}

#[async_trait::async_trait]
impl ByteSource for FakeStdout {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let changed = self.state.changed.notified();
            {
                let mut stdout = self
                    .state
                    .stdout
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some(line) = stdout.pop_front() {
                    return Ok(Some(format!("{line}\n").into_bytes()));
                }
            }
            if self.state.is_ended() {
                return Ok(None);
            }
            if let Some(signal) = &self.state.end_stdout_when {
                tokio::select! {
                    () = signal.cancelled() => return Ok(None),
                    () = changed => {}
                }
            } else {
                changed.await;
            }
        }
    }
}

struct FakeStdin {
    state: Arc<ChildState>,
    partial: Vec<u8>,
    failure: Option<StdinFailure>,
    lines_landed: usize,
    /// Set once a write was refused: every later write is refused too, even one with no newline.
    broken: bool,
}

#[async_trait::async_trait]
impl ByteSink for FakeStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        if self.broken
            && let Some(failure) = &self.failure
        {
            return Err(failure.error());
        }
        // Each byte is scanned once, from where the last write stopped, and the consumed lines are
        // removed in one pass at the end rather than one drain per line.
        let mut line_start = 0;
        let mut scan_from = self.partial.len();
        self.partial.extend_from_slice(bytes);
        while let Some(offset) = self.partial[scan_from..]
            .iter()
            .position(|byte| *byte == b'\n')
        {
            if let Some(failure) = &self.failure
                && self.lines_landed >= failure.after_lines
            {
                // A broken pipe stays broken, and the bytes of the refused line are gone with it.
                self.broken = true;
                self.partial.clear();
                return Err(failure.error());
            }
            self.lines_landed += 1;
            let line_end = scan_from + offset + 1;
            let line = String::from_utf8_lossy(&self.partial[line_start..line_end])
                .trim_end()
                .to_owned();
            line_start = line_end;
            scan_from = line_end;
            self.state
                .written
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(line.clone());
            if let Some(responder) = &self.state.responder {
                let answers = responder(&line);
                let mut stdout = self
                    .state
                    .stdout
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                stdout.extend(answers);
            }
            self.state.changed.notify_waiters();
        }
        self.partial.drain(..line_start);
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.state.end();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Announcer, FakeLauncher, FakeProcess};
    use crate::error::Error;
    use crate::host::CancelToken;
    use crate::process::{LaunchSpec, LineLimits, LineStream, ProcessLauncher};
    use crate::session::CancelReason;
    use std::collections::BTreeMap;

    fn spec(argv: &[&str]) -> LaunchSpec {
        LaunchSpec {
            argv: argv.iter().map(|argument| (*argument).to_owned()).collect(),
            cwd: "/workspace".into(),
            env: BTreeMap::from([(String::from("PATH"), String::from("/bin"))]),
            stdin: true,
            hide_window: true,
        }
    }

    /// A responder only speaks when spoken to. The peers this library drives also announce, and a
    /// session's behaviour while it is waiting cannot be reached by answering alone — so an
    /// announcement made before the child was launched has to survive until it is.
    #[tokio::test]
    async fn an_announcer_speaks_for_a_child_before_and_after_it_is_launched() {
        let announcer = Announcer::new();
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::responding(|_| Vec::new()).announcing(announcer.clone()));
        announcer.announce("{\"method\":\"queued/before/launch\"}");

        let child = launcher
            .spawn(spec(&["codex", "app-server"]))
            .await
            .expect("expected a child");
        announcer.announce("{\"method\":\"announced/while/running\"}");

        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        assert_eq!(
            lines.next_line().await.expect("expected a queued line"),
            Some(String::from("{\"method\":\"queued/before/launch\"}"))
        );
        assert_eq!(
            lines.next_line().await.expect("expected a live line"),
            Some(String::from("{\"method\":\"announced/while/running\"}"))
        );
        assert!(
            launcher.written().is_empty(),
            "expected an announcement to need no question first, received {:?}",
            launcher.written()
        );
    }

    #[tokio::test]
    async fn replays_a_transcript_a_line_at_a_time_and_then_ends() {
        let launcher = FakeLauncher::scripted("{\"type\":\"system\"}\n\n{\"type\":\"result\"}\n");
        let child = launcher
            .spawn(spec(&["claude", "-p"]))
            .await
            .expect("expected a child");

        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("{\"type\":\"system\"}"))
        );
        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from("{\"type\":\"result\"}"))
        );
        assert_eq!(lines.next_line().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn records_the_command_line_the_harness_built() {
        let launcher = FakeLauncher::scripted("");
        launcher
            .spawn(spec(&["claude", "-p", "--output-format", "stream-json"]))
            .await
            .expect("expected a child");

        let launch = launcher.last_launch().expect("expected one launch");
        assert_eq!(launch.argv[0], "claude");
        assert_eq!(launch.cwd.to_string_lossy(), "/workspace");
        assert_eq!(launch.env.get("PATH").map(String::as_str), Some("/bin"));
    }

    /// A pipe that breaks lets the lines before the break land, refuses the one at the break and
    /// every one after it, records none of them and never asks the responder about them.
    #[tokio::test]
    async fn a_failing_stdin_lets_the_first_lines_land_and_refuses_the_rest() {
        let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let responder_asked = std::sync::Arc::clone(&asked);
        let launcher = FakeLauncher::new();
        launcher.push(
            FakeProcess::responding(move |_| {
                responder_asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Vec::new()
            })
            .failing_stdin_after(2, "EPIPE"),
        );
        let mut child = launcher
            .spawn(spec(&["codex", "app-server"]))
            .await
            .expect("expected a child");
        let mut stdin = child.stdin.take().expect("expected a stdin");

        for line in ["one\n", "two\n"] {
            stdin
                .write_all(line.as_bytes())
                .await
                .unwrap_or_else(|error| panic!("expected {line:?} to land | received {error}"));
        }
        // The last one has no newline: a broken pipe refuses that too, rather than buffering it.
        for line in ["three\n", "four\n", "five"] {
            let refused = stdin.write_all(line.as_bytes()).await;
            assert!(
                matches!(&refused, Err(Error::Link { message, .. }) if message == "EPIPE"),
                "expected {line:?} refused with Link(EPIPE) | received {refused:?}"
            );
        }

        assert_eq!(
            launcher.written(),
            vec![String::from("one"), String::from("two")],
            "expected only the lines before the break recorded"
        );
        assert_eq!(
            asked.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "expected the responder asked about the two lines that landed only"
        );
    }

    /// Feeds `input` to a fresh child's stdin in `sizes`-long writes (cycled) and returns the
    /// lines the fake recorded.
    async fn written_lines(input: &[u8], sizes: &[usize]) -> Vec<String> {
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let mut child = launcher
            .spawn(spec(&["codex", "app-server"]))
            .await
            .expect("expected a child");
        let mut stdin = child.stdin.take().expect("expected a stdin");
        let mut rest = input;
        for size in sizes.iter().copied().cycle() {
            if rest.is_empty() {
                break;
            }
            let (piece, tail) = rest.split_at(size.max(1).min(rest.len()));
            stdin
                .write_all(piece)
                .await
                .expect("expected the write to land");
            rest = tail;
        }
        launcher.written()
    }

    /// What the peer reads: the stream cut at every newline, however it was chunked on the way.
    fn expected_lines(input: &[u8]) -> Vec<String> {
        let mut lines: Vec<&[u8]> = input.split(|byte| *byte == b'\n').collect();
        lines.pop();
        lines
            .into_iter()
            .map(|line| String::from_utf8_lossy(line).trim_end().to_owned())
            .collect()
    }

    /// A deterministic pseudo-random stream of short lines: some empty, some with CRs, trailing
    /// blanks or bytes that are not UTF-8, and a tail with no newline.
    fn noisy_stream(seed: u64) -> Vec<u8> {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let alphabet: &[u8] = b"ab {}\":,\r\t\n\n\xc3\xa9\xff\xe2\x80";
        (0..2_000)
            .map(|_| alphabet[usize::try_from(next() % alphabet.len() as u64).unwrap_or(0)])
            .collect()
    }

    #[tokio::test]
    async fn stdin_lines_do_not_depend_on_how_the_bytes_are_chunked() {
        for seed in 1..=40 {
            let input = noisy_stream(seed);
            let expected = expected_lines(&input);
            for sizes in [&[1][..], &[2, 3], &[7], &[64], &[1, 500, 2], &[usize::MAX]] {
                assert_eq!(
                    written_lines(&input, sizes).await,
                    expected,
                    "expected the recorded lines to match the newline split | seed: {seed} | write sizes: {sizes:?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_line_split_across_writes_is_recorded_once_and_the_tail_waits() {
        let lines = written_lines(b"one\r\ntw", &[4, 100]).await;
        assert_eq!(lines, vec![String::from("one")]);
        let lines = written_lines(b"one\ntwo\nthr", &[3, 3, 3, 3]).await;
        assert_eq!(lines, vec![String::from("one"), String::from("two")]);
    }

    /// One write carrying every line, the shape that once cost time quadratic in its size.
    #[tokio::test]
    async fn one_large_write_of_many_lines_records_every_line_in_order() {
        let lines: Vec<String> = (0..25_000)
            .map(|index| format!("{{\"id\":\"{index}\",\"p\":\"{}\"}}", "x".repeat(24)))
            .collect();
        let input = format!("{}\n", lines.join("\n"));
        let recorded = written_lines(input.as_bytes(), &[usize::MAX]).await;
        assert_eq!(
            (recorded.len(), recorded.first(), recorded.last()),
            (lines.len(), lines.first(), lines.last()),
            "expected every line of a {} byte write recorded in order",
            input.len()
        );
        assert_eq!(
            recorded, lines,
            "expected the recorded lines to equal the written lines"
        );
    }

    #[tokio::test]
    async fn answers_each_line_written_to_it() {
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::responding(|line| {
            if line.contains("thread/start") {
                return vec![String::from(r#"{"id":"1","result":{"threadId":"t-1"}}"#)];
            }
            Vec::new()
        }));
        let mut child = launcher
            .spawn(spec(&["codex", "app-server"]))
            .await
            .expect("expected a child");

        let mut stdin = child.stdin.take().expect("expected a stdin");
        stdin
            .write_all(b"{\"id\":\"1\",\"method\":\"thread/start\"}\n")
            .await
            .expect("expected the write to land");

        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        assert_eq!(
            lines.next_line().await.expect("expected a line"),
            Some(String::from(r#"{"id":"1","result":{"threadId":"t-1"}}"#))
        );
        assert_eq!(
            launcher.written(),
            vec![String::from("{\"id\":\"1\",\"method\":\"thread/start\"}")]
        );
    }

    #[tokio::test]
    async fn a_responding_child_ends_when_its_input_is_closed() {
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let mut child = launcher
            .spawn(spec(&["codex", "app-server"]))
            .await
            .expect("expected a child");

        let mut stdin = child.stdin.take().expect("expected a stdin");
        stdin.close().await.expect("expected the close to land");

        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        assert_eq!(lines.next_line().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn closing_stdout_by_signal_keeps_the_child_alive_until_it_is_killed() {
        let launcher = FakeLauncher::new();
        let close_stdout = CancelToken::new();
        launcher
            .push(FakeProcess::responding(|_| Vec::new()).ending_stdout_when(close_stdout.clone()));
        let child = launcher
            .spawn(spec(&["codex", "app-server"]))
            .await
            .expect("expected a child");
        let control = child.control.clone();
        let mut lines = LineStream::new(child.stdout, LineLimits::default());

        close_stdout.cancel();
        assert_eq!(lines.next_line().await.expect("expected stdout EOF"), None);
        assert_eq!(
            launcher.live_children(),
            1,
            "expected the child to remain alive"
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), control.wait())
                .await
                .is_err(),
            "expected stdout EOF not to imply that the child exited"
        );

        control
            .kill(CancelReason::Shutdown)
            .await
            .expect("expected fake child cleanup");
        assert_eq!(
            launcher.live_children(),
            0,
            "expected the child to be reaped"
        );
    }

    #[tokio::test]
    async fn hands_back_a_redacted_stderr_tail() {
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::transcript(Vec::<String>::new()).with_stderr(
            "Authorization: Bearer top-secret API_KEY=another-secret redis://app:password@db/main",
        ));
        let child = launcher
            .spawn(spec(&["claude"]))
            .await
            .expect("expected a child");

        let tail = child.control.stderr_tail();
        assert!(!tail.contains("top-secret"), "received {tail:?}");
        assert!(!tail.contains("another-secret"), "received {tail:?}");
        assert!(!tail.contains("password@"), "received {tail:?}");
    }

    /// A greeting goes in front of what is already queued. Assigning over it instead threw the
    /// transcript away without saying so, which reads as a fixture that stopped replaying.
    #[tokio::test]
    async fn a_greeting_goes_in_front_of_what_was_already_queued() {
        let launcher = FakeLauncher::new();
        launcher.push(FakeProcess::transcript(["queued"]).with_greeting(["hello", "ready"]));
        let child = launcher
            .spawn(spec(&["codex", "app-server"]))
            .await
            .expect("expected a child");

        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        let mut seen = Vec::new();
        while let Some(line) = lines.next_line().await.expect("expected a line or the end") {
            seen.push(line);
        }
        assert_eq!(
            seen,
            vec![
                String::from("hello"),
                String::from("ready"),
                String::from("queued"),
            ]
        );
    }

    /// A vendor that failed exited non-zero, and a fake that could not say so would make every
    /// failure path untestable.
    #[tokio::test]
    async fn a_child_reports_the_exit_status_it_was_given() {
        let launcher = FakeLauncher::new();
        launcher.push(
            FakeProcess::transcript(Vec::<String>::new()).with_exit(crate::ExitStatus {
                code: Some(2),
                signal: None,
            }),
        );
        let child = launcher
            .spawn(spec(&["claude"]))
            .await
            .expect("expected a child");

        let status = child.control.wait().await.expect("expected an exit status");
        assert_eq!(status.code, Some(2));
        assert!(
            !status.success(),
            "expected a failing status, received {status:?}"
        );
    }

    #[tokio::test]
    async fn a_launch_nobody_queued_is_refused_rather_than_silently_empty() {
        let error = FakeLauncher::new()
            .spawn(spec(&["claude"]))
            .await
            .err()
            .expect("expected a refusal, received a child");
        assert!(
            matches!(error, Error::Launch { .. }),
            "expected a launch refusal, received {error:?}"
        );
    }
}
