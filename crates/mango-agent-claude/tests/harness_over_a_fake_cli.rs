//! Discovery, sessions and turns, against a scripted `claude`.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mango_agent_claude::ClaudeHarness;
use mango_external_agents::{
    ApprovalRouting, AuthMode, AuthState, CancelReason, CloseReason, Configuration, Error,
    EventKind, ExecutablePath, GateVerdict, Harness, Limits, LineLimits, OpenSession,
    PermissionLevel, PermissionResponse, ResumeMode, Session, TurnRequest, TurnStream,
};
use support::{
    FakeClaudeCli, HELP_2_1_227, HELP_2_1_270, READ_TURN, Run, SIGNED_OUT, host, host_under,
    value_after,
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

mod discovery {
    use super::*;

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
        assert!(discovery.capabilities.structured_streaming);
        assert!(
            discovery.capabilities.model_catalog,
            "2.1.260 advertises aliases"
        );
        assert!(
            !discovery.capabilities.interactive_approvals,
            "Claude Code delivers no answerable approval over its documented headless surface"
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
            !discovery.capabilities.model_catalog,
            "expected a build advertising no aliases to keep the picker hidden"
        );
        assert!(discovery.models.is_empty());
    }

    #[tokio::test]
    async fn refuses_a_build_that_lost_a_flag_every_turn_passes() {
        let stripped = support::DEFAULT_HELP.replace("--forward-subagent-text", "--forward-txt");
        let launcher = Arc::new(FakeClaudeCli::new().with_help(&stripped));
        let discovery = ClaudeHarness::new()
            .discover(&host(Arc::clone(&launcher)))
            .await
            .expect("expected a discovery");

        assert!(
            matches!(discovery.gate, GateVerdict::VersionTooOld { ref minimum, .. } if minimum == "2.1.211"),
            "received {:?}",
            discovery.gate
        );
        assert!(!discovery.is_usable());
        assert_eq!(
            discovery.capabilities,
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
        assert!(!session.info().resumed);
        assert!(
            launcher.turn_argvs().is_empty(),
            "expected opening to spawn probes only"
        );
    }

    #[tokio::test]
    async fn adopts_a_resume_reference_at_face_value() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let host = host(Arc::clone(&launcher));
        for mode in [ResumeMode::Strict, ResumeMode::Fallback] {
            let session = ClaudeHarness::new()
                .open_session(
                    &host,
                    OpenSession::new("chat-1")
                        .resuming("22222222-3333-4444-5555-666666666666", mode),
                )
                .await
                .expect("expected a session");
            assert!(session.info().resumed);
            assert_eq!(session.info().fallback_reason, None);
            assert_eq!(
                session.ids().native_session_id,
                "22222222-3333-4444-5555-666666666666"
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
                matches!(error, Error::HostConfiguration { .. }),
                "expected a host-configuration refusal for {injected:?}, received {error:?}"
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
            matches!(error, Error::AuthRequired { ref login_hint } if login_hint == "claude auth login"),
            "received {error:?}"
        );
        assert!(launcher.turn_argvs().is_empty());
    }

    #[tokio::test]
    async fn refuses_a_build_too_old_to_drive_before_a_turn_is_spawned() {
        let stripped = support::DEFAULT_HELP.replace("--forward-subagent-text", "--forward-txt");
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_version("2.1.150 (Claude Code)")
                .with_help(&stripped),
        );
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), OpenSession::new("chat-1"))
            .await
            .map(drop)
            .expect_err("expected a refusal");

        assert!(
            matches!(error, Error::VersionGate { ref minimum, .. } if minimum == "2.1.211"),
            "received {error:?}"
        );
    }

    #[tokio::test]
    async fn refuses_an_auto_review_pair_this_account_cannot_run() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_auth(r#"{"loggedIn":true,"authMethod":"apiKey","apiProvider":"firstParty"}"#),
        );
        let request = OpenSession::new("chat-1").with_configuration(Configuration {
            level: PermissionLevel::Default,
            routing: ApprovalRouting::AutoReview,
            ..Configuration::default()
        });
        let error = ClaudeHarness::new()
            .open_session(&host(Arc::clone(&launcher)), request)
            .await
            .map(drop)
            .expect_err("expected a refusal");

        assert!(
            matches!(error, Error::HostConfiguration { .. }),
            "received {error:?}"
        );
        assert!(launcher.turn_argvs().is_empty());
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
    async fn streams_the_recorded_turn_and_closes_with_completed() {
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::replaying(READ_TURN)));
        let session = open(&launcher).await;

        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "read note.txt"))
            .await
            .expect("expected a turn");
        let events = drain(&mut turn).await;

        assert_eq!(turn.native_turn_id, "turn-1");
        assert_eq!(events.last(), Some(&EventKind::Completed));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, EventKind::TextDelta { .. }))
                .count(),
            2
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EventKind::SessionStarted { .. }))
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
    async fn resumes_after_a_cancelled_turn_rather_than_minting_the_same_id_again() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::stalling([
                    r#"{"type":"system","subtype":"init","session_id":"aaaaaaaa-1111-2222-3333-444444444444"}"#,
                ]))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#)),
        );
        let session = open(&launcher).await;

        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "start something long"))
            .await
            .expect("expected a turn");
        // The init has to land before the cancel, or there is nothing to remember.
        let started = tokio::time::timeout(Duration::from_secs(5), first.recv())
            .await
            .expect("expected the init to arrive");
        assert!(matches!(
            started.map(|event| event.kind),
            Some(EventKind::SessionStarted { .. })
        ));

        session
            .cancel(CancelReason::Requested)
            .await
            .expect("expected the cancel to land");
        drain(&mut first).await;

        let mut second = session
            .start_turn(TurnRequest::new("turn-2", "carry on"))
            .await
            .expect("expected a second turn");
        drain(&mut second).await;

        let argvs = launcher.turn_argvs();
        assert_eq!(
            value_after(&argvs[1], "--resume"),
            Some("aaaaaaaa-1111-2222-3333-444444444444")
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

        let EventKind::ActivityCompleted { result, .. } = &events[1] else {
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
        let launcher = Arc::new(FakeClaudeCli::new().with_turn(Run::exiting(1)));
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

    /// `close` while a turn's process is still being spawned.
    ///
    /// `start_turn` reads `closed` before it awaits the spawn and assigns the turn after, so a
    /// `close` that lands in between takes an `active` that is still `None`: it records nothing,
    /// kills nothing, and removes the MCP file the child about to start was launched with. The
    /// turn then arrives on a session that has already answered every other caller `Closed`, and
    /// runs with nobody holding a handle to stop it.
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
        session
            .close(CloseReason::Requested)
            .await
            .expect("expected the close to succeed");
        gate.release();

        let outcome = starting.await.expect("expected the task to finish");
        assert!(
            matches!(outcome, Err(Error::Closed { .. })),
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
                Err(Error::Cancelled {
                    reason: CancelReason::Requested
                })
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
    /// Both used to read `active` as `None` before the spawn, both to spawn their own child, and
    /// the later assignment to overwrite the first `ActiveTurn` without ending it: two children
    /// ran and only the second could ever be reached by a later `cancel`. Reserving the slot
    /// before the spawn awaits means the second call finds the first's reservation rather than an
    /// empty one, and supersedes it exactly the way a sequential second call already does.
    #[tokio::test]
    async fn a_second_start_turn_racing_the_first_spawn_supersedes_it_rather_than_losing_it() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::stalling::<[String; 0], String>([]))
                .with_turn(Run::stalling::<[String; 0], String>([])),
        );
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

        let second = tokio::spawn({
            let session = Arc::clone(&session);
            async move {
                session
                    .start_turn(TurnRequest::new("turn-2", "hello again"))
                    .await
            }
        });
        gate.wait_for_spawn().await;

        gate.release();
        gate.release();

        let first_outcome = first.await.expect("expected the first task to finish");
        let second_outcome = second.await.expect("expected the second task to finish");

        assert!(
            matches!(
                first_outcome,
                Err(Error::Cancelled {
                    reason: CancelReason::Requested
                })
            ),
            "expected the first turn to be refused as superseded, received {first_outcome:?}"
        );
        assert!(
            second_outcome.is_ok(),
            "expected the second turn to be handed a stream, received {second_outcome:?}"
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
            matches!(error, Error::HostConfiguration { .. }),
            "received {error:?}"
        );
        assert!(launcher.turn_argvs().is_empty());
    }

    #[tokio::test]
    async fn answers_no_approval_because_claude_offers_none_to_answer() {
        let launcher = Arc::new(FakeClaudeCli::new());
        let session = open(&launcher).await;
        let error = session
            .respond(PermissionResponse::from_user("req-1", "allow"))
            .await
            .expect_err("expected a refusal");
        assert!(
            matches!(error, Error::Vendor(ref vendor) if vendor.code.as_str() == "claude-approvals-unsupported"),
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
            matches!(error, Error::HostConfiguration { .. }),
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
        assert!(discovery.capabilities.mcp_passthrough);

        let silent = Arc::new(FakeClaudeCli::new());
        let discovery = ClaudeHarness::new()
            .discover(&host(silent))
            .await
            .expect("expected a discovery");
        assert!(!discovery.capabilities.mcp_passthrough);
    }
}

mod cancelling_and_closing {
    use super::*;

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
        // unnecessary rather than that it works.
        let first = tokio::time::timeout(Duration::from_secs(5), turn.recv())
            .await
            .expect("expected the stream to open");
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
        session
            .close(CloseReason::Shutdown)
            .await
            .expect("expected closing twice to be harmless");

        let error = session
            .start_turn(TurnRequest::new("turn-1", "too late"))
            .await
            .expect_err("expected a closed session to refuse a turn");
        assert!(matches!(error, Error::Closed { .. }), "received {error:?}");
    }

    #[tokio::test]
    async fn starting_a_second_turn_ends_the_first() {
        let launcher = Arc::new(
            FakeClaudeCli::new()
                .with_turn(Run::stalling(Vec::<String>::new()))
                .with_turn(Run::replaying(r#"{"type":"result","is_error":false}"#)),
        );
        let session = open(&launcher).await;
        let mut first = session
            .start_turn(TurnRequest::new("turn-1", "one"))
            .await
            .expect("expected a turn");
        let mut second = session
            .start_turn(TurnRequest::new("turn-2", "two"))
            .await
            .expect("expected a second turn");

        let first_events = drain(&mut first).await;
        assert_eq!(first_events.last(), Some(&EventKind::Completed));
        assert!(
            first_events.contains(&EventKind::Cancelled {
                reason: CancelReason::Requested
            }),
            "received {first_events:?}"
        );

        let second_events = drain(&mut second).await;
        assert_eq!(second_events.last(), Some(&EventKind::Completed));
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
}
