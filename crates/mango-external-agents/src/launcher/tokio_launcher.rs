//! A launcher on `tokio::process`.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::watch;

use crate::error::{Error, Result};
use crate::process::{
    ByteSink, ByteSource, DEFAULT_STDERR_TAIL_BYTES, ExitStatus, LaunchSpec, ManagedProcess,
    ProcessControl, ProcessLauncher, StderrTail,
};
use crate::session::CancelReason;

/// Applies the host's exact working directory, environment and pipe policy to a command.
fn configured_command(spec: &LaunchSpec, program: &str) -> Command {
    let mut command = Command::new(program);
    command
        .args(&spec.argv[1..])
        .current_dir(&spec.cwd)
        // The allowlist is the whole environment, not an addition to this process's.
        .env_clear()
        .envs(&spec.env)
        .stdin(if spec.stdin {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    if spec.hide_window {
        command.creation_flags(CREATE_NO_WINDOW);
    }

    command
}

/// How much of a child's output is read at once.
const CHUNK_BYTES: usize = 16 * 1024;

/// Windows `CREATE_NO_WINDOW`: an agent CLI is not something a user asked to see a console for.
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The ordinary way to spawn a vendor CLI.
///
/// On Unix a child leads its own process group, so escalation reaches everything it started rather
/// than only the process the library can see. On Windows the tree is ended through `taskkill /T`,
/// because a direct child handle does not imply ownership of its descendants there. Installed
/// PowerShell entrypoints use `powershell.exe -File` after native executable resolution fails;
/// script and interpreter paths come only from the supplied launch environment.
///
/// Escalation asks before it insists: the caller closes stdin, then this sends the polite signal,
/// waits out the grace, and only then insists. A vendor asked to stop writes its own state first,
/// and a library that skipped straight to the unstoppable signal would lose that every time.
///
/// What decides both steps is the group, not the leader. A helper the vendor started can ignore
/// the polite signal or outlive the process that spawned it, so a leader `wait` already reported
/// is never on its own a reason to stop escalating — and an empty group is the one thing that is,
/// because past that point the number names whatever the operating system hands it to next.
/// Dropping the last handle to a child runs the same escalation rather than a signal of its own:
/// a handshake that fails after the spawn is the path that leaves a workspace held.
#[derive(Clone, Debug)]
pub struct TokioLauncher {
    kill_grace: Duration,
    stderr_tail_bytes: usize,
}

impl Default for TokioLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl TokioLauncher {
    /// A launcher with the ordinary grace period.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::launcher::TokioLauncher;
    /// use std::sync::Arc;
    ///
    /// let launcher: Arc<dyn mango_external_agents::ProcessLauncher> = Arc::new(TokioLauncher::new());
    /// ```
    pub fn new() -> Self {
        Self {
            kill_grace: Duration::from_secs(2),
            stderr_tail_bytes: DEFAULT_STDERR_TAIL_BYTES,
        }
    }

    /// Gives a child this long to exit on its own at each step of the escalation.
    #[must_use]
    pub fn with_kill_grace(mut self, kill_grace: Duration) -> Self {
        self.kill_grace = kill_grace;
        self
    }

    /// Keeps this much of a child's stderr for diagnostics.
    #[must_use]
    pub fn with_stderr_tail_bytes(mut self, stderr_tail_bytes: usize) -> Self {
        self.stderr_tail_bytes = stderr_tail_bytes;
        self
    }

    /// Takes every bound this launcher has from the host's own.
    ///
    /// [`Limits`](crate::Limits) carries a kill grace and a stderr tail, and this type carried its own copies of
    /// both. A host that set one and constructed the launcher with the other silently got the
    /// launcher's — the defaults agree, so nothing would have shown it up until the day a host
    /// changed one.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::Limits;
    /// use mango_external_agents::launcher::TokioLauncher;
    /// use std::time::Duration;
    ///
    /// let limits = Limits {
    ///     kill_grace: Duration::from_secs(10),
    ///     ..Limits::default()
    /// };
    /// let launcher = TokioLauncher::new().with_limits(&limits);
    ///
    /// assert_eq!(launcher.kill_grace(), Duration::from_secs(10));
    /// ```
    #[must_use]
    pub fn with_limits(mut self, limits: &crate::host::Limits) -> Self {
        self.kill_grace = limits.kill_grace;
        self.stderr_tail_bytes = limits.stderr_tail_bytes;
        self
    }

    /// How long a child gets at each step of the escalation.
    pub fn kill_grace(&self) -> Duration {
        self.kill_grace
    }

    /// How much of a child's stderr is kept.
    pub fn stderr_tail_bytes(&self) -> usize {
        self.stderr_tail_bytes
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for TokioLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        // A host may construct LaunchSpec directly. Normalize once before both native spawning
        // and PowerShell fallback inspect the environment; Windows treats key casing as equal.
        #[cfg(windows)]
        let spec = LaunchSpec {
            env: crate::env::windows_effective_environment(&spec.env),
            ..spec
        };
        let Some(program) = spec.program() else {
            return Err(Error::HostConfiguration {
                expected: "an argv naming a program",
                received: String::from("an empty argv"),
            });
        };

        let mut command = configured_command(&spec, program);
        let launched = command.spawn();
        #[cfg(windows)]
        let launched = launched.or_else(|error| {
            let script = std::path::Path::new(program)
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("ps1"));
            if error.kind() != std::io::ErrorKind::NotFound && !script {
                return Err(error);
            }
            let Some(fallback) = super::powershell::fallback(&spec) else {
                return Err(error);
            };
            configured_command(&fallback, &fallback.argv[0]).spawn()
        });
        // The kind rather than the message: `io::Error`'s own text can name the path the operating
        // system was given, which is host-provided, while the kind is a bounded enum and is the
        // half an operator acts on — an absent executable is a different fix from a refused one.
        let mut child = launched.map_err(|error| Error::Launch {
            program: program.to_owned(),
            message: format!("a launcher failure ({:?})", error.kind()),
        })?;

        let pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| Error::Launch {
            program: program.to_owned(),
            message: String::from("a child without a readable stdout"),
        })?;
        let stdin = child.stdin.take();
        let stderr_pipe = child.stderr.take();

        let stderr = StderrTail::with_capacity(self.stderr_tail_bytes);
        if let Some(mut pipe) = stderr_pipe {
            let tail = stderr.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; CHUNK_BYTES];
                while let Ok(read) = pipe.read(&mut buffer).await {
                    if read == 0 {
                        break;
                    }
                    tail.push(&buffer[..read]);
                }
            });
        }

        // The child is owned by one reaper task, so every waiter reads the same answer and nothing
        // races to `wait` on it twice.
        let (exited, exit) = watch::channel(None);
        tokio::spawn(async move {
            let status = child.wait().await.ok().map(|status| ExitStatus {
                code: status.code(),
                signal: signal_of(&status),
            });
            let _ = exited.send(Some(status.unwrap_or_default()));
        });

        Ok(ManagedProcess {
            stdout: Box::new(PipeSource {
                stdout,
                buffer: vec![0_u8; CHUNK_BYTES],
            }),
            stdin: stdin
                .map(|stdin| -> Box<dyn ByteSink> { Box::new(PipeSink { stdin: Some(stdin) }) }),
            control: Arc::new(TokioChild {
                pid,
                exit,
                stderr,
                kill_grace: self.kill_grace,
                killed: AtomicBool::new(false),
            }),
        })
    }
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    std::os::unix::process::ExitStatusExt::signal(status)
}

#[cfg(not(unix))]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

struct PipeSource {
    stdout: ChildStdout,
    buffer: Vec<u8>,
}

#[async_trait::async_trait]
impl ByteSource for PipeSource {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let read = self
            .stdout
            .read(&mut self.buffer)
            .await
            .map_err(|error| Error::Link {
                peer: String::from("child stdout"),
                message: error.to_string(),
            })?;
        if read == 0 {
            return Ok(None);
        }
        Ok(Some(self.buffer[..read].to_vec()))
    }
}

/// The write half of a child's stdin, held so that closing it really closes it.
///
/// `shutdown` is not enough: on a child's stdin it resolves without closing the descriptor, and
/// the pipe only reaches the child as end-of-input when the handle is dropped. A print-mode vendor
/// reads until end of input, so a close that did not close would be a process that never finishes
/// and a turn that ends only at the host's hard timeout.
struct PipeSink {
    stdin: Option<ChildStdin>,
}

#[async_trait::async_trait]
impl ByteSink for PipeSink {
    async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(Error::Closed {
                subject: "child stdin",
            });
        };
        stdin.write_all(bytes).await.map_err(pipe_error)?;
        stdin.flush().await.map_err(pipe_error)
    }

    async fn close(&mut self) -> Result<()> {
        // Idempotent: a close racing a normal finish must not fail the second caller.
        let Some(mut stdin) = self.stdin.take() else {
            return Ok(());
        };
        // A child that already exited closed this end for us; that is not a failure to report.
        let _ = stdin.shutdown().await;
        drop(stdin);
        Ok(())
    }
}

fn pipe_error(error: std::io::Error) -> Error {
    Error::Link {
        peer: String::from("child stdin"),
        message: error.to_string(),
    }
}

struct TokioChild {
    pid: Option<u32>,
    exit: watch::Receiver<Option<ExitStatus>>,
    stderr: StderrTail,
    kill_grace: Duration,
    killed: AtomicBool,
}

impl TokioChild {
    fn exited(&self) -> Option<ExitStatus> {
        *self.exit.borrow()
    }
}

/// Waits out the grace, and says whether the leader went away inside it.
#[cfg(windows)]
async fn leader_exits_within(
    mut exit: watch::Receiver<Option<ExitStatus>>,
    grace: Duration,
) -> bool {
    tokio::time::timeout(grace, async {
        loop {
            if exit.borrow_and_update().is_some() {
                return;
            }
            if exit.changed().await.is_err() {
                return;
            }
        }
    })
    .await
    .is_ok()
}

#[async_trait::async_trait]
impl ProcessControl for TokioChild {
    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn stderr_tail(&self) -> String {
        self.stderr.read()
    }

    async fn wait(&self) -> Result<ExitStatus> {
        let mut exit = self.exit.clone();
        loop {
            if let Some(status) = *exit.borrow_and_update() {
                return Ok(status);
            }
            if exit.changed().await.is_err() {
                return Ok(ExitStatus::default());
            }
        }
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        let Some(pid) = self.pid else {
            // A child that never had a pid never started; one whose pid is already gone was
            // reaped before any handle asked, and there is nothing left to signal.
            return if self.exited().is_some() {
                Ok(())
            } else {
                Err(Error::Launch {
                    program: String::from("<unknown>"),
                    message: String::from("a running child with no process id to end"),
                })
            };
        };
        // Escalated once, even if several tasks ask: a second escalation would be signalling a
        // group id the operating system may already have handed to somebody else. The second
        // caller waits on the first caller's outcome rather than reporting a child that is still
        // inside its grace as already gone — `Ok` from `kill` is what a host reads as "the
        // workspace is free".
        if self.killed.swap(true, Ordering::AcqRel) {
            let waited = self.kill_grace * 2;
            return if tree_ends_within(pid, self.exit.clone(), waited).await {
                Ok(())
            } else {
                Err(Error::Launch {
                    program: format!("process group {pid}"),
                    message: format!(
                        "a live process after {waited:?}, with an escalation already running"
                    ),
                })
            };
        }
        end_process_tree(pid, self.kill_grace, self.exit.clone()).await
    }
}

impl Drop for TokioChild {
    /// A child whose last handle went away was never asked to stop.
    ///
    /// The reaper task owns the `Child` and parks in `wait`, so nothing else would ever end it: a
    /// handshake that failed after the spawn — the transport is built, `initialize` times out, the
    /// harness returns `Err` — would leave a persistent app-server holding the user's workspace
    /// for the life of the host. Relying on the vendor noticing its stdin closed is relying on
    /// vendor behaviour this library refuses to assume anywhere else.
    fn drop(&mut self) {
        // Claimed the same way `kill` claims it, so a drop racing a kill escalates once. The
        // child having exited is deliberately not a reason to stop here: the leader is not the
        // tree, and a helper it left behind is exactly what this is for.
        if self.killed.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(pid) = self.pid else {
            return;
        };
        let grace = self.kill_grace;
        let exit = self.exit.clone();
        match tokio::runtime::Handle::try_current() {
            // The same escalation `kill` runs, rather than a sleep and a signal: it asks first,
            // stops the moment nothing is left, and never signals a tree that is already gone.
            Ok(runtime) => {
                runtime.spawn(async move {
                    let _ = end_process_tree(pid, grace, exit).await;
                });
            }
            // Nothing left to wait a grace on, so there is nowhere to wait between asking and
            // insisting. Still checked first: a tree that is already gone must not be signalled,
            // because the number may name whatever the operating system handed it to next.
            Err(_) => {
                if !tree_is_gone(pid, &exit) {
                    insist_tree_stops(pid);
                }
            }
        }
    }
}

/// Asks a whole tree to stop, as a unit, so every member runs its own shutdown.
#[cfg(unix)]
fn ask_tree_to_stop(pid: u32) {
    use nix::sys::signal::{Signal, killpg};

    let _ = killpg(group_of(pid), Signal::SIGTERM);
}

/// Ends a tree with the signal it cannot decline.
#[cfg(unix)]
fn insist_tree_stops(pid: u32) {
    use nix::sys::signal::{Signal, killpg};

    let _ = killpg(group_of(pid), Signal::SIGKILL);
}

#[cfg(unix)]
fn group_of(pid: u32) -> nix::unistd::Pid {
    nix::unistd::Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX))
}

/// Ends a tree with the only step Windows has.
///
/// There is no polite call to make first: `taskkill` without `/F` posts `WM_CLOSE`, which a
/// console program does not handle, so asking and insisting would be the same call.
#[cfg(windows)]
fn insist_tree_stops(pid: u32) {
    use std::os::windows::process::CommandExt as _;

    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// How often a tree is asked whether anything is left of it.
#[cfg(unix)]
const TREE_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Whether the operating system still knows anything in this tree.
///
/// The group, not the leader. A helper the vendor started can block the polite signal or outlive
/// the process that spawned it, and both present as a leader `wait` already reported while the
/// workspace is still held — so the leader exiting is never on its own a reason to stop.
///
/// `ESRCH` is also what says the group id itself is free: the kernel keeps the number reserved
/// while the group has a member, so an empty group is the one moment after which signalling it
/// would reach whatever the operating system hands the number to next. Only `ESRCH` counts;
/// `EPERM` is a group that exists and that this process may not touch, which is a failure to
/// report rather than a tree that is gone.
#[cfg(unix)]
fn tree_is_gone(pid: u32, _exit: &watch::Receiver<Option<ExitStatus>>) -> bool {
    matches!(
        nix::sys::signal::killpg(group_of(pid), None),
        Err(nix::errno::Errno::ESRCH)
    )
}

/// Whether the operating system still knows anything in this tree.
///
/// The leader's own exit, because Windows offers no cheap equivalent of a process group to ask.
/// `taskkill /T` is what reaches the descendants; this is what says there is still something to
/// run it against, and what stops a second run from naming a process id Windows has reissued.
#[cfg(windows)]
fn tree_is_gone(_pid: u32, exit: &watch::Receiver<Option<ExitStatus>>) -> bool {
    exit.borrow().is_some()
}

/// Waits out the grace, and says whether the tree went away inside it.
#[cfg(unix)]
async fn tree_ends_within(
    pid: u32,
    exit: watch::Receiver<Option<ExitStatus>>,
    grace: Duration,
) -> bool {
    tokio::time::timeout(grace, async {
        while !tree_is_gone(pid, &exit) {
            tokio::time::sleep(TREE_POLL_INTERVAL).await;
        }
    })
    .await
    .is_ok()
}

/// Waits out the grace, and says whether the tree went away inside it.
#[cfg(windows)]
async fn tree_ends_within(
    _pid: u32,
    exit: watch::Receiver<Option<ExitStatus>>,
    grace: Duration,
) -> bool {
    leader_exits_within(exit, grace).await
}

/// Asks a tree to stop, waits, and insists only on what is left.
///
/// Takes the reaper's view of the leader rather than the child itself, so the drop path — which no
/// longer holds a handle — runs the same escalation as [`ProcessControl::kill`] instead of a sleep
/// and a signal of its own.
#[cfg(unix)]
async fn end_process_tree(
    pid: u32,
    grace: Duration,
    exit: watch::Receiver<Option<ExitStatus>>,
) -> Result<()> {
    if tree_is_gone(pid, &exit) {
        return Ok(());
    }

    ask_tree_to_stop(pid);
    if tree_ends_within(pid, exit.clone(), grace).await {
        return Ok(());
    }

    insist_tree_stops(pid);
    if tree_ends_within(pid, exit, grace).await {
        return Ok(());
    }
    Err(Error::Launch {
        program: format!("process group {pid}"),
        message: format!("a live group member after {grace:?}"),
    })
}

/// Ends a tree with the one step Windows has.
///
/// A direct child handle does not imply ownership of descendants here, so the tree is ended
/// through the system's own primitive rather than by killing what the library can see. There is no
/// polite step to take first: `taskkill` without `/F` posts `WM_CLOSE`, which a console program
/// does not handle.
#[cfg(windows)]
async fn end_process_tree(
    pid: u32,
    grace: Duration,
    exit: watch::Receiver<Option<ExitStatus>>,
) -> Result<()> {
    if tree_is_gone(pid, &exit) {
        return Ok(());
    }

    let mut taskkill = Command::new("taskkill");
    taskkill
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);

    let ran = tokio::time::timeout(grace, taskkill.status()).await;
    if tree_ends_within(pid, exit, grace).await {
        return Ok(());
    }
    Err(Error::Launch {
        program: format!("process tree {pid}"),
        message: match ran {
            Ok(Ok(status)) => {
                format!("a live process after taskkill exited with status {status}")
            }
            // The kind, not the message: see the spawn arm above.
            Ok(Err(error)) => format!("a taskkill that would not run ({:?})", error.kind()),
            Err(_) => format!("no answer from taskkill within {grace:?}"),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::TokioLauncher;
    use crate::process::{LaunchSpec, LineLimits, LineStream, ProcessLauncher};
    use crate::session::CancelReason;
    use std::collections::BTreeMap;
    use std::time::Duration;

    /// Selects what the re-executed test binary does when it stands in for a vendor CLI.
    const FIXTURE_MODE: &str = "MEA_LAUNCHER_FIXTURE";

    /// The child half of every test below.
    ///
    /// Re-executing this binary is what makes the launcher testable on all three operating systems
    /// without shipping a fixture program or assuming a shell: the test binary is the one
    /// executable that is certainly present and certainly runnable.
    #[test]
    fn launcher_fixture_child() {
        let Ok(mode) = std::env::var(FIXTURE_MODE) else {
            return;
        };
        match mode.as_str() {
            "lines" => println!("MEA-FIXTURE first line\nMEA-FIXTURE second"),
            "environment" => {
                for (key, value) in std::env::vars() {
                    println!("MEA-FIXTURE {key}={value}");
                }
            }
            "stderr" => eprint!(
                "Authorization: Bearer top-secret API_KEY=another-secret redis://app:password@db/main"
            ),
            "echo" => {
                use std::io::Write as _;
                let mut line = String::new();
                while std::io::stdin()
                    .read_line(&mut line)
                    .is_ok_and(|read| read > 0)
                {
                    print!("MEA-FIXTURE echo:{line}");
                    // A piped stdout is block-buffered, so an answer nobody flushed is an answer
                    // that never arrives — which is exactly how a real vendor CLI hangs a turn.
                    std::io::stdout().flush().ok();
                    line.clear();
                }
            }
            "forever" => loop {
                std::thread::sleep(Duration::from_secs(60));
            },
            // Spawns a helper of its own and exits — the shape a vendor CLI leaves behind when it
            // hands its work to a daemon. The helper inherits this stdout and this process group,
            // so the pipe the test holds outlives the process the launcher can see.
            #[cfg(unix)]
            "leaves-a-helper" => {
                let executable =
                    std::env::current_exe().expect("expected the test binary's own path");
                let _ = std::process::Command::new(executable)
                    .args(["launcher_fixture_child", "--nocapture", "--test-threads=1"])
                    .env(FIXTURE_MODE, "survives-sigterm")
                    .stdin(std::process::Stdio::null())
                    .spawn();
            }
            // Blocked rather than handled: installing a handler needs `unsafe`, which this crate
            // forbids. A blocked `SIGTERM` stays pending forever while `SIGKILL` still lands,
            // which is the only thing that reaches this helper.
            #[cfg(unix)]
            "survives-sigterm" => {
                let mut blocked = nix::sys::signal::SigSet::empty();
                blocked.add(nix::sys::signal::Signal::SIGTERM);
                let _ = blocked.thread_block();
                loop {
                    std::thread::sleep(Duration::from_secs(60));
                }
            }
            _ => {}
        }
        // Before the harness prints its own trailer, so the child's output is only its own.
        std::process::exit(0);
    }

    fn fixture(mode: &str) -> LaunchSpec {
        let executable = std::env::current_exe().expect("expected the test binary's own path");
        LaunchSpec {
            argv: vec![
                executable.to_string_lossy().into_owned(),
                // A substring filter rather than `--exact`, which would have to spell out the
                // module path and would silently match nothing after a rename.
                String::from("launcher_fixture_child"),
                String::from("--nocapture"),
                String::from("--test-threads=1"),
            ],
            cwd: std::env::temp_dir(),
            // Built the way a harness builds it rather than from a hand-picked pair: on Windows a
            // child needs `SystemRoot` and friends to start at all, and going through the library's
            // own allowlist is what proves that list alone is enough to run a program.
            env: fixture_environment(mode),
            stdin: true,
            hide_window: true,
        }
    }

    /// The allowlist, plus the one key that tells the re-executed binary which child to be.
    fn fixture_environment(mode: &str) -> BTreeMap<String, String> {
        environment_from(&crate::env::EnvSource::from_process(), mode)
    }

    fn environment_from(source: &crate::env::EnvSource, mode: &str) -> BTreeMap<String, String> {
        let mut environment = crate::env::allowlist(source, &[]);
        environment.insert(String::from(FIXTURE_MODE), mode.to_owned());
        environment
    }

    /// A launcher built from the host's bounds uses them, rather than keeping its own copies.
    #[test]
    fn a_launcher_takes_its_bounds_from_the_host() {
        let limits = crate::Limits {
            kill_grace: Duration::from_secs(10),
            stderr_tail_bytes: 4_096,
            ..crate::Limits::default()
        };
        let launcher = TokioLauncher::new().with_limits(&limits);

        assert_eq!(launcher.kill_grace(), Duration::from_secs(10));
        assert_eq!(launcher.stderr_tail_bytes(), 4_096);
    }

    /// A handshake that fails after the spawn drops every handle to the child. The reaper owns it
    /// and parks in `wait`, so without a teardown here a persistent vendor process would hold the
    /// user's workspace for the life of the host.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_last_handle_ends_the_child_rather_than_orphaning_it() {
        let child = TokioLauncher::new()
            .with_kill_grace(Duration::from_millis(50))
            .spawn(fixture("forever"))
            .await
            .expect("expected a child");
        let pid = child.control.pid().expect("expected a process id");

        // Settled first, deliberately. Dropping the moment after the spawn closes the stdout pipe
        // under a child that has not written yet, and the `SIGPIPE` that follows ends it for a
        // reason that has nothing to do with this teardown — which is a test that passes without
        // the code it is testing.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            alive(pid),
            "expected a child still running before the handles are dropped"
        );

        drop(child);

        // Bounded: a regression leaves the process running, and an unbounded wait would report
        // nothing at all.
        tokio::time::timeout(Duration::from_secs(10), async {
            while alive(pid) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("expected the orphaned child to be ended");
    }

    /// The leader's own `wait` is not the tree's. A helper the vendor started can block `SIGTERM`
    /// and outlive the process that spawned it, and the module contract says escalation reaches
    /// everything the child started — so returning `Ok` because the leader was reaped left the
    /// helper holding the workspace.
    ///
    /// Stdout is the probe rather than the helper's own pid: a helper whose parent exited is
    /// reparented, and `kill(pid, 0)` still reports a zombie nobody has reaped as alive.
    #[cfg(unix)]
    #[tokio::test]
    async fn escalation_reaches_a_helper_that_outlived_the_child() {
        let child = TokioLauncher::new()
            .with_kill_grace(Duration::from_millis(300))
            .spawn(fixture("leaves-a-helper"))
            .await
            .expect("expected a child");

        // The leader gone and the helper running is the shape the escalation used to read as an
        // empty tree, so the kill has to happen after it has settled rather than into a race.
        tokio::time::sleep(Duration::from_millis(500)).await;

        child
            .control
            .kill(CancelReason::Shutdown)
            .await
            .expect("expected the tree to be ended");

        let mut stdout = child.stdout;
        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(Some(_)) = stdout.next_chunk().await {}
        })
        .await;
        assert!(
            ended.is_ok(),
            "expected the helper's end of the pipe to close, received an open pipe"
        );
    }

    /// The drop path is the one a failed handshake takes, and it runs the same escalation as
    /// `kill` rather than one of its own: a leader that exited is not a tree that is gone, so
    /// stopping at the leader left the helper holding the workspace for the life of the host.
    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_last_handle_reaches_a_helper_the_child_left_behind() {
        let child = TokioLauncher::new()
            .with_kill_grace(Duration::from_millis(300))
            .spawn(fixture("leaves-a-helper"))
            .await
            .expect("expected a child");

        tokio::time::sleep(Duration::from_millis(500)).await;

        // Only the handles go: stdout stays, because the helper inherited the other end of it and
        // the pipe is what says when the helper is gone.
        let mut stdout = child.stdout;
        drop(child.stdin);
        drop(child.control);

        let ended = tokio::time::timeout(Duration::from_secs(10), async {
            while let Ok(Some(_)) = stdout.next_chunk().await {}
        })
        .await;
        assert!(
            ended.is_ok(),
            "expected the helper's end of the pipe to close, received an open pipe"
        );
    }

    /// Whether the operating system still knows this process id.
    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None).is_ok()
    }

    /// Every line the fixture itself wrote, without the test harness's own chatter.
    async fn fixture_lines(mode: &str) -> Vec<String> {
        let child = TokioLauncher::new()
            .spawn(fixture(mode))
            .await
            .expect("expected a child");
        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        let mut written = Vec::new();
        while let Some(line) = lines.next_line().await.expect("expected a line or the end") {
            if let Some((_, rest)) = line.split_once("MEA-FIXTURE ") {
                written.push(rest.to_owned());
            }
        }
        written
    }

    #[tokio::test]
    async fn reads_a_childs_output_line_by_line() {
        assert_eq!(
            fixture_lines("lines").await,
            vec![String::from("first line"), String::from("second")]
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_powershell_cli_on_path_receives_literal_arguments() {
        let directory = std::env::temp_dir().join(format!("mea-script-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let script = directory.join("mea-fixture.ps1");
        std::fs::write(&script, "Write-Output $args[0]\n").expect("fixture script");
        let literal = "literal $(Get-Date); & text";
        for program in [
            String::from("mea-fixture"),
            script.to_string_lossy().into_owned(),
        ] {
            let mut spec = fixture("lines");
            spec.argv = vec![program, literal.into()];
            spec.env
                .insert("PATH".into(), directory.to_string_lossy().into_owned());
            let child = match TokioLauncher::new().spawn(spec).await {
                Ok(child) => child,
                Err(error) => panic!(
                    "expected a PowerShell CLI on the supplied PATH to launch, received {error}"
                ),
            };
            let mut lines = LineStream::new(child.stdout, LineLimits::default());
            assert_eq!(
                lines.next_line().await.expect("stdout"),
                Some(literal.into())
            );
            assert!(child.control.wait().await.expect("exit status").success());
        }
        std::fs::remove_dir_all(&directory).expect("remove fixture directory");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn powershell_lookup_and_child_see_the_same_windows_path_value() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("mea-path-casing-{nonce}"));
        std::fs::create_dir(&directory).expect("fixture directory");
        std::fs::write(directory.join("mea-casing.ps1"), "Write-Output $env:Path\n")
            .expect("fixture script");

        let mut spec = fixture("lines");
        spec.argv = vec![String::from("mea-casing")];
        spec.env.insert(
            String::from("PATH"),
            directory.to_string_lossy().into_owned(),
        );
        spec.env
            .insert(String::from("Path"), String::from(r"C:\wrong"));

        let child = TokioLauncher::new()
            .spawn(spec)
            .await
            .expect("lookup must use the same PATH as process creation");
        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        assert_eq!(
            lines.next_line().await.expect("child PATH"),
            Some(directory.to_string_lossy().into_owned())
        );
        assert!(child.control.wait().await.expect("exit status").success());
        std::fs::remove_dir_all(&directory).expect("remove fixture directory");
    }

    #[tokio::test]
    async fn a_child_receives_the_allowlist_and_nothing_else() {
        // A host whose own environment carries a secret next to the keys a child legitimately
        // needs. The allowlist is what separates them, and this is the only test that proves the
        // separation survives an actual process creation.
        let source = crate::env::EnvSource::from_pairs(std::env::vars().chain([(
            String::from("CONNECTOR_SECRET"),
            String::from("never-forward-this"),
        )]));
        let mut spec = fixture("environment");
        spec.env = environment_from(&source, "environment");
        let child = TokioLauncher::new()
            .spawn(spec)
            .await
            .expect("expected a child");

        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        let mut environment = Vec::new();
        while let Some(line) = lines.next_line().await.expect("expected a line or the end") {
            if let Some((_, rest)) = line.split_once("MEA-FIXTURE ") {
                environment.push(rest.to_owned());
            }
        }

        // What the spec carried reaches the child; what the allowlist dropped does not.
        assert!(
            environment
                .iter()
                .any(|entry| entry.starts_with("MEA_LAUNCHER_FIXTURE=")),
            "expected the spec's own keys, received {environment:?}"
        );
        assert!(
            !environment
                .iter()
                .any(|entry| entry.contains("CONNECTOR_SECRET")),
            "expected the host's secret to stay behind, received {environment:?}"
        );
        assert!(
            !environment
                .iter()
                .any(|entry| entry.contains("CARGO_PKG_NAME")),
            "expected this process's environment to be cleared, received {environment:?}"
        );
    }

    #[tokio::test]
    async fn writes_to_a_childs_stdin_and_reads_its_answer() {
        let mut child = TokioLauncher::new()
            .spawn(fixture("echo"))
            .await
            .expect("expected a child");
        let mut stdin = child.stdin.take().expect("expected a stdin");

        stdin
            .write_all(b"ping\n")
            .await
            .expect("expected the write to land");

        let mut lines = LineStream::new(child.stdout, LineLimits::default());
        let mut seen = Vec::new();
        let answered = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(line) = lines.next_line().await.expect("expected a line or the end") {
                if let Some((_, rest)) = line.split_once("MEA-FIXTURE ") {
                    return rest.to_owned();
                }
                seen.push(line);
            }
            String::new()
        })
        .await
        .unwrap_or_else(|_| panic!("expected an answer, received {seen:?}"));
        assert_eq!(answered, "echo:ping", "received {seen:?}");

        // Closing stdin is the polite end: the child's read returns nothing and it exits. Bounded,
        // because a close that did not really close would otherwise hang this test rather than
        // fail it — which is how the defect it guards against first showed up.
        stdin.close().await.expect("expected the close to land");
        let status = tokio::time::timeout(Duration::from_secs(20), child.control.wait())
            .await
            .expect("expected closing stdin to end the child")
            .expect("expected an exit status");
        assert!(status.success(), "received {status:?}");
    }

    #[tokio::test]
    async fn closing_a_childs_input_twice_is_harmless() {
        let mut child = TokioLauncher::new()
            .spawn(fixture("echo"))
            .await
            .expect("expected a child");
        let mut stdin = child.stdin.take().expect("expected a stdin");

        stdin.close().await.expect("expected the close to land");
        stdin
            .close()
            .await
            .expect("expected a second close to be harmless");
        assert!(
            stdin.write_all(b"late\n").await.is_err(),
            "expected a write after close to be refused"
        );
    }

    #[tokio::test]
    async fn keeps_a_redacted_stderr_tail() {
        let child = TokioLauncher::new()
            .spawn(fixture("stderr"))
            .await
            .expect("expected a child");
        child.control.wait().await.expect("expected an exit");

        let tail = child.control.stderr_tail();
        assert!(tail.contains("[REDACTED]"), "received {tail:?}");
        assert!(!tail.contains("top-secret"), "received {tail:?}");
        assert!(!tail.contains("another-secret"), "received {tail:?}");
        assert!(!tail.contains("password@"), "received {tail:?}");
    }

    #[tokio::test]
    async fn ends_a_child_that_would_never_exit_on_its_own() {
        let child = TokioLauncher::new()
            .with_kill_grace(Duration::from_millis(500))
            .spawn(fixture("forever"))
            .await
            .expect("expected a child");

        child
            .control
            .kill(CancelReason::Shutdown)
            .await
            .expect("expected the child to be ended");
        let status = tokio::time::timeout(Duration::from_secs(10), child.control.wait())
            .await
            .expect("expected the child to have exited")
            .expect("expected an exit status");
        assert!(
            !status.success(),
            "expected a killed child not to report success, received {status:?}"
        );
    }

    #[tokio::test]
    async fn killing_twice_is_harmless() {
        let child = TokioLauncher::new()
            .with_kill_grace(Duration::from_millis(500))
            .spawn(fixture("forever"))
            .await
            .expect("expected a child");

        child
            .control
            .kill(CancelReason::Shutdown)
            .await
            .expect("expected the child to be ended");
        child
            .control
            .kill(CancelReason::Shutdown)
            .await
            .expect("expected a second kill to be harmless");
    }

    #[tokio::test]
    async fn a_program_that_does_not_exist_is_a_typed_launch_failure() {
        let mut spec = fixture("lines");
        spec.argv[0] = String::from("mea-no-such-program");

        let error = TokioLauncher::new()
            .spawn(spec)
            .await
            .err()
            .expect("expected a refusal, received a child");
        assert!(
            matches!(error, crate::Error::Launch { .. }),
            "expected a launch failure, received {error:?}"
        );
        // The kind is what an operator acts on, and it is the only part of an `io::Error` that
        // cannot carry the path the operating system was handed.
        let rendered = error.to_string();
        assert!(
            rendered.ends_with("received a launcher failure (NotFound)"),
            "expected the io error kind, received {rendered}"
        );
        assert!(
            !rendered.contains("mea-no-such-program"),
            "expected the requested program to stay out of the diagnostic, received {rendered}"
        );
    }

    #[tokio::test]
    async fn an_empty_argv_is_refused_before_anything_is_spawned() {
        let mut spec = fixture("lines");
        spec.argv.clear();

        let error = TokioLauncher::new()
            .spawn(spec)
            .await
            .err()
            .expect("expected a refusal, received a child");
        assert!(
            matches!(error, crate::Error::HostConfiguration { .. }),
            "expected a configuration refusal, received {error:?}"
        );
    }
}
