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
use std::time::{Duration, SystemTime};

use crate::configuration::ConfigurationCatalog;
use crate::error::{Error, Result};
use crate::harness::{CapabilityCeiling, DiscoveredCapabilities, HarnessDescriptor};
use crate::identity::HarnessId;
use crate::normalize::{self, MODEL_CATALOG_MAX_ITEMS, REASONING_EFFORT_MAX_ITEMS, TextLimit};
use crate::permission::{PermissionMatrix, UnsupportedReason};

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
    pub capabilities: DiscoveredCapabilities,
    /// Which permission configurations this installed build can run.
    ///
    /// A probe may narrow the harness declaration for account or policy facts it learned. The
    /// provided [`Harness::discover`](crate::Harness::discover) method clamps it back to that
    /// declaration before a host receives it.
    ///
    /// This is the **declared** matrix narrowed by whatever the probe could learn without opening
    /// a session. A cell an account or an administrator policy removes only at open time still
    /// reads as supported here; the refusal arrives from
    /// [`open_session`](crate::Harness::open_session).
    pub permission_matrix: PermissionMatrix,
    /// The models the vendor advertises, when it enumerates them.
    pub models: Vec<Model>,
    /// Everything else the vendor lets a session be configured with, when it enumerates them.
    ///
    /// Empty is "the vendor does not publish its settings", which is a different statement from a
    /// catalog whose rows are all unsupported.
    pub configuration_catalog: ConfigurationCatalog,
}

impl Discovery {
    /// Nothing found: not installed, nothing known, nothing supported.
    pub fn not_installed() -> Self {
        Self {
            executable: None,
            version: None,
            gate: GateVerdict::NotInstalled,
            auth: AuthState::Unknown,
            capabilities: DiscoveredCapabilities::none(),
            permission_matrix: PermissionMatrix::none(UnsupportedReason::Other(String::from(
                "the agent executable is not installed",
            ))),
            models: Vec::new(),
            configuration_catalog: ConfigurationCatalog::empty(),
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
    /// raw output. Ids and paths are refused rather than cut, on the same terms as everywhere else:
    /// a shortened id names a different model, and the model it names would be echoed straight back
    /// to the vendor as the one that was chosen.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            executable: self.executable.and_then(bounded_executable),
            version: self
                .version
                .map(|version| normalize::bound_text(&version, TextLimit::AccountLabel).text),
            gate: self.gate.normalized(),
            auth: self.auth.normalized(),
            permission_matrix: self.permission_matrix.normalized(),
            models: self
                .models
                .into_iter()
                .filter_map(Model::normalized)
                .take(MODEL_CATALOG_MAX_ITEMS)
                .collect(),
            configuration_catalog: self.configuration_catalog.normalized(),
            ..self
        }
    }

    /// This discovery constrained by the harness facts that are true before probing.
    #[must_use]
    pub fn bounded_by(
        mut self,
        capability_ceiling: &CapabilityCeiling,
        declared_permissions: &PermissionMatrix,
    ) -> Self {
        self.capabilities = self.capabilities.clamped_to(capability_ceiling);
        self.permission_matrix = self.permission_matrix.bounded_by(declared_permissions);
        self
    }
}

/// How long a host-vouched discovery stays usable when nobody says otherwise.
pub const DISCOVERY_RECEIPT_MAX_AGE: Duration = Duration::from_secs(300);

/// A discovery a host already ran, offered back so opening a session need not run it again.
///
/// Probing costs process launches — up to six for one Claude Code open — and a host that has just
/// drawn a picker from a probe has the answer in its hand. This is the seam that lets it say so.
///
/// It is **not** a cache. Nothing in this library stores one, looks one up, or reuses one across
/// calls: the receipt travels on the [`OpenSession`](crate::OpenSession) that uses it and is
/// forgotten afterwards. Deciding how fresh an answer has to be stays the host's decision, which
/// is the same reason [`Harness::probe`](crate::Harness::probe) may not memoise.
///
/// What it carries beyond the discovery itself is identity: which harness this was a probe *of*,
/// which executable, and opaque fingerprints of the executable and the environment it was probed
/// under. A host that upgraded the CLI between the probe and the open has a receipt that no longer
/// describes the file about to be launched, and [`DiscoveryReceipt::verify_for`] says so.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiscoveryReceipt {
    /// Which harness this was a probe of.
    pub harness: HarnessId,
    /// What the probe found.
    pub discovery: Discovery,
    /// An opaque fingerprint of the executable the host probed, when it computed one.
    ///
    /// The library never computes or interprets one: a host that hashes the binary, reads its
    /// mtime, or records its package version all produce something [`DiscoveryReceipt::describes`]
    /// can compare for equality, which is all it needs to.
    ///
    /// It is **not** checked by [`DiscoveryReceipt::verify_for`], and cannot be: measuring what
    /// the executable looks like *now* is something only the host can do. A host that wants the
    /// check measures again at open time and calls [`DiscoveryReceipt::describes`].
    pub executable_fingerprint: Option<String>,
    /// An opaque fingerprint of the environment the probe ran under, when the host computed one.
    ///
    /// Compared by [`DiscoveryReceipt::describes`] on the same terms as the executable's.
    pub environment_fingerprint: Option<String>,
    /// When the probe ran.
    pub observed_at: SystemTime,
    /// How long after `observed_at` this receipt may still be used.
    pub max_age: Duration,
}

impl DiscoveryReceipt {
    /// A receipt for a probe that ran at `observed_at`.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{Discovery, DiscoveryReceipt, HarnessId};
    /// use std::time::{Duration, SystemTime};
    ///
    /// let now = SystemTime::now();
    /// let receipt = DiscoveryReceipt::new(HarnessId::claude(), Discovery::not_installed(), now);
    /// assert!(receipt.is_fresh(now));
    /// assert!(!receipt.is_fresh(now + Duration::from_secs(3_600)));
    /// ```
    pub fn new(harness: HarnessId, discovery: Discovery, observed_at: SystemTime) -> Self {
        Self {
            harness,
            discovery,
            executable_fingerprint: None,
            environment_fingerprint: None,
            observed_at,
            max_age: DISCOVERY_RECEIPT_MAX_AGE,
        }
    }

    /// Records the host's own fingerprint of the executable it probed.
    #[must_use]
    pub fn with_executable_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.executable_fingerprint = Some(fingerprint.into());
        self
    }

    /// Records the host's own fingerprint of the environment the probe ran under.
    #[must_use]
    pub fn with_environment_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.environment_fingerprint = Some(fingerprint.into());
        self
    }

    /// Sets how long this receipt may be used for.
    #[must_use]
    pub fn valid_for(mut self, max_age: Duration) -> Self {
        self.max_age = max_age;
        self
    }

    /// How old this receipt is at `now`, or nothing when `now` is before it was taken.
    ///
    /// The second case is not hypothetical: a clock steps backwards on an NTP correction, a
    /// resumed virtual machine, or a receipt stamped by a machine that was ahead of this one. An
    /// unsigned duration cannot express it, so it is reported as absence rather than folded into
    /// zero — folding it into zero is what would make such a receipt look fresh for as long as the
    /// skew lasted.
    pub fn age(&self, now: SystemTime) -> Option<Duration> {
        now.duration_since(self.observed_at).ok()
    }

    /// Whether this receipt is still inside its own freshness window.
    ///
    /// False when `now` is before [`observed_at`](Self::observed_at): a receipt this library
    /// cannot age is a receipt it will not vouch for.
    pub fn is_fresh(&self, now: SystemTime) -> bool {
        self.age(now).is_some_and(|age| age <= self.max_age)
    }

    /// Refuses a receipt that does not match what the host is measuring now.
    ///
    /// The half [`DiscoveryReceipt::verify_for`] cannot do. A fingerprint is whatever the host
    /// chose it to be — a hash, an mtime, a package version — so the only thing that can say
    /// whether the executable on disk is still the one that was probed is the host, measuring
    /// again. This compares the two.
    ///
    /// A fingerprint the receipt does not carry is not checked: a host that recorded nothing is
    /// vouching without one, which is its decision to make.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] naming which fingerprint moved.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{Discovery, DiscoveryReceipt, HarnessId};
    /// use std::time::SystemTime;
    ///
    /// let receipt = DiscoveryReceipt::new(HarnessId::claude(), Discovery::not_installed(),
    ///     SystemTime::now())
    ///     .with_executable_fingerprint("sha256:abc");
    ///
    /// assert!(receipt.describes(Some("sha256:abc"), None).is_ok());
    /// assert!(receipt.describes(Some("sha256:def"), None).is_err());
    /// ```
    pub fn describes(
        &self,
        executable_fingerprint: Option<&str>,
        environment_fingerprint: Option<&str>,
    ) -> Result<()> {
        compare(
            "executable",
            self.executable_fingerprint.as_deref(),
            executable_fingerprint,
        )?;
        compare(
            "environment",
            self.environment_fingerprint.as_deref(),
            environment_fingerprint,
        )
    }

    /// Refuses a receipt that does not describe the session about to be opened.
    ///
    /// Applied by [`Harness::validate_open_session`](crate::Harness::validate_open_session), so a
    /// harness that accepts receipts gets the identity and freshness checks without writing them.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] naming which of the three did not match: the harness it was a
    /// probe of, the executable the session will launch, or its age.
    pub fn verify_for(
        &self,
        descriptor: &HarnessDescriptor,
        now: SystemTime,
        request: &crate::OpenSession,
    ) -> Result<()> {
        if &self.harness != descriptor.id() {
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt for the harness being opened",
                received: format!(
                    "a receipt for {}, opening {}",
                    self.harness, descriptor.identity.id
                ),
            });
        }
        if !self.is_fresh(now) {
            let age = self.age(now).map_or_else(
                || String::from("one taken in the future"),
                |age| format!("one {}s old", age.as_secs()),
            );
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt inside its own freshness window",
                received: format!("{age}, valid for {}s", self.max_age.as_secs()),
            });
        }
        let requested = request.executable.get();
        let probed = self.discovery.executable.as_ref();
        match (requested, probed) {
            (Some(requested), Some(probed)) if requested != probed => {
                Err(Error::HostConfiguration {
                    expected: "a discovery receipt for the executable being launched",
                    received: format!(
                        "a receipt for {}, launching {}",
                        probed.display(),
                        requested.display()
                    ),
                })
            }
            // The probe resolved a path and the request did not, so the launcher will resolve the
            // program name itself — off a `PATH` that may well answer with a different file. A
            // receipt that vouches for one binary cannot vouch for whichever one that turns out to
            // be, so it is refused rather than quietly applied to it.
            (None, Some(probed)) => Err(Error::HostConfiguration {
                expected: "a request naming the executable its receipt was a probe of",
                received: format!(
                    "a receipt for {}, launching whatever the program name resolves to",
                    probed.display()
                ),
            }),
            _ => Ok(()),
        }
    }
}

/// Refuses one fingerprint that moved since the probe.
fn compare(subject: &'static str, recorded: Option<&str>, measured: Option<&str>) -> Result<()> {
    match (recorded, measured) {
        (Some(recorded), Some(measured)) if recorded != measured => Err(Error::HostConfiguration {
            expected: "a discovery receipt whose fingerprints still match",
            received: format!("the {subject} fingerprint moved from {recorded:?} to {measured:?}"),
        }),
        _ => Ok(()),
    }
}

/// A probe's executable path, kept only when it survives bounding exactly as it was written.
///
/// Dropped rather than repaired, and the one place in this module where that matters most: this
/// path is what a host hands back as [`OpenSession::with_executable`](crate::OpenSession), which
/// becomes `argv[0]`. A truncated path names a different file, and a path whose non-UTF-8 bytes
/// were replaced names a different file too — `ExecutablePath::or` renders it with
/// `to_string_lossy` on the way to the launcher. A discovery without an executable is one the host
/// resolves by program name, which is what it did before any probe ran.
fn bounded_executable(executable: PathBuf) -> Option<PathBuf> {
    normalize::vendor_path(executable.to_str()?).map(PathBuf::from)
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

impl GateVerdict {
    /// This verdict with every vendor-written label bounded.
    ///
    /// Only `found` is the vendor's: `minimum` is the floor this harness declares, so it is not a
    /// value a probe can grow.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self {
            Self::VersionTooOld { found, minimum } => Self::VersionTooOld {
                found: normalize::bound_text(&found, TextLimit::AccountLabel).text,
                minimum,
            },
            other => other,
        }
    }
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
    use super::{
        AuthMode, AuthState, Discovery, DiscoveryReceipt, GateVerdict, Model, ReasoningEffort,
    };
    use crate::configuration::ConfigurationCatalog;
    use crate::harness::DiscoveredCapabilities;
    use crate::identity::HarnessId;
    use crate::permission::{PermissionMatrix, UnsupportedReason};

    fn usable() -> Discovery {
        Discovery {
            executable: Some("/usr/local/bin/claude".into()),
            version: Some(String::from("2.1.0")),
            gate: GateVerdict::Usable,
            auth: AuthState::LoggedIn {
                mode: AuthMode::Subscription,
            },
            capabilities: DiscoveredCapabilities::none(),
            permission_matrix: PermissionMatrix::none(UnsupportedReason::NotOfferedByVendor),
            models: Vec::new(),
            configuration_catalog: ConfigurationCatalog::empty(),
        }
    }

    #[test]
    fn nothing_installed_reports_nothing_and_claims_nothing() {
        let discovery = Discovery::not_installed();
        assert_eq!(discovery.gate, GateVerdict::NotInstalled);
        assert_eq!(discovery.auth, AuthState::Unknown);
        assert_eq!(discovery.capabilities, DiscoveredCapabilities::none());
        assert_eq!(discovery.permission_matrix.cells().len(), 6);
        assert!(
            discovery.permission_matrix.cells().iter().all(|cell| {
                !cell.supported
                    && matches!(
                        cell.unsupported_reason,
                        Some(UnsupportedReason::Other(ref reason))
                            if reason == "the agent executable is not installed"
                    )
            }),
            "expected every permission configuration to stay unavailable without an executable"
        );
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

    /// The one probe-written field that round-trips into a spawn: `Discovery::executable` becomes
    /// `OpenSession::with_executable`, which becomes `argv[0]`. A path that cannot be kept as the
    /// vendor wrote it is dropped, never repaired, so the host resolves by name instead of running
    /// something else.
    #[test]
    fn an_executable_path_that_cannot_be_kept_verbatim_is_dropped_rather_than_repaired() {
        let cases = [
            ("/usr/local/bin/\u{202e}claude", "a bidirectional override"),
            ("/opt/\u{0}/claude", "a control character"),
            ("", "an empty path"),
        ];
        for (path, what) in cases {
            let discovery = Discovery {
                executable: Some(path.into()),
                ..usable()
            }
            .normalized();
            assert_eq!(
                discovery.executable, None,
                "expected {what} to drop the executable, received {:?}",
                discovery.executable
            );
        }

        let long = Discovery {
            executable: Some(format!("/{}", "p".repeat(4_096)).into()),
            ..usable()
        }
        .normalized();
        assert_eq!(long.executable, None, "expected an oversized path to drop");

        let kept = Discovery { ..usable() }.normalized();
        assert_eq!(
            kept.executable,
            Some(std::path::PathBuf::from("/usr/local/bin/claude")),
            "expected an ordinary path to survive untouched"
        );
    }

    #[test]
    fn the_version_a_gate_refused_is_bounded_like_any_other_vendor_label() {
        let discovery = Discovery {
            gate: GateVerdict::VersionTooOld {
                found: "9".repeat(5_000),
                minimum: String::from("2.0.0"),
            },
            ..usable()
        }
        .normalized();

        let GateVerdict::VersionTooOld { found, minimum } = discovery.gate else {
            panic!("expected a refused version, received {:?}", discovery.gate);
        };
        assert_eq!(found.chars().count(), 128);
        assert_eq!(
            minimum, "2.0.0",
            "expected the harness's own floor untouched"
        );
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

    fn request() -> crate::OpenSession {
        crate::OpenSession::new("chat-1").with_executable(
            crate::transport::ExecutablePath::resolved("/usr/local/bin/claude"),
        )
    }

    fn descriptor() -> crate::harness::HarnessDescriptor {
        crate::harness::HarnessDescriptor {
            identity: crate::identity::HarnessIdentity::claude(),
            vendor: crate::harness::VendorInfo {
                company: "Anthropic",
                terms_url: "https://www.anthropic.com/legal/consumer-terms",
                privacy_url: "https://www.anthropic.com/legal/privacy",
                skills_are_slash_commands: true,
            },
            capabilities: crate::harness::CapabilityCeiling::none(),
            transports: &[crate::transport::TransportKind::Stdio],
            vendor_environment_keys: &[],
        }
    }

    /// A receipt is a seam, not a cache: it says which probe, of what, and when, and every one of
    /// those is checked against the session about to be opened.
    #[test]
    fn a_fresh_receipt_for_the_same_harness_and_executable_is_accepted() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .with_executable_fingerprint("sha256:abc")
            .with_environment_fingerprint("env-1");

        assert_eq!(receipt.age(now), Some(std::time::Duration::ZERO));
        assert!(receipt.is_fresh(now));
        receipt
            .verify_for(
                &descriptor(),
                now + std::time::Duration::from_secs(10),
                &crate::OpenSession::new("chat-1").with_executable(
                    crate::transport::ExecutablePath::resolved("/usr/local/bin/claude"),
                ),
            )
            .expect("expected a fresh matching receipt to be accepted");

        // The fingerprints are the host's own check, because only the host can measure what the
        // file looks like now.
        receipt
            .describes(Some("sha256:abc"), Some("env-1"))
            .expect("expected matching fingerprints to be accepted");
        let error = receipt
            .describes(Some("sha256:def"), Some("env-1"))
            .expect_err("expected a moved executable fingerprint to be refused");
        assert!(
            error.to_string().contains("executable fingerprint moved"),
            "expected the diagnostic to name which fingerprint moved, received {error}"
        );
        let error = receipt
            .describes(Some("sha256:abc"), Some("env-2"))
            .expect_err("expected a moved environment fingerprint to be refused");
        assert!(
            error.to_string().contains("environment fingerprint moved"),
            "received {error}"
        );
        // A host that recorded nothing is vouching without a fingerprint, which is its decision.
        receipt
            .describes(None, None)
            .expect("expected an unmeasured fingerprint to be left alone");
    }

    /// A receipt for a resolved path cannot vouch for whatever a bare program name resolves to.
    #[test]
    fn a_receipt_for_a_resolved_executable_refuses_a_request_that_names_none() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let error = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .verify_for(&descriptor(), now, &crate::OpenSession::new("chat-1"))
            .expect_err("expected a refusal");
        assert!(
            error
                .to_string()
                .contains("whatever the program name resolves to"),
            "received {error}"
        );
    }

    /// A host that upgraded the CLI between the probe and the open has a receipt that no longer
    /// describes the file about to be launched.
    #[test]
    fn a_receipt_for_another_executable_is_refused_by_name() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let error = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .verify_for(
                &descriptor(),
                now,
                &crate::OpenSession::new("chat-1").with_executable(
                    crate::transport::ExecutablePath::resolved("/opt/claude/bin/claude"),
                ),
            )
            .expect_err("expected a refusal");
        assert!(
            error.to_string().contains("/opt/claude/bin/claude"),
            "expected the executable being launched in the diagnostic, received {error}"
        );
    }

    #[test]
    fn a_receipt_for_another_harness_is_refused_by_name() {
        let now = std::time::SystemTime::UNIX_EPOCH;
        let error = DiscoveryReceipt::new(HarnessId::codex(), usable(), now)
            .verify_for(&descriptor(), now, &request())
            .expect_err("expected a refusal");
        assert!(
            error.to_string().contains("a receipt for codex"),
            "expected both harness ids in the diagnostic, received {error}"
        );
    }

    #[test]
    fn a_stale_receipt_is_refused_with_its_own_age_and_window() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .valid_for(std::time::Duration::from_secs(60));
        let later = now + std::time::Duration::from_secs(61);

        assert!(!receipt.is_fresh(later));
        let error = receipt
            .verify_for(&descriptor(), later, &request())
            .expect_err("expected a refusal");
        assert!(
            error.to_string().contains("61s old, valid for 60s"),
            "expected the age and the window in the diagnostic, received {error}"
        );
    }

    /// A clock that went backwards must not make a receipt look fresh forever — which is exactly
    /// what folding an un-measurable age into zero would have done, for as long as the skew lasted.
    #[test]
    fn a_receipt_this_library_cannot_age_is_one_it_will_not_vouch_for() {
        let observed = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), usable(), observed);
        let earlier = std::time::SystemTime::UNIX_EPOCH;

        assert_eq!(receipt.age(earlier), None);
        assert!(!receipt.is_fresh(earlier));
        let error = receipt
            .verify_for(&descriptor(), earlier, &request())
            .expect_err("expected a refusal");
        assert!(
            error.to_string().contains("taken in the future"),
            "received {error}"
        );
    }
}
