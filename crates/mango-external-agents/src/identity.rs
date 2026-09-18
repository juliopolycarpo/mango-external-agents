//! Who a harness is, as three facts that evolve at different rates.
//!
//! A vendor enum would make every new native harness a change to this crate, and the host that
//! wanted one would have to wait for a release. So the three are split:
//!
//! - [`HarnessId`] is the stable registration name a host dispatches on and persists.
//! - [`ProtocolFamily`] is the wire dialect the harness speaks, which several ids may share.
//! - [`ProfileId`] is the execution profile inside a family — which ACP agent, which argv.
//!
//! None of the three is the transport. How bytes reach a harness is
//! [`TransportKind`](crate::transport::TransportKind), chosen per session, and the same dialect
//! rides more than one carrier.
//!
//! Every id is validated on construction and refused deterministically, because these strings are
//! map keys, log lines, directory names in a host's own storage and values a host persists next to
//! a conversation. A value that only *sometimes* round-trips is worse than one that is refused.

use std::fmt;

use crate::error::{Error, Result};

/// The longest an identifier may be.
///
/// Long enough for `acp:` plus a vendor's own agent name, short enough to sit in a log line and in
/// a filename a host derives from it.
pub const IDENTIFIER_MAX_LENGTH: usize = 64;

/// The prefix a generic Agent Client Protocol harness registers its profiles under.
const ACP_PREFIX: &str = "acp:";

/// The longest a profile id may be.
///
/// Shorter than the rest by exactly the `acp:` prefix, so [`HarnessId::acp`] can compose the two
/// halves without either a fallible signature or a length nobody checked. A profile that only
/// fitted until it was prefixed would be a validated value producing an unvalidated one.
pub const PROFILE_ID_MAX_LENGTH: usize = IDENTIFIER_MAX_LENGTH - ACP_PREFIX.len();

/// Whether one character may appear in an identifier.
///
/// ASCII lowercase, digits, and the four separators the existing ids already use. Deliberately
/// narrow: these strings become map keys, log fields and path components in a host's own storage,
/// and a separator the library did not choose is a separator a host's own parser will meet.
const fn is_identifier_char(character: char) -> bool {
    character.is_ascii_lowercase()
        || character.is_ascii_digit()
        || matches!(character, '-' | '_' | '.' | ':')
}

/// Whether one character is a separator rather than a word.
const fn is_separator(character: char) -> bool {
    matches!(character, '-' | '_' | '.' | ':')
}

/// Checks one identifier and says what was wrong with it.
///
/// The rules are the same for every identifier in this module, so a host that learns them once
/// knows them for all three.
fn validate(raw: &str, subject: &'static str, max_length: usize) -> Result<String> {
    let expected = "ASCII lowercase, digits, `-`, `_`, `.` or `:`, within the identifier length, \
                    not beginning or ending with a separator and with no separator repeated";
    let refuse = |received: String| {
        Err(Error::HostConfiguration {
            expected,
            received: format!("{subject} {received}"),
        })
    };

    if raw.is_empty() {
        return refuse(String::from("was empty"));
    }
    let length = raw.chars().count();
    if length > max_length {
        return refuse(format!(
            "was {length} characters, over the {max_length} allowed"
        ));
    }
    if raw.chars().any(|character| !is_identifier_char(character)) {
        return refuse(String::from("contains a disallowed character"));
    }
    let first = raw.chars().next().unwrap_or_default();
    let last = raw.chars().next_back().unwrap_or_default();
    if is_separator(first) || is_separator(last) {
        return refuse(String::from("begins or ends with a separator"));
    }
    if raw
        .as_bytes()
        .windows(2)
        .any(|pair| is_separator(pair[0] as char) && is_separator(pair[1] as char))
    {
        return refuse(String::from("repeats a separator"));
    }
    Ok(raw.to_owned())
}

/// A macro would hide three near-identical newtypes behind a name nobody can grep for, so the
/// three are written out. What they share is [`validate`].
macro_rules! identifier {
    ($name:ident, $subject:literal, $max:expr, $doc:literal) => {
        #[doc = $doc]
        ///
        /// Validated on construction and serialized as the bare string it was built from, so a
        /// value a host persisted is the value it reads back.
        #[derive(
            Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
        )]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl fmt::Debug for $name {
            /// Marks an identifier without logging its host-provided spelling.
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(stringify!($name))
            }
        }

        impl $name {
            #[doc = concat!("Checks and wraps one ", $subject, ".")]
            ///
            /// # Errors
            ///
            /// [`Error::HostConfiguration`] naming the invalid shape and the shape expected,
            /// without copying the caller's value into diagnostics.
            pub fn new(raw: impl AsRef<str>) -> Result<Self> {
                validate(raw.as_ref(), $subject, $max).map(Self)
            }

            /// The identifier as written.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Wraps a value this crate wrote and already knows is valid.
            #[allow(dead_code, reason = "not every identifier has a built-in constant")]
            pub(crate) fn trusted(raw: impl Into<String>) -> Self {
                Self(raw.into())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl TryFrom<String> for $name {
            type Error = Error;

            fn try_from(raw: String) -> Result<Self> {
                Self::new(raw)
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> Self {
                value.0
            }
        }
    };
}

identifier!(
    HarnessId,
    "harness id",
    IDENTIFIER_MAX_LENGTH,
    r#"The name a harness is registered and dispatched under.

The one identifier a host persists next to a conversation, so its spelling is a compatibility
surface: `claude`, `codex`, `acp:cursor`. A host adding a native harness of its own picks a new
one rather than waiting for an arm in this crate.

# Example

```
use mango_external_agents::HarnessId;

assert_eq!(HarnessId::claude().as_str(), "claude");
assert_eq!(HarnessId::acp(&"cursor".parse().expect("a profile")).as_str(), "acp:cursor");
assert!(HarnessId::new("My Harness").is_err());
```"#
);

identifier!(
    ProtocolFamily,
    "protocol family",
    IDENTIFIER_MAX_LENGTH,
    r#"The wire dialect a harness speaks.

Several harness ids may share one: every ACP agent speaks [`ProtocolFamily::acp`] whatever its
profile, which is what lets one implementation serve all of them. A host reads this to know what
kind of surface it is talking to, never to decide which harness to launch — that is
[`HarnessId`].

# Example

```
use mango_external_agents::ProtocolFamily;

assert_eq!(ProtocolFamily::acp().as_str(), "agent-client-protocol");
```"#
);

identifier!(
    ProfileId,
    "profile id",
    PROFILE_ID_MAX_LENGTH,
    r#"Which execution profile inside a protocol family.

A profile is the argv and the quirks of one agent — `cursor`, `opencode`, `gemini`, `goose` — or
`custom`, where the host supplies the argv itself. Open by design: an agent nobody has heard of
yet speaks the same protocol as the ones that ship with a profile.

# Example

```
use mango_external_agents::ProfileId;

let profile: ProfileId = "opencode".parse().expect("a profile");
assert_eq!(profile.as_str(), "opencode");
```"#
);

impl std::str::FromStr for HarnessId {
    type Err = Error;

    fn from_str(raw: &str) -> Result<Self> {
        Self::new(raw)
    }
}

impl std::str::FromStr for ProtocolFamily {
    type Err = Error;

    fn from_str(raw: &str) -> Result<Self> {
        Self::new(raw)
    }
}

impl std::str::FromStr for ProfileId {
    type Err = Error;

    fn from_str(raw: &str) -> Result<Self> {
        Self::new(raw)
    }
}

impl HarnessId {
    /// The id the Claude Code harness registers under.
    pub fn claude() -> Self {
        Self::trusted("claude")
    }

    /// The id the Codex harness registers under.
    pub fn codex() -> Self {
        Self::trusted("codex")
    }

    /// The id one ACP profile registers under, which is `acp:` and the profile.
    ///
    /// Composed here rather than at each call site so every ACP harness spells it the same way,
    /// and infallible because the composition cannot fail: a [`ProfileId`] already passed every
    /// rule, `:` is a separator neither half can begin or end with, and [`PROFILE_ID_MAX_LENGTH`]
    /// is the whole ceiling minus this prefix — so the result is always a value
    /// [`HarnessId::new`] would have accepted.
    pub fn acp(profile: &ProfileId) -> Self {
        Self::trusted(format!("{ACP_PREFIX}{profile}"))
    }

    /// The ACP profile this id names, when it names one.
    ///
    /// The other direction of [`HarnessId::acp`], so the `acp:` prefix is spelled in exactly one
    /// place. A caller that split the string itself would be the second place, and the second
    /// place is where the two stop agreeing.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{HarnessId, ProfileId};
    ///
    /// let profile = ProfileId::new("goose").expect("a profile");
    /// assert_eq!(HarnessId::acp(&profile).acp_profile(), Some(profile));
    /// assert_eq!(HarnessId::claude().acp_profile(), None);
    /// ```
    pub fn acp_profile(&self) -> Option<ProfileId> {
        self.0.strip_prefix(ACP_PREFIX).map(ProfileId::trusted)
    }
}

impl ProtocolFamily {
    /// Claude Code's headless stream-json dialect.
    pub fn claude_code() -> Self {
        Self::trusted("claude-code")
    }

    /// The `codex app-server` JSON-RPC dialect.
    pub fn codex_app_server() -> Self {
        Self::trusted("codex-app-server")
    }

    /// The Agent Client Protocol.
    pub fn acp() -> Self {
        Self::trusted("agent-client-protocol")
    }
}

/// The three identity facts about one harness, together.
///
/// Carried on [`HarnessDescriptor`](crate::HarnessDescriptor) and knowable without touching the
/// machine. Splitting them is what lets a host group every ACP agent under one protocol while
/// still dispatching to each by its own id.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct HarnessIdentity {
    /// What this harness is registered and dispatched under.
    pub id: HarnessId,
    /// The wire dialect it speaks.
    pub protocol: ProtocolFamily,
    /// Which profile inside that dialect, for a family that has more than one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ProfileId>,
}

impl fmt::Debug for HarnessIdentity {
    /// Reports identity shape without logging host-selected registration or profile ids.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HarnessIdentity")
            .field("has_profile", &self.profile.is_some())
            .finish_non_exhaustive()
    }
}

impl HarnessIdentity {
    /// The Claude Code harness.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::HarnessIdentity;
    ///
    /// let identity = HarnessIdentity::claude();
    /// assert_eq!(identity.id.as_str(), "claude");
    /// assert_eq!(identity.profile, None);
    /// ```
    pub fn claude() -> Self {
        Self {
            id: HarnessId::claude(),
            protocol: ProtocolFamily::claude_code(),
            profile: None,
        }
    }

    /// The Codex harness.
    pub fn codex() -> Self {
        Self {
            id: HarnessId::codex(),
            protocol: ProtocolFamily::codex_app_server(),
            profile: None,
        }
    }

    /// One ACP agent, under its profile.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{HarnessIdentity, ProfileId};
    ///
    /// let identity = HarnessIdentity::acp(ProfileId::new("goose").expect("a profile"));
    /// assert_eq!(identity.id.as_str(), "acp:goose");
    /// assert_eq!(identity.protocol.as_str(), "agent-client-protocol");
    /// ```
    pub fn acp(profile: ProfileId) -> Self {
        Self {
            id: HarnessId::acp(&profile),
            protocol: ProtocolFamily::acp(),
            profile: Some(profile),
        }
    }

    /// A harness a host wrote, under identifiers it chose.
    ///
    /// The seam that keeps a new native harness out of this crate's release cycle. Every part is
    /// validated, so a host cannot register something a log line or a map key cannot carry.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when any of the three does not survive validation.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::HarnessIdentity;
    ///
    /// let identity = HarnessIdentity::custom("acme-agent", "acme-rpc", None)
    ///     .expect("expected a valid identity");
    /// assert_eq!(identity.id.as_str(), "acme-agent");
    /// ```
    pub fn custom(
        id: impl AsRef<str>,
        protocol: impl AsRef<str>,
        profile: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            id: HarnessId::new(id)?,
            protocol: ProtocolFamily::new(protocol)?,
            profile: profile.map(ProfileId::new).transpose()?,
        })
    }
}

impl fmt::Display for HarnessIdentity {
    /// The registration id, which is the name a person sees and a host persists.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::{HarnessId, HarnessIdentity, ProfileId, ProtocolFamily};
    use crate::error::Error;

    #[test]
    fn the_built_in_identities_keep_the_spellings_hosts_already_persist() {
        assert_eq!(HarnessIdentity::claude().id.as_str(), "claude");
        assert_eq!(HarnessIdentity::codex().id.as_str(), "codex");
        assert_eq!(
            HarnessIdentity::acp(ProfileId::new("cursor").expect("a profile"))
                .id
                .as_str(),
            "acp:cursor"
        );
    }

    #[test]
    fn a_harness_id_round_trips_as_the_bare_string_it_was_built_from() {
        let id = HarnessId::acp(&ProfileId::new("opencode").expect("a profile"));
        let encoded = serde_json::to_value(&id).expect("expected a serializable id");
        assert_eq!(encoded, serde_json::json!("acp:opencode"));
        assert_eq!(
            serde_json::from_value::<HarnessId>(encoded).expect("expected the id back"),
            id
        );
    }

    /// Deserialization goes through the same rules as construction, or a persisted value nobody
    /// checked would arrive as a map key no host could have registered.
    #[test]
    fn deserializing_refuses_a_value_construction_would_have_refused() {
        let error = serde_json::from_value::<HarnessId>(serde_json::json!("Claude Code"))
            .expect_err("expected a refusal, received an id");
        assert!(
            error.to_string().contains("harness id"),
            "expected the subject in the refusal, received {error}"
        );
    }

    #[test]
    fn an_identity_serializes_its_three_parts_and_omits_an_absent_profile() {
        let encoded = serde_json::to_value(HarnessIdentity::claude())
            .expect("expected a serializable identity");
        assert_eq!(encoded["id"], "claude");
        assert_eq!(encoded["protocol"], "claude-code");
        assert!(encoded.get("profile").is_none(), "received {encoded}");
    }

    #[test]
    fn every_rejected_shape_names_what_was_wrong_with_it() {
        let cases = [
            ("", "was empty"),
            ("Claude", "contains"),
            ("claude code", "contains"),
            ("-claude", "begins or ends with a separator"),
            ("claude-", "begins or ends with a separator"),
            ("acp::cursor", "repeats a separator"),
        ];
        for (raw, expected) in cases {
            let error = HarnessId::new(raw).expect_err("expected a refusal, received an id");
            let Error::HostConfiguration { received, .. } = &error else {
                panic!("expected a host configuration refusal, received {error:?}");
            };
            assert!(
                received.contains(expected),
                "expected {expected:?} for {raw:?}, received {received}"
            );
        }
    }

    #[test]
    fn an_identifier_longer_than_the_ceiling_reports_its_own_length() {
        let error = HarnessId::new("i".repeat(65)).expect_err("expected a refusal");
        assert!(
            error.to_string().contains("was 65 characters"),
            "expected the received length in the refusal, received {error}"
        );
    }

    /// `HarnessId::acp` is infallible, so the only way it could produce a value construction
    /// would have refused is a profile that fitted until it was prefixed. The profile ceiling is
    /// the whole ceiling minus `acp:` precisely so that cannot happen — and this is the boundary.
    #[test]
    fn the_longest_allowed_profile_still_composes_into_an_id_construction_would_accept() {
        use super::{IDENTIFIER_MAX_LENGTH, PROFILE_ID_MAX_LENGTH};

        let longest = ProfileId::new("p".repeat(PROFILE_ID_MAX_LENGTH))
            .expect("expected the longest profile to be accepted");
        let composed = HarnessId::acp(&longest);

        assert_eq!(composed.as_str().chars().count(), IDENTIFIER_MAX_LENGTH);
        assert_eq!(
            HarnessId::new(composed.as_str()).expect("expected the composed id to re-validate"),
            composed
        );
        assert!(
            ProfileId::new("p".repeat(PROFILE_ID_MAX_LENGTH + 1)).is_err(),
            "expected a profile one character too long to be refused before it could be prefixed"
        );
    }

    #[test]
    fn a_custom_identity_needs_no_arm_in_this_crate() {
        let identity = HarnessIdentity::custom("acme-agent", "acme-rpc", Some("fast"))
            .expect("expected a valid identity");
        assert_eq!(identity.id.as_str(), "acme-agent");
        assert_eq!(identity.protocol.as_str(), "acme-rpc");
        assert_eq!(
            identity.profile.as_ref().map(ProfileId::as_str),
            Some("fast")
        );
    }

    #[test]
    fn a_custom_identity_refuses_an_invalid_part_rather_than_repairing_it() {
        assert!(HarnessIdentity::custom("ACME", "acme-rpc", None).is_err());
        assert!(HarnessIdentity::custom("acme", "acme rpc", None).is_err());
        assert!(HarnessIdentity::custom("acme", "acme-rpc", Some("")).is_err());
    }

    /// The prefix is spelled once, and the round trip is what pins that. A caller splitting the
    /// string itself would be the second place it is spelled, and the second place is where the
    /// two stop agreeing.
    #[test]
    fn an_acp_id_round_trips_back_to_the_profile_it_was_built_from() {
        for name in ["cursor", "opencode", "claude-agent-acp"] {
            let profile = ProfileId::new(name).expect("expected a valid profile");
            assert_eq!(HarnessId::acp(&profile).acp_profile(), Some(profile));
        }
        assert_eq!(HarnessId::claude().acp_profile(), None);
        assert_eq!(HarnessId::codex().acp_profile(), None);
        assert_eq!(
            HarnessId::new("acpx").expect("a valid id").acp_profile(),
            None,
            "expected only the prefix with its separator to count as an ACP id"
        );
    }

    #[test]
    fn the_known_protocol_families_are_spelled_once() {
        assert_eq!(ProtocolFamily::claude_code().as_str(), "claude-code");
        assert_eq!(
            ProtocolFamily::codex_app_server().as_str(),
            "codex-app-server"
        );
        assert_eq!(ProtocolFamily::acp().as_str(), "agent-client-protocol");
    }
}
