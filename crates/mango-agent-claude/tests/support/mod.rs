// Shared by every integration test in this crate, and no one test uses all of it: a helper only
// `conformance.rs` needs is dead code from `harness_over_a_fake_cli.rs`'s point of view, because
// cargo compiles each test target separately.
#![allow(dead_code)]

//! A fake `claude` binary: answers the three probes from its argv, and every turn from a script.
//!
//! Core's own [`FakeLauncher`](mango_external_agents::testing::FakeLauncher) is first-in-first-out
//! and ties a child's end of output to its stdin being closed. Neither fits this dialect. A turn
//! here spawns four children in a fixed order only the argv distinguishes, and `claude --print`
//! reads stdin to EOF **before** it starts working — so a fake whose stdout ended when stdin closed
//! could never model a turn that is still running when a host cancels it.
//!
//! Nothing here spawns a process. A test asserts on what the library made of a scripted vendor,
//! which is the only way to test a dialect on a machine without that vendor's CLI — every machine,
//! in CI.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use mango_external_agents::testing::FrozenClock;
use mango_external_agents::{
    ByteSink, ByteSource, CancelReason, EnvSource, ExitStatus, HostContext, InterruptOutcome,
    LaunchSpec, Limits, ManagedProcess, ProcessControl, ProcessLauncher, Result, StderrTail,
};
use tokio::sync::Notify;

/// The `--help` a fake build prints unless a test says otherwise.
pub const DEFAULT_HELP: &str = include_str!("../../../../fixtures/claude/help/2.1.260.txt");

/// The `--version` banner a fake build prints unless a test says otherwise.
pub const DEFAULT_VERSION: &str = "2.1.270 (Claude Code)";

/// A signed-in `auth status`, as the committed contract capture shapes it.
pub const SIGNED_IN: &str = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","subscriptionType":"max"}"#;

/// A signed-out `auth status`.
pub const SIGNED_OUT: &str = r#"{"loggedIn":false}"#;

/// A captured `read a file` turn, replayed byte for byte across every test that needs one.
pub const READ_TURN: &str = include_str!("../../../../fixtures/claude/transcripts/read-turn.jsonl");

/// An excerpt from before `--effort` and `--permission-prompts` existed, which is what proves an
/// older build keeps working with those features off.
pub const HELP_2_1_227: &str = include_str!("../../../../fixtures/claude/help/2.1.227.txt");

/// The whole surface of a real build, which is where `--mcp-config` is actually declared.
pub const HELP_2_1_270: &str = include_str!("../../../../fixtures/claude/help/2.1.270.txt");

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

impl Run {
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
pub struct FakeClaudeCli {
    version: Mutex<String>,
    help: Mutex<String>,
    auth: Mutex<String>,
    turns: Mutex<VecDeque<Run>>,
    launches: Mutex<Vec<LaunchSpec>>,
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
}

impl Default for FakeClaudeCli {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeClaudeCli {
    /// A build that answers every probe and completes every turn without saying anything.
    pub fn new() -> Self {
        Self {
            version: Mutex::new(String::from(DEFAULT_VERSION)),
            help: Mutex::new(String::from(DEFAULT_HELP)),
            auth: Mutex::new(String::from(SIGNED_IN)),
            turns: Mutex::new(VecDeque::new()),
            launches: Mutex::new(Vec::new()),
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

    /// Every launch, in order.
    pub fn launches(&self) -> Vec<LaunchSpec> {
        lock(&self.launches).clone()
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
        lock(&self.children).iter().all(|child| child.is_finished())
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
            .filter(|child| child.is_turn && !child.is_finished())
        {
            child.end(ExitStatus {
                code: Some(143),
                signal: None,
            });
        }
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

    fn run_for(&self, argv: &[String]) -> (Run, Vec<u8>) {
        if argv.iter().any(|argument| argument == "--version") {
            return (Run::replaying(&lock(&self.version)), Vec::new());
        }
        if argv.iter().any(|argument| argument == "--help") {
            return (Run::replaying(&lock(&self.help)), Vec::new());
        }
        if is_auth_status(argv) {
            return (Run::replaying(&lock(&self.auth)), Vec::new());
        }
        let queued = lock(&self.turns)
            .pop_front()
            .unwrap_or_else(|| Run::replaying(r#"{"type":"result","is_error":false}"#));
        (queued, lock(&self.stderr).clone())
    }
}

pub(crate) fn value_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    let at = argv.iter().position(|argument| argument == flag)?;
    argv.get(at + 1).map(String::as_str)
}

fn is_probe(argv: &[String]) -> bool {
    argv.iter()
        .any(|argument| argument == "--version" || argument == "--help")
        || is_auth_status(argv)
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
        let (run, stderr_bytes) = self.run_for(&spec.argv);
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

        Ok(ManagedProcess {
            stdout: Box::new(FakeStdout {
                child: Arc::clone(&child),
            }),
            stdin: Some(Box::new(FakeStdin {
                child: Arc::clone(&child),
                partial: Vec::new(),
                input_write_gate,
                input_close_gate,
            })),
            control: child,
        })
    }
}

struct Child {
    pending: Mutex<VecDeque<String>>,
    finished: Mutex<bool>,
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

    fn end(&self, exit: ExitStatus) {
        *lock(&self.exit) = exit;
        *lock(&self.finished) = true;
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
            changed.await;
        }
    }

    async fn interrupt(&self, _reason: CancelReason) -> Result<InterruptOutcome> {
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
        if self.is_finished() {
            return Ok(());
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
            if let Some(line) = lock(&self.child.pending).pop_front() {
                return Ok(Some(format!("{line}\n").into_bytes()));
            }
            if self.child.is_finished() {
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
        Ok(())
    }
}

/// A host whose launcher is this fake, with a frozen clock and the ordinary caps.
pub fn host(launcher: Arc<FakeClaudeCli>) -> HostContext {
    host_under(launcher, Limits::default())
}

/// The same host, reading a vendor under caps this narrow.
///
/// What a host sets here is its own policy, so a test that wants a cap reached says so rather than
/// making a fake write a megabyte to reach the default one.
///
/// Takes any launcher, not just [`FakeClaudeCli`]: a test that wants to prove what happens when the
/// launcher itself cannot spawn builds its own and still gets this host's environment and clock.
pub fn host_under(launcher: Arc<dyn ProcessLauncher>, limits: Limits) -> HostContext {
    HostContext::builder()
        .launcher(launcher)
        .cwd(std::env::temp_dir())
        .scratch(std::env::temp_dir())
        .environment(EnvSource::from_pairs([
            ("PATH", "/usr/bin"),
            ("CLAUDE_CONFIG_DIR", "/home/ada/.claude"),
            ("MANGO_HUB_TOKEN", "never-forward-this"),
        ]))
        .client_info("mea-tests", "0.0.0")
        .clock(Arc::new(FrozenClock::default()))
        .limits(limits)
        .build()
        .expect("expected a host")
}
