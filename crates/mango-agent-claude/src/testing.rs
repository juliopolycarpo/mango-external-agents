//! A fake `claude` binary, for proving a host or this harness without Claude Code installed.
//!
//! [`FakeClaudeCli`] is a [`ProcessLauncher`] that answers the three probes the harness runs
//! (`--version`, `--help` and `auth status`) from its argv, and every turn from a script queued with
//! [`with_turn`](FakeClaudeCli::with_turn). It records every launch's argv and every line the
//! library wrote to a child's stdin, so a test can assert on what reached the vendor command line.
//!
//! Core's own [`FakeLauncher`](mango_external_agents::testing::FakeLauncher) is first-in-first-out
//! and ties a child's end of output to its stdin being closed. Neither fits this dialect. A turn
//! here spawns four children in a fixed order only the argv distinguishes, and `claude --print`
//! reads stdin to EOF **before** it starts working, so a fake whose stdout ended when stdin closed
//! could never model a turn that is still running when a host cancels it.
//!
//! Nothing here spawns a process, and nothing here reads a credential: `auth status` answers
//! [`SIGNED_IN`] or whatever [`with_auth`](FakeClaudeCli::with_auth) was given, and any other `auth`
//! subcommand is refused. The fake is **not** a fixture. Captured contracts belong under
//! `fixtures/` and are produced by `mea capture` against a real CLI; this is a named fake, which is
//! what the repository's own rule asks for in place of inline stubs.
//!
//! # Example
//!
//! ```
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! use mango_agent_claude::testing::{FakeClaudeCli, HELP_WITH_JSON_SCHEMA, Run};
//! use mango_external_agents::{LaunchSpec, ProcessLauncher};
//!
//! let cli = FakeClaudeCli::new()
//!     .with_help(HELP_WITH_JSON_SCHEMA)
//!     .with_turn(Run::answering_schema(&serde_json::json!({"word": "ok"})));
//!
//! let probe = LaunchSpec {
//!     argv: vec!["claude".into(), "--version".into()],
//!     cwd: std::env::temp_dir(),
//!     env: Default::default(),
//!     stdin: false,
//!     hide_window: false,
//! };
//! cli.spawn(probe).await.expect("expected the version probe to be answered");
//! assert_eq!(cli.launches().len(), 1);
//! assert!(cli.turn_argvs().is_empty(), "a probe is not a turn");
//! # }
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use mango_external_agents::{
    ByteSink, ByteSource, CancelReason, Error, ExitStatus, InterruptOutcome, LaunchSpec,
    ManagedProcess, ProcessControl, ProcessLauncher, Result, StderrTail,
};
use tokio::sync::Notify;

/// The `--help` a fake build prints unless a test says otherwise: Claude Code 2.1.260, a build that
/// predates `--json-schema`.
pub const DEFAULT_HELP: &str = include_str!("../fixtures/claude/help/2.1.260.txt");

/// The whole `--help` surface of Claude Code 2.1.270, which declares `--json-schema` and
/// `--mcp-config`.
pub const HELP_WITH_JSON_SCHEMA: &str = include_str!("../fixtures/claude/help/2.1.270.txt");

/// The `--version` banner a fake build prints unless a test says otherwise.
pub const DEFAULT_VERSION: &str = "2.1.270 (Claude Code)";

/// A signed-in `auth status`, as the committed contract capture shapes it.
pub const SIGNED_IN: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max"}"#;

/// A signed-out `auth status`.
pub const SIGNED_OUT: &str = r#"{"loggedIn":false}"#;

/// A turn's spawn, held open for as long as a test needs the window it makes.
///
/// Both halves are `notify_one` rather than `notify_waiters`, so neither side has to arrive first:
/// a permit waits for whoever gets there second.
#[derive(Clone)]
pub struct SpawnGate {
    arrived: Arc<Notify>,
    release: Arc<Notify>,
}

impl SpawnGate {
    /// Returns once a turn's spawn is in flight and waiting to be released.
    pub async fn wait_for_spawn(&self) {
        self.arrived.notified().await;
    }

    /// Lets the held spawn finish.
    pub fn release(&self) {
        self.release.notify_one();
    }
}

/// How a turn child answers the two things a stop asks of it, for a test that times a stop.
///
/// The durations are the child's own behaviour, not a wait the test performs: each one is how long
/// the fake host launcher takes to answer that call, measured on the runtime's clock, so a test
/// under a paused clock reads the stop's bound off `Instant::elapsed` to the millisecond.
#[derive(Clone, Copy, Debug)]
pub struct SlowStop {
    /// How long the graceful interrupt takes to be delivered. The child does not exit for it.
    /// `None` is a launcher with no graceful interrupt, which is what Windows reports.
    pub interrupt: Option<std::time::Duration>,
    /// How long the kill takes to answer.
    pub kill: std::time::Duration,
    /// Whether the child is gone once the kill has answered.
    pub kill_ends_child: bool,
}

/// What one fake child does once it is spawned.
#[derive(Clone, Debug)]
pub enum Run {
    /// Write these lines, then exit with this status.
    Transcript {
        /// The stdout lines, in order.
        lines: Vec<String>,
        /// How the child then exits.
        exit: ExitStatus,
    },
    /// Write these lines, then stay alive until something ends it.
    ///
    /// What a turn looks like while it is running: stdin is already closed, and the process is
    /// working.
    Stalls {
        /// The stdout lines written before it goes quiet.
        lines: Vec<String>,
    },
}

/// The session id the scripted schema turns announce.
const SCHEMA_TURN_SESSION_ID: &str = "b01414e7-4b4b-43a2-9109-a33e21664340";

/// A credential expiry the scripted answered turn carries on its `result` record, as the vendor's
/// own stream does.
///
/// Nothing in the harness may forward it to a host, so a test that reads the events a turn
/// produced asserts that this text appears in none of them.
pub const CREDENTIAL_EXPIRY_CANARY: &str = "CANARY-credential-expiry";

/// Joins records into the JSONL text [`Run::replaying`] reads.
fn frames<const N: usize>(records: [serde_json::Value; N]) -> String {
    records
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

impl Run {
    /// A turn that answers a `--json-schema` request with `answer`, as Claude Code 2.1.325 streamed
    /// one (#362), reduced to the members the harness reads.
    ///
    /// The lines are inline, not a captured fixture; `mea capture --transcripts` records the real thing. They carry the
    /// `StructuredOutput` tool call and its result, a rate-limit record, then a `result` record
    /// whose `structured_output` is `answer` and whose `credential_expires_at` is
    /// [`CREDENTIAL_EXPIRY_CANARY`].
    ///
    /// ```
    /// use mango_agent_claude::testing::Run;
    ///
    /// let Run::Transcript { lines, .. } = Run::answering_schema(&serde_json::json!({"word": "ok"}))
    /// else {
    ///     panic!("expected a transcript run");
    /// };
    /// assert_eq!(lines.len(), 5);
    /// assert!(lines[4].contains(r#""structured_output":{"word":"ok"}"#), "received: {}", lines[4]);
    /// ```
    pub fn answering_schema(answer: &serde_json::Value) -> Self {
        Self::replaying(&frames([
            serde_json::json!({"type": "system", "subtype": "init",
                "session_id": SCHEMA_TURN_SESSION_ID, "model": "claude-sonnet-5"}),
            serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_answer", "name": "StructuredOutput",
                 "input": answer}]}}),
            serde_json::json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_answer",
                 "content": "Structured output provided successfully"}]}}),
            serde_json::json!({"type": "rate_limit_event", "rate_limit_info": {"status": "allowed"}}),
            serde_json::json!({"type": "result", "subtype": "success", "is_error": false,
                "result": answer.to_string(), "structured_output": answer,
                "credential_expires_at": CREDENTIAL_EXPIRY_CANARY,
                "usage": {"input_tokens": 3, "output_tokens": 5}}),
        ]))
    }

    /// A `--json-schema` turn on Claude Code 2.1.325 with an unsatisfiable schema (`n` with
    /// `minimum: 10`, `maximum: 5`), as it streamed (#382), reduced to the members the harness
    /// reads.
    ///
    /// Two `StructuredOutput` calls come back `is_error: true`, the model gives up in `prose`, and
    /// the `result` is `success` with no `structured_output`. The lines are inline, not a fixture.
    ///
    /// ```
    /// use mango_agent_claude::testing::Run;
    ///
    /// let Run::Transcript { lines, .. } = Run::leaving_schema_unanswered("no answer fits") else {
    ///     panic!("expected a transcript run");
    /// };
    /// assert!(lines.last().is_some_and(|line| line.contains("no answer fits")));
    /// assert!(!lines.iter().any(|line| line.contains("structured_output")));
    /// ```
    pub fn leaving_schema_unanswered(prose: &str) -> Self {
        Self::replaying(&frames([
            serde_json::json!({"type": "system", "subtype": "init",
                "session_id": SCHEMA_TURN_SESSION_ID, "model": "claude-sonnet-5"}),
            serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_first", "name": "StructuredOutput",
                 "input": {"n": 7}}]}}),
            serde_json::json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_first", "is_error": true,
                 "content": "Output does not match required schema: /n: must be <= 5"}]}}),
            serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_second", "name": "StructuredOutput",
                 "input": {"n": 7}}]}}),
            serde_json::json!({"type": "user", "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_second", "is_error": true,
                 "content": "Output does not match required schema: /n: must be >= 10"}]}}),
            serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "text", "text": prose}]}}),
            serde_json::json!({"type": "result", "subtype": "success", "is_error": false,
                "terminal_reason": "completed", "result": prose,
                "usage": {"input_tokens": 3, "output_tokens": 5}}),
        ]))
    }

    /// A child that ends its turn with an empty successful `result` and nothing else.
    ///
    /// For a test that needs a turn to launch and finish but does not read what it said.
    ///
    /// ```
    /// use mango_agent_claude::testing::Run;
    ///
    /// let Run::Transcript { lines, .. } = Run::completing() else {
    ///     panic!("expected a transcript run");
    /// };
    /// assert_eq!(lines, [r#"{"type":"result","is_error":false}"#]);
    /// ```
    pub fn completing() -> Self {
        Self::replaying(r#"{"type":"result","is_error":false}"#)
    }

    /// A child that writes `transcript`'s non-blank lines and exits cleanly.
    pub fn replaying(transcript: &str) -> Self {
        Self::Transcript {
            lines: transcript
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(str::to_owned)
                .collect(),
            exit: ExitStatus {
                code: Some(0),
                signal: None,
            },
        }
    }

    /// A child that prints nothing and exits with this code.
    pub fn exiting(code: i32) -> Self {
        Self::Transcript {
            lines: Vec::new(),
            exit: ExitStatus {
                code: Some(code),
                signal: None,
            },
        }
    }

    /// A child that writes these lines and then keeps running.
    pub fn stalling<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Stalls {
            lines: lines.into_iter().map(Into::into).collect(),
        }
    }
}

/// A scripted `claude`, dispatching on the argv it was spawned with.
///
/// A launch whose argv has `--version` or `--help` anywhere, or `auth status` as its first two
/// arguments, is a probe and is answered from the build's own text. Any other launch is a turn and
/// plays the next [`Run`] queued with [`with_turn`](Self::with_turn). With nothing queued a turn is
/// refused with a launch error, so a test never passes over a launch it did not script; queue
/// [`Run::completing`] for a turn whose output does not matter.
///
/// # Example
///
/// ```
/// use mango_agent_claude::testing::{FakeClaudeCli, Run, SIGNED_OUT};
///
/// let cli = FakeClaudeCli::new()
///     .with_auth(SIGNED_OUT)
///     .with_turn(Run::exiting(1));
/// assert!(cli.launches().is_empty(), "nothing has launched yet");
/// ```
pub struct FakeClaudeCli {
    version: Mutex<String>,
    help: Mutex<String>,
    auth: Mutex<String>,
    turns: Mutex<VecDeque<Run>>,
    launches: Mutex<Vec<LaunchSpec>>,
    /// Launches the script did not cover, kept apart from `launches` so the error can stay argv-free.
    refused: Mutex<Vec<LaunchSpec>>,
    written: Arc<Mutex<Vec<String>>>,
    children: Mutex<Vec<Arc<Child>>>,
    stderr: Mutex<Vec<u8>>,
    /// Held closed while a test wants a turn's spawn to still be in flight.
    spawn_gate: Mutex<Option<SpawnGate>>,
    /// Held closed while a test needs process termination to remain in flight.
    stop_gate: Mutex<Option<SpawnGate>>,
    /// Held closed while a test needs a prompt write to remain in flight.
    input_write_gate: Mutex<Option<SpawnGate>>,
    /// Held closed while a test needs stdin close to remain in flight.
    input_close_gate: Mutex<Option<SpawnGate>>,
    /// Whether each killed child's `--mcp-config` file was still on disk when it was killed.
    config_at_kill: Arc<Mutex<Vec<bool>>>,
    kill_requests: Arc<Mutex<usize>>,
    graceful_interrupts: Arc<Mutex<usize>>,
    interrupt_supported: bool,
    slow_stop: Option<SlowStop>,
    /// How long after a kill that answered without ending the child the child is reaped.
    slow_reap: Option<std::time::Duration>,
    /// Fired each time a child is handed out, for a test waiting on one that is still spawning.
    spawned: Notify,
    /// Holds one permit per turn whose prompt was written and whose stdin was closed.
    input_closed: Arc<Notify>,
    /// Whether a turn child's stdout needs one more pass of the runtime before a line is ready.
    output_needs_a_pass: bool,
}

impl Default for FakeClaudeCli {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeClaudeCli {
    /// A build that answers every probe and plays only the turns a test queued.
    pub fn new() -> Self {
        Self {
            version: Mutex::new(String::from(DEFAULT_VERSION)),
            help: Mutex::new(String::from(DEFAULT_HELP)),
            auth: Mutex::new(String::from(SIGNED_IN)),
            turns: Mutex::new(VecDeque::new()),
            launches: Mutex::new(Vec::new()),
            refused: Mutex::new(Vec::new()),
            written: Arc::new(Mutex::new(Vec::new())),
            children: Mutex::new(Vec::new()),
            stderr: Mutex::new(Vec::new()),
            spawn_gate: Mutex::new(None),
            stop_gate: Mutex::new(None),
            input_write_gate: Mutex::new(None),
            input_close_gate: Mutex::new(None),
            config_at_kill: Arc::new(Mutex::new(Vec::new())),
            kill_requests: Arc::new(Mutex::new(0)),
            graceful_interrupts: Arc::new(Mutex::new(0)),
            interrupt_supported: false,
            slow_stop: None,
            slow_reap: None,
            spawned: Notify::new(),
            input_closed: Arc::new(Notify::new()),
            output_needs_a_pass: false,
        }
    }

    /// Prints this instead of the default `--version` banner.
    #[must_use]
    pub fn with_version(self, banner: &str) -> Self {
        *lock(&self.version) = String::from(banner);
        self
    }

    /// Prints this instead of the default `--help`.
    #[must_use]
    pub fn with_help(self, help: &str) -> Self {
        *lock(&self.help) = String::from(help);
        self
    }

    /// Changes the next `--help` probe, as an installed CLI can change between discovery and open.
    pub fn set_help(&self, help: &str) {
        *lock(&self.help) = String::from(help);
    }

    /// Answers `auth status` with this.
    #[must_use]
    pub fn with_auth(self, status: &str) -> Self {
        *lock(&self.auth) = String::from(status);
        self
    }

    /// Writes this to a turn child's stderr.
    #[must_use]
    pub fn with_stderr(self, stderr: &str) -> Self {
        *lock(&self.stderr) = stderr.as_bytes().to_vec();
        self
    }

    /// Queues what the next turn's child does.
    #[must_use]
    pub fn with_turn(self, run: Run) -> Self {
        lock(&self.turns).push_back(run);
        self
    }

    /// Makes turn children acknowledge a graceful host interrupt before a kill is considered.
    #[must_use]
    pub fn with_graceful_interrupt(mut self) -> Self {
        self.interrupt_supported = true;
        self
    }

    /// Makes turn children answer a stop this slowly, and not exit for the interrupt.
    #[must_use]
    pub fn with_slow_stop(mut self, stop: SlowStop) -> Self {
        self.slow_stop = Some(stop);
        self
    }

    /// Makes a child that a kill left running exit this long after the kill answered.
    ///
    /// For a test that needs a stop to succeed at the very end of its bound: the kill answers
    /// inside `shutdown_timeout`, and the reap that follows lands inside the next one. Only
    /// meaningful with a [`SlowStop`] whose `kill_ends_child` is false.
    #[must_use]
    pub fn with_slow_reap(mut self, reap: std::time::Duration) -> Self {
        self.slow_reap = Some(reap);
        self
    }

    /// Makes a turn child's stdout answer a read one pass of the runtime after a line is there.
    ///
    /// What a real pipe does: the bytes are in the kernel, and the read is ready only once the
    /// runtime's I/O driver has reported them. The in-memory queue this fake reads is ready the
    /// moment a line is pushed, which hides that gap unless a test asks for it.
    #[must_use]
    pub fn with_output_ready_a_pass_late(mut self) -> Self {
        self.output_needs_a_pass = true;
        self
    }

    /// Returns once a turn's prompt was written and its stdin closed.
    ///
    /// The pump goes from that close into its read in the same poll, so a test that resumes
    /// after this finds the pump reading. One permit per turn, so the order of arrival is free.
    pub async fn turn_reading(&self) {
        self.input_closed.notified().await;
    }

    /// Every launch, in order.
    pub fn launches(&self) -> Vec<LaunchSpec> {
        lock(&self.launches).clone()
    }

    /// Every launch the script did not cover, in order, with the argv it received.
    ///
    /// A refusal is an [`Error::Launch`], which never carries an argv because a host's argv can
    /// hold secrets; its message gives the reason and the argument count, and this is where the
    /// argv itself is read. A refused launch is not in [`launches`](Self::launches).
    pub fn refused_launches(&self) -> Vec<LaunchSpec> {
        lock(&self.refused).clone()
    }

    /// Every refused launch as a redacted shape, in order: program, argument count, the flag
    /// names it carried, and how many other arguments were left out.
    ///
    /// The shape is for a failure message a test author can read without the argv: a value
    /// after a flag, a `--flag=value` tail and a positional prompt are all counted, never
    /// printed, so it is safe to put in a panic or a log. [`refused_launches`](Self::refused_launches)
    /// still holds the full argv for a test that needs to assert on it.
    pub fn refused_launch_shapes(&self) -> Vec<String> {
        lock(&self.refused).iter().map(refused_shape).collect()
    }

    /// Every turn's argv, in order — the launches that are not one of the three probes.
    pub fn turn_argvs(&self) -> Vec<Vec<String>> {
        self.launches()
            .into_iter()
            .map(|launch| launch.argv)
            .filter(|argv| !is_probe(argv))
            .collect()
    }

    /// Every line the library wrote to a child's stdin, across all of them.
    pub fn written(&self) -> Vec<String> {
        lock(&self.written).clone()
    }

    /// Holds every turn spawn until the returned handle releases it.
    ///
    /// The window between "this session is still open" and "this turn is the active one" is only
    /// reachable while a spawn is in flight, and a fake that returns instantly never opens it.
    #[must_use]
    pub fn gate_turn_spawns(&self) -> SpawnGate {
        let gate = SpawnGate {
            arrived: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        *lock(&self.spawn_gate) = Some(gate.clone());
        gate
    }

    /// Holds the next turn's forced termination until the returned gate releases it.
    #[must_use]
    pub fn gate_turn_stops(&self) -> SpawnGate {
        let gate = SpawnGate {
            arrived: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        *lock(&self.stop_gate) = Some(gate.clone());
        gate
    }

    /// Holds the next turn's prompt write until the returned gate releases it.
    #[must_use]
    pub fn gate_turn_input_writes(&self) -> SpawnGate {
        let gate = SpawnGate {
            arrived: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        *lock(&self.input_write_gate) = Some(gate.clone());
        gate
    }

    /// Holds the next turn's stdin close until the returned gate releases it.
    #[must_use]
    pub fn gate_turn_input_closes(&self) -> SpawnGate {
        let gate = SpawnGate {
            arrived: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        *lock(&self.input_close_gate) = Some(gate.clone());
        gate
    }

    /// For each child that was killed, whether its `--mcp-config` file still existed then.
    ///
    /// A child is launched with `--mcp-config <path>` and reads it at startup, so unlinking that
    /// file before the kill lands is a child that can observe a missing configuration. Recorded at
    /// the moment of the kill because the ordering is the whole claim.
    pub fn mcp_config_at_kill(&self) -> Vec<bool> {
        lock(&self.config_at_kill).clone()
    }

    /// How many times the library asked a turn child to end, including asks a child had already
    /// ended for. Turn children only: they are the ones a `--mcp-config` names.
    ///
    /// Deliberately different from [`mcp_config_at_kill`](Self::mcp_config_at_kill), which records
    /// one entry per kill that had something to do: `ProcessControl::kill` documents that "the
    /// library asks once", and a second ask is a contract break even where the launcher happens to
    /// absorb it. Counting the asks is the only way a fake can see that, because absorbing them is
    /// exactly what the real `TokioChild` does.
    pub fn kill_requests(&self) -> usize {
        *lock(&self.kill_requests)
    }

    /// How many turn children the harness asked to stop gracefully.
    pub fn graceful_interrupts(&self) -> usize {
        *lock(&self.graceful_interrupts)
    }

    /// Whether every child this launcher handed out has ended.
    pub fn all_children_ended(&self) -> bool {
        lock(&self.children)
            .iter()
            .all(|child| child.is_finished() && !child.output_is_open())
    }

    /// Whether any child is still running.
    pub fn a_child_is_running(&self) -> bool {
        !self.all_children_ended()
    }

    /// Ends live turn children as the host after a test has proved the library stopped asking.
    ///
    /// A failed bounded stop means the host still owns an unfinished process. Tests use this only
    /// to release fixture state after asserting that the harness made its one allowed stop ask.
    pub fn end_turns_for_host_cleanup(&self) {
        let children = lock(&self.children).clone();
        for child in children
            .into_iter()
            .filter(|child| child.is_turn && (!child.is_finished() || child.output_is_open()))
        {
            child.end(ExitStatus {
                code: Some(143),
                signal: None,
            });
        }
    }

    /// Makes the newest running turn child exit while something it started keeps its stdout open.
    ///
    /// A pipe ends when its last writer closes it, not when the process the library spawned
    /// exits: a helper that inherited the descriptor holds the read open after `wait` has already
    /// answered. Killing the tree is what closes it.
    pub fn exit_turn_holding_its_output(&self, code: i32) {
        let Some(child) = self.newest_turn(|child| !child.is_finished()) else {
            return;
        };
        *lock(&child.exit) = ExitStatus {
            code: Some(code),
            signal: None,
        };
        *lock(&child.finished) = true;
        child.changed.notify_waiters();
    }

    /// Closes the newest running turn child's stdout while the child itself keeps running.
    pub fn close_turn_output(&self) {
        let Some(child) = self.newest_turn(|child| !child.is_finished()) else {
            return;
        };
        *lock(&child.output_open) = false;
        child.changed.notify_waiters();
    }

    /// Returns once the newest turn child is gone: exited, and its stdout closed.
    ///
    /// Waits on the child's own notification rather than polling, so a test under a paused clock
    /// can read off how long the reap took.
    pub async fn turn_reaped(&self) {
        // A turn whose spawn is still in flight has no child yet; wait for the launcher to hand
        // one out before waiting on it.
        let child = loop {
            let spawned = self.spawned.notified();
            if let Some(child) = self.newest_turn(|_| true) {
                break child;
            }
            spawned.await;
        };
        loop {
            let changed = child.changed.notified();
            if child.is_finished() && !child.output_is_open() {
                return;
            }
            changed.await;
        }
    }

    fn newest_turn(&self, wanted: impl Fn(&Child) -> bool) -> Option<Arc<Child>> {
        lock(&self.children)
            .iter()
            .rev()
            .find(|child| child.is_turn && wanted(child))
            .map(Arc::clone)
    }

    /// Makes the newest live turn child print one more line after it has gone quiet.
    ///
    /// A [`Run`] says everything it is going to say before a test can arrange anything around it,
    /// which leaves one ordering out of reach: the vendor's own `result` arriving *after* a stop
    /// already owns the turn. That window is where the harness has to decide whether it still owes
    /// the launcher a teardown ask, so a fake that cannot reach it cannot hold that decision.
    pub fn announce_to_turn(&self, line: impl Into<String>) {
        let child = lock(&self.children)
            .iter()
            .rev()
            .find(|child| child.is_turn && !child.is_finished())
            .map(Arc::clone);
        let Some(child) = child else {
            return;
        };
        lock(&child.pending).push_back(line.into());
        child.changed.notify_waiters();
    }

    /// Writes one line to the newest turn child's stdout from whatever still holds it open,
    /// which after [`exit_turn_holding_its_output`](Self::exit_turn_holding_its_output) is the
    /// helper the child left behind.
    pub fn write_to_held_output(&self, line: impl Into<String>) {
        let Some(child) = self.newest_turn(Child::output_is_open) else {
            return;
        };
        lock(&child.pending).push_back(line.into());
        child.changed.notify_waiters();
    }

    /// Records `spec` as refused and builds the launch error for it, with no argv in the message.
    fn refuse(&self, spec: &LaunchSpec, unscripted: &str) -> Error {
        lock(&self.refused).push(spec.clone());
        Error::Launch {
            program: spec.argv.first().cloned().unwrap_or_default(),
            message: format!(
                "unscripted launch, expected --version, --help, auth status or a queued turn | received {unscripted} ({} arguments); read FakeClaudeCli::refused_launch_shapes for the flags, or refused_launches for the argv",
                spec.argv.len()
            ),
        }
    }

    fn run_for(&self, spec: &LaunchSpec) -> Result<(Run, Vec<u8>)> {
        let argv = &spec.argv;
        if argv.iter().any(|argument| argument == "--version") {
            return Ok((Run::replaying(&lock(&self.version)), Vec::new()));
        }
        if argv.iter().any(|argument| argument == "--help") {
            return Ok((Run::replaying(&lock(&self.help)), Vec::new()));
        }
        if is_auth_status(argv) {
            return Ok((Run::replaying(&lock(&self.auth)), Vec::new()));
        }
        // The library never logs in, so an `auth` subcommand other than `status` is not something
        // a script should answer: it would be a turn that quietly succeeded.
        if argv.get(1).is_some_and(|word| word == "auth") {
            return Err(self.refuse(
                spec,
                "an auth subcommand other than `status`, which this fake never answers",
            ));
        }
        let queued = lock(&self.turns).pop_front();
        let run = match queued {
            Some(run) => run,
            None => {
                return Err(self.refuse(spec, "a turn with no Run queued through with_turn"));
            }
        };
        Ok((run, lock(&self.stderr).clone()))
    }
}

/// The argument that follows `flag` in `argv`, if the flag is there and has one.
///
/// ```
/// use mango_agent_claude::testing::value_after;
///
/// let argv: Vec<String> = ["claude", "--json-schema", "{}", "--print"].map(String::from).into();
/// assert_eq!(value_after(&argv, "--json-schema"), Some("{}"));
/// assert_eq!(value_after(&argv, "--print"), None);
/// assert_eq!(value_after(&argv, "--effort"), None);
/// ```
pub fn value_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    let at = argv.iter().position(|argument| argument == flag)?;
    argv.get(at + 1).map(String::as_str)
}

/// Whether `argv` is one of the three probes (`--version`, `--help`, `auth status`) rather than a
/// turn.
///
/// ```
/// use mango_agent_claude::testing::is_probe;
///
/// let probe: Vec<String> = ["claude", "auth", "status"].map(String::from).into();
/// let turn: Vec<String> = ["claude", "--print"].map(String::from).into();
/// assert!(is_probe(&probe));
/// assert!(!is_probe(&turn));
/// ```
pub fn is_probe(argv: &[String]) -> bool {
    argv.iter()
        .any(|argument| argument == "--version" || argument == "--help")
        || is_auth_status(argv)
}

/// Reads a refused launch as a shape: the program, the argument count, the flag names and the
/// number of arguments left out, so a failure can say what was launched without printing a value.
///
/// A flag is an argument after the program that starts with `-`; anything after an `=` in it is
/// dropped. Every other argument is a flag's value, a subcommand or a prompt, and is only counted.
fn refused_shape(spec: &LaunchSpec) -> String {
    let program = spec.argv.first().map_or("", String::as_str);
    let flags: Vec<&str> = spec
        .argv
        .iter()
        .skip(1)
        .filter_map(|argument| {
            let name = argument
                .split_once('=')
                .map_or(argument.as_str(), |(name, _)| name);
            is_flag_name(name).then_some(name)
        })
        .collect();
    let redacted = spec.argv.len().saturating_sub(1 + flags.len());
    let flag_names = if flags.is_empty() {
        "no flags".to_owned()
    } else {
        format!("flags {}", flags.join(", "))
    };
    format!(
        "{program} with {} arguments: {flag_names}; {redacted} values redacted",
        spec.argv.len()
    )
}

/// Whether `word` is shaped like a flag name: one or two dashes, a letter, then letters, digits
/// or dashes. A value that merely starts with a dash, such as a token or a prompt, fails this
/// and is counted instead of printed.
fn is_flag_name(word: &str) -> bool {
    let Some(rest) = word.strip_prefix("--").or_else(|| word.strip_prefix('-')) else {
        return false;
    };
    rest.starts_with(|first: char| first.is_ascii_alphabetic())
        && rest
            .chars()
            .all(|next| next.is_ascii_alphanumeric() || next == '-')
}

fn is_auth_status(argv: &[String]) -> bool {
    argv.len() >= 3 && argv[1] == "auth" && argv[2] == "status"
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait::async_trait]
impl ProcessLauncher for FakeClaudeCli {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let (run, stderr_bytes) = self.run_for(&spec)?;
        let is_turn = !is_probe(&spec.argv);
        let gate = is_turn.then(|| lock(&self.spawn_gate).clone()).flatten();
        if let Some(gate) = gate {
            gate.arrived.notify_one();
            gate.release.notified().await;
        }
        let mcp_config = value_after(&spec.argv, "--mcp-config").map(PathBuf::from);
        let stop_gate = is_turn.then(|| lock(&self.stop_gate).clone()).flatten();
        let input_write_gate = is_turn
            .then(|| lock(&self.input_write_gate).clone())
            .flatten();
        let input_close_gate = is_turn
            .then(|| lock(&self.input_close_gate).clone())
            .flatten();
        lock(&self.launches).push(spec);

        let stderr = StderrTail::default();
        stderr.push(&stderr_bytes);
        let (lines, exit, finished) = match run {
            Run::Transcript { lines, exit } => (lines, exit, true),
            Run::Stalls { lines } => (
                lines,
                ExitStatus {
                    code: Some(0),
                    signal: None,
                },
                false,
            ),
        };
        let child = Arc::new(Child {
            pending: Mutex::new(lines.into()),
            finished: Mutex::new(finished),
            output_open: Mutex::new(!finished),
            output_needs_a_pass: is_turn && self.output_needs_a_pass,
            slow_stop: is_turn.then_some(self.slow_stop).flatten(),
            slow_reap: is_turn.then_some(self.slow_reap).flatten(),
            reap_at: Mutex::new(None),
            exit: Mutex::new(exit),
            stderr,
            written: Arc::clone(&self.written),
            changed: Notify::new(),
            mcp_config,
            config_at_kill: Arc::clone(&self.config_at_kill),
            kill_requests: Arc::clone(&self.kill_requests),
            graceful_interrupts: Arc::clone(&self.graceful_interrupts),
            interrupt_supported: self.interrupt_supported,
            is_turn,
            stop_gate,
        });
        lock(&self.children).push(Arc::clone(&child));
        self.spawned.notify_waiters();

        Ok(ManagedProcess {
            stdout: Box::new(FakeStdout {
                child: Arc::clone(&child),
            }),
            stdin: Some(Box::new(FakeStdin {
                child: Arc::clone(&child),
                partial: Vec::new(),
                input_write_gate,
                input_close_gate,
                closed: is_turn.then(|| Arc::clone(&self.input_closed)),
            })),
            control: child,
        })
    }
}

struct Child {
    pending: Mutex<VecDeque<String>>,
    finished: Mutex<bool>,
    /// Whether anything still holds the write end of stdout. Separate from `finished` because a
    /// pipe outlives the process that was handed it for as long as a helper keeps it open.
    output_open: Mutex<bool>,
    output_needs_a_pass: bool,
    slow_stop: Option<SlowStop>,
    slow_reap: Option<std::time::Duration>,
    /// When a kill that left the child running promised its exit.
    reap_at: Mutex<Option<tokio::time::Instant>>,
    exit: Mutex<ExitStatus>,
    stderr: StderrTail,
    written: Arc<Mutex<Vec<String>>>,
    changed: Notify,
    mcp_config: Option<PathBuf>,
    config_at_kill: Arc<Mutex<Vec<bool>>>,
    kill_requests: Arc<Mutex<usize>>,
    graceful_interrupts: Arc<Mutex<usize>>,
    interrupt_supported: bool,
    is_turn: bool,
    stop_gate: Option<SpawnGate>,
}

impl Child {
    fn is_finished(&self) -> bool {
        *lock(&self.finished)
    }

    fn output_is_open(&self) -> bool {
        *lock(&self.output_open)
    }

    /// Ends the whole tree: the child exits and nothing is left holding its stdout. A child that
    /// already exited keeps the status it exited with.
    fn end(&self, exit: ExitStatus) {
        if !self.is_finished() {
            *lock(&self.exit) = exit;
        }
        *lock(&self.finished) = true;
        *lock(&self.output_open) = false;
        self.changed.notify_waiters();
    }
}

#[async_trait::async_trait]
impl ProcessControl for Child {
    fn pid(&self) -> Option<u32> {
        None
    }

    fn stderr_tail(&self) -> String {
        self.stderr.read()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        loop {
            let changed = self.changed.notified();
            if self.is_finished() {
                return Ok(*lock(&self.exit));
            }
            let reap_at = *lock(&self.reap_at);
            if let Some(at) = reap_at {
                tokio::time::sleep_until(at).await;
                self.end(ExitStatus {
                    code: Some(143),
                    signal: None,
                });
                continue;
            }
            changed.await;
        }
    }

    async fn interrupt(&self, _reason: CancelReason) -> Result<InterruptOutcome> {
        if let Some(stop) = self.slow_stop {
            let Some(delay) = stop.interrupt else {
                return Ok(InterruptOutcome::Unsupported);
            };
            tokio::time::sleep(delay).await;
            *lock(&self.graceful_interrupts) += 1;
            return Ok(InterruptOutcome::Delivered);
        }
        if !self.interrupt_supported {
            return Ok(InterruptOutcome::Unsupported);
        }
        if self.is_turn {
            *lock(&self.graceful_interrupts) += 1;
        }
        if !self.is_finished() {
            self.end(ExitStatus {
                code: Some(130),
                signal: None,
            });
        }
        Ok(InterruptOutcome::Delivered)
    }

    /// 128 + SIGTERM, which is what the vendor documents for a `claude -p` run stopped that way.
    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        // Counted before the early return: the ask itself is what the trait bounds. Version and
        // help probes are ended by their own code paths, so only turn children contribute.
        if self.is_turn {
            *lock(&self.kill_requests) += 1;
        }
        if let Some(gate) = &self.stop_gate {
            gate.arrived.notify_one();
            gate.release.notified().await;
        }
        // A child that already ended is not killed again. `TokioChild` escalates once and the
        // second caller waits on the first caller's outcome without signalling anything, so a fake
        // that recorded a second observation here would be inventing a kill the real launcher
        // never performs — and the turn's own stream task does call `kill` after a close already
        // did.
        if self.is_finished() && !self.output_is_open() {
            return Ok(());
        }
        if let Some(stop) = self.slow_stop {
            tokio::time::sleep(stop.kill).await;
            if !stop.kill_ends_child {
                if let Some(reap) = self.slow_reap {
                    *lock(&self.reap_at) = Some(tokio::time::Instant::now() + reap);
                    self.changed.notify_waiters();
                }
                return Ok(());
            }
        }
        if let Some(path) = &self.mcp_config {
            lock(&self.config_at_kill).push(path.exists());
        }
        self.end(ExitStatus {
            code: Some(143),
            signal: None,
        });
        Ok(())
    }
}

struct FakeStdout {
    child: Arc<Child>,
}

#[async_trait::async_trait]
impl ByteSource for FakeStdout {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let changed = self.child.changed.notified();
            if self.child.output_needs_a_pass && !lock(&self.child.pending).is_empty() {
                tokio::task::yield_now().await;
            }
            if let Some(line) = lock(&self.child.pending).pop_front() {
                return Ok(Some(format!("{line}\n").into_bytes()));
            }
            if !self.child.output_is_open() {
                return Ok(None);
            }
            changed.await;
        }
    }
}

struct FakeStdin {
    child: Arc<Child>,
    partial: Vec<u8>,
    input_write_gate: Option<SpawnGate>,
    input_close_gate: Option<SpawnGate>,
    /// Told when a turn's stdin is closed.
    closed: Option<Arc<Notify>>,
}

#[async_trait::async_trait]
impl ByteSink for FakeStdin {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(gate) = &self.input_write_gate {
            gate.arrived.notify_one();
            gate.release.notified().await;
        }
        self.partial.extend_from_slice(bytes);
        while let Some(newline) = self.partial.iter().position(|byte| *byte == b'\n') {
            let record: Vec<u8> = self.partial.drain(..=newline).collect();
            lock(&self.child.written).push(String::from_utf8_lossy(&record).trim_end().to_owned());
        }
        Ok(())
    }

    /// Closing input does **not** end the child.
    ///
    /// `claude --print` reads stdin to EOF and only then starts working, so a fake that ended here
    /// could never model a turn that is still running when a host cancels it.
    async fn close(&mut self) -> Result<()> {
        if let Some(gate) = &self.input_close_gate {
            gate.arrived.notify_one();
            gate.release.notified().await;
        }
        if let Some(closed) = &self.closed {
            closed.notify_one();
        }
        Ok(())
    }
}
