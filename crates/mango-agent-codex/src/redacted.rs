//! The placeholder a metadata-only `Debug` prints where a value's content would have been.
//!
//! `docs/compliance.md` puts interactions under a metadata-only `Debug` policy: a carrier prints
//! what kind of thing it is, which optional members are present and how large they are, never what
//! they say. A derive prints everything, so a carrier writes its own `fmt` and hands each content
//! member to one of these helpers:
//!
//! ```ignore
//! formatter
//!     .debug_struct("CommandExecutionApprovalParams")
//!     .field("command", &redacted::opt_text(self.command.as_deref()))
//!     .field("amendment", &redacted::opt_json(self.amendment.as_ref()))
//!     .finish()
//! // CommandExecutionApprovalParams { command: Some(<14 bytes redacted>), amendment: None }
//! ```
//!
//! The size is the only thing reported: enough to tell an empty value from a large one when
//! reading a log, and nothing a reader could recover text from.

use std::fmt;

use serde_json::Value;

/// A value that is present but not printed, with its size in bytes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Redacted {
    bytes: usize,
}

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "<{} bytes redacted>", self.bytes)
    }
}

/// A string member, reported by its length.
pub(crate) fn text(value: &str) -> Redacted {
    Redacted { bytes: value.len() }
}

impl Redacted {
    /// The combined size of several members, for a list reported as one figure.
    pub(crate) fn sum(sizes: impl Iterator<Item = usize>) -> Self {
        Self { bytes: sizes.sum() }
    }
}

/// An optional string member: `None` when absent, its length when present.
pub(crate) fn opt_text(value: Option<&str>) -> Option<Redacted> {
    value.map(text)
}

/// A JSON member, reported by the length of its compact serialization.
pub(crate) fn json(value: &Value) -> Redacted {
    // `Value`'s `Display` is its compact serialization and cannot fail, so an unserializable value
    // never reads as an empty one.
    Redacted {
        bytes: value.to_string().len(),
    }
}

/// An optional JSON member: `None` when absent, its serialized length when present.
pub(crate) fn opt_json(value: Option<&Value>) -> Option<Redacted> {
    value.map(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_string_is_reported_by_its_byte_length_and_nothing_else() {
        let printed = format!("{:?}", text("CANARY-é"));
        assert_eq!(
            printed, "<9 bytes redacted>",
            "expected the byte length of a 9-byte string and no text | received: {printed}"
        );
    }

    #[test]
    fn several_members_are_reported_as_one_combined_size() {
        assert_eq!(
            format!("{:?}", Redacted::sum([3_usize, 4].into_iter())),
            "<7 bytes redacted>"
        );
    }

    #[test]
    fn an_absent_optional_member_prints_as_none() {
        assert_eq!(format!("{:?}", opt_text(None)), "None");
        assert_eq!(format!("{:?}", opt_json(None)), "None");
        assert_eq!(
            format!("{:?}", opt_text(Some("abc"))),
            "Some(<3 bytes redacted>)"
        );
    }

    #[test]
    fn json_is_reported_by_its_compact_length_without_its_content() {
        let value = json!({"host": "CANARY-host"});
        let printed = format!("{:?}", json(&value));
        assert_eq!(
            printed, "<22 bytes redacted>",
            "expected the compact length of {{\"host\":\"CANARY-host\"}} | received: {printed}"
        );
        assert!(!printed.contains("CANARY"), "received: {printed}");
        assert_eq!(
            format!("{:?}", opt_json(Some(&value))),
            "Some(<22 bytes redacted>)"
        );
    }
}
