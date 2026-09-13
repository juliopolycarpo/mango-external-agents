//! Running one short-lived `claude` invocation and reading what it printed.
//!
//! Three probes exist — `--version`, `--help` and `auth status` — and all three are read-only,
//! non-secret surfaces the vendor documents. None of them takes input, so stdin is closed the
//! moment the child is up: a CLI that waits for input otherwise holds the probe open until the
//! timeout, and a probe that timed out is indistinguishable from a binary that is not there.
//!
//! Every failure lands on `None`. That is deliberate and is not the same as "the binary has no
//! options": a spawn that failed, a CLI that printed to stderr or a wrapper that swallowed the
//! output must not look like a vendor that removed everything. Callers read `None` as "not
//! established" and fall back rather than narrowing.

use mango_external_agents::{
    CancelReason, ExecutablePath, HostContext, StdioSpec, transports::stdio,
};

use crate::pinned::{PROBE_TIMEOUT, VENDOR_ENVIRONMENT_KEYS};

/// The program name every probe and every turn is spawned under.
pub const PROGRAM: &str = "claude";

/// Everything one probe wrote to stdout, joined by newlines, or nothing when it wrote nothing.
pub async fn output(
    host: &HostContext,
    executable: &ExecutablePath,
    arguments: &[&str],
) -> Option<String> {
    let mut argv = vec![String::from(PROGRAM)];
    argv.extend(arguments.iter().map(|argument| String::from(*argument)));

    let transport = stdio::open(
        host,
        &StdioSpec::new(argv),
        executable,
        VENDOR_ENVIRONMENT_KEYS,
    )
    .await
    .ok()?;
    let control = transport.control;
    let (mut sender, mut receiver) = transport.link.split();
    // A probe reads and never writes.
    let _ = sender.close().await;

    let lines = tokio::time::timeout(PROBE_TIMEOUT, async {
        let mut lines: Vec<String> = Vec::new();
        while let Ok(Some(line)) = receiver.recv().await {
            lines.push(line);
        }
        lines
    })
    .await;

    // Nothing this harness starts outlives the call that started it, including a probe that
    // answered promptly and then declined to exit.
    let _ = control.kill(CancelReason::Shutdown).await;

    let lines = lines.ok()?;
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{PROGRAM, output};
    use mango_external_agents::testing::{FakeLauncher, FakeProcess};
    use mango_external_agents::{EnvSource, ExecutablePath, HostContext};
    use std::sync::Arc;

    fn host(launcher: Arc<FakeLauncher>) -> HostContext {
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

        let printed = output(&host, &ExecutablePath::default(), &["--version"]).await;
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
            output(&host, &ExecutablePath::default(), &["--help"]).await,
            None
        );
    }

    #[tokio::test]
    async fn a_spawn_that_failed_is_not_established_either() {
        let launcher = Arc::new(FakeLauncher::new());
        let host = host(Arc::clone(&launcher));

        assert_eq!(
            output(&host, &ExecutablePath::default(), &["--version"]).await,
            None,
            "expected a launcher refusal to read as unestablished rather than to fail the probe"
        );
    }

    #[tokio::test]
    async fn a_probe_child_sees_only_what_the_allowlist_passes() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["2.1.270 (Claude Code)"]));
        let host = host(Arc::clone(&launcher));

        output(&host, &ExecutablePath::default(), &["--version"]).await;

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
        .await;

        let launch = launcher.last_launch().expect("expected a launch");
        assert_eq!(launch.argv[0], "/opt/claude/bin/claude");
    }
}
