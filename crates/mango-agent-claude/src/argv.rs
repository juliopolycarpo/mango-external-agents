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

/// The flags every turn passes right after the program name.
///
/// A constant, not a literal inside [`TurnArgv::build`], because
/// [`MAX_JSON_SCHEMA_ARGUMENT_UNITS`](crate::json_schema::MAX_JSON_SCHEMA_ARGUMENT_UNITS) is
/// derived from their length.
pub(crate) const FIXED_FLAGS: [&str; 8] = [
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
];

/// The longest the flags that depend on the turn can be when a schema is passed, besides the
/// model, effort and MCP path, which a host chooses and the whole-command-line check counts as
/// given.
///
/// The longest permission mode and `--permission-prompts none`, a resumed session's UUID, and the
/// schema's own flag name. Pinned against [`CliMode`] and the UUID shape by tests.
pub(crate) const WORST_CASE_CONDITIONAL_FLAGS: [&str; 7] = [
    "--permission-mode",
    "bypassPermissions",
    "--permission-prompts",
    "none",
    "--resume",
    "00000000-0000-0000-0000-000000000000",
    "--json-schema",
];

/// Every flag name [`TurnArgv::build`] can put on a turn's command line, with no value.
///
/// The fake CLI names a refused launch's flags from this list and counts every other word, so a
/// flag-shaped value such as `-sk-ant-abc` is never printed. Tests pin the list against the argv
/// the builder produces: a flag added to `build` without an entry here fails them.
#[cfg(any(test, feature = "testing"))]
pub(crate) const TURN_FLAG_NAMES: [&str; 14] = [
    "--print",
    "--input-format",
    "--output-format",
    "--verbose",
    "--include-partial-messages",
    "--forward-subagent-text",
    "--permission-mode",
    "--permission-prompts",
    "--resume",
    "--session-id",
    "--model",
    "--effort",
    "--mcp-config",
    "--json-schema",
];

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
    /// The JSON Schema the host asked this turn's answer to be validated against.
    ///
    /// Raw host text: [`build`](TurnArgv::build) refuses what [`crate::json_schema::compact`]
    /// refuses and passes the compact form. The caller has already checked that this build
    /// declares `--json-schema`.
    pub json_schema: Option<&'a str>,
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
            .field("json_schema_present", &self.json_schema.is_some())
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
    ///     json_schema: None,
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

        // Validated here as well as at open: the fields are public, so a caller can build a
        // `TurnArgv` that never went through the harness.
        let json_schema = self
            .json_schema
            .map(crate::json_schema::compact)
            .transpose()?;

        let mut argv: Vec<String> = std::iter::once(self.program)
            .chain(FIXED_FLAGS)
            .map(String::from)
            .collect();

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
        if let Some(json_schema) = json_schema {
            // Inline, because the flag takes the schema itself rather than a path, so it is
            // visible in a process listing. The prompt never is.
            argv.extend([String::from("--json-schema"), json_schema]);
            // The schema is the one argument a host can make large, so the whole line is checked
            // against the tightest Windows limit only when one is present. The default argv is
            // never refused on length.
            crate::json_schema::check_turn_command_line(&argv)?;
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

/// Mints the UUID a new session proposes with `--session-id`, and names its scratch directory.
///
/// A version 4 UUID: 122 bits from the operating system's random source, through the `ring` the
/// core already links for its own digest. The shape is the one [`is_vendor_session_id`] accepts:
/// the CLI documents `--session-id` as taking a valid UUID and refuses anything else.
///
/// Entropy failure returns a typed refusal without a weaker id. Artifact naming calls this before
/// directory creation and discovery probes; new-session id generation follows discovery checks
/// and uses the existing open-failure artifact cleanup. A strict resume reuses its native id.
pub(crate) fn mint_uuid() -> Result<String> {
    let random = ring::rand::SystemRandom::new();
    mint_uuid_with_entropy(|bytes| ring::rand::SecureRandom::fill(&random, bytes))
}

/// Keeps the random-source failure path deterministic in tests.
fn mint_uuid_with_entropy(
    fill: impl FnOnce(&mut [u8]) -> std::result::Result<(), ring::error::Unspecified>,
) -> Result<String> {
    let mut bytes = [0_u8; 16];
    fill(&mut bytes).map_err(|_| Error::HostConfiguration {
        expected: "16 bytes from the operating system's random source to generate a UUID",
        received: String::from("a random source that failed"),
    })?;
    Ok(format_uuid_v4(bytes))
}

/// Renders sixteen bytes as a version 4, RFC 4122 variant UUID in the 8-4-4-4-12 text form.
///
/// The version nibble and the two variant bits are overwritten, which is what makes any sixteen
/// bytes a valid version 4 UUID; the other 122 bits are the caller's.
fn format_uuid_v4(mut bytes: [u8; 16]) -> String {
    use std::fmt::Write as _;

    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        // Writing to a `String` cannot fail.
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{
        TurnArgv, format_uuid_v4, is_vendor_session_id, mint_uuid, mint_uuid_with_entropy,
    };
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
            json_schema: None,
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
    fn passes_a_json_schema_only_when_the_host_supplied_one() {
        assert_eq!(
            value_after(
                &base().build().expect("expected valid argv"),
                "--json-schema"
            ),
            None,
            "expected the default argv to carry no --json-schema"
        );
        let argv = TurnArgv {
            json_schema: Some("{\n \"type\": \"object\"\n}"),
            ..base()
        }
        .build()
        .expect("expected valid argv");
        assert_eq!(
            value_after(&argv, "--json-schema"),
            Some(r#"{"type":"object"}"#),
            "expected the compact schema after --json-schema"
        );
    }

    fn x_schema(units: usize) -> String {
        // `{"d":""}` takes 14 units quoted, and each `x` one more.
        format!("{{\"d\":\"{}\"}}", "x".repeat(units - 14))
    }

    /// The worst-case conditional flags are the longest a turn can carry, so a schema at the cap
    /// fills the batch line exactly and nothing a host can leave out makes it longer.
    #[test]
    fn a_schema_at_the_cap_fills_the_batch_line_exactly_on_the_longest_turn() {
        use crate::json_schema::{
            BATCH_COMMAND_LINE_LIMIT_UNITS, MAX_JSON_SCHEMA_ARGUMENT_UNITS, turn_command_line_units,
        };

        let schema = x_schema(MAX_JSON_SCHEMA_ARGUMENT_UNITS);
        let argv = TurnArgv {
            mode: Some(CliMode::BypassPermissions),
            established: true,
            declares_permission_prompts: true,
            json_schema: Some(&schema),
            ..base()
        }
        .build()
        .expect("expected the longest schema turn to fit the batch line");
        assert_eq!(
            turn_command_line_units(&argv),
            BATCH_COMMAND_LINE_LIMIT_UNITS,
            "expected the capped schema to fill the line | received {} flags",
            argv.len()
        );
    }

    /// Every combination of the optional flags: the names `build` emits and `TURN_FLAG_NAMES`
    /// lists must be the same set, so the fake CLI neither prints a value nor hides a real flag.
    #[test]
    fn the_turn_flag_names_are_exactly_the_flags_build_can_emit() {
        let schema = r#"{"type":"object"}"#;
        let efforts = [String::from("high")];
        let mut emitted = std::collections::BTreeSet::new();
        for established in [false, true] {
            for mode in [None, Some(CliMode::Manual)] {
                for declares_permission_prompts in [false, true] {
                    for with_options in [false, true] {
                        let turn = TurnArgv {
                            established,
                            mode,
                            declares_permission_prompts,
                            model: with_options.then_some("opus"),
                            effort: with_options.then_some("high"),
                            accepted_efforts: Some(&efforts),
                            mcp_config: with_options.then_some("/tmp/mcp.json"),
                            json_schema: with_options.then_some(schema),
                            ..base()
                        };
                        let argv = turn.build().expect("expected valid argv");
                        emitted.extend(
                            argv.into_iter()
                                .skip(1)
                                .filter(|word| word.starts_with("--")),
                        );
                    }
                }
            }
        }
        let listed: std::collections::BTreeSet<String> = super::TURN_FLAG_NAMES
            .iter()
            .map(|flag| (*flag).to_owned())
            .collect();
        assert_eq!(
            listed, emitted,
            "expected TURN_FLAG_NAMES to equal the flags TurnArgv::build emits | received left: listed, right: emitted"
        );
    }

    #[test]
    fn the_worst_case_flags_are_no_shorter_than_any_flag_a_turn_can_pass() {
        for mode in [
            CliMode::Manual,
            CliMode::AcceptEdits,
            CliMode::Plan,
            CliMode::Auto,
            CliMode::DontAsk,
            CliMode::BypassPermissions,
        ] {
            assert!(
                mode.as_arg().len() <= super::WORST_CASE_CONDITIONAL_FLAGS[1].len(),
                "expected the worst-case permission mode to be the longest | received {:?}",
                mode.as_arg()
            );
        }
        assert!(
            is_vendor_session_id(super::WORST_CASE_CONDITIONAL_FLAGS[5]),
            "expected the worst-case session placeholder to have the UUID shape"
        );
        assert_eq!(super::WORST_CASE_CONDITIONAL_FLAGS[6], "--json-schema");
    }

    /// The regression for an 8,000-character schema beside the real fixed flags: it was accepted
    /// under the earlier 16 KiB cap, and the `claude.cmd` line then passed cmd.exe's 8,191.
    #[test]
    fn refuses_an_eight_thousand_character_schema_beside_the_real_fixed_flags() {
        let schema = format!("{{\"d\":\"{}\"}}", "x".repeat(8_000));
        let error = TurnArgv {
            json_schema: Some(&schema),
            ..base()
        }
        .build()
        .expect_err("expected a schema the fixed flags leave no room for to be refused");
        let mango_external_agents::Error::HostConfiguration { received, .. } = error else {
            panic!("expected a host-configuration refusal | received {error:?}");
        };
        assert!(
            received.contains("8014 UTF-16 code units"),
            "expected the schema's escaped length | received {received:?}"
        );
        assert!(!received.contains("xxxx"), "received {received:?}");
    }

    /// A schema inside its own cap still has to share the line with the MCP path, which a host
    /// sets per session and may be up to 4,096 characters.
    #[test]
    fn refuses_a_schema_that_fits_alone_but_not_beside_a_long_mcp_config_path() {
        let schema = x_schema(6_014);
        let long = std::env::temp_dir().join("m".repeat(2_000));
        let long = long.to_str().expect("expected a native UTF-8 fixture path");
        let error = TurnArgv {
            mcp_config: Some(long),
            json_schema: Some(&schema),
            ..base()
        }
        .build()
        .expect_err("expected the whole line to be refused");
        let mango_external_agents::Error::HostConfiguration { expected, received } = error else {
            panic!("expected a host-configuration refusal | received {error:?}");
        };
        assert!(expected.contains("8191"), "received {expected:?}");
        assert!(
            received.starts_with("a command line of ") && received.ends_with(" UTF-16 code units"),
            "expected the line's units named | received {received:?}"
        );
        assert!(
            !received.contains("mmmm") && !received.contains("xxxx"),
            "expected neither the path nor the schema in the message | received {received:?}"
        );

        // The same schema beside a short path fits, and the long path alone is not a length
        // problem: no schema, no line check.
        let short = std::env::temp_dir().join("servers.json");
        let short = short
            .to_str()
            .expect("expected a native UTF-8 fixture path");
        TurnArgv {
            mcp_config: Some(short),
            json_schema: Some(&schema),
            ..base()
        }
        .build()
        .expect("expected the schema beside a short MCP path to fit");
        TurnArgv {
            mcp_config: Some(long),
            ..base()
        }
        .build()
        .expect("expected the default argv never to be refused on length");
    }

    #[test]
    fn refuses_a_json_schema_that_could_become_another_flag_or_is_not_json() {
        for schema in ["-1", "--dangerously-skip-permissions", "{broken", "[]", ""] {
            let error = TurnArgv {
                json_schema: Some(schema),
                ..base()
            }
            .build()
            .expect_err("expected an unusable schema to be refused");
            assert!(
                matches!(
                    error,
                    mango_external_agents::Error::HostConfiguration { .. }
                ),
                "expected a host-configuration refusal for {schema:?} | received {error:?}"
            );
        }
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
                json_schema: Some("{\"title\":\"schema-secret\"}"),
                ..base()
            }
        );
        for secret in [
            "native-session-secret",
            "model-secret",
            "effort-secret",
            "/tmp/mcp-secret.json",
            "schema-secret",
        ] {
            assert!(
                !debug.contains(secret),
                "expected debug output to omit {secret:?}, received {debug:?}"
            );
        }
        assert!(debug.contains("model_present: true"), "received {debug:?}");
        assert!(
            debug.contains("json_schema_present: true"),
            "received {debug:?}"
        );
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

    #[test]
    fn any_sixteen_bytes_format_as_a_version_4_uuid_in_the_8_4_4_4_12_form() {
        assert_eq!(
            format_uuid_v4([0x00; 16]),
            "00000000-0000-4000-8000-000000000000"
        );
        assert_eq!(
            format_uuid_v4([0xff; 16]),
            "ffffffff-ffff-4fff-bfff-ffffffffffff"
        );
        assert_eq!(
            format_uuid_v4([
                0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x10, 0x32, 0x54, 0x76, 0x98, 0xba,
                0xdc, 0xfe
            ]),
            "01234567-89ab-4def-9032-547698badcfe"
        );
    }

    #[test]
    fn a_minted_uuid_is_one_the_cli_accepts_and_is_not_minted_twice() {
        let first = mint_uuid().expect("expected the operating system's random source to answer");
        let second = mint_uuid().expect("expected the operating system's random source to answer");
        for minted in [&first, &second] {
            assert!(
                is_vendor_session_id(minted),
                "expected a UUID in the 8-4-4-4-12 form | received {minted:?}"
            );
            assert_eq!(
                minted.as_bytes()[14],
                b'4',
                "expected the version nibble 4 | received {minted:?}"
            );
            assert!(
                matches!(minted.as_bytes()[19], b'8' | b'9' | b'a' | b'b'),
                "expected the RFC 4122 variant (8, 9, a or b) | received {minted:?}"
            );
        }
        assert_ne!(
            first, second,
            "expected two mints to differ | received the same UUID twice"
        );
    }

    struct FailingUuidEntropy;

    impl FailingUuidEntropy {
        fn fill(bytes: &mut [u8]) -> std::result::Result<(), ring::error::Unspecified> {
            // Even a partial write followed by failure must refuse UUID generation.
            bytes.fill(0xab);
            Err(ring::error::Unspecified)
        }
    }

    #[test]
    fn uuid_entropy_failure_describes_generation_for_both_callers() {
        let error = mint_uuid_with_entropy(FailingUuidEntropy::fill)
            .expect_err("expected failed entropy to refuse UUID generation");
        let mango_external_agents::Error::HostConfiguration { expected, received } = error else {
            panic!("expected a typed UUID entropy refusal | received {error:?}");
        };
        assert_eq!(
            (expected, received.as_str()),
            (
                "16 bytes from the operating system's random source to generate a UUID",
                "a random source that failed",
            ),
            "expected a UUID generation refusal for session IDs and MCP artifact names | received expected={expected:?}, received={received:?}"
        );
    }
}
