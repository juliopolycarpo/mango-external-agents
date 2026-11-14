//! Shared discovery and turn arguments, checked before launching a vendor.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use mango_external_agents::{HarnessId, PermissionLevel, ProfileId, TransportKind};

#[derive(Parser)]
#[command(name = "mea", disable_help_flag = true)]
struct Arguments {
    #[arg(long)]
    harness: Option<String>,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long)]
    level: Option<String>,
    #[arg(long)]
    transport: Option<String>,
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    // Hyphen values reach `cancel_after`, so `--cancel-after -1` is refused in this file's own
    // words, with the value it received, rather than as a flag clap does not know.
    #[arg(long, allow_hyphen_values = true)]
    cancel_after: Option<String>,
    prompt: Vec<String>,
}

/// What this CLI's own `--harness` flag selected.
///
/// The library registry uses [`HarnessId`], a validated string. This enum makes `mea`'s three
/// supported selections exhaustive: `claude`, `codex` and `acp:<profile>`.
/// [`HarnessChoice::id`] translates the CLI selection into the registry key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HarnessChoice {
    Claude,
    Codex,
    Acp(ProfileId),
}

impl HarnessChoice {
    /// The registry key this selection dispatches through. Example: `HarnessChoice::Codex.id()`
    /// is `HarnessId::codex()`; `a_harness_choice_maps_onto_the_registrys_own_id` below covers it.
    pub(crate) fn id(&self) -> HarnessId {
        match self {
            Self::Claude => HarnessId::claude(),
            Self::Codex => HarnessId::codex(),
            Self::Acp(profile) => HarnessId::acp(profile),
        }
    }
}

impl fmt::Display for HarnessChoice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.id(), formatter)
    }
}

pub(crate) struct Options {
    pub kind: Option<HarnessChoice>,
    pub level: Option<PermissionLevel>,
    pub transport: Option<TransportKind>,
    /// The Unix socket of an app-server the host already runs, for `--transport unix-socket`.
    pub socket: Option<PathBuf>,
    /// The Codex model id `mea turn` opens its thread with, instead of the user's configured one.
    pub model: Option<String>,
    pub cwd: Option<PathBuf>,
    pub json: bool,
    /// How long after a turn starts `mea turn` asks its session to cancel it, when asked to.
    pub cancel_after: Option<Duration>,
    pub prompt: String,
}

impl Options {
    /// Validates the CLI request before discovery. Example: `--harness acp --profile cursor`.
    pub fn parse(arguments: &[String]) -> Result<Self, String> {
        let args = Arguments::try_parse_from(
            std::iter::once("mea").chain(arguments.iter().map(String::as_str)),
        )
        .map_err(|error| error.to_string())?;
        Ok(Self {
            kind: kind(args.harness.as_deref(), args.profile.as_deref())?,
            level: args.level.as_deref().map(level).transpose()?,
            transport: args.transport.as_deref().map(transport).transpose()?,
            socket: args.socket,
            model: args.model,
            cwd: args.cwd,
            json: args.json,
            cancel_after: args.cancel_after.as_deref().map(cancel_after).transpose()?,
            prompt: args.prompt.join(" "),
        })
    }

    /// Parses the arguments of one command and checks what only that command can judge.
    ///
    /// Example: `Options::parse_for("turn", &["--transport", "unix-socket", "--socket", "/run/codex.sock"])`.
    /// The refusals name the command, `(mea turn)`, because this parser is shared by `discover`,
    /// `doctor` and `turn` and an operator reads one of them.
    pub fn parse_for(command: &str, arguments: &[String]) -> Result<Self, String> {
        let options = Self::parse(arguments)?;
        options.check_socket(command)?;
        options.check_model(command)?;
        Ok(options)
    }

    /// The `--model` pairing: only `turn` opens a session, and only the Codex harness takes it.
    ///
    /// The id is the host's, sent as `thread/start`'s `model` member and never written to the
    /// vendor's own configuration. Other harnesses are refused here rather than quietly run on
    /// their configured model, which is the run the flag exists to avoid.
    fn check_model(&self, command: &str) -> Result<(), String> {
        let Some(model) = &self.model else {
            return Ok(());
        };
        if command != "turn" {
            return Err(format!(
                "(mea {command}): expected no --model; {command} probes the local CLI and never opens a session | received --model {model:?}"
            ));
        }
        model_id(command, model)?;
        let kind = self.kind.clone().unwrap_or(HarnessChoice::Claude);
        if kind == HarnessChoice::Codex {
            Ok(())
        } else {
            Err(format!(
                "(mea {command}): expected --model with --harness codex | received --model {model:?} with --harness {kind}"
            ))
        }
    }

    /// The `--socket` and `--transport` pairing: the path goes with `unix-socket` and with nothing
    /// else, and it is the host's, so it is passed on as typed.
    ///
    /// Only the shape is judged here (present, absolute). The harness still refuses a path that is
    /// too long for a socket address or a server owned by another user, and `mea` prints its words.
    /// Only `turn` attaches: `discover` and `doctor` probe the local CLI, never open a session, and
    /// so would leave a thread on the operator's server, so they refuse both flags.
    fn check_socket(&self, command: &str) -> Result<(), String> {
        let attaches = self.transport == Some(TransportKind::UnixSocket);
        if command != "turn" {
            let mut received = Vec::new();
            if attaches {
                received.push(String::from("--transport unix-socket"));
            }
            if let Some(path) = &self.socket {
                received.push(format!("--socket {path:?}"));
            }
            return if received.is_empty() {
                Ok(())
            } else {
                Err(format!(
                    "(mea {command}): expected no --socket and no unix-socket transport; {command} probes the local CLI and never opens a session | received {}",
                    received.join(" ")
                ))
            };
        }
        match (&self.socket, attaches) {
            (None, false) => Ok(()),
            (None, true) => Err(format!(
                "(mea {command}): expected --socket PATH with --transport unix-socket | received --transport unix-socket without --socket"
            )),
            (Some(_), false) => Err(format!(
                "(mea {command}): expected --socket with --transport unix-socket | received --socket {}",
                self.transport.map_or_else(
                    || String::from("without it"),
                    |other| format!("with --transport {other}")
                )
            )),
            (Some(path), true) if !path.is_absolute() => Err(format!(
                "(mea {command}): expected --socket as an absolute path | received {path:?}"
            )),
            (Some(_), true) => Ok(()),
        }
    }
}

/// The model id `--model` carries, or the refusal that names it.
///
/// The same shape the Codex harness accepts for a model id: nonempty, bounded, no surrounding
/// spaces and no control characters. `mea capture` sends the id to the app-server itself, so the
/// check cannot be left to the harness. Example: `--model gpt-5.2-codex`.
pub(crate) fn model_id<'a>(command: &str, received: &'a str) -> Result<&'a str, String> {
    if received.trim() != received
        || received.chars().any(char::is_control)
        || mango_external_agents::normalize::opaque_id(received, "model id").is_err()
    {
        return Err(format!(
            "(mea {command}): expected --model as a nonempty, bounded model id without surrounding spaces or control characters | received {received:?}"
        ));
    }
    Ok(received)
}

/// The permission level this name asks for. Example: `--level read-only`.
///
/// Named here rather than through clap's `value_parser` so the accepted set is written once: a
/// list on the field and a `_` arm here would let a new value parse into the wrong level.
fn level(named: &str) -> Result<PermissionLevel, String> {
    match named {
        "read-only" => Ok(PermissionLevel::ReadOnly),
        "default" => Ok(PermissionLevel::Default),
        "full-access" => Ok(PermissionLevel::FullAccess),
        other => Err(format!(
            "expected `read-only`, `default` or `full-access`, received {other:?}"
        )),
    }
}

/// The transport this name asks for. Example: `--transport stdio`.
fn transport(named: &str) -> Result<TransportKind, String> {
    match named {
        "stdio" => Ok(TransportKind::Stdio),
        "websocket" => Ok(TransportKind::WebSocket),
        "unix-socket" => Ok(TransportKind::UnixSocket),
        "acp" => Ok(TransportKind::Acp),
        other => Err(format!(
            "expected `stdio`, `websocket`, `unix-socket` or `acp`, received {other:?}"
        )),
    }
}

/// The delay this number of seconds asks for. Example: `--cancel-after 1.5`.
///
/// Bounded by the turn deadline: a cancel scheduled at or past it can never be sent, because the
/// deadline ends the turn first, so a larger value is a mistake rather than a long wait.
fn cancel_after(seconds: &str) -> Result<Duration, String> {
    let limit = crate::turn::TURN_DEADLINE;
    seconds
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|value| Duration::try_from_secs_f64(value).ok())
        .filter(|delay| *delay < limit)
        .ok_or_else(|| {
            format!(
                "expected --cancel-after as seconds from 0 to below {}, such as 1.5 | received {seconds:?}",
                limit.as_secs()
            )
        })
}

fn kind(harness: Option<&str>, profile: Option<&str>) -> Result<Option<HarnessChoice>, String> {
    match (harness, profile) {
        (None, None) => Ok(None),
        (Some("claude"), None) => Ok(Some(HarnessChoice::Claude)),
        (Some("codex"), None) => Ok(Some(HarnessChoice::Codex)),
        (Some("acp"), Some(profile)) => acp(profile),
        (Some(name), None) if let Some(profile) = name.strip_prefix("acp:") => acp(profile),
        _ => Err(format!(
            "expected claude, codex, acp:<profile> or acp --profile <id>; received harness {harness:?}, profile {profile:?}"
        )),
    }
}

fn acp(profile: &str) -> Result<Option<HarnessChoice>, String> {
    if mango_agent_acp::builtin_profile(profile).is_none() {
        return Err(format!(
            "expected a built-in ACP profile, received {profile:?}"
        ));
    }
    let profile = ProfileId::new(profile).map_err(|error| error.to_string())?;
    Ok(Some(HarnessChoice::Acp(profile)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_profile_directory_transport_and_json() {
        let args = [
            "--harness",
            "acp",
            "--profile",
            "cursor",
            "--cwd",
            ".",
            "--transport",
            "acp",
            "--json",
            "hello",
        ]
        .map(String::from);
        let options = Options::parse(&args).expect("valid turn arguments");
        assert_eq!(
            options.kind,
            Some(HarnessChoice::Acp(
                ProfileId::new("cursor").expect("expected a valid profile")
            ))
        );
        assert_eq!(options.cwd, Some(PathBuf::from(".")));
        assert_eq!(options.transport, Some(TransportKind::Acp));
        assert!(options.json);
        assert_eq!(options.cancel_after, None);
        assert_eq!(options.prompt, "hello");
    }

    #[test]
    fn reads_the_cancel_delay_as_decimal_seconds() {
        let args = ["--cancel-after", "1.5", "count", "slowly"].map(String::from);
        let options = Options::parse(&args).expect("valid turn arguments");
        assert_eq!(options.cancel_after, Some(Duration::from_millis(1500)));
        assert_eq!(options.prompt, "count slowly");

        assert_eq!(cancel_after("0").expect("zero"), Duration::ZERO);
        assert_eq!(cancel_after("2").expect("two"), Duration::from_secs(2));
        assert_eq!(
            cancel_after("299.5").expect("just below the deadline"),
            Duration::from_millis(299_500)
        );
    }

    #[test]
    fn refuses_a_cancel_delay_that_is_not_a_reachable_number_of_seconds() {
        for received in [
            "-1", "-0.5", "NaN", "inf", "soon", "", "1s", "1e400", "300", "86400",
        ] {
            let error = cancel_after(received).expect_err("expected the delay to be refused");
            assert_eq!(
                error,
                format!(
                    "expected --cancel-after as seconds from 0 to below 300, such as 1.5 | received {received:?}"
                ),
                "expected {received:?} to be refused with the value and the accepted shape"
            );
        }
    }

    #[test]
    fn a_refused_cancel_delay_stops_the_parse_with_its_own_message() {
        for received in ["-1", "soon", "--json"] {
            let args = ["--cancel-after", received, "hello"].map(String::from);
            let error = Options::parse(&args)
                .err()
                .expect("expected the arguments to be refused");
            assert!(
                error.ends_with(&format!("| received {received:?}")),
                "expected the refusal to name {received:?}, received {error:?}"
            );
        }
        let missing = Options::parse(&[String::from("--cancel-after")])
            .err()
            .expect("expected a flag without a value to be refused");
        assert!(
            missing.contains("--cancel-after"),
            "expected the refusal to name the flag, received {missing:?}"
        );
    }

    #[test]
    fn rejects_misspelled_flags_and_mismatched_profiles() {
        for args in [
            vec!["--jsno"],
            vec!["--cwd"],
            vec!["--harness", "codex", "--profile", "cursor"],
            vec!["--harness", "acp:no-such-profile"],
            // A value outside the accepted set must be refused, never quietly read as the
            // permissive end of the axis.
            vec!["--level", "plan"],
            vec!["--transport", "unix"],
        ] {
            assert!(
                Options::parse(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err()
            );
        }
        assert!(kind(Some("acp"), None).is_err());
        assert!(acp("cursor").is_ok());
    }

    fn arguments(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn a_socket_path_goes_with_the_unix_socket_transport_as_typed() {
        // Built from the temporary directory so the path is absolute on every platform the parser
        // tests run on; a Unix-style literal has no drive letter on Windows.
        let socket = std::env::temp_dir().join("codex").join("app-server.sock");
        let socket_argument = socket.display().to_string();
        let options = Options::parse_for(
            "turn",
            &arguments(&[
                "--harness",
                "codex",
                "--transport",
                "unix-socket",
                "--socket",
                &socket_argument,
                "hello",
            ]),
        )
        .unwrap_or_else(|error| panic!("expected (mea turn) to accept it, received {error}"));
        assert_eq!(options.transport, Some(TransportKind::UnixSocket));
        assert_eq!(options.socket, Some(socket));
    }

    #[test]
    fn a_socket_path_without_the_unix_socket_transport_is_refused() {
        let without =
            Options::parse_for("turn", &arguments(&["--socket", "/run/codex.sock", "hi"]));
        assert_eq!(
            without.err().as_deref(),
            Some(
                "(mea turn): expected --socket with --transport unix-socket | received --socket without it"
            )
        );
        for other in ["stdio", "websocket", "acp"] {
            let refused = Options::parse_for(
                "turn",
                &arguments(&["--transport", other, "--socket", "/run/codex.sock"]),
            );
            assert_eq!(
                refused.err(),
                Some(format!(
                    "(mea turn): expected --socket with --transport unix-socket | received --socket with --transport {other}"
                ))
            );
        }
    }

    #[test]
    fn the_unix_socket_transport_without_a_socket_path_is_refused() {
        let refused = Options::parse_for(
            "turn",
            &arguments(&["--harness", "codex", "--transport", "unix-socket"]),
        );
        assert_eq!(
            refused.err(),
            Some(String::from(
                "(mea turn): expected --socket PATH with --transport unix-socket | received --transport unix-socket without --socket"
            ))
        );
    }

    #[test]
    fn a_relative_socket_path_is_refused_with_the_path_it_received() {
        let refused = Options::parse_for(
            "turn",
            &arguments(&[
                "--transport",
                "unix-socket",
                "--socket",
                "run/codex.sock",
                "hi",
            ]),
        );
        assert_eq!(
            refused.err(),
            Some(String::from(
                "(mea turn): expected --socket as an absolute path | received \"run/codex.sock\""
            ))
        );
    }

    /// `discover` and `doctor` only probe the local CLI. A socket would mean opening a session, which
    /// leaves a thread on the operator's server, so both refuse the flag and the transport.
    #[test]
    fn discover_and_doctor_refuse_the_socket_and_the_unix_socket_transport() {
        for command in ["discover", "doctor"] {
            for (words, received) in [
                (
                    &["--socket", "/run/codex.sock"][..],
                    "--socket \"/run/codex.sock\"",
                ),
                (
                    &["--transport", "unix-socket"][..],
                    "--transport unix-socket",
                ),
                (
                    &[
                        "--harness",
                        "codex",
                        "--transport",
                        "unix-socket",
                        "--socket",
                        "/run/codex.sock",
                    ][..],
                    "--transport unix-socket --socket \"/run/codex.sock\"",
                ),
                (
                    &["--transport", "stdio", "--socket", "/run/codex.sock"][..],
                    "--socket \"/run/codex.sock\"",
                ),
            ] {
                let refused = Options::parse_for(command, &arguments(words));
                assert_eq!(
                    refused.err(),
                    Some(format!(
                        "(mea {command}): expected no --socket and no unix-socket transport; {command} probes the local CLI and never opens a session | received {received}"
                    )),
                    "expected (mea {command}) to refuse {words:?}"
                );
            }
        }
    }

    #[test]
    fn discover_and_doctor_still_parse_without_the_socket() {
        for command in ["discover", "doctor"] {
            Options::parse_for(
                command,
                &arguments(&["--harness", "codex", "--transport", "stdio", "--json"]),
            )
            .unwrap_or_else(|error| {
                panic!("expected a plain {command} to parse, received {error}")
            });
        }
    }

    #[test]
    fn every_accepted_level_and_transport_names_its_own_variant() {
        // Refusing the values outside the set says nothing about the ones inside it: a swapped arm
        // here runs the turn at a permission level the caller did not ask for.
        assert_eq!(
            level("read-only").expect("read-only"),
            PermissionLevel::ReadOnly
        );
        assert_eq!(level("default").expect("default"), PermissionLevel::Default);
        assert_eq!(
            level("full-access").expect("full-access"),
            PermissionLevel::FullAccess
        );
        assert_eq!(transport("stdio").expect("stdio"), TransportKind::Stdio);
        assert_eq!(
            transport("websocket").expect("websocket"),
            TransportKind::WebSocket
        );
        assert_eq!(
            transport("unix-socket").expect("unix-socket"),
            TransportKind::UnixSocket
        );
        assert_eq!(transport("acp").expect("acp"), TransportKind::Acp);
        assert_eq!(
            transport("unix"),
            Err(String::from(
                "expected `stdio`, `websocket`, `unix-socket` or `acp`, received \"unix\""
            )),
            "expected the refusal to list every accepted name"
        );
    }

    #[test]
    fn a_codex_turn_carries_the_model_id_as_typed() {
        let options = Options::parse_for(
            "turn",
            &arguments(&["--harness", "codex", "--model", "gpt-5.2-codex", "hello"]),
        )
        .unwrap_or_else(|error| panic!("expected (mea turn) to accept it, received {error}"));
        assert_eq!(options.model.as_deref(), Some("gpt-5.2-codex"));
        assert_eq!(options.prompt, "hello");

        let without = Options::parse_for("turn", &arguments(&["--harness", "codex", "hello"]))
            .expect("expected a turn without --model");
        assert_eq!(without.model, None);
    }

    #[test]
    fn a_model_for_another_harness_is_refused_with_the_harness_it_received() {
        for (flags, harness) in [
            (vec!["--harness", "claude"], "claude"),
            // No `--harness` runs Claude, the turn's default, which has no model flag here.
            (vec![], "claude"),
            (vec!["--harness", "acp:cursor"], "acp:cursor"),
        ] {
            let mut words = flags;
            words.extend(["--model", "gpt-5.2-codex", "hello"]);
            let refused = Options::parse_for("turn", &arguments(&words));
            assert_eq!(
                refused.err(),
                Some(format!(
                    "(mea turn): expected --model with --harness codex | received --model \"gpt-5.2-codex\" with --harness {harness}"
                ))
            );
        }
    }

    #[test]
    fn discover_and_doctor_refuse_a_model_they_would_never_use() {
        for command in ["discover", "doctor"] {
            let refused = Options::parse_for(
                command,
                &arguments(&["--harness", "codex", "--model", "gpt-5.2-codex"]),
            );
            assert_eq!(
                refused.err(),
                Some(format!(
                    "(mea {command}): expected no --model; {command} probes the local CLI and never opens a session | received --model \"gpt-5.2-codex\""
                ))
            );
        }
    }

    #[test]
    fn model_id_passes_a_plain_id_and_refuses_one_the_harness_would() {
        assert_eq!(
            model_id("turn", "gpt-5.2-codex"),
            Ok("gpt-5.2-codex"),
            "expected an ordinary model id to pass as typed"
        );
        for received in [
            "",
            " ",
            " gpt-5.2-codex",
            "gpt-5.2-codex ",
            "gpt\n5",
            "gpt\u{7f}",
        ] {
            assert_eq!(
                model_id("turn", received),
                Err(format!(
                    "(mea turn): expected --model as a nonempty, bounded model id without surrounding spaces or control characters | received {received:?}"
                ))
            );
        }
        let overlong = "m".repeat(10_000);
        assert!(
            model_id("turn", &overlong).is_err(),
            "expected an id past the vendor-id bound to be refused"
        );
    }

    #[test]
    fn a_malformed_model_stops_the_turn_parse_before_the_harness_check() {
        let refused = Options::parse_for(
            "turn",
            &arguments(&["--harness", "claude", "--model", "", "hello"]),
        );
        assert_eq!(
            refused.err(),
            Some(String::from(
                "(mea turn): expected --model as a nonempty, bounded model id without surrounding spaces or control characters | received \"\""
            ))
        );
    }
}

/// Contract capture arguments. `capture codex` preserves the historical transcript command.
#[derive(Parser)]
#[command(name = "mea capture", disable_help_flag = true)]
pub(crate) struct CaptureOptions {
    #[arg(value_parser = ["codex"], conflicts_with_all = ["harness", "profile", "transcripts"])]
    pub legacy: Option<String>,
    #[arg(long)]
    pub harness: Option<String>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    #[arg(long)]
    pub transcripts: bool,
    /// The Codex model id every transcript conversation opens its thread with.
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, value_parser = clap::builder::PossibleValuesParser::new(crate::capture::scenario_names()))]
    pub scenario: Option<String>,
}

impl CaptureOptions {
    /// Parses capture options without launching processes. Example: `--harness claude --out fixtures/claude`.
    pub fn parse(arguments: &[String]) -> Result<Self, String> {
        Self::try_parse_from(
            std::iter::once("mea capture").chain(arguments.iter().map(String::as_str)),
        )
        .map_err(|error| error.to_string())
    }

    /// Resolves the requested vendor. Example: `acp` uses the documented default capture profile.
    pub fn kind(&self) -> Result<HarnessChoice, String> {
        let name = self.harness.as_deref().unwrap_or("codex");
        let default_profile =
            (name == "acp").then_some(crate::capture::DEFAULT_ACP_CAPTURE_PROFILE);
        let selected = kind(Some(name), self.profile.as_deref().or(default_profile))?
            .ok_or("expected a capture harness, received none")?;
        if self.transcripts && selected != HarnessChoice::Codex {
            return Err(format!(
                "expected codex for --transcripts, received {selected}"
            ));
        }
        if self.scenario.is_some()
            && (selected != HarnessChoice::Codex || (self.legacy.is_none() && !self.transcripts))
        {
            return Err(format!(
                "expected a Codex transcript capture for --scenario, received {selected} without transcript selection"
            ));
        }
        if let Some(model) = &self.model {
            model_id("capture", model)?;
            let transcript =
                selected == HarnessChoice::Codex && (self.legacy.is_some() || self.transcripts);
            if !transcript {
                let received = if selected == HarnessChoice::Codex {
                    format!("--model {model:?} with --harness codex without --transcripts")
                } else {
                    format!("--model {model:?} with --harness {selected}")
                };
                return Err(format!(
                    "(mea capture): expected --model with a Codex transcript capture (--harness codex --transcripts or `codex`) | received {received}"
                ));
            }
        }
        Ok(selected)
    }
}

#[cfg(test)]
mod capture_tests {
    use super::*;

    #[test]
    fn public_capture_and_legacy_transcripts_are_explicit() {
        let options =
            CaptureOptions::parse(&["--harness".into(), "acp".into()]).expect("valid capture");
        assert_eq!(
            options.kind().expect("ACP capture profile"),
            HarnessChoice::Acp(
                ProfileId::new(crate::capture::DEFAULT_ACP_CAPTURE_PROFILE)
                    .expect("expected a valid profile")
            )
        );
        let legacy = CaptureOptions::parse(&["codex".into()]).expect("legacy capture");
        assert!(legacy.legacy.is_some());
        assert_eq!(legacy.kind().expect("Codex"), HarnessChoice::Codex);
    }

    #[test]
    fn malformed_capture_flags_never_fall_back_to_defaults() {
        for args in [
            vec!["--out"],
            vec!["--harness", "claude", "--transcripts"],
            vec!["codex", "--harness", "claude"],
            vec!["--transcrips"],
            vec!["--harness", "claude", "--scenario", "subagent"],
            vec!["--harness", "codex", "--scenario", "subagent"],
            vec!["codex", "--scenario", "unknown"],
            vec!["codex", "--scenario"],
        ] {
            let parsed =
                CaptureOptions::parse(&args.into_iter().map(String::from).collect::<Vec<_>>());
            assert!(parsed.and_then(|options| options.kind()).is_err());
        }
    }

    #[test]
    fn every_named_scenario_is_accepted_only_for_explicit_codex_transcripts() {
        for name in crate::capture::scenario_names() {
            for args in [
                vec!["codex", "--scenario", name],
                vec!["--harness", "codex", "--transcripts", "--scenario", name],
            ] {
                let options =
                    CaptureOptions::parse(&args.into_iter().map(String::from).collect::<Vec<_>>())
                        .expect("registered scenario");
                assert_eq!(
                    options.kind().expect("transcript capture"),
                    HarnessChoice::Codex
                );
                assert_eq!(options.scenario.as_deref(), Some(name));
            }
        }
    }

    fn capture_arguments(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    #[test]
    fn a_model_is_accepted_for_both_spellings_of_a_codex_transcript_capture() {
        for words in [
            vec![
                "--harness",
                "codex",
                "--transcripts",
                "--model",
                "gpt-5.2-codex",
            ],
            vec!["codex", "--model", "gpt-5.2-codex"],
        ] {
            let options = CaptureOptions::parse(&capture_arguments(&words))
                .expect("expected the capture arguments to parse");
            assert_eq!(options.kind(), Ok(HarnessChoice::Codex));
            assert_eq!(options.model.as_deref(), Some("gpt-5.2-codex"));
        }
        let without = CaptureOptions::parse(&capture_arguments(&["codex"]))
            .expect("expected a capture without --model");
        assert_eq!(without.model, None);
    }

    #[test]
    fn a_model_is_refused_where_no_codex_transcript_would_use_it() {
        for (words, received) in [
            (
                vec!["--harness", "codex", "--model", "gpt-5.2-codex"],
                "--model \"gpt-5.2-codex\" with --harness codex without --transcripts",
            ),
            (
                vec!["--model", "gpt-5.2-codex"],
                "--model \"gpt-5.2-codex\" with --harness codex without --transcripts",
            ),
            (
                vec!["--harness", "claude", "--model", "gpt-5.2-codex"],
                "--model \"gpt-5.2-codex\" with --harness claude",
            ),
            (
                vec!["--harness", "acp:cursor", "--model", "gpt-5.2-codex"],
                "--model \"gpt-5.2-codex\" with --harness acp:cursor",
            ),
        ] {
            let refused = CaptureOptions::parse(&capture_arguments(&words))
                .and_then(|options| options.kind());
            assert_eq!(
                refused.err(),
                Some(format!(
                    "(mea capture): expected --model with a Codex transcript capture (--harness codex --transcripts or `codex`) | received {received}"
                )),
                "expected {words:?} to be refused with the value it received"
            );
        }
    }

    #[test]
    fn a_malformed_capture_model_is_refused_with_the_value_it_received() {
        let refused = CaptureOptions::parse(&capture_arguments(&["codex", "--model", ""]))
            .and_then(|options| options.kind());
        assert_eq!(
            refused.err(),
            Some(String::from(
                "(mea capture): expected --model as a nonempty, bounded model id without surrounding spaces or control characters | received \"\""
            ))
        );
    }
}

/// Fixture digest arguments. Example: `mea digests --out fixtures/claude`.
#[derive(Parser)]
#[command(name = "mea digests", disable_help_flag = true)]
pub(crate) struct DigestOptions {
    #[arg(long)]
    pub out: Option<PathBuf>,
    #[arg(long)]
    pub check: bool,
}

impl DigestOptions {
    /// Parses digest options without touching the tree. Example: `--out fixtures/codex`.
    pub fn parse(arguments: &[String]) -> Result<Self, String> {
        Self::try_parse_from(
            std::iter::once("mea digests").chain(arguments.iter().map(String::as_str)),
        )
        .map_err(|error| error.to_string())
    }

    /// The fixture root to walk, the whole `fixtures` tree unless told otherwise.
    pub fn root(&self) -> PathBuf {
        self.out
            .clone()
            .unwrap_or_else(|| PathBuf::from("fixtures"))
    }
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    #[test]
    fn digests_default_to_the_whole_fixture_tree() {
        let options = DigestOptions::parse(&[]).expect("expected default digest options");
        assert_eq!(options.root(), PathBuf::from("fixtures"));

        assert!(!options.check);

        let scoped =
            DigestOptions::parse(&["--out".into(), "fixtures/codex".into(), "--check".into()])
                .expect("expected a scoped digest root");
        assert_eq!(scoped.root(), PathBuf::from("fixtures/codex"));
        assert!(scoped.check);
        assert!(DigestOptions::parse(&["--oot".into()]).is_err());
    }
}

/// The fixture root for one captured profile. Example: ACP OpenCode lives in `fixtures/acp/opencode`.
pub(crate) fn capture_output(kind: &HarnessChoice) -> PathBuf {
    let vendor = match kind {
        HarnessChoice::Claude => "claude",
        HarnessChoice::Codex => "codex",
        HarnessChoice::Acp(_) => "acp",
    };
    let root = PathBuf::from("fixtures").join(vendor);
    match kind {
        HarnessChoice::Acp(profile) => root.join(profile.as_str()),
        HarnessChoice::Claude | HarnessChoice::Codex => root,
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn acp_captures_default_to_separate_profile_directories() {
        assert_eq!(
            capture_output(&HarnessChoice::Acp(
                ProfileId::new("opencode").expect("expected a valid profile")
            )),
            PathBuf::from("fixtures/acp/opencode")
        );
        assert_eq!(
            capture_output(&HarnessChoice::Acp(
                ProfileId::new("cursor").expect("expected a valid profile")
            )),
            PathBuf::from("fixtures/acp/cursor")
        );
        assert_eq!(
            capture_output(&HarnessChoice::Claude),
            PathBuf::from("fixtures/claude")
        );
        assert_eq!(
            capture_output(&HarnessChoice::Codex),
            PathBuf::from("fixtures/codex")
        );
    }

    #[test]
    fn a_harness_choice_maps_onto_the_registrys_own_id() {
        assert_eq!(HarnessChoice::Claude.id(), HarnessId::claude());
        assert_eq!(HarnessChoice::Codex.id(), HarnessId::codex());
        let profile = ProfileId::new("cursor").expect("expected a valid profile");
        assert_eq!(
            HarnessChoice::Acp(profile.clone()).id(),
            HarnessId::acp(&profile)
        );
        assert_eq!(HarnessChoice::Codex.to_string(), "codex");
    }
}
