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

/// The longest command line `cmd.exe` accepts, in characters.
///
/// CreateProcess takes 32,767 UTF-16 code units, but Rust's `Command` runs a `.bat` or `.cmd`
/// program, such as an npm `claude.cmd` shim, through `cmd.exe`, which rejects a line past this.
/// The bound is the tighter one, so a host that launches the native binary has more room and
/// gets the same refusal.
pub(crate) const BATCH_COMMAND_LINE_LIMIT_UNITS: usize = 8_191;

/// What `Command` writes before the program for a batch launch.
const BATCH_LAUNCH_PREFIX: &str = "cmd.exe /e:ON /v:OFF /d /c \"";

/// Units set aside for the resolved launcher path and its quotes, which the library does not see.
///
/// The host's [`ExecutablePath`](mango_external_agents::ExecutablePath) replaces the program name
/// at spawn time. 1,024 is four times the legacy `MAX_PATH` of 260.
pub(crate) const LAUNCHER_PATH_RESERVE_UNITS: usize = 1_024;

/// The units a fixed overhead takes in a batch launch: the prefix, the launcher reserve and the
/// quote that closes the whole line.
const BATCH_OVERHEAD_UNITS: usize = BATCH_LAUNCH_PREFIX.len() + LAUNCHER_PATH_RESERVE_UNITS + 1;

/// What each of these ASCII arguments takes on the line: a separator and two quotes around it.
///
/// None of them holds a `"`, a backslash or a `%`, which a test pins, so the length is the count.
const fn plain_argument_units(arguments: &[&str]) -> usize {
    let mut units = 0;
    let mut index = 0;
    while index < arguments.len() {
        units += 1 + arguments[index].len() + 2;
        index += 1;
    }
    units
}

/// The most UTF-16 code units the schema argument may occupy on a Windows command line.
///
/// The schema travels inline, because the flag takes a value rather than a path. It is derived,
/// not chosen: the 8,191 characters `cmd.exe` accepts for a batch launch, less the launch prefix,
/// the launcher-path reserve, every fixed flag, the longest conditional flags and the schema's
/// own separator. A schema over this can never fit beside the flags every turn passes, so
/// `with_json_schema` refuses it before anything runs. The model, effort and MCP path vary per
/// turn, so `TurnArgv::build` counts the whole line again when a turn is built.
///
/// The count is [`windows_argument_units`]: every `"` becomes `\"` and the backslashes before it are
/// doubled, and a batch-file launcher also turns every `%` into 8 units. A compact schema dense in
/// quotes or percent signs therefore grows well past its byte length, so the bound is on the
/// escaped length. A host with a larger schema needs a surface the vendor documents for it; none
/// is documented for `--json-schema` today.
pub const MAX_JSON_SCHEMA_ARGUMENT_UNITS: usize = BATCH_COMMAND_LINE_LIMIT_UNITS
    - BATCH_OVERHEAD_UNITS
    - plain_argument_units(&crate::argv::FIXED_FLAGS)
    - plain_argument_units(&crate::argv::WORST_CASE_CONDITIONAL_FLAGS)
    // The separator before the schema itself; its own quotes are in its count.
    - 1;

/// What one `%` takes in a batch-file argument: `%%cd:~,` is written before the `%` itself.
///
/// Rust's `Command` runs a `.bat` or `.cmd` program through `cmd.exe` and rewrites each `%` this
/// way so `%VAR%` in an argument is not expanded.
const PERCENT_UNITS_IN_A_BATCH_ARGUMENT: usize = 8;

/// What a refusal names as the accepted shape.
const EXPECTED_SHAPE: &str = "a JSON object, no larger than 6861 UTF-16 code units once compacted and escaped as a Windows batch-launch command-line argument, that is a JSON Schema for --json-schema";

/// What a refusal of a whole turn command line names as the accepted shape.
const EXPECTED_COMMAND_LINE: &str = "a turn command line of at most 8191 UTF-16 code units as a cmd.exe batch launch writes it, counting the launch prefix, every quoted argument and 1024 units for the launcher path";

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
/// The count is an upper bound valid for both writers `Command` uses, because the host's launcher
/// may run an `.exe` or a `.bat`/`.cmd` shim such as an npm `claude.cmd`:
///
/// - The argument is counted as quoted. An `.exe` argument is quoted only when it holds a space or
///   tab, a batch argument when it holds a character cmd.exe treats as special.
/// - Each `"` takes 2 units (`\"` for an `.exe`, `""` for a batch file), and each backslash
///   directly before a quote, or before the closing quote, is written twice. Both writers agree.
/// - Each `%` takes 8 units. An `.exe` writes it as is (1 unit), but a batch file writes it as
///   `%%cd:~,%` so cmd.exe does not expand `%VAR%`, and the bound takes the larger.
/// - Every other character takes its UTF-16 length.
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
/// // A batch file writes `%` as `%%cd:~,%`.
/// assert_eq!(windows_argument_units("%"), 10);
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
            // `%%cd:~,%` is what a batch file's argument writer emits for each `%`. An `.exe`
            // writes 1 unit; the larger figure holds for either, so a `claude.cmd` launcher never
            // meets a limit the typed refusal at `with_json_schema` did not.
            '%' => {
                backslashes = 0;
                units += PERCENT_UNITS_IN_A_BATCH_ARGUMENT;
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

/// The units a whole turn command line takes when launched through a `.bat` or `.cmd` shim.
///
/// The worst case for either launcher: the batch prefix and closing quote, the reserve for the
/// launcher path in place of `argv[0]`, and each other argument counted by
/// [`windows_argument_units`] with its separator.
pub(crate) fn turn_command_line_units(argv: &[String]) -> usize {
    BATCH_OVERHEAD_UNITS
        + argv
            .iter()
            .skip(1)
            .map(|argument| 1 + windows_argument_units(argument))
            .sum::<usize>()
}

/// Refuses a turn command line that could not run when launched through a batch shim.
///
/// # Errors
///
/// [`Error::HostConfiguration`] naming the units the line takes and the cap, never an argument's
/// content: the line holds the schema and the MCP path.
pub(crate) fn check_turn_command_line(argv: &[String]) -> Result<()> {
    let units = turn_command_line_units(argv);
    if units > BATCH_COMMAND_LINE_LIMIT_UNITS {
        return Err(Error::HostConfiguration {
            expected: EXPECTED_COMMAND_LINE,
            received: format!("a command line of {units} UTF-16 code units"),
        });
    }
    Ok(())
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
            received.contains(&format!(
                "{} UTF-16 code units",
                MAX_JSON_SCHEMA_ARGUMENT_UNITS + 1
            )),
            "received {received:?}"
        );
    }

    /// The regression for an 8,000-character schema: under the earlier 16 KiB cap it was
    /// accepted, and a `claude.cmd` launch then died inside cmd.exe, which takes 8,191 characters
    /// for the whole line.
    #[test]
    fn refuses_an_eight_thousand_character_schema_the_fixed_flags_leave_no_room_for() {
        let raw = format!("{{\"d\":\"{}\"}}", "x".repeat(8_000));
        let received = refusal_received(&raw);
        assert_eq!(
            received,
            "a schema of 8008 bytes that takes 8014 UTF-16 code units on a Windows command line",
            "expected the escaped length named | received {received:?}"
        );
    }

    /// The cap is derived from the flags every turn passes, so it moves when they do. This pins
    /// the number, and the inputs it is derived from, so a change shows in review.
    #[test]
    fn the_schema_cap_is_derived_from_the_fixed_flags_and_the_batch_limit() {
        use super::{
            BATCH_COMMAND_LINE_LIMIT_UNITS, BATCH_LAUNCH_PREFIX, LAUNCHER_PATH_RESERVE_UNITS,
        };
        use crate::argv::{FIXED_FLAGS, WORST_CASE_CONDITIONAL_FLAGS};

        assert_eq!(BATCH_LAUNCH_PREFIX.len(), 28);
        // Each plain flag counts as its length, a separator and two quotes only while no flag
        // holds a character the writers escape.
        for flag in FIXED_FLAGS.iter().chain(&WORST_CASE_CONDITIONAL_FLAGS) {
            assert!(
                flag.is_ascii() && !flag.contains(['"', '\\', '%']),
                "expected a plain ASCII flag | received {flag:?}"
            );
            assert_eq!(windows_argument_units(flag), flag.len() + 2);
        }
        // 8191 - (28 + 1024 + 1) - 140 - 136 - 1
        assert_eq!(MAX_JSON_SCHEMA_ARGUMENT_UNITS, 6_861);
        assert_eq!(BATCH_COMMAND_LINE_LIMIT_UNITS, 8_191);
        assert_eq!(LAUNCHER_PATH_RESERVE_UNITS, 1_024);
    }

    #[test]
    fn the_refusal_texts_name_the_caps_the_code_enforces() {
        use super::{EXPECTED_COMMAND_LINE, EXPECTED_SHAPE};

        assert!(
            EXPECTED_SHAPE.contains(&MAX_JSON_SCHEMA_ARGUMENT_UNITS.to_string()),
            "expected the schema cap in the shape | received {EXPECTED_SHAPE:?}"
        );
        for number in [
            super::BATCH_COMMAND_LINE_LIMIT_UNITS,
            super::LAUNCHER_PATH_RESERVE_UNITS,
        ] {
            assert!(
                EXPECTED_COMMAND_LINE.contains(&number.to_string()),
                "expected {number} in the command-line shape | received {EXPECTED_COMMAND_LINE:?}"
            );
        }
    }

    #[test]
    fn counts_a_turn_command_line_as_a_batch_launch_writes_it() {
        use super::turn_command_line_units;

        // 28 for the prefix, 1024 for the launcher, 1 for the closing quote, then for each
        // argument after the program a separator and its quoted count.
        let argv = |arguments: &[&str]| -> Vec<String> {
            arguments
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect()
        };
        assert_eq!(turn_command_line_units(&argv(&["claude"])), 1_053);
        assert_eq!(turn_command_line_units(&argv(&["claude", "a"])), 1_057);
        // The program name is the reserve, whatever it is.
        assert_eq!(
            turn_command_line_units(&argv(&["C:\\very\\long\\claude.cmd", "a"])),
            1_057
        );
        assert_eq!(
            turn_command_line_units(&argv(&["claude", "%", "\""])),
            1_053 + 11 + 5
        );
    }

    #[test]
    fn refuses_a_command_line_over_the_batch_limit_naming_units_and_never_its_content() {
        use super::{
            BATCH_COMMAND_LINE_LIMIT_UNITS, check_turn_command_line, turn_command_line_units,
        };

        // One argument takes 1 + 2 + length units on top of the 1,053 fixed ones.
        let at_limit = vec![
            String::from("claude"),
            "s".repeat(BATCH_COMMAND_LINE_LIMIT_UNITS - 1_053 - 3),
        ];
        assert_eq!(
            turn_command_line_units(&at_limit),
            BATCH_COMMAND_LINE_LIMIT_UNITS
        );
        assert!(check_turn_command_line(&at_limit).is_ok());

        let mut over = at_limit;
        over[1].push_str("secret-canary");
        let Err(Error::HostConfiguration { expected, received }) = check_turn_command_line(&over)
        else {
            panic!("expected a host-configuration refusal for a command line over the batch limit");
        };
        assert!(expected.contains("8191"), "received {expected:?}");
        assert_eq!(
            received,
            format!(
                "a command line of {} UTF-16 code units",
                BATCH_COMMAND_LINE_LIMIT_UNITS + 13
            )
        );
        assert!(!received.contains("secret-canary"), "received {received:?}");
    }

    /// Each `"` in a schema string is `\"` in JSON and `\\\"` on a Windows command line, so a
    /// schema well under the cap in bytes can overflow it once escaped.
    #[test]
    fn refuses_a_quote_dense_schema_that_fits_in_bytes_but_not_once_escaped() {
        let quotes = 2_000;
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
        // 14 units of structure, 4 per escaped quote, then filler characters for the remainder.
        let room = MAX_JSON_SCHEMA_ARGUMENT_UNITS - 14;
        let (quotes, filler) = (room / 4, room % 4);
        let raw = format!(
            "{{\"d\":\"{}{}\"}}",
            "\\\"".repeat(quotes),
            "x".repeat(filler)
        );
        let argument = compact(&raw).expect("expected the escaped cap itself to pass");
        assert_eq!(
            windows_argument_units(&argument),
            MAX_JSON_SCHEMA_ARGUMENT_UNITS
        );
        let over = format!(
            "{{\"d\":\"{}{}\"}}",
            "\\\"".repeat(quotes),
            "x".repeat(filler + 1)
        );
        refusal_received(&over);
    }

    /// A `claude.cmd` launcher writes each `%` as `%%cd:~,%`, so 5,000 of them take 40,008 units
    /// and CreateProcess would refuse the first turn.
    #[test]
    fn refuses_a_percent_dense_schema_that_a_batch_launcher_would_expand_past_the_cap() {
        let percents = 5_000;
        let raw = format!("{{\"d\":\"{}\"}}", "%".repeat(percents));
        let received = refusal_received(&raw);
        assert_eq!(
            received,
            format!(
                "a schema of {} bytes that takes {} UTF-16 code units on a Windows command line",
                8 + percents,
                14 + 8 * percents
            ),
            "expected the batch-escaped length named | received {received:?}"
        );
    }

    #[test]
    fn accepts_a_percent_dense_schema_exactly_at_the_cap_and_refuses_one_unit_over() {
        // 14 units of structure, 8 per `%`, then filler characters for the remainder.
        let room = MAX_JSON_SCHEMA_ARGUMENT_UNITS - 14;
        let (percents, filler) = (room / 8, room % 8);
        let schema = |percents: usize, filler: usize| {
            format!(
                "{{\"d\":\"{}{}\"}}",
                "%".repeat(percents),
                "x".repeat(filler)
            )
        };
        let argument = compact(&schema(percents, filler)).expect("expected the cap itself to pass");
        assert_eq!(
            windows_argument_units(&argument),
            MAX_JSON_SCHEMA_ARGUMENT_UNITS
        );
        let received = refusal_received(&schema(percents, filler + 1));
        assert!(
            received.contains(&format!(
                "{} UTF-16 code units",
                MAX_JSON_SCHEMA_ARGUMENT_UNITS + 1
            )),
            "received {received:?}"
        );
        // One more `%` is 8 units over, not 1.
        let received = refusal_received(&schema(percents + 1, filler));
        assert!(
            received.contains(&format!(
                "{} UTF-16 code units",
                MAX_JSON_SCHEMA_ARGUMENT_UNITS + 8
            )),
            "received {received:?}"
        );
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
            // A batch file writes `%` as `%%cd:~,%`.
            ("%", 10),
            ("a%b", 12),
            // A `%` ends a backslash run, so the next quote doubles nothing.
            ("\\%\"", 13),
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
