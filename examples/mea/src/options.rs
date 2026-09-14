//! Shared discovery and turn arguments, checked before launching a vendor.

use std::path::PathBuf;

use clap::Parser;
use mango_external_agents::{AcpProfileId, HarnessKind, PermissionLevel, TransportKind};

#[derive(Parser)]
#[command(name = "mea", disable_help_flag = true)]
struct Arguments {
    #[arg(long)]
    harness: Option<String>,
    #[arg(long)]
    profile: Option<String>,
    #[arg(long, value_parser = ["read-only", "default", "full-access"])]
    level: Option<String>,
    #[arg(long, value_parser = ["stdio", "websocket", "acp"])]
    transport: Option<String>,
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long)]
    json: bool,
    prompt: Vec<String>,
}

pub(crate) struct Options {
    pub kind: Option<HarnessKind>,
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
            level: args.level.map(|level| match level.as_str() {
                "read-only" => PermissionLevel::ReadOnly,
                "full-access" => PermissionLevel::FullAccess,
                _ => PermissionLevel::Default,
            }),
            transport: args.transport.map(|transport| match transport.as_str() {
                "acp" => TransportKind::Acp,
                "websocket" => TransportKind::WebSocket,
                _ => TransportKind::Stdio,
            }),
            cwd: args.cwd,
            json: args.json,
            prompt: args.prompt.join(" "),
        })
    }
}

fn kind(harness: Option<&str>, profile: Option<&str>) -> Result<Option<HarnessKind>, String> {
    match (harness, profile) {
        (None, None) => Ok(None),
        (Some("claude"), None) => Ok(Some(HarnessKind::Claude)),
        (Some("codex"), None) => Ok(Some(HarnessKind::Codex)),
        (Some("acp"), Some(profile)) => acp(profile),
        (Some(name), None) if name.starts_with("acp:") => acp(&name[4..]),
        _ => Err(format!(
            "expected claude, codex, acp:<profile> or acp --profile <id>; received harness {harness:?}, profile {profile:?}"
        )),
    }
}

fn acp(profile: &str) -> Result<Option<HarnessKind>, String> {
    if mango_agent_acp::builtin_profile(profile).is_none() {
        return Err(format!(
            "expected a built-in ACP profile, received {profile:?}"
        ));
    }
    Ok(Some(HarnessKind::Acp(AcpProfileId::new(profile))))
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
            Some(HarnessKind::Acp(AcpProfileId::new("cursor")))
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
        ] {
            assert!(
                Options::parse(&args.into_iter().map(String::from).collect::<Vec<_>>()).is_err()
            );
        }
        assert!(kind(Some("acp"), None).is_err());
        assert!(acp("cursor").is_ok());
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
    pub fn kind(&self) -> Result<HarnessKind, String> {
        let name = self.harness.as_deref().unwrap_or("codex");
        let default_profile =
            (name == "acp").then_some(crate::capture::DEFAULT_ACP_CAPTURE_PROFILE);
        let selected = kind(Some(name), self.profile.as_deref().or(default_profile))?
            .ok_or("expected a capture harness, received none")?;
        if self.transcripts && selected != HarnessKind::Codex {
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
            HarnessKind::Acp(AcpProfileId::new(
                crate::capture::DEFAULT_ACP_CAPTURE_PROFILE
            ))
        );
        let legacy = CaptureOptions::parse(&["codex".into()]).expect("legacy capture");
        assert!(legacy.legacy.is_some());
        assert_eq!(legacy.kind().expect("Codex"), HarnessKind::Codex);
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

/// The fixture root for one captured profile. Example: ACP OpenCode lives in `fixtures/acp/opencode`.
pub(crate) fn capture_output(kind: &HarnessKind) -> PathBuf {
    let vendor = match kind {
        HarnessKind::Claude => "claude",
        HarnessKind::Codex => "codex",
        _ => "acp",
    };
    let root = PathBuf::from("fixtures").join(vendor);
    match kind {
        HarnessKind::Acp(profile) => root.join(profile.to_string()),
        _ => root,
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn acp_captures_default_to_separate_profile_directories() {
        assert_eq!(
            capture_output(&HarnessKind::Acp(AcpProfileId::new("opencode"))),
            PathBuf::from("fixtures/acp/opencode")
        );
        assert_eq!(
            capture_output(&HarnessKind::Acp(AcpProfileId::new("cursor"))),
            PathBuf::from("fixtures/acp/cursor")
        );
        assert_eq!(
            capture_output(&HarnessKind::Claude),
            PathBuf::from("fixtures/claude")
        );
        assert_eq!(
            capture_output(&HarnessKind::Codex),
            PathBuf::from("fixtures/codex")
        );
    }
}
