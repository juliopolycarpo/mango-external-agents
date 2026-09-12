//! What a probe found on this machine.
//!
//! A harness never caches a [`Discovery`]: probing costs a process launch and sometimes a
//! handshake, and how fresh an answer has to be is the host's decision, not the library's. A host
//! that wants a two-tier cache builds one; a host that wants to probe every time may.
//!
//! Nothing here reads a credential. [`AuthState`] is filled only from a vendor surface that says
//! whether somebody is signed in without exposing what they signed in with; when the only way to
//! know would be to read a token file, the answer is [`AuthState::Unknown`] and the host tells the
//! user to run the vendor's own login command.

use std::path::PathBuf;

use crate::harness::Capabilities;
use crate::normalize::{self, MODEL_CATALOG_MAX_ITEMS, REASONING_EFFORT_MAX_ITEMS, TextLimit};

/// How the installed CLI was found, and what it can do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovery {
    /// Where the executable is, when the probe found one.
    pub executable: Option<PathBuf>,
    /// The version it reported, bounded.
    pub version: Option<String>,
    /// Whether this build can be driven at all.
    pub gate: GateVerdict,
    /// Whether somebody is signed in, as far as a non-secret surface can say.
    pub auth: AuthState,
    /// What this build actually supports, which may be narrower than the harness's ceiling.
    pub capabilities: Capabilities,
    /// The models the vendor advertises, when it enumerates them.
    pub models: Vec<Model>,
}

impl Discovery {
    /// Nothing found: not installed, nothing known, nothing supported.
    pub fn not_installed() -> Self {
        Self {
            executable: None,
            version: None,
            gate: GateVerdict::NotInstalled,
            auth: AuthState::Unknown,
            capabilities: Capabilities::none(),
            models: Vec::new(),
        }
    }

    /// Whether a turn could be started against this build right now.
    pub fn is_usable(&self) -> bool {
        matches!(self.gate, GateVerdict::Usable)
            && !matches!(self.auth, AuthState::LoggedOut { .. })
    }

    /// This discovery with every vendor-supplied value bounded.
    ///
    /// [`Harness::discover`](crate::Harness::discover) applies this, so a host never sees a probe's
    /// raw output. Ids are refused rather than cut, on the same terms as everywhere else: a
    /// shortened id names a different model, and the model it names would be echoed straight back
    /// to the vendor as the one that was chosen.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            version: self
                .version
                .map(|version| normalize::bound_text(&version, TextLimit::AccountLabel).text),
            auth: self.auth.normalized(),
            models: self
                .models
                .into_iter()
                .filter_map(Model::normalized)
                .take(MODEL_CATALOG_MAX_ITEMS)
                .collect(),
            ..self
        }
    }
}

/// Whether the installed build can be driven.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GateVerdict {
    /// It can.
    Usable,
    /// No executable was found.
    NotInstalled,
    /// It is older than the floor this harness drives.
    VersionTooOld {
        /// What the CLI reported.
        found: String,
        /// The oldest version this harness drives.
        minimum: String,
    },
    /// The probe could not tell — it timed out, or printed something unrecognisable.
    ///
    /// Deliberately not a refusal: a CLI that changed the shape of `--version` is not a CLI that
    /// stopped working, and a host may still choose to try.
    Unknown,
}

/// Whether somebody is signed in.
///
/// Reported, never established. The library has no login method anywhere, opens no browser and
/// reads no token: this is what a non-secret vendor surface said, or `Unknown`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthState {
    /// Somebody is signed in.
    LoggedIn {
        /// How, as far as the vendor's own surface reported it.
        mode: AuthMode,
    },
    /// Nobody is signed in, and this is the command that fixes it.
    LoggedOut {
        /// The vendor's own login command, verbatim, for the host to display.
        ///
        /// Text for a person to run. The library never runs it.
        login_hint: String,
    },
    /// The probe could not tell without reading a credential, so it did not look.
    Unknown,
}

impl AuthState {
    /// This state with every vendor-supplied label bounded.
    ///
    /// The login hint is text a host displays to a person, so it is bounded like any other label
    /// the vendor wrote.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self {
            Self::LoggedIn { mode } => Self::LoggedIn {
                mode: mode.normalized(),
            },
            Self::LoggedOut { login_hint } => Self::LoggedOut {
                login_hint: normalize::bound_text(&login_hint, TextLimit::Title).text,
            },
            other => other,
        }
    }
}

/// How an account is signed in.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthMode {
    /// A consumer subscription.
    Subscription,
    /// An API key the user configured with the vendor's own CLI.
    ApiKey,
    /// Something else the vendor named.
    Other(String),
}

impl AuthMode {
    /// This mode with a vendor-supplied label bounded.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self {
            Self::Other(label) => {
                Self::Other(normalize::bound_text(&label, TextLimit::AccountLabel).text)
            }
            other => other,
        }
    }
}

/// A model as the vendor advertised it.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// The vendor's own id, sent back verbatim when this model is chosen.
    pub id: String,
    /// A name for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What the vendor says it is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the vendor picks this one when nobody chooses.
    #[serde(default)]
    pub is_default: bool,
    /// The reasoning choices this model offers, kept as the vendor's own values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_efforts: Vec<ReasoningEffort>,
    /// Which of them applies when nobody chooses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_reasoning_effort: Option<String>,
}

impl Model {
    /// A model with nothing but its id.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ..Self::default()
        }
    }

    /// This model with every label bounded, or nothing when its id cannot survive bounding.
    ///
    /// A model whose id is dropped is a model a host cannot choose, which is the safe outcome: an
    /// id that was cut short, or that carried a bidi override into a picker, would be sent back to
    /// the vendor as a choice nobody made.
    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        let default_reasoning_effort = self
            .default_reasoning_effort
            .and_then(|id| normalize::opaque_id(&id, "reasoning effort id").ok());
        Some(Self {
            id: normalize::opaque_id(&self.id, "model id").ok()?,
            display_name: self
                .display_name
                .map(|name| normalize::bound_text(&name, TextLimit::Title).text),
            description: self
                .description
                .map(|text| normalize::bound_text(&text, TextLimit::Detail).text),
            reasoning_efforts: self
                .reasoning_efforts
                .into_iter()
                .filter_map(ReasoningEffort::normalized)
                .take(REASONING_EFFORT_MAX_ITEMS)
                .collect(),
            default_reasoning_effort,
            ..self
        })
    }
}

/// One vendor-defined reasoning choice, kept without flattening it to an enum of ours.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffort {
    /// The vendor's own id.
    pub id: String,
    /// A name for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What the vendor says it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ReasoningEffort {
    /// This choice with every label bounded, or nothing when its id cannot survive bounding.
    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        Some(Self {
            id: normalize::opaque_id(&self.id, "reasoning effort id").ok()?,
            display_name: self
                .display_name
                .map(|name| normalize::bound_text(&name, TextLimit::Title).text),
            description: self
                .description
                .map(|text| normalize::bound_text(&text, TextLimit::Detail).text),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AuthMode, AuthState, Discovery, GateVerdict, Model, ReasoningEffort};
    use crate::harness::Capabilities;

    fn usable() -> Discovery {
        Discovery {
            executable: Some("/usr/local/bin/claude".into()),
            version: Some(String::from("2.1.0")),
            gate: GateVerdict::Usable,
            auth: AuthState::LoggedIn {
                mode: AuthMode::Subscription,
            },
            capabilities: Capabilities::none(),
            models: Vec::new(),
        }
    }

    #[test]
    fn nothing_installed_reports_nothing_and_claims_nothing() {
        let discovery = Discovery::not_installed();
        assert_eq!(discovery.gate, GateVerdict::NotInstalled);
        assert_eq!(discovery.auth, AuthState::Unknown);
        assert_eq!(discovery.capabilities, Capabilities::none());
        assert!(!discovery.is_usable());
    }

    #[test]
    fn a_signed_out_build_is_not_usable_even_when_the_gate_passes() {
        let signed_out = Discovery {
            auth: AuthState::LoggedOut {
                login_hint: String::from("claude setup-token"),
            },
            ..usable()
        };
        assert!(!signed_out.is_usable());
        assert!(usable().is_usable());
    }

    #[test]
    fn an_unknown_auth_state_does_not_block_a_host_that_wants_to_try() {
        // The probe declining to read a credential is not evidence of being signed out.
        let unknown = Discovery {
            auth: AuthState::Unknown,
            ..usable()
        };
        assert!(unknown.is_usable());
    }

    #[test]
    fn an_old_build_carries_both_versions_so_a_host_can_say_which() {
        let gate = GateVerdict::VersionTooOld {
            found: String::from("1.0.0"),
            minimum: String::from("2.0.0"),
        };
        let old = Discovery { gate, ..usable() };
        assert!(!old.is_usable());
    }

    /// The attack `normalize::is_strippable` exists to stop, arriving through the one vendor
    /// surface that was not normalised: a model id lands in a host's picker and is echoed straight
    /// back to the vendor as the choice.
    #[test]
    fn a_model_whose_id_cannot_be_bounded_is_dropped_rather_than_offered() {
        let discovery = Discovery {
            models: vec![
                Model::new("gpt\u{202e}5-mini"),
                Model::new("i".repeat(200)),
                Model::new("opus"),
            ],
            ..usable()
        }
        .normalized();

        assert_eq!(
            discovery
                .models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["opus"],
            "expected only the id that survived bounding"
        );
    }

    #[test]
    fn a_catalogue_the_vendor_never_stops_enumerating_is_capped() {
        let discovery = Discovery {
            models: (0..5_000).map(|n| Model::new(format!("m-{n}"))).collect(),
            ..usable()
        }
        .normalized();

        assert_eq!(discovery.models.len(), 256);
    }

    #[test]
    fn a_login_hint_is_bounded_like_any_other_label() {
        let discovery = Discovery {
            auth: AuthState::LoggedOut {
                login_hint: "h".repeat(4_000),
            },
            ..usable()
        }
        .normalized();

        let AuthState::LoggedOut { login_hint } = discovery.auth else {
            panic!("expected a logged-out state, received {:?}", discovery.auth);
        };
        assert_eq!(login_hint.chars().count(), 256);
    }

    #[test]
    fn normalising_bounds_every_vendor_label() {
        let discovery = Discovery {
            version: Some("v".repeat(200)),
            models: vec![Model {
                display_name: Some("n".repeat(400)),
                description: Some("d".repeat(5_000)),
                reasoning_efforts: vec![ReasoningEffort {
                    id: String::from("high"),
                    display_name: Some("e".repeat(400)),
                    description: None,
                }],
                ..Model::new("opus")
            }],
            ..usable()
        }
        .normalized();

        assert_eq!(
            discovery.version.map(|version| version.chars().count()),
            Some(128)
        );
        let model = &discovery.models[0];
        assert_eq!(model.id, "opus");
        assert_eq!(model.reasoning_efforts[0].id, "high");
        assert_eq!(
            model.display_name.as_ref().map(|name| name.chars().count()),
            Some(256)
        );
        assert_eq!(
            model.description.as_ref().map(|text| text.chars().count()),
            Some(4_096)
        );
        assert_eq!(
            model.reasoning_efforts[0]
                .display_name
                .as_ref()
                .map(|name| name.chars().count()),
            Some(256)
        );
    }
}
