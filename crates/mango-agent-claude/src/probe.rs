//! Running one short-lived `claude` invocation and reading what it printed.
//!
//! Three probes exist — `--version`, `--help` and `auth status` — and all three are read-only,
//! non-secret surfaces the vendor documents. None of them takes input, so stdin is closed the
//! moment the child is up: a CLI that waits for input otherwise holds the probe open until the
//! timeout, and a probe that timed out is indistinguishable from a binary that is not there.
//!
//! Ordinary probe failures land on `None`. That is deliberate and is not the same as "the binary
//! has no options": a spawn that failed, a CLI that printed to stderr or a wrapper that swallowed
//! the output must not look like a vendor that removed everything. `Error::CleanupRequired` is the
//! exception: the host must receive its process control rather than silently losing a child it has
//! to reconcile. Callers read `None` as "not established" and fall back rather than narrowing.

use mango_external_agents::{
    CancelReason, ExecutablePath, HostContext, LinkReceiver, ProcessCleanupGuard, Result,
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
/// treated exactly like a read error: not established.
const OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;

/// Everything one probe wrote to stdout, joined by newlines, or nothing when it wrote nothing.
pub async fn output(
    host: &HostContext,
    executable: &ExecutablePath,
    arguments: &[&str],
) -> Result<Option<String>> {
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
        Err(_) => return Ok(None),
    };
    let cleanup =
        ProcessCleanupGuard::new(transport.control, *host.limits(), CancelReason::Shutdown);
    let (mut sender, mut receiver) = transport.link.split();
    // A probe reads and never writes.
    let _ = sender.close().await;

    let printed = tokio::time::timeout(
        PROBE_TIMEOUT,
        collect(receiver.as_mut(), OUTPUT_LIMIT_BYTES),
    )
    .await;

    // Nothing this harness starts outlives the call that started it, including a probe that
    // answered promptly and then declined to exit. A caller cancelled while reading drops the
    // guard, which starts the same bounded cleanup worker.
    cleanup.finish().await?;

    Ok(match printed {
        Ok(Collected::Whole(text)) => text,
        Ok(Collected::Unknown) | Err(_) => None,
    })
}

/// What reading a probe's stdout established.
#[derive(Debug, PartialEq, Eq)]
enum Collected {
    /// The child closed stdout: every line it wrote, joined by newlines, or nothing when it wrote
    /// no line at all.
    Whole(Option<String>),
    /// The output may be incomplete: a read failed or the output outgrew its cap. Parsing what
    /// arrived would report a cut-off listing as a CLI that lacks features.
    Unknown,
}

/// Reads `receiver` to its end, keeping at most `limit` bytes.
///
/// Lines are appended to one string as they arrive, so the output is held once rather than as a
/// list of lines and then again as their join. A read error and output past `limit` both answer
/// [`Collected::Unknown`] and stop reading at once; only a closed stdout answers
/// [`Collected::Whole`].
async fn collect(receiver: &mut dyn LinkReceiver, limit: usize) -> Collected {
    let mut text = String::new();
    let mut any = false;
    loop {
        let line = match receiver.recv().await {
            Ok(Some(line)) => line,
            Ok(None) => return Collected::Whole(any.then_some(text)),
            Err(_) => return Collected::Unknown,
        };
        let separator = usize::from(any);
        if text.len() + separator + line.len() > limit {
            return Collected::Unknown;
        }
        if any {
            text.push('\n');
        }
        text.push_str(&line);
        any = true;
    }
}

#[cfg(test)]
mod tests {
    use super::{Collected, OUTPUT_LIMIT_BYTES, PROGRAM, collect, output};
    use mango_external_agents::testing::{FakeLauncher, FakeProcess};
    use mango_external_agents::{
        CancelReason, EnvSource, Error, ExecutablePath, ExitStatus, HostContext, LaunchSpec,
        Limits, LineLimits, LinkReceiver, ManagedProcess, ProcessControl, ProcessLauncher, Result,
    };
    use std::collections::VecDeque;
    use std::sync::Arc;

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
        let help = include_str!("../../../fixtures/claude/help/2.1.270.txt");
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

    #[tokio::test]
    async fn collect_joins_lines_with_newlines_and_no_trailing_one() {
        let mut receiver = ScriptedReceiver::lines(["a", "b", "c"]);
        assert_eq!(
            collect(&mut receiver, 64).await,
            Collected::Whole(Some(String::from("a\nb\nc")))
        );
    }

    #[tokio::test]
    async fn collect_tells_no_lines_from_one_empty_line() {
        assert_eq!(
            collect(&mut ScriptedReceiver::lines([]), 64).await,
            Collected::Whole(None),
            "expected a child that wrote nothing to yield no text"
        );
        assert_eq!(
            collect(&mut ScriptedReceiver::lines([""]), 64).await,
            Collected::Whole(Some(String::new())),
            "expected one empty line to stay distinct from no output"
        );
    }

    #[tokio::test]
    async fn collect_reads_a_link_error_as_unknown_not_as_the_end() {
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

        assert_eq!(
            collect(&mut receiver, 64).await,
            Collected::Unknown,
            "expected a read error after a valid prefix to be unknown"
        );
        assert_eq!(receiver.reads, 2, "expected reading to stop at the error");
    }

    #[tokio::test]
    async fn collect_accepts_output_of_exactly_the_cap_and_refuses_one_byte_more() {
        // "aa\nbb" is 5 bytes: two lines and the newline that joins them.
        assert_eq!(
            collect(&mut ScriptedReceiver::lines(["aa", "bb"]), 5).await,
            Collected::Whole(Some(String::from("aa\nbb")))
        );
        assert_eq!(
            collect(&mut ScriptedReceiver::lines(["aa", "bbb"]), 5).await,
            Collected::Unknown,
            "expected 6 bytes to be refused by a 5-byte cap"
        );
    }

    #[tokio::test]
    async fn collect_stops_reading_once_the_cap_is_passed() {
        let mut receiver = ScriptedReceiver::lines(["aaaa", "bbbb", "cccc", "dddd"]);

        assert_eq!(collect(&mut receiver, 6).await, Collected::Unknown);
        assert_eq!(
            receiver.reads, 2,
            "expected the reader to stop at the first line that broke the cap"
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
