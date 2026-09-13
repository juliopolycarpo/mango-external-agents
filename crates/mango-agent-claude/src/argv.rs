//! The command line one turn is spawned with.
//!
//! Pure, so the exact argv a host's launcher will receive is a value a test can assert on rather
//! than something only a live run reveals. Everything that is not a flag comes from state this
//! harness owns: the prompt is never here — it travels on stdin, because argv is world-readable in
//! `ps` on every platform this runs on and a conversation is exactly the kind of thing that must
//! not appear in a process listing.
//!
//! <https://code.claude.com/docs/en/headless.md>

use crate::models;
use crate::permissions::CliMode;

/// Everything the argv depends on, gathered so the builder decides nothing on its own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnArgv<'a> {
    /// The program name. The host's launcher resolves it, or
    /// [`ExecutablePath`](mango_external_agents::ExecutablePath) replaces it at spawn time.
    pub program: &'a str,
    /// The mode this turn's (level, routing) pair resolved to.
    pub mode: CliMode,
    /// The vendor session handle this turn belongs to.
    pub native_session_id: &'a str,
    /// Whether a previous run already created that conversation on disk.
    pub established: bool,
    /// The model the host chose, if any.
    pub model: Option<&'a str>,
    /// The reasoning effort the host chose, if any.
    pub effort: Option<&'a str>,
    /// The effort levels this build declared, from the CLI surface.
    pub accepted_efforts: Option<&'a [String]>,
    /// Whether this build declares `--permission-prompts`.
    pub declares_permission_prompts: bool,
    /// A `--mcp-config` file this turn should load, when the host configured servers.
    pub mcp_config: Option<&'a str>,
}

impl TurnArgv<'_> {
    /// The turn's command line.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_claude::argv::TurnArgv;
    /// use mango_agent_claude::permissions::CliMode;
    ///
    /// let argv = TurnArgv {
    ///     program: "claude",
    ///     mode: CliMode::Plan,
    ///     native_session_id: "11111111-2222-3333-4444-555555555555",
    ///     established: false,
    ///     model: None,
    ///     effort: None,
    ///     accepted_efforts: None,
    ///     declares_permission_prompts: false,
    ///     mcp_config: None,
    /// }
    /// .build();
    ///
    /// assert_eq!(argv[0], "claude");
    /// assert!(argv.contains(&String::from("--session-id")));
    /// assert!(!argv.contains(&String::from("--resume")));
    /// ```
    pub fn build(&self) -> Vec<String> {
        let mut argv: Vec<String> = [
            self.program,
            "--print",
            // stdin, so the prompt never appears in a process listing.
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            // Both required for token-level deltas; without them the stream arrives in whole
            // messages and nothing renders until each block is complete.
            "--verbose",
            "--include-partial-messages",
            // Subagent output, tagged with the tool call that spawned it. Needs 2.1.211, which is
            // why that is the pinned minimum rather than 2.1.200.
            "--forward-subagent-text",
            "--permission-mode",
            self.mode.as_arg(),
        ]
        .map(String::from)
        .into();

        // Stated rather than left to the default. The vendor documents `none` as "nobody: anything
        // that would prompt is denied automatically; the permission mode still decides everything
        // else", and this harness genuinely is that host — it reports no answerable approval, so a
        // prompt has nowhere to go. Leaving it implicit means a build whose `host` default later
        // *waits* for an answer would park every approval-needing turn until the idle timeout, and
        // the first report would be "Claude hangs". Never `host`: that value promises an answering
        // host this harness does not have.
        if self.declares_permission_prompts {
            argv.extend(["--permission-prompts", "none"].map(String::from));
        }

        // The session id is the whole continuity mechanism: minted on the first run, resumed
        // afterwards. The same working directory is passed either way, because below 2.1.223
        // `--resume` only looks inside the directory the session was made in.
        let continuity = if self.established {
            "--resume"
        } else {
            "--session-id"
        };
        argv.extend([
            String::from(continuity),
            String::from(self.native_session_id),
        ]);

        if let Some(model) = models::safe_model(self.model) {
            argv.extend([String::from("--model"), String::from(model)]);
        }
        if let Some(effort) = self.effort
            && models::effort_accepted(Some(effort), self.accepted_efforts)
        {
            argv.extend([String::from("--effort"), String::from(effort)]);
        }
        if let Some(mcp_config) = self.mcp_config {
            argv.extend([String::from("--mcp-config"), String::from(mcp_config)]);
        }
        argv
    }
}

#[cfg(test)]
mod tests {
    use super::TurnArgv;
    use crate::permissions::CliMode;

    const SESSION: &str = "11111111-2222-3333-4444-555555555555";

    fn base() -> TurnArgv<'static> {
        TurnArgv {
            program: "claude",
            mode: CliMode::Manual,
            native_session_id: SESSION,
            established: false,
            model: None,
            effort: None,
            accepted_efforts: None,
            declares_permission_prompts: false,
            mcp_config: None,
        }
    }

    fn value_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        let at = argv.iter().position(|argument| argument == flag)?;
        argv.get(at + 1).map(String::as_str)
    }

    #[test]
    fn never_puts_the_prompt_in_argv() {
        let argv = TurnArgv {
            model: Some("opus"),
            ..base()
        }
        .build();
        assert!(
            argv.iter().all(|argument| !argument.contains(' ')),
            "expected no prose on the command line, received {argv:?}"
        );
    }

    #[test]
    fn asks_for_the_flags_token_level_deltas_need() {
        let argv = base().build();
        for required in [
            "--print",
            "--verbose",
            "--include-partial-messages",
            "--forward-subagent-text",
        ] {
            assert!(
                argv.contains(&String::from(required)),
                "expected {required}"
            );
        }
        assert_eq!(value_after(&argv, "--input-format"), Some("stream-json"));
        assert_eq!(value_after(&argv, "--output-format"), Some("stream-json"));
    }

    #[test]
    fn mints_the_session_on_the_first_turn_and_resumes_it_afterwards() {
        let first = base().build();
        assert_eq!(value_after(&first, "--session-id"), Some(SESSION));
        assert!(!first.contains(&String::from("--resume")));

        let later = TurnArgv {
            established: true,
            ..base()
        }
        .build();
        assert_eq!(value_after(&later, "--resume"), Some(SESSION));
        assert!(!later.contains(&String::from("--session-id")));
    }

    #[test]
    fn passes_manual_on_the_command_line_while_default_is_what_is_persisted() {
        let argv = base().build();
        assert_eq!(value_after(&argv, "--permission-mode"), Some("manual"));
        assert_eq!(CliMode::Manual.canonical(), "default");
    }

    #[test]
    fn uses_plan_for_read_only_and_bypass_permissions_for_full_access() {
        for (mode, expected) in [
            (CliMode::Plan, "plan"),
            (CliMode::BypassPermissions, "bypassPermissions"),
            (CliMode::Auto, "auto"),
        ] {
            let argv = TurnArgv { mode, ..base() }.build();
            assert_eq!(value_after(&argv, "--permission-mode"), Some(expected));
        }
    }

    #[test]
    fn never_passes_a_dangerously_skip_permissions_flag() {
        for mode in [
            CliMode::Manual,
            CliMode::AcceptEdits,
            CliMode::Plan,
            CliMode::Auto,
            CliMode::DontAsk,
            CliMode::BypassPermissions,
        ] {
            let argv = TurnArgv { mode, ..base() }.build();
            assert!(
                argv.iter()
                    .all(|argument| !argument.contains("skip-permissions")),
                "expected the interactive escape hatch never to be passed, received {argv:?}"
            );
        }
    }

    #[test]
    fn forwards_a_model_only_when_one_was_chosen() {
        assert_eq!(value_after(&base().build(), "--model"), None);
        let chosen = TurnArgv {
            model: Some("opus"),
            ..base()
        }
        .build();
        assert_eq!(value_after(&chosen, "--model"), Some("opus"));
    }

    #[test]
    fn never_lets_a_model_value_become_another_flag() {
        let argv = TurnArgv {
            model: Some("--dangerously-skip-permissions"),
            ..base()
        }
        .build();
        assert!(
            !argv.contains(&String::from("--model")),
            "expected an unusable model to be dropped with its flag, received {argv:?}"
        );
        assert!(
            argv.iter()
                .all(|argument| !argument.contains("skip-permissions"))
        );
    }

    #[test]
    fn passes_an_effort_level_this_build_declared() {
        let levels = ["low", "high"].map(String::from);
        let argv = TurnArgv {
            effort: Some("high"),
            accepted_efforts: Some(&levels),
            ..base()
        }
        .build();
        assert_eq!(value_after(&argv, "--effort"), Some("high"));
    }

    #[test]
    fn drops_an_effort_level_this_build_did_not_declare() {
        let levels = ["low", "high"].map(String::from);
        let argv = TurnArgv {
            effort: Some("ultra"),
            accepted_efforts: Some(&levels),
            ..base()
        }
        .build();
        assert!(
            !argv.contains(&String::from("--effort")),
            "received {argv:?}"
        );
    }

    #[test]
    fn passes_no_effort_at_all_to_a_build_that_declared_none() {
        let argv = TurnArgv {
            effort: Some("high"),
            accepted_efforts: None,
            ..base()
        }
        .build();
        assert!(
            !argv.contains(&String::from("--effort")),
            "received {argv:?}"
        );
    }

    #[test]
    fn tells_a_build_that_offers_the_flag_that_nobody_answers_prompts() {
        let argv = TurnArgv {
            declares_permission_prompts: true,
            ..base()
        }
        .build();
        assert_eq!(value_after(&argv, "--permission-prompts"), Some("none"));
    }

    #[test]
    fn never_claims_a_host_answers_prompts_because_none_does() {
        let argv = TurnArgv {
            declares_permission_prompts: true,
            ..base()
        }
        .build();
        assert!(
            !argv.contains(&String::from("host")),
            "expected `host` never to be passed, received {argv:?}"
        );
    }

    #[test]
    fn omits_the_flag_on_a_build_that_does_not_declare_it() {
        let argv = base().build();
        assert!(
            !argv.contains(&String::from("--permission-prompts")),
            "received {argv:?}"
        );
    }

    #[test]
    fn every_flag_the_cli_surface_calls_required_actually_appears_on_some_turn() {
        // `--resume` and `--session-id` are mutually exclusive, and `--model` is passed only when
        // a model was chosen (REQUIRED_FLAGS's own documented exception) — so no single build()
        // carries every required flag; a flag counts if any of these plausible turns passes it.
        let turns = [
            base().build(),
            TurnArgv {
                established: true,
                ..base()
            }
            .build(),
            TurnArgv {
                model: Some("opus"),
                ..base()
            }
            .build(),
        ];
        for &flag in crate::cli_surface::REQUIRED_FLAGS {
            assert!(
                turns.iter().any(|argv| argv.contains(&String::from(flag))),
                "expected {flag:?}, declared required by cli_surface::REQUIRED_FLAGS, to appear \
                 on some turn"
            );
        }
    }

    #[test]
    fn loads_the_hosts_mcp_servers_only_when_there_are_some() {
        assert_eq!(value_after(&base().build(), "--mcp-config"), None);
        let argv = TurnArgv {
            mcp_config: Some("/tmp/mea/servers.json"),
            ..base()
        }
        .build();
        assert_eq!(
            value_after(&argv, "--mcp-config"),
            Some("/tmp/mea/servers.json")
        );
    }
}
