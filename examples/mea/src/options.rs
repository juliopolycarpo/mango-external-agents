//! Shared discovery and turn arguments, checked before launching a vendor.

use std::fmt;
use std::path::PathBuf;

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
    cwd: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    prompt: Vec<String>,
}

/// What this CLI's own `--harness` flag selected.
///
/// The library dropped its `HarnessKind` enum for [`HarnessId`], a validated string a host
/// dispatches on — but a `match` over a string prefix (`"acp:"`) is worse than the enum it
/// replaced. This is `mea`'s own three-way vocabulary for `claude`, `codex` and `acp:<profile>`;
/// [`HarnessChoice::id`] is the one place it is translated into the registry's own key.
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
    pub cwd: Option<PathBuf>,
    pub json: bool,
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
            cwd: args.cwd,
            json: args.json,
            prompt: args.prompt.join(" "),
        })
    }
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
        "acp" => Ok(TransportKind::Acp),
        other => Err(format!(
            "expected `stdio`, `websocket` or `acp`, received {other:?}"
        )),
    }
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
        assert_eq!(options.prompt, "hello");
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
            vec!["--transport", "unix-socket"],
        ] {
            assert!(
                Options::parse(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err()
            );
        }
        assert!(kind(Some("acp"), None).is_err());
        assert!(acp("cursor").is_ok());
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
        assert_eq!(transport("acp").expect("acp"), TransportKind::Acp);
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
        ] {
            let parsed =
                CaptureOptions::parse(&args.into_iter().map(String::from).collect::<Vec<_>>());
            assert!(parsed.and_then(|options| options.kind()).is_err());
        }
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
