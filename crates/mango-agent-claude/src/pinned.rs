//! What this harness was written against, and what a live `claude` must be for it to apply.
//!
//! The programmatic surface is Anthropic's own: `claude --print --output-format stream-json` is
//! documented as the headless interface, including `parent_tool_use_id` reconstruction for an
//! external consumer and defined SIGTERM behaviour for a host closing the session. Nothing here
//! has to argue that driving the CLI programmatically is intended.
//!
//! <https://code.claude.com/docs/en/headless.md>

use std::path::PathBuf;
use std::time::Duration;

use mango_external_agents::{EnvSource, VendorInfo};

/// The oldest `claude` this harness will drive, as the vendor spells it.
///
/// Not 2.1.200. That is the floor for the `manual` alias alone, and every turn this harness builds
/// also passes `--forward-subagent-text`, which arrived in **2.1.211** — the changelog entry reads
/// "Added `--forward-subagent-text` flag and `CLAUDE_CODE_FORWARD_SUBAGENT_TEXT` environment
/// variable to include subagent text and thinking in stream-json output". Pinning the lower number
/// would produce a discovery that looks usable and a turn that fails on an unknown flag.
///
/// Two later builds change behaviour without changing what this harness may pass, so they are
/// recorded here rather than gated on:
///
/// - **2.1.219** is where subagent text stops being flat. Below it, `--forward-subagent-text`
///   emits one level, so a nested subagent's output is attributed to the wrong parent or dropped.
///   A partially nested transcript is still worth running.
/// - **2.1.223** is where `--resume` stops being scoped to the project directory the session was
///   created in. This harness passes the same working directory on resume either way, which is why
///   the directory is not a free variable between turns. Documented only in the headless guide's
///   own version note; the changelog entry for that release does not mention it.
///
/// <https://code.claude.com/docs/en/changelog>
pub const MINIMUM_VERSION: &str = "2.1.211";

/// The command that signs a user in.
///
/// Text for a person to run, carried on
/// [`AuthState::LoggedOut`](mango_external_agents::AuthState::LoggedOut) and on
/// [`Error::AuthRequired`](mango_external_agents::Error::AuthRequired). This library never runs it
/// and never handles a login.
pub const LOGIN_COMMAND: &str = "claude auth login";

/// Documented Claude variables a child is allowed to inherit.
///
/// `CLAUDE_CONFIG_DIR` relocates the whole configuration home, so without it a user who moved it
/// would appear signed out. `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` is the vendor's own ceiling on
/// how long a finished turn waits for background subagents; an operator who lowered it means it,
/// and dropping the variable would silently restore the ten-minute default.
///
/// `ANTHROPIC_API_KEY` is deliberately **not** here. The library forwards no credential, and a
/// harness that let one through by name would be doing exactly that under a documented-variable
/// heading. A user who wants API-key mode configures it in `claude` itself.
pub const VENDOR_ENVIRONMENT_KEYS: &[&str] =
    &["CLAUDE_CONFIG_DIR", "CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS"];

/// Exit code the CLI uses for a run stopped by SIGTERM.
///
/// 128 + SIGTERM, and the vendor documents it: "If you stop a `claude -p` run with SIGTERM … Claude
/// Code exits with code 143." A turn that reported an error for that would put a failure in the
/// transcript for something the user asked for.
///
/// The same page is explicit that SIGTERM leaves the vendor's own turn **unfinished** — "Claude
/// Code leaves the turn that was in progress unfinished and records no result for it … When you
/// resume the session, Claude Code continues the turn that SIGTERM left unfinished." The
/// cancellation this harness reports is therefore the host's, not the vendor's; see
/// `docs/harness-claude.md`.
///
/// <https://code.claude.com/docs/en/headless.md>
pub const SIGTERM_EXIT_CODE: i32 = 143;

/// How long a short-lived probe (`--version`, `--help`, `auth status`) is given to answer.
///
/// A harness constant rather than [`Limits::request_timeout`](mango_external_agents::Limits): that
/// one bounds a turn's own request, and a probe that has not printed its first line in fifteen
/// seconds is a probe that is not going to.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Who owns the CLI, and the documents a host's disclosure links.
pub const VENDOR: VendorInfo = VendorInfo {
    company: "Anthropic",
    terms_url: "https://www.anthropic.com/legal/consumer-terms",
    privacy_url: "https://www.anthropic.com/legal/privacy",
    // Claude Code lists every skill under `/` in the same `slash_commands` catalog as its
    // commands, which is why `commands::catalog` never separates the two.
    skills_are_slash_commands: true,
};

/// Where administrator-managed settings live, per platform.
///
/// Read directly, because `disableAutoMode` has to be known *before* a mode is chosen. Inferring it
/// from a failed run cannot work: `--permission-mode auto` being rejected at startup is
/// indistinguishable from any other startup failure, and guessing wrong means passing a mode the
/// user's organisation deliberately turned off.
///
/// `%PROGRAMDATA%` is read from the host's own environment rather than from this process, for the
/// same reason nothing else here reads `std::env`: the host decides what a vendor child sees, and
/// a library that consulted its own environment would be answering a question it was not asked.
/// An unreadable path is caught by the caller and reads as "no policy".
///
/// # Example
///
/// ```
/// use mango_agent_claude::pinned::managed_settings_path;
/// use mango_external_agents::EnvSource;
///
/// let empty = EnvSource::empty();
/// assert_eq!(
///     managed_settings_path("linux", &empty).to_string_lossy(),
///     "/etc/claude-code/managed-settings.json"
/// );
///
/// let windows = EnvSource::from_pairs([("PROGRAMDATA", "D:\\Data")]);
/// assert_eq!(
///     managed_settings_path("windows", &windows).to_string_lossy(),
///     "D:\\Data\\ClaudeCode\\managed-settings.json"
/// );
/// ```
pub fn managed_settings_path(os: &str, environment: &EnvSource) -> PathBuf {
    match os {
        "macos" => PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json"),
        "windows" => {
            let program_data = environment
                .get("PROGRAMDATA")
                .or_else(|| environment.get("ProgramData"))
                .unwrap_or("C:\\ProgramData");
            PathBuf::from(format!("{program_data}\\ClaudeCode\\managed-settings.json"))
        }
        _ => PathBuf::from("/etc/claude-code/managed-settings.json"),
    }
}

#[cfg(test)]
mod tests {
    use super::{MINIMUM_VERSION, VENDOR, VENDOR_ENVIRONMENT_KEYS, managed_settings_path};
    use mango_external_agents::EnvSource;

    #[test]
    fn the_pinned_minimum_is_a_version_this_harness_can_compare() {
        let parsed = crate::version::parse(MINIMUM_VERSION).expect("expected a parseable pin");
        assert_eq!(parsed, crate::version::minimum());
    }

    #[test]
    fn no_credential_variable_is_forwarded_by_name() {
        for forbidden in [
            "ANTHROPIC_API_KEY",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "ANTHROPIC_AUTH_TOKEN",
        ] {
            assert!(
                !VENDOR_ENVIRONMENT_KEYS.contains(&forbidden),
                "expected {forbidden:?} never to be forwarded, received it in the allowlist"
            );
        }
    }

    #[test]
    fn the_vendor_is_named_with_two_documents_a_disclosure_can_link() {
        assert_eq!(VENDOR.company, "Anthropic");
        assert!(VENDOR.terms_url.starts_with("https://"));
        assert!(VENDOR.privacy_url.starts_with("https://"));
    }

    #[test]
    fn reads_the_windows_location_from_the_hosts_environment() {
        let environment = EnvSource::from_pairs([("ProgramData", "E:\\Managed")]);
        assert_eq!(
            managed_settings_path("windows", &environment).to_string_lossy(),
            "E:\\Managed\\ClaudeCode\\managed-settings.json"
        );
    }

    #[test]
    fn falls_back_to_the_conventional_windows_location() {
        assert_eq!(
            managed_settings_path("windows", &EnvSource::empty()).to_string_lossy(),
            "C:\\ProgramData\\ClaudeCode\\managed-settings.json"
        );
    }

    #[test]
    fn each_platform_has_its_own_documented_location() {
        assert_eq!(
            managed_settings_path("macos", &EnvSource::empty()).to_string_lossy(),
            "/Library/Application Support/ClaudeCode/managed-settings.json"
        );
        for unixish in ["linux", "freebsd", "anything-else"] {
            assert_eq!(
                managed_settings_path(unixish, &EnvSource::empty()).to_string_lossy(),
                "/etc/claude-code/managed-settings.json"
            );
        }
    }
}
