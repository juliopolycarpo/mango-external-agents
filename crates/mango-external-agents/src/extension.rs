//! Useful vendor detail this crate has no field for, bounded and observational.
//!
//! Vendors report more than the neutral vocabulary names, and throwing it away costs a host real
//! information — a token budget, a sandbox label, a model revision. Carrying it as
//! `serde_json::Value` would cost something worse: an unbounded channel from a vendor process into
//! a host's database and interface, which is the shape every other boundary in this crate exists
//! to prevent.
//!
//! So [`Extensions`] is a small, flat, scalar-only map with three properties:
//!
//! 1. **Bounded.** A capped number of entries, capped key and value lengths, no nesting. A vendor
//!    that starts emitting a document gets its document dropped, not forwarded.
//! 2. **Observational.** Nothing read from here is ever executed, dispatched, or turned into an
//!    RPC. It is metadata to render, log or ignore.
//! 3. **Redacted.** Values go through the same credential-shaped-text redaction a stderr tail
//!    does, because a vendor that puts a token in a metadata field has still put a token in a
//!    host's database.
//!
//! What belongs in a real field stays in a real field. This is the escape hatch for the long tail,
//! and a harness that finds itself putting something load-bearing here should be adding a field.

use std::collections::BTreeMap;
use std::fmt;

use crate::normalize::{self, TextLimit};

/// How many entries one extension map may carry.
pub const EXTENSIONS_MAX_ENTRIES: usize = 32;

/// How many code points an extension key may carry.
pub const EXTENSION_KEY_MAX_LENGTH: usize = 64;

/// How many code points an extension text value may carry.
///
/// Derived from the cap [`ExtensionValue::normalized`] actually applies rather than written out
/// beside it, so the published number and the enforced one cannot drift: a host sizing a column or
/// a renderer off a constant that disagreed with the code would be wrong and have no way to tell.
pub const EXTENSION_VALUE_MAX_LENGTH: usize = EXTENSION_VALUE_LIMIT.max_code_points();

/// The bound one extension text value is cut to.
///
/// A label's length, because that is what an extension value is for: a word or two a host renders
/// beside an activity. Anything longer belongs in a field of its own.
const EXTENSION_VALUE_LIMIT: TextLimit = TextLimit::ApprovalOptionLabel;

/// One value a vendor reported that this crate has no field for.
///
/// Scalars only. There is no list arm and no map arm, and that is the design: nesting is what
/// turns a metadata field into a payload channel, and a host that has to walk a tree to render a
/// label is a host depending on a vendor's wire shape.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ExtensionValue {
    /// Text, bounded and redacted.
    Text(String),
    /// A whole number.
    Integer(i64),
    /// A number.
    Float(f64),
    /// A flag.
    Boolean(bool),
}

impl fmt::Debug for ExtensionValue {
    /// Names the extension value type without formatting vendor-provided text.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Text(_) => "Text",
            Self::Integer(_) => "Integer",
            Self::Float(_) => "Float",
            Self::Boolean(_) => "Boolean",
        })
    }
}

impl ExtensionValue {
    /// Text, the common case.
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// This value bounded, or nothing when it cannot be carried.
    ///
    /// Text is cut rather than refused: unlike an id, nothing echoes an extension value back to a
    /// vendor, so a shortened one names nothing and misleads nobody. A non-finite number is
    /// dropped instead — `serde_json` refuses to write one, and an event that cannot be persisted
    /// over one optional field is worse than an event missing the field.
    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        match self {
            Self::Text(value) => {
                let redacted = crate::redact::stderr_text(&value);
                let bounded = normalize::bound_text(&redacted, EXTENSION_VALUE_LIMIT);
                (!bounded.text.is_empty()).then_some(Self::Text(bounded.text))
            }
            Self::Float(value) => value.is_finite().then_some(Self::Float(value)),
            scalar @ (Self::Integer(_) | Self::Boolean(_)) => Some(scalar),
        }
    }
}

/// Bounded observational metadata, keyed by the vendor's own field name.
///
/// # Example
///
/// ```
/// use mango_external_agents::{ExtensionValue, Extensions};
///
/// let extensions = Extensions::new()
///     .with("sandbox", ExtensionValue::text("workspace-write"))
///     .with("cachedTokens", ExtensionValue::Integer(1_024))
///     .normalized();
///
/// assert_eq!(extensions.get("sandbox"), Some(&ExtensionValue::text("workspace-write")));
/// assert!(extensions.get("absent").is_none());
/// ```
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct Extensions(BTreeMap<String, ExtensionValue>);

impl fmt::Debug for Extensions {
    /// Reports extension count without logging vendor-defined keys or values.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Extensions")
            .field("entry_count", &self.0.len())
            .finish()
    }
}

impl Extensions {
    /// Nothing carried.
    pub fn new() -> Self {
        Self::default()
    }

    /// Carries one more entry.
    #[must_use]
    pub fn with(mut self, key: impl Into<String>, value: ExtensionValue) -> Self {
        self.0.insert(key.into(), value);
        self
    }

    /// One entry, when it is there.
    pub fn get(&self, key: &str) -> Option<&ExtensionValue> {
        self.0.get(key)
    }

    /// Every entry, in key order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &ExtensionValue)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    /// Whether nothing is carried.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many entries are carried.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// These extensions with every key and value bounded and entries beyond the cap dropped.
    ///
    /// Applied at the same boundary as every other vendor value, so a harness cannot emit an
    /// unbounded map by forgetting to call it.
    #[must_use]
    pub fn normalized(self) -> Self {
        let mut kept = BTreeMap::new();
        for (key, value) in self.0 {
            if kept.len() >= EXTENSIONS_MAX_ENTRIES {
                break;
            }
            let key = normalize::sanitize_field(&key).text;
            if key.is_empty() || key.chars().count() > EXTENSION_KEY_MAX_LENGTH {
                continue;
            }
            let Some(value) = value.normalized() else {
                continue;
            };
            kept.insert(key, value);
        }
        Self(kept)
    }
}

impl FromIterator<(String, ExtensionValue)> for Extensions {
    fn from_iter<I: IntoIterator<Item = (String, ExtensionValue)>>(entries: I) -> Self {
        Self(entries.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::{EXTENSIONS_MAX_ENTRIES, ExtensionValue, Extensions};

    #[test]
    fn a_map_past_the_cap_is_cut_rather_than_carried() {
        let extensions: Extensions = (0..EXTENSIONS_MAX_ENTRIES + 10)
            .map(|index| {
                (
                    format!("key{index:03}"),
                    ExtensionValue::Integer(index as i64),
                )
            })
            .collect();
        assert_eq!(extensions.normalized().len(), EXTENSIONS_MAX_ENTRIES);
    }

    /// A vendor that puts a token in a metadata field has still put a token in a host's database.
    #[test]
    fn a_credential_shaped_value_is_redacted_before_it_reaches_a_host() {
        let extensions = Extensions::new()
            .with(
                "auth",
                ExtensionValue::text("Authorization: Bearer sk-live-abcdefghijklmnop"),
            )
            .normalized();
        let Some(ExtensionValue::Text(value)) = extensions.get("auth") else {
            panic!("expected text, received {:?}", extensions.get("auth"));
        };
        assert!(
            !value.contains("sk-live-abcdefghijklmnop"),
            "expected the token to be redacted, received {value}"
        );
    }

    /// `serde_json` refuses to write a non-finite number, so an event carrying one could not be
    /// persisted at all. The field goes; the event stays.
    #[test]
    fn a_number_that_could_not_be_written_is_dropped_rather_than_carried() {
        let extensions = Extensions::new()
            .with("ratio", ExtensionValue::Float(f64::NAN))
            .with("used", ExtensionValue::Float(0.5))
            .normalized();

        assert_eq!(extensions.get("ratio"), None);
        assert_eq!(extensions.get("used"), Some(&ExtensionValue::Float(0.5)));
        serde_json::to_string(&extensions).expect("expected a map that can be written");
    }

    #[test]
    fn a_key_that_cannot_be_carried_whole_drops_its_entry() {
        let extensions = Extensions::new()
            .with("k".repeat(65), ExtensionValue::Integer(1))
            .with("  ", ExtensionValue::Integer(2))
            .with("kept", ExtensionValue::Integer(3))
            .normalized();

        assert_eq!(extensions.len(), 2, "received {extensions:?}");
        assert_eq!(extensions.get("kept"), Some(&ExtensionValue::Integer(3)));
    }

    /// Nothing echoes an extension value back to a vendor, so cutting one names nothing.
    #[test]
    fn an_over_long_text_value_is_cut_rather_than_dropped() {
        let extensions = Extensions::new()
            .with("note", ExtensionValue::text("n".repeat(4_000)))
            .normalized();
        let Some(ExtensionValue::Text(value)) = extensions.get("note") else {
            panic!("expected the entry to survive, received {extensions:?}");
        };
        assert_eq!(
            value.chars().count(),
            super::EXTENSION_VALUE_MAX_LENGTH,
            "expected the published cap to be the one the code applies"
        );
    }

    #[test]
    fn a_map_round_trips_as_a_flat_object() {
        let extensions = Extensions::new()
            .with("sandbox", ExtensionValue::text("workspace-write"))
            .with("attempts", ExtensionValue::Integer(2))
            .with("streaming", ExtensionValue::Boolean(true));
        let encoded = serde_json::to_value(&extensions).expect("expected a serializable map");
        assert_eq!(encoded["sandbox"], "workspace-write");
        assert_eq!(encoded["attempts"], 2);
        assert_eq!(encoded["streaming"], true);
        assert_eq!(
            serde_json::from_value::<Extensions>(encoded).expect("expected the map back"),
            extensions
        );
    }
}
