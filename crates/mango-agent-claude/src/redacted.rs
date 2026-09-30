//! The placeholder a metadata-only `Debug` prints where a value's content would have been.
//!
//! `docs/compliance.md` puts raw protocol records and reducers under a metadata-only `Debug`
//! policy: a record prints what kind of thing it is, how many members it has and how large they
//! are, never what they say. A derive prints everything, so a record writes its own `fmt` and hands
//! each content member to one of these helpers. The Codex crate keeps a copy of the same pattern
//! (`mango_agent_codex`'s private `redacted` module), so the two harnesses read alike in a log; it
//! is a copy, not a shared item, so neither crate's public surface grows.
//!
//! ```ignore
//! formatter
//!     .debug_struct("ContentBlock")
//!     .field("kind", &redacted::label(self.kind()))
//!     .field("body", &redacted::text(self.result_text()))
//!     .finish()
//! // ContentBlock { kind: Some("tool_result"), body: <19 bytes redacted> }
//! ```

use std::fmt;

use serde_json::{Map, Value};

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

/// A JSON object member, reported by the length of its compact serialization.
pub(crate) fn object(fields: &Map<String, Value>) -> Redacted {
    // `Value`'s `Display` is its compact serialization and cannot fail, so an unserializable value
    // never reads as an empty one. The clone is only paid when a record is debug-formatted.
    Redacted {
        bytes: Value::Object(fields.clone()).to_string().len(),
    }
}

/// A protocol discriminator such as a record's `type`: printed when it has a label's shape, so a
/// log names the record, and reported by size when it does not, because a value the vendor
/// composed can carry anything.
pub(crate) struct Label<'a>(pub(crate) Option<&'a str>);

/// The longest discriminator printed as itself.
const LABEL_MAX_BYTES: usize = 48;

impl fmt::Debug for Label<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => formatter.write_str("None"),
            Some(value) if is_label(value) => write!(formatter, "Some({value:?})"),
            Some(value) => write!(formatter, "Some({:?})", text(value)),
        }
    }
}

/// Whether a value has a label's shape: short, and only `a-z`, `A-Z`, `0-9`, `-`, `_`, `.` and `:`.
fn is_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= LABEL_MAX_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

/// An optional discriminator; see [`Label`].
pub(crate) fn label(value: Option<&str>) -> Label<'_> {
    Label(value)
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
    fn an_object_is_reported_by_its_compact_length_without_its_content() {
        let fields = json!({"host": "CANARY-host"});
        let Value::Object(fields) = fields else {
            panic!("expected an object");
        };
        let printed = format!("{:?}", object(&fields));
        assert_eq!(
            printed, "<22 bytes redacted>",
            "expected the compact length of {{\"host\":\"CANARY-host\"}} | received: {printed}"
        );
    }

    #[test]
    fn a_discriminator_with_a_labels_shape_prints_and_anything_else_is_sized() {
        assert_eq!(
            format!("{:?}", label(Some("tool_result"))),
            "Some(\"tool_result\")"
        );
        assert_eq!(format!("{:?}", label(None)), "None");
        for unlabelled in ["has a space", "", &"x".repeat(49), "café"] {
            let printed = format!("{:?}", label(Some(unlabelled)));
            assert_eq!(
                printed,
                format!("Some({:?})", text(unlabelled)),
                "expected {unlabelled:?} to be reported by size | received: {printed}"
            );
        }
    }
}
