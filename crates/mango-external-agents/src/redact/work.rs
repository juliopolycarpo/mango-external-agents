//! The work the redactor does is proportional to the text it is given.
//!
//! A rule that is asked at each of many candidates, and reads the same bytes again for every one,
//! is linear on ordinary text and quadratic on text built to repeat that read: a credential name
//! made of a keyword repeated many times, or a URL scheme made of dotted letters. Neither needs
//! more than a hundred kilobytes to hold a caller for seconds, and the default stderr cap bounds
//! one caller only, so these tests count the work rather than time it. The count is
//! [`steps`](super::scan::steps): every byte a scanner looks at, the same on every machine.
//!
//! Each shape is measured at two lengths. The absolute budget holds the work to a small multiple
//! of the length, and the doubling check holds its growth to the growth of the text, so a rule
//! that is linear only up to some size still fails.

use super::scan::steps;
use super::{ends_awaiting_value, message_text, stderr_text};
use crate::ExtensionValue;

/// A short text and one twice its size, both far past what a quadratic rule survives in a test.
const SMALL: usize = 16 * 1024;

/// What a byte of text may cost across the passes (the stripper and the three rules, with the
/// keyword look-ups behind them): 1 to 9 on the shapes below, and up to twice that where a removed
/// byte makes the rules read the text a second time, so 24 leaves room for a compiler or
/// a pass more without hiding a rule that reads a suffix per candidate, which costs thousands of
/// steps a byte at this size.
const STEPS_PER_BYTE: usize = 24;

/// [`ends_awaiting_value`] runs two rules over the text four times and sweeps it once more.
const AWAITING_STEPS_PER_BYTE: usize = 40;

/// Doubling the text may add a little more than double the work: block searches round up to a
/// whole block, each shape has a constant tail, and a read bounded by a count of lines or items
/// (a container, a sequence) costs less than a byte for each byte until its bound is reached.
const DOUBLING_SLACK: usize = 4096;

/// One shape of text that has cost a rule more than its length, and the length it is built to.
struct Shape {
    what: &'static str,
    build: fn(usize) -> String,
}

fn repeated(unit: &str, tail: &str, len: usize) -> String {
    format!("{}{tail}", unit.repeat(len / unit.len()))
}

/// `text` cut or padded with spaces to `len` bytes, for a shape whose size is not a multiple of
/// its unit and would otherwise double to a little more or less than twice.
fn exactly(mut text: String, len: usize) -> String {
    text.truncate(len);
    format!("{text:<len$}")
}

const SHAPES: &[Shape] = &[
    Shape {
        what: "a keyword repeated in a name with no value",
        build: |len| repeated("token_", "=", len),
    },
    Shape {
        what: "a keyword repeated in a name, then a colon and spaces with no value",
        build: |len| repeated("secret-", ":   ", len),
    },
    Shape {
        what: "a keyword repeated in a name, then an empty value before a comma",
        build: |len| repeated("api_key_", "=  ,rest", len),
    },
    Shape {
        what: "a keyword repeated in a name with a value to redact",
        build: |len| repeated("password_", "=value", len),
    },
    Shape {
        what: "dotted letters before a URL scheme separator and no authority",
        build: |len| repeated("a.", "://", len),
    },
    Shape {
        what: "dotted letters before a URL scheme separator and a user with no password",
        build: |len| repeated("a.", "://user@host", len),
    },
    Shape {
        what: "dotted letters before a URL scheme separator and a password with no host",
        build: |len| repeated("a.", "://user:secret/x", len),
    },
    Shape {
        what: "dotted letters before a URL scheme separator and a password to redact",
        build: |len| repeated("a.", "://user:secret@host", len),
    },
    Shape {
        what: "a bearer header name after many spaces",
        build: |len| format!("authorization{}:", " ".repeat(len)),
    },
    Shape {
        what: "every byte a separator",
        build: |len| ":=".repeat(len / 2),
    },
    Shape {
        what: "a keyword and a scheme in turn, each rejected",
        build: |len| repeated("token_a.", "=://", len),
    },
    Shape {
        what: "unterminated string escapes",
        build: |len| repeated("\u{1b}]0;title", "", len),
    },
    Shape {
        what: "escape introducers with nothing after them",
        build: |len| "\u{1b}[".repeat(len / 2),
    },
    Shape {
        what: "escape sequences between name fragments",
        build: |len| repeated("tok\u{1b}[1men_", "=v", len),
    },
    Shape {
        what: "a control byte every few bytes",
        build: |len| repeated("\u{0}a\u{7}\r\u{1b}b", "", len),
    },
    Shape {
        what: "eight-bit string introducers that never end",
        build: |len| repeated("\u{9d}title\u{90}x", "", len),
    },
    Shape {
        what: "a removed byte before every name",
        build: |len| repeated("x\rtoken=v ", "", len),
    },
    Shape {
        what: "a removed byte between a scheme and its token",
        build: |len| repeated("authorization: bearer\rsk ", "", len),
    },
    Shape {
        what: "a removed byte after every letter of a keyword",
        build: |len| repeated("s\u{0}e\u{0}c\u{0}r\u{0}e\u{0}t\u{0}=v ", "", len),
    },
    Shape {
        what: "keywords with no separator at all",
        build: |len| repeated("token password secret ", "", len),
    },
    Shape {
        what: "nothing but colons",
        build: |len| ":".repeat(len),
    },
    Shape {
        what: "one assignment after another with one-letter names",
        build: |len| repeated("a=", "", len),
    },
    Shape {
        what: "one long name made of short words before a single assignment",
        build: |len| repeated("a_", "=v", len),
    },
    Shape {
        what: "spaces before a colon with no name",
        build: |len| format!("{}:", " ".repeat(len)),
    },
    Shape {
        what: "scheme separators one after another with one-letter schemes",
        build: |len| repeated("a://", "", len),
    },
    Shape {
        what: "a quoted keyword repeated in a name with no value",
        build: |len| repeated("\"token_", "\":", len),
    },
    Shape {
        what: "nothing but quotes and a colon",
        build: |len| repeated("\"", ":x", len),
    },
    Shape {
        what: "a keyword glued to letters, repeated, before one assignment",
        build: |len| repeated("atoken", "=v", len),
    },
    Shape {
        what: "a keyword repeated in a name whose value says it is missing",
        build: |len| repeated("token_", ": expired", len),
    },
    Shape {
        what: "a plural keyword repeated in a name whose value is a count",
        build: |len| repeated("tokens_", ": 100000", len),
    },
    Shape {
        what: "a quote that is never closed after a name",
        build: |len| repeated("token=\"", "a", len),
    },
    Shape {
        what: "a quoted value of backslashes",
        build: |len| repeated("api_key='", &"\\".repeat(len), len),
    },
    Shape {
        what: "a password colon repeated on one long line",
        build: |len| repeated("password: ", "", len),
    },
    Shape {
        what: "a cookie colon repeated on one long line",
        build: |len| repeated("cookie: ", "", len),
    },
    Shape {
        what: "query parameters one after another",
        build: |len| repeated("?key=a&token=b", "", len),
    },
    Shape {
        what: "status words one after another after a name",
        build: |len| repeated("token: ", &"not ".repeat(len / 4), len),
    },
    Shape {
        what: "authorization headers one after another with unusual schemes",
        build: |len| repeated("authorization: x y ", "", len),
    },
    Shape {
        what: "a status after a name with no space, repeated on one line",
        build: |len| repeated("token:set.", "", len),
    },
    Shape {
        what: "a cookie status repeated on one line",
        build: |len| repeated("Cookie: set.", "", len),
    },
    Shape {
        what: "a password and a status repeated on one line",
        build: |len| repeated("password: not set.", "", len),
    },
    Shape {
        what: "an authorization status repeated on one line",
        build: |len| repeated("Authorization: 401 Unauthorized.", "", len),
    },
    Shape {
        what: "an authorization reason that is no status, repeated on one line",
        build: |len| repeated("Authorization: bad bad.", "", len),
    },
    Shape {
        what: "a keyword repeated in a name before an empty quoted value",
        build: |len| repeated("token_", "=\"\"", len),
    },
    Shape {
        what: "a quoted keyword repeated in a name before a bare closing brace",
        build: |len| format!("{{\"{}\":}}", "token_".repeat(len / 6)),
    },
    Shape {
        what: "a keyword repeated in a query name before an ampersand",
        build: |len| format!("?{}=&", "token_".repeat(len / 6)),
    },
    Shape {
        what: "a capitalised keyword repeated before a blank and `key`",
        build: |len| repeated("token", &"Token".repeat(len / 5), len / 5) + " key=v",
    },
    Shape {
        what: "a brace and a string that never close, repeated",
        build: |len| repeated("token=[\"", "{[", len),
    },
    Shape {
        what: "a known scheme glued to a colon, repeated on one line",
        build: |len| repeated("Authorization:Bearer:", "", len),
    },
    Shape {
        what: "a multi-part scheme glued to a comma, repeated on one line",
        build: |len| repeated("Authorization:Digest,", "", len),
    },
    Shape {
        what: "a known scheme glued to an equals sign, repeated",
        build: |len| repeated("Authorization:Basic=", "", len),
    },
    Shape {
        what: "an authorization header opening a quote that never closes, repeated",
        build: |len| repeated("Authorization:'x", "", len),
    },
    Shape {
        what: "an array that never closes under a password, repeated",
        build: |len| repeated("password:[\"", "", len),
    },
    Shape {
        what: "an escaped string that never closes inside an array, repeated",
        build: |len| repeated("token=[\\\"a", "", len),
    },
    Shape {
        what: "two different keywords alternating in one name before a status",
        build: |len| repeated("token_secret_", ": expired", len),
    },
    Shape {
        what: "a token and a cookie alternating in one name before an empty value",
        build: |len| repeated("token_cookie_", "=,", len),
    },
    Shape {
        what: "ampersands with no query string, repeated",
        build: |len| repeated("&token=x", "", len),
    },
    Shape {
        what: "a quoted value with text glued to its closing quote, repeated",
        build: |len| repeated("password=\"a\"b", "", len),
    },
    Shape {
        what: "a quoted value with a glued tail that never ends, repeated",
        build: |len| repeated("token=\"a\"", &"b".repeat(len), len),
    },
    Shape {
        what: "query-only names after ampersands with no query string, repeated",
        build: |len| repeated("&key=x", "", len),
    },
    Shape {
        what: "ampersand parameters with no query string, repeated",
        build: |len| repeated("&a=", "", len),
    },
    Shape {
        what: "a question mark ahead of 341 query-only names, repeated",
        build: |len| repeated(&format!("?{}", "&key=x".repeat(341)), "", len),
    },
    Shape {
        what: "one question mark far ahead of ampersand parameters",
        build: |len| format!("?{}", "&a=b".repeat(len / 4)),
    },
    Shape {
        what: "one question mark far ahead of query-only names",
        build: |len| format!("?{}", "&key=x".repeat(len / 6)),
    },
    Shape {
        what: "query-only names glued with dots, repeated",
        build: |len| repeated("x.&auth=1", "", len),
    },
    Shape {
        what: "plain words that start with a keyword, repeated",
        build: |len| repeated("tokenizer: ", "", len),
    },
    Shape {
        what: "token-length userinfo with no password, repeated",
        build: |len| repeated("a://aaaaaaaaaaaaaaaaaaa1@", "", len),
    },
    Shape {
        what: "a run of backslashes as long as the text, then a quoted name",
        build: |len| format!("{}\"token\":x", "\\".repeat(len)),
    },
    Shape {
        what: "a run of backslashes before every quote of a name, repeated",
        build: |len| repeated(&format!("{}\"token_", "\\".repeat(31)), "\":", len),
    },
    Shape {
        what: "a run of backslashes one past the bound before a quote, repeated",
        build: |len| repeated(&format!("api_key={}\"a", "\\".repeat(32)), "", len),
    },
    Shape {
        what: "a value opened with three backslashes that is never closed, repeated",
        build: |len| repeated("api_key=\\\\\\\"a\\\"", "", len),
    },
    Shape {
        what: "a value of backslashes and quotes of every count but its own",
        build: |len| repeated("api_key=\\\\\\\"a\\\"b\\\\\\\\\\\"", "", len),
    },
    Shape {
        what: "a bracket opening a string with 31 backslashes that never closes, repeated",
        build: |len| {
            repeated(
                &format!("token=[{}\"a{}", "\\".repeat(31), " ".repeat(24)),
                "",
                len,
            )
        },
    },
    Shape {
        what: "a long run of backslashes inside an array under a credential",
        build: |len| format!("token=[\\\"a{}\\\"]", "\\".repeat(len)),
    },
    Shape {
        what: "a long run of backslashes inside an array outside any string",
        build: |len| format!("token=[{}]", "\\".repeat(len)),
    },
    Shape {
        what: "an array opened at the end of every line that never closes",
        build: |len| repeated("token: [\n", "", len),
    },
    Shape {
        what: "an array with entries on every line that never closes, repeated",
        build: |len| repeated("\"token\": [\n  \"a\",\n", "", len),
    },
    Shape {
        what: "a quoted name and an object opened at the end of every line",
        build: |len| repeated("{\"password\":{\n", "", len),
    },
    Shape {
        what: "arrays that close one line past the line bound, repeated",
        build: |len| {
            repeated(
                &format!("token: [\n{}]\n{}", "a,\n".repeat(65), " ".repeat(50)),
                "",
                len,
            )
        },
    },
    Shape {
        what: "arrays that close on the last line the bound allows, repeated",
        build: |len| {
            repeated(
                &format!("token: [\n{}]\n{}", "a,\n".repeat(61), " ".repeat(62)),
                "",
                len,
            )
        },
    },
    Shape {
        what: "one array as long as the text, one entry to a line",
        build: |len| exactly(format!("token: [\n{}]", "a,\n".repeat(len / 3)), len),
    },
    Shape {
        what: "strings that never close inside arrays that span lines, repeated",
        build: |len| repeated("token: [\n\"a\n'b\n", "", len),
    },
    Shape {
        what: "closing brackets and braces only, after a name",
        build: |len| format!("token: [{}", "]}".repeat(len / 2)),
    },
    Shape {
        what: "a sequence item under a name, repeated",
        build: |len| repeated("tokens:\n  - a\n", "", len),
    },
    Shape {
        what: "one sequence as long as the text",
        build: |len| exactly(format!("tokens:\n{}", "  - a\n".repeat(len / 6)), len),
    },
    Shape {
        what: "sequence markers with nothing after them, repeated",
        build: |len| repeated("tokens:\n  -\n", "", len),
    },
    Shape {
        what: "sequence markers in every column, repeated",
        build: |len| repeated("tokens:\n - a\n  - b\n   - c\n", "", len),
    },
    Shape {
        what: "blank lines between sequence items",
        build: |len| repeated("tokens:\n  - a\n\n\n\n\n", "", len),
    },
    Shape {
        what: "escaped JSON quotes, three deep, one after another",
        build: |len| repeated("\\\\\\\"api_key\\\\\\\":\\\\\\\"v\\\\\\\" ", "", len),
    },
];

fn redaction_steps(text: &str) -> usize {
    steps::counted(|| {
        std::hint::black_box(stderr_text(std::hint::black_box(text)));
    })
}

/// The entry point a vendor's messages take: the rules of `stderr_text` and the token pass.
fn message_steps(text: &str) -> usize {
    steps::counted(|| {
        std::hint::black_box(message_text(std::hint::black_box(text)));
    })
}

fn awaiting_steps(text: &str) -> usize {
    steps::counted(|| {
        std::hint::black_box(ends_awaiting_value(std::hint::black_box(text)));
    })
}

fn extension_steps(text: &str) -> usize {
    steps::counted(|| {
        std::hint::black_box(ExtensionValue::text(text).normalized());
    })
}

/// Fails, naming every shape that broke and both lengths, unless `measure` is within the budget
/// at each length and at most about doubles when the text does.
fn assert_work_is_linear(
    caller: &str,
    shapes: &[&[Shape]],
    measure: fn(&str) -> usize,
    per_byte: usize,
) {
    let mut failures = Vec::new();
    for shape in shapes.iter().copied().flatten() {
        let small = (shape.build)(SMALL);
        let large = (shape.build)(2 * SMALL);
        let (small_steps, large_steps) = (measure(&small), measure(&large));
        for (len, steps) in [(small.len(), small_steps), (large.len(), large_steps)] {
            let budget = per_byte * len;
            if steps > budget {
                failures.push(format!(
                    "expected scan steps <= {budget} ({per_byte} per byte) for {} at {len} \
                     bytes | received: {steps}",
                    shape.what
                ));
            }
        }
        let allowed = 2 * small_steps + DOUBLING_SLACK;
        if large_steps > allowed {
            failures.push(format!(
                "expected scan steps <= {allowed} (twice {small_steps} plus slack) for {} when {} \
                 bytes became {} | received: {large_steps}",
                shape.what,
                small.len(),
                large.len()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "expected linear work through {caller} for every shape | {}",
        failures.join("\n  ")
    );
}

#[test]
fn redaction_work_is_proportional_to_the_text_for_every_shape() {
    assert_work_is_linear(
        "redact::stderr_text",
        &[SHAPES],
        redaction_steps,
        STEPS_PER_BYTE,
    );
}

/// Extension normalization redacts the whole value before it keeps 128 code points, so the bound
/// protects nothing: the redactor has to be linear for this caller as well.
#[test]
fn extension_normalization_work_is_proportional_to_the_value_for_every_shape() {
    assert_work_is_linear(
        "ExtensionValue::normalized",
        &[SHAPES],
        extension_steps,
        STEPS_PER_BYTE,
    );
}

/// The tail a stderr discard asks about when a cut falls inside a line: it strips the text, sweeps
/// it for a keyword, and redacts it again with four probes appended, so it is allowed more steps
/// a byte than one redaction and has to stay proportional all the same.
#[test]
fn ends_awaiting_value_work_is_proportional_to_the_text_for_every_shape() {
    assert_work_is_linear(
        "redact::ends_awaiting_value",
        &[SHAPES],
        awaiting_steps,
        AWAITING_STEPS_PER_BYTE,
    );
}

/// Shapes built against the token pass: runs that nearly are tokens, tokens that run on, and
/// private-key blocks that never end. A scan that reads a run again for each prefix it might begin,
/// or an end marker again for each begin marker, fails here.
const TOKEN_SHAPES: &[Shape] = &[
    Shape {
        what: "one run of name characters with no token in it",
        build: |len| "a".repeat(len),
    },
    Shape {
        what: "a token prefix and a body that runs the whole text",
        build: |len| format!("{}{}", concat!("gh", "p_"), "a".repeat(len)),
    },
    Shape {
        what: "a token prefix repeated inside one run",
        build: |len| repeated(concat!("gh", "p_"), "", len),
    },
    Shape {
        what: "a hyphenated prefix repeated inside one run",
        build: |len| repeated(concat!("s", "k-live-"), "", len),
    },
    Shape {
        what: "token prefixes one after another with a body too short",
        build: |len| repeated(concat!("gh", "p_abc "), "", len),
    },
    Shape {
        what: "access key ids one after another",
        build: |len| repeated(concat!("AK", "IA0123456789ABCDEF "), "", len),
    },
    Shape {
        what: "access key ids a character too long, one after another",
        build: |len| repeated(concat!("AK", "IA0123456789ABCDEFG "), "", len),
    },
    Shape {
        what: "web tokens run together on dots",
        build: |len| repeated(concat!("ey", "Jaaaaaaaaaaaa."), "", len),
    },
    Shape {
        what: "web token headers with a claims part and no signature dot",
        build: |len| {
            repeated(
                concat!("ey", "Jaaaaaaaaaaaa.", "ey", "Jaaaaaaaaaaaa "),
                "",
                len,
            )
        },
    },
    Shape {
        what: "web token headers with a second part that is no JSON",
        build: |len| repeated(concat!("ey", "Jaaaaaaaaaaaa.xx "), "", len),
    },
    Shape {
        what: "a web token whose parts run the whole text",
        build: |len| {
            format!(
                "{}{}.{}.{}",
                concat!("ey", "J"),
                "a".repeat(len / 3),
                concat!("ey", "Jb"),
                "c".repeat(len / 3)
            )
        },
    },
    Shape {
        what: "an OAuth prefix repeated with a body too short",
        build: |len| repeated(concat!("ya", "29.abc "), "", len),
    },
    Shape {
        what: "an OAuth prefix and a body that runs the whole text",
        build: |len| format!("{}{}", concat!("ya", "29."), "a".repeat(len)),
    },
    Shape {
        what: "private-key begin markers with no end marker",
        build: |len| repeated(concat!("-----BEGIN PRIV", "ATE KEY-----\n"), "", len),
    },
    Shape {
        what: "private-key begin markers, each with a long line of base64 under it and no end marker",
        build: |len| {
            repeated(
                &format!(
                    concat!("-----BEGIN PRIV", "ATE KEY-----\n{}\n"),
                    "Ab+/".repeat(16)
                ),
                "",
                len,
            )
        },
    },
    Shape {
        what: "a begin marker and more lines of base64 than the body bound takes, repeated",
        build: |len| {
            let unit = format!(
                concat!("-----BEGIN PRIV", "ATE KEY-----\n{}"),
                format!("{}\n", "Ab+/".repeat(16)).repeat(140)
            );
            exactly(unit.repeat(len / unit.len() + 1), len)
        },
    },
    Shape {
        what: "private-key begin markers escaped as a JSON string writes them, no end marker",
        build: |len| {
            repeated(
                &format!(
                    concat!("-----BEGIN PRIV", "ATE KEY-----\\n{}\\n"),
                    "Ab+/".repeat(16)
                ),
                "",
                len,
            )
        },
    },
    Shape {
        what: "certificate begin markers, which are not secrets",
        build: |len| repeated("-----BEGIN CERTIFICATE-----\n", "", len),
    },
    Shape {
        what: "begin markers with a label that never closes",
        build: |len| repeated(&format!("-----BEGIN {} ", "A".repeat(52)), "", len),
    },
    Shape {
        what: "end markers with no begin marker",
        build: |len| repeated(concat!("-----END PRIV", "ATE KEY-----\n"), "", len),
    },
    Shape {
        what: "private-key blocks one after another",
        build: |len| {
            repeated(
                concat!(
                    "-----BEGIN PRIV",
                    "ATE KEY-----\nQQ==\n-----END PRIV",
                    "ATE KEY-----\n"
                ),
                "",
                len,
            )
        },
    },
    Shape {
        what: "hyphens only",
        build: |len| "-".repeat(len),
    },
    Shape {
        what: "JSON line breaks before a run, repeated",
        build: |len| repeated(concat!("\\ngh", "p_abc "), "", len),
    },
    Shape {
        what: "a backslash and a letter before every run",
        build: |len| repeated("\\nnnnnnnnnn ", "", len),
    },
];

/// The token pass is read for every message a vendor sends, so it is held to the same budget as
/// the rules, over the shapes above and over every shape of the rules.
#[test]
fn message_redaction_work_is_proportional_to_the_text_for_every_shape() {
    assert_work_is_linear(
        "redact::bounded",
        &[SHAPES, TOKEN_SHAPES],
        message_steps,
        STEPS_PER_BYTE,
    );
}

/// The scan reads a byte of the text and a byte of each run it is in, and a begin marker with no
/// end marker takes the rest of the text, so each of these has to record at least its length.
#[test]
fn the_token_pass_counts_its_steps() {
    let len = 4096;
    for (what, text, at_least) in [
        ("plain text", "x ".repeat(len / 2), len),
        ("one long run", "x".repeat(len), len),
        (
            "a begin marker with no end marker",
            format!(
                "{}{}",
                concat!("-----BEGIN PRIV", "ATE KEY-----\n"),
                "x ".repeat(len / 2)
            ),
            len,
        ),
    ] {
        let received = message_steps(&text);
        assert!(
            received >= at_least,
            "expected {what} of {} bytes to record >= {at_least} scan steps through the token pass \
             | received: {received}",
            text.len()
        );
    }
}

#[test]
fn the_step_count_follows_the_text_it_is_given() {
    let short = redaction_steps(&"error: nothing to see here\n".repeat(64));
    let long = redaction_steps(&"error: nothing to see here\n".repeat(128));
    assert!(
        short > 0 && long > short,
        "expected a longer plain text to count more scan steps than a shorter one | received \
         {short} then {long}"
    );
}

/// A loop that reads every byte and does not count is invisible to the budgets above, so each
/// loop the budgets rely on is shown to count: its text has to record at least the steps below.
#[test]
fn every_loop_over_the_text_counts_its_steps() {
    let len = 4096;
    for (what, text, at_least) in [
        ("escape introducers", "\u{1b}[".repeat(len / 2), len / 2),
        (
            "a run of removed bytes",
            "\u{0}\u{7}\r\u{b}".repeat(len / 4),
            len,
        ),
        (
            "dense controls",
            repeated("\u{0}a\u{7}\r", "", len),
            len / 4,
        ),
        (
            "a removed byte before each name",
            repeated("x\rtoken ", "", len),
            len / 8,
        ),
        ("text with nothing to strip", "x".repeat(len), len),
        (
            "a quoted value",
            format!("token=\"{}\"", "a".repeat(len)),
            4 * len,
        ),
        (
            "a cookie line with trailing blanks",
            format!("Cookie: x{}\n", " ".repeat(len)),
            5 * len,
        ),
    ] {
        let received = redaction_steps(&text);
        assert!(
            received >= at_least,
            "expected {what} of {} bytes to record >= {at_least} scan steps through stderr_text | \
             received: {received}",
            text.len()
        );
    }
    // The keyword sweep of `ends_awaiting_value` adds a step for each byte on top of the
    // stripper's search, and a probe copy adds the text again for each of its four probes.
    let text = "x".repeat(len);
    let received = awaiting_steps(&text);
    assert!(
        received >= 2 * len,
        "expected the keyword sweep of {len} bytes to record >= {} scan steps through \
         ends_awaiting_value | received: {received}",
        2 * len
    );
    let marked = format!("{text}token");
    let received = awaiting_steps(&marked);
    assert!(
        received >= 6 * len,
        "expected four probe copies of {} bytes to record >= {} scan steps through \
         ends_awaiting_value | received: {received}",
        marked.len(),
        6 * len
    );
}

/// The work counter is per thread, so a test sees only the work it did itself.
#[test]
fn the_step_count_ignores_work_done_on_another_thread() {
    let text = repeated("token_", "=", SMALL);
    let own = redaction_steps(&text);
    let counted = steps::counted(|| {
        std::thread::scope(|scope| {
            scope.spawn(|| std::hint::black_box(stderr_text(&text)));
        });
    });
    assert!(
        own > 0 && counted == 0,
        "expected work on another thread to count 0 steps here | received {counted} (the same \
         text counts {own} on this thread)"
    );
}

/// What the redactor still has to produce for the shapes above: the budget means nothing if the
/// text it guards came out wrong.
#[test]
fn the_shapes_that_carry_a_credential_are_still_redacted() {
    let text = repeated("password_", "=value", 64);
    let expected = format!("{}=[REDACTED]", "password_".repeat(7));
    assert_eq!(
        stderr_text(&text),
        expected,
        "expected the final value redacted in {text:?}"
    );

    let text = repeated("a.", "://user:secret@host", 8);
    assert_eq!(
        stderr_text(&text),
        "a.a.a.a.://user:[REDACTED]@host",
        "expected the password after a long scheme redacted in {text:?}"
    );
}

#[test]
fn a_normalized_extension_value_is_still_bounded_after_redaction() {
    let text = repeated("token_", "=", SMALL);
    let normalized = ExtensionValue::text(text).normalized();
    let kept = match &normalized {
        Some(ExtensionValue::Text(kept)) => kept.chars().count(),
        _ => 0,
    };
    assert_eq!(
        kept, 128,
        "expected 128 code points of redacted text kept | received {normalized:?}"
    );
}

/// The end marker of a block that is past the window is searched for once, not once for each
/// begin marker, so a text of many such blocks costs a constant number of steps for each byte.
#[test]
fn begin_markers_whose_end_marker_is_past_the_window_cost_a_constant_for_each_byte() {
    let unit = format!(
        concat!(
            "-----BEGIN PRIV",
            "ATE KEY-----{}-----BEGIN PRIV",
            "ATE KEY-----{}-----END PRIV",
            "ATE KEY-----\n"
        ),
        " x".repeat(8200),
        " x".repeat(100)
    );
    let mut per_byte = Vec::new();
    for copies in [1, 4, 16] {
        let text = unit.repeat(copies);
        per_byte.push(message_steps(&text) as f64 / text.len() as f64);
    }
    let (first, last) = (per_byte[0], per_byte[2]);
    assert!(
        last <= first * 1.5 + 1.0 && last <= 24.0,
        "expected steps for each byte to stay flat from 1 to 16 copies | received: {per_byte:?}"
    );
}
