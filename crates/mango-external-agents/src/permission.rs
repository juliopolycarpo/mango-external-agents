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
use crate::interaction::{Interaction, InteractionId};
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

/// What choosing an option does to the request in front of it.
///
/// One axis only. How *far* the choice reaches is [`PermissionScope`], and whether it rewrites a
/// standing policy is [`PermissionOption::policy_changing`] — three separate facts, because
/// "always" collapsed all three into one word and a host reading it could not tell an
/// allow-for-this-turn from a rule written into the vendor's configuration file.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PermissionEffect {
    /// The agent may proceed.
    Allow,
    /// The agent may not; the turn goes on.
    Reject,
    /// Something else the vendor offered, which only a person can weigh.
    Other,
}

impl PermissionEffect {
    /// Whether choosing this lets the agent proceed.
    pub const fn allows(self) -> bool {
        matches!(self, Self::Allow)
    }

    /// Whether choosing this refuses.
    pub const fn rejects(self) -> bool {
        matches!(self, Self::Reject)
    }
}

impl fmt::Display for PermissionEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Allow => "allow",
            Self::Reject => "reject",
            Self::Other => "other",
        })
    }
}

/// How far a choice reaches.
///
/// Ordered from narrowest to widest, and the order is load-bearing: every automatic preference in
/// this module picks the smallest scope that does the job, because a wider grant is a decision
/// about requests nobody has seen yet.
///
/// Reported only where the vendor actually exposes it. A harness that cannot tell a session-wide
/// choice from a persistent one leaves the scope absent rather than guessing the narrower reading,
/// which would understate what a person is about to agree to.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PermissionScope {
    /// This request and nothing else.
    Once,
    /// Anything like it for the rest of this turn.
    Turn,
    /// Anything like it for the rest of this session.
    Session,
    /// Anything like it from now on, including sessions nobody has opened yet.
    Persistent,
}

impl PermissionScope {
    /// Every scope, narrowest first.
    pub const ALL: [Self; 4] = [Self::Once, Self::Turn, Self::Session, Self::Persistent];

    /// Whether this choice outlives the request that prompted it.
    pub const fn is_standing(self) -> bool {
        !matches!(self, Self::Once)
    }

    /// Whether this choice outlives the session that prompted it.
    pub const fn outlives_session(self) -> bool {
        matches!(self, Self::Persistent)
    }
}

impl fmt::Display for PermissionScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Once => "once",
            Self::Turn => "this turn",
            Self::Session => "this session",
            Self::Persistent => "from now on",
        })
    }
}

/// What the vendor said about how risky a choice is.
///
/// Reported, never derived. A library that decided for itself which shell command is destructive
/// would be wrong in both directions, and the direction that matters is the one where it labels a
/// `rm -rf` safe.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PermissionRisk {
    /// The vendor did not say.
    #[default]
    Unspecified,
    /// The vendor marked it as changing nothing that cannot be undone.
    Reversible,
    /// The vendor marked it destructive.
    Destructive,
}

impl fmt::Display for PermissionRisk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unspecified => "unspecified",
            Self::Reversible => "reversible",
            Self::Destructive => "destructive",
        })
    }
}

/// One choice the vendor offered.
///
/// The option set is passed through untouched: the library never adds, removes, reorders or
/// renames a choice. What it does add is the three facts a policy needs in order to answer without
/// reading a label written in a language it does not know — [`PermissionEffect`],
/// [`PermissionScope`] and [`PermissionOption::policy_changing`].
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionOption {
    /// The vendor's own id, echoed back verbatim when this option is chosen.
    pub id: String,
    /// What choosing it does.
    pub effect: PermissionEffect,
    /// How far it reaches, when the vendor exposes that.
    ///
    /// Absent means the vendor did not say. It does not mean [`PermissionScope::Once`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<PermissionScope>,
    /// Whether choosing it writes a rule the vendor will apply to later requests on its own.
    ///
    /// Separate from [`PermissionScope`] because the two come apart: a vendor can offer a
    /// session-wide allow that it forgets on exit, and a once-only allow that it records in a
    /// settings file. A host showing "just this once" over the second one would be wrong.
    #[serde(default)]
    pub policy_changing: bool,
    /// What the vendor said about the risk of choosing it.
    #[serde(default)]
    pub risk: PermissionRisk,
    /// The vendor's own label, when it supplied one. Rendered as plain text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl fmt::Debug for PermissionOption {
    /// Shows an option's policy-relevant shape without logging opaque ids or vendor copy.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionOption")
            .field("effect", &self.effect)
            .field("scope", &self.scope)
            .field("policy_changing", &self.policy_changing)
            .field("has_label", &self.label.is_some())
            .field("risk", &self.risk)
            .finish()
    }
}

impl PermissionOption {
    /// An option with no label, no declared scope and no declared risk.
    pub fn new(id: impl Into<String>, effect: PermissionEffect) -> Self {
        Self {
            id: id.into(),
            effect,
            scope: None,
            policy_changing: false,
            risk: PermissionRisk::Unspecified,
            label: None,
        }
    }

    /// Carries the vendor's own label.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Records how far this choice reaches.
    #[must_use]
    pub fn with_scope(mut self, scope: PermissionScope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// Records what the vendor said about the risk.
    #[must_use]
    pub fn with_risk(mut self, risk: PermissionRisk) -> Self {
        self.risk = risk;
        self
    }

    /// Records that choosing this writes a standing rule.
    #[must_use]
    pub fn policy_changing(mut self) -> Self {
        self.policy_changing = true;
        self
    }

    /// Whether choosing this lets the agent proceed.
    pub const fn allows(&self) -> bool {
        self.effect.allows()
    }

    /// Whether choosing this refuses.
    pub const fn rejects(&self) -> bool {
        self.effect.rejects()
    }

    /// Whether the vendor marked it destructive.
    pub const fn is_destructive(&self) -> bool {
        matches!(self.risk, PermissionRisk::Destructive)
    }

    /// Whether choosing this decides anything beyond the request in front of it.
    ///
    /// True for a scope wider than [`PermissionScope::Once`], for anything that writes a standing
    /// rule, and for an option whose scope the vendor never stated. The last is the one worth
    /// spelling out: an unstated reach is a reach nobody measured, and the only answer that cannot
    /// understate what somebody is agreeing to is to treat it as standing.
    pub const fn is_standing(&self) -> bool {
        if self.policy_changing {
            return true;
        }
        match self.scope {
            Some(scope) => scope.is_standing(),
            // Unmeasured, so not known to be narrow.
            None => true,
        }
    }

    /// How much this option decides, for preferring the choice that decides least.
    ///
    /// Two keys, in order:
    ///
    /// 1. **Whether it writes a standing rule.** The loudest signal there is: an option marked
    ///    `policy_changing` will be applied by the vendor to requests nobody has seen yet, whatever
    ///    its scope says.
    /// 2. **The reach it states.** Narrowest first, with an unstated reach sorting after every
    ///    stated one — a policy choosing automatically should reach for the choice whose reach is
    ///    known, and an unstated reach could be any of them.
    fn breadth(&self) -> (u8, u8) {
        let scope = match self.scope {
            Some(PermissionScope::Once) => 0,
            Some(PermissionScope::Turn) => 1,
            Some(PermissionScope::Session) => 2,
            Some(PermissionScope::Persistent) => 3,
            None => 4,
        };
        (u8::from(self.policy_changing), scope)
    }
}

/// The vendor is asking whether it may do something.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PermissionRequest {
    /// The lifecycle fields: id, kind, session, turn, deadline, status.
    ///
    /// Shared with [`QuestionRequest`](crate::QuestionRequest) so a host holds one map of what it
    /// is waiting on. [`InteractionKind::Permission`](crate::InteractionKind) is what marks this
    /// one as the kind whose answer grants authority.
    ///
    /// The host and harness share the deadline it carries. Harnesses use the core's
    /// [`ApprovalDeadline`](crate::approval::ApprovalDeadline) to bound broker deliberation and
    /// host response time, resolving unanswered requests with [`DecisionSource::Expired`].
    pub interaction: Interaction,
    /// What kind of thing is being asked about.
    pub kind: ActivityKind,
    /// A one-line summary of what it wants to do.
    pub title: String,
    /// The specifics: the command, the diff, the server and tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The choices, exactly as the vendor offered them.
    pub options: Vec<PermissionOption>,
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
            .field("expires_at", &self.interaction.expires_at)
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl PermissionRequest {
    /// The vendor is asking about this, with these choices.
    pub fn new(
        interaction: Interaction,
        kind: ActivityKind,
        title: impl Into<String>,
        options: Vec<PermissionOption>,
    ) -> Self {
        Self {
            interaction,
            kind,
            title: title.into(),
            detail: None,
            options,
            truncated: false,
        }
    }

    /// Carries the specifics: the command, the diff, the server and tool.
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The vendor's own id for this question.
    pub fn id(&self) -> &InteractionId {
        &self.interaction.id
    }

    /// When this question stops being answerable.
    pub fn expires_at(&self) -> SystemTime {
        self.interaction.expires_at
    }

    /// The answer that lets the vendor proceed.
    ///
    /// Prefers the narrowest reach: a one-time allow over a session-wide one over anything that
    /// writes a standing rule, because a wider grant decides requests nobody has seen yet.
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
        self.narrowest(PermissionEffect::Allow, "an option that allows")
    }

    /// The answer that refuses.
    ///
    /// Prefers the narrowest reach, for the same reason as [`PermissionRequest::allow`].
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when the vendor offered no option that refuses.
    pub fn deny(&self) -> Result<PermissionResponse> {
        self.narrowest(PermissionEffect::Reject, "an option that refuses")
    }

    /// The answer naming one of this request's own options.
    ///
    /// # Errors
    ///
    /// [`Error::Protocol`] when `option_id` is not one of the options the vendor offered. A host
    /// cannot invent a choice: the vendor would refuse it, or worse, accept a different one.
    pub fn respond(&self, option_id: &str, source: DecisionSource) -> Result<PermissionResponse> {
        let chosen = self
            .options
            .iter()
            .find(|option| option.id == option_id)
            .ok_or_else(|| Error::Protocol {
                // The count, never the ids: an option id is the vendor's own string, and
                // `Display` writes this shape verbatim. A host that wants the ids reads
                // `option_ids` off the request it already holds.
                expected: format!(
                    "one of the {} options this request offered",
                    self.option_ids().len()
                ),
                received: option_id.to_owned(),
            })?;
        Ok(self.answer(chosen, source))
    }

    /// This request with every vendor-supplied value bounded.
    ///
    /// Scope, risk and the policy-changing flag are carried through untouched: they are what a
    /// host renders and audits, and normalisation that widened or dropped one would understate
    /// what a person is agreeing to.
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
                label: label.map(|label| label.text),
                ..option
            });
        }

        Ok(Self {
            interaction: self.interaction.normalized()?,
            kind: self.kind,
            title: title.text,
            detail: detail.map(|detail| detail.text),
            options,
            truncated: self.truncated || title.truncated || detail_truncated || options_truncated,
        })
    }

    fn option_ids(&self) -> Vec<&str> {
        self.options
            .iter()
            .map(|option| option.id.as_str())
            .collect()
    }

    /// The option with this effect that decides least.
    ///
    /// A standing grant is a decision about every future request and not just this one, so it is
    /// never preferred over an answer that says exactly as much as it means. See
    /// [`PermissionOption::breadth`] for the order and why writing a standing rule outranks reach.
    fn narrowest(&self, effect: PermissionEffect, expected: &str) -> Result<PermissionResponse> {
        let chosen = self
            .options
            .iter()
            .filter(|option| option.effect == effect)
            .min_by_key(|option| option.breadth())
            .ok_or_else(|| Error::Protocol {
                expected: expected.to_owned(),
                received: format!("{:?}", self.option_effects()),
            })?;
        Ok(self.answer(chosen, DecisionSource::User))
    }

    /// The response naming one option, carrying the reach it was chosen with.
    fn answer(&self, option: &PermissionOption, source: DecisionSource) -> PermissionResponse {
        PermissionResponse {
            interaction_id: self.interaction.id.clone(),
            option_id: option.id.clone(),
            source,
        }
    }

    fn option_effects(&self) -> Vec<PermissionEffect> {
        self.options.iter().map(|option| option.effect).collect()
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
#[non_exhaustive]
pub struct PermissionResponse {
    /// Which question this answers.
    pub interaction_id: InteractionId,
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
    pub fn from_user(interaction_id: InteractionId, option_id: impl Into<String>) -> Self {
        Self {
            interaction_id,
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
///
/// Carries the reach of the option that won, not just its id. A host's audit trail that recorded
/// only "option `allow_always` was chosen" would have to ask the vendor what that meant, and the
/// request it meant it about is gone by then.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ApprovalDecision {
    /// Which option won.
    pub option_id: String,
    /// What it did.
    pub effect: PermissionEffect,
    /// How far it reached, when the vendor exposed that.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<PermissionScope>,
    /// Whether it wrote a rule the vendor will apply on its own from now on.
    #[serde(default)]
    pub policy_changing: bool,
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

impl ApprovalDecision {
    /// What a chosen option decided.
    ///
    /// Built from the option rather than from its id, so the reach a host audits is the reach the
    /// vendor declared and not one anybody re-derived from a label.
    pub fn from_option(option: &PermissionOption, source: DecisionSource) -> Self {
        Self {
            option_id: option.id.clone(),
            effect: option.effect,
            scope: option.scope,
            policy_changing: option.policy_changing,
            source,
        }
    }

    /// A decision about an option this harness could not describe any further.
    ///
    /// For the paths that resolve a request without anyone choosing — an expiry, a cancelled turn
    /// — where there is no option to read a reach from.
    pub fn unresolved(option_id: impl Into<String>, source: DecisionSource) -> Self {
        Self {
            option_id: option_id.into(),
            effect: PermissionEffect::Other,
            scope: None,
            policy_changing: false,
            source,
        }
    }

    /// Whether this decision reaches past the request that prompted it.
    pub const fn is_standing(&self) -> bool {
        if self.policy_changing {
            return true;
        }
        match self.scope {
            Some(scope) => scope.is_standing(),
            None => false,
        }
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
        ApprovalDecision, ApprovalRouting, BrokerDecision, ConfigurationVerdict, DecisionSource,
        PermissionBroker, PermissionEffect, PermissionLevel, PermissionMatrix, PermissionOption,
        PermissionRequest, PermissionResponse, PermissionRisk, PermissionScope, UnsupportedReason,
        broker_response,
    };
    use crate::error::Error;
    use crate::event::{ActivityKind, SessionId};
    use crate::interaction::{Interaction, InteractionId, InteractionKind};
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
        PermissionRequest::new(
            Interaction::new(
                InteractionId::new("req-1"),
                InteractionKind::Permission,
                SessionId::new("chat-1"),
                SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            ),
            ActivityKind::Command,
            "Run `rm -rf build`",
            options,
        )
    }

    /// `Error::Protocol` writes its expected shape verbatim, so this one counts rather than lists.
    ///
    /// An option id is the vendor's own string. Naming the count still tells a host what it got
    /// wrong — it answered with an id this request never offered — without putting the vendor's
    /// vocabulary into a log line.
    #[test]
    fn an_unoffered_answer_counts_the_options_rather_than_naming_them() {
        let request = request(vec![
            PermissionOption::new("option-id-secret", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once),
            PermissionOption::new("other-id-secret", PermissionEffect::Reject)
                .with_scope(PermissionScope::Once),
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
        let question = PermissionRequest::new(
            Interaction::new(
                InteractionId::new("question-id-secret"),
                InteractionKind::Permission,
                SessionId::new("session-id-secret"),
                SystemTime::UNIX_EPOCH,
            ),
            ActivityKind::Command,
            "question-title-secret",
            vec![
                PermissionOption::new("option-id-secret", PermissionEffect::Allow)
                    .with_scope(PermissionScope::Once)
                    .with_label("option-label-secret"),
            ],
        )
        .with_detail("question-detail-secret");
        let answer = PermissionResponse::from_user(
            InteractionId::new("question-id-secret"),
            "option-id-secret",
        );
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
            PermissionOption::new("always", PermissionEffect::Allow)
                .with_scope(PermissionScope::Session),
            PermissionOption::new("once", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once),
            PermissionOption::new("never", PermissionEffect::Reject)
                .with_scope(PermissionScope::Session),
            PermissionOption::new("no", PermissionEffect::Reject).with_scope(PermissionScope::Once),
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
            PermissionOption::new("always", PermissionEffect::Allow)
                .with_scope(PermissionScope::Session),
            PermissionOption::new("never", PermissionEffect::Reject)
                .with_scope(PermissionScope::Session),
        ]);

        assert_eq!(
            request.allow().expect("expected an allow").option_id,
            "always"
        );
        assert_eq!(request.deny().expect("expected a deny").option_id, "never");
    }

    /// Four scopes, not two. A turn-wide allow is narrower than a session-wide one and wider than
    /// a one-time one, and the old two-way "always" could not say either thing.
    #[test]
    fn the_narrowest_reach_wins_across_all_four_scopes() {
        let request = request(vec![
            PermissionOption::new("forever", PermissionEffect::Allow)
                .with_scope(PermissionScope::Persistent),
            PermissionOption::new("session", PermissionEffect::Allow)
                .with_scope(PermissionScope::Session),
            PermissionOption::new("turn", PermissionEffect::Allow)
                .with_scope(PermissionScope::Turn),
        ]);

        assert_eq!(
            request.allow().expect("expected an allow").option_id,
            "turn",
            "expected the narrowest reach on offer"
        );
    }

    /// A vendor can offer a session-wide allow it forgets on exit, and a once-only allow it writes
    /// into a settings file. Preferring the one that writes nothing is the whole point of keeping
    /// the two flags apart.
    #[test]
    fn a_choice_that_writes_a_standing_rule_loses_to_one_that_does_not() {
        let request = request(vec![
            PermissionOption::new("remember", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once)
                .policy_changing(),
            PermissionOption::new("just-now", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once),
        ]);

        assert_eq!(
            request.allow().expect("expected an allow").option_id,
            "just-now"
        );
    }

    /// An unstated reach is a reach nobody measured, so it must not be read as the narrow one.
    #[test]
    fn an_option_with_no_stated_scope_is_not_preferred_over_a_stated_narrow_one() {
        let request = request(vec![
            PermissionOption::new("unknown", PermissionEffect::Allow),
            PermissionOption::new("once", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once),
        ]);

        assert_eq!(
            request.allow().expect("expected an allow").option_id,
            "once"
        );
        assert!(
            PermissionOption::new("unknown", PermissionEffect::Allow).is_standing(),
            "an unstated reach is a reach nobody measured, so it cannot be reported as narrow"
        );
        assert!(
            PermissionOption::new("session", PermissionEffect::Allow)
                .with_scope(PermissionScope::Session)
                .is_standing()
        );
        assert!(
            !PermissionOption::new("once", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once)
                .is_standing(),
            "expected a stated one-time reach to be the only thing that is not standing"
        );
    }

    /// The case that decides the order of the two keys. A vendor offering an explicit
    /// "always, and remember it" beside a plain "allow" whose reach it never stated must not have
    /// the first one chosen automatically: `policy_changing` is the only flag here that says for
    /// certain the vendor will apply this to requests nobody has seen yet.
    #[test]
    fn a_stated_persistent_rule_loses_to_an_option_whose_reach_was_never_stated() {
        let request = request(vec![
            PermissionOption::new("allow-always", PermissionEffect::Allow)
                .with_scope(PermissionScope::Persistent)
                .policy_changing(),
            PermissionOption::new("allow", PermissionEffect::Allow),
        ]);

        assert_eq!(
            request.allow().expect("expected an allow").option_id,
            "allow",
            "expected the choice that is not known to write a standing rule"
        );
    }

    #[test]
    fn a_request_with_no_matching_option_refuses_to_invent_one() {
        let request = request(vec![PermissionOption::new(
            "tell-me-more",
            PermissionEffect::Other,
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
            .map(|index| PermissionOption::new(format!("option-{index}"), PermissionEffect::Allow))
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
                PermissionOption::new(format!("option-{index}"), PermissionEffect::Allow)
                    .with_scope(PermissionScope::Once)
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
        let mut over_long = request(vec![
            PermissionOption::new("once", PermissionEffect::Allow).with_label("l".repeat(200)),
        ]);
        over_long.title = "t".repeat(300);
        let normalised = over_long.normalized().expect("expected a bounded request");

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
            PermissionEffect::Allow,
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

    /// Scope, risk and the policy-changing flag are what a host renders as the warning on a
    /// choice. Normalising that widened or dropped one would understate what somebody is agreeing
    /// to — the one direction in this whole module that must never happen.
    #[test]
    fn scope_risk_and_policy_effects_survive_normalising_without_widening() {
        let marked = PermissionOption::new("wipe", PermissionEffect::Allow)
            .with_scope(PermissionScope::Persistent)
            .with_risk(PermissionRisk::Destructive)
            .policy_changing()
            .with_label("l".repeat(200));
        assert!(marked.is_destructive());

        let normalised = request(vec![marked])
            .normalized()
            .expect("expected a bounded request");
        let option = &normalised.options[0];

        assert_eq!(option.scope, Some(PermissionScope::Persistent));
        assert_eq!(option.risk, PermissionRisk::Destructive);
        assert!(option.policy_changing);
        assert!(option.is_standing());
        assert_eq!(
            option.label.as_ref().map(|label| label.chars().count()),
            Some(128),
            "expected only the label to have been cut"
        );

        let unmarked = PermissionOption::new("ok", PermissionEffect::Allow);
        assert!(!unmarked.is_destructive());
        assert_eq!(unmarked.risk, PermissionRisk::Unspecified);
        assert!(!unmarked.policy_changing);
    }

    /// The request that prompted a decision is gone by the time anybody audits it, so the decision
    /// has to carry what the chosen option meant rather than just its id.
    #[test]
    fn a_decision_records_the_reach_of_the_option_that_won() {
        let option = PermissionOption::new("always", PermissionEffect::Allow)
            .with_scope(PermissionScope::Session)
            .policy_changing();
        let decision = ApprovalDecision::from_option(&option, DecisionSource::User);

        assert_eq!(decision.option_id, "always");
        assert_eq!(decision.effect, PermissionEffect::Allow);
        assert_eq!(decision.scope, Some(PermissionScope::Session));
        assert!(decision.policy_changing);
        assert!(decision.is_standing());

        let expired = ApprovalDecision::unresolved("none", DecisionSource::Expired);
        assert!(
            !expired.is_standing(),
            "expected a request nobody answered to grant nothing standing"
        );
    }

    #[test]
    fn the_scope_ladder_runs_narrowest_first() {
        assert_eq!(
            PermissionScope::ALL,
            [
                PermissionScope::Once,
                PermissionScope::Turn,
                PermissionScope::Session,
                PermissionScope::Persistent,
            ]
        );
        assert!(!PermissionScope::Once.is_standing());
        assert!(PermissionScope::Turn.is_standing());
        assert!(!PermissionScope::Session.outlives_session());
        assert!(PermissionScope::Persistent.outlives_session());
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
            PermissionEffect::Other,
        )]);

        assert_eq!(broker_response(Some(&allow), &unanswerable).await, None);
    }
}
