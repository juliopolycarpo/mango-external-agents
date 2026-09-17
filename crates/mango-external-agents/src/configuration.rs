//! What a session is set to, what was asked for, and what a vendor said back.
//!
//! Three types that a single struct used to do the work of, because the three answer different
//! questions and a host that cannot tell them apart will show the wrong one:
//!
//! - [`ConfigurationPatch`] is a **request**. Every axis is keep, set or reset, so "leave it
//!   alone" and "remove my override" stop being the same omission.
//! - [`Configuration`] is a **set of values**, each one optional, where absence means *unknown*.
//! - [`ConfigurationState`] is the **current picture**: what the host requested, what the harness
//!   accepted, and what the vendor was actually observed reporting. A model name accepted onto a
//!   command line is not a model the vendor confirmed it loaded, and reporting it as one is how a
//!   picker ends up showing a setting nothing is running under.
//!
//! [`ConfigurationCatalog`] is the open half. Models, reasoning efforts, modes and whatever a
//! vendor invents next are rows with the vendor's own ids and ordering, not arms of an enum this
//! crate would have to grow. A category nobody recognises is carried as
//! [`ConfigurationCategory::Other`] and does not stop the rows beside it from being usable.

use std::collections::BTreeMap;
use std::fmt;

use crate::error::{Error, Result};
use crate::normalize::{self, TextLimit};
use crate::permission::{ApprovalRouting, PermissionLevel};

/// How many options one catalog may carry.
///
/// Sized like the other catalogs in [`normalize`]: a vendor enumerating a few dozen settings is
/// ordinary, and one enumerating thousands has started enumerating something else.
pub const CONFIGURATION_CATALOG_MAX_OPTIONS: usize = 256;

/// How many values one enumerated option may offer.
pub const CONFIGURATION_OPTION_MAX_VALUES: usize = 64;

/// The longest text value a configuration option may carry.
pub const CONFIGURATION_TEXT_MAX_LENGTH: usize = 1_024;

/// What one axis of a [`ConfigurationPatch`] asks for.
///
/// The three-way distinction the old optional field could not make. `None` meant both "do not
/// touch this" and "there is no value", so a host could never clear an override it had set.
///
/// # Example
///
/// ```
/// use mango_external_agents::ConfigurationChange;
///
/// let keep: ConfigurationChange<String> = ConfigurationChange::Keep;
/// let set = ConfigurationChange::Set(String::from("opus"));
/// let reset: ConfigurationChange<String> = ConfigurationChange::Reset;
///
/// assert!(keep.is_keep());
/// assert_eq!(set.set_value(), Some(&String::from("opus")));
/// assert!(reset.is_reset());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", content = "value", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ConfigurationChange<T> {
    /// Leave this axis exactly as it is, override and all.
    #[default]
    Keep,
    /// Run under this value.
    Set(
        /// The value to apply.
        T,
    ),
    /// Remove the host's override and fall back to whatever the vendor's own configuration says.
    ///
    /// Not every vendor can do this. One that cannot refuses with
    /// [`SettingRejection::ResetNotSupported`] rather than reporting a success it did not have.
    Reset,
}

impl<T> ConfigurationChange<T> {
    /// Whether this axis is being left alone.
    pub const fn is_keep(&self) -> bool {
        matches!(self, Self::Keep)
    }

    /// Whether this axis is having its override removed.
    pub const fn is_reset(&self) -> bool {
        matches!(self, Self::Reset)
    }

    /// The value this axis is being set to, when it is being set.
    pub const fn set_value(&self) -> Option<&T> {
        match self {
            Self::Set(value) => Some(value),
            Self::Keep | Self::Reset => None,
        }
    }

    /// Applies this change to a current value.
    ///
    /// The whole of keep/set/reset in one place, so no caller re-derives it.
    #[must_use]
    pub fn applied_to(self, current: Option<T>) -> Option<T> {
        match self {
            Self::Keep => current,
            Self::Set(value) => Some(value),
            Self::Reset => None,
        }
    }

    /// The change that reproduces an optional value: set when present, reset when absent.
    ///
    /// The bridge from a host that stores a settings row as nullable columns.
    pub fn from_option(value: Option<T>) -> Self {
        match value {
            Some(value) => Self::Set(value),
            None => Self::Reset,
        }
    }
}

/// Settings a session is running under, as far as anyone can say.
///
/// Every axis is optional and absence means **unknown**, never a library default. A harness that
/// could not read what the vendor is set to omits the axis rather than reporting this crate's
/// preference, because a permission level nobody chose is the one setting that must never be
/// invented in either direction.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Configuration {
    /// The vendor's own model id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The vendor's own reasoning-effort id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// What the agent may do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<PermissionLevel>,
    /// Who answers its prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<ApprovalRouting>,
    /// Vendor-native options that have no axis of their own, by the vendor's own option id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native: BTreeMap<ConfigurationOptionId, ConfigurationValue>,
}

impl Configuration {
    /// Nothing known and nothing chosen.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::Configuration;
    ///
    /// assert_eq!(Configuration::unknown().level, None);
    /// ```
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Whether nothing at all is known about this configuration.
    pub fn is_unknown(&self) -> bool {
        self == &Self::default()
    }

    /// Records the model this configuration is known to carry.
    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Records the reasoning effort.
    #[must_use]
    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    /// Records what the agent may do.
    #[must_use]
    pub fn with_level(mut self, level: PermissionLevel) -> Self {
        self.level = Some(level);
        self
    }

    /// Records who answers its prompts.
    #[must_use]
    pub fn with_routing(mut self, routing: ApprovalRouting) -> Self {
        self.routing = Some(routing);
        self
    }

    /// Records one vendor-native option's value.
    #[must_use]
    pub fn with_native(mut self, option: ConfigurationOptionId, value: ConfigurationValue) -> Self {
        self.native.insert(option, value);
        self
    }

    /// This configuration with `patch` applied.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{Configuration, ConfigurationChange, ConfigurationPatch};
    ///
    /// let current = Configuration::unknown().with_model("opus");
    /// let cleared = current.patched(&ConfigurationPatch::new().model(ConfigurationChange::Reset));
    /// assert_eq!(cleared.model, None);
    /// ```
    #[must_use]
    pub fn patched(&self, patch: &ConfigurationPatch) -> Self {
        let mut native = self.native.clone();
        for (option, change) in &patch.native {
            match change {
                ConfigurationChange::Keep => {}
                ConfigurationChange::Set(value) => {
                    native.insert(option.clone(), value.clone());
                }
                ConfigurationChange::Reset => {
                    native.remove(option);
                }
            }
        }
        Self {
            model: patch.model.clone().applied_to(self.model.clone()),
            effort: patch.effort.clone().applied_to(self.effort.clone()),
            level: patch.level.applied_to(self.level),
            routing: patch.routing.applied_to(self.routing),
            native,
        }
    }

    /// This configuration with every vendor-written value bounded.
    ///
    /// Ids that cannot survive bounding are dropped rather than cut: a shortened model id names a
    /// model the vendor does not have, and it would be sent straight back as the chosen one. A
    /// native entry is dropped whole when either half cannot be carried — an option id is echoed
    /// back to the vendor exactly like a model id, so it is refused on the same terms.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            model: self
                .model
                .and_then(|model| normalize::opaque_id(&model, "model id").ok()),
            effort: self
                .effort
                .and_then(|effort| normalize::opaque_id(&effort, "reasoning effort id").ok()),
            native: self
                .native
                .into_iter()
                .filter_map(|(option, value)| {
                    let option = ConfigurationOptionId::new(
                        normalize::opaque_id(option.as_str(), "configuration option id").ok()?,
                    );
                    Some((option, value.normalized()?))
                })
                .collect(),
            ..self
        }
    }
}

/// A requested change to a session's settings.
///
/// Built rather than struct-literalled, because the axes are the surface most likely to grow and a
/// host that wrote `..Default::default()` would silently start keeping a new one.
///
/// # Example
///
/// ```
/// use mango_external_agents::{ConfigurationChange, ConfigurationPatch, PermissionLevel};
///
/// let patch = ConfigurationPatch::new()
///     .model(ConfigurationChange::Set(String::from("opus")))
///     .level(ConfigurationChange::Set(PermissionLevel::ReadOnly))
///     .effort(ConfigurationChange::Reset);
///
/// assert!(!patch.is_empty());
/// assert!(patch.routing.is_keep());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ConfigurationPatch {
    /// What to do about the model.
    #[serde(default, skip_serializing_if = "ConfigurationChange::is_keep")]
    pub model: ConfigurationChange<String>,
    /// What to do about the reasoning effort.
    #[serde(default, skip_serializing_if = "ConfigurationChange::is_keep")]
    pub effort: ConfigurationChange<String>,
    /// What to do about what the agent may do.
    #[serde(default, skip_serializing_if = "ConfigurationChange::is_keep")]
    pub level: ConfigurationChange<PermissionLevel>,
    /// What to do about who answers its prompts.
    #[serde(default, skip_serializing_if = "ConfigurationChange::is_keep")]
    pub routing: ConfigurationChange<ApprovalRouting>,
    /// What to do about vendor-native options, by the vendor's own option id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native: BTreeMap<ConfigurationOptionId, ConfigurationChange<ConfigurationValue>>,
}

impl ConfigurationPatch {
    /// A patch that changes nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets, resets or keeps the model.
    #[must_use]
    pub fn model(mut self, change: ConfigurationChange<String>) -> Self {
        self.model = change;
        self
    }

    /// Sets, resets or keeps the reasoning effort.
    #[must_use]
    pub fn effort(mut self, change: ConfigurationChange<String>) -> Self {
        self.effort = change;
        self
    }

    /// Sets, resets or keeps what the agent may do.
    #[must_use]
    pub fn level(mut self, change: ConfigurationChange<PermissionLevel>) -> Self {
        self.level = change;
        self
    }

    /// Sets, resets or keeps who answers its prompts.
    #[must_use]
    pub fn routing(mut self, change: ConfigurationChange<ApprovalRouting>) -> Self {
        self.routing = change;
        self
    }

    /// Sets, resets or keeps one vendor-native option.
    #[must_use]
    pub fn native(
        mut self,
        option: ConfigurationOptionId,
        change: ConfigurationChange<ConfigurationValue>,
    ) -> Self {
        self.native.insert(option, change);
        self
    }

    /// Whether this patch asks for nothing.
    pub fn is_empty(&self) -> bool {
        self.model.is_keep()
            && self.effort.is_keep()
            && self.level.is_keep()
            && self.routing.is_keep()
            && self.native.values().all(ConfigurationChange::is_keep)
    }

    /// Whether this patch asks to remove an override anywhere.
    ///
    /// What a harness checks before encoding: a vendor with no reset semantics refuses here rather
    /// than sending a request it knows will be read as something else.
    pub fn asks_for_a_reset(&self) -> bool {
        !self.resetting_axes().is_empty()
    }

    /// Every axis this patch asks to remove an override on, by name.
    ///
    /// What a refusal puts in its message: a harness turning down a five-axis patch with "a patch
    /// asking to remove an override" leaves a maintainer nothing to act on.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{ConfigurationChange, ConfigurationPatch};
    ///
    /// let patch = ConfigurationPatch::new()
    ///     .model(ConfigurationChange::Reset)
    ///     .effort(ConfigurationChange::Reset);
    /// assert_eq!(patch.resetting_axes(), vec!["model", "effort"]);
    /// ```
    pub fn resetting_axes(&self) -> Vec<String> {
        let mut axes: Vec<String> = [
            ("model", self.model.is_reset()),
            ("effort", self.effort.is_reset()),
            ("level", self.level.is_reset()),
            ("routing", self.routing.is_reset()),
        ]
        .into_iter()
        .filter(|(_, reset)| *reset)
        .map(|(name, _)| String::from(name))
        .collect();
        axes.extend(
            self.native
                .iter()
                .filter(|(_, change)| change.is_reset())
                .map(|(option, _)| option.to_string()),
        );
        axes
    }

    /// The values this patch would set, as a plain configuration.
    ///
    /// The requested half of a [`ConfigurationState`]: what the host asked for, before anyone
    /// confirmed any of it.
    #[must_use]
    pub fn requested(&self) -> Configuration {
        Configuration::unknown().patched(self)
    }
}

/// What a session is set to, split by who says so.
///
/// The split exists because these three disagree in practice, and a host that shows the wrong one
/// tells a person their turn is running under a setting it is not.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ConfigurationState {
    /// What the host asked for, whether or not anything accepted it.
    pub requested: Configuration,
    /// What the harness confirmed it applied — the flag it really passed, the field it really set.
    ///
    /// An accepted setting is a statement about the request the harness encoded, not about what
    /// the vendor did with it.
    pub accepted: Configuration,
    /// What the vendor itself reported it is running under.
    ///
    /// The only one of the three that is evidence. A harness that has no vendor surface reporting
    /// a setting leaves the axis unknown here, permanently, rather than copying `accepted` across.
    pub observed: Configuration,
}

impl ConfigurationState {
    /// Nothing requested, nothing accepted, nothing observed.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// The three readings, together.
    pub fn new(requested: Configuration, accepted: Configuration, observed: Configuration) -> Self {
        Self {
            requested,
            accepted,
            observed,
        }
    }

    /// Records what the host asked for.
    #[must_use]
    pub fn with_requested(mut self, requested: Configuration) -> Self {
        self.requested = requested;
        self
    }

    /// Records what the harness confirmed it encoded.
    #[must_use]
    pub fn with_accepted(mut self, accepted: Configuration) -> Self {
        self.accepted = accepted;
        self
    }

    /// Records what the vendor reported about itself.
    #[must_use]
    pub fn with_observed(mut self, observed: Configuration) -> Self {
        self.observed = observed;
        self
    }

    /// All three readings with every vendor-written value bounded.
    ///
    /// Applied by [`SessionState::set_configuration`](crate::SessionState::set_configuration), so
    /// a harness cannot publish an unbounded vendor value onto a session snapshot by forgetting to
    /// call it. The `observed` half is the one that matters most — it is filled straight from
    /// whatever the vendor said about itself — but all three go through it, because a `requested`
    /// value a host echoed back from a vendor catalog is a vendor value too.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            requested: self.requested.normalized(),
            accepted: self.accepted.normalized(),
            observed: self.observed.normalized(),
        }
    }

    /// The value to show for the model, and where it came from.
    ///
    /// Observed beats accepted beats requested, because that is the order of how much each one
    /// proves. A host that wants one specific answer reads the field it means.
    pub fn effective_model(&self) -> Option<(&str, ConfigurationSource)> {
        Self::pick(
            self.observed.model.as_deref(),
            self.accepted.model.as_deref(),
            self.requested.model.as_deref(),
        )
    }

    /// The value to show for the reasoning effort, and where it came from.
    pub fn effective_effort(&self) -> Option<(&str, ConfigurationSource)> {
        Self::pick(
            self.observed.effort.as_deref(),
            self.accepted.effort.as_deref(),
            self.requested.effort.as_deref(),
        )
    }

    /// The permission level to show, and where it came from.
    pub fn effective_level(&self) -> Option<(PermissionLevel, ConfigurationSource)> {
        Self::pick(
            self.observed.level,
            self.accepted.level,
            self.requested.level,
        )
    }

    /// The approval routing to show, and where it came from.
    pub fn effective_routing(&self) -> Option<(ApprovalRouting, ConfigurationSource)> {
        Self::pick(
            self.observed.routing,
            self.accepted.routing,
            self.requested.routing,
        )
    }

    fn pick<T>(
        observed: Option<T>,
        accepted: Option<T>,
        requested: Option<T>,
    ) -> Option<(T, ConfigurationSource)> {
        observed
            .map(|value| (value, ConfigurationSource::Observed))
            .or_else(|| accepted.map(|value| (value, ConfigurationSource::Accepted)))
            .or_else(|| requested.map(|value| (value, ConfigurationSource::Requested)))
    }
}

/// Which of the three said so.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ConfigurationSource {
    /// The host asked for it and nothing has confirmed it.
    Requested,
    /// The harness confirmed it encoded the request.
    Accepted,
    /// The vendor reported it about itself.
    Observed,
}

impl fmt::Display for ConfigurationSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Requested => "requested",
            Self::Accepted => "accepted",
            Self::Observed => "observed",
        })
    }
}

/// A vendor's own id for one configurable option.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ConfigurationOptionId(String);

impl ConfigurationOptionId {
    /// Names one option, exactly as the vendor spells it.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ConfigurationOptionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One value a configuration option can hold.
///
/// Scalars only, and bounded. Not a JSON value: an option whose value is an arbitrary document is
/// a passthrough channel wearing a settings label, and nothing in this library forwards one.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ConfigurationValue {
    /// Text, or an enumerated value's own id.
    Text(String),
    /// A whole number.
    Integer(i64),
    /// A flag.
    Boolean(bool),
}

impl ConfigurationValue {
    /// Text, the common case.
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// This value bounded, or nothing when it could not be carried whole.
    ///
    /// Text is dropped rather than cut for the same reason an id is: a setting value is echoed
    /// back to the vendor, and half of one names something else.
    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        match self {
            Self::Text(value) => {
                let bounded = normalize::sanitize_field(&value);
                (!bounded.truncated
                    && !bounded.text.is_empty()
                    && bounded.text.chars().count() <= CONFIGURATION_TEXT_MAX_LENGTH)
                    .then_some(Self::Text(bounded.text))
            }
            scalar @ (Self::Integer(_) | Self::Boolean(_)) => Some(scalar),
        }
    }

    /// Which type of value this is, for matching against an option's declared type.
    pub const fn value_type(&self) -> ConfigurationValueType {
        match self {
            Self::Text(_) => ConfigurationValueType::Text,
            Self::Integer(_) => ConfigurationValueType::Integer,
            Self::Boolean(_) => ConfigurationValueType::Boolean,
        }
    }
}

/// What kind of value an option takes.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ConfigurationValueType {
    /// One of the values the option enumerates.
    Enumerated,
    /// Free text.
    Text,
    /// A whole number.
    Integer,
    /// A flag.
    Boolean,
    /// Something the vendor named that this crate has no arm for.
    Other(String),
}

/// What an option is for, as far as this crate can tell.
///
/// Open on purpose. A vendor that adds a setting nobody has a name for yet lands in
/// [`ConfigurationCategory::Other`], which is a row a host can still render and still set — the
/// rows beside it keep working either way.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ConfigurationCategory {
    /// Which model runs.
    Model,
    /// How hard it reasons.
    ReasoningEffort,
    /// A named operating mode the vendor offers.
    Mode,
    /// Something about what the agent may do.
    Permission,
    /// A category this crate has no arm for, under the vendor's own name.
    Other(String),
}

/// One thing a session can be configured to do, as the vendor describes it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ConfigurationOption {
    /// The vendor's own id, echoed back verbatim when this option is set.
    pub id: ConfigurationOptionId,
    /// What this option is for.
    pub category: ConfigurationCategory,
    /// A name for a person, when the vendor wrote one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// What the vendor says it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// What kind of value it takes.
    pub value_type: ConfigurationValueType,
    /// The values it enumerates, in the vendor's own order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<ConfigurationOptionValue>,
    /// What it is currently set to, when the vendor said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ConfigurationValue>,
    /// Whether the vendor can put this option back to its own default.
    ///
    /// False is the honest answer for a vendor that has no reset: a patch asking for one is
    /// refused rather than reported as applied.
    #[serde(default)]
    pub resettable: bool,
}

impl ConfigurationOption {
    /// An option with nothing but its id, category and value type.
    pub fn new(
        id: ConfigurationOptionId,
        category: ConfigurationCategory,
        value_type: ConfigurationValueType,
    ) -> Self {
        Self {
            id,
            category,
            name: None,
            description: None,
            value_type,
            values: Vec::new(),
            current: None,
            resettable: false,
        }
    }

    /// Carries the vendor's own name for it.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Carries what the vendor says it does.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Carries the values it enumerates, in the vendor's own order.
    #[must_use]
    pub fn with_values(mut self, values: Vec<ConfigurationOptionValue>) -> Self {
        self.values = values;
        self
    }

    /// Carries what it is currently set to.
    #[must_use]
    pub fn with_current(mut self, current: ConfigurationValue) -> Self {
        self.current = Some(current);
        self
    }

    /// Marks the option as one the vendor can put back to its own default.
    #[must_use]
    pub fn resettable(mut self) -> Self {
        self.resettable = true;
        self
    }

    /// This option with every vendor-written value bounded, or nothing when its id cannot survive.
    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        Some(Self {
            id: ConfigurationOptionId::new(
                normalize::opaque_id(self.id.as_str(), "configuration option id").ok()?,
            ),
            category: self.category.normalized(),
            name: self
                .name
                .map(|name| normalize::bound_text(&name, TextLimit::Title).text),
            description: self
                .description
                .map(|text| normalize::bound_text(&text, TextLimit::Detail).text),
            value_type: self.value_type.normalized(),
            values: self
                .values
                .into_iter()
                .filter_map(ConfigurationOptionValue::normalized)
                .take(CONFIGURATION_OPTION_MAX_VALUES)
                .collect(),
            current: self.current.and_then(ConfigurationValue::normalized),
            ..self
        })
    }
}

impl ConfigurationValueType {
    /// This type with a vendor-written name bounded.
    #[must_use]
    fn normalized(self) -> Self {
        match self {
            Self::Other(name) => Self::Other(normalize::bound_text(&name, TextLimit::Title).text),
            known => known,
        }
    }
}

impl ConfigurationCategory {
    /// This category with a vendor-written name bounded.
    #[must_use]
    fn normalized(self) -> Self {
        match self {
            Self::Other(name) => Self::Other(normalize::bound_text(&name, TextLimit::Title).text),
            known => known,
        }
    }
}

/// One value an enumerated option offers.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ConfigurationOptionValue {
    /// The value itself, as the vendor spells it.
    pub value: ConfigurationValue,
    /// A name for a person.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What the vendor says it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the vendor picks this one when nobody chooses.
    #[serde(default)]
    pub is_default: bool,
}

impl ConfigurationOptionValue {
    /// A value with no labels.
    pub fn new(value: ConfigurationValue) -> Self {
        Self {
            value,
            display_name: None,
            description: None,
            is_default: false,
        }
    }

    /// Carries a name for a person.
    #[must_use]
    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    /// Carries what the vendor says it does.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Marks this as the value the vendor picks when nobody chooses.
    #[must_use]
    pub fn as_default(mut self) -> Self {
        self.is_default = true;
        self
    }

    /// This value with its labels bounded, or nothing when the value itself cannot be carried.
    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        Some(Self {
            value: self.value.normalized()?,
            display_name: self
                .display_name
                .map(|name| normalize::bound_text(&name, TextLimit::Title).text),
            description: self
                .description
                .map(|text| normalize::bound_text(&text, TextLimit::Detail).text),
            ..self
        })
    }
}

/// Every option a session exposes, in the vendor's own order.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ConfigurationCatalog(Vec<ConfigurationOption>);

impl ConfigurationCatalog {
    /// A catalog of these options.
    pub fn new(options: Vec<ConfigurationOption>) -> Self {
        Self(options)
    }

    /// Nothing enumerated.
    ///
    /// The honest answer for a vendor that does not publish its settings, and a different
    /// statement from a catalog whose rows are all unsupported.
    pub fn empty() -> Self {
        Self(Vec::new())
    }

    /// Every option, in the order the vendor listed them.
    pub fn options(&self) -> &[ConfigurationOption] {
        &self.0
    }

    /// One option, by the vendor's own id.
    pub fn option(&self, id: &ConfigurationOptionId) -> Option<&ConfigurationOption> {
        self.0.iter().find(|option| &option.id == id)
    }

    /// Every option in one category, in order.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{
    ///     ConfigurationCatalog, ConfigurationCategory, ConfigurationOption, ConfigurationOptionId,
    ///     ConfigurationValueType,
    /// };
    ///
    /// let catalog = ConfigurationCatalog::new(vec![ConfigurationOption::new(
    ///     ConfigurationOptionId::new("model"),
    ///     ConfigurationCategory::Model,
    ///     ConfigurationValueType::Enumerated,
    /// )]);
    /// assert_eq!(catalog.in_category(&ConfigurationCategory::Model).len(), 1);
    /// ```
    pub fn in_category(&self, category: &ConfigurationCategory) -> Vec<&ConfigurationOption> {
        self.0
            .iter()
            .filter(|option| &option.category == category)
            .collect()
    }

    /// Whether nothing is enumerated.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many options are enumerated.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// This catalog with every row bounded and unusable rows dropped.
    ///
    /// A row whose id could not survive is dropped on its own. One unreadable option must not take
    /// the readable ones with it: a host that lost its whole model picker because the vendor added
    /// a setting with a broken id is worse off than one missing the broken row.
    #[must_use]
    pub fn normalized(self) -> Self {
        let mut kept: Vec<ConfigurationOption> = Vec::new();
        for option in self.0 {
            if kept.len() >= CONFIGURATION_CATALOG_MAX_OPTIONS {
                break;
            }
            let Some(option) = option.normalized() else {
                continue;
            };
            if kept.iter().any(|seen| seen.id == option.id) {
                continue;
            }
            kept.push(option);
        }
        Self(kept)
    }
}

/// What actually happened to a requested patch.
///
/// Returned instead of a bare `Result` because most vendors cannot set several options in one
/// atomic call, so "it worked" and "it failed" are not the only two answers. A patch that set two
/// of three options and was refused the third is [`ConfigurationOutcome::is_partial`], and saying
/// so is the difference between a host showing the truth and a host showing a transaction that
/// never happened.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ConfigurationOutcome {
    /// The state after whatever landed, landed.
    pub state: ConfigurationState,
    /// Which axes the harness applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applied: Vec<ConfigurationOptionId>,
    /// Which it did not, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rejected: Vec<RejectedSetting>,
    /// What happened to the settings that had already landed when one was refused.
    pub rollback: Rollback,
}

impl ConfigurationOutcome {
    /// An outcome where everything asked for landed.
    pub fn applied(state: ConfigurationState, applied: Vec<ConfigurationOptionId>) -> Self {
        Self {
            state,
            applied,
            rejected: Vec::new(),
            rollback: Rollback::NotNeeded,
        }
    }

    /// Whether some of the patch landed and some did not.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{
    ///     ConfigurationOutcome, ConfigurationOptionId, ConfigurationState, RejectedSetting,
    ///     Rollback, SettingRejection,
    /// };
    ///
    /// let outcome = ConfigurationOutcome::applied(
    ///     ConfigurationState::unknown(),
    ///     vec![ConfigurationOptionId::new("model")],
    /// )
    /// .rejecting(
    ///     vec![RejectedSetting::new(
    ///         ConfigurationOptionId::new("effort"),
    ///         SettingRejection::ResetNotSupported,
    ///     )],
    ///     Rollback::NotAttempted,
    /// );
    /// assert!(outcome.is_partial());
    /// assert!(!outcome.is_complete());
    /// ```
    pub fn is_partial(&self) -> bool {
        !self.applied.is_empty() && !self.rejected.is_empty()
    }

    /// Whether everything asked for landed.
    pub fn is_complete(&self) -> bool {
        self.rejected.is_empty()
    }

    /// Records what was refused and what became of the rest.
    ///
    /// The rollback is named rather than inferred: a harness whose vendor cannot un-set a setting
    /// reports [`Rollback::NotAttempted`], and the difference between that and
    /// [`Rollback::Restored`] is the difference between a host showing the truth and a host
    /// showing a transaction that did not happen.
    #[must_use]
    pub fn rejecting(mut self, rejected: Vec<RejectedSetting>, rollback: Rollback) -> Self {
        self.rejected = rejected;
        self.rollback = rollback;
        self
    }
}

/// One axis a harness would not set, and why.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RejectedSetting {
    /// Which option.
    pub option: ConfigurationOptionId,
    /// Why not.
    pub reason: SettingRejection,
}

impl RejectedSetting {
    /// Names one option and why it did not land.
    pub fn new(option: ConfigurationOptionId, reason: SettingRejection) -> Self {
        Self { option, reason }
    }
}

/// Why one requested setting did not land.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SettingRejection {
    /// The vendor has no option by that id.
    UnknownOption,
    /// The vendor has the option but not that value.
    UnsupportedValue {
        /// What was asked for, bounded.
        received: String,
    },
    /// The vendor cannot put this option back to its own default.
    ///
    /// An explicit refusal rather than a silent success: a reset reported as applied against a
    /// vendor that cannot reset leaves a host showing a default nothing is running under.
    ResetNotSupported,
    /// The option cannot be changed on a session that is already open.
    NotChangeableAfterOpen,
    /// The vendor refused it, in its own words.
    RefusedByVendor {
        /// What the vendor said, bounded.
        detail: String,
    },
}

impl SettingRejection {
    /// This reason with its vendor-written text bounded.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self {
            Self::UnsupportedValue { received } => Self::UnsupportedValue {
                received: normalize::bound_text(&received, TextLimit::Detail).text,
            },
            Self::RefusedByVendor { detail } => Self::RefusedByVendor {
                detail: normalize::bound_text(&detail, TextLimit::Detail).text,
            },
            known => known,
        }
    }
}

/// What happened to the part of a patch that had already landed when the rest was refused.
///
/// A vendor without atomic multi-option updates leaves a real choice here, and every answer but
/// [`Rollback::Restored`] means a host is looking at a state nobody asked for. Reporting which one
/// is the point: "partially applied, not rolled back" is actionable, and "it failed" is not.
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
pub enum Rollback {
    /// Nothing was refused, so nothing had to be undone.
    #[default]
    NotNeeded,
    /// Something was refused and the harness put the rest back.
    Restored,
    /// Something was refused and the harness left the rest applied.
    ///
    /// The honest answer for a vendor whose settings cannot be un-set.
    NotAttempted,
    /// Something was refused, the harness tried to put the rest back, and could not.
    Failed,
}

impl fmt::Display for Rollback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotNeeded => "not needed",
            Self::Restored => "restored",
            Self::NotAttempted => "not attempted",
            Self::Failed => "failed",
        })
    }
}

/// Refuses a patch that asks a vendor without reset semantics to remove an override.
///
/// The one rule shared by every harness whose vendor cannot reset, written once so three crates do
/// not each decide what "reset" means on a surface that has none.
///
/// # Errors
///
/// [`Error::HostConfiguration`] naming the axes that asked for a reset.
///
/// # Example
///
/// ```
/// use mango_external_agents::{configuration, ConfigurationChange, ConfigurationPatch};
///
/// let patch = ConfigurationPatch::new().model(ConfigurationChange::Reset);
/// assert!(configuration::refuse_unsupported_reset(&patch).is_err());
/// ```
pub fn refuse_unsupported_reset(patch: &ConfigurationPatch) -> Result<()> {
    let axes = patch.resetting_axes();
    if axes.is_empty() {
        return Ok(());
    }
    Err(Error::HostConfiguration {
        expected: "a patch that sets or keeps every axis, for a vendor with no reset",
        received: format!(
            "a patch asking to remove the override on {}",
            axes.join(", ")
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Configuration, ConfigurationCatalog, ConfigurationCategory, ConfigurationChange,
        ConfigurationOption, ConfigurationOptionId, ConfigurationOptionValue, ConfigurationPatch,
        ConfigurationSource, ConfigurationState, ConfigurationValue, ConfigurationValueType,
        Rollback,
    };
    use crate::permission::{ApprovalRouting, PermissionLevel};

    /// The distinction the old optional field could not make: a host that had set an override had
    /// no way to say "take it off again", because `None` already meant "leave it alone".
    #[test]
    fn keep_set_and_reset_round_trip_without_collapsing_into_each_other() {
        let current = Configuration {
            model: Some(String::from("opus")),
            effort: Some(String::from("high")),
            level: Some(PermissionLevel::ReadOnly),
            ..Configuration::unknown()
        };

        let kept = current.patched(&ConfigurationPatch::new());
        assert_eq!(kept, current, "expected keep to change nothing");

        let set = current.patched(
            &ConfigurationPatch::new().model(ConfigurationChange::Set(String::from("sonnet"))),
        );
        assert_eq!(set.model.as_deref(), Some("sonnet"));
        assert_eq!(
            set.effort.as_deref(),
            Some("high"),
            "expected an untouched axis to keep its value"
        );

        let reset = current.patched(&ConfigurationPatch::new().model(ConfigurationChange::Reset));
        assert_eq!(reset.model, None, "expected reset to remove the override");
        assert_eq!(reset.effort.as_deref(), Some("high"));
    }

    #[test]
    fn a_patch_serializes_only_the_axes_it_actually_asks_about() {
        let patch = ConfigurationPatch::new()
            .level(ConfigurationChange::Set(PermissionLevel::Default))
            .routing(ConfigurationChange::Reset);
        let encoded = serde_json::to_value(&patch).expect("expected a serializable patch");

        assert!(encoded.get("model").is_none(), "received {encoded}");
        assert_eq!(encoded["level"]["op"], "set");
        assert_eq!(encoded["level"]["value"], "default");
        assert_eq!(encoded["routing"]["op"], "reset");
        assert_eq!(
            serde_json::from_value::<ConfigurationPatch>(encoded).expect("expected the patch back"),
            patch
        );
    }

    #[test]
    fn a_patch_that_asks_for_nothing_is_empty_and_asks_for_no_reset() {
        let patch = ConfigurationPatch::new();
        assert!(patch.is_empty());
        assert!(!patch.asks_for_a_reset());

        let resetting = ConfigurationPatch::new().effort(ConfigurationChange::Reset);
        assert!(!resetting.is_empty());
        assert!(resetting.asks_for_a_reset());
    }

    /// A vendor with no reset must refuse rather than report a success it did not have.
    #[test]
    fn a_reset_is_refused_by_name_for_a_vendor_that_has_none() {
        let error = super::refuse_unsupported_reset(
            &ConfigurationPatch::new()
                .level(ConfigurationChange::Reset)
                .native(
                    ConfigurationOptionId::new("web-search"),
                    ConfigurationChange::Reset,
                ),
        )
        .expect_err("expected a refusal, received acceptance");
        assert!(
            error.to_string().contains("level, web-search"),
            "expected the refusal to name every axis that asked, received {error}"
        );
        super::refuse_unsupported_reset(
            &ConfigurationPatch::new().level(ConfigurationChange::Set(PermissionLevel::ReadOnly)),
        )
        .expect("expected a set-only patch to be accepted");
    }

    /// An accepted command-line option is not proof of a vendor-observed model. The three stay
    /// separate so a host can say which one it is showing.
    #[test]
    fn requested_accepted_and_observed_stay_distinguishable() {
        let state = ConfigurationState {
            requested: Configuration {
                model: Some(String::from("opus")),
                ..Configuration::unknown()
            },
            accepted: Configuration {
                model: Some(String::from("opus")),
                effort: Some(String::from("high")),
                ..Configuration::unknown()
            },
            observed: Configuration {
                model: Some(String::from("opus-20260101")),
                ..Configuration::unknown()
            },
        };

        assert_eq!(
            state.effective_model(),
            Some(("opus-20260101", ConfigurationSource::Observed)),
            "expected the vendor's own reading to win"
        );
        assert_eq!(
            state.effective_effort(),
            Some(("high", ConfigurationSource::Accepted)),
            "expected an accepted axis with no observation to be labelled accepted"
        );
        assert_eq!(
            state.effective_level(),
            None,
            "expected an axis nobody spoke about to stay unknown rather than take a default"
        );
    }

    #[test]
    fn an_unknown_configuration_says_so_rather_than_carrying_a_library_default() {
        assert!(Configuration::unknown().is_unknown());
        assert!(ConfigurationState::unknown().requested.is_unknown());
        assert_eq!(ConfigurationState::unknown().effective_routing(), None);
    }

    fn option(id: &str, category: ConfigurationCategory) -> ConfigurationOption {
        ConfigurationOption::new(
            ConfigurationOptionId::new(id),
            category,
            ConfigurationValueType::Enumerated,
        )
    }

    /// A category nobody has an arm for must not take the rows beside it down.
    #[test]
    fn an_unknown_category_stays_usable_next_to_the_known_ones() {
        let catalog = ConfigurationCatalog::new(vec![
            option("model", ConfigurationCategory::Model),
            option(
                "thinking-budget",
                ConfigurationCategory::Other(String::from("budget")),
            ),
        ])
        .normalized();

        assert_eq!(catalog.len(), 2, "received {:?}", catalog.options());
        assert_eq!(
            catalog
                .option(&ConfigurationOptionId::new("thinking-budget"))
                .map(|option| &option.category),
            Some(&ConfigurationCategory::Other(String::from("budget")))
        );
        assert_eq!(catalog.in_category(&ConfigurationCategory::Model).len(), 1);
    }

    /// One row with an unusable id must not cost a host its whole picker.
    #[test]
    fn a_row_whose_id_cannot_survive_bounding_is_dropped_on_its_own() {
        let catalog = ConfigurationCatalog::new(vec![
            option(&"i".repeat(129), ConfigurationCategory::Mode),
            option("model", ConfigurationCategory::Model),
        ])
        .normalized();

        assert_eq!(catalog.len(), 1, "received {:?}", catalog.options());
        assert_eq!(catalog.options()[0].id.as_str(), "model");
    }

    #[test]
    fn a_catalog_keeps_the_vendors_own_ordering_and_native_value_ids() {
        let catalog = ConfigurationCatalog::new(vec![ConfigurationOption {
            values: vec![
                ConfigurationOptionValue::new(ConfigurationValue::text("high")),
                ConfigurationOptionValue {
                    is_default: true,
                    ..ConfigurationOptionValue::new(ConfigurationValue::text("medium"))
                },
            ],
            ..option("effort", ConfigurationCategory::ReasoningEffort)
        }])
        .normalized();

        let values: Vec<&ConfigurationValue> = catalog.options()[0]
            .values
            .iter()
            .map(|value| &value.value)
            .collect();
        assert_eq!(
            values,
            vec![
                &ConfigurationValue::text("high"),
                &ConfigurationValue::text("medium")
            ],
            "expected the vendor's own order"
        );
        assert!(catalog.options()[0].values[1].is_default);
    }

    /// A settings value is echoed back to the vendor, so half of one names something else.
    #[test]
    fn a_text_value_that_would_have_to_be_repaired_is_dropped_rather_than_cut() {
        assert_eq!(ConfigurationValue::text("opus\u{202e}").normalized(), None);
        assert_eq!(
            ConfigurationValue::text("o".repeat(super::CONFIGURATION_TEXT_MAX_LENGTH + 1))
                .normalized(),
            None
        );
        assert_eq!(
            ConfigurationValue::Integer(7).normalized(),
            Some(ConfigurationValue::Integer(7))
        );
        assert_eq!(
            ConfigurationValue::text("opus").normalized(),
            Some(ConfigurationValue::text("opus"))
        );
    }

    #[test]
    fn a_native_option_is_set_and_reset_through_the_same_three_way_change() {
        let option = ConfigurationOptionId::new("web-search");
        let enabled = Configuration::unknown().patched(&ConfigurationPatch::new().native(
            option.clone(),
            ConfigurationChange::Set(ConfigurationValue::Boolean(true)),
        ));
        assert_eq!(
            enabled.native.get(&option),
            Some(&ConfigurationValue::Boolean(true))
        );

        let cleared = enabled
            .patched(&ConfigurationPatch::new().native(option.clone(), ConfigurationChange::Reset));
        assert_eq!(cleared.native.get(&option), None);

        let untouched = enabled.patched(&ConfigurationPatch::new());
        assert_eq!(
            untouched.native.get(&option),
            Some(&ConfigurationValue::Boolean(true))
        );
    }

    #[test]
    fn a_value_reports_the_type_an_option_would_be_checked_against() {
        assert_eq!(
            ConfigurationValue::text("x").value_type(),
            ConfigurationValueType::Text
        );
        assert_eq!(
            ConfigurationValue::Boolean(true).value_type(),
            ConfigurationValueType::Boolean
        );
        assert_eq!(
            ConfigurationValue::Integer(1).value_type(),
            ConfigurationValueType::Integer
        );
    }

    #[test]
    fn the_requested_half_of_a_state_is_derivable_from_the_patch_alone() {
        let patch = ConfigurationPatch::new()
            .model(ConfigurationChange::Set(String::from("opus")))
            .routing(ConfigurationChange::Set(ApprovalRouting::User));
        let requested = patch.requested();
        assert_eq!(requested.model.as_deref(), Some("opus"));
        assert_eq!(requested.routing, Some(ApprovalRouting::User));
        assert_eq!(requested.level, None);
    }

    /// The one reading that is filled straight from what a vendor said about itself. Nothing else
    /// stands between it and a host's picker, so it has to be bounded here or it is not bounded.
    #[test]
    fn every_reading_of_a_state_is_bounded_including_the_one_the_vendor_wrote() {
        let state = ConfigurationState::new(
            Configuration::unknown().with_model("opus"),
            Configuration::unknown().with_effort("e".repeat(129)),
            Configuration::unknown()
                .with_model("opus\u{202e}gnihton")
                .with_native(
                    ConfigurationOptionId::new("o".repeat(129)),
                    ConfigurationValue::Boolean(true),
                )
                .with_native(
                    ConfigurationOptionId::new("web-search"),
                    ConfigurationValue::Boolean(true),
                ),
        )
        .normalized();

        assert_eq!(state.requested.model.as_deref(), Some("opus"));
        assert_eq!(
            state.accepted.effort, None,
            "expected an id too long to carry to be dropped rather than cut"
        );
        assert_eq!(
            state.observed.model, None,
            "expected an id carrying an override to be refused rather than repaired"
        );
        assert_eq!(
            state.observed.native.len(),
            1,
            "expected only the unusable native key to be dropped, received {:?}",
            state.observed.native
        );
        assert!(
            state
                .observed
                .native
                .contains_key(&ConfigurationOptionId::new("web-search"))
        );
    }

    /// A category this crate has no arm for is bounded; so is a value type. Both are vendor-written
    /// and both reach a host's renderer.
    #[test]
    fn a_vendor_named_value_type_is_bounded_like_a_vendor_named_category() {
        let catalog = ConfigurationCatalog::new(vec![ConfigurationOption::new(
            ConfigurationOptionId::new("budget"),
            ConfigurationCategory::Other("c".repeat(300)),
            ConfigurationValueType::Other("t".repeat(300)),
        )])
        .normalized();

        let option = &catalog.options()[0];
        let ConfigurationCategory::Other(category) = &option.category else {
            panic!(
                "expected a vendor-named category, received {:?}",
                option.category
            );
        };
        let ConfigurationValueType::Other(value_type) = &option.value_type else {
            panic!(
                "expected a vendor-named value type, received {:?}",
                option.value_type
            );
        };
        assert_eq!(category.chars().count(), 256);
        assert_eq!(value_type.chars().count(), 256);
    }

    #[test]
    fn a_rollback_prints_the_four_answers_a_host_has_to_tell_apart() {
        assert_eq!(Rollback::default(), Rollback::NotNeeded);
        assert_eq!(Rollback::NotAttempted.to_string(), "not attempted");
        assert_eq!(Rollback::Restored.to_string(), "restored");
        assert_eq!(Rollback::Failed.to_string(), "failed");
    }
}
