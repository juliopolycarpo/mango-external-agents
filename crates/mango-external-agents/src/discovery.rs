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

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::configuration::ConfigurationCatalog;
use crate::error::{Error, Result};
use crate::harness::{CapabilityCeiling, DiscoveredCapabilities, HarnessDescriptor};
use crate::identity::{HarnessId, HarnessIdentity};
use crate::normalize::{self, MODEL_CATALOG_MAX_ITEMS, REASONING_EFFORT_MAX_ITEMS, TextLimit};
use crate::permission::{PermissionMatrix, UnsupportedReason};

/// How the installed CLI was found, and what it can do.
#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for Discovery {
    /// Reports discovery shape without logging probe-written paths, labels, or catalog values.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Discovery")
            .field("has_executable", &self.executable.is_some())
            .field("has_version", &self.version.is_some())
            .field("gate", &self.gate)
            .field("auth", &self.auth)
            .field("capabilities", &self.capabilities)
            .field("permission_matrix", &self.permission_matrix)
            .field("model_count", &self.models.len())
            .field(
                "configuration_option_count",
                &self.configuration_catalog.options().len(),
            )
            .finish()
    }
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

/// Fresh host measurements used to bind a discovery receipt to one opening request.
///
/// Every value is opaque to this crate. A host computes each value from the current state it
/// authorises: the resolved executable, the allowlisted child environment, and the account or
/// managed-policy facts that affected discovery. The receipt compares them for equality and never
/// formats or serializes them.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct DiscoveryReceiptMeasurements {
    executable_fingerprint: Option<String>,
    environment_fingerprint: Option<String>,
    authorization_fingerprint: Option<String>,
}

impl fmt::Debug for DiscoveryReceiptMeasurements {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveryReceiptMeasurements")
            .field(
                "has_executable_fingerprint",
                &self.executable_fingerprint.is_some(),
            )
            .field(
                "has_environment_fingerprint",
                &self.environment_fingerprint.is_some(),
            )
            .field(
                "has_authorization_fingerprint",
                &self.authorization_fingerprint.is_some(),
            )
            .finish()
    }
}

impl DiscoveryReceiptMeasurements {
    /// Starts with no measurements.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds the host's new measurement of the executable being opened.
    #[must_use]
    pub fn with_executable_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.executable_fingerprint = Some(fingerprint.into());
        self
    }

    /// Adds the host's new measurement of the allowlisted child environment.
    #[must_use]
    pub fn with_environment_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.environment_fingerprint = Some(fingerprint.into());
        self
    }

    /// Adds the host's new measurement of account and managed-policy facts.
    #[must_use]
    pub fn with_authorization_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.authorization_fingerprint = Some(fingerprint.into());
        self
    }
}

/// The non-secret facts a receipt must hold constant from binding to opening.
#[derive(Clone, PartialEq, Eq)]
struct ReceiptBinding {
    identity: HarnessIdentity,
    transport: crate::TransportKind,
    workspace: PathBuf,
    request: ReceiptRequestContext,
}

/// The request fields that affect what a vendor opens.
#[derive(Clone, PartialEq, Eq)]
struct ReceiptRequestContext {
    configuration: crate::ConfigurationPatch,
    resume: Option<crate::Resume>,
    mcp_servers: Vec<crate::McpServer>,
}

impl ReceiptRequestContext {
    fn from_open_request(request: &crate::OpenSession) -> Self {
        Self {
            configuration: request.configuration.clone(),
            resume: request.resume.clone(),
            mcp_servers: request.mcp_servers.clone(),
        }
    }

    fn matches(&self, request: &crate::OpenSession) -> bool {
        self == &Self::from_open_request(request)
    }
}

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
/// The receipt records the probe's stable harness id and host-provided fingerprints. Before it can
/// be reused, [`DiscoveryReceipt::bind_to_open`] requires current measurements and binds the full
/// harness identity, effective transport, authorised workspace and opening request. The binding
/// is private and has no serialization or diagnostic representation.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DiscoveryReceipt {
    /// Which harness this was a probe of.
    pub harness: HarnessId,
    /// What the probe found.
    pub discovery: Discovery,
    /// An opaque fingerprint of the executable the host probed, when it computed one.
    ///
    /// The library never computes or interprets one: a host that hashes the binary, reads its
    /// mtime, or records its package version can use it. Reuse requires a matching current
    /// measurement through [`DiscoveryReceipt::bind_to_open`].
    pub executable_fingerprint: Option<String>,
    /// An opaque fingerprint of the environment the probe ran under, when the host computed one.
    ///
    /// Compared by [`DiscoveryReceipt::describes`] on the same terms as the executable's.
    pub environment_fingerprint: Option<String>,
    /// An opaque fingerprint of account and managed-policy facts observed by the probe.
    ///
    /// A host computes this from every authentication or policy fact that narrowed discovery's
    /// permission matrix. Reuse requires a new matching measurement, so a receipt cannot widen a
    /// permission after an account or administrator policy changed.
    pub authorization_fingerprint: Option<String>,
    /// When the probe ran.
    pub observed_at: SystemTime,
    /// How long after `observed_at` this receipt may still be used.
    pub max_age: Duration,
    binding: Option<ReceiptBinding>,
}

impl fmt::Debug for DiscoveryReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiscoveryReceipt")
            .field("harness", &self.harness)
            .field("has_executable", &self.discovery.executable.is_some())
            .field(
                "has_executable_fingerprint",
                &self.executable_fingerprint.is_some(),
            )
            .field(
                "has_environment_fingerprint",
                &self.environment_fingerprint.is_some(),
            )
            .field(
                "has_authorization_fingerprint",
                &self.authorization_fingerprint.is_some(),
            )
            .field("is_bound", &self.binding.is_some())
            .field("observed_at", &self.observed_at)
            .field("max_age", &self.max_age)
            .finish()
    }
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
            authorization_fingerprint: None,
            observed_at,
            max_age: DISCOVERY_RECEIPT_MAX_AGE,
            binding: None,
        }
    }

    /// Records the host's own fingerprint of the executable it probed.
    #[must_use]
    pub fn with_executable_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.executable_fingerprint = Some(fingerprint.into());
        self
    }

    /// Records the host's fingerprint of the allowlisted child environment used by the probe.
    #[must_use]
    pub fn with_environment_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.environment_fingerprint = Some(fingerprint.into());
        self
    }

    /// Records the probe's account and managed-policy fingerprint.
    #[must_use]
    pub fn with_authorization_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.authorization_fingerprint = Some(fingerprint.into());
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
    /// A fingerprint is whatever the host chose it to be — a hash, an mtime, a package version —
    /// so only the host can say whether the executable on disk is still the one it probed. A
    /// recorded fingerprint requires a current measurement. Absence is a refusal, never a match.
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

    /// Binds this receipt to the session about to open after the host remeasured its launch state.
    ///
    /// A receipt starts unbound. Calling this method records the full harness identity, the
    /// selected effective transport, the host-authorised workspace and every request field that
    /// can change the opening. It also requires current executable, child-environment and
    /// authorization measurements to match the probe's recorded fingerprints. The receipt can
    /// then travel on exactly that [`OpenSession`](crate::OpenSession).
    ///
    /// The environment measurement covers the output of
    /// [`HostContext::child_environment`](crate::HostContext::child_environment), including this
    /// harness's documented vendor keys. The authorization measurement covers every observed
    /// account or managed-policy fact that narrowed discovery. The library only compares opaque
    /// values, so neither value reaches debug output, errors or serialization.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when identity, executable, measurements or the requested
    /// transport do not match, or when any required fingerprint is missing.
    pub fn bind_to_open(
        mut self,
        descriptor: &HarnessDescriptor,
        host: &crate::HostContext,
        request: &crate::OpenSession,
        measurements: DiscoveryReceiptMeasurements,
    ) -> Result<Self> {
        if &self.harness != descriptor.id() {
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt for the harness being opened",
                received: String::from("a receipt for a different harness"),
            });
        }
        let transport = descriptor.resolve_transport(request.transport)?;
        self.verify_executable(request)?;
        self.verify_measurements(&measurements)?;
        self.binding = Some(ReceiptBinding {
            identity: descriptor.identity.clone(),
            transport,
            workspace: host.cwd().to_owned(),
            request: ReceiptRequestContext::from_open_request(request),
        });
        Ok(self)
    }

    /// Refuses a receipt that does not describe the session about to be opened.
    ///
    /// Applied by [`Harness::validate_open_session`](crate::Harness::validate_open_session), so a
    /// harness that accepts receipts gets the identity and freshness checks without writing them.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] naming the receipt evidence that did not match.
    pub fn verify_for(
        &self,
        descriptor: &HarnessDescriptor,
        host: &crate::HostContext,
        request: &crate::OpenSession,
    ) -> Result<()> {
        if &self.harness != descriptor.id() {
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt for the harness being opened",
                received: String::from("a receipt for a different harness"),
            });
        }
        if !self.is_fresh(host.now()) {
            let age = self.age(host.now()).map_or_else(
                || String::from("one taken in the future"),
                |age| format!("one {}s old", age.as_secs()),
            );
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt inside its own freshness window",
                received: format!("{age}, valid for {}s", self.max_age.as_secs()),
            });
        }
        self.verify_executable(request)?;
        let binding = self
            .binding
            .as_ref()
            .ok_or_else(|| Error::HostConfiguration {
                expected: "a discovery receipt bound to this opening request",
                received: String::from("an unbound discovery receipt"),
            })?;
        if binding.identity != descriptor.identity {
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt for the protocol and profile being opened",
                received: String::from("a receipt bound to a different protocol or profile"),
            });
        }
        if binding.transport != descriptor.resolve_transport(request.transport)? {
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt for the selected effective transport",
                received: String::from("a receipt bound to a different transport"),
            });
        }
        if binding.workspace != host.cwd() {
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt for the authorised workspace",
                received: String::from("a receipt bound to a different workspace"),
            });
        }
        if !binding.request.matches(request) {
            return Err(Error::HostConfiguration {
                expected: "a discovery receipt for the requested session context",
                received: String::from("a receipt bound to a different request context"),
            });
        }
        Ok(())
    }

    fn verify_executable(&self, request: &crate::OpenSession) -> Result<()> {
        let requested = request.executable.get();
        let probed = self.discovery.executable.as_ref();
        match (requested, probed) {
            (Some(requested), Some(probed)) if requested != probed => {
                Err(Error::HostConfiguration {
                    expected: "a discovery receipt for the executable being launched",
                    received: String::from("a receipt and request that name different executables"),
                })
            }
            (Some(_), None) => Err(Error::HostConfiguration {
                expected: "a discovery receipt for the executable being launched",
                received: String::from("a receipt without an executable and a request with one"),
            }),
            // The probe resolved a path and the request did not, so the launcher will resolve the
            // program name itself — off a `PATH` that may well answer with a different file. A
            // receipt that vouches for one binary cannot vouch for whichever one that turns out to
            // be, so it is refused rather than quietly applied to it.
            (None, Some(_)) => Err(Error::HostConfiguration {
                expected: "a request naming the executable its receipt was a probe of",
                received: String::from("a receipt with an executable and a request without one"),
            }),
            _ => Ok(()),
        }
    }

    fn verify_measurements(&self, measurements: &DiscoveryReceiptMeasurements) -> Result<()> {
        require_measurement(
            "executable",
            self.executable_fingerprint.as_deref(),
            measurements.executable_fingerprint.as_deref(),
        )?;
        require_measurement(
            "allowlisted environment",
            self.environment_fingerprint.as_deref(),
            measurements.environment_fingerprint.as_deref(),
        )?;
        require_measurement(
            "authorization",
            self.authorization_fingerprint.as_deref(),
            measurements.authorization_fingerprint.as_deref(),
        )
    }
}

/// Refuses one fingerprint that moved since the probe.
fn compare(subject: &'static str, recorded: Option<&str>, measured: Option<&str>) -> Result<()> {
    match (recorded, measured) {
        (Some(recorded), Some(measured)) if recorded != measured => Err(Error::HostConfiguration {
            expected: "a discovery receipt whose fingerprints still match",
            received: format!("the {subject} fingerprint changed"),
        }),
        (Some(_), None) => Err(Error::HostConfiguration {
            expected: "a current measurement for every recorded discovery fingerprint",
            received: format!("the {subject} fingerprint was not remeasured"),
        }),
        _ => Ok(()),
    }
}

/// Refuses a receipt that was never given all the evidence safe reuse needs.
fn require_measurement(
    subject: &'static str,
    recorded: Option<&str>,
    measured: Option<&str>,
) -> Result<()> {
    match (recorded, measured) {
        (Some(_), Some(_)) => compare(subject, recorded, measured),
        (Some(_), None) => compare(subject, recorded, measured),
        (None, _) => Err(Error::HostConfiguration {
            expected: "a discovery receipt with executable, allowlisted-environment and authorization fingerprints",
            received: format!("the recorded {subject} fingerprint was missing"),
        }),
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
#[derive(Clone, PartialEq, Eq)]
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
    /// The installed CLI did not publish a documented surface the harness needs to drive it.
    ///
    /// Both strings are library-authored summaries. They name the required surface and the safe
    /// shape of the observation without copying a vendor help line into a diagnostic.
    MissingRequiredSurface {
        /// The documented surface the harness needs.
        expected: &'static str,
        /// The safe summary of what the harness observed.
        received: &'static str,
    },
    /// The probe could not tell — it timed out, or printed something unrecognisable.
    ///
    /// Deliberately not a refusal: a CLI that changed the shape of `--version` is not a CLI that
    /// stopped working, and a host may still choose to try.
    Unknown,
}

impl fmt::Debug for GateVerdict {
    /// Names the gate result without logging versions reported by a probe.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usable => formatter.write_str("Usable"),
            Self::NotInstalled => formatter.write_str("NotInstalled"),
            Self::VersionTooOld { .. } => formatter.write_str("VersionTooOld"),
            Self::MissingRequiredSurface { .. } => formatter.write_str("MissingRequiredSurface"),
            Self::Unknown => formatter.write_str("Unknown"),
        }
    }
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
#[derive(Clone, PartialEq, Eq)]
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

impl fmt::Debug for AuthState {
    /// Names the auth state without logging a vendor login hint.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LoggedIn { mode } => formatter
                .debug_struct("LoggedIn")
                .field("mode", mode)
                .finish(),
            Self::LoggedOut { .. } => formatter.write_str("LoggedOut"),
            Self::Unknown => formatter.write_str("Unknown"),
        }
    }
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
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthMode {
    /// A consumer subscription.
    Subscription,
    /// An API key the user configured with the vendor's own CLI.
    ApiKey,
    /// Something else the vendor named.
    Other(String),
}

impl fmt::Debug for AuthMode {
    /// Names the auth mode without logging a vendor-defined mode label.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Subscription => "Subscription",
            Self::ApiKey => "ApiKey",
            Self::Other(_) => "Other",
        })
    }
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
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

impl fmt::Debug for Model {
    /// Reports model shape without logging vendor ids or labels.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Model")
            .field("has_id", &!self.id.is_empty())
            .field("has_display_name", &self.display_name.is_some())
            .field("has_description", &self.description.is_some())
            .field("is_default", &self.is_default)
            .field("reasoning_effort_count", &self.reasoning_efforts.len())
            .field(
                "has_default_reasoning_effort",
                &self.default_reasoning_effort.is_some(),
            )
            .finish()
    }
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
#[derive(Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

impl fmt::Debug for ReasoningEffort {
    /// Reports reasoning-effort shape without logging a vendor id or labels.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReasoningEffort")
            .field("has_id", &!self.id.is_empty())
            .field("has_display_name", &self.display_name.is_some())
            .field("has_description", &self.description.is_some())
            .finish()
    }
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
        AuthMode, AuthState, Discovery, DiscoveryReceipt, DiscoveryReceiptMeasurements,
        GateVerdict, Model, ReasoningEffort,
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

    #[test]
    fn a_missing_required_surface_is_a_safe_unusable_gate() {
        let gate = GateVerdict::MissingRequiredSurface {
            expected: "a CLI help surface declaring every required launch flag",
            received: "1 required flag was absent",
        };
        let discovery = Discovery {
            gate: gate.clone(),
            ..usable()
        }
        .normalized();

        assert!(!discovery.is_usable());
        assert_eq!(discovery.gate, gate);
        assert_eq!(format!("{gate:?}"), "MissingRequiredSurface");
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

    fn host_at(now: std::time::SystemTime, cwd: &str) -> crate::HostContext {
        crate::HostContext::builder()
            .launcher(std::sync::Arc::new(crate::testing::FakeLauncher::new()))
            .cwd(cwd)
            .client_info("test", "0.0.0")
            .clock(std::sync::Arc::new(crate::testing::FrozenClock::at(now)))
            .build()
            .expect("expected a host")
    }

    fn measurements() -> super::DiscoveryReceiptMeasurements {
        super::DiscoveryReceiptMeasurements::new()
            .with_executable_fingerprint("sha256:abc")
            .with_environment_fingerprint("env-1")
            .with_authorization_fingerprint("auth-policy-1")
    }

    /// A receipt is a seam, not a cache: it says which probe, of what, and when, and every one of
    /// those is checked against the session about to be opened.
    #[test]
    fn a_fresh_receipt_for_the_same_harness_and_executable_is_accepted() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let request = request();
        let host = host_at(now + std::time::Duration::from_secs(10), "/workspace");
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .with_executable_fingerprint("sha256:abc")
            .with_environment_fingerprint("env-1")
            .with_authorization_fingerprint("auth-policy-1")
            .bind_to_open(&descriptor(), &host, &request, measurements())
            .expect("expected matching current measurements to bind the receipt");

        assert_eq!(receipt.age(now), Some(std::time::Duration::ZERO));
        assert!(receipt.is_fresh(now));
        receipt
            .verify_for(&descriptor(), &host, &request)
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
            error.to_string().contains("executable fingerprint changed"),
            "expected the diagnostic to name which fingerprint moved, received {error}"
        );
        let error = receipt
            .describes(Some("sha256:abc"), Some("env-2"))
            .expect_err("expected a moved environment fingerprint to be refused");
        assert!(
            error
                .to_string()
                .contains("environment fingerprint changed"),
            "received {error}"
        );
        let error = receipt
            .describes(None, None)
            .expect_err("expected a recorded fingerprint without a remeasurement to be refused");
        assert!(
            error
                .to_string()
                .contains("executable fingerprint was not remeasured"),
            "expected the missing executable remeasurement to be named, received {error}"
        );
    }

    #[test]
    fn a_receipt_cannot_be_reused_without_all_current_measurements() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let host = host_at(now, "/workspace");
        let request = request();
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .with_executable_fingerprint("sha256:abc")
            .with_environment_fingerprint("env-1")
            .with_authorization_fingerprint("auth-policy-1");

        for (measurements, expected) in [
            (
                super::DiscoveryReceiptMeasurements::new()
                    .with_environment_fingerprint("env-1")
                    .with_authorization_fingerprint("auth-policy-1"),
                "executable fingerprint was not remeasured",
            ),
            (
                super::DiscoveryReceiptMeasurements::new()
                    .with_executable_fingerprint("sha256:abc")
                    .with_authorization_fingerprint("auth-policy-1"),
                "allowlisted environment fingerprint was not remeasured",
            ),
            (
                super::DiscoveryReceiptMeasurements::new()
                    .with_executable_fingerprint("sha256:abc")
                    .with_environment_fingerprint("env-1"),
                "authorization fingerprint was not remeasured",
            ),
        ] {
            let error = receipt
                .clone()
                .bind_to_open(&descriptor(), &host, &request, measurements)
                .expect_err("expected absent current evidence to refuse receipt reuse");
            assert!(
                error.to_string().contains(expected),
                "expected {expected:?}, received {error}"
            );
        }
    }

    #[test]
    fn a_bound_receipt_refuses_changed_identity_transport_workspace_or_request() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let host = host_at(now, "/workspace-one");
        let request = request();
        let receipt = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .with_executable_fingerprint("sha256:abc")
            .with_environment_fingerprint("env-1")
            .with_authorization_fingerprint("auth-policy-1")
            .bind_to_open(&descriptor(), &host, &request, measurements())
            .expect("expected matching evidence to bind the receipt");

        let workspace_error = receipt
            .verify_for(&descriptor(), &host_at(now, "/workspace-two"), &request)
            .expect_err("expected another workspace to refuse reuse");
        assert!(
            workspace_error
                .to_string()
                .contains("a receipt bound to a different workspace"),
            "received {workspace_error}"
        );

        let changed_request = request.clone().with_configuration(
            crate::ConfigurationPatch::new()
                .model(crate::ConfigurationChange::Set(String::from("model-b"))),
        );
        let request_error = receipt
            .verify_for(&descriptor(), &host, &changed_request)
            .expect_err("expected changed configuration to refuse reuse");
        assert!(
            request_error
                .to_string()
                .contains("a receipt bound to a different request context"),
            "received {request_error}"
        );

        let other_identity = crate::identity::HarnessIdentity::custom(
            "claude",
            "other-protocol",
            Some("other-profile"),
        )
        .expect("expected a test identity");
        let mut other_descriptor = descriptor();
        other_descriptor.identity = other_identity;
        let identity_error = receipt
            .verify_for(&other_descriptor, &host, &request)
            .expect_err("expected a changed protocol or profile to refuse reuse");
        assert!(
            identity_error
                .to_string()
                .contains("a receipt bound to a different protocol or profile"),
            "received {identity_error}"
        );

        static WEBSOCKET_THEN_STDIO: &[crate::TransportKind] =
            &[crate::TransportKind::WebSocket, crate::TransportKind::Stdio];
        let mut other_transport_descriptor = descriptor();
        other_transport_descriptor.transports = WEBSOCKET_THEN_STDIO;
        let transport_bound = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .with_executable_fingerprint("sha256:abc")
            .with_environment_fingerprint("env-1")
            .with_authorization_fingerprint("auth-policy-1")
            .bind_to_open(&other_transport_descriptor, &host, &request, measurements())
            .expect("expected receipt to bind to the selected websocket transport");
        let transport_error = transport_bound
            .verify_for(&descriptor(), &host, &request)
            .expect_err("expected a changed effective transport to refuse reuse");
        assert!(
            transport_error
                .to_string()
                .contains("a receipt bound to a different transport"),
            "received {transport_error}"
        );
    }

    /// A receipt for a resolved path cannot vouch for a request without an executable.
    #[test]
    fn a_receipt_for_a_resolved_executable_refuses_a_request_that_names_none() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let error = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .verify_for(
                &descriptor(),
                &host_at(now, "/workspace"),
                &crate::OpenSession::new("chat-1"),
            )
            .expect_err("expected a refusal");
        assert!(
            error
                .to_string()
                .contains("a receipt with an executable and a request without one"),
            "received {error}"
        );
    }

    #[test]
    fn a_receipt_without_an_executable_cannot_vouch_for_a_requested_one() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let error = DiscoveryReceipt::new(HarnessId::claude(), Discovery::not_installed(), now)
            .verify_for(
                &descriptor(),
                &host_at(now, "/workspace"),
                &crate::OpenSession::new("chat-1").with_executable(
                    crate::transport::ExecutablePath::resolved("/opt/claude/bin/claude"),
                ),
            )
            .expect_err("expected an executable requested without a probed path to be refused");
        assert_eq!(
            error.to_string(),
            "expected a discovery receipt for the executable being launched, received a receipt without an executable and a request with one"
        );
    }

    #[test]
    fn receipt_debug_omits_probe_and_fingerprint_payloads() {
        let receipt = DiscoveryReceipt::new(
            HarnessId::claude(),
            usable(),
            std::time::SystemTime::UNIX_EPOCH,
        )
        .with_executable_fingerprint("executable-fingerprint-secret")
        .with_environment_fingerprint("environment-fingerprint-secret")
        .with_authorization_fingerprint("authorization-fingerprint-secret");
        let measurements = DiscoveryReceiptMeasurements::new()
            .with_executable_fingerprint("measured-executable-secret")
            .with_environment_fingerprint("measured-environment-secret")
            .with_authorization_fingerprint("measured-authorization-secret");
        let rendered = format!("{receipt:?}");
        for secret in [
            "executable-fingerprint-secret",
            "environment-fingerprint-secret",
            "authorization-fingerprint-secret",
            "measured-executable-secret",
            "measured-environment-secret",
            "measured-authorization-secret",
        ] {
            assert!(
                !rendered.contains(secret) && !format!("{measurements:?}").contains(secret),
                "expected no receipt payload in debug output, received {rendered}"
            );
        }
    }

    #[test]
    fn discovery_diagnostics_omit_probe_payloads() {
        let secret_executable = "/private/discovery-executable-secret";
        let secret_version = "discovery-version-secret";
        let secret_login_hint = "discovery-login-hint-secret";
        let secret_model = "discovery-model-secret";
        let secret_effort = "discovery-effort-secret";
        let discovery = Discovery {
            executable: Some(secret_executable.into()),
            version: Some(String::from(secret_version)),
            gate: GateVerdict::VersionTooOld {
                found: String::from(secret_version),
                minimum: String::from("discovery-minimum-secret"),
            },
            auth: AuthState::LoggedOut {
                login_hint: String::from(secret_login_hint),
            },
            models: vec![Model {
                id: String::from(secret_model),
                display_name: Some(String::from("discovery-model-name-secret")),
                description: Some(String::from("discovery-model-description-secret")),
                reasoning_efforts: vec![ReasoningEffort {
                    id: String::from(secret_effort),
                    display_name: Some(String::from("discovery-effort-name-secret")),
                    description: Some(String::from("discovery-effort-description-secret")),
                }],
                default_reasoning_effort: Some(String::from(secret_effort)),
                ..Model::default()
            }],
            ..usable()
        };
        let receipt = DiscoveryReceipt::new(
            HarnessId::claude(),
            discovery.clone(),
            std::time::SystemTime::UNIX_EPOCH,
        )
        .with_executable_fingerprint("recorded-fingerprint-secret");
        let fingerprint_error = receipt
            .describes(Some("measured-fingerprint-secret"), None)
            .expect_err("expected a changed fingerprint to be refused");
        let executable_error = DiscoveryReceipt::new(
            HarnessId::claude(),
            discovery,
            std::time::SystemTime::UNIX_EPOCH,
        )
        .verify_for(
            &descriptor(),
            &host_at(std::time::SystemTime::UNIX_EPOCH, "/workspace"),
            &crate::OpenSession::new("chat-1").with_executable(
                crate::transport::ExecutablePath::resolved("/private/requested-executable-secret"),
            ),
        )
        .expect_err("expected a different executable to be refused");

        for rendered in [
            format!("{receipt:?}"),
            format!("{:#?}", receipt.discovery),
            format!(
                "{:?}",
                GateVerdict::VersionTooOld {
                    found: String::from(secret_version),
                    minimum: String::from("discovery-minimum-secret"),
                }
            ),
            format!(
                "{:?}",
                AuthState::LoggedIn {
                    mode: AuthMode::Other(String::from("discovery-auth-mode-secret")),
                }
            ),
            format!(
                "{:?}",
                Model {
                    id: String::from(secret_model),
                    display_name: Some(String::from("discovery-model-name-secret")),
                    description: Some(String::from("discovery-model-description-secret")),
                    reasoning_efforts: vec![ReasoningEffort {
                        id: String::from(secret_effort),
                        display_name: Some(String::from("discovery-effort-name-secret")),
                        description: Some(String::from("discovery-effort-description-secret")),
                    }],
                    default_reasoning_effort: Some(String::from(secret_effort)),
                    ..Model::default()
                }
            ),
            format!(
                "{:?}",
                ReasoningEffort {
                    id: String::from(secret_effort),
                    display_name: Some(String::from("discovery-effort-name-secret")),
                    description: Some(String::from("discovery-effort-description-secret")),
                }
            ),
            fingerprint_error.to_string(),
            executable_error.to_string(),
        ] {
            for secret in [
                secret_executable,
                secret_version,
                secret_login_hint,
                secret_model,
                secret_effort,
                "discovery-minimum-secret",
                "discovery-model-name-secret",
                "discovery-model-description-secret",
                "discovery-effort-name-secret",
                "discovery-effort-description-secret",
                "discovery-auth-mode-secret",
                "recorded-fingerprint-secret",
                "measured-fingerprint-secret",
                "/private/requested-executable-secret",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "expected no probe payload in diagnostics, received {rendered}"
                );
            }
        }
    }

    /// A host that upgraded the CLI between the probe and the open has a receipt that no longer
    /// describes the file about to be launched.
    #[test]
    fn a_receipt_for_another_executable_is_refused_by_shape() {
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let error = DiscoveryReceipt::new(HarnessId::claude(), usable(), now)
            .verify_for(
                &descriptor(),
                &host_at(now, "/workspace"),
                &crate::OpenSession::new("chat-1").with_executable(
                    crate::transport::ExecutablePath::resolved("/opt/claude/bin/claude"),
                ),
            )
            .expect_err("expected a refusal");
        assert!(
            error
                .to_string()
                .contains("a receipt and request that name different executables"),
            "expected an executable mismatch shape, received {error}"
        );
    }

    #[test]
    fn a_receipt_for_another_harness_is_refused_by_shape() {
        let now = std::time::SystemTime::UNIX_EPOCH;
        let error = DiscoveryReceipt::new(HarnessId::codex(), usable(), now)
            .verify_for(&descriptor(), &host_at(now, "/workspace"), &request())
            .expect_err("expected a refusal");
        assert!(
            error
                .to_string()
                .contains("a receipt for a different harness"),
            "expected a harness mismatch shape, received {error}"
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
            .verify_for(&descriptor(), &host_at(later, "/workspace"), &request())
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
            .verify_for(&descriptor(), &host_at(earlier, "/workspace"), &request())
            .expect_err("expected a refusal");
        assert!(
            error.to_string().contains("taken in the future"),
            "received {error}"
        );
    }
}
