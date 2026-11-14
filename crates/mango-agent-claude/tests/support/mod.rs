// Shared by every integration test in this crate, and no one test uses all of it: a helper only
// `conformance.rs` needs is dead code from `harness_over_a_fake_cli.rs`'s point of view, because
// cargo compiles each test target separately.
#![allow(dead_code, unused_imports)]

//! What the tests around the fake `claude` need and a host does not: the host the fake is plugged
//! into, private scratch roots, and the launcher wrappers that make a host's own control refuse.
//!
//! The fake itself, [`FakeClaudeCli`], is the crate's `testing` module, re-exported here so a test
//! names it from one place. Nothing here spawns a process.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use mango_agent_claude::protocol::StreamRecord;
use mango_agent_claude::testing::is_probe;
pub use mango_agent_claude::testing::{
    DEFAULT_HELP, DEFAULT_VERSION, FakeClaudeCli, Run, SIGNED_IN, SIGNED_OUT, SlowStop, SpawnGate,
    value_after,
};
use mango_external_agents::testing::FrozenClock;
use mango_external_agents::{
    CancelReason, EnvSource, Error, ExitStatus, HostContext, InterruptOutcome, LaunchSpec, Limits,
    ManagedProcess, ProcessControl, ProcessLauncher, Result,
};

/// A captured `read a file` turn, replayed byte for byte across every test that needs one.
pub const READ_TURN: &str = include_str!("../../fixtures/claude/transcripts/read-turn.jsonl");

/// The stream-json lines of a recording, with its blank and `#` header lines skipped.
///
/// A recording written by `mea capture --harness claude --transcripts` opens with `# claude/...`
/// and `# capture {json}` lines that are metadata, not child output. Any other line must be one
/// stream-json object record; one that is not is a damaged fixture, so it is reported with its
/// 1-based line number in the file (header and blank lines count) instead of being dropped.
///
/// This is the fixture reader. The harness stays lenient about vendor noise on a live stdout,
/// which is why this check lives here and not in [`StreamRecord::parse`].
pub fn recorded_lines(transcript: &str) -> Result<Vec<&str>, String> {
    let mut lines = Vec::new();
    for (index, line) in transcript.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if StreamRecord::parse(trimmed).is_none() {
            return Err(format!(
                "(recording line {}): expected a blank line, a `#` header line or one stream-json \
                 object record | received: {trimmed}",
                index + 1
            ));
        }
        lines.push(line);
    }
    Ok(lines)
}

/// [`recorded_lines`], each parsed as the record the reducer reads. A fixture that does not parse
/// fails the test that loaded it.
pub fn recorded_records(transcript: &str) -> Vec<StreamRecord> {
    recorded_lines(transcript)
        .unwrap_or_else(|message| panic!("{message}"))
        .into_iter()
        .filter_map(StreamRecord::parse)
        .collect()
}

/// An excerpt from before `--effort` and `--permission-prompts` existed, which is what proves an
/// older build keeps working with those features off.
pub const HELP_2_1_227: &str = include_str!("../../fixtures/claude/help/2.1.227.txt");

/// The whole surface of a real build, which is where `--mcp-config` is actually declared.
pub const HELP_2_1_270: &str = mango_agent_claude::testing::HELP_WITH_JSON_SCHEMA;

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A scratch root of this test's own under the system temporary directory.
///
/// Owner-only whatever the process umask is. `std::fs::create_dir` under `umask 002`, the default
/// for a user on Debian and Ubuntu, makes a group-writable directory, and the harness refuses a
/// scratch root another user could replace: the tests that wanted a usable root then failed on the
/// refusal instead of on what they assert. The name carries the process id and a per-process
/// counter, so two tests, and two test binaries, never share one.
///
/// The caller removes it; a test that leaves one behind leaves an empty private directory.
pub fn private_scratch(label: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    private_scratch_with_counter(&std::env::temp_dir(), label, &NEXT)
}

/// Allocates a scratch root with a caller-owned counter so collisions can be reproduced.
pub(crate) fn private_scratch_with_counter(
    parent: &std::path::Path,
    label: &str,
    next: &std::sync::atomic::AtomicU64,
) -> PathBuf {
    loop {
        let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = parent.join(format!("mea-{label}-{}-{index}", std::process::id()));
        match try_private_directory(&path) {
            Ok(()) => return path,
            // A failed earlier run can leave this name behind before its PID is reused.
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => panic!(
                "expected to create a fresh private scratch directory {} | received: {error}",
                path.display()
            ),
        }
    }
}

/// Creates `path` as an owner-only directory whatever the process umask is.
///
/// For a test that needs a directory with a name of its own choosing inside a
/// [`private_scratch`] root, and needs the harness to accept it as a scratch root.
pub fn private_directory(path: &std::path::Path) {
    if let Err(error) = try_private_directory(path) {
        panic!(
            "expected to create the private directory {} | received: {error}",
            path.display()
        );
    }
}

/// Atomically creates an owner-only directory, leaving collision handling to its caller.
fn try_private_directory(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(not(unix))]
    let builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // mkdir applies the umask even to 0700. Restore owner access only after
        // creating a fresh directory, so a collision never changes an existing path.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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

/// What the launcher says when it fails a launch over a child it could not end.
pub const REFUSED_LAUNCH: &str = "the host's launcher could not end the child it failed to use";

/// What the host's control says when it refuses the kill: the cause a host reads back.
pub const REFUSED_KILL: &str = "the host's process control refused the kill";

/// A launcher that remembers the control it hands the harness for each turn child, so a test can
/// tell the host's own control from any other.
///
/// Wraps a [`FakeClaudeCli`] and passes every spawn through. With [`refusing_kill`]
/// (Self::refusing_kill) the control it hands out answers a kill with an error of the host's own,
/// instead of the stalling the fake's `SlowStop` models.
pub struct ControlTap {
    inner: Arc<FakeClaudeCli>,
    kill: KillMode,
    refused: Arc<std::sync::atomic::AtomicUsize>,
    controls: Mutex<Vec<Arc<dyn ProcessControl>>>,
    foreign: Mutex<Option<Arc<dyn ProcessControl>>>,
    fails_launch: bool,
}

impl ControlTap {
    /// Passes every spawn through to `inner` and records each turn child's control.
    pub fn new(inner: Arc<FakeClaudeCli>) -> Self {
        Self {
            inner,
            kill: KillMode::Passed,
            refused: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            controls: Mutex::new(Vec::new()),
            foreign: Mutex::new(None),
            fails_launch: false,
        }
    }

    /// The same, with a launcher that creates the turn child and then fails the launch with a
    /// `CleanupRequired` carrying that child's control, as `stdio::open` does when it cannot end
    /// a child it could not use.
    pub fn failing_launch_over_a_child(inner: Arc<FakeClaudeCli>) -> Self {
        Self {
            fails_launch: true,
            ..Self::new(inner)
        }
    }

    /// The same, with a control whose kill fails with [`REFUSED_KILL`].
    pub fn refusing_kill(inner: Arc<FakeClaudeCli>) -> Self {
        Self {
            kill: KillMode::Refused,
            ..Self::new(inner)
        }
    }

    /// The same, with a control whose kill panics, as a faulty host control can.
    pub fn panicking_kill(inner: Arc<FakeClaudeCli>) -> Self {
        Self {
            kill: KillMode::Panics,
            ..Self::new(inner)
        }
    }

    /// The same, with a control whose kill fails with a `CleanupRequired` of its own, naming
    /// another control than the one the launcher handed out ([`foreign_control`](Self::foreign_control)).
    pub fn answering_with_its_own_cleanup(inner: Arc<FakeClaudeCli>) -> Self {
        Self {
            kill: KillMode::OwnCleanup,
            ..Self::new(inner)
        }
    }

    /// The control named inside the `CleanupRequired` that [`answering_with_its_own_cleanup`]
    /// (Self::answering_with_its_own_cleanup) returns, once a turn child was launched.
    pub fn foreign_control(&self) -> Option<Arc<dyn ProcessControl>> {
        lock(&self.foreign).as_ref().map(Arc::clone)
    }

    /// How many kills the refusing control was asked for.
    pub fn refused_kills(&self) -> usize {
        self.refused.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The control of the newest turn child, exactly as the harness was handed it.
    pub fn newest_turn_control(&self) -> Option<Arc<dyn ProcessControl>> {
        lock(&self.controls).last().map(Arc::clone)
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for ControlTap {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let is_turn = !is_probe(&spec.argv);
        let mut child = self.inner.spawn(spec).await?;
        if is_turn && self.fails_launch {
            lock(&self.controls).push(Arc::clone(&child.control));
            return Err(Error::CleanupRequired {
                control: child.control,
                source: Box::new(Error::Closed {
                    subject: REFUSED_LAUNCH,
                }),
            });
        }
        if is_turn && self.kill != KillMode::Passed {
            let foreign = (self.kill == KillMode::OwnCleanup).then(|| {
                let foreign: Arc<dyn ProcessControl> = Arc::new(HostControl {
                    inner: Arc::clone(&child.control),
                    kill: KillMode::Refused,
                    asked: Arc::clone(&self.refused),
                    foreign: None,
                });
                *lock(&self.foreign) = Some(Arc::clone(&foreign));
                foreign
            });
            child.control = Arc::new(HostControl {
                inner: child.control,
                kill: self.kill,
                asked: Arc::clone(&self.refused),
                foreign,
            });
        }
        if is_turn {
            lock(&self.controls).push(Arc::clone(&child.control));
        }
        Ok(child)
    }
}

/// What a [`ControlTap`]'s control does when it is asked to kill.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KillMode {
    /// Whatever the fake child does.
    Passed,
    /// Fails with an error of the host's own.
    Refused,
    /// Panics.
    Panics,
    /// Fails with a `CleanupRequired` that names another control.
    OwnCleanup,
}

/// A host control whose kill fails or panics, in the way a host's own would.
struct HostControl {
    inner: Arc<dyn ProcessControl>,
    kill: KillMode,
    asked: Arc<std::sync::atomic::AtomicUsize>,
    foreign: Option<Arc<dyn ProcessControl>>,
}

#[async_trait::async_trait]
impl ProcessControl for HostControl {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        self.inner.wait().await
    }

    async fn interrupt(&self, reason: CancelReason) -> Result<InterruptOutcome> {
        self.inner.interrupt(reason).await
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.kill == KillMode::Panics {
            panic!("{REFUSED_KILL}: a host control that panics");
        }
        if let (KillMode::OwnCleanup, Some(control)) = (self.kill, &self.foreign) {
            return Err(Error::CleanupRequired {
                control: Arc::clone(control),
                source: Box::new(Error::Closed {
                    subject: REFUSED_KILL,
                }),
            });
        }
        Err(Error::Closed {
            subject: REFUSED_KILL,
        })
    }
}
