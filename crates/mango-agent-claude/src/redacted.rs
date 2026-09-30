//! The placeholder a metadata-only `Debug` prints where a value's content would have been.
//!
//! `docs/compliance.md` puts raw protocol records and reducers under a metadata-only `Debug`
//! policy: a record prints what kind of thing it is, how many members it has and how large they
//! are, never what they say. A derive prints everything, so a record writes its own `fmt` and hands
//! each content member to one of these helpers. The Codex crate follows the same pattern for its
//! own records, so the two harnesses read alike in a log; each keeps its own private helper, so
//! neither crate's public surface grows.
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
use std::io;

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

impl Redacted {
    /// The combined size of several members, for a collection reported as one figure.
    pub(crate) fn sum(sizes: impl Iterator<Item = usize>) -> Self {
        Self { bytes: sizes.sum() }
    }
}

/// A JSON object member, reported by the length of its compact serialization.
///
/// Counted while serializing the borrowed map into a writer that keeps only the length, so a large
/// record is never cloned or materialised to be measured.
pub(crate) fn object(fields: &Map<String, Value>) -> Redacted {
    struct Counter(usize);

    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // Serializing a `Map<String, Value>` cannot fail, and the counter never refuses a write, so
    // the count is always the whole length.
    let mut counter = Counter(0);
    let _ = serde_json::to_writer(&mut counter, fields);
    Redacted { bytes: counter.0 }
}

/// A protocol discriminator such as a record's `type`: printed only when it is one this harness
/// knows by name, so a log names the record, and reported by size otherwise. A value the vendor
/// composed can carry anything, and a credential or an id can have a label's shape, so the shape
/// of a value is not a reason to print it.
pub(crate) struct Label<'a>(pub(crate) Option<&'a str>);

/// Every discriminator the pinned Claude Code build writes for a record, a content block, a stream
/// event or a delta, plus the result subtypes it documents.
const KNOWN_DISCRIMINATORS: &[&str] = &[
    // Records.
    "system",
    "assistant",
    "user",
    "result",
    "stream_event",
    "rate_limit_event",
    // System and result subtypes.
    "init",
    "status",
    "thinking_tokens",
    "api_retry",
    "permission_denied",
    "success",
    "error_max_turns",
    "error_during_execution",
    // Content blocks.
    "text",
    "thinking",
    "redacted_thinking",
    "tool_use",
    "tool_result",
    // Stream events.
    "message_start",
    "message_delta",
    "message_stop",
    "content_block_start",
    "content_block_delta",
    "content_block_stop",
    "ping",
    // Deltas.
    "text_delta",
    "thinking_delta",
    "input_json_delta",
    "signature_delta",
];

impl fmt::Debug for Label<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => formatter.write_str("None"),
            Some(value) if KNOWN_DISCRIMINATORS.contains(&value) => {
                write!(formatter, "Some({value:?})")
            }
            Some(value) => write!(formatter, "Some({:?})", text(value)),
        }
    }
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
    fn several_members_are_reported_as_one_combined_size() {
        assert_eq!(
            format!("{:?}", Redacted::sum([3_usize, 4].into_iter())),
            "<7 bytes redacted>"
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
    fn a_known_discriminator_prints_and_anything_else_is_sized() {
        assert_eq!(
            format!("{:?}", label(Some("tool_result"))),
            "Some(\"tool_result\")"
        );
        assert_eq!(format!("{:?}", label(None)), "None");
        // Credential- and id-shaped values have a label's shape and must still be sized.
        for unknown in [
            "AKIAIOSFODNN7EXAMPLE",
            "3f2b8c1e-0d4a-4c7e-9a55-1b2c3d4e5f60",
            "has a space",
            "",
            "café",
        ] {
            let printed = format!("{:?}", label(Some(unknown)));
            assert_eq!(
                printed,
                format!("Some({:?})", text(unknown)),
                "expected {unknown:?} to be reported by size | received: {printed}"
            );
        }
    }

    #[test]
    fn a_large_object_is_measured_without_being_cloned() {
        let Value::Object(fields) = json!({"body": "x".repeat(1_000_000)}) else {
            panic!("expected an object");
        };
        assert_eq!(
            format!("{:?}", object(&fields)),
            format!("<{} bytes redacted>", 1_000_000 + 11),
            "expected the compact length of a million-byte body plus its framing"
        );
    }
}
