//! Who Claude thinks is signed in — reduced to the least that is useful.
//!
//! `claude auth status` is documented, structured and non-secret: "Show authentication status as
//! JSON. Use `--text` for human-readable output. Exits with code 0 if logged in, 1 if not."
//! (<https://code.claude.com/docs/en/cli-reference.md>). Reading it is the whole of this harness's
//! auth handling — there is no login, no browser, no token file.
//!
//! It also returns **more personal data than any other vendor's status call**:
//!
//! ```json
//! { "loggedIn": true, "authMethod": "claude.ai", "apiProvider": "firstParty",
//!   "email": "…", "orgId": "…", "orgName": "…", "subscriptionType": "pro" }
//! ```
//!
//! Four of those fields never leave this function. `email`, `orgId`, `orgName` and
//! `projectsDirectory` are not needed to decide a permission mode or to tell a host somebody is
//! signed in, and a diagnostic carrying an organisation name would put a customer's identity in a
//! log that outlives the session. What crosses is [`AuthState`] and nothing else.
//!
//! `subscriptionType` is dropped too, for a different reason: [`AuthMode`] says *how* an account
//! authenticates, and putting a plan tier in it would answer a question nobody asked with a value
//! that changes on the vendor's schedule. The one place a plan tier belongs is
//! [`AccountLimits::plan_type`](mango_external_agents::AccountLimits), and Claude Code exposes no
//! quota surface to carry it.

use mango_external_agents::{AuthMode, AuthState};
use serde_json::Value;

use crate::pinned::LOGIN_COMMAND;

/// How the account is paid for, coarsely.
///
/// Three buckets, because three is what a caller needs to distinguish: `auto` needs a qualifying
/// plan tier, so [`permissions`](crate::permissions) has to know whether one is in play. A finer
/// split would start encoding plan tiers, which change on the vendor's schedule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AccountKind {
    /// An Anthropic subscription, signed in through the vendor's own OAuth.
    Subscription,
    /// An API key the user configured with the vendor's own CLI.
    ApiKey,
    /// A hyperscaler's credentials: Bedrock, Vertex, Foundry.
    CloudProvider,
}

/// What one `claude auth status` invocation established.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authentication {
    /// What a host is told.
    pub state: AuthState,
    /// How the account is paid for. Absent unless the call established a signed-in account.
    pub kind: Option<AccountKind>,
}

impl Authentication {
    /// What an invocation that produced nothing usable reads as.
    pub fn unknown() -> Self {
        Self::default()
    }
}

impl Default for Authentication {
    /// Unknown, which is the only honest answer before `auth status` has answered.
    fn default() -> Self {
        Self {
            state: AuthState::Unknown,
            kind: None,
        }
    }
}

/// Parses the JSON `claude auth status` writes to stdout.
///
/// Tolerant of surrounding lines: the command prints one JSON object today, but a build that
/// prefixed a warning would otherwise turn a signed-in account into "unknown", which reads to the
/// user as a broken install. Anything that is not JSON at all still lands on
/// [`AuthState::Unknown`], which is the honest answer.
///
/// A payload whose `loggedIn` is not a boolean is unknown rather than signed-out: Claude may keep
/// credentials in the system keychain, and an unreadable status call is the same class of
/// ignorance as an unreadable credential store. Only an explicit `loggedIn: false` is a signed-out
/// verdict.
///
/// # Example
///
/// ```
/// use mango_agent_claude::auth::{AccountKind, parse_status};
/// use mango_external_agents::{AuthMode, AuthState};
///
/// let signed_in = parse_status(r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}"#);
/// assert_eq!(signed_in.state, AuthState::LoggedIn { mode: AuthMode::Subscription });
/// assert_eq!(signed_in.kind, Some(AccountKind::Subscription));
///
/// assert_eq!(parse_status("not json").state, AuthState::Unknown);
/// ```
pub fn parse_status(stdout: &str) -> Authentication {
    let Some(payload) = embedded_object(stdout) else {
        return Authentication::unknown();
    };
    match payload.get("loggedIn") {
        Some(Value::Bool(false)) => Authentication {
            state: AuthState::LoggedOut {
                login_hint: String::from(LOGIN_COMMAND),
            },
            kind: None,
        },
        Some(Value::Bool(true)) => {
            let kind = account_kind(&payload);
            Authentication {
                state: AuthState::LoggedIn {
                    mode: mode_of(kind),
                },
                kind: Some(kind),
            }
        }
        _ => Authentication::unknown(),
    }
}

/// The first `{ … }` in the output, parsed.
///
/// Read as *one* value from the first `{` onwards rather than as everything up to the last `}`.
/// The tolerance this function exists for cuts both ways: a build that printed a line after the
/// payload — a deprecation notice, a shell wrapper's own epilogue — puts a stray `}` past the end
/// of the object, and a slice that ran to it would hand `serde_json` trailing garbage and turn a
/// signed-in account into "unknown", which is the failure this was written to avoid.
fn embedded_object(stdout: &str) -> Option<serde_json::Map<String, Value>> {
    let start = stdout.find('{')?;
    let mut values = serde_json::Deserializer::from_str(&stdout[start..]).into_iter::<Value>();
    match values.next()? {
        Ok(Value::Object(fields)) => Some(fields),
        _ => None,
    }
}

/// `apiProvider` mapped onto the coarse kind.
///
/// `firstParty` is Anthropic billing the account directly, which covers both a subscription and a
/// raw API key — `authMethod` is what separates them. Anything naming a hyperscaler is a cloud
/// provider's credentials rather than an Anthropic account, and the account-sharing clause in
/// Anthropic's consumer terms reads differently there because the seat being shared is not
/// Anthropic's to police.
fn account_kind(payload: &serde_json::Map<String, Value>) -> AccountKind {
    let provider = lowercased(payload, "apiProvider");
    if ["bedrock", "vertex", "foundry"]
        .into_iter()
        .any(|hyperscaler| provider.contains(hyperscaler))
    {
        return AccountKind::CloudProvider;
    }
    // `claude.ai` is the OAuth sign-in behind Pro, Max and Team. An API key authenticates the same
    // first-party service without a seat behind it.
    let method = lowercased(payload, "authMethod");
    if method.contains("claude.ai") || method.contains("oauth") {
        AccountKind::Subscription
    } else {
        AccountKind::ApiKey
    }
}

fn lowercased(payload: &serde_json::Map<String, Value>, key: &str) -> String {
    crate::protocol::text(payload, key)
        .unwrap_or_default()
        .to_lowercase()
}

fn mode_of(kind: AccountKind) -> AuthMode {
    match kind {
        AccountKind::Subscription => AuthMode::Subscription,
        AccountKind::ApiKey => AuthMode::ApiKey,
        // No core member names a hyperscaler, and inventing one for a single vendor's three
        // provider names would put a Claude fact in a shared vocabulary.
        AccountKind::CloudProvider => AuthMode::Other(String::from("cloud-provider")),
    }
}

#[cfg(test)]
mod tests {
    use super::{AccountKind, parse_status};
    use crate::pinned::LOGIN_COMMAND;
    use mango_external_agents::{AuthMode, AuthState};

    /// Every key the committed contract capture declares, as the vendor spells them.
    const CONTRACT: &str =
        include_str!("../../../fixtures/claude/historical/contract/auth-status.json");

    /// The captured contract still carries the three fields this parser reads.
    ///
    /// The capture is a shape document rather than a sample, so nothing replays it — which leaves
    /// it able to disagree with the code beside it in silence. Reading it here is what turns a
    /// vendor that renamed `loggedIn` into a failing test instead of an account that quietly reads
    /// as unknown.
    #[test]
    fn the_captured_contract_still_names_every_field_this_parser_reads() {
        let contract: serde_json::Value =
            serde_json::from_str(CONTRACT).expect("expected the captured contract to be JSON");
        for field in ["loggedIn", "apiProvider", "authMethod"] {
            assert!(
                contract.get(field).is_some(),
                "expected the captured `auth status` to carry {field:?}, received {contract}"
            );
        }
    }

    /// The shape of the committed contract capture, with the placeholder values filled in.
    const SIGNED_IN: &str = r#"{
        "analyticsDisabled": false,
        "apiProvider": "firstParty",
        "authMethod": "claude.ai",
        "email": "ada@example.com",
        "loggedIn": true,
        "orgId": "org_01",
        "orgName": "Example Ltd",
        "projectsDirectory": "/home/ada/.claude/projects",
        "subscriptionType": "max"
    }"#;

    #[test]
    fn reports_a_subscription_without_carrying_who_it_belongs_to() {
        let authentication = parse_status(SIGNED_IN);
        assert_eq!(
            authentication.state,
            AuthState::LoggedIn {
                mode: AuthMode::Subscription
            }
        );
        assert_eq!(authentication.kind, Some(AccountKind::Subscription));

        let rendered = format!("{authentication:?}");
        for personal in [
            "ada@example.com",
            "org_01",
            "Example Ltd",
            "/home/ada",
            "max",
        ] {
            assert!(
                !rendered.contains(personal),
                "expected {personal:?} never to leave the parser, received {rendered}"
            );
        }
    }

    #[test]
    fn separates_an_api_key_from_the_subscription_on_the_same_first_party_provider() {
        let authentication =
            parse_status(r#"{"loggedIn":true,"apiProvider":"firstParty","authMethod":"apiKey"}"#);
        assert_eq!(
            authentication.state,
            AuthState::LoggedIn {
                mode: AuthMode::ApiKey
            }
        );
        assert_eq!(authentication.kind, Some(AccountKind::ApiKey));
    }

    #[test]
    fn names_a_hyperscalers_credentials_as_neither() {
        for provider in ["bedrock", "vertex", "Foundry"] {
            let line = format!(
                r#"{{"loggedIn":true,"apiProvider":"{provider}","authMethod":"claude.ai"}}"#
            );
            let authentication = parse_status(&line);
            assert_eq!(
                authentication.kind,
                Some(AccountKind::CloudProvider),
                "expected {provider} to read as a cloud provider"
            );
            assert_eq!(
                authentication.state,
                AuthState::LoggedIn {
                    mode: AuthMode::Other(String::from("cloud-provider"))
                }
            );
        }
    }

    #[test]
    fn carries_the_vendors_own_login_command_when_nobody_is_signed_in() {
        let authentication = parse_status(r#"{"loggedIn":false}"#);
        assert_eq!(
            authentication.state,
            AuthState::LoggedOut {
                login_hint: String::from(LOGIN_COMMAND)
            }
        );
        assert_eq!(authentication.kind, None);
    }

    #[test]
    fn an_unreadable_answer_is_unknown_rather_than_signed_out() {
        for unreadable in [
            "",
            "not json",
            "{",
            r#"{"loggedIn":"yes"}"#,
            r#"{"loggedIn":null}"#,
            "[]",
        ] {
            assert_eq!(
                parse_status(unreadable).state,
                AuthState::Unknown,
                "expected {unreadable:?} to read as unknown"
            );
        }
    }

    #[test]
    fn tolerates_a_build_that_printed_a_warning_before_the_object() {
        let authentication = parse_status(
            "warning: a new version is available\n{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}\n",
        );
        assert_eq!(authentication.kind, Some(AccountKind::Subscription));
    }

    /// The other half of the same tolerance, and the half a last-brace scan gets wrong.
    ///
    /// A line printed *after* the payload puts a stray `}` past the object's own end, and reading
    /// to the last one hands `serde_json` trailing text — which reports a signed-in account as
    /// unknown, refuses every configuration that needs an account, and reads to the user as a
    /// broken install.
    #[test]
    fn tolerates_a_build_that_printed_a_brace_after_the_object() {
        for epilogue in [
            "\nnote: settings merged from {project} and {user}",
            "\n}",
            "\ndone {}",
        ] {
            let stdout = format!("{{\"loggedIn\":true,\"authMethod\":\"claude.ai\"}}{epilogue}");
            assert_eq!(
                parse_status(&stdout).kind,
                Some(AccountKind::Subscription),
                "expected {epilogue:?} after the payload to change nothing"
            );
        }
    }
}
