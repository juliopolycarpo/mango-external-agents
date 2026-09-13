//! What a probe can learn about the installed `codex`, without reading a credential.
//!
//! Two questions, answered from two surfaces. `codex --version` says which build is here, which is
//! the gate. The app-server's `account/read` says whether somebody is signed in and, for a ChatGPT
//! sign-in, which plan — and that is all it says. The library never opens `~/.codex/auth.json`,
//! never asks for a token refresh, and has no login method anywhere.

use mango_external_agents::discovery::{AuthMode, AuthState, GateVerdict};

use crate::protocol::requests::Account;

/// The vendor's own login command, as text for a host to show. Never run by this library.
pub const LOGIN_HINT: &str = "codex login";

/// The program name, when the host did not resolve a path.
pub const PROGRAM: &str = "codex";

/// The version this build reports, from whatever `codex --version` printed.
///
/// `codex-cli 0.153.4` today. Only the last whitespace-separated token is read, so a build that
/// grows a prefix still parses; a line with nothing version-shaped in it is `None`, which the
/// caller turns into [`GateVerdict::Unknown`] rather than a refusal.
///
/// # Example
///
/// ```
/// use mango_agent_codex::discovery::parse_version;
///
/// assert_eq!(parse_version("codex-cli 0.153.4\n").as_deref(), Some("0.153.4"));
/// assert_eq!(parse_version("what?"), None);
/// ```
#[must_use]
pub fn parse_version(output: &str) -> Option<String> {
    let token = output.split_whitespace().next_back()?;
    let looks_like_a_version = token
        .split('-')
        .next()
        .is_some_and(|core| core.contains('.') && core.starts_with(|c: char| c.is_ascii_digit()));
    looks_like_a_version.then(|| token.to_owned())
}

/// The Codex version out of the user agent the handshake answered with.
///
/// `mangostudio/0.153.4 (Ubuntu 26.4.0; x86_64) …` — the client's own name, then the build that is
/// running. Reading it here is what lets a session gate its own connection without spawning a
/// second `codex --version`: the process is already open, and it has just said which build it is.
///
/// # Example
///
/// ```
/// use mango_agent_codex::discovery::parse_user_agent_version;
///
/// let agent = "mangostudio/0.153.4 (Ubuntu 26.4.0; x86_64) WindowsTerminal";
/// assert_eq!(parse_user_agent_version(agent).as_deref(), Some("0.153.4"));
/// assert_eq!(parse_user_agent_version("something else entirely"), None);
/// ```
#[must_use]
pub fn parse_user_agent_version(user_agent: &str) -> Option<String> {
    let product = user_agent.split_whitespace().next()?;
    // The client's own name may contain a slash of its own, so the version is what follows the
    // last one rather than the first.
    let (_, version) = product.rsplit_once('/')?;
    parse_version(version)
}

/// Whether `found` is at least `minimum`.
///
/// Compared as dotted numbers, with any pre-release suffix dropped: `0.155.0-alpha.3` is treated
/// as `0.155.0`, which is the reading that matters here — an alpha of a newer minor carries the
/// newer protocol. A component that is not a number makes the whole comparison unanswerable, which
/// the caller reports as [`GateVerdict::Unknown`].
#[must_use]
pub fn meets_minimum(found: &str, minimum: &str) -> Option<bool> {
    let parts = |version: &str| -> Option<Vec<u64>> {
        version
            .split('-')
            .next()?
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect()
    };
    let (found, minimum) = (parts(found)?, parts(minimum)?);
    let width = found.len().max(minimum.len());
    let at = |parts: &[u64], index: usize| parts.get(index).copied().unwrap_or(0);
    for index in 0..width {
        match at(&found, index).cmp(&at(&minimum, index)) {
            std::cmp::Ordering::Less => return Some(false),
            std::cmp::Ordering::Greater => return Some(true),
            std::cmp::Ordering::Equal => {}
        }
    }
    Some(true)
}

/// The verdict for a build that reported `version`.
///
/// A version nobody could parse is [`GateVerdict::Unknown`] rather than a refusal: a CLI that
/// changed the shape of `--version` is not a CLI that stopped working, and the host may still
/// choose to try.
#[must_use]
pub fn gate(version: Option<&str>, minimum: &str) -> GateVerdict {
    let Some(version) = version else {
        return GateVerdict::Unknown;
    };
    match meets_minimum(version, minimum) {
        Some(true) => GateVerdict::Usable,
        Some(false) => GateVerdict::VersionTooOld {
            found: version.to_owned(),
            minimum: minimum.to_owned(),
        },
        None => GateVerdict::Unknown,
    }
}

/// Who is signed in, from what `account/read` answered.
///
/// Three outcomes, and the third is the point of the whole module. A build pointed at a provider
/// of its own answers `requiresOpenaiAuth: false`, and being signed out of OpenAI says nothing
/// about whether a turn would run — so the answer is [`AuthState::Unknown`] and the host is not
/// told to run a login command that would not help.
#[must_use]
pub fn auth_state(account: Option<&Account>, requires_openai_auth: bool) -> AuthState {
    match account {
        Some(Account::Chatgpt { plan_type }) => AuthState::LoggedIn {
            mode: plan_type.as_ref().map_or(AuthMode::Subscription, |_| {
                // The plan name is a label the vendor wrote, and `Subscription` is the fact. The
                // label goes nowhere: a plan is not how an account is signed in.
                AuthMode::Subscription
            }),
        },
        Some(Account::ApiKey) => AuthState::LoggedIn {
            mode: AuthMode::ApiKey,
        },
        Some(Account::AmazonBedrock) => AuthState::LoggedIn {
            mode: AuthMode::Other(String::from("amazon-bedrock")),
        },
        None if requires_openai_auth => AuthState::LoggedOut {
            login_hint: String::from(LOGIN_HINT),
        },
        None => AuthState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::{LOGIN_HINT, auth_state, gate, meets_minimum, parse_version};
    use crate::protocol::requests::Account;
    use mango_external_agents::discovery::{AuthMode, AuthState, GateVerdict};

    #[test]
    fn the_version_comes_off_the_line_the_cli_actually_prints() {
        assert_eq!(
            parse_version("codex-cli 0.153.4\n").as_deref(),
            Some("0.153.4")
        );
        assert_eq!(
            parse_version("codex-cli 0.155.0-alpha.3\n").as_deref(),
            Some("0.155.0-alpha.3")
        );
    }

    /// A CLI that changed the shape of `--version` has not stopped working. Reporting nothing lets
    /// the caller say `Unknown` rather than refuse a build that would run.
    #[test]
    fn a_line_with_nothing_version_shaped_in_it_reports_nothing_rather_than_guessing() {
        for output in ["", "codex", "error: not found", "codex-cli unknown"] {
            assert_eq!(
                parse_version(output),
                None,
                "expected {output:?} to parse as no version"
            );
        }
    }

    /// The captured handshake answer, so a session gates its own connection without spawning a
    /// second process to ask a question the open one has already answered.
    #[test]
    fn the_handshake_answer_says_which_build_is_running() {
        assert_eq!(
            super::parse_user_agent_version(
                "mango-smoke/0.153.4 (Ubuntu 26.4.0; x86_64) WindowsTerminal (mango-smoke; 0.0.1)"
            )
            .as_deref(),
            Some("0.153.4")
        );
    }

    /// A host may call itself `acme/studio`, and the version is what follows the last slash.
    #[test]
    fn a_client_name_with_a_slash_in_it_does_not_become_the_version() {
        assert_eq!(
            super::parse_user_agent_version("acme/studio/0.153.4 (Linux)").as_deref(),
            Some("0.153.4")
        );
    }

    #[test]
    fn a_user_agent_with_nothing_version_shaped_in_it_reports_nothing() {
        for agent in ["", "codex", "codex/unknown (Linux)", "no-slash-here 1.2.3"] {
            assert_eq!(
                super::parse_user_agent_version(agent),
                None,
                "expected {agent:?} to parse as no version"
            );
        }
    }

    #[test]
    fn versions_compare_as_numbers_rather_than_as_text() {
        // The text comparison this exists to avoid: "0.9.0" > "0.153.4" as strings.
        assert_eq!(meets_minimum("0.9.0", "0.153.4"), Some(false));
        assert_eq!(meets_minimum("0.153.4", "0.153.4"), Some(true));
        assert_eq!(meets_minimum("0.153.10", "0.153.4"), Some(true));
        assert_eq!(meets_minimum("0.154.0", "0.153.4"), Some(true));
        assert_eq!(meets_minimum("1.0.0", "0.153.4"), Some(true));
    }

    /// An alpha of a newer minor carries the newer protocol, which is what the gate is about.
    #[test]
    fn a_prerelease_is_read_as_the_version_it_is_a_prerelease_of() {
        assert_eq!(meets_minimum("0.155.0-alpha.3", "0.153.4"), Some(true));
        assert_eq!(meets_minimum("0.152.0-alpha.1", "0.153.4"), Some(false));
    }

    #[test]
    fn a_shorter_version_is_padded_rather_than_refused() {
        assert_eq!(meets_minimum("1.0", "0.153.4"), Some(true));
        assert_eq!(meets_minimum("0.153.4", "0.153"), Some(true));
    }

    #[test]
    fn a_version_component_that_is_not_a_number_is_unanswerable() {
        assert_eq!(meets_minimum("0.x.4", "0.153.4"), None);
    }

    #[test]
    fn an_old_build_is_refused_with_both_versions_so_a_host_can_say_which() {
        let verdict = gate(Some("0.147.0"), "0.153.4");
        assert_eq!(
            verdict,
            GateVerdict::VersionTooOld {
                found: String::from("0.147.0"),
                minimum: String::from("0.153.4")
            }
        );
        assert_eq!(gate(Some("0.153.4"), "0.153.4"), GateVerdict::Usable);
        assert_eq!(gate(None, "0.153.4"), GateVerdict::Unknown);
        assert_eq!(gate(Some("nonsense"), "0.153.4"), GateVerdict::Unknown);
    }

    #[test]
    fn a_signed_in_account_reports_how_without_reporting_who() {
        assert_eq!(
            auth_state(
                Some(&Account::Chatgpt {
                    plan_type: Some(String::from("plus"))
                }),
                true
            ),
            AuthState::LoggedIn {
                mode: AuthMode::Subscription
            }
        );
        assert_eq!(
            auth_state(Some(&Account::ApiKey), true),
            AuthState::LoggedIn {
                mode: AuthMode::ApiKey
            }
        );
        assert_eq!(
            auth_state(Some(&Account::AmazonBedrock), false),
            AuthState::LoggedIn {
                mode: AuthMode::Other(String::from("amazon-bedrock"))
            }
        );
    }

    #[test]
    fn a_signed_out_build_carries_the_vendors_own_command_as_text() {
        assert_eq!(
            auth_state(None, true),
            AuthState::LoggedOut {
                login_hint: String::from(LOGIN_HINT)
            }
        );
    }

    /// A build pointed at its own provider is not signed out of anything that matters. Telling the
    /// user to run `codex login` would send them to fix something that is not broken.
    #[test]
    fn a_build_that_needs_no_openai_sign_in_is_unknown_rather_than_signed_out() {
        assert_eq!(auth_state(None, false), AuthState::Unknown);
    }
}
