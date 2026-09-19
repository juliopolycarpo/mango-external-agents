//! Discovery, sessions and turns, against a scripted `claude`.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mango_agent_claude::ClaudeHarness;
use mango_external_agents::testing::FrozenClock;
use mango_external_agents::{
    ApprovalRouting, AuthMode, AuthState, CancelReason, CancelToken, CloseReason, Configuration,
    ConfigurationChange, ConfigurationOptionId, ConfigurationPatch, ConfigurationValue,
    DiscoveryReceipt, DiscoveryReceiptMeasurements, Dispatch, Error, EventKind, ExecutablePath,
    GateVerdict, Harness, HarnessId, HostContext, InteractionId, LaunchSpec, Limits, LineLimits,
    ManagedProcess, OpenSession, PermissionLevel, PermissionResponse, ProcessControl,
    ProcessLauncher, Result, ResumeMode, Session, SessionStatus, TurnRequest, TurnStream,
};
use support::{
    FakeClaudeCli, HELP_2_1_227, HELP_2_1_270, READ_TURN, Run, SIGNED_OUT, SpawnGate, host,
    host_under, value_after,
};

/// A session several tasks can hold, for the tests that race two of its methods.
async fn shared(launcher: &Arc<FakeClaudeCli>) -> Arc<dyn Session> {
    Arc::from(open(launcher).await)
}

/// Everything a turn produced, up to and including its terminal event.
async fn drain(turn: &mut TurnStream) -> Vec<EventKind> {
    let collected = tokio::time::timeout(Duration::from_secs(5), async {
        let mut events = Vec::new();
        while let Some(event) = turn.recv().await {
            let terminal = event.is_terminal();
            events.push(event.kind);
            if terminal {
                break;
            }
        }
        events
    })
    .await;
    collected.expect("expected the turn to end rather than hang")
}

async fn open(launcher: &Arc<FakeClaudeCli>) -> Box<dyn Session> {
    let host = host(Arc::clone(launcher));
    ClaudeHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session")
}

struct FailingTurnLauncher {
    inner: FakeClaudeCli,
    first_turn: AtomicBool,
    arrived: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl FailingTurnLauncher {
    fn new() -> Self {
        Self {
            inner: FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])),
            first_turn: AtomicBool::new(true),
            arrived: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        }
    }

    async fn wait_for_first_turn(&self) {
        self.arrived.notified().await;
    }

    fn release_first_turn(&self) {
        self.release.notify_one();
    }
}

#[async_trait::async_trait]
impl ProcessLauncher for FailingTurnLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        if !spec.argv.iter().any(|argument| argument == "--print") {
            return self.inner.spawn(spec).await;
        }
        if self.first_turn.swap(false, Ordering::AcqRel) {
            self.arrived.notify_one();
            self.release.notified().await;
            return Err(Error::Launch {
                program: String::from("claude"),
                message: String::from("injected turn launch failure"),
            });
        }
        self.inner.spawn(spec).await
    }
}

/// Makes a probe's failed native cleanup visible to the harness caller.
struct CleanupRequiredProbeLauncher {
    inner: Arc<FakeClaudeCli>,
}

#[async_trait::async_trait]
impl ProcessLauncher for CleanupRequiredProbeLauncher {
    async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
        let mut child = self.inner.spawn(spec).await?;
        child.stdin = None;
        child.control = Arc::new(FailingProbeCleanup {
            inner: child.control,
        });
        Ok(child)
    }
}

/// Refuses forced termination so the host must retain its process control.
struct FailingProbeCleanup {
    inner: Arc<dyn ProcessControl>,
}

#[async_trait::async_trait]
impl ProcessControl for FailingProbeCleanup {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn stderr_tail(&self) -> String {
        self.inner.stderr_tail()
    }

    async fn wait(&self) -> Result<mango_external_agents::ExitStatus> {
        self.inner.wait().await
    }

    async fn kill(&self, _reason: CancelReason) -> Result<()> {
        Err(Error::Closed {
            subject: "test probe process",
        })
    }
}

mod discovery {
    use super::*;

    #[tokio::test]
    async fn propagates_probe_cleanup_required_through_discovery_and_opening() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let host = host_under(
            Arc::new(CleanupRequiredProbeLauncher {
                inner: Arc::clone(&launcher),
            }),
            Limits::default(),
        );

        let discovery = ClaudeHarness::new().discover(&host).await;
        let error = discovery.expect_err("expected failed probe cleanup to reach discovery");
        assert!(
            matches!(error, Error::CleanupRequired { .. }),
            "expected the cleanup-required error rather than an unavailable discovery, received {error:?}"
        );
        assert!(
            error.cleanup_control().is_some(),
            "expected discovery to retain the host cleanup control"
        );

        let opening = ClaudeHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await;
        let error = match opening {
            Ok(_) => panic!("expected failed probe cleanup to reach opening"),
            Err(error) => error,
        };
        assert!(
            matches!(error, Error::CleanupRequired { .. }),
            "expected the cleanup-required error rather than a launch refusal, received {error:?}"
        );
        assert!(
            error.cleanup_control().is_some(),
            "expected opening to retain the host cleanup control"
        );
    }

    #[tokio::test]
    async fn reads_the_binary_rather_than_the_pin() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let discovery = ClaudeHarness::new()
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");

        assert_eq!(discovery.gate, GateVerdict::Usable);
        assert_eq!(discovery.version.as_deref(), Some("2.1.270 (Claude Code)"));
        assert_eq!(
            discovery.auth,
            AuthState::LoggedIn {
                mode: AuthMode::Subscription
            }
        );
        assert!(discovery.is_usable());
        assert!(
            discovery
                .permission_matrix
                .supports(PermissionLevel::Default, ApprovalRouting::AutoReview),
            "expected subscription discovery to retain supported auto-review"
        );
        assert!(discovery.capabilities.capabilities().structured_streaming);
        assert!(
            discovery.capabilities.capabilities().model_catalog,
            "2.1.260 advertises aliases"
        );
        assert!(
            !discovery.capabilities.capabilities().interactive_approvals,
            "Claude Code delivers no answerable approval over its documented headless surface"
        );
    }

    #[tokio::test]
    async fn cold_discovery_runs_each_documented_probe_once() {
        let launcher = Arc::new(FakeClaudeCli::new());
        ClaudeHarness::new()
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected discovery");

        let probes: Vec<Vec<String>> = launcher
            .launches()
            .into_iter()
            .map(|launch| launch.argv)
            .collect();
        assert_eq!(
            probes,
            vec![
                vec![String::from("claude"), String::from("--version")],
                vec![String::from("claude"), String::from("--help")],
                vec![
                    String::from("claude"),
                    String::from("auth"),
                    String::from("status"),
                ],
            ],
            "expected one launch for each documented non-secret probe"
        );
    }

    #[tokio::test]
    async fn reports_only_capabilities_the_documented_surface_supports() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let discovery = ClaudeHarness::new()
            .discover(&host(launcher))
            .await
            .expect("expected discovery");
        let capabilities = discovery.capabilities.capabilities();

        assert!(capabilities.structured_streaming);
        assert!(capabilities.reasoning_stream);
        assert!(capabilities.resume);
        assert!(capabilities.cancellation);
        assert!(capabilities.usage_reporting);
        assert!(capabilities.configuration);
        assert!(capabilities.model_catalog);
        assert!(
            !capabilities.mcp_passthrough,
            "expected this captured help surface not to promise --mcp-config"
        );
        assert!(!capabilities.interactive_approvals);
        assert!(!capabilities.questions);
        assert!(!capabilities.images);
        assert!(!capabilities.steering);
        assert!(!capabilities.session_listing);
        assert!(!capabilities.native_review);
        assert!(!capabilities.account_usage);
        assert!(!capabilities.configuration_catalog);
        assert!(!capabilities.session_configuration);
    }

    /// A wrapper script's preamble is not the version, and neither is the whole of stdout.
    ///
    /// `--version` reaches the harness as every line joined together, because the token that is a
    /// version can arrive behind a preamble. What a host renders is one line, so one line is what
    /// `Discovery::version` carries.
    #[tokio::test]
    async fn reports_one_line_of_a_version_banner_a_wrapper_script_padded() {
        let launcher = Arc::new(FakeClaudeCli::new().with_version(
            "npm notice a new version of npm is available\n\n2.1.270 (Claude Code)\n",
        ));
        let discovery = ClaudeHarness::new()
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");

        assert_eq!(
            discovery.version.as_deref(),
            Some("2.1.270 (Claude Code)"),
            "expected the line the version was read from, received {:?}",
            discovery.version
        );
    }

    /// The same rule where there is no version to key off: one line, not the whole banner.
    #[tokio::test]
    async fn leaves_an_unreadable_version_and_help_probe_unknown_without_quoting_stdout() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_version("claude: this build is a repackage\nno version here\n")
                .with_help("--print\n"),
        );
        let discovery = ClaudeHarness::new()
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");

        assert_eq!(
            discovery.version.as_deref(),
            Some("claude: this build is a repackage")
        );
        assert_eq!(
            discovery.gate,
            GateVerdict::Unknown,
            "expected an unreadable probe not to claim an old version, received {:?}",
            discovery.gate
        );
    }

    #[tokio::test]
    async fn keeps_a_build_older_than_the_pin_when_every_flag_it_passes_is_there() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_version("2.1.150 (Claude Code)")
                .with_help(HELP_2_1_227),
        );
        let discovery = ClaudeHarness::new()
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");

        assert_eq!(discovery.gate, GateVerdict::Usable);
        assert!(
            !discovery.capabilities.capabilities().model_catalog,
            "expected a build advertising no aliases to keep the picker hidden"
        );
        assert!(discovery.models.is_empty());
    }

    #[tokio::test]
    async fn marks_a_build_missing_a_required_flag_unknown_rather_than_too_old() {
        let stripped = support::DEFAULT_HELP.replace("--forward-subagent-text", "--forward-txt");
        let launcher = Arc::new(FakeClaudeCli::new().with_help(&stripped));
        let discovery = ClaudeHarness::new()
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");

        assert_eq!(
            discovery.gate,
            GateVerdict::MissingRequiredSurface {
                expected: "a Claude --help surface declaring every required launch flag",
                received: "one or more required flags were absent",
            }
        );
        assert!(!discovery.is_usable());
        assert_eq!(
            *discovery.capabilities.capabilities(),
            mango_external_agents::Capabilities::none()
        );
    }

    #[tokio::test]
    async fn a_cli_that_is_not_there_is_not_installed_rather_than_an_error() {
        // The fake answers nothing, because a launcher that cannot spawn returns no output.
        struct NothingLauncher;
        #[async_trait::async_trait]
        impl mango_external_agents::ProcessLauncher for NothingLauncher {
            async fn spawn(
                &self,
                spec: mango_external_agents::LaunchSpec,
            ) -> mango_external_agents::Result<mango_external_agents::ManagedProcess> {
                Err(Error::Launch {
                    program: spec.program().unwrap_or_default().to_owned(),
                    message: String::from("No such file or directory"),
                })
            }
        }
        let context = host_under(Arc::new(NothingLauncher), Limits::default());

        let discovery = ClaudeHarness::new()
            .discover(&context)
            .await
            .expect("expected a discovery rather than an error");
        assert_eq!(discovery.gate, GateVerdict::NotInstalled);
        assert_eq!(discovery.auth, AuthState::Unknown);
    }

    #[tokio::test]
    async fn reports_a_signed_out_cli_with_the_vendors_own_login_command() {
        let launcher = Arc::new(FakeClaudeCli::new().with_auth(SIGNED_OUT));
        let discovery = ClaudeHarness::new()
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");

        assert_eq!(
            discovery.auth,
            AuthState::LoggedOut {
                login_hint: String::from("claude auth login")
            }
        );
        assert!(
            !discovery
                .permission_matrix
                .supports(PermissionLevel::Default, ApprovalRouting::AutoReview),
            "expected an account with no verified subscription to refuse auto-review"
        );
        assert!(
            !discovery.is_usable(),
            "expected a signed-out CLI not to be usable"
        );
    }

    #[tokio::test]
    async fn stays_within_the_ceiling_the_descriptor_declares() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let harness = ClaudeHarness::new();
        let discovery = harness
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");
        assert_eq!(
            discovery
                .capabilities
                .beyond(&harness.descriptor().capabilities),
            Vec::<&str>::new()
        );
    }

    #[tokio::test]
    async fn spawns_the_executable_the_host_resolved() {
        let launcher = Arc::new(FakeClaudeCli::new());
        ClaudeHarness::new()
            .with_executable("/opt/claude/bin/claude")
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");

        for launch in launcher.launches() {
            assert_eq!(launch.argv[0], "/opt/claude/bin/claude");
        }
    }
}

mod opening_a_session {
    use super::*;

    #[tokio::test]
    async fn a_not_installed_receipt_keeps_the_no_cli_refusal_without_probing() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let harness = ClaudeHarness::new();
        let host = host(Arc::clone(&launcher));
        let request = OpenSession::new("chat-1");
        let receipt = DiscoveryReceipt::new(
            HarnessId::claude(),
            mango_external_agents::Discovery::not_installed(),
            host.now(),
        )
        .with_executable_fingerprint("no-cli")
        .with_environment_fingerprint("same-environment")
        .with_authorization_fingerprint("same-authorization")
        .bind_to_open(
            harness.descriptor(),
            &host,
            &request,
            DiscoveryReceiptMeasurements::new()
                .with_executable_fingerprint("no-cli")
                .with_environment_fingerprint("same-environment")
                .with_authorization_fingerprint("same-authorization"),
        )
        .expect("expected current receipt evidence to match");

        let error = harness
            .open_session(&host, request.with_discovery(receipt))
            .await
            .map(|_| ())
            .expect_err("expected a no-CLI refusal");
        assert!(
            matches!(error.cause(), Error::Launch { .. }),
            "expected the no-CLI launch refusal, received {error:?}"
        );
        assert!(launcher.launches().is_empty(), "expected no vendor probe");
    }

    /// What a receipt is worth, as a number rather than as a claim.
    ///
    /// Three probes cold, one on reuse. The comparison is the whole argument for
    /// [`DiscoveryReceipt`]: a host that discovers and then opens without one pays the survey
    /// twice, because `open_session` deliberately does not cache — the harness has no way to know
    /// the executable, the environment or the account did not move between the two calls, and
    /// guessing is what a receipt exists to replace.
    ///
    /// Asserted as exact counts, so probe creep shows up here as a failing number rather than as a
    /// slower first token. The two halves are the evidence class `docs/adopt.md` records.
    #[tokio::test]
    async fn opening_without_a_receipt_pays_the_survey_twice() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let harness = ClaudeHarness::new();
        let host = host(Arc::clone(&launcher));

        let discovery = harness.discover(&host).await.expect("expected a discovery");
        let cold = launcher.launches().len();
        assert_eq!(
            cold,
            3,
            "expected the three documented cold probes, received {:?}",
            launcher
                .launches()
                .into_iter()
                .map(|launch| launch.argv)
                .collect::<Vec<_>>()
        );

        let mut request = OpenSession::new("chat-1");
        if let Some(executable) = &discovery.executable {
            request = request.with_executable(ExecutablePath::resolved(executable.clone()));
        }
        harness
            .open_session(&host, request)
            .await
            .expect("expected a session to open without a receipt");

        let launches = launcher.launches();
        let unvouched = &launches[cold..];
        assert_eq!(
            unvouched.len(),
            3,
            "expected an unvouched opening to repeat the whole survey, received {:?}",
            unvouched
                .iter()
                .map(|launch| launch.argv.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            unvouched
                .iter()
                .map(|launch| launch.argv[1..].join(" "))
                .collect::<Vec<_>>(),
            vec!["--version", "--help", "auth status"],
            "expected the same three probes the cold survey ran"
        );
    }

    #[tokio::test]
    async fn a_fresh_receipt_reuses_the_version_and_auth_answers_but_rechecks_help() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let harness = ClaudeHarness::new();
        let host = host(Arc::clone(&launcher));
        let discovery = harness.discover(&host).await.expect("expected a discovery");
        let before = launcher.launches().len();
        let mut request = OpenSession::new("chat-1");
        if let Some(executable) = &discovery.executable {
            request = request.with_executable(ExecutablePath::resolved(executable.clone()));
        }
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), discovery, host.now())
            .with_executable_fingerprint("same-test-binary")
            .with_environment_fingerprint("same-test-environment")
            .with_authorization_fingerprint("same-test-authorization")
            .bind_to_open(
                harness.descriptor(),
                &host,
                &request,
                DiscoveryReceiptMeasurements::new()
                    .with_executable_fingerprint("same-test-binary")
                    .with_environment_fingerprint("same-test-environment")
                    .with_authorization_fingerprint("same-test-authorization"),
            )
            .expect("expected current receipt evidence to match");

        harness
            .open_session(&host, request.with_discovery(receipt))
            .await
            .expect("expected the fresh receipt to open a session");

        let launches = launcher.launches();
        let reused = &launches[before..];
        assert_eq!(
            reused.len(),
            1,
            "expected only the help surface to be rechecked, received {reused:?}"
        );
        assert_eq!(
            reused[0].argv[1..],
            ["--help"],
            "expected the exact argv surface to be refreshed"
        );
    }

    #[tokio::test]
    async fn a_fresh_usable_help_surface_recovers_a_missing_surface_receipt_gate() {
        let mut missing_flag_help = String::from(HELP_2_1_270);
        missing_flag_help = missing_flag_help.replace(
            "  --include-partial-messages",
            "  --omitted-partial-messages",
        );
        let launcher = Arc::new(FakeClaudeCli::new().with_help(&missing_flag_help));
        let harness = ClaudeHarness::new();
        let host = host(Arc::clone(&launcher));
        let discovery = harness.discover(&host).await.expect("expected a discovery");
        assert!(
            matches!(discovery.gate, GateVerdict::MissingRequiredSurface { .. }),
            "expected the incomplete initial help surface to be gated, received {:?}",
            discovery.gate
        );
        let request = OpenSession::new("chat-1");
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), discovery, host.now())
            .with_executable_fingerprint("same-test-binary")
            .with_environment_fingerprint("same-test-environment")
            .with_authorization_fingerprint("same-test-authorization")
            .bind_to_open(
                harness.descriptor(),
                &host,
                &request,
                DiscoveryReceiptMeasurements::new()
                    .with_executable_fingerprint("same-test-binary")
                    .with_environment_fingerprint("same-test-environment")
                    .with_authorization_fingerprint("same-test-authorization"),
            )
            .expect("expected current receipt evidence to match");

        launcher.set_help(HELP_2_1_270);
        let before = launcher.launches().len();
        harness
            .open_session(&host, request.with_discovery(receipt))
            .await
            .expect("expected the refreshed complete help surface to open a session");

        let launches = launcher.launches();
        assert_eq!(
            launches[before..]
                .iter()
                .map(|launch| launch.argv.as_slice())
                .collect::<Vec<_>>(),
            vec![["claude", "--help"]],
            "expected opening to reuse receipt facts and refresh only help, received {launches:?}"
        );
    }

    #[tokio::test]
    async fn a_fresh_usable_help_surface_recovers_a_version_fallback_receipt_gate() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_version("2.1.200 (Claude Code)")
                .with_help("not Claude help"),
        );
        let harness = ClaudeHarness::new();
        let host = host(Arc::clone(&launcher));
        let discovery = harness.discover(&host).await.expect("expected a discovery");
        assert!(
            matches!(discovery.gate, GateVerdict::VersionTooOld { .. }),
            "expected unreadable help to fall back to the version gate, received {:?}",
            discovery.gate
        );
        let request = OpenSession::new("chat-1");
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), discovery, host.now())
            .with_executable_fingerprint("same-test-binary")
            .with_environment_fingerprint("same-test-environment")
            .with_authorization_fingerprint("same-test-authorization")
            .bind_to_open(
                harness.descriptor(),
                &host,
                &request,
                DiscoveryReceiptMeasurements::new()
                    .with_executable_fingerprint("same-test-binary")
                    .with_environment_fingerprint("same-test-environment")
                    .with_authorization_fingerprint("same-test-authorization"),
            )
            .expect("expected current receipt evidence to match");

        launcher.set_help(HELP_2_1_270);
        let before = launcher.launches().len();
        harness
            .open_session(&host, request.with_discovery(receipt))
            .await
            .expect("expected usable fresh help to supersede the version fallback");

        let launches = launcher.launches();
        assert_eq!(
            launches[before..]
                .iter()
                .map(|launch| launch.argv.as_slice())
                .collect::<Vec<_>>(),
            vec![["claude", "--help"]],
            "expected opening to refresh only help, received {launches:?}"
        );
    }

    #[tokio::test]
    async fn an_unreadable_fresh_help_surface_keeps_a_version_fallback_receipt_gate() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_version("2.1.200 (Claude Code)")
                .with_help("not Claude help"),
        );
        let harness = ClaudeHarness::new();
        let host = host(Arc::clone(&launcher));
        let discovery = harness.discover(&host).await.expect("expected a discovery");
        let request = OpenSession::new("chat-1");
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), discovery, host.now())
            .with_executable_fingerprint("same-test-binary")
            .with_environment_fingerprint("same-test-environment")
            .with_authorization_fingerprint("same-test-authorization")
            .bind_to_open(
                harness.descriptor(),
                &host,
                &request,
                DiscoveryReceiptMeasurements::new()
                    .with_executable_fingerprint("same-test-binary")
                    .with_environment_fingerprint("same-test-environment")
                    .with_authorization_fingerprint("same-test-authorization"),
            )
            .expect("expected current receipt evidence to match");

        let error = harness
            .open_session(&host, request.with_discovery(receipt))
            .await
            .map(|_| ())
            .expect_err("expected unreadable fresh help to retain the version fallback");
        assert!(
            matches!(error.cause(), Error::VersionGate { .. }),
            "expected the version fallback refusal, received {error:?}"
        );
    }

    #[tokio::test]
    async fn an_invalidated_bound_receipt_runs_no_additional_probe() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let clock = Arc::new(FrozenClock::default());
        let host_launcher: Arc<dyn mango_external_agents::ProcessLauncher> = launcher.clone();
        let host_clock: Arc<dyn mango_external_agents::Clock> = clock.clone();
        let host = HostContext::builder()
            .launcher(host_launcher)
            .cwd(std::env::temp_dir())
            .scratch(std::env::temp_dir())
            .environment(mango_external_agents::EnvSource::from_pairs([
                ("PATH", "/usr/bin"),
                ("CLAUDE_CONFIG_DIR", "/home/ada/.claude"),
            ]))
            .client_info("mea-tests", "0.0.0")
            .clock(host_clock)
            .build()
            .expect("expected a host");
        let harness = ClaudeHarness::new();
        let discovery = harness.discover(&host).await.expect("expected discovery");
        let before = launcher.launches().len();
        let request = OpenSession::new("chat-1");
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), discovery, host.now())
            .with_executable_fingerprint("fake-claude")
            .with_environment_fingerprint("fake-environment")
            .with_authorization_fingerprint("fake-authorization")
            .valid_for(Duration::ZERO)
            .bind_to_open(
                harness.descriptor(),
                &host,
                &request,
                DiscoveryReceiptMeasurements::new()
                    .with_executable_fingerprint("fake-claude")
                    .with_environment_fingerprint("fake-environment")
                    .with_authorization_fingerprint("fake-authorization"),
            )
            .expect("expected current receipt measurements to bind");

        clock.advance(Duration::from_secs(1));
        let error = harness
            .open_session(&host, request.with_discovery(receipt))
            .await
            .map(drop)
            .expect_err("expected the stale receipt to be refused");

        assert!(
            matches!(error.cause(), Error::HostConfiguration { .. }),
            "expected a typed stale-receipt refusal, received {error:?}"
        );
        assert_eq!(
            launcher.launches().len(),
            before,
            "expected receipt invalidation before any fresh vendor probe"
        );
    }

    #[tokio::test]
    async fn a_changed_receipt_fingerprint_runs_no_additional_probe() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let harness = ClaudeHarness::new();
        let host = host(Arc::clone(&launcher));
        let discovery = harness.discover(&host).await.expect("expected discovery");
        let before = launcher.launches().len();
        let request = OpenSession::new("chat-1");
        let error = DiscoveryReceipt::new(HarnessId::claude(), discovery, host.now())
            .with_executable_fingerprint("probed-fake-claude")
            .with_environment_fingerprint("fake-environment")
            .with_authorization_fingerprint("fake-authorization")
            .bind_to_open(
                harness.descriptor(),
                &host,
                &request,
                DiscoveryReceiptMeasurements::new()
                    .with_executable_fingerprint("changed-fake-claude")
                    .with_environment_fingerprint("fake-environment")
                    .with_authorization_fingerprint("fake-authorization"),
            )
            .expect_err("expected a changed executable fingerprint to refuse receipt reuse");

        assert!(
            matches!(error, Error::HostConfiguration { .. }),
            "expected a typed changed-fingerprint refusal, received {error:?}"
        );
        assert_eq!(
            launcher.launches().len(),
            before,
            "expected receipt invalidation before any additional vendor probe"
        );
    }

    #[tokio::test]
    async fn mints_a_uuid_the_cli_will_accept_without_starting_anything() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let session = open(&launcher).await;

        let ids = session.ids();
        assert_eq!(ids.session_id.as_str(), "chat-1");
        assert_eq!(
            ids.native_session_id.len(),
            36,
            "expected a UUID, received {:?}",
            ids.native_session_id
        );
        assert!(!session.snapshot().resumed);
        assert!(
            launcher.turn_argvs().is_empty(),
            "expected opening to spawn probes only"
        );
    }

    #[tokio::test]
    async fn refuses_fallback_resume_without_a_conclusive_vendor_signal() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let host = host(Arc::clone(&launcher));
        let error = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1")
                    .resuming("22222222-3333-4444-5555-666666666666", ResumeMode::Fallback),
            )
            .await
            .map(drop)
            .expect_err("expected fallback resume to be refused");

        assert!(
            matches!(error.cause(), Error::HostConfiguration { .. }),
            "expected a typed refusal, received {error:?}"
        );
        assert!(
            launcher.turn_argvs().is_empty(),
            "expected no turn launch, received {:?}",
            launcher.turn_argvs()
        );
        assert!(
            launcher.launches().is_empty(),
            "expected fallback to be refused before a probe, received {:?}",
            launcher.launches()
        );
    }

    #[tokio::test]
    async fn opens_a_strict_resume_as_unverified_until_the_vendor_starts_a_turn() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let host = host(Arc::clone(&launcher));
        let session = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1")
                    .resuming("22222222-3333-4444-5555-666666666666", ResumeMode::Strict),
            )
            .await
            .expect("expected strict resume to defer vendor confirmation");

        assert!(
            !session.snapshot().resumed,
            "expected opening to remain unverified until Claude emits system/init"
        );
        assert_eq!(
            session.ids().native_session_id,
            "22222222-3333-4444-5555-666666666666"
        );
    }

    #[tokio::test]
    async fn confirms_a_strict_resume_only_when_system_init_echoes_its_handle() {
        let native_session_id = "22222222-3333-4444-5555-666666666666";
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(&format!(
            r#"{{"type":"system","subtype":"init","session_id":"{native_session_id}"}}
{{"type":"result","is_error":false}}"#
        ))));
        let host = host(Arc::clone(&launcher));
        let session = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1").resuming(native_session_id, ResumeMode::Strict),
            )
            .await
            .expect("expected a strict resume session");

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "continue"))
            .await
            .expect("expected the deferred resume to start");
        assert_eq!(drain(&mut turn).await.last(), Some(&EventKind::Completed));
        assert!(session.snapshot().resumed);
        assert_eq!(session.ids().native_session_id, native_session_id);
        assert_eq!(
            value_after(&launcher.turn_argvs()[0], "--resume"),
            Some(native_session_id)
        );
    }

    #[tokio::test]
    async fn fails_a_strict_resume_when_system_init_names_a_different_conversation() {
        let requested = "22222222-3333-4444-5555-666666666666";
        let different = "33333333-4444-5555-6666-777777777777";
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(&format!(
            r#"{{"type":"system","subtype":"init","session_id":"{different}"}}
{{"type":"result","is_error":false}}"#
        ))));
        let host = host(Arc::clone(&launcher));
        let session = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1").resuming(requested, ResumeMode::Strict),
            )
            .await
            .expect("expected a strict resume session");

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "continue"))
            .await
            .expect("expected the deferred resume to start");
        let events = drain(&mut turn).await;
        assert!(
            matches!(events.last(), Some(EventKind::Error { .. })),
            "expected a failed strict resume, received {events:?}"
        );
        assert!(!session.snapshot().resumed);
        assert_eq!(session.ids().native_session_id, requested);
    }

    #[tokio::test]
    async fn strict_resume_does_not_emit_content_before_identity_confirmation() {
        let requested = "22222222-3333-4444-5555-666666666666";
        let different = "33333333-4444-5555-6666-777777777777";
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(&format!(
            r#"{{"type":"stream_event","event":{{"type":"content_block_start","index":0,"content_block":{{"type":"text"}}}}}}
{{"type":"stream_event","event":{{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"foreign content"}}}}}}
{{"type":"system","subtype":"init","session_id":"{different}"}}
{{"type":"result","is_error":false}}"#
        ))));
        let host = host(Arc::clone(&launcher));
        let session = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1").resuming(requested, ResumeMode::Strict),
            )
            .await
            .expect("expected a strict resume session");

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "continue"))
            .await
            .expect("expected the deferred resume to start");
        let events = drain(&mut turn).await;
        assert!(
            matches!(events.last(), Some(EventKind::Error { .. })),
            "expected a terminal protocol failure, received {events:?}"
        );
        assert!(
            events.iter().all(|event| matches!(
                event,
                EventKind::TurnStarted { .. } | EventKind::Error { .. }
            )),
            "expected no unconfirmed vendor content, received {events:?}"
        );
        assert!(!session.snapshot().resumed);
    }

    #[tokio::test]
    async fn fails_a_strict_resume_that_ends_without_identity_confirmation() {
        let requested = "22222222-3333-4444-5555-666666666666";
        let launcher = Arc::new(
            FakeClaudeCli::new().with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#)),
        );
        let host = host(Arc::clone(&launcher));
        let session = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1").resuming(requested, ResumeMode::Strict),
            )
            .await
            .expect("expected a strict resume session");

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "continue"))
            .await
            .expect("expected the deferred resume to start");
        assert!(
            matches!(drain(&mut turn).await.last(), Some(EventKind::Error { .. })),
            "expected a terminal confirmation failure"
        );
        assert!(!session.snapshot().resumed);
        assert_eq!(session.ids().native_session_id, requested);
    }

    #[tokio::test]
    async fn strict_resume_verification_failure_is_nonresumable_after_a_graceful_cleanup() {
        let requested = "22222222-3333-4444-5555-666666666666";
        let different = "33333333-4444-5555-6666-777777777777";
        let failures = [
            (
                "a different init handle",
                Run::replaying(&format!(
                    r#"{{"type":"system","subtype":"init","session_id":"{different}"}}"#
                )),
            ),
            (
                "content before init",
                Run::replaying(
                    r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}"#,
                ),
            ),
            (
                "a result without init",
                Run::replaying(r#"{"type":"result","is_error":false}"#),
            ),
            ("an ended stream without init", Run::exiting(0)),
        ];

        for (label, run) in failures {
            let launcher = Arc::new(
                FakeClaudeCli::new()
                    .with_graceful_interrupt()
                    .with_turn(run),
            );
            let host = host(Arc::clone(&launcher));
            let session = ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1").resuming(requested, ResumeMode::Strict),
                )
                .await
                .expect("expected a strict resume session");

            let mut turn = session
                .start_turn(TurnRequest::new("turn-1", "continue"))
                .await
                .expect("expected the deferred resume to start");
            assert!(
                matches!(drain(&mut turn).await.last(), Some(EventKind::Error { .. })),
                "expected {label} to fail strict resume verification"
            );
            assert_eq!(
                launcher.graceful_interrupts(),
                1,
                "expected {label} to reach the graceful cleanup path"
            );

            let error = session
                .start_turn(TurnRequest::new("turn-2", "must not continue"))
                .await
                .expect_err("expected strict resume verification failure to taint continuation");
            assert!(
                matches!(
                    error.cause(),
                    Error::Cancelled {
                        reason: CancelReason::Shutdown
                    }
                ),
                "expected {label} to refuse continuation, received {error:?}"
            );
            assert_eq!(
                launcher.turn_argvs().len(),
                1,
                "expected {label} to refuse before spawning another Claude turn"
            );
        }
    }

    /// At face value is not the same as unvetted.
    ///
    /// The reference goes on the command line as `--resume <value>`, and an argv array stops shell
    /// injection but not argument injection: a stored handle beginning with `-` is read by the
    /// CLI's parser as a flag of its own. Refused at `open_session`, so no turn is ever spawned
    /// with it.
    #[tokio::test]
    async fn refuses_a_resume_reference_that_could_become_another_flag() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let host = host(Arc::clone(&launcher));
        for injected in [
            "--dangerously-skip-permissions",
            "-p",
            "22222222-3333-4444-5555-666666666666 --print",
        ] {
            let error = ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1").resuming(injected, ResumeMode::Strict),
                )
                .await
                .map(drop)
                .expect_err("expected the reference to be refused");
            assert!(
                matches!(error.cause(), Error::HostConfiguration { .. }),
                "expected a host-configuration refusal for {injected:?}, received {error:?}"
            );
            // The refusal names the shape, never the handle: a stored resume reference is the
            // host's own opaque data, and this one is being reported precisely because nobody
            // knows what is in it.
            assert!(
                !error.to_string().contains(injected),
                "expected the rejected reference to stay out of the message, received {error}"
            );
        }
        assert!(
            launcher.turn_argvs().is_empty(),
            "expected no turn to be spawned, received {:?}",
            launcher.turn_argvs()
        );
    }

    #[tokio::test]
    async fn refuses_a_signed_out_cli_before_a_turn_is_spawned() {
        let launcher = Arc::new(FakeClaudeCli::new().with_auth(SIGNED_OUT));
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), OpenSession::new("chat-1"))
            .await
            .map(drop)
            .expect_err("expected a refusal");

        assert!(
            matches!(error.cause(), Error::AuthRequired { login_hint } if login_hint == "claude auth login"),
            "received {error:?}"
        );
        assert!(launcher.turn_argvs().is_empty());
    }

    #[tokio::test]
    async fn refuses_an_old_build_when_its_help_surface_is_unreadable_before_a_turn_is_spawned() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_version("2.1.150 (Claude Code)")
                .with_help("not Claude help"),
        );
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), OpenSession::new("chat-1"))
            .await
            .map(drop)
            .expect_err("expected a refusal");

        assert!(
            matches!(error.cause(), Error::VersionGate { minimum, .. } if minimum == "2.1.211"),
            "received {error:?}"
        );
    }

    #[tokio::test]
    async fn distinguishes_a_missing_required_flag_from_an_old_version_before_a_turn_is_spawned() {
        let stripped = support::DEFAULT_HELP.replace("--forward-subagent-text", "--forward-txt");
        let launcher = Arc::new(FakeClaudeCli::new().with_help(&stripped));
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), OpenSession::new("chat-1"))
            .await
            .map(drop)
            .expect_err("expected the missing documented flag to be refused");

        assert!(
            matches!(
                error.cause(),
                Error::Protocol { expected, received }
                    if expected == "a Claude --help surface declaring every required launch flag"
                        && received == "one or more required flags were absent"
            ),
            "expected a distinct safe missing-flag refusal, received {error:?}"
        );
        assert!(launcher.turn_argvs().is_empty());
    }

    #[tokio::test]
    async fn distinguishes_an_unreadable_help_probe_from_a_missing_flag_before_a_turn_is_spawned() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_help("Authorization: Bearer probe-output-must-not-reach-diagnostics"),
        );
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), OpenSession::new("chat-1"))
            .await
            .map(drop)
            .expect_err("expected an unreadable help probe to be refused");

        assert!(
            matches!(
                error.cause(),
                Error::Protocol { expected, received }
                    if expected == "a readable Claude --help launch surface"
                        && received == "unreadable vendor output"
            ),
            "expected a distinct safe unreadable-probe refusal, received {error:?}"
        );
        assert!(
            !error
                .to_string()
                .contains("probe-output-must-not-reach-diagnostics"),
            "expected raw help output to stay out of diagnostics, received {error:?}"
        );
        assert!(launcher.turn_argvs().is_empty());
    }

    #[tokio::test]
    async fn refuses_an_auto_review_pair_this_account_cannot_run() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_auth(r#"{"loggedIn":true,"authMethod":"apiKey","apiProvider":"firstParty"}"#),
        );
        let request = OpenSession::new("chat-1").with_configuration(
            ConfigurationPatch::new()
                .level(ConfigurationChange::Set(PermissionLevel::Default))
                .routing(ConfigurationChange::Set(ApprovalRouting::AutoReview)),
        );
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), request)
            .await
            .map(drop)
            .expect_err("expected a refusal");

        assert!(
            matches!(error.cause(), Error::HostConfiguration { .. }),
            "received {error:?}"
        );
        assert!(launcher.turn_argvs().is_empty());
    }

    /// Claude's model and permission-mode flags are argv on a fresh child: there is no surface to
    /// un-set an override once a session is open, so a reset is refused rather than silently
    /// treated as "leave it alone" — see `mango_external_agents::configuration::refuse_unsupported_reset`.
    #[tokio::test]
    async fn refuses_a_reset_request_before_a_turn_is_spawned() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let request = OpenSession::new("chat-1")
            .with_configuration(ConfigurationPatch::new().model(ConfigurationChange::Reset));
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), request)
            .await
            .map(drop)
            .expect_err("expected a reset request to be refused");

        assert!(
            matches!(error.cause(), Error::HostConfiguration { .. }),
            "received {error:?}"
        );
        assert!(
            error.to_string().contains("reset"),
            "expected the refusal to name the reset, received {error}"
        );
        assert!(
            launcher.turn_argvs().is_empty(),
            "expected no turn to be spawned"
        );
    }

    #[tokio::test]
    async fn refuses_a_native_opening_axis_before_probing_or_spawning() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let request =
            OpenSession::new("chat-1").with_configuration(ConfigurationPatch::new().native(
                ConfigurationOptionId::new("web-search"),
                ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
            ));
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), request)
            .await
            .map(drop)
            .expect_err("expected an unsupported native axis to be refused");

        assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
        assert!(
            matches!(error.cause(), Error::Protocol { .. }),
            "received {error:?}"
        );
        assert!(
            launcher.launches().is_empty(),
            "expected no probe or turn to spawn"
        );
    }

    #[tokio::test]
    async fn prefers_the_executable_the_request_resolved_over_the_harnesss_own() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let request =
            OpenSession::new("chat-1").with_executable(ExecutablePath::resolved("/srv/claude"));
        let session = ClaudeHarness::new()
            .with_executable("/opt/claude/bin/claude")
            .open_session(&host(Arc::clone(&launcher)), request)
            .await
            .expect("expected a session");

        session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");
        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        assert_eq!(argv[0], "/srv/claude");
    }
}

mod a_turn {
    use super::*;

    #[tokio::test]
    async fn publishes_the_model_system_init_reports_as_observed_configuration() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(READ_TURN)));
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "read note.txt"))
            .await
            .expect("expected a turn");
        drain(&mut turn).await;

        assert_eq!(
            session.snapshot().configuration.observed.model.as_deref(),
            Some("claude-sonnet-5"),
            "expected the vendor-reported model to remain distinct from accepted argv settings"
        );
    }

    /// A configuration value reaches the child process argv. Reject it before reserving or
    /// starting a child, because dropping it would run under settings the host did not choose.
    #[tokio::test]
    async fn refuses_an_invalid_model_or_effort_before_starting_a_child() {
        let cases = [
            ConfigurationPatch::new().model(ConfigurationChange::Set(String::from(
                "--dangerously-skip-permissions",
            ))),
            ConfigurationPatch::new().effort(ConfigurationChange::Set(String::from("ultra"))),
        ];

        for configuration in cases {
            let launcher = Arc::new(FakeClaudeCli::new());
            let session = open(&launcher).await;
            let before = session.snapshot().configuration.clone();
            let error = session
                .start_turn(
                    TurnRequest::new("turn-1", "use the explicit settings")
                        .with_configuration(configuration),
                )
                .await
                .expect_err("expected an invalid explicit configuration to be refused");

            assert!(
                matches!(error.cause(), Error::HostConfiguration { .. }),
                "received {error:?}"
            );
            assert!(
                launcher.turn_argvs().is_empty(),
                "expected no turn launch, received {:?}",
                launcher.turn_argvs()
            );
            assert_eq!(
                session.snapshot().configuration,
                before,
                "expected a refused explicit configuration not to replace accepted defaults"
            );
        }
    }

    /// An omitted pair must remain absent from argv, while a later accepted pair is repeated on
    /// turns that omit their own configuration.
    #[tokio::test]
    async fn omitted_permissions_use_vendor_defaults_then_inherit_an_explicit_turn_choice() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#)),
        );
        let session = open(&launcher).await;

        let mut vendor_default = session
            .start_turn(TurnRequest::new("turn-1", "keep vendor defaults"))
            .await
            .expect("expected the default turn to start");
        drain(&mut vendor_default).await;

        let explicit = ConfigurationPatch::new()
            .level(ConfigurationChange::Set(PermissionLevel::Default))
            .routing(ConfigurationChange::Set(ApprovalRouting::User));
        let mut configured = session
            .start_turn(
                TurnRequest::new("turn-2", "set explicit defaults")
                    .with_configuration(explicit.clone()),
            )
            .await
            .expect("expected the explicit turn to start");
        drain(&mut configured).await;

        let mut inherited = session
            .start_turn(TurnRequest::new("turn-3", "inherit explicit defaults"))
            .await
            .expect("expected the inherited turn to start");
        drain(&mut inherited).await;

        // A model-only override is the regression: replacing the configuration wholesale cleared
        // the accepted permission pair, so this invocation omitted `--permission-mode`.
        let mut model_only = session
            .start_turn(
                TurnRequest::new("turn-4", "keep permissions while changing model")
                    .with_configuration(
                        ConfigurationPatch::new()
                            .model(ConfigurationChange::Set(String::from("sonnet"))),
                    ),
            )
            .await
            .expect("expected the model-only turn to start");
        drain(&mut model_only).await;

        let argvs = launcher.turn_argvs();
        assert_eq!(value_after(&argvs[0], "--permission-mode"), None);
        assert_eq!(value_after(&argvs[0], "--permission-prompts"), None);
        assert_eq!(value_after(&argvs[1], "--permission-mode"), Some("manual"));
        assert_eq!(value_after(&argvs[2], "--permission-mode"), Some("manual"));
        assert_eq!(value_after(&argvs[3], "--permission-mode"), Some("manual"));
        assert_eq!(value_after(&argvs[3], "--model"), Some("sonnet"));
        assert_eq!(
            session.snapshot().configuration.accepted,
            Configuration::unknown()
                .with_model("sonnet")
                .with_level(PermissionLevel::Default)
                .with_routing(ApprovalRouting::User)
        );
    }

    #[tokio::test]
    async fn streams_the_recorded_turn_and_closes_with_completed() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(READ_TURN)));
        let session = open(&launcher).await;

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "read note.txt"))
            .await
            .expect("expected a turn");
        let events = drain(&mut turn).await;

        assert_eq!(turn.native_turn_id(), "turn-1");
        assert_eq!(events.last(), Some(&EventKind::Completed));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventKind::TextDelta { .. }))
                .count(),
            2
        );
        // `claude --print` names no turn, so the host's own id is the vendor handle — emitted
        // first, before anything the vendor stream itself reports.
        assert_eq!(
            events.first(),
            Some(&EventKind::TurnStarted {
                native_turn_id: String::from("turn-1")
            })
        );
    }

    /// The catalog the reducer read off `system/init` has to actually reach
    /// [`SessionState`](mango_external_agents::SessionState), not just the `RunInit` the reducer's
    /// own unit tests inspect — this is the wiring `apply_init` is for.
    #[tokio::test]
    async fn a_run_that_announces_commands_publishes_them_on_session_state() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(READ_TURN)));
        let session = open(&launcher).await;
        assert!(
            session.snapshot().commands.is_empty(),
            "expected no commands before any turn ran"
        );

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "read note.txt"))
            .await
            .expect("expected a turn");
        drain(&mut turn).await;

        let snapshot = session.snapshot();
        let names: Vec<&str> = snapshot
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert!(
            names.contains(&"dataviz") && names.contains(&"code-review:code-review"),
            "expected the announced catalog to reach session state, received {names:?}"
        );
    }

    /// `TurnId::new` validates nothing, so a turn id too strange for `EventKind::normalized` to
    /// keep is only refused once `EventKind::TurnStarted` fails to normalize — after the child is
    /// already running. The child must be reaped, not left behind for a refusal nobody can act on.
    #[tokio::test]
    async fn refuses_a_turn_whose_id_cannot_be_kept_rather_than_leaving_the_child_running() {
        // Stalling, not replaying: a turn that finishes on its own would still show
        // `a_child_is_running() == false` on a codepath that never killed it, and prove nothing.
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let session = open(&launcher).await;

        let error = session
            .start_turn(TurnRequest::new("   ", "hello"))
            .await
            .expect_err("expected a whitespace-only turn id to be refused");
        assert!(
            matches!(
                error.cause(),
                Error::InvalidVendorValue {
                    field: "native turn id",
                    ..
                }
            ),
            "received {error:?}"
        );

        tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.a_child_is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected the child spawned before the refusal to be reaped");
    }

    /// The turn-level twin of `opening_a_session::refuses_a_reset_request_before_a_turn_is_spawned`
    /// — Claude has no reset semantics on either surface.
    #[tokio::test]
    async fn refuses_a_turn_level_reset_request_rather_than_spawning_with_it() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let session = open(&launcher).await;

        let error =
            session
                .start_turn(TurnRequest::new("turn-1", "hello").with_configuration(
                    ConfigurationPatch::new().model(ConfigurationChange::Reset),
                ))
                .await
                .expect_err("expected a reset request to be refused");

        assert!(
            matches!(error.cause(), Error::HostConfiguration { .. }),
            "received {error:?}"
        );
        assert!(
            error.to_string().contains("reset"),
            "expected the refusal to name the reset, received {error}"
        );
        assert!(
            launcher.turn_argvs().is_empty(),
            "expected no turn to be spawned"
        );
    }

    #[tokio::test]
    async fn refuses_a_native_turn_axis_before_spawning() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let session = open(&launcher).await;
        let error = session
            .start_turn(TurnRequest::new("turn-1", "hello").with_configuration(
                ConfigurationPatch::new().native(
                    ConfigurationOptionId::new("web-search"),
                    ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
                ),
            ))
            .await
            .expect_err("expected an unsupported native axis to be refused");

        assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
        assert!(
            matches!(error.cause(), Error::Protocol { .. }),
            "received {error:?}"
        );
        assert!(
            launcher.turn_argvs().is_empty(),
            "expected no turn to be spawned"
        );
    }

    #[tokio::test]
    async fn writes_the_prompt_to_stdin_exactly_once_and_never_into_argv() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(READ_TURN)));
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "read note.txt"))
            .await
            .expect("expected a turn");
        drain(&mut turn).await;

        let written = launcher.written();
        assert_eq!(written.len(), 1, "received {written:?}");
        let parsed: serde_json::Value =
            serde_json::from_str(&written[0]).expect("expected one stream-json message");
        assert_eq!(parsed["message"]["content"][0]["text"], "read note.txt");

        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        assert!(
            argv.iter().all(|argument| !argument.contains("note.txt")),
            "expected the prompt to stay out of the process listing, received {argv:?}"
        );
    }

    #[tokio::test]
    async fn mints_the_session_on_the_first_turn_and_resumes_it_afterwards() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::replaying(READ_TURN))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#)),
        );
        let session = open(&launcher).await;

        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "read note.txt"))
            .await
            .expect("expected a turn");
        drain(&mut first).await;
        let mut second = session
            .start_turn(TurnRequest::new("turn-2", "and again"))
            .await
            .expect("expected a second turn");
        drain(&mut second).await;

        let argvs = launcher.turn_argvs();
        assert_eq!(argvs.len(), 2);
        assert!(
            value_after(&argvs[0], "--session-id").is_some(),
            "received {:?}",
            argvs[0]
        );
        assert_eq!(
            value_after(&argvs[1], "--resume"),
            Some("b01414e7-4b4b-43a2-9109-a33e21664340"),
            "expected the second turn to resume the conversation the run reported"
        );
    }

    /// The handle a host persists has to be the one its next open can resume.
    ///
    /// Turns after the first already follow a `session_id` the run reported instead of the one
    /// `--session-id` proposed. A host reading `ids()` has to see the same handle, or it stores
    /// the abandoned one and its next `open_session` fails at the first turn with the vendor's
    /// "no conversation found".
    #[tokio::test]
    async fn reports_the_session_handle_the_run_chose_rather_than_the_one_it_minted() {
        const CHOSEN: &str = "9d2b7c14-6f0a-4b8e-9a31-2c5d7e0f4a6b";
        let transcript = format!(
            "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{CHOSEN}\"}}\n{{\"type\":\"result\",\"is_error\":false}}"
        );
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(&transcript)));
        let session = open(&launcher).await;
        let minted = session.ids().native_session_id;
        assert_ne!(
            minted, CHOSEN,
            "expected the run to report a handle other than the minted one"
        );

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "start"))
            .await
            .expect("expected a turn");
        drain(&mut turn).await;

        assert_eq!(
            session.ids().native_session_id,
            CHOSEN,
            "expected the handle the run chose, received {:?}",
            session.ids().native_session_id
        );
    }

    /// A second turn resumes a named conversation, so an `init` naming a different one means the
    /// CLI answered from history this session has never seen.
    ///
    /// Adopting it silently is the failure worth a test: the handle a host persisted would be
    /// replaced by one nobody read, with no event, and the host's next `open_session` would resume
    /// a conversation it believes it has a transcript for. The first turn stays lenient — it
    /// proposes a handle with `--session-id` and a CLI that started a different conversation anyway
    /// has made that one real — which is why this only bites from the second turn on.
    #[tokio::test]
    async fn refuses_a_later_turn_that_answers_from_a_different_conversation() {
        const FIRST: &str = "b01414e7-4b4b-43a2-9109-a33e21664340";
        const OTHER: &str = "3f8a1d52-91c4-4e7b-8a06-5d2b9c7e1f04";
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::replaying(&format!(
                    "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{FIRST}\"}}\n{{\"type\":\"result\",\"is_error\":false}}"
                )))
                .with_turn(Run::replaying(&format!(
                    "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{OTHER}\"}}\n{{\"type\":\"result\",\"is_error\":false}}"
                ))),
        );
        let session = open(&launcher).await;

        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "start"))
            .await
            .expect("expected a turn");
        drain(&mut first).await;
        assert_eq!(session.ids().native_session_id, FIRST);

        let mut second = session
            .start_turn(TurnRequest::new("turn-2", "and again"))
            .await
            .expect("expected a second turn");
        let events = drain(&mut second).await;

        assert!(
            events.iter().any(|kind| matches!(
                kind,
                mango_external_agents::EventKind::Error { error }
                    if error.message.contains("continuing the session this turn resumed")
            )),
            "expected the turn to report the conversation mismatch, received {events:?}"
        );
        assert_eq!(
            session.ids().native_session_id,
            FIRST,
            "the handle a host persisted must survive a run that answered from another conversation"
        );
    }

    /// A later turn resumes a named conversation even when the vendor's echo is malformed.
    ///
    /// Treating that echo as if it were absent lets the process publish a result while the host
    /// keeps the previous handle, leaving the host unable to tell whether the result belongs to
    /// the conversation it asked Claude to resume.
    #[tokio::test]
    async fn refuses_a_later_turn_that_names_an_invalid_session_handle() {
        const FIRST: &str = "b01414e7-4b4b-43a2-9109-a33e21664340";
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::replaying(&format!(
                    "{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"{FIRST}\"}}\n{{\"type\":\"result\",\"is_error\":false}}"
                )))
                .with_turn(Run::replaying(
                    r#"{"type":"system","subtype":"init","session_id":"not-a-uuid"}
{"type":"result","is_error":false}"#,
                )),
        );
        let session = open(&launcher).await;

        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "start"))
            .await
            .expect("expected the first turn to start");
        drain(&mut first).await;
        assert_eq!(session.ids().native_session_id, FIRST);

        let mut second = session
            .start_turn(TurnRequest::new("turn-2", "and again"))
            .await
            .expect("expected the resumed turn to start");
        let events = drain(&mut second).await;

        assert!(
            events.iter().any(|kind| matches!(
                kind,
                EventKind::Error { error }
                    if error.message.contains("UUID-shaped Claude resume handle")
            )),
            "expected an invalid echoed handle to fail the resumed turn, received {events:?}"
        );
        assert!(
            events.iter().all(|kind| matches!(
                kind,
                EventKind::TurnStarted { .. } | EventKind::Error { .. }
            )),
            "expected no result from an unverified conversation, received {events:?}"
        );
        assert_eq!(
            session.ids().native_session_id,
            FIRST,
            "the persisted handle must survive an invalid echoed handle"
        );
        assert_eq!(
            value_after(&launcher.turn_argvs()[1], "--resume"),
            Some(FIRST),
            "expected the rejected turn to have resumed the established handle"
        );
    }

    /// The echoed handle is a vendor-chosen value that a later argv carries.
    ///
    /// `system/init` is followed because it names the conversation that now exists — but following
    /// it verbatim puts whatever the run printed into the next turn's `--resume <value>`, where a
    /// leading `-` is read as a flag rather than as the option's value. The minted id stays in
    /// force instead, and the turn still resumes rather than trying to mint the same id twice.
    #[tokio::test]
    async fn does_not_follow_an_echoed_session_handle_that_could_become_another_flag() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::replaying(
                    r#"{"type":"system","subtype":"init","session_id":"--dangerously-skip-permissions"}
{"type":"result","is_error":false}"#,
                ))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#)),
        );
        let session = open(&launcher).await;

        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "start"))
            .await
            .expect("expected a turn");
        drain(&mut first).await;
        let mut second = session
            .start_turn(TurnRequest::new("turn-2", "carry on"))
            .await
            .expect("expected a second turn");
        drain(&mut second).await;

        let argvs = launcher.turn_argvs();
        let minted = value_after(&argvs[0], "--session-id").expect("expected a minted id");
        assert_eq!(
            value_after(&argvs[1], "--resume"),
            Some(minted),
            "expected the minted handle to stay in force, received {:?}",
            argvs[1]
        );
        assert!(
            argvs[1]
                .iter()
                .all(|argument| !argument.contains("skip-permissions")),
            "received {:?}",
            argvs[1]
        );
    }

    #[tokio::test]
    async fn a_forcibly_cancelled_turn_refuses_an_unsafe_resume() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::stalling([
                    r#"{"type":"system","subtype":"init","session_id":"aaaaaaaa-1111-2222-3333-444444444444"}"#,
                ]))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#)),
        );
        let session = open(&launcher).await;
        // The vendor's own session handle is session state now, not a turn event — subscribing
        // before the turn starts is what lets this test wait for it without a race.
        let mut state_changes = session.subscribe();

        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "start something long"))
            .await
            .expect("expected a turn");
        // The init has to land before the cancel, or there is nothing to remember.
        tokio::time::timeout(Duration::from_secs(5), state_changes.changed())
            .await
            .expect("expected the vendor's own session handle to land")
            .expect("expected the session state to stay alive");

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected the cancel to land");
        drain(&mut first).await;

        let error = session
            .start_turn(TurnRequest::new("turn-2", "carry on"))
            .await
            .expect_err("expected an explicit refusal instead of an implicit fresh session");
        assert!(
            matches!(
                error.cause(),
                Error::Cancelled {
                    reason: CancelReason::Requested
                }
            ),
            "expected the forced stop to make continuation nonresumable, received {error:?}"
        );
        assert_eq!(
            launcher.turn_argvs().len(),
            1,
            "expected the refusal to avoid both resume and a replacement submission"
        );
    }

    #[tokio::test]
    async fn a_gracefully_interrupted_turn_keeps_its_verified_native_continuation() {
        let native_id = "aaaaaaaa-1111-2222-3333-444444444444";
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_graceful_interrupt()
                .with_turn(Run::stalling([format!(
                    r#"{{"type":"system","subtype":"init","session_id":"{native_id}"}}"#
                )]))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#)),
        );
        let session = open(&launcher).await;
        let mut changes = session.subscribe();
        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected the first turn");
        tokio::time::timeout(Duration::from_secs(5), changes.changed())
            .await
            .expect("expected Claude to verify its native session id")
            .expect("expected the session state to remain available");

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected a graceful interruption");
        drain(&mut first).await;

        let mut second = session
            .start_turn(TurnRequest::new("turn-2", "continue"))
            .await
            .expect("expected a verified graceful interruption to preserve resume");
        drain(&mut second).await;
        let argvs = launcher.turn_argvs();
        assert_eq!(
            value_after(&argvs[1], "--resume"),
            Some(native_id),
            "expected continuation with the verified native id, received {:?}",
            argvs[1]
        );
    }

    #[tokio::test]
    async fn a_revoked_host_refuses_before_reserving_or_spawning_a_turn() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let cancel = CancelToken::new();
        cancel.cancel();
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(std::env::temp_dir())
            .scratch(std::env::temp_dir())
            .client_info("mea-tests", "0.0.0")
            .cancel(cancel)
            .build()
            .expect("expected a host");
        let session = ClaudeHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected opening to remain side-effect free");

        let error = session
            .start_turn(TurnRequest::new("turn-1", "do not run"))
            .await
            .expect_err("expected the revoked host to refuse admission");
        assert_eq!(error.dispatch(), Dispatch::NotSubmitted);
        assert!(
            matches!(
                error.cause(),
                Error::Cancelled {
                    reason: CancelReason::Shutdown
                }
            ),
            "received {error:?}"
        );
        assert!(
            launcher.turn_argvs().is_empty(),
            "expected revocation to precede any turn launch"
        );
    }

    #[tokio::test]
    async fn cancels_the_activity_a_failure_left_open_rather_than_leaving_it_running() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::Transcript {
                    lines: vec![String::from(
                        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"sleep 600"}}]}}"#,
                    )],
                    exit: mango_external_agents::ExitStatus {
                        code: Some(1),
                        signal: None,
                    },
                })
                .with_stderr("error: something went wrong"),
        );
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "run it"))
            .await
            .expect("expected a turn");
        let events = drain(&mut turn).await;

        let EventKind::ActivityCompleted { result, .. } = &events[2] else {
            panic!("expected the open call to be closed, received {events:?}");
        };
        assert_eq!(
            result.status,
            mango_external_agents::ActivityStatus::Cancelled
        );

        let EventKind::Error { error } = events.last().expect("expected a terminal") else {
            panic!("expected an error, received {events:?}");
        };
        assert_eq!(error.code.as_str(), "claude-no-result");
        assert!(
            error.message.contains("exit code 1"),
            "received {:?}",
            error.message
        );
        assert!(
            error.message.contains("something went wrong"),
            "expected the stderr tail, received {:?}",
            error.message
        );
    }

    #[tokio::test]
    async fn ends_the_stream_with_an_error_when_the_process_stops_before_a_result() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::exiting(1))
                .with_turn(Run::replaying(READ_TURN)),
        );
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");
        let events = drain(&mut turn).await;

        assert!(
            matches!(events.last(), Some(EventKind::Error { .. })),
            "received {events:?}"
        );
        let error = session
            .start_turn(TurnRequest::new("turn-2", "do not resume unknown work"))
            .await
            .expect_err("expected a missing vendor result to taint continuation");
        assert!(
            matches!(
                error.cause(),
                Error::Cancelled {
                    reason: CancelReason::Shutdown
                }
            ),
            "received {error:?}"
        );
        assert_eq!(
            launcher.turn_argvs().len(),
            1,
            "expected the unsafe continuation to be refused before launch"
        );
    }

    /// The child is still running, and still writing, when the link gives up on it.
    ///
    /// Every other failing path here reaches a process that has already exited, so a terminal
    /// arrives however long the turn is willing to wait for an exit status. This is the path where
    /// it has not exited: one line past the cap breaks the link while the vendor works on. Waiting
    /// on that exit without a bound is a turn that never ends, on exactly the failure a host most
    /// needs to be told about.
    #[tokio::test]
    async fn ends_the_turn_when_a_line_breaks_the_link_and_the_process_runs_on() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling(["x".repeat(10_000)])));
        let host = host_under(
            launcher.clone(),
            Limits {
                line: LineLimits {
                    max_line_bytes: 4_096,
                    max_buffered_bytes: 8_192,
                },
                ..Limits::default()
            },
        );
        let session = ClaudeHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "say something enormous"))
            .await
            .expect("expected a turn");
        let events = drain(&mut turn).await;

        let Some(EventKind::Error { error }) = events.last() else {
            panic!("expected an error, received {events:?}");
        };
        assert_eq!(error.code.as_str(), "claude-stream-broken");
        assert!(
            error.message.contains("8192") && error.message.contains("10001"),
            "expected the cap and what passed it, received {:?}",
            error.message
        );
        assert!(
            launcher.all_children_ended(),
            "expected the child to be reaped even though it never exited on its own"
        );
    }

    /// `close` while a turn's process is still being spawned waits for the late child to stop.
    #[tokio::test]
    async fn refuses_a_turn_whose_session_closed_while_its_process_was_starting() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let gate = launcher.gate_turn_spawns();
        let session = shared(&launcher).await;

        let starting = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .start_turn(TurnRequest::new("turn-1", "hello"))
                    .await
                    .map(drop)
            }
        });

        gate.wait_for_spawn().await;
        let closing = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.close(CloseReason::Requested).await }
        });
        gate.release();

        closing
            .await
            .expect("expected the close task to finish")
            .expect("expected the close to succeed");

        let outcome = starting.await.expect("expected the task to finish");
        assert!(
            matches!(outcome, Err(ref error) if matches!(error.cause(), Error::Closed { .. })),
            "expected the turn to be refused by the closed session, received {outcome:?}"
        );
        assert!(
            launcher.all_children_ended(),
            "expected the child spawned into the window to be reaped, not left running"
        );
    }

    /// `cancel` while a turn's process is still being spawned.
    ///
    /// The slot `start_turn` reserves before awaiting the spawn has no control to kill yet, so a
    /// `cancel` landing in that window used to record no reason and report success, then have the
    /// turn plant a live child anyway once the spawn finished. No stream was ever handed out to
    /// this call, so there is nothing for a `Cancelled` event to reach — this refuses the turn
    /// with `Error::Cancelled` instead of handing one out.
    #[tokio::test]
    async fn refuses_a_turn_that_a_cancel_stopped_while_its_process_was_starting() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let gate = launcher.gate_turn_spawns();
        let session = shared(&launcher).await;

        let starting = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .start_turn(TurnRequest::new("turn-1", "hello"))
                    .await
                    .map(drop)
            }
        });

        gate.wait_for_spawn().await;
        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected the cancel to succeed");
        gate.release();

        let outcome = starting.await.expect("expected the task to finish");
        assert!(
            matches!(
                outcome,
                Err(ref error)
                    if matches!(
                        error.cause(),
                        Error::Cancelled {
                            reason: CancelReason::Requested
                        }
                    )
            ),
            "expected the turn to be refused by the cancel that landed while it was starting, received {outcome:?}"
        );
        assert!(
            launcher.all_children_ended(),
            "expected the child spawned into the window to be reaped, not left running"
        );
    }

    /// Two `start_turn`s racing the same spawn window.
    ///
    /// The first claim owns the slot before it waits for the launcher. The second request must
    /// therefore observe that reservation and receive a typed busy refusal without launching a
    /// second vendor process or changing the first request's cancellation reason.
    #[tokio::test]
    async fn a_second_start_turn_racing_the_first_spawn_is_refused_as_busy() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let gate = launcher.gate_turn_spawns();
        let session = shared(&launcher).await;

        let first = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .start_turn(TurnRequest::new("turn-1", "hello"))
                    .await
            }
        });
        gate.wait_for_spawn().await;

        let second = session
            .start_turn(TurnRequest::new("turn-2", "hello again"))
            .await
            .expect_err("expected the active first turn to refuse the second request");
        assert!(
            matches!(second.cause(), Error::Busy),
            "expected a typed busy refusal, received {second:?}"
        );

        gate.release();

        let first_outcome = first.await.expect("expected the first task to finish");
        assert!(
            first_outcome.is_ok(),
            "expected the first turn to keep its claim, received {first_outcome:?}"
        );
        assert_eq!(
            launcher.turn_argvs().len(),
            1,
            "expected only the admitted turn to reach Claude"
        );

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the close to succeed");
        assert!(
            launcher.all_children_ended(),
            "expected both children to be reaped rather than leaving the superseded one running"
        );
    }

    #[tokio::test]
    async fn dropping_start_during_spawn_reaps_the_child_returned_after_abort() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let gate = launcher.gate_turn_spawns();
        let session = shared(&launcher).await;

        let starting = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .start_turn(TurnRequest::new("turn-1", "hold"))
                    .await
                    .map(drop)
            }
        });
        gate.wait_for_spawn().await;
        starting.abort();
        let _ = starting.await;
        gate.release();

        tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.turn_argvs().len() != 1
                || launcher.kill_requests() != 1
                || launcher.a_child_is_running()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected the abandoned spawn owner to launch and reap its late child");
        assert_eq!(
            launcher.turn_argvs().len(),
            1,
            "expected the abandoned launch to return exactly one child"
        );
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected the abandoned launch owner to issue one child reap"
        );
    }

    #[tokio::test]
    async fn dropping_a_failed_start_releases_its_reservation_for_a_retry() {
        let launcher = Arc::new(FailingTurnLauncher::new());
        let host = host_under(launcher.clone(), Limits::default());
        let session: Arc<dyn Session> = Arc::from(
            ClaudeHarness::new()
                .open_session(&host, OpenSession::new("chat-1"))
                .await
                .expect("expected a session"),
        );

        let starting = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .start_turn(TurnRequest::new("turn-1", "hold"))
                    .await
                    .map(drop)
            }
        });
        launcher.wait_for_first_turn().await;
        starting.abort();
        let _ = starting.await;
        launcher.release_first_turn();

        let retry = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match session
                    .start_turn(TurnRequest::new("turn-2", "retry after launch failure"))
                    .await
                {
                    Ok(turn) => break turn,
                    Err(error) if matches!(error.cause(), Error::Busy) => {
                        tokio::task::yield_now().await;
                    }
                    Err(error) => {
                        panic!("expected the failed reservation to release, received {error:?}")
                    }
                }
            }
        })
        .await
        .expect("expected a retry to enter after the abandoned launch failed");

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the retried turn to close");
        drop(retry);
    }

    #[tokio::test]
    async fn aborting_a_stopped_start_during_reap_keeps_the_child_owned() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let spawn = launcher.gate_turn_spawns();
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let starting = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .start_turn(TurnRequest::new("turn-1", "hold"))
                    .await
                    .map(drop)
            }
        });
        spawn.wait_for_spawn().await;
        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected the pending spawn to record cancellation");
        spawn.release();
        stop.wait_for_spawn().await;
        starting.abort();
        let _ = starting.await;
        stop.release();

        tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.a_child_is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected the dropped reaper to retain and reap the child");
    }

    #[tokio::test]
    async fn refuses_a_turn_carrying_attachments_rather_than_dropping_them() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let session = open(&launcher).await;
        let request = TurnRequest::new("turn-1", "look at this").with_attachments(vec![
            mango_external_agents::Attachment {
                id: String::from("a1"),
                name: String::from("screenshot.png"),
                mime_type: String::from("image/png"),
                kind: mango_external_agents::AttachmentKind::Image,
                bytes: vec![0x89, 0x50],
            },
        ]);

        let error = session
            .start_turn(request)
            .await
            .expect_err("expected the attachment to be refused rather than silently dropped");
        assert!(
            matches!(
                error.cause(),
                Error::NotSupported {
                    capability: mango_external_agents::Capability::Images
                }
            ),
            "received {error:?}"
        );
        assert!(launcher.turn_argvs().is_empty());
    }

    #[tokio::test]
    async fn answers_no_approval_because_claude_offers_none_to_answer() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let session = open(&launcher).await;
        let error = session
            .respond(PermissionResponse::from_user(
                InteractionId::new("req-1"),
                "allow",
            ))
            .await
            .expect_err("expected a refusal");
        assert!(
            matches!(
                error.cause(),
                Error::NotSupported {
                    capability: mango_external_agents::Capability::InteractiveApprovals
                }
            ),
            "received {error:?}"
        );
    }
}

mod mcp_passthrough {
    use super::*;
    use mango_external_agents::{McpServer, McpTransport};

    fn servers() -> Vec<McpServer> {
        vec![McpServer {
            name: String::from("docs"),
            transport: McpTransport::Stdio {
                command: String::from("docs-mcp"),
                args: vec![String::from("--stdio")],
                env: [(String::from("DOCS_TOKEN"), String::from("s3cret"))]
                    .into_iter()
                    .collect(),
            },
        }]
    }

    async fn open_with_servers(launcher: &Arc<FakeClaudeCli>) -> Box<dyn Session> {
        ClaudeHarness::new()
            .open_session(
                &host(Arc::clone(launcher)),
                OpenSession::new("chat-1").with_mcp_servers(servers()),
            )
            .await
            .expect("expected a session")
    }

    #[tokio::test]
    async fn loads_the_hosts_servers_from_a_file_rather_than_from_the_command_line() {
        let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
        let session = open_with_servers(&launcher).await;
        session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");

        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        let path = value_after(&argv, "--mcp-config").expect("expected the flag");
        assert!(
            argv.iter().all(|argument| !argument.contains("s3cret")),
            "expected no credential on a command line anyone can read, received {argv:?}"
        );

        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).expect("expected the file"))
                .expect("expected valid JSON");
        assert_eq!(written["mcpServers"]["docs"]["command"], "docs-mcp");
        assert_eq!(written["mcpServers"]["docs"]["env"]["DOCS_TOKEN"], "s3cret");
    }

    /// A configured turn commits its settings as the session's defaults when it starts, so a turn
    /// refused after that commit must not leave them behind. Otherwise a cancelled `FullAccess`
    /// start hands its permissions to the next turn that asked for nothing.
    #[test]
    fn a_turn_refused_after_its_lease_release_leaves_no_configuration_behind() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("expected a runtime whose blocking pool this test owns");

        runtime.block_on(async {
            let launcher = Arc::new(
                FakeClaudeCli::new()
                    .with_help(HELP_2_1_270)
                    .with_graceful_interrupt()
                    .with_turn(Run::stalling::<[String; 0], String>([]))
                    .with_turn(Run::replaying(READ_TURN)),
            );
            let scratch =
                std::env::temp_dir().join(format!("mea-config-scratch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
            let host = HostContext::builder()
                .launcher(launcher.clone())
                .cwd(std::env::temp_dir())
                .scratch(&scratch)
                .client_info("mea-tests", "0.0.0")
                .build()
                .expect("expected a host with scratch storage");
            let session = ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1").with_mcp_servers(servers()),
                )
                .await
                .expect("expected a session");

            let (release_pool, pool_held) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = pool_held.recv();
            });

            let mut refused = Box::pin(
                session.start_turn(
                    TurnRequest::new("turn-1", "raise the session's permissions")
                        .with_configuration(
                            ConfigurationPatch::new()
                                .level(ConfigurationChange::Set(PermissionLevel::Default))
                                .routing(ConfigurationChange::Set(ApprovalRouting::User)),
                        ),
                ),
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(250), refused.as_mut())
                    .await
                    .is_err(),
                "expected the configured turn to be parked on the occupied blocking pool"
            );
            session
                .cancel(CancelReason::Requested)
                .await
                .expect("expected the cancel to be accepted");
            drop(release_pool);
            let _ = occupied.await;
            assert!(
                refused.await.is_err(),
                "expected the cancelled turn to be refused"
            );

            assert_eq!(
                session.snapshot().configuration.accepted.level,
                None,
                "expected a refused start to leave the session's own configuration alone"
            );

            let mut next = session
                .start_turn(TurnRequest::new("turn-2", "read note.txt"))
                .await
                .expect("expected the next turn to start");
            drain(&mut next).await;

            let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
            assert_eq!(
                value_after(&argv, "--permission-mode"),
                None,
                "expected the unconfigured turn to inherit nothing from the refused one, received {argv:?}"
            );

            let _ = std::fs::remove_dir_all(&scratch);
        });
    }

    #[test]
    fn close_stays_closing_until_blocked_mcp_cleanup_finishes() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("expected a runtime whose blocking pool this test owns");

        runtime.block_on(async {
            let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
            let scratch =
                std::env::temp_dir().join(format!("mea-close-scratch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
            let host = HostContext::builder()
                .launcher(launcher)
                .cwd(std::env::temp_dir())
                .scratch(&scratch)
                .client_info("mea-tests", "0.0.0")
                .build()
                .expect("expected a host with scratch storage");
            let session: Arc<dyn Session> = Arc::from(
                ClaudeHarness::new()
                    .open_session(
                        &host,
                        OpenSession::new("chat-1").with_mcp_servers(servers()),
                    )
                    .await
                    .expect("expected a session"),
            );
            let mut state_changes = session.subscribe();

            let (release_pool, pool_held) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = pool_held.recv();
            });
            let closing = tokio::spawn({
                let session = Arc::clone(&session);
                async move { session.close(CloseReason::Requested).await }
            });

            let closing_state =
                tokio::time::timeout(Duration::from_secs(5), state_changes.changed())
                    .await
                    .expect("expected close to publish a state change")
                    .expect("expected the session state to stay alive");
            assert_eq!(closing_state.status, SessionStatus::Closing);
            assert!(
                !closing.is_finished(),
                "expected close to await the blocked MCP cleanup"
            );

            drop(release_pool);
            occupied
                .await
                .expect("expected the blocking task to finish");
            closing
                .await
                .expect("expected the close task to finish")
                .expect("expected MCP cleanup to succeed");

            let closed_state =
                tokio::time::timeout(Duration::from_secs(5), state_changes.changed())
                    .await
                    .expect("expected cleanup completion to publish a state change")
                    .expect("expected the session state to stay alive");
            assert_eq!(closed_state.status, SessionStatus::Closed);

            let _ = std::fs::remove_dir_all(&scratch);
        });
    }

    /// The guard reaps a child nobody else owns. When a `cancel` already took the turn it also
    /// killed that child, so a guard firing afterwards would hand a host's launcher a second
    /// teardown for the same process — `ProcessControl::kill` carries no idempotence promise.
    #[test]
    fn a_stop_that_already_reaped_the_child_is_not_asked_to_reap_it_twice() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("expected a runtime whose blocking pool this test owns");

        runtime.block_on(async {
            let launcher = Arc::new(
                FakeClaudeCli::new()
                    .with_help(HELP_2_1_270)
                    .with_turn(Run::stalling::<[String; 0], String>([])),
            );
            let scratch =
                std::env::temp_dir().join(format!("mea-twice-scratch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
            let host = HostContext::builder()
                .launcher(launcher.clone())
                .cwd(std::env::temp_dir())
                .scratch(&scratch)
                .client_info("mea-tests", "0.0.0")
                .build()
                .expect("expected a host with scratch storage");
            let session = ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1").with_mcp_servers(servers()),
                )
                .await
                .expect("expected a session");

            let (release_pool, pool_held) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = pool_held.recv();
            });

            // Boxed rather than dropped by the timeout, so the stop lands first and the guard
            // runs second — the ordering where the turn is no longer the guard's to reap.
            let mut start =
                Box::pin(session.start_turn(TurnRequest::new("turn-1", "start something long")));
            assert!(
                tokio::time::timeout(Duration::from_millis(250), start.as_mut())
                    .await
                    .is_err(),
                "expected the turn to be parked on the occupied blocking pool"
            );
            session
                .cancel(CancelReason::Requested)
                .await
                .expect("expected the cancel to be accepted");
            drop(start);

            drop(release_pool);
            let _ = occupied.await;
            tokio::time::sleep(Duration::from_millis(100)).await;

            assert_eq!(
                launcher.kill_requests(),
                1,
                "expected the library to ask once, as ProcessControl::kill documents"
            );

            let _ = std::fs::remove_dir_all(&scratch);
        });
    }

    /// A cancellation can own a child while `start_turn` is still waiting to release the MCP
    /// lease. Dropping that start then joins the retained cancellation result; it also has to
    /// settle the no-stream owner, or the finished child leaves the session permanently busy.
    #[test]
    fn dropping_a_start_joins_and_settles_a_cancellation_that_already_owns_its_child() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("expected a runtime whose blocking pool this test owns");

        runtime.block_on(async {
            let launcher = Arc::new(
                FakeClaudeCli::new()
                    .with_help(HELP_2_1_270)
                    .with_graceful_interrupt()
                    .with_turn(Run::stalling::<[String; 0], String>([]))
                    .with_turn(Run::replaying(READ_TURN)),
            );
            let stop = launcher.gate_turn_stops();
            let scratch =
                std::env::temp_dir().join(format!("mea-settle-scratch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
            let host = HostContext::builder()
                .launcher(launcher.clone())
                .cwd(std::env::temp_dir())
                .scratch(&scratch)
                .client_info("mea-tests", "0.0.0")
                .build()
                .expect("expected a host with scratch storage");
            let session: Arc<dyn Session> = Arc::from(
                ClaudeHarness::new()
                    .open_session(
                        &host,
                        OpenSession::new("chat-1").with_mcp_servers(servers()),
                    )
                    .await
                    .expect("expected a session"),
            );

            let (release_pool, pool_held) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = pool_held.recv();
            });
            let starting = tokio::spawn({
                let session = Arc::clone(&session);
                async move {
                    session
                        .start_turn(TurnRequest::new("turn-1", "hold"))
                        .await
                        .map(drop)
                }
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while launcher.turn_argvs().len() != 1 || !launcher.a_child_is_running() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("expected the start to install a child before its blocked lease release");

            let cancelling = tokio::spawn({
                let session = Arc::clone(&session);
                async move { session.cancel(CancelReason::Requested).await }
            });
            stop.wait_for_spawn().await;
            starting.abort();
            let _ = starting.await;
            drop(release_pool);
            occupied
                .await
                .expect("expected the blocking task to finish");
            stop.release();
            cancelling
                .await
                .expect("expected cancellation to return")
                .expect("expected the cancellation-owned teardown to finish");

            let mut retry = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match session
                        .start_turn(TurnRequest::new("turn-2", "resume after cleanup"))
                        .await
                    {
                        Ok(turn) => break turn,
                        Err(error) if matches!(error.cause(), Error::Busy) => {
                            tokio::task::yield_now().await;
                        }
                        Err(error) => panic!(
                            "expected the abandoned start to settle admission, received {error:?}"
                        ),
                    }
                }
            })
            .await
            .expect("expected the settled cancellation owner to release admission");
            drain(&mut retry).await;
            stop.wait_for_spawn().await;
            stop.release();
            session
                .close(CloseReason::Requested)
                .await
                .expect("expected a clean close after the retry");
            let _ = std::fs::remove_dir_all(&scratch);
        });
    }

    /// Session drop records the shared teardown, but a `ProcessControl` that is merely dropped is
    /// not killed. So a `start_turn` abandoned at the lease release must join that teardown or
    /// start it with the child it launched, or the process outlives the host's interest in it.
    #[test]
    fn a_dropped_start_turn_reaps_the_child_it_launched() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("expected a runtime whose blocking pool this test owns");

        runtime.block_on(async {
            let launcher = Arc::new(
                FakeClaudeCli::new()
                    .with_help(HELP_2_1_270)
                    .with_turn(Run::stalling::<[String; 0], String>([])),
            );
            let scratch =
                std::env::temp_dir().join(format!("mea-reap-scratch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
            let host = HostContext::builder()
                .launcher(launcher.clone())
                .cwd(std::env::temp_dir())
                .scratch(&scratch)
                .client_info("mea-tests", "0.0.0")
                .build()
                .expect("expected a host with scratch storage");
            let session = ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1").with_mcp_servers(servers()),
                )
                .await
                .expect("expected a session");

            let (release_pool, pool_held) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = pool_held.recv();
            });

            let start = session.start_turn(TurnRequest::new("turn-1", "start something long"));
            let abandoned = tokio::time::timeout(Duration::from_millis(250), start).await;
            assert!(
                abandoned.is_err(),
                "expected the turn to be parked on the occupied blocking pool"
            );

            drop(release_pool);
            let _ = occupied.await;
            // The reap runs on a task, because `Drop` cannot await a kill.
            tokio::time::sleep(Duration::from_millis(100)).await;

            assert_eq!(
                launcher.mcp_config_at_kill().len(),
                1,
                "expected the abandoned start to kill the child it launched"
            );

            let _ = std::fs::remove_dir_all(&scratch);
        });
    }

    /// The release is an await, so a `cancel`, a `close` or a second `start_turn` can take this
    /// turn while it is parked there. Resuming and spawning the pump anyway would hand back a
    /// successful stream for a turn that was already stopped, and write its prompt to a child
    /// somebody is killing.
    ///
    /// The returned result is the assertion that matters: whether the prompt reaches a dying child
    /// depends on when the kill lands, but a stopped turn must never answer `Ok`.
    #[test]
    fn a_turn_stopped_while_its_lease_is_released_is_refused_rather_than_started() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("expected a runtime whose blocking pool this test owns");

        runtime.block_on(async {
            let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
            let scratch =
                std::env::temp_dir().join(format!("mea-stop-scratch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
            let host = HostContext::builder()
                .launcher(launcher.clone())
                .cwd(std::env::temp_dir())
                .scratch(&scratch)
                .client_info("mea-tests", "0.0.0")
                .build()
                .expect("expected a host with scratch storage");
            let session = ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1").with_mcp_servers(servers()),
                )
                .await
                .expect("expected a session");

            let (release_pool, pool_held) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = pool_held.recv();
            });

            let start = session.start_turn(TurnRequest::new("turn-1", "read note.txt"));
            tokio::pin!(start);
            // The future is held rather than dropped, so it resumes after the stop lands.
            assert!(
                tokio::time::timeout(Duration::from_millis(250), start.as_mut())
                    .await
                    .is_err(),
                "expected the turn to be parked on the occupied blocking pool"
            );

            session
                .cancel(CancelReason::Requested)
                .await
                .expect("expected the cancel to be accepted");

            drop(release_pool);
            let _ = occupied.await;

            // `TurnStream` is not `Debug`, so the success arm is named rather than unwrapped.
            let error = match start.await {
                Ok(_) => panic!("expected a stopped turn to be refused, received a stream"),
                Err(error) => error,
            };
            assert!(
                matches!(error.cause(), Error::Cancelled { .. }),
                "expected the recorded cancellation, received {error:?}"
            );
            // The cancel took the turn and killed its child. `ProcessControl::kill` is not
            // documented idempotent, so the refusal must not ask a host's launcher to tear the
            // same child down twice.
            assert_eq!(
                launcher.kill_requests(),
                1,
                "expected the library to ask once, as ProcessControl::kill documents"
            );

            let _ = std::fs::remove_dir_all(&scratch);
        });
    }

    /// A host that times out or drops `start_turn` must not leave Claude working on a turn it will
    /// never read. The MCP release runs on the blocking pool, so it is an await the future can be
    /// dropped at — and anything spawned before it keeps running afterwards.
    ///
    /// The pool is given exactly one thread and that thread is occupied, so the release is
    /// deterministically pending rather than "probably slow": the future parks there every run.
    #[test]
    fn a_dropped_start_turn_leaves_claude_no_prompt_to_work_on() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("expected a runtime whose blocking pool this test owns");

        runtime.block_on(async {
            let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
            let scratch =
                std::env::temp_dir().join(format!("mea-drop-scratch-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
            let host = HostContext::builder()
                .launcher(launcher.clone())
                .cwd(std::env::temp_dir())
                .scratch(&scratch)
                .client_info("mea-tests", "0.0.0")
                .build()
                .expect("expected a host with scratch storage");
            let session = ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1").with_mcp_servers(servers()),
                )
                .await
                .expect("expected a session");

            // Take the pool's only thread and hold it, so the turn's release cannot complete.
            let (release_pool, pool_held) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = pool_held.recv();
            });

            let start = session.start_turn(TurnRequest::new("turn-1", "read note.txt"));
            // Long enough for a pump, had one been spawned, to write its single prompt line.
            let abandoned = tokio::time::timeout(Duration::from_millis(250), start).await;
            assert!(
                abandoned.is_err(),
                "expected the turn to still be parked on the occupied blocking pool"
            );

            drop(release_pool);
            let _ = occupied.await;
            tokio::time::sleep(Duration::from_millis(50)).await;

            let written = launcher.written();
            assert!(
                written.is_empty(),
                "expected an abandoned start to send Claude nothing, received {written:?}"
            );

            let _ = std::fs::remove_dir_all(&scratch);
        });
    }

    #[tokio::test]
    async fn passes_the_host_authorised_scratch_path_to_the_child_unchanged() {
        let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
        let scratch =
            std::env::temp_dir().join(format!("mea-host-scratch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(std::env::temp_dir())
            .scratch(&scratch)
            .client_info("mea-tests", "0.0.0")
            .build()
            .expect("expected a host with scratch storage");
        let session = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1").with_mcp_servers(servers()),
            )
            .await
            .expect("expected a session");
        session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");

        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        let config = std::path::PathBuf::from(
            value_after(&argv, "--mcp-config").expect("expected the config argument"),
        );
        assert!(
            config.starts_with(&scratch),
            "expected the child to receive an artifact below the host root"
        );

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected cleanup");
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test]
    async fn refuses_missing_host_scratch_before_running_a_vendor_probe() {
        let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(std::env::temp_dir())
            .client_info("mea-tests", "0.0.0")
            .build()
            .expect("expected a host without scratch storage");

        let error = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1").with_mcp_servers(servers()),
            )
            .await
            .map(drop)
            .expect_err("expected MCP setup without host scratch to be refused");
        assert!(
            matches!(
                error.cause(),
                Error::HostConfiguration {
                    expected: "a host-owned scratch directory for MCP configuration",
                    ..
                }
            ),
            "received {error:?}"
        );
        assert!(
            launcher.launches().is_empty(),
            "expected scratch refusal before any vendor work, received {:?}",
            launcher.launches()
        );
    }

    #[tokio::test]
    async fn refuses_an_unusable_host_scratch_before_running_a_vendor_probe() {
        let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
        let root = std::env::temp_dir().join(format!("mea-host-scratch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).expect("expected a dedicated test root");
        let scratch_file = root.join("not-a-directory");
        std::fs::write(&scratch_file, "host-owned file")
            .expect("expected an unusable scratch path");
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(std::env::temp_dir())
            .scratch(&scratch_file)
            .client_info("mea-tests", "0.0.0")
            .build()
            .expect("expected a host with a supplied scratch path");

        let error = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1").with_mcp_servers(servers()),
            )
            .await
            .map(drop)
            .expect_err("expected a non-directory scratch path to be refused");
        assert!(
            matches!(error.cause(), Error::HostConfiguration { .. }),
            "received {error:?}"
        );
        assert!(
            launcher.launches().is_empty(),
            "expected scratch refusal before any vendor work, received {:?}",
            launcher.launches()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_failed_open_removes_the_prepared_host_scratch_artifact() {
        // This build does not advertise `--mcp-config`, so opening fails after the local artifact
        // was prepared and the survey established the unsupported capability.
        let launcher = Arc::new(FakeClaudeCli::new());
        let scratch =
            std::env::temp_dir().join(format!("mea-host-scratch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&scratch).expect("expected a dedicated scratch root");
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(std::env::temp_dir())
            .scratch(&scratch)
            .client_info("mea-tests", "0.0.0")
            .build()
            .expect("expected a host with scratch storage");

        let error = ClaudeHarness::new()
            .open_session(
                &host,
                OpenSession::new("chat-1").with_mcp_servers(servers()),
            )
            .await
            .map(drop)
            .expect_err("expected unsupported MCP passthrough to refuse opening");
        assert!(
            matches!(
                error.cause(),
                Error::NotSupported {
                    capability: mango_external_agents::Capability::McpPassthrough
                }
            ),
            "received {error:?}"
        );
        assert_eq!(
            std::fs::read_dir(&scratch)
                .expect("expected the host scratch root")
                .count(),
            0,
            "expected a failed open to remove its prepared artifact"
        );
        let _ = std::fs::remove_dir_all(&scratch);
    }

    #[tokio::test]
    async fn the_file_leaves_with_the_session_that_wrote_it() {
        let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
        let session = open_with_servers(&launcher).await;
        session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");
        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        let path = std::path::PathBuf::from(
            value_after(&argv, "--mcp-config").expect("expected the flag"),
        );
        assert!(path.exists());

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected a clean close");
        assert!(
            !path.exists(),
            "expected closing to remove {}",
            path.display()
        );
    }

    /// The file outlives the child that was launched pointing at it.
    ///
    /// `--mcp-config` is a path the vendor reads at startup, so a close that unlinks it before the
    /// kill lands leaves a starting child reading a configuration that is no longer there — a turn
    /// running without the servers somebody set up rather than a turn that stopped. The unlink also
    /// has to happen off the session lock, which every other method needs while it runs.
    #[tokio::test]
    async fn keeps_the_file_until_the_turn_it_configured_has_been_killed() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_help(HELP_2_1_270)
                .with_turn(Run::stalling::<[String; 0], String>([])),
        );
        let session = open_with_servers(&launcher).await;
        session
            .start_turn(TurnRequest::new("turn-1", "start something long"))
            .await
            .expect("expected a turn");
        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        let path = std::path::PathBuf::from(
            value_after(&argv, "--mcp-config").expect("expected the flag"),
        );

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected a clean close");

        assert_eq!(
            launcher.mcp_config_at_kill(),
            vec![true],
            "expected the config file to still exist when the child was killed"
        );
        assert!(
            !path.exists(),
            "expected the close to still remove {}",
            path.display()
        );
    }

    /// A launcher can still be creating a child when `close` takes the session's configuration.
    /// The child reads `--mcp-config` at startup, so the start call keeps its own lease until it
    /// observes the close, kills the child, and only then lets cleanup remove the file.
    #[tokio::test]
    async fn keeps_mcp_configuration_until_a_child_spawned_during_close_has_been_killed() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_help(HELP_2_1_270)
                .with_turn(Run::stalling::<[String; 0], String>([])),
        );
        let gate = launcher.gate_turn_spawns();
        let session: Arc<dyn Session> = Arc::from(open_with_servers(&launcher).await);

        let starting = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .start_turn(TurnRequest::new("turn-1", "start something long"))
                    .await
                    .map(drop)
            }
        });

        gate.wait_for_spawn().await;
        let closing = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.close(CloseReason::Requested).await }
        });
        gate.release();

        closing
            .await
            .expect("expected the close task to finish")
            .expect("expected a clean close");

        let outcome = starting.await.expect("expected the start task to finish");
        assert!(
            matches!(outcome, Err(ref error) if matches!(error.cause(), Error::Closed { .. })),
            "expected the closed session to refuse the turn, received {outcome:?}"
        );
        assert_eq!(
            launcher.mcp_config_at_kill(),
            vec![true],
            "expected the spawned child to keep its MCP configuration until it was killed"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_native_close_keeps_the_mcp_artifact_after_the_session_drops() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_help(HELP_2_1_270)
                .with_turn(Run::stalling::<[String; 0], String>([])),
        );
        let stop = launcher.gate_turn_stops();
        let host = host_under(
            launcher.clone(),
            Limits {
                kill_grace: Duration::from_secs(1),
                shutdown_timeout: Duration::from_secs(1),
                ..Limits::default()
            },
        );
        let session: Arc<dyn Session> = Arc::from(
            ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1").with_mcp_servers(servers()),
                )
                .await
                .expect("expected a session"),
        );
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");
        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        let path = std::path::PathBuf::from(
            value_after(&argv, "--mcp-config").expect("expected the config argument"),
        );

        let closing = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.close(CloseReason::Requested).await }
        });
        tokio::time::timeout(Duration::from_secs(5), stop.wait_for_spawn())
            .await
            .expect("expected close to issue its one bounded process-stop request");
        tokio::time::advance(Duration::from_secs(2)).await;

        let error = closing
            .await
            .expect("expected the close task to finish")
            .expect_err("expected the bounded native cleanup to fail");
        assert!(
            matches!(error.cause(), Error::Timeout { .. }),
            "received {error:?}"
        );
        assert_eq!(session.snapshot().status, SessionStatus::Closing);
        assert!(
            path.exists(),
            "expected a live child to keep its MCP artifact after cleanup failed"
        );

        stop.release();
        drop(turn);
        drop(session);
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected the failed close and session drop to share one stop request"
        );
        assert!(
            path.exists(),
            "expected the preserved artifact to outlive the failed close after session drop"
        );
        assert!(
            launcher.a_child_is_running(),
            "expected a timed-out host-owned child to remain live until host cleanup"
        );
        launcher.end_turns_for_host_cleanup();
        assert!(
            launcher.all_children_ended(),
            "expected explicit fake host cleanup to end the retained child"
        );
        let _ = std::fs::remove_dir_all(
            path.parent()
                .expect("expected the config file to have a private directory"),
        );
    }

    #[tokio::test]
    async fn an_artifact_removal_failure_still_closes_the_session() {
        let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
        let session = open_with_servers(&launcher).await;
        session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");
        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        let path = std::path::PathBuf::from(
            value_after(&argv, "--mcp-config").expect("expected the config argument"),
        );
        std::fs::remove_dir_all(
            path.parent()
                .expect("expected the config file to have a private directory"),
        )
        .expect("expected the test to remove the artifact first");

        let error = session
            .close(CloseReason::Requested)
            .await
            .expect_err("expected close to report the failed artifact removal");
        assert!(
            matches!(error.cause(), Error::HostConfiguration { .. }),
            "received {error:?}"
        );
        assert_eq!(
            session.snapshot().status,
            SessionStatus::Closed,
            "expected native cleanup to close the session despite artifact removal failure"
        );
    }

    #[tokio::test]
    async fn refuses_a_build_that_cannot_load_them_rather_than_dropping_them() {
        // The 2.1.260 excerpt declares no `--mcp-config`.
        let launcher = Arc::new(FakeClaudeCli::new());
        let error = ClaudeHarness::new()
            .open_session(
                &host(Arc::clone(&launcher)),
                OpenSession::new("chat-1").with_mcp_servers(servers()),
            )
            .await
            .map(drop)
            .expect_err("expected a refusal rather than a session without the servers");
        assert!(
            matches!(
                error.cause(),
                Error::NotSupported {
                    capability: mango_external_agents::Capability::McpPassthrough
                }
            ),
            "received {error:?}"
        );
    }

    #[tokio::test]
    async fn a_session_with_no_servers_writes_no_file_and_passes_no_flag() {
        let launcher = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
        let session = open(&launcher).await;
        session
            .start_turn(TurnRequest::new("turn-1", "hello"))
            .await
            .expect("expected a turn");
        let argv = launcher.turn_argvs().pop().expect("expected a turn launch");
        assert_eq!(value_after(&argv, "--mcp-config"), None);
    }

    #[tokio::test]
    async fn reports_the_capability_only_on_a_build_that_declares_the_flag() {
        let declaring = Arc::new(FakeClaudeCli::new().with_help(HELP_2_1_270));
        let discovery = ClaudeHarness::new()
            .discover(&host(declaring))
            .await
            .expect("expected a discovery");
        assert!(discovery.capabilities.capabilities().mcp_passthrough);

        let silent = Arc::new(FakeClaudeCli::new());
        let discovery = ClaudeHarness::new()
            .discover(&host(silent))
            .await
            .expect("expected a discovery");
        assert!(!discovery.capabilities.capabilities().mcp_passthrough);
    }
}

mod cancelling_and_closing {
    use super::*;

    async fn wait_for_gate(gate: &SpawnGate, expectation: &'static str) {
        tokio::time::timeout(Duration::from_secs(5), gate.wait_for_spawn())
            .await
            .expect(expectation);
    }

    async fn drain_blocked_prompt(
        turn: &mut TurnStream,
        expectation: &'static str,
    ) -> Vec<EventKind> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            while let Some(event) = turn.recv().await {
                let terminal = event.is_terminal();
                events.push(event.kind);
                if terminal {
                    break;
                }
            }
            events
        })
        .await
        .expect(expectation)
    }

    async fn wait_for_reap(launcher: &FakeClaudeCli) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.a_child_is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected the turn child to be reaped");
    }

    #[tokio::test(start_paused = true)]
    async fn a_prompt_write_honors_the_hosts_request_deadline() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let write = launcher.gate_turn_input_writes();
        let host = host_under(
            launcher.clone(),
            Limits {
                request_timeout: Duration::from_secs(1),
                ..Limits::default()
            },
        );
        let session = ClaudeHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        wait_for_gate(
            &write,
            "expected the blocked prompt write to begin before its deadline",
        )
        .await;
        tokio::time::sleep(Duration::from_secs(5)).await;
        for _ in 0..10 {
            if launcher.kill_requests() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected the request deadline to stop the blocked prompt write"
        );
        let events = drain_blocked_prompt(
            &mut turn,
            "expected the request deadline to commit a terminal prompt error",
        )
        .await;
        assert!(
            events.iter().any(|event| {
                matches!(event, EventKind::Error { error } if error.message.contains("Claude prompt input"))
            }),
            "expected the prompt deadline to reach the stream, received {events:?}"
        );
        wait_for_reap(&launcher).await;
    }

    #[tokio::test]
    async fn dropping_a_stream_interrupts_a_blocked_prompt_write() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let write = launcher.gate_turn_input_writes();
        let stop = launcher.gate_turn_stops();
        let session = open(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        wait_for_gate(
            &write,
            "expected the blocked prompt write before stream abandonment",
        )
        .await;
        drop(turn);
        wait_for_gate(
            &stop,
            "expected stream abandonment to request a stop while stdin stayed blocked",
        )
        .await;
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected stream abandonment to claim one prompt teardown"
        );
        stop.release();
        wait_for_reap(&launcher).await;
    }

    #[tokio::test]
    async fn host_shutdown_interrupts_a_blocked_prompt_close() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let close = launcher.gate_turn_input_closes();
        let stop = launcher.gate_turn_stops();
        let host = host_under(launcher.clone(), Limits::default());
        let session = ClaudeHarness::new()
            .open_session(&host, OpenSession::new("chat-1"))
            .await
            .expect("expected a session");
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        wait_for_gate(
            &close,
            "expected the blocked prompt close before host shutdown",
        )
        .await;
        host.cancel().cancel();
        wait_for_gate(
            &stop,
            "expected host shutdown to request a stop while stdin close stayed blocked",
        )
        .await;
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected host shutdown to claim one prompt teardown"
        );
        stop.release();
        wait_for_reap(&launcher).await;
        drop(turn);
    }

    #[tokio::test]
    async fn a_session_cancel_interrupts_a_blocked_prompt_write() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let write = launcher.gate_turn_input_writes();
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        wait_for_gate(
            &write,
            "expected the blocked prompt write before session cancellation",
        )
        .await;
        let cancelling = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        wait_for_gate(
            &stop,
            "expected session cancellation to request a stop while stdin stayed blocked",
        )
        .await;
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected session cancellation to claim one prompt teardown"
        );
        stop.release();
        cancelling
            .await
            .expect("expected cancellation task to finish")
            .expect("expected cancellation to finish its teardown");
        let events = drain_blocked_prompt(
            &mut turn,
            "expected session cancellation to commit a terminal cancellation",
        )
        .await;
        assert!(
            events.contains(&EventKind::Cancelled {
                reason: CancelReason::Requested
            }),
            "expected blocked prompt cancellation to reach the stream, received {events:?}"
        );
    }

    #[tokio::test]
    async fn closing_a_session_interrupts_a_blocked_prompt_write() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let write = launcher.gate_turn_input_writes();
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        wait_for_gate(
            &write,
            "expected the blocked prompt write before session close",
        )
        .await;
        let closing = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.close(CloseReason::Requested).await }
        });
        wait_for_gate(
            &stop,
            "expected session close to request a stop while stdin stayed blocked",
        )
        .await;
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected close to claim one prompt teardown"
        );
        stop.release();
        closing
            .await
            .expect("expected close task to finish")
            .expect("expected close to finish its teardown");
        wait_for_reap(&launcher).await;
        drop(turn);
    }

    #[tokio::test]
    async fn cancellation_uses_a_supported_graceful_interrupt_before_escalating() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_graceful_interrupt()
                .with_turn(Run::stalling(Vec::<String>::new())),
        );
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected cancellation to stop the turn");
        let events = drain(&mut turn).await;

        assert_eq!(
            launcher.graceful_interrupts(),
            1,
            "expected the harness to ask the supported launcher for one graceful interrupt"
        );
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected process-tree cleanup after the graceful interrupt exited the leader"
        );
        assert!(
            events.contains(&EventKind::Cancelled {
                reason: CancelReason::Requested
            }),
            "expected the interrupted turn to retain its cancellation marker, received {events:?}"
        );
    }

    /// `ProcessControl::kill` documents that the library asks once. A cancel claims the turn and
    /// owns its stop, and the pump's own teardown then has nothing left to ask for — including
    /// when Claude's `result` lands while that cancel is still inside the launcher. Asking twice
    /// is not free: the real `TokioChild` refuses the second ask while the first is still inside
    /// its grace, so the cancel that actually stopped the child reports a failed teardown.
    #[tokio::test]
    async fn a_result_landing_under_a_cancel_never_asks_the_launcher_to_stop_twice() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        let cancelling = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        // The cancel owns the stop and is inside the launcher: its ask is already counted.
        stop.wait_for_spawn().await;
        assert_eq!(launcher.kill_requests(), 1);

        // Claude answers anyway. The pump now reaches its terminal with a finished reducer *and* a
        // recorded stop reason, which is the one combination that used to ask a second time.
        launcher.announce_to_turn(r#"{"type":"result","is_error":false}"#);
        let events = drain(&mut turn).await;
        assert!(
            events.contains(&EventKind::Completed),
            "expected the vendor's own result to end the turn, received {events:?}"
        );

        // Settled well past the point the second ask would have been made: it is issued as soon as
        // the launcher reports no graceful interrupt, with nothing awaited in between.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected the cancel that owns this turn to be the only caller to ask"
        );

        stop.release();
        cancelling
            .await
            .expect("expected the cancel task to finish")
            .expect("expected the cancel to report the stop it owns");
    }

    #[tokio::test]
    async fn a_cancel_waits_for_the_teardown_started_by_a_native_result() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::stalling::<[String; 0], String>([]))
                .with_turn(Run::replaying(READ_TURN)),
        );
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        launcher.announce_to_turn(r#"{"type":"result","is_error":false}"#);
        stop.wait_for_spawn().await;
        let cancelling = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stop.wait_for_spawn())
                .await
                .is_err(),
            "expected the native terminal teardown to remain the only process stop owner"
        );
        assert!(
            !cancelling.is_finished(),
            "expected a later cancel to wait for the native teardown"
        );

        stop.release();
        cancelling
            .await
            .expect("expected cancellation task to finish")
            .expect("expected cancellation to observe the native teardown");
        let events = drain(&mut turn).await;
        assert!(
            events.contains(&EventKind::Completed),
            "expected the native result to remain terminal, received {events:?}"
        );
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected native completion and cancel to share one teardown"
        );
        let mut resumed = session
            .start_turn(TurnRequest::new("turn-2", "resume after native completion"))
            .await
            .expect("expected the native result owner to preserve continuation");
        drain(&mut resumed).await;
        stop.wait_for_spawn().await;
        stop.release();
    }

    #[tokio::test]
    async fn dropping_a_session_joins_an_inflight_cancel_teardown() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        let cancelling = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        stop.wait_for_spawn().await;
        cancelling.abort();
        let _ = cancelling.await;
        drop(session);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stop.wait_for_spawn())
                .await
                .is_err(),
            "expected session drop to join the in-flight teardown instead of stopping twice"
        );

        stop.release();
        wait_for_reap(&launcher).await;
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected session drop and cancellation to share one teardown"
        );
        drop(turn);
    }

    #[tokio::test]
    async fn a_second_cancel_waits_for_the_first_claimed_teardown() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        let first = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        stop.wait_for_spawn().await;
        let second = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !second.is_finished(),
            "expected the second cancel to wait for the claimed teardown"
        );

        stop.release();
        first
            .await
            .expect("expected first cancellation task to finish")
            .expect("expected first cancellation to finish its teardown");
        second
            .await
            .expect("expected second cancellation task to finish")
            .expect("expected second cancellation to observe the first teardown");
        assert_eq!(
            launcher.kill_requests(),
            1,
            "expected both cancellations to share one teardown"
        );
        drop(turn);
    }

    #[tokio::test]
    async fn a_stopping_turn_remains_busy_until_its_process_is_reaped() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_graceful_interrupt()
                .with_turn(Run::stalling::<[String; 0], String>([]))
                .with_turn(Run::replaying(READ_TURN)),
        );
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let first = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        let cancelling = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        stop.wait_for_spawn().await;

        let error = session
            .start_turn(TurnRequest::new("turn-2", "must not overlap"))
            .await
            .expect_err("expected native teardown to retain admission ownership");
        assert!(matches!(error.cause(), Error::Busy), "received {error:?}");

        stop.release();
        cancelling
            .await
            .expect("expected cancellation task to finish")
            .expect("expected reaped cancellation to succeed");
        drop(first);

        let mut next = session
            .start_turn(TurnRequest::new("turn-3", "resume after terminal cleanup"))
            .await
            .expect("expected the terminal owner to release after successful cleanup");
        drain(&mut next).await;
    }

    #[tokio::test]
    async fn aborting_cancel_after_native_teardown_starts_still_reaps_the_child() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        let cancelling = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        stop.wait_for_spawn().await;
        cancelling.abort();
        let _ = cancelling.await;
        stop.release();

        tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.a_child_is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(
            "expected the abandoned cancellation worker to reap its child without another call",
        );
        drop(turn);
    }

    #[tokio::test]
    async fn a_second_close_waits_for_the_abandoned_close_workers_result() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let stop = launcher.gate_turn_stops();
        let session = shared(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        let closing = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.close(CloseReason::Requested).await }
        });
        stop.wait_for_spawn().await;
        closing.abort();
        let _ = closing.await;

        let repeated = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.close(CloseReason::Shutdown).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !repeated.is_finished(),
            "expected a repeated close to wait for the active teardown"
        );

        stop.release();
        repeated
            .await
            .expect("expected the repeated close task to finish")
            .expect("expected the durable close worker to report native cleanup");
        assert!(
            launcher.all_children_ended(),
            "expected the abandoned close worker to reap its child"
        );
        drop(turn);
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_stop_reports_failure_and_retains_the_turn_owner() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let stop = launcher.gate_turn_stops();
        let host = host_under(
            launcher.clone(),
            Limits {
                kill_grace: Duration::from_secs(1),
                shutdown_timeout: Duration::from_secs(1),
                ..Limits::default()
            },
        );
        let session: Arc<dyn Session> = Arc::from(
            ClaudeHarness::new()
                .open_session(&host, OpenSession::new("chat-1"))
                .await
                .expect("expected a session"),
        );
        let first = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        let cancelling = tokio::spawn({
            let session = Arc::clone(&session);
            async move { session.cancel(CancelReason::Requested).await }
        });
        stop.wait_for_spawn().await;
        tokio::time::advance(Duration::from_secs(1)).await;

        let error = cancelling
            .await
            .expect("expected the cancellation task to return")
            .expect_err("expected the bounded stop to report timeout");
        assert!(
            matches!(error.cause(), Error::Timeout { .. }),
            "expected a cleanup timeout, received {error:?}"
        );
        let error = session
            .start_turn(TurnRequest::new("turn-2", "must not overlap"))
            .await
            .expect_err("expected the un-reaped owner to keep admission closed");
        assert!(matches!(error.cause(), Error::Busy), "received {error:?}");

        stop.release();
        drop(first);
    }

    #[tokio::test]
    async fn closes_the_stream_without_putting_a_failure_in_the_transcript() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::stalling([
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"work"}}}"#,
        ])));
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "start something long"))
            .await
            .expect("expected a turn");

        // The child has to be alive before the cancel, or the test proves the teardown is
        // unnecessary rather than that it works. `TurnStarted` is always first and carries no
        // proof the vendor process is actually running yet, so the delta after it is what this
        // waits for.
        let started = tokio::time::timeout(Duration::from_secs(5), turn.recv())
            .await
            .expect("expected the stream to open");
        assert!(matches!(
            started.map(|event| event.kind),
            Some(EventKind::TurnStarted { .. })
        ));
        let first = tokio::time::timeout(Duration::from_secs(5), turn.recv())
            .await
            .expect("expected a delta after the turn started");
        assert!(matches!(
            first.map(|event| event.kind),
            Some(EventKind::TextDelta { .. })
        ));
        assert!(
            launcher.a_child_is_running(),
            "expected the turn's child to still be running before the cancel"
        );

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected the cancel to land");
        let events = drain(&mut turn).await;

        assert_eq!(
            events.last(),
            Some(&EventKind::Completed),
            "expected the cancellation marker to be followed by a terminal, received {events:?}"
        );
        assert!(
            events.contains(&EventKind::Cancelled {
                reason: CancelReason::Requested
            }),
            "received {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, EventKind::Error { .. })),
            "expected exit 143 to read as a clean stop, received {events:?}"
        );
        assert!(
            launcher.all_children_ended(),
            "expected nothing to outlive the turn"
        );
    }

    #[tokio::test]
    async fn a_cancelled_turn_ends_exactly_once() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling(Vec::<String>::new())));
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("first cancel");
        session
            .cancel(CancelReason::Shutdown)
            .await
            .expect("second cancel");
        session.close(CloseReason::Requested).await.expect("close");

        let events = drain(&mut turn).await;
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventKind::Completed | EventKind::Error { .. }))
                .count(),
            1,
            "received {events:?}"
        );
        assert!(
            events.contains(&EventKind::Cancelled {
                reason: CancelReason::Requested
            }),
            "expected the first reason to win, received {events:?}"
        );
    }

    #[tokio::test]
    async fn closing_twice_is_not_an_error_and_refuses_a_later_turn() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let session = open(&launcher).await;

        session
            .close(CloseReason::Requested)
            .await
            .expect("expected a clean close");
        assert_eq!(
            session.snapshot().status,
            mango_external_agents::SessionStatus::Closed,
            "expected the snapshot to say so, not just the session's own refusal"
        );
        session
            .close(CloseReason::Shutdown)
            .await
            .expect("expected closing twice to be harmless");

        let error = session
            .start_turn(TurnRequest::new("turn-1", "too late"))
            .await
            .expect_err("expected a closed session to refuse a turn");
        assert!(
            matches!(error.cause(), Error::Closed { .. }),
            "received {error:?}"
        );
    }

    #[tokio::test]
    async fn starting_a_second_turn_refuses_while_the_first_is_active() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling(Vec::<String>::new())));
        let session = open(&launcher).await;
        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "one"))
            .await
            .expect("expected a turn");
        let error = session
            .start_turn(TurnRequest::new("turn-2", "two"))
            .await
            .expect_err("expected the active first turn to refuse the second request");
        assert!(
            matches!(error.cause(), Error::Busy),
            "expected a typed busy refusal, received {error:?}"
        );
        assert_eq!(
            launcher.turn_argvs().len(),
            1,
            "expected no replacement launch"
        );

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected cleanup to stop the admitted turn");
        let first_events = drain(&mut first).await;
        assert!(
            first_events.contains(&EventKind::Cancelled {
                reason: CancelReason::Requested
            }),
            "expected the original turn to retain ownership, received {first_events:?}"
        );
    }

    #[tokio::test]
    async fn a_host_that_stops_reading_stops_the_vendor() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::stalling([
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"one"}}}"#,
        ])));
        let session = open(&launcher).await;
        let turn = session
            .start_turn(TurnRequest::new("turn-1", "talk"))
            .await
            .expect("expected a turn");

        drop(turn);

        tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.a_child_is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected dropping the stream to end the vendor's process");
    }

    #[tokio::test]
    async fn dropping_the_owning_session_reaps_an_active_child_even_if_the_stream_survives() {
        let launcher =
            Arc::new(FakeClaudeCli::new().with_turn(Run::stalling::<[String; 0], String>([])));
        let session = open(&launcher).await;
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "hold"))
            .await
            .expect("expected a turn");

        drop(session);

        tokio::time::timeout(Duration::from_secs(5), async {
            while launcher.a_child_is_running() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("expected dropping the session to stop and reap its child");
        let events = drain(&mut turn).await;
        assert!(
            events.contains(&EventKind::Cancelled {
                reason: CancelReason::Shutdown
            }),
            "expected the retained stream to report owner shutdown, received {events:?}"
        );
    }
}
