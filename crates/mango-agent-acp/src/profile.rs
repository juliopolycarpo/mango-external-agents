//! One profile per ACP agent: its argv, its floor, its documented login command, its quirks.
//!
//! Every agent here speaks the same protocol, so the harness is one implementation. What differs
//! is everything *around* the protocol — how the agent is spelled on a command line, what
//! `--version` prints, which document a host's disclosure has to link, and which mode ids (if any)
//! its `session/new` advertises. That is what a profile carries, and it is data rather than code so
//! adding an agent is a table entry rather than a new harness.
//!
//! The set is open on purpose: [`AcpProfile::custom`] takes a host's own argv, and an ACP agent
//! nobody has heard of yet speaks the same v1 wire as the ones with a built-in entry.
//!
//! # Verified and unverified
//!
//! [`AcpProfile::verified`] says whether the entry was checked against the agent actually running
//! on a machine. An unverified profile is a documented guess: it is offered, and a host can say so
//! in its own interface, but nothing here pretends a capture exists that does not.

use std::sync::Arc;

use mango_external_agents::permission::{
    ApprovalRouting, ConfigurationVerdict, PermissionLevel, PermissionMatrix, UnsupportedReason,
};
use mango_external_agents::{AcpProfileId, VendorInfo};

/// The agent's own mode ids for the three permission levels, when a profile knows them.
///
/// ACP v1's one documented lever for "what may this agent do" is `session/set_mode` over the mode
/// ids the agent itself advertised in `session/new`. The ids are the agent's, not the protocol's,
/// so they live per profile and every one of them is `None` until a capture proves otherwise.
///
/// What an absent id means is different per level, and the difference is the whole reason this is
/// three fields rather than a flag — see [`matrix`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionModeIds {
    /// The mode where the agent may read and answer and nothing else.
    pub read_only: Option<&'static str>,
    /// The mode where it acts but asks first.
    pub default: Option<&'static str>,
    /// The mode where it acts without asking.
    pub full_access: Option<&'static str>,
}

impl SessionModeIds {
    /// No mode ids known for this agent.
    pub const UNKNOWN: Self = Self {
        read_only: None,
        default: None,
        full_access: None,
    };

    /// The agent's own id for `level`, when this profile knows one.
    #[must_use]
    pub const fn for_level(&self, level: PermissionLevel) -> Option<&'static str> {
        match level {
            PermissionLevel::ReadOnly => self.read_only,
            PermissionLevel::Default => self.default,
            PermissionLevel::FullAccess => self.full_access,
        }
    }
}

/// Everything about one ACP agent that is not the protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AcpProfile {
    /// How this profile is named, which is also half of its [`HarnessKind`](mango_external_agents::HarnessKind).
    pub id: AcpProfileId,
    /// A name for a person, for a picker row.
    pub display_name: String,
    /// Who owns the CLI, and the documents a host's disclosure links.
    pub vendor: VendorInfo,
    /// The argv that starts the agent in ACP mode. `argv[0]` is replaced by
    /// [`OpenSession::executable`](mango_external_agents::OpenSession) when the host resolved one.
    pub argv: Vec<String>,
    /// The argv that prints a version, for the probe.
    pub version_argv: Vec<String>,
    /// The oldest build this profile drives, when one is pinned.
    ///
    /// `None` means no floor is claimed: an agent whose release history nobody has checked is
    /// better driven with an unknown gate than refused by a number somebody invented.
    pub minimum_version: Option<String>,
    /// The agent's own login command, verbatim, for a host to show a person.
    ///
    /// Text, never something the library runs. An agent with no login command of its own carries
    /// [`None`] and the host says what it can.
    pub login_hint: Option<String>,
    /// Where this agent's ACP mode is documented.
    pub docs_url: &'static str,
    /// The agent's own mode ids, when a capture proved them.
    pub modes: SessionModeIds,
    /// Vendor-documented environment variables that survive the allowlist.
    pub vendor_environment_keys: &'static [&'static str],
    /// Whether this entry was checked against the agent actually running.
    pub verified: bool,
}

impl AcpProfile {
    /// A profile for an agent the built-in table does not know.
    ///
    /// The host supplies the argv, because the library will not guess one, and the
    /// [`VendorInfo`] — a host driving somebody else's agent is the party that has to name whose
    /// terms its users are under, and the library has no way to find out.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_acp::AcpProfile;
    /// use mango_external_agents::VendorInfo;
    ///
    /// const VENDOR: VendorInfo = VendorInfo {
    ///     company: "Example Inc",
    ///     terms_url: "https://example.com/terms",
    ///     privacy_url: "https://example.com/privacy",
    ///     skills_are_slash_commands: false,
    /// };
    ///
    /// let profile = AcpProfile::custom("in-house", ["my-agent", "--acp"], VENDOR);
    /// assert_eq!(profile.id.as_str(), "in-house");
    /// assert!(!profile.verified);
    /// ```
    pub fn custom<I, S>(id: impl Into<String>, argv: I, vendor: VendorInfo) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let argv: Vec<String> = argv.into_iter().map(Into::into).collect();
        let version_argv = argv
            .first()
            .map(|program| vec![program.clone(), String::from("--version")])
            .unwrap_or_default();
        let id = id.into();
        Self {
            display_name: id.clone(),
            id: AcpProfileId::new(id),
            vendor,
            argv,
            version_argv,
            minimum_version: None,
            login_hint: None,
            docs_url: ACP_PROTOCOL_DOCS,
            modes: SessionModeIds::UNKNOWN,
            vendor_environment_keys: &[],
            verified: false,
        }
    }

    /// Probes with this argv instead of `<program> --version`.
    #[must_use]
    pub fn with_version_argv<I, S>(mut self, argv: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.version_argv = argv.into_iter().map(Into::into).collect();
        self
    }

    /// Refuses builds older than this.
    #[must_use]
    pub fn with_minimum_version(mut self, minimum: impl Into<String>) -> Self {
        self.minimum_version = Some(minimum.into());
        self
    }

    /// Carries the agent's own login command as text for a host to display.
    #[must_use]
    pub fn with_login_hint(mut self, hint: impl Into<String>) -> Self {
        self.login_hint = Some(hint.into());
        self
    }

    /// Records the agent's own mode ids, once a capture has proved them.
    #[must_use]
    pub fn with_modes(mut self, modes: SessionModeIds) -> Self {
        self.modes = modes;
        self
    }

    /// Lets these vendor-documented environment variables through the allowlist.
    ///
    /// Named by the profile from the agent's own documentation, never from a host request: this is
    /// not a seam for a host to pass a secret of its own into a third party's process. Each key
    /// this crate ships is cited in `docs/harness-acp.md`.
    #[must_use]
    pub fn with_vendor_environment_keys(mut self, keys: &'static [&'static str]) -> Self {
        self.vendor_environment_keys = keys;
        self
    }

    /// Marks this entry as checked against the agent actually running.
    #[must_use]
    pub fn verified(mut self) -> Self {
        self.verified = true;
        self
    }

    /// The text a host shows somebody who has to sign in.
    ///
    /// The agent's own command when it has one, and its documentation when it does not. A URL rather
    /// than an invented command: an agent whose sign-in happens inside an interactive session has no
    /// command to print, and printing a plausible one would send a person to a prompt that does not
    /// exist.
    ///
    /// # Example
    ///
    /// ```
    /// let opencode = mango_agent_acp::builtin_profile("opencode").expect("a built-in profile");
    /// assert_eq!(opencode.login_text(), "opencode auth login");
    ///
    /// let gemini = mango_agent_acp::builtin_profile("gemini").expect("a built-in profile");
    /// assert!(gemini.login_text().starts_with("see https://"));
    /// ```
    #[must_use]
    pub fn login_text(&self) -> String {
        self.login_hint
            .clone()
            .unwrap_or_else(|| format!("see {}", self.docs_url))
    }

    /// The program the probe and the session launch, with a host-resolved path winning.
    #[must_use]
    pub fn program(&self, executable: &mango_external_agents::ExecutablePath) -> String {
        executable.or(self.argv.first().cloned().unwrap_or_default())
    }

    /// This profile's argv with `argv[0]` replaced by a host-resolved path, when there is one.
    #[must_use]
    pub fn resolved_argv(&self, executable: &mango_external_agents::ExecutablePath) -> Vec<String> {
        replace_program(&self.argv, executable)
    }

    /// This profile's version argv with `argv[0]` replaced the same way.
    #[must_use]
    pub fn resolved_version_argv(
        &self,
        executable: &mango_external_agents::ExecutablePath,
    ) -> Vec<String> {
        replace_program(&self.version_argv, executable)
    }
}

fn replace_program(
    argv: &[String],
    executable: &mango_external_agents::ExecutablePath,
) -> Vec<String> {
    let mut argv = argv.to_vec();
    if let Some(program) = argv.first_mut() {
        *program = executable.or(program.clone());
    }
    argv
}

/// Which (level, routing) pairs an ACP agent under these mode ids can run.
///
/// Routing never varies: who answers an approval is the host's own arrangement — a
/// [`PermissionBroker`](mango_external_agents::PermissionBroker) or the
/// [`ApprovalRequested`](mango_external_agents::EventKind) event — and the agent cannot tell the
/// difference. Level does, and the three cases are not symmetric:
///
/// * **`Default`** is what plain ACP already is. The agent asks before anything that needs
///   permission and somebody answers; no mode is required, and a profile that knows a mode id for
///   it names it so `session/set_mode` can make it explicit.
/// * **`ReadOnly`** needs no mode either, because refusing every request the agent raises grants
///   nothing. It is the only level the library can reach by answering, and answering "no" on a
///   host's standing instruction is not the library deciding anything.
/// * **`FullAccess`** is unreachable without a mode id. Getting there by answering would mean the
///   library allowing on the agent's behalf, which is the one thing it must never do — so an agent
///   with no documented full-access mode reports [`UnsupportedReason::NotOfferedByVendor`] rather
///   than a cell that quietly turns into "ask every time".
///
/// # Example
///
/// ```
/// use mango_agent_acp::profile::{SessionModeIds, matrix};
/// use mango_external_agents::{ApprovalRouting, PermissionLevel};
///
/// let plain = matrix(&SessionModeIds::UNKNOWN);
/// assert!(plain.supports(PermissionLevel::ReadOnly, ApprovalRouting::User));
/// assert!(plain.supports(PermissionLevel::Default, ApprovalRouting::AutoReview));
/// assert!(!plain.supports(PermissionLevel::FullAccess, ApprovalRouting::User));
///
/// let with_yolo = matrix(&SessionModeIds { full_access: Some("bypassPermissions"), ..SessionModeIds::UNKNOWN });
/// assert!(with_yolo.supports(PermissionLevel::FullAccess, ApprovalRouting::User));
/// ```
#[must_use]
pub fn matrix(modes: &SessionModeIds) -> PermissionMatrix {
    PermissionMatrix::build(|level, routing| {
        // Named and dropped rather than ignored with `_`: the reader's first question here is
        // whether routing was forgotten, and it was not — see this function's own docs.
        let (ApprovalRouting::User | ApprovalRouting::AutoReview) = routing;
        let vendor_id = modes.for_level(level).map(str::to_owned);
        match level {
            PermissionLevel::ReadOnly | PermissionLevel::Default => {
                ConfigurationVerdict::Supported { vendor_id }
            }
            PermissionLevel::FullAccess => match vendor_id {
                Some(vendor_id) => ConfigurationVerdict::Supported {
                    vendor_id: Some(vendor_id),
                },
                None => ConfigurationVerdict::Unsupported {
                    reason: UnsupportedReason::NotOfferedByVendor,
                    vendor_id: None,
                },
            },
        }
    })
}

/// The protocol's own documentation, for a profile with nothing more specific.
pub const ACP_PROTOCOL_DOCS: &str = "https://agentclientprotocol.com/protocol/overview";

/// Every profile this crate ships, in a stable order.
///
/// # Example
///
/// ```
/// let ids: Vec<String> = mango_agent_acp::builtin_profiles()
///     .iter()
///     .map(|profile| profile.id.to_string())
///     .collect();
/// assert!(ids.contains(&String::from("cursor")));
/// assert!(ids.contains(&String::from("opencode")));
/// ```
#[must_use]
pub fn builtin_profiles() -> Vec<Arc<AcpProfile>> {
    vec![
        Arc::new(cursor()),
        Arc::new(opencode()),
        Arc::new(gemini()),
        Arc::new(copilot()),
        Arc::new(goose()),
        Arc::new(codex_acp()),
        Arc::new(claude_agent_acp()),
    ]
}

/// One built-in profile by id.
#[must_use]
pub fn builtin_profile(id: &str) -> Option<Arc<AcpProfile>> {
    builtin_profiles()
        .into_iter()
        .find(|profile| profile.id.as_str() == id)
}

fn profile(
    id: &'static str,
    display_name: &'static str,
    vendor: VendorInfo,
    argv: &[&'static str],
    docs_url: &'static str,
) -> AcpProfile {
    AcpProfile {
        display_name: String::from(display_name),
        docs_url,
        ..AcpProfile::custom(id, argv.iter().copied(), vendor)
    }
}

/// Cursor's CLI in ACP mode.
///
/// The binary is `agent`. `cursor-agent` is a legacy alias some installs still ship, and a host
/// whose machine has only that one resolves it and passes the path on
/// [`OpenSession::executable`](mango_external_agents::OpenSession).
fn cursor() -> AcpProfile {
    profile(
        "cursor",
        "Cursor CLI",
        VendorInfo {
            company: "Anysphere",
            terms_url: "https://cursor.com/terms-of-service",
            privacy_url: "https://cursor.com/privacy",
            skills_are_slash_commands: true,
        },
        &["agent", "acp"],
        "https://cursor.com/docs/cli/acp",
    )
    .with_login_hint("agent login")
}

/// OpenCode in ACP mode.
fn opencode() -> AcpProfile {
    profile(
        "opencode",
        "OpenCode",
        VendorInfo {
            company: "Anomaly Innovations",
            terms_url: "https://opencode.ai/legal/terms-of-service",
            privacy_url: "https://opencode.ai/legal/privacy-policy",
            skills_are_slash_commands: true,
        },
        &["opencode", "acp"],
        "https://opencode.ai/docs/acp/",
    )
    .with_login_hint("opencode auth login")
}

/// Gemini CLI in ACP mode.
///
/// `--acp` is the current flag; `--experimental-acp` is its deprecated predecessor, still accepted.
/// The CLI has no login subcommand — signing in happens inside an interactive `gemini` session — so
/// this profile carries no login hint and the harness falls back to the documentation link.
fn gemini() -> AcpProfile {
    profile(
        "gemini",
        "Gemini CLI",
        VendorInfo {
            company: "Google",
            terms_url: "https://policies.google.com/terms",
            privacy_url: "https://policies.google.com/privacy",
            skills_are_slash_commands: false,
        },
        &["gemini", "--acp"],
        "https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md",
    )
    .with_vendor_environment_keys(&["GEMINI_API_KEY", "GOOGLE_API_KEY"])
}

/// GitHub Copilot CLI in ACP mode.
///
/// stdio is the default carrier; `--acp --port N` selects TCP instead, which this harness does not
/// drive.
fn copilot() -> AcpProfile {
    profile(
        "copilot",
        "GitHub Copilot CLI",
        VendorInfo {
            company: "GitHub",
            terms_url: "https://docs.github.com/site-policy/github-terms/github-terms-of-service",
            privacy_url:
                "https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement",
            skills_are_slash_commands: false,
        },
        &["copilot", "--acp"],
        "https://docs.github.com/copilot/reference/copilot-cli-reference/acp-server",
    )
    .with_login_hint("copilot login")
}

/// Goose in ACP mode.
///
/// `terms_url` is the project's own acceptable-usage document rather than a corporate page: Goose
/// is Apache-2.0 and brings its own model credentials, and Block publishes no single terms of
/// service covering it. Pointing at a page that does not govern this tool would be worse than
/// pointing at the one that does.
fn goose() -> AcpProfile {
    profile(
        "goose",
        "Goose",
        VendorInfo {
            company: "Block",
            terms_url: "https://github.com/block/goose/blob/main/ACCEPTABLE_USAGE.md",
            privacy_url: "https://block.xyz/legal/privacy-notice",
            skills_are_slash_commands: false,
        },
        &["goose", "acp", "--with-builtin", "developer"],
        "https://block.github.io/goose/docs/advanced/acp-protocol",
    )
    .with_login_hint("goose configure")
}

/// The ACP adapter that fronts OpenAI Codex.
///
/// The argv is the installed binary, not `npx -y @agentclientprotocol/codex-acp`. The package's own
/// README documents the `npx` form for a person to run, and the library does not invoke a package
/// fetcher on its own initiative — "no downloaded binaries" is an invariant, not a default. A host
/// that wants the `npx` form supplies it through [`AcpProfile::custom`].
fn codex_acp() -> AcpProfile {
    profile(
        "codex-acp",
        "Codex (ACP adapter)",
        VendorInfo {
            company: "OpenAI",
            terms_url: "https://openai.com/policies/terms-of-use/",
            privacy_url: "https://openai.com/policies/privacy-policy/",
            skills_are_slash_commands: false,
        },
        &["codex-acp"],
        "https://github.com/agentclientprotocol/codex-acp",
    )
    .with_login_hint("codex login")
    // `NO_BROWSER` is the adapter's own documented way to stop it opening a sign-in page. The
    // library never opens a browser; letting the variable through is how a host says the adapter
    // must not either.
    .with_vendor_environment_keys(&["CODEX_API_KEY", "OPENAI_API_KEY", "NO_BROWSER"])
}

/// The ACP adapter that fronts Claude Code.
///
/// Launched as the installed binary for the same reason as [`codex_acp`]. The package moved twice —
/// `@zed-industries/claude-code-acp`, then `@zed-industries/claude-agent-acp`, now
/// `@agentclientprotocol/claude-agent-acp` — and the binary moved with it, so this profile is
/// `claude-agent-acp` rather than the older spelling.
fn claude_agent_acp() -> AcpProfile {
    profile(
        "claude-agent-acp",
        "Claude Code (ACP adapter)",
        VendorInfo {
            company: "Anthropic",
            terms_url: "https://www.anthropic.com/legal/consumer-terms",
            privacy_url: "https://www.anthropic.com/legal/privacy",
            skills_are_slash_commands: true,
        },
        &["claude-agent-acp"],
        "https://github.com/agentclientprotocol/claude-agent-acp",
    )
    .with_login_hint("claude auth login")
    .with_vendor_environment_keys(&["ANTHROPIC_API_KEY"])
}

#[cfg(test)]
mod tests {
    use super::{AcpProfile, SessionModeIds, builtin_profile, builtin_profiles, matrix};
    use mango_external_agents::permission::{ApprovalRouting, PermissionLevel, UnsupportedReason};
    use mango_external_agents::{ExecutablePath, VendorInfo};

    const VENDOR: VendorInfo = VendorInfo {
        company: "Example Inc",
        terms_url: "https://example.com/terms",
        privacy_url: "https://example.com/privacy",
        skills_are_slash_commands: false,
    };

    #[test]
    fn every_builtin_profile_names_a_program_and_a_document() {
        for profile in builtin_profiles() {
            assert!(
                !profile.argv.is_empty(),
                "expected an argv, received none for {}",
                profile.id
            );
            assert!(
                profile.docs_url.starts_with("https://"),
                "expected an https document, received {:?} for {}",
                profile.docs_url,
                profile.id
            );
            assert!(
                profile.vendor.terms_url.starts_with("https://")
                    && profile.vendor.privacy_url.starts_with("https://")
                    && !profile.vendor.company.is_empty(),
                "expected a named vendor with two https documents, received {:?} for {}",
                profile.vendor,
                profile.id
            );
        }
    }

    #[test]
    fn profile_ids_are_distinct() {
        let mut ids: Vec<String> = builtin_profiles()
            .iter()
            .map(|profile| profile.id.to_string())
            .collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "expected distinct ids, received {ids:?}");
    }

    /// Nothing in this crate has been run against a released agent yet, and a profile that claimed
    /// otherwise would put a capture in a host's interface that does not exist. When the first
    /// capture lands, this test is what has to be updated alongside it.
    #[test]
    fn no_builtin_profile_claims_to_be_verified_yet() {
        let claimed: Vec<String> = builtin_profiles()
            .iter()
            .filter(|profile| profile.verified)
            .map(|profile| profile.id.to_string())
            .collect();
        assert!(
            claimed.is_empty(),
            "expected every builtin to be unverified until captured, received {claimed:?}"
        );
    }

    /// The npm shims are the two profiles where the obvious argv would have the library fetch a
    /// package. It launches the installed binary instead, and this is the test that says so.
    #[test]
    fn no_builtin_profile_launches_a_package_fetcher() {
        for profile in builtin_profiles() {
            for argv in [&profile.argv, &profile.version_argv] {
                assert!(
                    !argv
                        .iter()
                        .any(|word| matches!(word.as_str(), "npx" | "bunx" | "pnpx" | "uvx")),
                    "expected no package fetcher, received {argv:?} for {}",
                    profile.id
                );
            }
        }
    }

    #[test]
    fn a_builtin_is_found_by_id_and_an_unknown_one_is_not() {
        assert!(builtin_profile("opencode").is_some());
        assert!(builtin_profile("not-an-agent").is_none());
    }

    #[test]
    fn a_custom_profile_probes_its_own_program_for_a_version() {
        let profile = AcpProfile::custom("in-house", ["my-agent", "--acp"], VENDOR);
        assert_eq!(profile.version_argv, vec!["my-agent", "--version"]);
        assert_eq!(profile.login_hint, None);
        assert!(!profile.verified);
    }

    /// `argv[0]` is what the launcher spawns, so a path the host resolved has to win in both the
    /// probe and the session — a profile that only replaced one of them would probe one binary and
    /// then drive another.
    #[test]
    fn a_host_resolved_executable_replaces_the_program_in_both_argvs() {
        let profile = AcpProfile::custom("in-house", ["my-agent", "--acp"], VENDOR);
        let resolved = ExecutablePath::resolved("/opt/agents/bin/my-agent");

        assert_eq!(
            profile.resolved_argv(&resolved),
            vec!["/opt/agents/bin/my-agent", "--acp"]
        );
        assert_eq!(
            profile.resolved_version_argv(&resolved),
            vec!["/opt/agents/bin/my-agent", "--version"]
        );
        assert_eq!(
            profile.resolved_argv(&ExecutablePath::default()),
            vec!["my-agent", "--acp"]
        );
    }

    #[test]
    fn full_access_is_refused_until_a_profile_knows_the_agents_own_mode_for_it() {
        let plain = matrix(&SessionModeIds::UNKNOWN);
        for routing in ApprovalRouting::ALL {
            let cell = plain
                .cell(PermissionLevel::FullAccess, routing)
                .expect("expected a full-access cell");
            assert!(!cell.supported, "received {cell:?}");
            assert_eq!(
                cell.unsupported_reason,
                Some(UnsupportedReason::NotOfferedByVendor),
                "received {cell:?}"
            );
        }
    }

    #[test]
    fn read_only_and_default_hold_on_every_agent_because_neither_needs_the_library_to_allow() {
        let plain = matrix(&SessionModeIds::UNKNOWN);
        for routing in ApprovalRouting::ALL {
            for level in [PermissionLevel::ReadOnly, PermissionLevel::Default] {
                assert!(
                    plain.supports(level, routing),
                    "expected {level:?}/{routing:?} to hold, received a refusal"
                );
            }
        }
    }

    #[test]
    fn a_known_mode_id_reaches_the_cell_that_will_send_it() {
        let modes = SessionModeIds {
            read_only: Some("plan"),
            default: Some("default"),
            full_access: Some("bypassPermissions"),
        };
        let matrix = matrix(&modes);
        for (level, expected) in [
            (PermissionLevel::ReadOnly, "plan"),
            (PermissionLevel::Default, "default"),
            (PermissionLevel::FullAccess, "bypassPermissions"),
        ] {
            let cell = matrix
                .cell(level, ApprovalRouting::User)
                .expect("expected a cell");
            assert!(cell.supported, "received {cell:?}");
            assert_eq!(
                cell.vendor_id.as_deref(),
                Some(expected),
                "received {cell:?}"
            );
        }
    }
}
