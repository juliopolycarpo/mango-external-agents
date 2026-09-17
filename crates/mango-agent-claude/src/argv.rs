//! The command line one turn is spawned with.
//!
//! Pure, so the exact argv a host's launcher will receive is a value a test can assert on rather
//! than something only a live run reveals. Everything that is not a flag comes from state this
//! harness owns: the prompt is never here — it travels on stdin, because argv is world-readable in
//! `ps` on every platform this runs on and a conversation is exactly the kind of thing that must
//! not appear in a process listing.
//!
//! <https://code.claude.com/docs/en/headless.md>

use mango_external_agents::{Error, Result};

use crate::models;
use crate::permissions::CliMode;

/// Everything the argv depends on, gathered so the builder decides nothing on its own.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnArgv<'a> {
    /// The program name. The host's launcher resolves it, or
    /// [`ExecutablePath`](mango_external_agents::ExecutablePath) replaces it at spawn time.
    pub program: &'a str,
    /// The mode this turn's explicit (level, routing) pair resolved to.
    ///
    /// `None` leaves Claude's own configured permission defaults untouched.
    pub mode: Option<CliMode>,
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

impl std::fmt::Debug for TurnArgv<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnArgv")
            .field("program_present", &!self.program.is_empty())
            .field("mode", &self.mode)
            .field(
                "native_session_id_present",
                &!self.native_session_id.is_empty(),
            )
            .field("established", &self.established)
            .field("model_present", &self.model.is_some())
            .field("effort_present", &self.effort.is_some())
            .field(
                "accepted_effort_count",
                &self.accepted_efforts.map_or(0, <[String]>::len),
            )
            .field(
                "declares_permission_prompts",
                &self.declares_permission_prompts,
            )
            .field("mcp_config_present", &self.mcp_config.is_some())
            .finish()
    }
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
    ///     mode: Some(CliMode::Plan),
    ///     native_session_id: "11111111-2222-3333-4444-555555555555",
    ///     established: false,
    ///     model: None,
    ///     effort: None,
    ///     accepted_efforts: None,
    ///     declares_permission_prompts: false,
    ///     mcp_config: None,
    /// }
    /// .build()
    /// .expect("expected valid argv");
    ///
    /// assert_eq!(argv[0], "claude");
    /// assert!(argv.contains(&String::from("--session-id")));
    /// assert!(!argv.contains(&String::from("--resume")));
    /// ```
    pub fn build(&self) -> Result<Vec<String>> {
        if !is_vendor_session_id(self.native_session_id) {
            return Err(Error::HostConfiguration {
                expected: "a Claude session id shaped like the UUID the CLI mints",
                received: value_summary(self.native_session_id),
            });
        }
        if let Some(model) = self.model {
            models::validate_model(model)?;
        }
        if let Some(effort) = self.effort {
            models::validate_effort(effort, self.accepted_efforts)?;
        }
        if let Some(mcp_config) = self.mcp_config
            && (!std::path::Path::new(mcp_config).is_absolute()
                || !mango_external_agents::normalize::is_argv_value_with_max(
                    mcp_config,
                    mango_external_agents::normalize::MAX_PATH_LENGTH,
                ))
        {
            return Err(Error::HostConfiguration {
                expected: "an absolute MCP configuration path that can occupy a --mcp-config value",
                received: value_summary(mcp_config),
            });
        }

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
        ]
        .map(String::from)
        .into();

        // Stated rather than left to the default when the host chose a mode. The vendor documents `none` as "nobody: anything
        // that would prompt is denied automatically; the permission mode still decides everything
        // else", and this harness genuinely is that host — it reports no answerable approval, so a
        // prompt has nowhere to go. Leaving it implicit means a build whose `host` default later
        // *waits* for an answer would park every approval-needing turn until the idle timeout, and
        // the first report would be "Claude hangs". Never `host`: that value promises an answering
        // host this harness does not have.
        if let Some(mode) = self.mode {
            argv.extend(["--permission-mode", mode.as_arg()].map(String::from));
        }
        if self.mode.is_some() && self.declares_permission_prompts {
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

        if let Some(model) = self.model {
            argv.extend([String::from("--model"), String::from(model)]);
        }
        if let Some(effort) = self.effort {
            argv.extend([String::from("--effort"), String::from(effort)]);
        }
        if let Some(mcp_config) = self.mcp_config {
            argv.extend([String::from("--mcp-config"), String::from(mcp_config)]);
        }
        Ok(argv)
    }
}

/// Summarises a rejected host value without exposing its contents through diagnostics.
pub(crate) fn value_summary(value: &str) -> String {
    format!("{} code points", value.chars().count())
}

/// Whether a vendor session handle is one this harness may put on a command line.
///
/// The same argument-position safety rule [`model_accepted`](crate::models::model_accepted) uses,
/// caller-owned value that reaches argv. Two sources feed `--session-id` and `--resume` and
/// neither is this harness's own: a host's
/// [`Resume::native_session_id`](mango_external_agents::Resume), and the id a run echoes back in
/// `system/init`, which the session follows because that is the conversation that now exists. An
/// argv array stops *shell* injection, not **argument** injection — a value beginning with `-` is
/// read by the CLI's parser as a new flag rather than as the option's value, which is how a
/// stored resume reference could put `--dangerously-skip-permissions` on the command line.
///
/// A UUID is the whole shape, rather than the looser "could not become a flag" rule
/// [`model_accepted`](crate::models::model_accepted) settles for: the vendor documents `--session-id` as
/// taking one and echoes it back verbatim, so there is a published shape to check against instead
/// of a guess to accommodate.
///
/// # Example
///
/// ```
/// use mango_agent_claude::argv::is_vendor_session_id;
///
/// assert!(is_vendor_session_id("b01414e7-4b4b-43a2-9109-a33e21664340"));
/// assert!(!is_vendor_session_id("--dangerously-skip-permissions"));
/// ```
pub fn is_vendor_session_id(id: &str) -> bool {
    const GROUPS: [usize; 5] = [8, 4, 4, 4, 12];

    let mut groups = id.split('-');
    for width in GROUPS {
        let Some(group) = groups.next() else {
            return false;
        };
        if group.len() != width || !group.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
    }
    groups.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::{TurnArgv, is_vendor_session_id};
    use crate::permissions::CliMode;

    const SESSION: &str = "11111111-2222-3333-4444-555555555555";

    fn base() -> TurnArgv<'static> {
        TurnArgv {
            program: "claude",
            mode: Some(CliMode::Manual),
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
        .build()
        .expect("expected valid argv");
        assert!(
            argv.iter().all(|argument| !argument.contains(' ')),
            "expected no prose on the command line, received {argv:?}"
        );
    }

    #[test]
    fn asks_for_the_flags_token_level_deltas_need() {
        let argv = base().build().expect("expected valid argv");
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
        let first = base().build().expect("expected valid argv");
        assert_eq!(value_after(&first, "--session-id"), Some(SESSION));
        assert!(!first.contains(&String::from("--resume")));

        let later = TurnArgv {
            established: true,
            ..base()
        }
        .build()
        .expect("expected valid argv");
        assert_eq!(value_after(&later, "--resume"), Some(SESSION));
        assert!(!later.contains(&String::from("--session-id")));
    }

    #[test]
    fn passes_manual_on_the_command_line_while_default_is_what_is_persisted() {
        let argv = base().build().expect("expected valid argv");
        assert_eq!(value_after(&argv, "--permission-mode"), Some("manual"));
        assert_eq!(CliMode::Manual.canonical(), "default");
    }

    #[test]
    fn leaves_permission_flags_out_when_the_host_omitted_both_axes() {
        let argv = TurnArgv {
            mode: None,
            declares_permission_prompts: true,
            ..base()
        }
        .build()
        .expect("expected valid argv");
        assert_eq!(value_after(&argv, "--permission-mode"), None);
        assert_eq!(value_after(&argv, "--permission-prompts"), None);
    }

    #[test]
    fn uses_plan_for_read_only_and_bypass_permissions_for_full_access() {
        for (mode, expected) in [
            (CliMode::Plan, "plan"),
            (CliMode::BypassPermissions, "bypassPermissions"),
            (CliMode::Auto, "auto"),
        ] {
            let argv = TurnArgv {
                mode: Some(mode),
                ..base()
            }
            .build()
            .expect("expected valid argv");
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
            let argv = TurnArgv {
                mode: Some(mode),
                ..base()
            }
            .build()
            .expect("expected valid argv");
            assert!(
                argv.iter()
                    .all(|argument| !argument.contains("skip-permissions")),
                "expected the interactive escape hatch never to be passed, received {argv:?}"
            );
        }
    }

    #[test]
    fn forwards_a_model_only_when_one_was_chosen() {
        assert_eq!(
            value_after(&base().build().expect("expected valid argv"), "--model"),
            None
        );
        let chosen = TurnArgv {
            model: Some("opus"),
            ..base()
        }
        .build()
        .expect("expected valid argv");
        assert_eq!(value_after(&chosen, "--model"), Some("opus"));
    }

    #[test]
    fn refuses_a_model_value_that_could_become_another_flag() {
        let error = TurnArgv {
            model: Some("--dangerously-skip-permissions"),
            ..base()
        }
        .build()
        .expect_err("expected an injected model to be refused");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "received {error:?}"
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
        .build()
        .expect("expected valid argv");
        assert_eq!(value_after(&argv, "--effort"), Some("high"));
    }

    #[test]
    fn refuses_an_effort_level_this_build_did_not_declare() {
        let levels = ["low", "high"].map(String::from);
        let error = TurnArgv {
            effort: Some("ultra"),
            accepted_efforts: Some(&levels),
            ..base()
        }
        .build()
        .expect_err("expected an undeclared effort to be refused");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "received {error:?}"
        );
    }

    #[test]
    fn refuses_an_effort_when_the_build_declared_none() {
        let error = TurnArgv {
            effort: Some("high"),
            accepted_efforts: None,
            ..base()
        }
        .build()
        .expect_err("expected an undeclared effort to be refused");
        assert!(
            matches!(
                error,
                mango_external_agents::Error::HostConfiguration { .. }
            ),
            "received {error:?}"
        );
    }

    #[test]
    fn tells_a_build_that_offers_the_flag_that_nobody_answers_prompts() {
        let argv = TurnArgv {
            declares_permission_prompts: true,
            ..base()
        }
        .build()
        .expect("expected valid argv");
        assert_eq!(value_after(&argv, "--permission-prompts"), Some("none"));
    }

    #[test]
    fn never_claims_a_host_answers_prompts_because_none_does() {
        let argv = TurnArgv {
            declares_permission_prompts: true,
            ..base()
        }
        .build()
        .expect("expected valid argv");
        assert!(
            !argv.contains(&String::from("host")),
            "expected `host` never to be passed, received {argv:?}"
        );
    }

    #[test]
    fn omits_the_flag_on_a_build_that_does_not_declare_it() {
        let argv = base().build().expect("expected valid argv");
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
            base().build().expect("expected valid argv"),
            TurnArgv {
                established: true,
                ..base()
            }
            .build()
            .expect("expected valid argv"),
            TurnArgv {
                model: Some("opus"),
                ..base()
            }
            .build()
            .expect("expected valid argv"),
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
        assert_eq!(
            value_after(
                &base().build().expect("expected valid argv"),
                "--mcp-config"
            ),
            None
        );
        let path = std::env::temp_dir().join("mea").join("servers.json");
        let path = path.to_str().expect("expected a native UTF-8 fixture path");
        let argv = TurnArgv {
            mcp_config: Some(path),
            ..base()
        }
        .build()
        .expect("expected valid argv");
        assert_eq!(value_after(&argv, "--mcp-config"), Some(path));
    }

    #[test]
    fn preserves_an_absolute_mcp_path_that_is_longer_than_an_ordinary_option_value() {
        let path = std::env::temp_dir().join("m".repeat(128));
        let path = path.to_str().expect("expected a native UTF-8 fixture path");
        assert!(path.chars().count() > 128);
        let argv = TurnArgv {
            mcp_config: Some(path),
            ..base()
        }
        .build()
        .expect("expected an absolute path under the path cap to be usable");
        assert_eq!(value_after(&argv, "--mcp-config"), Some(path));
    }

    #[test]
    fn refuses_direct_argv_values_that_could_change_the_invocation() {
        for argv in [
            TurnArgv {
                native_session_id: "--dangerously-skip-permissions",
                ..base()
            },
            TurnArgv {
                mcp_config: Some("relative-config.json"),
                ..base()
            },
            TurnArgv {
                mcp_config: Some("--dangerously-skip-permissions"),
                ..base()
            },
        ] {
            let error = argv
                .build()
                .expect_err("expected an unsafe direct argv value to be refused");
            assert!(
                matches!(
                    error,
                    mango_external_agents::Error::HostConfiguration { .. }
                ),
                "received {error:?}"
            );
        }
    }

    #[test]
    fn debug_reports_argv_shape_without_caller_owned_values() {
        let debug = format!(
            "{:?}",
            TurnArgv {
                native_session_id: "native-session-secret",
                model: Some("model-secret"),
                effort: Some("effort-secret"),
                mcp_config: Some("/tmp/mcp-secret.json"),
                ..base()
            }
        );
        for secret in [
            "native-session-secret",
            "model-secret",
            "effort-secret",
            "/tmp/mcp-secret.json",
        ] {
            assert!(
                !debug.contains(secret),
                "expected debug output to omit {secret:?}, received {debug:?}"
            );
        }
        assert!(debug.contains("model_present: true"), "received {debug:?}");
    }

    #[test]
    fn never_lets_a_session_handle_become_another_flag() {
        for injected in [
            "--dangerously-skip-permissions",
            "-p",
            "",
            " b01414e7-4b4b-43a2-9109-a33e21664340",
            "b01414e7-4b4b-43a2-9109-a33e21664340 --print",
            "b01414e7-4b4b-43a2-9109-a33e21664340\nsecond",
            "b01414e7-4b4b-43a2-9109-a33e2166434",
            "b01414e7-4b4b-43a2-9109-a33e21664340-extra",
            "b01414e7_4b4b_43a2_9109_a33e21664340",
            "zz1414e7-4b4b-43a2-9109-a33e21664340",
        ] {
            assert!(
                !is_vendor_session_id(injected),
                "expected {injected:?} to be refused rather than passed on"
            );
        }
    }

    #[test]
    fn keeps_the_handle_shape_the_vendor_actually_mints() {
        for accepted in [
            SESSION,
            "b01414e7-4b4b-43a2-9109-a33e21664340",
            "B01414E7-4B4B-43A2-9109-A33E21664340",
        ] {
            assert!(
                is_vendor_session_id(accepted),
                "expected {accepted:?} to be usable"
            );
        }
    }
}
