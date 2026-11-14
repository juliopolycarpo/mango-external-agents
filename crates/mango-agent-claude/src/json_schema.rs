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
//! - the argument fits [`MAX_JSON_SCHEMA_ARGUMENT_UNITS`] as a Windows command line writes it.

use mango_external_agents::{Error, Result};

/// The most UTF-16 code units the schema argument may occupy on a Windows command line.
///
/// The schema travels inline, because the flag takes a value rather than a path. Windows caps a
/// whole command line at 32,767 UTF-16 code units, and Rust's `Command` writes an argument as
/// [`windows_argument_units`] counts it: every `"` becomes `\"` and the backslashes before it are
/// doubled. A compact schema dense in quotes therefore grows well past its byte length, so the
/// bound is on the escaped length. Half the cap leaves the other half for the program path, the
/// other flags, the session id and the MCP path. A host with a larger schema needs a surface the
/// vendor documents for it; none is documented for `--json-schema` today.
pub const MAX_JSON_SCHEMA_ARGUMENT_UNITS: usize = 16 * 1024;

/// What a refusal names as the accepted shape.
const EXPECTED_SHAPE: &str = "a JSON object, no larger than 16384 UTF-16 code units once compacted and escaped as a Windows command-line argument, that is a JSON Schema for --json-schema";

/// Checks a host's schema text and returns the compact form that goes on argv.
///
/// # Errors
///
/// [`Error::HostConfiguration`] naming the size (never the content) of what was received, when the
/// text is not JSON, is not an object, or does not fit [`MAX_JSON_SCHEMA_ARGUMENT_UNITS`].
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
    if raw.len() > MAX_JSON_SCHEMA_ARGUMENT_UNITS.saturating_mul(4) {
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
    let units = windows_argument_units(&compact);
    if units > MAX_JSON_SCHEMA_ARGUMENT_UNITS {
        return Err(refusal(format!(
            "a schema of {} bytes that takes {units} UTF-16 code units on a Windows command line",
            compact.len()
        )));
    }
    Ok(compact)
}

/// How many UTF-16 code units `argument` takes on a Windows command line, as Rust's `Command`
/// writes it.
///
/// The argument is counted as quoted, which is conservative: `Command` quotes only an argument
/// holding a space or tab. Inside the quotes each `"` is written as `\"`, and each backslash
/// directly before a quote (or before the closing quote) is written twice. Every other
/// character takes its UTF-16 length.
///
/// # Example
///
/// ```
/// use mango_agent_claude::json_schema::windows_argument_units;
///
/// assert_eq!(windows_argument_units("{}"), 4);
/// // The quote becomes `\"`: 2 units, plus the 2 surrounding quotes.
/// assert_eq!(windows_argument_units("\""), 4);
/// // A backslash before a quote is written twice: `\\\"`.
/// assert_eq!(windows_argument_units("\\\""), 6);
/// ```
pub fn windows_argument_units(argument: &str) -> usize {
    // The surrounding quotes.
    let mut units = 2;
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => {
                backslashes += 1;
                units += 1;
            }
            // The quote becomes `\"`, and each backslash before it is written a second time.
            '"' => {
                units += backslashes + 2;
                backslashes = 0;
            }
            other => {
                backslashes = 0;
                units += other.len_utf16();
            }
        }
    }
    // Backslashes before the closing quote are doubled too.
    units + backslashes
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
    use super::{MAX_JSON_SCHEMA_ARGUMENT_UNITS, compact, json_kind, windows_argument_units};
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
            // Sizes only: a schema fixture can be 20,000 characters long.
            Ok(argument) => panic!(
                "expected a host-configuration refusal for {} bytes of schema text | received an accepted argument of {} bytes taking {} Windows command-line units",
                raw.len(),
                argument.len(),
                windows_argument_units(&argument)
            ),
            Err(other) => panic!(
                "expected a host-configuration refusal for {} bytes of schema text | received {other:?}",
                raw.len()
            ),
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
    fn refuses_a_schema_over_the_argument_cap_and_accepts_one_at_it() {
        let filler = |length: usize| format!("{{\"d\":\"{}\"}}", "x".repeat(length));
        // `{"d":""}` takes 14 units quoted and escaped, and each `x` takes one.
        let at_cap = filler(MAX_JSON_SCHEMA_ARGUMENT_UNITS - 14);
        let argument = compact(&at_cap).expect("expected the cap itself to pass");
        assert_eq!(
            windows_argument_units(&argument),
            MAX_JSON_SCHEMA_ARGUMENT_UNITS
        );
        let received = refusal_received(&filler(MAX_JSON_SCHEMA_ARGUMENT_UNITS - 13));
        assert!(
            received.contains("16385 UTF-16 code units"),
            "received {received:?}"
        );
    }

    /// Each `"` in a schema string is `\"` in JSON and `\\\"` on a Windows command line, so a
    /// schema well under the cap in bytes can overflow it once escaped.
    #[test]
    fn refuses_a_quote_dense_schema_that_fits_in_bytes_but_not_once_escaped() {
        let quotes = 5_000;
        let raw = format!("{{\"d\":\"{}\"}}", "\\\"".repeat(quotes));
        let compact_bytes = 8 + 2 * quotes;
        assert!(
            compact_bytes < MAX_JSON_SCHEMA_ARGUMENT_UNITS,
            "expected the fixture to fit the cap in bytes | received {compact_bytes} bytes"
        );
        let received = refusal_received(&raw);
        assert_eq!(
            received,
            format!(
                "a schema of {compact_bytes} bytes that takes {} UTF-16 code units on a Windows command line",
                14 + 4 * quotes
            ),
            "expected the escaped length named | received {received:?}"
        );
    }

    #[test]
    fn accepts_a_quote_dense_schema_exactly_at_the_escaped_cap() {
        // 14 units of structure, 4 per escaped quote, then two filler characters.
        let raw = format!("{{\"d\":\"{}xx\"}}", "\\\"".repeat(4_092));
        let argument = compact(&raw).expect("expected the escaped cap itself to pass");
        assert_eq!(
            windows_argument_units(&argument),
            MAX_JSON_SCHEMA_ARGUMENT_UNITS
        );
        let over = format!("{{\"d\":\"{}xxx\"}}", "\\\"".repeat(4_092));
        refusal_received(&over);
    }

    #[test]
    fn counts_a_windows_argument_as_rusts_command_writes_it() {
        for (argument, units) in [
            ("", 2),
            ("{}", 4),
            // The quote becomes `\"`.
            ("\"", 4),
            // The backslash before a quote is doubled: `\\\"`.
            ("\\\"", 6),
            // Backslashes elsewhere are written once.
            ("a\\b", 5),
            // A backslash run before the closing quote is doubled.
            ("\\\\", 6),
            // A character outside the BMP takes two UTF-16 units.
            ("\u{1F600}", 4),
        ] {
            assert_eq!(
                windows_argument_units(argument),
                units,
                "expected {units} units | received {argument:?}"
            );
        }
    }

    #[test]
    fn refuses_oversized_text_before_parsing_it() {
        let received = refusal_received(&" ".repeat(MAX_JSON_SCHEMA_ARGUMENT_UNITS * 4 + 1));
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
