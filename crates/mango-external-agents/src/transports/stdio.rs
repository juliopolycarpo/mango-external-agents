//! A child process's pipes, framed by lines.
//!
//! The library owns the framing and the caps; the host owns the spawn. What comes back is a
//! [`Link`] that knows nothing about processes, plus the control handle for the child underneath
//! it — separate because ending a process is a different decision from ending a conversation.

use std::sync::Arc;

use crate::error::{Error, Result};
use crate::host::HostContext;
use crate::link::{Link, LinkReceiver, LinkSender};
use crate::process::{ByteSink, LineStream, ProcessCleanupGuard, ProcessControl};
use crate::transport::{ExecutablePath, StdioSpec};

/// A spawned child, as a link and the handle that ends it.
pub struct StdioTransport {
    /// Messages in and out, one line each.
    pub link: Link,
    /// The child underneath, for waiting on it and ending it.
    pub control: Arc<dyn ProcessControl>,
}

impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StdioTransport")
            .field("pid", &self.control.pid())
            .finish_non_exhaustive()
    }
}

/// Spawns `spec` through the host's launcher and frames its stdout by lines.
///
/// The working directory and the environment come from the host: the directory it authorised, and
/// the positive allowlist built from its [`EnvSource`](crate::EnvSource) plus the harness's own
/// documented keys. A harness cannot widen either, which is the point of passing only an argv.
///
/// # Errors
///
/// [`Error::HostConfiguration`] when the argv is empty, and whatever the host's launcher reported
/// otherwise.
pub async fn open(
    host: &HostContext,
    spec: &StdioSpec,
    executable: &ExecutablePath,
    vendor_environment_keys: &[&str],
) -> Result<StdioTransport> {
    let Some(program) = spec.program() else {
        return Err(Error::HostConfiguration {
            expected: "an argv naming a program",
            received: String::from("an empty argv"),
        });
    };

    // The host resolved the executable if it could; the harness only knows the program's name.
    // It comes from the request rather than the context because it was resolved for this harness.
    let mut argv = spec.argv.clone();
    argv[0] = executable.or(program.to_owned());

    let mut child = host
        .launcher()
        .spawn(crate::process::LaunchSpec {
            argv,
            cwd: host.cwd().to_path_buf(),
            env: host.child_environment(vendor_environment_keys),
            stdin: true,
            hide_window: true,
        })
        .await?;

    let Some(stdin) = child.stdin.take() else {
        let cleanup =
            ProcessCleanupGuard::new(child.control, *host.limits(), crate::CancelReason::Shutdown);
        // The caller may abandon this refusal while the injected process control is pending.
        // A detached task retains cleanup ownership and every shutdown stage stays bounded.
        cleanup.finish().await?;
        return Err(Error::Launch {
            program: program.to_owned(),
            message: String::from("a child without a writable stdin"),
        });
    };

    Ok(StdioTransport {
        link: Link::new(
            Box::new(LineSink { stdin }),
            Box::new(LineSource {
                lines: LineStream::new(child.stdout, host.limits().line),
            }),
        ),
        control: child.control,
    })
}

/// Writes one message per line.
struct LineSink {
    stdin: Box<dyn ByteSink>,
}

#[async_trait::async_trait]
impl LinkSender for LineSink {
    async fn send(&mut self, message: String) -> Result<()> {
        // One write rather than two: a second write could interleave with another task's message
        // and split a frame across two lines.
        self.stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
    }

    async fn close(&mut self) -> Result<()> {
        self.stdin.close().await
    }
}

/// Reads one message per line.
struct LineSource {
    lines: LineStream,
}

#[async_trait::async_trait]
impl LinkReceiver for LineSource {
    async fn recv(&mut self) -> Result<Option<String>> {
        self.lines.next_line().await
    }
}

#[cfg(test)]
mod tests {
    use super::open;
    use crate::env::EnvSource;
    use crate::error::Error;
    use crate::host::HostContext;
    use crate::process::{ExitStatus, LaunchSpec, ManagedProcess, ProcessControl, ProcessLauncher};
    use crate::testing::{FakeLauncher, FakeProcess};
    use crate::transport::{ExecutablePath, StdioSpec};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A host launcher that returns a live child without the requested input pipe.
    struct MissingStdinLauncher(Arc<FakeLauncher>);

    #[async_trait::async_trait]
    impl ProcessLauncher for MissingStdinLauncher {
        async fn spawn(&self, spec: LaunchSpec) -> crate::Result<ManagedProcess> {
            let mut child = self.0.spawn(spec).await?;
            child.stdin = None;
            Ok(child)
        }
    }

    /// Holds process-tree cleanup after launch returned an invalid pipe set.
    struct HeldMissingStdinLauncher {
        inner: Arc<FakeLauncher>,
        kill_started: Arc<tokio::sync::Notify>,
        release_kill: Arc<tokio::sync::Notify>,
        kill_claims: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProcessLauncher for HeldMissingStdinLauncher {
        async fn spawn(&self, spec: LaunchSpec) -> crate::Result<ManagedProcess> {
            let mut child = self.inner.spawn(spec).await?;
            child.stdin = None;
            child.control = Arc::new(HeldProcessControl {
                inner: child.control,
                kill_started: Arc::clone(&self.kill_started),
                release_kill: Arc::clone(&self.release_kill),
                kill_claims: Arc::clone(&self.kill_claims),
            });
            Ok(child)
        }
    }

    struct HeldProcessControl {
        inner: Arc<dyn ProcessControl>,
        kill_started: Arc<tokio::sync::Notify>,
        release_kill: Arc<tokio::sync::Notify>,
        kill_claims: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ProcessControl for HeldProcessControl {
        fn pid(&self) -> Option<u32> {
            self.inner.pid()
        }

        fn stderr_tail(&self) -> String {
            self.inner.stderr_tail()
        }

        async fn wait(&self) -> crate::Result<ExitStatus> {
            self.inner.wait().await
        }

        async fn kill(&self, reason: crate::CancelReason) -> crate::Result<()> {
            self.kill_claims.fetch_add(1, Ordering::AcqRel);
            self.kill_started.notify_one();
            self.release_kill.notified().await;
            self.inner.kill(reason).await
        }
    }

    fn host(launcher: Arc<FakeLauncher>) -> HostContext {
        HostContext::builder()
            .launcher(launcher)
            .cwd("/workspace")
            .client_info("test-host", "0.0.0")
            .environment(EnvSource::from_pairs([
                ("PATH", "/bin"),
                ("CONNECTOR_SECRET", "never-forward-this"),
                ("VENDOR_CONFIG", "/home/ada/.vendor"),
            ]))
            .build()
            .expect("expected a context")
    }

    #[tokio::test]
    async fn spawns_with_the_hosts_directory_and_only_the_allowlisted_environment() {
        let launcher = Arc::new(FakeLauncher::scripted("{}\n"));
        let host = host(Arc::clone(&launcher));

        open(
            &host,
            &StdioSpec::new(["codex", "app-server"]),
            &ExecutablePath::default(),
            &["VENDOR_CONFIG"],
        )
        .await
        .expect("expected a transport");

        let launch = launcher.last_launch().expect("expected one launch");
        assert_eq!(launch.argv, vec!["codex", "app-server"]);
        assert_eq!(launch.cwd.to_string_lossy(), "/workspace");
        assert_eq!(launch.env.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(
            launch.env.get("VENDOR_CONFIG").map(String::as_str),
            Some("/home/ada/.vendor")
        );
        assert_eq!(launch.env.get("CONNECTOR_SECRET"), None);
        assert!(launch.stdin, "expected a writable stdin");
        assert!(launch.hide_window, "expected no console window");
    }

    #[tokio::test]
    async fn a_child_missing_its_requested_input_pipe_is_reaped() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let host = HostContext::builder()
            .launcher(Arc::new(MissingStdinLauncher(Arc::clone(&launcher))))
            .cwd("/workspace")
            .client_info("test-host", "0.0.0")
            .build()
            .expect("expected a host");

        let result = open(
            &host,
            &StdioSpec::new(["codex", "app-server"]),
            &ExecutablePath::default(),
            &[],
        )
        .await;
        let error = match result {
            Ok(_) => panic!("expected the absent input pipe to be refused"),
            Err(error) => error,
        };
        assert!(matches!(error, Error::Launch { .. }));
        assert_eq!(
            launcher.live_children(),
            0,
            "expected the failed transport to reap its child"
        );
    }

    #[tokio::test]
    async fn abandoning_a_missing_stdin_refusal_does_not_abandon_child_cleanup() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let kill_started = Arc::new(tokio::sync::Notify::new());
        let release_kill = Arc::new(tokio::sync::Notify::new());
        let kill_claims = Arc::new(AtomicUsize::new(0));
        let host = HostContext::builder()
            .launcher(Arc::new(HeldMissingStdinLauncher {
                inner: Arc::clone(&launcher),
                kill_started: Arc::clone(&kill_started),
                release_kill: Arc::clone(&release_kill),
                kill_claims: Arc::clone(&kill_claims),
            }))
            .cwd(std::env::temp_dir())
            .client_info("test-host", "0.0.0")
            .build()
            .expect("expected a host");
        let opening = tokio::spawn(async move {
            open(
                &host,
                &StdioSpec::new(["codex", "app-server"]),
                &ExecutablePath::default(),
                &[],
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), kill_started.notified())
            .await
            .expect("expected cleanup to reach process-tree termination");
        opening.abort();
        let _ = opening.await;
        release_kill.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while launcher.live_children() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected aborted open to retain child cleanup ownership");
        assert_eq!(
            kill_claims.load(Ordering::Acquire),
            1,
            "expected aborting cleanup to retain one process-tree termination claim"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn missing_stdin_cleanup_obeys_the_host_shutdown_deadline() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let deadline = std::time::Duration::from_secs(2);
        let kill_started = Arc::new(tokio::sync::Notify::new());
        let release_kill = Arc::new(tokio::sync::Notify::new());
        let kill_claims = Arc::new(AtomicUsize::new(0));
        let host = HostContext::builder()
            .launcher(Arc::new(HeldMissingStdinLauncher {
                inner: Arc::clone(&launcher),
                kill_started,
                release_kill: Arc::clone(&release_kill),
                kill_claims,
            }))
            .cwd(std::env::temp_dir())
            .client_info("test-host", "0.0.0")
            .limits(crate::Limits {
                shutdown_timeout: deadline,
                ..crate::Limits::default()
            })
            .build()
            .expect("expected a host");
        let result = tokio::time::timeout(
            deadline * 2,
            open(
                &host,
                &StdioSpec::new(["codex", "app-server"]),
                &ExecutablePath::default(),
                &[],
            ),
        )
        .await
        .expect("expected missing-stdin cleanup to respect the host deadline");
        let error = result.expect_err("expected the missing input pipe to be refused");
        assert!(
            matches!(error.cause(), Error::Timeout { after, .. } if *after == deadline),
            "expected a typed cleanup timeout, received {error:?}"
        );
        let control = error
            .cleanup_control()
            .expect("expected the host to retain a cleanup control after the timeout");
        release_kill.notify_one();
        crate::process::stop_process_with_limits(
            control.as_ref(),
            crate::CancelReason::Shutdown,
            host.limits(),
        )
        .await
        .expect("expected the host to recover and reap the child");
        assert_eq!(
            launcher.live_children(),
            0,
            "expected the recovered cleanup control to reap its child"
        );
    }

    #[tokio::test]
    async fn an_executable_the_host_resolved_replaces_the_bare_program_name() {
        let launcher = Arc::new(FakeLauncher::scripted(""));
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd("/workspace")
            .client_info("test-host", "0.0.0")
            .build()
            .expect("expected a context");

        open(
            &host,
            &StdioSpec::new(["codex", "app-server"]),
            &ExecutablePath::resolved("/opt/codex/bin/codex"),
            &[],
        )
        .await
        .expect("expected a transport");

        let launch = launcher.last_launch().expect("expected one launch");
        assert_eq!(launch.argv[0], "/opt/codex/bin/codex");
        assert_eq!(launch.argv[1], "app-server");
    }

    /// One `HostContext` is built once and serves every harness, so a path resolved for one must
    /// not reach another. Spawning the Claude binary with Codex's arguments fails as a Codex bug.
    #[tokio::test]
    async fn a_path_resolved_for_one_harness_never_reaches_another() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript::<[&str; 0], &str>([]));
        launcher.push(FakeProcess::transcript::<[&str; 0], &str>([]));
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd("/workspace")
            .client_info("test-host", "0.0.0")
            .build()
            .expect("expected a context");

        open(
            &host,
            &StdioSpec::new(["claude", "-p"]),
            &ExecutablePath::resolved("/opt/claude/bin/claude"),
            &[],
        )
        .await
        .expect("expected a transport");
        assert_eq!(
            launcher.last_launch().expect("expected a launch").argv[0],
            "/opt/claude/bin/claude"
        );

        open(
            &host,
            &StdioSpec::new(["codex", "app-server"]),
            &ExecutablePath::resolved("/opt/codex/bin/codex"),
            &[],
        )
        .await
        .expect("expected a transport");
        let second = launcher.last_launch().expect("expected a second launch");
        assert_eq!(second.argv[0], "/opt/codex/bin/codex");
        assert_eq!(second.argv[1], "app-server");
    }

    #[tokio::test]
    async fn one_message_is_one_line_in_each_direction() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::responding(|line| vec![format!("echo:{line}")]));
        let host = host(Arc::clone(&launcher));

        let transport = open(
            &host,
            &StdioSpec::new(["codex"]),
            &ExecutablePath::default(),
            &[],
        )
        .await
        .expect("expected a transport");
        let (mut sender, mut receiver) = transport.link.split();

        sender
            .send(String::from(r#"{"method":"ping"}"#))
            .await
            .expect("expected the send to land");
        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from(r#"echo:{"method":"ping"}"#))
        );
        assert_eq!(
            launcher.written(),
            vec![String::from(r#"{"method":"ping"}"#)]
        );
    }

    #[tokio::test]
    async fn a_child_that_exits_ends_the_link_rather_than_hanging() {
        let launcher = Arc::new(FakeLauncher::scripted("one\ntwo\n"));
        let host = host(Arc::clone(&launcher));

        let transport = open(
            &host,
            &StdioSpec::new(["claude"]),
            &ExecutablePath::default(),
            &[],
        )
        .await
        .expect("expected a transport");
        let (_, mut receiver) = transport.link.split();

        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from("one"))
        );
        assert_eq!(
            receiver.recv().await.expect("expected a message"),
            Some(String::from("two"))
        );
        assert_eq!(receiver.recv().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn closing_the_sender_closes_the_childs_input() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::responding(|_| Vec::new()));
        let host = host(Arc::clone(&launcher));

        let transport = open(
            &host,
            &StdioSpec::new(["claude"]),
            &ExecutablePath::default(),
            &[],
        )
        .await
        .expect("expected a transport");
        let (mut sender, mut receiver) = transport.link.split();

        sender.close().await.expect("expected the close to land");
        assert_eq!(receiver.recv().await.expect("expected the end"), None);
    }

    #[tokio::test]
    async fn an_empty_argv_is_refused_before_anything_is_spawned() {
        let launcher = Arc::new(FakeLauncher::new());
        let host = host(Arc::clone(&launcher));

        let error = open(
            &host,
            &StdioSpec::new(Vec::<String>::new()),
            &ExecutablePath::default(),
            &[],
        )
        .await
        .expect_err("expected a refusal, received a transport");
        assert!(
            matches!(error, Error::HostConfiguration { .. }),
            "expected a configuration refusal, received {error:?}"
        );
        assert!(
            launcher.launches().is_empty(),
            "expected nothing to be spawned"
        );
    }
}
