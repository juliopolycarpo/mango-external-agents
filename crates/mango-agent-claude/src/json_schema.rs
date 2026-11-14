//! The host-supplied JSON Schema a turn passes with `--json-schema`.
//!
//! The vendor documents the flag for headless runs: "To get output conforming to a specific
//! schema, use `--output-format json` with `--json-schema` and a JSON Schema definition", and a
//! value that is not a valid schema fails the run at startup
//! (<https://code.claude.com/docs/en/headless.md>,
//! <https://code.claude.com/docs/en/agent-sdk/structured-outputs.md>).
//!
//! This module only decides whether a host string may travel on a command line. It does not
//! evaluate the schema: the CLI owns that and reports its own diagnostic. What it settles is the
//! argv side, before anything is spawned:
//!
//! - the text parses as JSON, so a typo is a typed refusal rather than a CLI exit after launch;
//! - the top level is an object, so the argument starts with `{` and can never be read by the
//!   CLI's parser as another flag (`-1` is valid JSON and begins with `-`);
//! - the argument is the compact re-serialisation, so a newline or other control character the
//!   host's pretty-printing carried never reaches argv;
//! - the size stays under [`MAX_JSON_SCHEMA_BYTES`].

use mango_external_agents::{Error, Result};

/// The largest compact schema this harness puts on a command line.
///
/// The schema travels inline, because the flag takes a value rather than a path. Windows caps a
/// whole command line at 32,767 characters, so 16 KiB leaves room for every other flag. A host
/// with a larger schema needs a surface the vendor documents for it; none is documented for
/// `--json-schema` today.
pub const MAX_JSON_SCHEMA_BYTES: usize = 16 * 1024;

/// What a refusal names as the accepted shape.
const EXPECTED_SHAPE: &str = "a JSON object, no larger than 16384 bytes once compacted, that is a JSON Schema for --json-schema";

/// Checks a host's schema text and returns the compact form that goes on argv.
///
/// # Errors
///
/// [`Error::HostConfiguration`] naming the size (never the content) of what was received, when the
/// text is not JSON, is not an object, or does not fit [`MAX_JSON_SCHEMA_BYTES`].
///
/// # Example
///
/// ```
/// use mango_agent_claude::json_schema::compact;
///
/// let argument = compact("{\n  \"type\": \"object\"\n}").expect("expected a schema");
/// assert_eq!(argument, r#"{"type":"object"}"#);
/// assert!(compact("-1").is_err());
/// ```
pub fn compact(raw: &str) -> Result<String> {
    // Checked before parsing so a megabyte of nested brackets is never walked.
    if raw.len() > MAX_JSON_SCHEMA_BYTES.saturating_mul(4) {
        return Err(refusal(format!("{} bytes of text", raw.len())));
    }
    let value: serde_json::Value = serde_json::from_str(raw).map_err(|error| {
        refusal(format!(
            "{} that do not parse as JSON ({error})",
            crate::argv::value_summary(raw)
        ))
    })?;
    if !value.is_object() {
        return Err(refusal(format!(
            "{} that parse as {}",
            crate::argv::value_summary(raw),
            json_kind(&value)
        )));
    }
    // Serialising a parsed `Value` cannot fail: every key is a string.
    let compact = serde_json::to_string(&value).map_err(|error| {
        refusal(format!(
            "{} that could not be re-serialised ({error})",
            crate::argv::value_summary(raw)
        ))
    })?;
    if compact.len() > MAX_JSON_SCHEMA_BYTES {
        return Err(refusal(format!(
            "a schema of {} compact bytes",
            compact.len()
        )));
    }
    Ok(compact)
}

fn refusal(received: String) -> Error {
    Error::HostConfiguration {
        expected: EXPECTED_SHAPE,
        received,
    }
}

fn json_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_JSON_SCHEMA_BYTES, compact, json_kind};
    use mango_external_agents::Error;

    fn refusal_received(raw: &str) -> String {
        match compact(raw) {
            Err(Error::HostConfiguration { expected, received }) => {
                assert!(
                    expected.contains("JSON object"),
                    "expected the shape to name a JSON object | received {expected:?}"
                );
                received
            }
            other => {
                panic!("expected a host-configuration refusal for {raw:?} | received {other:?}")
            }
        }
    }

    #[test]
    fn compacts_pretty_printed_schema_text_into_one_line() {
        let argument = compact("{\n  \"type\": \"object\",\n  \"required\": [\"name\"]\n}")
            .expect("expected a schema");
        assert!(
            !argument.chars().any(char::is_control),
            "expected no control character on argv | received {argument:?}"
        );
        let reparsed: serde_json::Value =
            serde_json::from_str(&argument).expect("expected JSON back");
        assert_eq!(reparsed["required"][0], "name");
    }

    #[test]
    fn keeps_a_control_character_inside_a_string_escaped() {
        let argument =
            compact("{\"description\":\"line one\\nline two\\u0000\"}").expect("expected a schema");
        assert!(
            !argument.chars().any(char::is_control),
            "expected an escaped control character | received {argument:?}"
        );
    }

    #[test]
    fn refuses_text_that_is_not_json_without_echoing_it() {
        let received = refusal_received("{not json secret-canary");
        assert!(
            received.contains("code points that do not parse as JSON"),
            "received {received:?}"
        );
        assert!(!received.contains("secret-canary"), "received {received:?}");
    }

    #[test]
    fn refuses_every_top_level_value_that_is_not_an_object() {
        for (raw, kind) in [
            ("-1", "a number"),
            ("true", "a boolean"),
            ("null", "null"),
            ("\"--print\"", "a string"),
            ("[]", "an array"),
        ] {
            let received = refusal_received(raw);
            assert!(
                received.contains(kind),
                "expected {kind:?} named | received {received:?}"
            );
        }
    }

    #[test]
    fn refuses_empty_text() {
        refusal_received("");
    }

    #[test]
    fn refuses_a_schema_over_the_compact_cap_and_accepts_one_at_it() {
        let filler = |length: usize| format!("{{\"d\":\"{}\"}}", "x".repeat(length));
        // `{"d":""}` is 8 bytes of structure.
        let at_cap = filler(MAX_JSON_SCHEMA_BYTES - 8);
        assert_eq!(
            compact(&at_cap)
                .expect("expected the cap itself to pass")
                .len(),
            MAX_JSON_SCHEMA_BYTES
        );
        let received = refusal_received(&filler(MAX_JSON_SCHEMA_BYTES - 7));
        assert!(received.contains("compact bytes"), "received {received:?}");
    }

    #[test]
    fn refuses_oversized_text_before_parsing_it() {
        let received = refusal_received(&" ".repeat(MAX_JSON_SCHEMA_BYTES * 4 + 1));
        assert!(received.contains("bytes of text"), "received {received:?}");
    }

    #[test]
    fn names_each_json_kind() {
        use serde_json::json;
        for (value, kind) in [
            (json!(null), "null"),
            (json!(true), "a boolean"),
            (json!(1), "a number"),
            (json!("s"), "a string"),
            (json!([]), "an array"),
            (json!({}), "an object"),
        ] {
            assert_eq!(json_kind(&value), kind);
        }
    }
}
