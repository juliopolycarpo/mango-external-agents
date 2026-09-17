//! What the agent may do, who answers when it asks, and how an answer is reached.
//!
//! Two axes — a [`PermissionLevel`] and an [`ApprovalRouting`] — and a matrix saying which of the
//! six pairs a given harness supports. Returning the whole matrix is deliberate: an empty list
//! reads as "this harness has no configurations", which is a different and less useful statement
//! than "these are the configurations, and here is why none of them can be selected right now".
//!
//! Nothing in this module grants a permission. The library brokers approvals and never answers
//! one on a vendor's behalf: with no [`PermissionBroker`] every request reaches the host as an
//! event, and with one the host's own policy decides.

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::event::ActivityKind;
use crate::normalize::{self, APPROVAL_MAX_OPTIONS, TextLimit};

/// What the agent is allowed to do. One of the two axes.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionLevel {
    /// It may read and answer, and nothing else.
    ReadOnly,
    /// It may act, asking first for anything that changes the machine.
    Default,
    /// It may act without asking.
    FullAccess,
}

impl PermissionLevel {
    /// Every level, in increasing order of freedom.
    pub const ALL: [Self; 3] = [Self::ReadOnly, Self::Default, Self::FullAccess];

    /// The most restrictive level.
    ///
    /// What an unrecognised stored value must read as. The failure worth preventing is a
    /// downgraded or corrupted setting silently granting an agent more freedom than anyone chose.
    pub const RESTRICTIVE: Self = Self::ReadOnly;
}

/// Who answers the agent's approval prompts. The second axis.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalRouting {
    /// A person.
    User,
    /// Something other than a person: the host's own policy, a reviewing agent.
    AutoReview,
}

impl ApprovalRouting {
    /// Every routing.
    pub const ALL: [Self; 2] = [Self::User, Self::AutoReview];

    /// The routing that always asks a person.
    ///
    /// What an unrecognised stored value must read as, for the same reason as
    /// [`PermissionLevel::RESTRICTIVE`].
    pub const RESTRICTIVE: Self = Self::User;
}

/// Why one (level, routing) pair cannot be selected.
///
/// A reason enum, never an i18n key: the library does not know the host's copy, and a host that
/// received a key would be rendering a string this crate had chosen for it.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum UnsupportedReason {
    /// The vendor has no equivalent of this pair.
    NotOfferedByVendor,
    /// The vendor offers it only on a plan this account does not have.
    RequiresAccountUpgrade,
    /// The installed CLI is too old to offer it.
    RequiresNewerVersion,
    /// The vendor refuses to run unattended in this shape.
    UnattendedNotPermitted,
    /// Something only this harness can explain, in its own words.
    Other(String),
}

impl fmt::Debug for UnsupportedReason {
    /// Names the reason class without logging a harness-provided explanation.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotOfferedByVendor => "NotOfferedByVendor",
            Self::RequiresAccountUpgrade => "RequiresAccountUpgrade",
            Self::RequiresNewerVersion => "RequiresNewerVersion",
            Self::UnattendedNotPermitted => "UnattendedNotPermitted",
            Self::Other(_) => "Other",
        })
    }
}

/// One cell's verdict, as the harness that would run it sees the pair.
#[derive(Clone, PartialEq, Eq)]
pub enum ConfigurationVerdict {
    /// The pair works, optionally under the vendor's own name for it.
    Supported {
        /// The vendor's id for this combination, when it has one.
        vendor_id: Option<String>,
    },
    /// The pair does not work, and why.
    Unsupported {
        /// Why not.
        reason: UnsupportedReason,
        /// The vendor's id for this combination, when it has one anyway.
        vendor_id: Option<String>,
    },
}

impl fmt::Debug for ConfigurationVerdict {
    /// Shows whether a vendor configuration was named without logging its opaque identifier.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Supported { vendor_id } => formatter
                .debug_struct("Supported")
                .field("has_vendor_id", &vendor_id.is_some())
                .finish(),
            Self::Unsupported { reason, vendor_id } => formatter
                .debug_struct("Unsupported")
                .field("reason", reason)
                .field("has_vendor_id", &vendor_id.is_some())
                .finish(),
        }
    }
}

impl ConfigurationVerdict {
    /// A supported pair with no vendor name.
    pub fn supported() -> Self {
        Self::Supported { vendor_id: None }
    }

    /// An unsupported pair.
    pub fn unsupported(reason: UnsupportedReason) -> Self {
        Self::Unsupported {
            reason,
            vendor_id: None,
        }
    }
}

/// One (level, routing) pair, as vetted by the harness that would run it.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportedConfiguration {
    /// What the agent may do.
    pub level: PermissionLevel,
    /// Who answers its prompts.
    pub routing: ApprovalRouting,
    /// Whether this harness can actually run it.
    pub supported: bool,
    /// Why not, when it cannot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsupported_reason: Option<UnsupportedReason>,
    /// The vendor's own id for this combination, when discovered rather than declared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<String>,
    /// True when choosing this lets the agent act without a person in the loop.
    pub unattended: bool,
}

impl fmt::Debug for SupportedConfiguration {
    /// Shows permission support without logging a vendor-provided configuration identifier.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupportedConfiguration")
            .field("level", &self.level)
            .field("routing", &self.routing)
            .field("supported", &self.supported)
            .field("unsupported_reason", &self.unsupported_reason)
            .field("has_vendor_id", &self.vendor_id.is_some())
            .field("unattended", &self.unattended)
            .finish()
    }
}

/// The whole two-by-three product matrix, built once.
///
/// `unattended` is owned here rather than by the harness, because it is a statement about the
/// product axes and not about a vendor: either the level lets the agent act without asking, or the
/// routing means something other than a person is answering. A harness that could set it
/// independently could describe full access as attended.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PermissionMatrix(Vec<SupportedConfiguration>);

impl PermissionMatrix {
    /// Builds the matrix from a per-cell verdict.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::permission::{
    ///     ApprovalRouting, ConfigurationVerdict, PermissionLevel, PermissionMatrix,
    ///     UnsupportedReason,
    /// };
    ///
    /// let matrix = PermissionMatrix::build(|level, routing| match (level, routing) {
    ///     (_, ApprovalRouting::AutoReview) => {
    ///         ConfigurationVerdict::unsupported(UnsupportedReason::NotOfferedByVendor)
    ///     }
    ///     (PermissionLevel::ReadOnly, _) => ConfigurationVerdict::supported(),
    ///     _ => ConfigurationVerdict::supported(),
    /// });
    ///
    /// assert_eq!(matrix.cells().len(), 6);
    /// assert!(matrix.supports(PermissionLevel::ReadOnly, ApprovalRouting::User));
    /// assert!(!matrix.supports(PermissionLevel::ReadOnly, ApprovalRouting::AutoReview));
    /// ```
    pub fn build(
        mut verdict_for: impl FnMut(PermissionLevel, ApprovalRouting) -> ConfigurationVerdict,
    ) -> Self {
        let mut cells = Vec::with_capacity(PermissionLevel::ALL.len() * ApprovalRouting::ALL.len());
        for level in PermissionLevel::ALL {
            for routing in ApprovalRouting::ALL {
                let unattended =
                    level == PermissionLevel::FullAccess || routing == ApprovalRouting::AutoReview;
                let cell = match verdict_for(level, routing) {
                    ConfigurationVerdict::Supported { vendor_id } => SupportedConfiguration {
                        level,
                        routing,
                        supported: true,
                        unsupported_reason: None,
                        vendor_id,
                        unattended,
                    },
                    ConfigurationVerdict::Unsupported { reason, vendor_id } => {
                        SupportedConfiguration {
                            level,
                            routing,
                            supported: false,
                            unsupported_reason: Some(reason),
                            vendor_id,
                            unattended,
                        }
                    }
                };
                cells.push(cell);
            }
        }
        Self(cells)
    }

    /// A matrix where nothing is supported, for the same reason everywhere.
    pub fn none(reason: UnsupportedReason) -> Self {
        Self::build(|_, _| ConfigurationVerdict::unsupported(reason.clone()))
    }

    /// Every cell, in level-then-routing order.
    pub fn cells(&self) -> &[SupportedConfiguration] {
        &self.0
    }

    /// One cell.
    pub fn cell(
        &self,
        level: PermissionLevel,
        routing: ApprovalRouting,
    ) -> Option<&SupportedConfiguration> {
        self.0
            .iter()
            .find(|cell| cell.level == level && cell.routing == routing)
    }

    /// Whether this pair can be run.
    pub fn supports(&self, level: PermissionLevel, routing: ApprovalRouting) -> bool {
        self.cell(level, routing).is_some_and(|cell| cell.supported)
    }

    /// Keeps only the pairs this harness declared, while preserving probe-time refusals.
    ///
    /// The declaration is the upper bound. A probe can discover that an account, an administrator
    /// policy, or an older build removes a pair; it cannot make an undeclared pair available.
    #[must_use]
    pub fn bounded_by(&self, declaration: &Self) -> Self {
        Self::build(|level, routing| {
            let Some(declared) = declaration.cell(level, routing) else {
                return ConfigurationVerdict::unsupported(UnsupportedReason::Other(String::from(
                    "the declaration did not describe this permission configuration",
                )));
            };
            if !declared.supported {
                return ConfigurationVerdict::Unsupported {
                    reason: declared
                        .unsupported_reason
                        .clone()
                        .unwrap_or(UnsupportedReason::NotOfferedByVendor),
                    vendor_id: declared.vendor_id.clone(),
                };
            }

            let Some(probed) = self.cell(level, routing) else {
                return ConfigurationVerdict::unsupported(UnsupportedReason::Other(String::from(
                    "the probe did not describe this permission configuration",
                )));
            };
            if probed.supported {
                return ConfigurationVerdict::Supported {
                    vendor_id: probed
                        .vendor_id
                        .clone()
                        .or_else(|| declared.vendor_id.clone()),
                };
            }
            ConfigurationVerdict::Unsupported {
                reason: probed
                    .unsupported_reason
                    .clone()
                    .unwrap_or(UnsupportedReason::NotOfferedByVendor),
                vendor_id: probed
                    .vendor_id
                    .clone()
                    .or_else(|| declared.vendor_id.clone()),
            }
        })
        .normalized()
    }

    /// This matrix with vendor-written ids and explanations bounded.
    ///
    /// Permission declarations are static harness facts, but a probe can narrow one with a
    /// vendor-provided configuration id or policy explanation. Those values reach a host's picker
    /// and diagnostics, so ids are dropped unless they survive intact and explanations are bounded
    /// as detail text.
    pub(crate) fn normalized(&self) -> Self {
        Self::build(|level, routing| {
            let Some(configuration) = self.cell(level, routing) else {
                return ConfigurationVerdict::unsupported(UnsupportedReason::Other(String::from(
                    "the matrix did not describe this permission configuration",
                )));
            };
            let vendor_id = configuration
                .vendor_id
                .as_deref()
                .and_then(|id| normalize::opaque_id(id, "permission configuration vendor id").ok());
            if configuration.supported {
                return ConfigurationVerdict::Supported { vendor_id };
            }
            ConfigurationVerdict::Unsupported {
                reason: configuration
                    .unsupported_reason
                    .clone()
                    .unwrap_or(UnsupportedReason::NotOfferedByVendor)
                    .normalized(),
                vendor_id,
            }
        })
    }
}

impl UnsupportedReason {
    /// This reason with its vendor-written explanation bounded.
    fn normalized(self) -> Self {
        match self {
            Self::Other(reason) => {
                Self::Other(normalize::bound_text(&reason, TextLimit::Detail).text)
            }
            reason => reason,
        }
    }
}

/// What one approval option means, whoever the vendor is.
///
/// Without this a broker would be undecidable: "allow" has to become one of the vendor's own
/// option ids, and the vendor's label is text in whatever language it chose. Every dialect the
/// library drives supplies the distinction — Codex's approved/denied/abort, ACP's
/// allow-once/reject-once and their always variants — so a harness maps it rather than guessing.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PermissionOptionKind {
    /// Allow this one thing.
    AllowOnce,
    /// Allow this and anything like it for the rest of the session.
    AllowAlways,
    /// Refuse this one thing; the turn goes on.
    RejectOnce,
    /// Refuse this and anything like it for the rest of the session.
    RejectAlways,
    /// Something else the vendor offered, which only a person can weigh.
    Other,
}

impl PermissionOptionKind {
    /// Whether choosing this lets the agent proceed.
    pub const fn allows(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }

    /// Whether choosing this refuses.
    pub const fn rejects(self) -> bool {
        matches!(self, Self::RejectOnce | Self::RejectAlways)
    }
}

/// One choice the vendor offered.
///
/// The option set is passed through untouched: the library never adds, removes, reorders or
/// renames a choice.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionOption {
    /// The vendor's own id, echoed back verbatim when this option is chosen.
    pub id: String,
    /// What it means, so a policy can answer without reading a label.
    pub kind: PermissionOptionKind,
    /// The vendor's own label, when it supplied one. Rendered as plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Whether the vendor marked this choice destructive.
    #[serde(default)]
    pub destructive: bool,
}

impl fmt::Debug for PermissionOption {
    /// Shows an option's policy-relevant shape without logging opaque ids or vendor copy.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionOption")
            .field("kind", &self.kind)
            .field("has_label", &self.label.is_some())
            .field("destructive", &self.destructive)
            .finish()
    }
}

impl PermissionOption {
    /// An option with no label.
    pub fn new(id: impl Into<String>, kind: PermissionOptionKind) -> Self {
        Self {
            id: id.into(),
            kind,
            label: None,
            destructive: false,
        }
    }

    /// Carries the vendor's own label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Marks the option destructive.
    #[must_use]
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }
}

/// The vendor is asking whether it may do something.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRequest {
    /// The vendor's own id for this question, echoed back with the answer.
    pub id: String,
    /// What kind of thing is being asked about.
    pub kind: ActivityKind,
    /// A one-line summary of what it wants to do.
    pub title: String,
    /// The specifics: the command, the diff, the server and tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The choices, exactly as the vendor offered them.
    pub options: Vec<PermissionOption>,
    /// When this question stops being answerable.
    ///
    /// The host and harness share this deadline. Harnesses use the core's
    /// [`ApprovalDeadline`](crate::approval::ApprovalDeadline) to bound broker deliberation and
    /// host response time, resolving unanswered requests with [`DecisionSource::Expired`].
    pub expires_at: SystemTime,
    /// True when any field above was cut to fit its bound.
    #[serde(default)]
    pub truncated: bool,
}

impl fmt::Debug for PermissionRequest {
    /// Reports a question's shape without logging its text, options, or opaque identifiers.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionRequest")
            .field("kind", &self.kind)
            .field("has_title", &!self.title.is_empty())
            .field("has_detail", &self.detail.is_some())
            .field("option_count", &self.options.len())
            .field("expires_at", &self.expires_at)
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl PermissionRequest {
    /// The answer that lets the vendor proceed.
    ///
    /// Prefers the narrow choice: a one-time allow over a standing one, because a standing grant
    /// is a decision about every future request and not just this one.
    ///
    /// The source is [`DecisionSource::User`], the common case for a host rendering a prompt; a
    /// policy answering on its own marks the response with
    /// [`PermissionResponse::with_source`].
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when the vendor offered no option that allows. A host facing this has
    /// to ask a person, because there is nothing to answer with.
    pub fn allow(&self) -> Result<PermissionResponse> {
        self.respond_with(
            PermissionOptionKind::AllowOnce,
            PermissionOptionKind::AllowAlways,
            "an option that allows",
        )
    }

    /// The answer that refuses.
    ///
    /// Prefers the narrow choice, for the same reason as [`PermissionRequest::allow`].
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when the vendor offered no option that refuses.
    pub fn deny(&self) -> Result<PermissionResponse> {
        self.respond_with(
            PermissionOptionKind::RejectOnce,
            PermissionOptionKind::RejectAlways,
            "an option that refuses",
        )
    }

    /// The answer naming one of this request's own options.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when `option_id` is not one of the options the vendor offered. A host
    /// cannot invent a choice: the vendor would refuse it, or worse, accept a different one.
    pub fn respond(&self, option_id: &str, source: DecisionSource) -> Result<PermissionResponse> {
        if !self.options.iter().any(|option| option.id == option_id) {
            return Err(Error::Protocol {
                // The count, never the ids: an option id is the vendor's own string, and
                // `Display` writes this shape verbatim. A host that wants the ids reads
                // `option_ids` off the request it already holds.
                expected: format!(
                    "one of the {} options this request offered",
                    self.option_ids().len()
                ),
                received: option_id.to_owned(),
            });
        }
        Ok(PermissionResponse {
            request_id: self.id.clone(),
            option_id: option_id.to_owned(),
            source,
        })
    }

    /// This request with every vendor-supplied value bounded.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidVendorValue`] when the request id or an option id does not survive
    /// bounding, [`Error::Protocol`] when the vendor offered no options, and
    /// [`Error::LimitExceeded`] when it offered more than [`APPROVAL_MAX_OPTIONS`]. A request
    /// nobody can render is refused on its own rather than ending the turn it belongs to.
    pub fn normalized(self) -> Result<Self> {
        if self.options.is_empty() {
            return Err(Error::Protocol {
                expected: String::from("at least one approval option"),
                received: String::from("0"),
            });
        }
        if self.options.len() > APPROVAL_MAX_OPTIONS {
            // `LimitExceeded` rather than `Protocol`: its `received` is a count the library
            // itself computed, not raw vendor text, so unlike `Protocol.received` it is safe to
            // render and is what tells an operator how far over the cap the vendor went.
            return Err(Error::LimitExceeded {
                subject: "approval options offered",
                limit: APPROVAL_MAX_OPTIONS,
                received: self.options.len(),
            });
        }

        let title = normalize::bound_text(&self.title, TextLimit::Title);
        let detail = self
            .detail
            .map(|detail| normalize::bound_text(&detail, TextLimit::Detail));
        let detail_truncated = detail.as_ref().is_some_and(|detail| detail.truncated);
        let mut options = Vec::with_capacity(self.options.len());
        let mut options_truncated = false;
        for option in self.options {
            let label = option
                .label
                .map(|label| normalize::bound_text(&label, TextLimit::ApprovalOptionLabel));
            options_truncated |= label.as_ref().is_some_and(|label| label.truncated);
            options.push(PermissionOption {
                id: normalize::opaque_id(&option.id, "approval option id")?,
                kind: option.kind,
                label: label.map(|label| label.text),
                destructive: option.destructive,
            });
        }

        Ok(Self {
            id: normalize::opaque_id(&self.id, "approval request id")?,
            kind: self.kind,
            title: title.text,
            detail: detail.map(|detail| detail.text),
            options,
            expires_at: self.expires_at,
            truncated: self.truncated || title.truncated || detail_truncated || options_truncated,
        })
    }

    fn option_ids(&self) -> Vec<&str> {
        self.options
            .iter()
            .map(|option| option.id.as_str())
            .collect()
    }

    /// The narrow choice when the vendor offered one, the standing choice otherwise.
    ///
    /// A standing grant is a decision about every future request and not just this one, so it is
    /// never preferred over a one-time answer that says exactly as much as it means.
    fn respond_with(
        &self,
        narrow: PermissionOptionKind,
        standing: PermissionOptionKind,
        expected: &str,
    ) -> Result<PermissionResponse> {
        let chosen = self
            .options
            .iter()
            .find(|option| option.kind == narrow)
            .or_else(|| self.options.iter().find(|option| option.kind == standing))
            .ok_or_else(|| Error::Protocol {
                expected: expected.to_owned(),
                received: format!("{:?}", self.option_kinds()),
            })?;
        Ok(PermissionResponse {
            request_id: self.id.clone(),
            option_id: chosen.id.clone(),
            source: DecisionSource::User,
        })
    }

    fn option_kinds(&self) -> Vec<PermissionOptionKind> {
        self.options.iter().map(|option| option.kind).collect()
    }
}

/// How an approval was answered, for the audit trail a host shows.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum DecisionSource {
    /// A person chose.
    User,
    /// The host's own policy chose.
    AutoReview,
    /// Nobody chose in time, and the deadline on the request passed.
    Expired,
    /// The turn ended before anyone chose.
    Cancelled,
}

/// The answer to one approval.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionResponse {
    /// Which question this answers.
    pub request_id: String,
    /// Which of its options was chosen, by the vendor's own id.
    pub option_id: String,
    /// How the answer was reached.
    pub source: DecisionSource,
}

impl fmt::Debug for PermissionResponse {
    /// Shows how an approval was answered without logging opaque question or option ids.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionResponse")
            .field("has_request_id", &true)
            .field("has_option_id", &true)
            .field("source", &self.source)
            .finish()
    }
}

impl PermissionResponse {
    /// Records that a person chose this option.
    pub fn from_user(request_id: impl Into<String>, option_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            option_id: option_id.into(),
            source: DecisionSource::User,
        }
    }

    /// Records how the answer was really reached.
    ///
    /// [`PermissionRequest::allow`] and [`PermissionRequest::deny`] assume a person, because that
    /// is what a host rendering a prompt has; a policy answering on its own says so here.
    #[must_use]
    pub fn with_source(mut self, source: DecisionSource) -> Self {
        self.source = source;
        self
    }
}

/// What was decided, once it was.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecision {
    /// Which option won.
    pub option_id: String,
    /// How it was reached.
    pub source: DecisionSource,
}

impl fmt::Debug for ApprovalDecision {
    /// Shows decision provenance without logging its opaque option id.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApprovalDecision")
            .field("has_option_id", &true)
            .field("source", &self.source)
            .finish()
    }
}

/// A host policy that can answer approvals without asking a person.
///
/// Optional, and `Ask` by default: with no broker, every request reaches the host as an
/// [`EventKind::ApprovalRequested`](crate::EventKind::ApprovalRequested) event and the host answers
/// through [`Session::respond`](crate::Session::respond). Nothing in the library grants a
/// permission on its own.
#[async_trait::async_trait]
pub trait PermissionBroker: Send + Sync {
    /// What to do about one request.
    async fn decide(&self, request: &PermissionRequest) -> BrokerDecision;
}

/// A broker's answer.
#[derive(Clone, PartialEq, Eq)]
pub enum BrokerDecision {
    /// Put it to the host; the event is emitted and the turn waits.
    Ask,
    /// Let the vendor proceed.
    Allow,
    /// Refuse.
    Deny {
        /// Why, for the host's own audit trail. Never sent to the vendor.
        reason: String,
    },
}

impl fmt::Debug for BrokerDecision {
    /// Shows the broker decision without logging an audit reason supplied by the host.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ask => "Ask",
            Self::Allow => "Allow",
            Self::Deny { .. } => "Deny",
        })
    }
}

/// What a broker decided, as an answer the vendor will accept.
///
/// `Ok(None)` means the request has to reach the host: there was no broker, the broker said
/// [`BrokerDecision::Ask`], or the vendor offered nothing matching what the broker decided. The
/// last case is deliberately not an error — a policy that cannot be applied to this question is a
/// question for a person, not a failed turn.
pub async fn broker_response(
    broker: Option<&Arc<dyn PermissionBroker>>,
    request: &PermissionRequest,
) -> Option<PermissionResponse> {
    let decision = broker?.decide(request).await;
    let response = match decision {
        BrokerDecision::Ask => None,
        BrokerDecision::Allow => request.allow().ok(),
        BrokerDecision::Deny { .. } => request.deny().ok(),
    };
    response.map(|response| response.with_source(DecisionSource::AutoReview))
}

#[cfg(test)]
mod tests {
    use super::{
        ApprovalRouting, BrokerDecision, ConfigurationVerdict, DecisionSource, PermissionBroker,
        PermissionLevel, PermissionMatrix, PermissionOption, PermissionOptionKind,
        PermissionRequest, PermissionResponse, UnsupportedReason, broker_response,
    };
    use crate::error::Error;
    use crate::event::ActivityKind;
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    /// A broker that answers the same way every time and remembers what it was asked.
    struct FixedBroker {
        decision: BrokerDecision,
    }

    #[async_trait::async_trait]
    impl PermissionBroker for FixedBroker {
        async fn decide(&self, _request: &PermissionRequest) -> BrokerDecision {
            self.decision.clone()
        }
    }

    fn request(options: Vec<PermissionOption>) -> PermissionRequest {
        PermissionRequest {
            id: String::from("req-1"),
            kind: ActivityKind::Command,
            title: String::from("Run `rm -rf build`"),
            detail: None,
            options,
            expires_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            truncated: false,
        }
    }

    /// `Error::Protocol` writes its expected shape verbatim, so this one counts rather than lists.
    ///
    /// An option id is the vendor's own string. Naming the count still tells a host what it got
    /// wrong — it answered with an id this request never offered — without putting the vendor's
    /// vocabulary into a log line.
    #[test]
    fn an_unoffered_answer_counts_the_options_rather_than_naming_them() {
        let request = request(vec![
            PermissionOption::new("option-id-secret", PermissionOptionKind::AllowOnce),
            PermissionOption::new("other-id-secret", PermissionOptionKind::RejectOnce),
        ]);

        let error = request
            .respond("answer-id-secret", DecisionSource::User)
            .expect_err("expected an unoffered id to be refused");
        let rendered = error.to_string();

        assert!(
            rendered.contains("one of the 2 options this request offered"),
            "expected the option count, received {rendered}"
        );
        for secret in ["option-id-secret", "other-id-secret", "answer-id-secret"] {
            assert!(
                !rendered.contains(secret),
                "expected no option id in the diagnostic, received {rendered}"
            );
        }
    }

    #[test]
    fn question_and_answer_debug_omit_vendor_and_host_text() {
        let question = PermissionRequest {
            id: String::from("question-id-secret"),
            kind: ActivityKind::Command,
            title: String::from("question-title-secret"),
            detail: Some(String::from("question-detail-secret")),
            options: vec![
                PermissionOption::new("option-id-secret", PermissionOptionKind::AllowOnce)
                    .with_label("option-label-secret"),
            ],
            expires_at: SystemTime::UNIX_EPOCH,
            truncated: false,
        };
        let answer = PermissionResponse::from_user("question-id-secret", "option-id-secret");
        let decision = BrokerDecision::Deny {
            reason: String::from("broker-reason-secret"),
        };

        for rendered in [
            format!("{question:?}"),
            format!("{answer:?}"),
            format!("{decision:?}"),
        ] {
            for secret in [
                "question-id-secret",
                "question-title-secret",
                "question-detail-secret",
                "option-id-secret",
                "option-label-secret",
                "broker-reason-secret",
            ] {
                assert!(
                    !rendered.contains(secret),
                    "expected no question or answer payload, received {rendered}"
                );
            }
        }
    }

    fn four_options() -> Vec<PermissionOption> {
        vec![
            PermissionOption::new("always", PermissionOptionKind::AllowAlways),
            PermissionOption::new("once", PermissionOptionKind::AllowOnce),
            PermissionOption::new("never", PermissionOptionKind::RejectAlways),
            PermissionOption::new("no", PermissionOptionKind::RejectOnce),
        ]
    }

    #[test]
    fn the_matrix_is_always_six_cells_with_a_reason_on_every_refusal() {
        let matrix = PermissionMatrix::build(|level, routing| match (level, routing) {
            (PermissionLevel::FullAccess, _) => {
                ConfigurationVerdict::unsupported(UnsupportedReason::RequiresAccountUpgrade)
            }
            (_, ApprovalRouting::AutoReview) => {
                ConfigurationVerdict::unsupported(UnsupportedReason::NotOfferedByVendor)
            }
            _ => ConfigurationVerdict::supported(),
        });

        assert_eq!(matrix.cells().len(), 6);
        for cell in matrix.cells() {
            assert_eq!(
                cell.supported,
                cell.unsupported_reason.is_none(),
                "expected a reason exactly when unsupported, received {cell:?}"
            );
        }
    }

    #[test]
    fn a_probe_can_narrow_a_declared_cell_but_cannot_widen_one() {
        let declaration = PermissionMatrix::build(|level, routing| {
            if level == PermissionLevel::FullAccess && routing == ApprovalRouting::AutoReview {
                return ConfigurationVerdict::unsupported(
                    UnsupportedReason::UnattendedNotPermitted,
                );
            }
            ConfigurationVerdict::supported()
        });
        let probe = PermissionMatrix::build(|level, routing| {
            if level == PermissionLevel::ReadOnly && routing == ApprovalRouting::User {
                return ConfigurationVerdict::unsupported(UnsupportedReason::RequiresNewerVersion);
            }
            ConfigurationVerdict::supported()
        });

        let bounded = probe.bounded_by(&declaration);

        assert!(
            !bounded.supports(PermissionLevel::FullAccess, ApprovalRouting::AutoReview),
            "a probe must not widen a declared refusal"
        );
        assert!(
            !bounded.supports(PermissionLevel::ReadOnly, ApprovalRouting::User),
            "a probe's narrower reading must reach the host"
        );
    }

    #[test]
    fn bounding_a_probe_drops_unsafe_vendor_ids_and_bounds_its_explanation() {
        let declaration = PermissionMatrix::build(|_, _| ConfigurationVerdict::supported());
        let probe = PermissionMatrix::build(|level, routing| {
            if level == PermissionLevel::ReadOnly && routing == ApprovalRouting::User {
                return ConfigurationVerdict::Unsupported {
                    reason: UnsupportedReason::Other(format!(
                        "policy{}\u{202e}",
                        "x".repeat(5_000)
                    )),
                    vendor_id: Some("v".repeat(129)),
                };
            }
            ConfigurationVerdict::Supported {
                vendor_id: Some(String::from("safe-mode")),
            }
        });

        let bounded = probe.bounded_by(&declaration);
        let refused = bounded
            .cell(PermissionLevel::ReadOnly, ApprovalRouting::User)
            .expect("expected the read-only user cell");

        assert_eq!(
            refused.vendor_id, None,
            "expected an over-long opaque vendor id to be refused rather than copied"
        );
        assert!(
            matches!(
                refused.unsupported_reason,
                Some(UnsupportedReason::Other(ref reason))
                    if reason.chars().count() == 4_096 && !reason.contains('\u{202e}')
            ),
            "expected the vendor explanation to be stripped and bounded, received {refused:?}"
        );
        assert_eq!(
            bounded
                .cell(PermissionLevel::Default, ApprovalRouting::User)
                .and_then(|cell| cell.vendor_id.as_deref()),
            Some("safe-mode"),
            "expected a safe opaque id to remain exact"
        );
    }

    #[test]
    fn unattended_is_decided_by_the_axes_and_not_by_the_harness() {
        let matrix = PermissionMatrix::build(|_, _| ConfigurationVerdict::supported());

        for cell in matrix.cells() {
            let expected = cell.level == PermissionLevel::FullAccess
                || cell.routing == ApprovalRouting::AutoReview;
            assert_eq!(
                cell.unattended, expected,
                "expected unattended={expected} for {cell:?}"
            );
        }
    }

    #[test]
    fn a_matrix_that_supports_nothing_still_answers_for_every_pair() {
        let matrix = PermissionMatrix::none(UnsupportedReason::RequiresNewerVersion);

        assert_eq!(matrix.cells().len(), 6);
        assert!(!matrix.supports(PermissionLevel::ReadOnly, ApprovalRouting::User));
        assert_eq!(
            matrix
                .cell(PermissionLevel::ReadOnly, ApprovalRouting::User)
                .and_then(|cell| cell.unsupported_reason.clone()),
            Some(UnsupportedReason::RequiresNewerVersion)
        );
    }

    #[test]
    fn the_restrictive_defaults_are_the_narrow_ones() {
        assert_eq!(PermissionLevel::RESTRICTIVE, PermissionLevel::ReadOnly);
        assert_eq!(ApprovalRouting::RESTRICTIVE, ApprovalRouting::User);
    }

    #[test]
    fn allow_and_deny_prefer_the_one_time_choice_over_the_standing_one() {
        let request = request(four_options());

        assert_eq!(
            request.allow().expect("expected an allow").option_id,
            "once"
        );
        assert_eq!(request.deny().expect("expected a deny").option_id, "no");
    }

    #[test]
    fn allow_falls_back_to_the_standing_choice_when_it_is_the_only_one() {
        let request = request(vec![
            PermissionOption::new("always", PermissionOptionKind::AllowAlways),
            PermissionOption::new("never", PermissionOptionKind::RejectAlways),
        ]);

        assert_eq!(
            request.allow().expect("expected an allow").option_id,
            "always"
        );
        assert_eq!(request.deny().expect("expected a deny").option_id, "never");
    }

    #[test]
    fn a_request_with_no_matching_option_refuses_to_invent_one() {
        let request = request(vec![PermissionOption::new(
            "tell-me-more",
            PermissionOptionKind::Other,
        )]);

        let error = request
            .allow()
            .expect_err("expected a refusal, received an answer");
        assert!(
            matches!(error, Error::Protocol { .. }),
            "expected a protocol refusal, received {error:?}"
        );
        assert!(request.deny().is_err());
    }

    #[test]
    fn a_host_cannot_answer_with_an_option_the_vendor_never_offered() {
        let request = request(four_options());

        assert_eq!(
            request
                .respond("once", DecisionSource::User)
                .expect("expected an answer")
                .source,
            DecisionSource::User
        );
        let error = request
            .respond("invented", DecisionSource::User)
            .expect_err("expected a refusal, received an answer");
        assert!(
            matches!(error, Error::Protocol { .. }),
            "expected a protocol refusal, received {error:?}"
        );
    }

    #[test]
    fn a_request_with_no_options_or_too_many_is_refused_on_its_own() {
        assert!(request(Vec::new()).normalized().is_err());

        let many = (0..17)
            .map(|index| {
                PermissionOption::new(format!("option-{index}"), PermissionOptionKind::AllowOnce)
            })
            .collect();
        assert!(request(many).normalized().is_err());
    }

    #[test]
    fn an_empty_request_and_an_oversized_one_report_differently() {
        let empty = request(Vec::new())
            .normalized()
            .expect_err("expected a refusal");
        assert!(
            matches!(empty, Error::Protocol { .. }),
            "expected a protocol refusal, received {empty:?}"
        );

        let many = (0..17)
            .map(|index| {
                PermissionOption::new(format!("option-{index}"), PermissionOptionKind::AllowOnce)
            })
            .collect();
        let oversized = request(many).normalized().expect_err("expected a refusal");
        assert!(
            matches!(oversized, Error::LimitExceeded { .. }),
            "expected a limit refusal, received {oversized:?}"
        );
        assert_eq!(
            oversized.to_string(),
            "expected at most 16 approval options offered, received 17"
        );
    }

    #[test]
    fn normalising_bounds_the_labels_and_refuses_an_unusable_option_id() {
        let normalised = PermissionRequest {
            title: "t".repeat(300),
            options: vec![
                PermissionOption::new("once", PermissionOptionKind::AllowOnce)
                    .with_label("l".repeat(200)),
            ],
            ..request(four_options())
        }
        .normalized()
        .expect("expected a bounded request");

        assert_eq!(normalised.title.chars().count(), 256);
        assert_eq!(
            normalised.options[0]
                .label
                .as_ref()
                .map(|label| label.chars().count()),
            Some(128)
        );
        assert!(normalised.truncated);

        let error = request(vec![PermissionOption::new(
            "o".repeat(129),
            PermissionOptionKind::AllowOnce,
        )])
        .normalized()
        .expect_err("expected a refusal, received a request");
        assert!(
            matches!(
                error,
                Error::InvalidVendorValue {
                    field: "approval option id",
                    ..
                }
            ),
            "expected an invalid option id, received {error:?}"
        );
    }

    /// The flag a host renders as a warning on the choice that deletes something. It survives
    /// normalising, because a bounded label on an unmarked option is a prompt that lost the one
    /// thing it was trying to say.
    #[test]
    fn a_destructive_choice_stays_marked_through_normalising() {
        let marked = PermissionOption::new("wipe", PermissionOptionKind::AllowOnce).destructive();
        assert!(marked.destructive);
        assert!(!PermissionOption::new("ok", PermissionOptionKind::AllowOnce).destructive);

        let normalised = request(vec![marked])
            .normalized()
            .expect("expected a bounded request");
        assert!(
            normalised.options[0].destructive,
            "received {:?}",
            normalised.options[0]
        );
    }

    #[tokio::test]
    async fn no_broker_means_the_request_reaches_the_host() {
        assert_eq!(broker_response(None, &request(four_options())).await, None);
    }

    #[tokio::test]
    async fn a_broker_that_asks_still_sends_the_request_to_the_host() {
        let broker: Arc<dyn PermissionBroker> = Arc::new(FixedBroker {
            decision: BrokerDecision::Ask,
        });
        assert_eq!(
            broker_response(Some(&broker), &request(four_options())).await,
            None
        );
    }

    #[tokio::test]
    async fn a_broker_decision_becomes_one_of_the_vendors_own_options() {
        let allow: Arc<dyn PermissionBroker> = Arc::new(FixedBroker {
            decision: BrokerDecision::Allow,
        });
        let deny: Arc<dyn PermissionBroker> = Arc::new(FixedBroker {
            decision: BrokerDecision::Deny {
                reason: String::from("the workspace is read-only"),
            },
        });

        let allowed = broker_response(Some(&allow), &request(four_options()))
            .await
            .expect("expected an answer");
        assert_eq!(allowed.option_id, "once");
        assert_eq!(
            allowed.source,
            DecisionSource::AutoReview,
            "expected a policy answer to say so, received {:?}",
            allowed.source
        );

        let denied = broker_response(Some(&deny), &request(four_options()))
            .await
            .expect("expected an answer");
        assert_eq!(denied.option_id, "no");
        assert_eq!(denied.source, DecisionSource::AutoReview);
    }

    #[test]
    fn a_host_rendering_a_prompt_records_that_a_person_answered() {
        let request = request(four_options());
        assert_eq!(
            request.allow().expect("expected an allow").source,
            DecisionSource::User
        );
        assert_eq!(
            request
                .deny()
                .expect("expected a deny")
                .with_source(DecisionSource::Expired)
                .source,
            DecisionSource::Expired
        );
    }

    #[tokio::test]
    async fn a_policy_that_cannot_be_applied_becomes_a_question_for_a_person() {
        let allow: Arc<dyn PermissionBroker> = Arc::new(FixedBroker {
            decision: BrokerDecision::Allow,
        });
        let unanswerable = request(vec![PermissionOption::new(
            "tell-me-more",
            PermissionOptionKind::Other,
        )]);

        assert_eq!(broker_response(Some(&allow), &unanswerable).await, None);
    }
}
