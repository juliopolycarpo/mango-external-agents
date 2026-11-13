//! Running one short-lived `claude` invocation and reading what it printed.
//!
//! Three probes exist — `--version`, `--help` and `auth status` — and all three are read-only,
//! non-secret surfaces the vendor documents. None of them takes input, so stdin is closed the
//! moment the child is up, so a CLI waiting for input does not hold the probe open.
//!
//! A launch refusal establishes that the child did not start. A started child that printed
//! nothing still proves the CLI was present, but has no output to parse. `Probed::Whole` is
//! output the child finished writing; `Probed::Incomplete` keeps the complete lines before a
//! read error, size cap or timeout. A cut-off `--help` must not look like a vendor that removed
//! options, so `output` accepts only `Whole`. Callers that can use partial lines, such as a
//! version banner, use the crate-private `read`. Cleanup failures and other host errors reach
//! the caller with their original controls and dispatch annotations.

use mango_external_agents::{
    CancelReason, Error, ExecutablePath, HostContext, LinkReceiver, ProcessCleanupGuard, Result,
    StdioSpec, transports::stdio,
};

use crate::pinned::{PROBE_TIMEOUT, VENDOR_ENVIRONMENT_KEYS};

/// The program name every probe and every turn is spawned under.
pub const PROGRAM: &str = "claude";

/// The most stdout one probe may return, in bytes: lines plus the newlines that join them.
///
/// The largest captured `--help` is 21,401 bytes (`fixtures/claude/help/2.1.270.txt`), so 1 MiB
/// leaves a margin of about 49x for a CLI that grows its help, while a wrapper that streams
/// without end stops being held in memory long before the probe timeout. Output past the cap is
/// treated exactly like a read error: incomplete.
const OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;

/// What one probe established about the child's stdout.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Probed {
    /// The launcher could not start the child.
    NotStarted,
    /// The child started but produced no complete lines.
    StartedEmpty,
    /// A read error, the size cap or the timeout cut the output short. Holds the complete lines
    /// that arrived before, joined by newlines; empty when the failure came before the first line.
    Incomplete(String),
    /// The child closed stdout: every line it wrote, joined by newlines.
    Whole(String),
}

impl Probed {
    /// The complete lines that arrived, or an empty string if the child started silently.
    ///
    /// For a caller whose answer does not depend on what came after them: a version banner names
    /// its version on a line that either arrived whole or did not, and a status document is either
    /// a complete JSON object or is not parsed at all.
    pub(crate) fn lines(self) -> Option<String> {
        match self {
            Self::NotStarted => None,
            Self::StartedEmpty => Some(String::new()),
            Self::Incomplete(text) | Self::Whole(text) => Some(text),
        }
    }
}

/// Everything one probe wrote to stdout, joined by newlines, or nothing when it wrote nothing or
/// the output is not known to be complete.
///
/// # Example
///
/// ```no_run
/// # async fn example(host: &mango_external_agents::HostContext) -> mango_external_agents::Result<()> {
/// use mango_agent_claude::probe::output;
/// use mango_external_agents::ExecutablePath;
///
/// let help = output(host, &ExecutablePath::default(), &["--help"]).await?;
/// # let _ = help;
/// # Ok(())
/// # }
/// ```
pub async fn output(
    host: &HostContext,
    executable: &ExecutablePath,
    arguments: &[&str],
) -> Result<Option<String>> {
    Ok(match read(host, executable, arguments).await? {
        Probed::Whole(text) => Some(text),
        Probed::NotStarted | Probed::StartedEmpty | Probed::Incomplete(_) => None,
    })
}

/// Runs one probe and says how much of its output can be trusted.
pub(crate) async fn read(
    host: &HostContext,
    executable: &ExecutablePath,
    arguments: &[&str],
) -> Result<Probed> {
    let mut argv = vec![String::from(PROGRAM)];
    argv.extend(arguments.iter().map(|argument| String::from(*argument)));

    let transport = match stdio::open(
        host,
        &StdioSpec::new(argv),
        executable,
        VENDOR_ENVIRONMENT_KEYS,
    )
    .await
    {
        Ok(transport) => transport,
        Err(error) if error.cleanup_control().is_some() => return Err(error),
        Err(error) if matches!(error.cause(), Error::Launch { .. }) => {
            return Ok(Probed::NotStarted);
        }
        Err(error) => return Err(error),
    };
    let cleanup =
        ProcessCleanupGuard::new(transport.control, *host.limits(), CancelReason::Shutdown);
    let (mut sender, mut receiver) = transport.link.split();
    // A probe reads and never writes.
    let _ = sender.close().await;

    // The lines live outside the timed future so that a timeout keeps the ones that arrived.
    let mut captured = Captured::default();
    let end = tokio::time::timeout(
        PROBE_TIMEOUT,
        collect(receiver.as_mut(), OUTPUT_LIMIT_BYTES, &mut captured),
    )
    .await;

    // Nothing this harness starts outlives the call that started it, including a probe that
    // answered promptly and then declined to exit. A caller cancelled while reading drops the
    // guard, which starts the same bounded cleanup worker.
    cleanup.finish().await?;

    Ok(match end {
        Ok(End::Closed) => captured.whole(),
        Ok(End::Failed | End::OverCap) => Probed::Incomplete(captured.text),
        Err(_) if captured.any => Probed::Incomplete(captured.text),
        Err(_) => Probed::StartedEmpty,
    })
}

/// The complete lines read so far, joined by newlines.
#[derive(Debug, Default)]
struct Captured {
    text: String,
    /// Whether any line arrived, which an empty `text` cannot say: one empty line is output.
    any: bool,
}

impl Captured {
    /// What a closed stdout established.
    fn whole(self) -> Probed {
        if self.any {
            Probed::Whole(self.text)
        } else {
            Probed::StartedEmpty
        }
    }
}

/// Why [`collect`] stopped reading.
#[derive(Debug, PartialEq, Eq)]
enum End {
    /// The child closed stdout.
    Closed,
    /// A read failed.
    Failed,
    /// The next line would have taken the output past the cap.
    OverCap,
}

/// Reads `receiver` until it ends, fails or would pass `limit` bytes, appending to `captured`.
///
/// Lines are appended to one string as they arrive, so the output is held once rather than as a
/// list of lines and then again as their join. Only complete lines are ever appended, so `captured`
/// stays valid whichever way this stops, including when the caller drops it at a timeout.
async fn collect(receiver: &mut dyn LinkReceiver, limit: usize, captured: &mut Captured) -> End {
    loop {
        let line = match receiver.recv().await {
            Ok(Some(line)) => line,
            Ok(None) => return End::Closed,
            Err(_) => return End::Failed,
        };
        let separator = usize::from(captured.any);
        if captured.text.len() + separator + line.len() > limit {
            return End::OverCap;
        }
        if captured.any {
            captured.text.push('\n');
        }
        captured.text.push_str(&line);
        captured.any = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{Captured, End, OUTPUT_LIMIT_BYTES, PROGRAM, Probed, collect, output, read};
    use mango_external_agents::testing::{FakeLauncher, FakeProcess};
    use mango_external_agents::{
        ByteSink, CancelReason, Dispatch, EnvSource, Error, ExecutablePath, ExitStatus,
        HostContext, LaunchSpec, Limits, LineLimits, LinkReceiver, ManagedProcess, ProcessControl,
        ProcessLauncher, Result,
    };
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    fn host<L: ProcessLauncher + 'static>(launcher: Arc<L>) -> HostContext {
        HostContext::builder()
            .launcher(launcher)
            .cwd(std::env::temp_dir())
            .environment(EnvSource::from_pairs([
                ("PATH", "/usr/bin"),
                ("CLAUDE_CONFIG_DIR", "/home/ada/.claude"),
                ("MANGO_HUB_TOKEN", "never-forward-this"),
            ]))
            .client_info("mea-tests", "0.0.0")
            .build()
            .expect("expected a host")
    }

    #[tokio::test]
    async fn reads_everything_one_probe_printed() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["2.1.270 (Claude Code)"]));
        let host = host(Arc::clone(&launcher));

        let printed = output(&host, &ExecutablePath::default(), &["--version"])
            .await
            .expect("expected a completed probe");
        assert_eq!(printed.as_deref(), Some("2.1.270 (Claude Code)"));

        let launch = launcher.last_launch().expect("expected a launch");
        assert_eq!(launch.argv, vec![PROGRAM, "--version"]);
    }

    #[tokio::test]
    async fn a_probe_that_printed_nothing_is_not_established() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(Vec::<String>::new()));
        let host = host(Arc::clone(&launcher));

        assert_eq!(
            output(&host, &ExecutablePath::default(), &["--help"])
                .await
                .expect("expected a completed probe"),
            None
        );
    }

    /// A host whose line caps are narrowed to something a test can reach cheaply.
    fn host_with_line_cap<L: ProcessLauncher + 'static>(
        launcher: Arc<L>,
        max_line_bytes: usize,
    ) -> HostContext {
        HostContext::builder()
            .launcher(launcher)
            .cwd(std::env::temp_dir())
            .environment(EnvSource::from_pairs([("PATH", "/usr/bin")]))
            .client_info("mea-tests", "0.0.0")
            .limits(Limits {
                line: LineLimits {
                    max_line_bytes,
                    max_buffered_bytes: 4 * max_line_bytes,
                },
                ..Limits::default()
            })
            .build()
            .expect("expected a host")
    }

    #[tokio::test]
    async fn a_read_error_after_a_valid_prefix_is_not_established() {
        let launcher = Arc::new(FakeLauncher::new());
        // The oversized line is what makes the reader fail: `LineStream` refuses it mid-output.
        launcher.push(FakeProcess::transcript([
            String::from("Usage: claude [options]"),
            String::from("  --print   Print and exit"),
            "x".repeat(2048),
            String::from("  --output-format <format>"),
        ]));
        let host = host_with_line_cap(Arc::clone(&launcher), 1024);

        let printed = output(&host, &ExecutablePath::default(), &["--help"])
            .await
            .expect("expected a completed probe");

        assert_eq!(
            printed, None,
            "expected output cut off by a read error to be unknown | received {printed:?}"
        );
        assert_eq!(
            launcher.live_children(),
            0,
            "expected the probe child to be cleaned up after a read error"
        );
    }

    #[tokio::test]
    async fn output_over_the_total_cap_is_not_established_and_still_cleaned_up() {
        let launcher = Arc::new(FakeLauncher::new());
        let line = "y".repeat(1000);
        let lines = OUTPUT_LIMIT_BYTES / line.len() + 2;
        launcher.push(FakeProcess::transcript(vec![line; lines]));
        let host = host(Arc::clone(&launcher));

        let printed = output(&host, &ExecutablePath::default(), &["--help"])
            .await
            .expect("expected a completed probe");

        assert_eq!(
            printed.as_ref().map(String::len),
            None,
            "expected {lines} legal lines over the {OUTPUT_LIMIT_BYTES}-byte cap to be unknown | \
             received {:?} bytes",
            printed.as_ref().map(String::len)
        );
        assert_eq!(
            launcher.live_children(),
            0,
            "expected the probe child to be cleaned up after an oversized answer"
        );
    }

    #[tokio::test]
    async fn the_captured_help_fits_the_cap_with_room_to_spare() {
        let help = include_str!("../fixtures/claude/help/2.1.270.txt");
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(help.lines()));
        let host = host(Arc::clone(&launcher));

        let printed = output(&host, &ExecutablePath::default(), &["--help"])
            .await
            .expect("expected a completed probe");

        assert_eq!(
            printed.as_deref(),
            Some(help.trim_end_matches('\n')),
            "expected the largest captured help ({} bytes) to be returned whole",
            help.len()
        );
        assert!(
            OUTPUT_LIMIT_BYTES >= 32 * help.len(),
            "expected at least a 32x margin over the largest captured help | received cap \
             {OUTPUT_LIMIT_BYTES} for {} bytes",
            help.len()
        );
    }

    /// Wraps a fake launcher so that ending the child fails, which makes an awaited cleanup
    /// observable: only `cleanup.finish()` reports the failure back to the caller.
    struct FailingKillLauncher(Arc<FakeLauncher>);

    #[async_trait::async_trait]
    impl ProcessLauncher for FailingKillLauncher {
        async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
            let mut child = self.0.spawn(spec).await?;
            child.control = Arc::new(FailingKill {
                inner: child.control,
            });
            Ok(child)
        }
    }

    struct FailingKill {
        inner: Arc<dyn ProcessControl>,
    }

    #[async_trait::async_trait]
    impl ProcessControl for FailingKill {
        fn pid(&self) -> Option<u32> {
            self.inner.pid()
        }

        fn stderr_tail(&self) -> String {
            self.inner.stderr_tail()
        }

        async fn wait(&self) -> Result<ExitStatus> {
            self.inner.wait().await
        }

        async fn kill(&self, _reason: CancelReason) -> Result<()> {
            Err(Error::Closed {
                subject: "test probe process",
            })
        }
    }

    #[tokio::test]
    async fn cleanup_is_awaited_when_the_output_is_refused() {
        let fake = Arc::new(FakeLauncher::new());
        let line = "y".repeat(1000);
        fake.push(FakeProcess::transcript(vec![
            line;
            OUTPUT_LIMIT_BYTES / 1000 + 2
        ]));
        let host = host(Arc::new(FailingKillLauncher(Arc::clone(&fake))));

        let received = output(&host, &ExecutablePath::default(), &["--help"]).await;

        assert!(
            matches!(&received, Err(error) if error.cleanup_control().is_some()),
            "expected the refused probe to await cleanup and surface its failure | received {received:?}"
        );
    }

    #[tokio::test]
    async fn cleanup_is_awaited_when_a_read_fails() {
        let fake = Arc::new(FakeLauncher::new());
        fake.push(FakeProcess::transcript([
            String::from("Usage: claude"),
            "x".repeat(2048),
        ]));
        let host = host_with_line_cap(Arc::new(FailingKillLauncher(Arc::clone(&fake))), 1024);

        let received = output(&host, &ExecutablePath::default(), &["--help"]).await;

        assert!(
            matches!(&received, Err(error) if error.cleanup_control().is_some()),
            "expected the failed probe to await cleanup and surface its failure | received {received:?}"
        );
    }

    /// A receiver that replays scripted results and counts how many it was asked for.
    struct ScriptedReceiver {
        script: VecDeque<Result<Option<String>>>,
        reads: usize,
    }

    impl ScriptedReceiver {
        fn lines<const N: usize>(lines: [&str; N]) -> Self {
            Self {
                script: lines
                    .into_iter()
                    .map(|line| Ok(Some(String::from(line))))
                    .chain([Ok(None)])
                    .collect(),
                reads: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl LinkReceiver for ScriptedReceiver {
        async fn recv(&mut self) -> Result<Option<String>> {
            self.reads += 1;
            self.script.pop_front().unwrap_or(Ok(None))
        }
    }

    /// Runs `collect` to whatever end it reaches and returns that end with what it captured.
    async fn gather(receiver: &mut dyn LinkReceiver, limit: usize) -> (End, Captured) {
        let mut captured = Captured::default();
        let end = collect(receiver, limit, &mut captured).await;
        (end, captured)
    }

    #[tokio::test]
    async fn collect_joins_lines_with_newlines_and_no_trailing_one() {
        let (end, captured) = gather(&mut ScriptedReceiver::lines(["a", "b", "c"]), 64).await;
        assert_eq!(end, End::Closed);
        assert_eq!(captured.whole(), Probed::Whole(String::from("a\nb\nc")));
    }

    #[tokio::test]
    async fn collect_tells_no_lines_from_one_empty_line() {
        let (_, none) = gather(&mut ScriptedReceiver::lines([]), 64).await;
        assert_eq!(
            none.whole(),
            Probed::StartedEmpty,
            "expected a child that wrote nothing to yield no text"
        );
        let (_, empty) = gather(&mut ScriptedReceiver::lines([""]), 64).await;
        assert_eq!(
            empty.whole(),
            Probed::Whole(String::new()),
            "expected one empty line to stay distinct from no output"
        );
    }

    #[tokio::test]
    async fn collect_reads_a_link_error_as_a_failure_and_keeps_the_lines_before_it() {
        let mut receiver = ScriptedReceiver {
            script: VecDeque::from([
                Ok(Some(String::from("Usage: claude"))),
                Err(Error::Link {
                    peer: String::from("Claude Code"),
                    message: String::from("stdout closed under the reader"),
                }),
                Ok(Some(String::from("never read"))),
            ]),
            reads: 0,
        };

        let (end, captured) = gather(&mut receiver, 64).await;

        assert_eq!(
            end,
            End::Failed,
            "expected a read error after a valid prefix to be a failure"
        );
        assert_eq!(captured.text, "Usage: claude");
        assert_eq!(receiver.reads, 2, "expected reading to stop at the error");
    }

    #[tokio::test]
    async fn collect_accepts_output_of_exactly_the_cap_and_refuses_one_byte_more() {
        // "aa\nbb" is 5 bytes: two lines and the newline that joins them.
        let (end, captured) = gather(&mut ScriptedReceiver::lines(["aa", "bb"]), 5).await;
        assert_eq!(end, End::Closed);
        assert_eq!(captured.text, "aa\nbb");

        let (end, captured) = gather(&mut ScriptedReceiver::lines(["aa", "bbb"]), 5).await;
        assert_eq!(
            end,
            End::OverCap,
            "expected 6 bytes to be refused by a 5-byte cap"
        );
        assert_eq!(
            captured.text, "aa",
            "expected the lines before the cap to be kept"
        );
    }

    #[tokio::test]
    async fn collect_stops_reading_once_the_cap_is_passed() {
        let mut receiver = ScriptedReceiver::lines(["aaaa", "bbbb", "cccc", "dddd"]);

        let (end, _) = gather(&mut receiver, 6).await;

        assert_eq!(end, End::OverCap);
        assert_eq!(
            receiver.reads, 2,
            "expected the reader to stop at the first line that broke the cap"
        );
    }

    /// A receiver that delivers its lines and then never speaks again.
    struct StalledReceiver {
        lines: VecDeque<String>,
    }

    #[async_trait::async_trait]
    impl LinkReceiver for StalledReceiver {
        async fn recv(&mut self) -> Result<Option<String>> {
            match self.lines.pop_front() {
                Some(line) => Ok(Some(line)),
                None => std::future::pending().await,
            }
        }
    }

    #[tokio::test]
    async fn a_timeout_keeps_the_lines_that_arrived_before_it() {
        let mut receiver = StalledReceiver {
            lines: VecDeque::from([String::from("2.1.270 (Claude Code)")]),
        };
        let mut captured = Captured::default();

        let end = tokio::time::timeout(
            std::time::Duration::from_millis(20),
            collect(&mut receiver, 64, &mut captured),
        )
        .await;

        assert!(end.is_err(), "expected a stalled child to time out");
        assert_eq!(
            captured.text, "2.1.270 (Claude Code)",
            "expected the banner read before the stall to survive the timeout"
        );
    }

    #[tokio::test]
    async fn read_keeps_the_complete_lines_before_a_read_error() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript([
            String::from("2.1.270 (Claude Code)"),
            "x".repeat(2048),
        ]));
        let host = host_with_line_cap(Arc::clone(&launcher), 1024);

        let probed = read(&host, &ExecutablePath::default(), &["--version"])
            .await
            .expect("expected a completed probe");

        assert_eq!(
            probed,
            Probed::Incomplete(String::from("2.1.270 (Claude Code)"))
        );
    }

    #[tokio::test]
    async fn read_reports_a_failure_before_any_line_as_incomplete_and_empty() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["x".repeat(2048)]));
        let host = host_with_line_cap(Arc::clone(&launcher), 1024);

        let probed = read(&host, &ExecutablePath::default(), &["--version"])
            .await
            .expect("expected a completed probe");

        assert_eq!(probed, Probed::Incomplete(String::new()));
    }

    #[tokio::test]
    async fn a_silent_child_proves_the_probe_started() {
        let silent = Arc::new(FakeLauncher::new());
        silent.push(FakeProcess::transcript(Vec::<String>::new()));
        let result = read(
            &host(Arc::clone(&silent)),
            &ExecutablePath::default(),
            &["--version"],
        )
        .await
        .expect("expected a completed probe");
        assert_eq!(
            result.lines(),
            Some(String::new()),
            "expected a started child with no output to retain evidence of launch"
        );
        assert_eq!(silent.live_children(), 0);
    }

    fn no_probe_response(_: &str) -> Vec<String> {
        Vec::new()
    }

    struct PendingProbeLauncher(Arc<FakeLauncher>);

    struct PendingProbeStdin;

    #[async_trait::async_trait]
    impl ByteSink for PendingProbeStdin {
        async fn write_all(&mut self, _: &[u8]) -> Result<()> {
            Ok(())
        }
        async fn close(&mut self) -> Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl ProcessLauncher for PendingProbeLauncher {
        async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
            let mut child = self.0.spawn(spec).await?;
            assert_eq!(self.0.live_children(), 1, "expected a live silent child");
            child.stdin = Some(Box::new(PendingProbeStdin));
            Ok(child)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_timeout_proves_the_probe_started() {
        let fake = Arc::new(FakeLauncher::new());
        fake.push(FakeProcess::responding(no_probe_response));
        let result = read(
            &host(Arc::new(PendingProbeLauncher(Arc::clone(&fake)))),
            &ExecutablePath::default(),
            &["--version"],
        )
        .await
        .expect("expected a timed-out probe to complete cleanup");
        assert_eq!(
            result.lines(),
            Some(String::new()),
            "expected a silent timeout to retain evidence of launch"
        );
        assert_eq!(
            fake.live_children(),
            0,
            "expected the live child to be killed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn public_output_distinguishes_silence_from_an_empty_line() {
        let fake = Arc::new(FakeLauncher::new());
        fake.push(FakeProcess::responding(no_probe_response));
        let silent = output(
            &host(Arc::new(PendingProbeLauncher(Arc::clone(&fake)))),
            &ExecutablePath::default(),
            &["--version"],
        )
        .await
        .expect("expected silent probe cleanup");
        assert_eq!(
            silent, None,
            "expected no complete line from a silent timeout"
        );
        assert_eq!(fake.live_children(), 0);

        let empty_line = Arc::new(FakeLauncher::new());
        empty_line.push(FakeProcess::transcript([""]));
        let printed = output(
            &host(empty_line),
            &ExecutablePath::default(),
            &["--version"],
        )
        .await
        .expect("expected complete empty line");
        assert_eq!(
            printed.as_deref(),
            Some(""),
            "expected one complete empty line"
        );
    }

    struct PolicyRefusalLauncher;

    #[async_trait::async_trait]
    impl ProcessLauncher for PolicyRefusalLauncher {
        async fn spawn(&self, _: LaunchSpec) -> Result<ManagedProcess> {
            Err(Error::HostConfiguration {
                expected: "an authorized working directory",
                received: String::from("unauthorized directory"),
            }
            .with_dispatch(Dispatch::NotSubmitted))
        }
    }

    #[tokio::test]
    async fn a_wrapped_host_policy_refusal_reaches_the_caller() {
        let error = read(
            &host(Arc::new(PolicyRefusalLauncher)),
            &ExecutablePath::default(),
            &["--version"],
        )
        .await
        .expect_err("expected the host policy refusal");
        assert!(
            matches!(error.cause(), Error::HostConfiguration {
            expected: "an authorized working directory", received
        } if received == "unauthorized directory"),
            "expected the host policy refusal | received {error:?}"
        );
        assert_eq!(
            error.dispatch(),
            Dispatch::NotSubmitted,
            "expected the host dispatch annotation | received {error:?}"
        );
    }

    struct CleanupRequiredLaunch {
        inner: Arc<FakeLauncher>,
        control: Mutex<Option<Arc<dyn ProcessControl>>>,
    }

    #[async_trait::async_trait]
    impl ProcessLauncher for CleanupRequiredLaunch {
        async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
            let child = self.inner.spawn(spec).await?;
            *self.control.lock().expect("expected control lock") = Some(Arc::clone(&child.control));
            Err(Error::CleanupRequired {
                control: child.control,
                source: Box::new(Error::Launch {
                    program: String::from("claude"),
                    message: String::from("failed after launch"),
                }),
            }
            .with_dispatch(Dispatch::NotSubmitted))
        }
    }

    #[tokio::test]
    async fn cleanup_control_takes_precedence_over_a_wrapped_launch_error() {
        let fake = Arc::new(FakeLauncher::new());
        fake.push(FakeProcess::responding(no_probe_response));
        let launcher = Arc::new(CleanupRequiredLaunch {
            inner: Arc::clone(&fake),
            control: Mutex::new(None),
        });
        let error = read(
            &host(Arc::clone(&launcher)),
            &ExecutablePath::default(),
            &["--version"],
        )
        .await
        .expect_err("expected the host-owned cleanup refusal");
        assert!(
            matches!(error.cause(), Error::Launch { .. }),
            "expected the underlying launch error | received {error:?}"
        );
        assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
        let carried = error
            .cleanup_control()
            .expect("expected the host cleanup control");
        let original = launcher
            .control
            .lock()
            .expect("expected control lock")
            .clone()
            .expect("expected the original control");
        assert!(
            Arc::ptr_eq(&carried, &original),
            "expected the original host control"
        );
        assert_eq!(
            fake.live_children(),
            1,
            "expected the child to await host cleanup"
        );
        carried
            .kill(CancelReason::Shutdown)
            .await
            .expect("expected host cleanup");
        carried.wait().await.expect("expected host reap");
        assert_eq!(
            fake.live_children(),
            0,
            "expected host cleanup to end the child"
        );
    }

    struct WrappedMissingLauncher;

    #[async_trait::async_trait]
    impl ProcessLauncher for WrappedMissingLauncher {
        async fn spawn(&self, _: LaunchSpec) -> Result<ManagedProcess> {
            Err(Error::Launch {
                program: String::from("claude"),
                message: String::from("missing"),
            }
            .with_dispatch(Dispatch::NotSubmitted))
        }
    }

    #[tokio::test]
    async fn a_wrapped_launch_error_means_the_probe_did_not_start() {
        let result = read(
            &host(Arc::new(WrappedMissingLauncher)),
            &ExecutablePath::default(),
            &["--version"],
        )
        .await
        .expect("expected an unavailable probe");
        assert_eq!(
            result.lines(),
            None,
            "expected a failed spawn to have no started child"
        );
    }

    #[tokio::test]
    async fn a_spawn_that_failed_is_not_established_either() {
        let launcher = Arc::new(FakeLauncher::new());
        let host = host(Arc::clone(&launcher));

        assert_eq!(
            output(&host, &ExecutablePath::default(), &["--version"])
                .await
                .expect("expected an unavailable probe"),
            None,
            "expected a launcher refusal to read as unestablished rather than to fail the probe"
        );
    }

    #[tokio::test]
    async fn a_probe_child_sees_only_what_the_allowlist_passes() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["2.1.270 (Claude Code)"]));
        let host = host(Arc::clone(&launcher));

        output(&host, &ExecutablePath::default(), &["--version"])
            .await
            .expect("expected a completed probe");

        let launch = launcher.last_launch().expect("expected a launch");
        assert_eq!(launch.env.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(
            launch.env.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/home/ada/.claude"),
            "expected the vendor's own documented variable to survive"
        );
        assert_eq!(
            launch.env.get("MANGO_HUB_TOKEN"),
            None,
            "expected the host's own secret never to reach a vendor child"
        );
    }

    #[tokio::test]
    async fn spawns_the_executable_the_host_resolved_rather_than_the_bare_name() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["2.1.270 (Claude Code)"]));
        let host = host(Arc::clone(&launcher));

        output(
            &host,
            &ExecutablePath::resolved("/opt/claude/bin/claude"),
            &["--version"],
        )
        .await
        .expect("expected a completed probe");

        let launch = launcher.last_launch().expect("expected a launch");
        assert_eq!(launch.argv[0], "/opt/claude/bin/claude");
    }
}
