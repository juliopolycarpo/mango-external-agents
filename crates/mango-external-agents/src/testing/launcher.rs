//! A launcher that spawns nothing and replays what a vendor would have said.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::Notify;

use crate::error::{Error, Result};
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

    /// Writes this to stderr before exiting.
    #[must_use]
    pub fn with_stderr(mut self, stderr: impl AsRef<[u8]>) -> Self {
        self.stderr = stderr.as_ref().to_vec();
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
        self.launches().pop()
    }

    /// Every line the library wrote to a child's stdin, across all of them.
    pub fn written(&self) -> Vec<String> {
        self.state
            .written
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
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
                message: String::from("no fake process was queued for this launch"),
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
        let child = Arc::new(ChildState {
            stdout: Mutex::new(process.stdout.into_iter().collect()),
            ended: Mutex::new(process.responder.is_none()),
            responder: process.responder,
            exit: process.exit,
            stderr,
            written: Arc::clone(&self.state.written),
            changed: Notify::new(),
        });

        Ok(ManagedProcess {
            stdout: Box::new(FakeStdout {
                state: Arc::clone(&child),
            }),
            stdin: wants_stdin.then(|| -> Box<dyn ByteSink> {
                Box::new(FakeStdin {
                    state: Arc::clone(&child),
                    partial: Vec::new(),
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
    exit: ExitStatus,
    stderr: StderrTail,
    written: Arc<Mutex<Vec<String>>>,
    changed: Notify,
}

impl ChildState {
    fn end(&self) {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.changed.notify_waiters();
    }

    fn is_ended(&self) -> bool {
        *self.ended.lock().unwrap_or_else(PoisonError::into_inner)
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
            changed.await;
        }
    }
}

struct FakeStdin {
    state: Arc<ChildState>,
    partial: Vec<u8>,
}

#[async_trait::async_trait]
impl ByteSink for FakeStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.partial.extend_from_slice(bytes);
        while let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
            let record: Vec<u8> = self.partial.drain(..=newline).collect();
            let line = String::from_utf8_lossy(&record).trim_end().to_owned();
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
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        self.state.end();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{FakeLauncher, FakeProcess};
    use crate::error::Error;
    use crate::process::{LaunchSpec, LineLimits, LineStream, ProcessLauncher};
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
