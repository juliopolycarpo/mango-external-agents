//! `redact::stderr_text`: what a stderr tail costs to cross a diagnostic boundary.
//!
//! The redactor reads the whole tail several times: once to take control text out, once per
//! credential rule. Its cost follows what the text holds, so each size is run over four kinds of
//! tail: lines with nothing to remove, lines that carry credentials, coloured output with control
//! characters throughout, and lines dense in the punctuation and lead bytes the redactor has to
//! stop at without finding anything. Two more shapes are built to repeat a read for each word
//! start, a credential name made of one keyword over and over and a URL scheme made of dotted
//! letters, at 16 KiB, 120 KB and 1 MiB: their cost has to grow with the length, not with its
//! square. `stderr_text` reads what the release before it read and nothing more, so a tail costs
//! what it did. `redact::bounded` is the entry point for a vendor's messages and reads the rest:
//! JSON inside a string at three levels of escaping, a container that spans lines, a YAML
//! sequence and the unnamed-token pass. It is run over the same texts, beside `stderr_text`.
//! `redact::stderr_tail` is the entry point for a tail read once, when a failure is reported: the
//! rules of `stderr_text` and the token pass and nothing else. It is run over 16 KiB tails of plain
//! lines, credentials, tokens and headers, each beside `stderr_text` over the same text, and over
//! five 1 MiB shapes built to make the token pass or the cut-key read look again. The fixtures
//! are synthetic; no vendor wrote them. Run with:
//!
//! ```sh
//! cargo bench -p mango-external-agents --bench redact
//! ```

mod support;

use mango_external_agents::normalize::TextLimit;
use mango_external_agents::redact::{bounded, stderr_tail, stderr_text};
use support::{Bench, Unit};

const KIB: usize = 1024;

/// Bytes redacted per sample at every size, so a case is milliseconds rather than noise and the
/// two sizes of one kind of text can be read against each other.
const BYTES_PER_SAMPLE: usize = 320 * KIB;

/// Diagnostic lines with no credential, no escape sequence and no control character but the line
/// feed: what most tails are.
const PLAIN_LINES: &str = concat!(
    "error: the vendor exited with status 1 (no configuration found)\n",
    "warning: retrying request 3 of 5 after 250 ms\n",
    "  at Object.spawn (node:internal/child_process:420:11)\n",
    "thread 'main' panicked at src/main.rs:42:9: called `Option::unwrap()` on a `None` value\n",
    "note: run with RUST_BACKTRACE=1 to display a backtrace\n",
    "info: loaded 12 tools from 3 servers in 87 ms; cache hit rate 0.93, 4 stale\n",
);

/// The same kind of lines with each credential shape among them: a bearer and a basic header, an
/// assignment under every keyword, and a URL with a password.
const SECRET_LINES: &str = concat!(
    "error: request failed with status 401\n",
    "  Authorization: Bearer sk-live-0123456789abcdef\n",
    "  OPENAI_API_KEY=sk-proj-0123456789 api-key: abcdef\n",
    "warning: retrying request 3 of 5 after 250 ms\n",
    "  env: GITHUB_TOKEN=ghp_0123456789 client_secret = shh-0123\n",
    "  proxy-authorization : basic dXNlcjpodW50ZXIy\n",
    "retry at https://user:hunter2@agent.internal/v1/messages?stream=true\n",
    "  password: hunter2; passwd=hunter2, credential=opaque\n",
    "note: run with RUST_BACKTRACE=1 to display a backtrace\n",
);

/// Coloured progress output: SGR sequences, a window title, an erased line, a carriage return, a
/// character-set designation, an 8-bit introducer and a bidirectional override.
const CONTROL_LINES: &str = concat!(
    "\u{1b}[1;31merror\u{1b}[0m: the vendor exited with status \u{1b}[33m1\u{1b}[0m\n",
    "\u{1b}]0;agent: running\u{7}\u{1b}[2K\rdownloading \u{1b}[32m42%\u{1b}[0m\r",
    "\u{1b}(B\u{1b}[m  at \u{1b}[4msrc/main.rs\u{1b}[24m:42:9 \u{9b}0m\u{0}\n",
    "caf\u{e9} \u{202e}gpj.exe\u{202c} \u{2014} done\u{7f}\n",
);

/// Nothing to remove, but something to look at every few bytes: `key=value` logging under names
/// that are not credentials, clock times, a URL with no password, and a table drawn with
/// characters that share a lead byte with ones the redactor strips.
const SEPARATOR_LINES: &str = concat!(
    "ts=2026-10-03T10:57:33Z level=info msg=done status=200 dur_ms=87 path=/v1/messages\n",
    "┌──────────────┬────────┐\n│ tests passed │ 42/42  │\n└──────────────┴────────┘\n",
    "see https://example.com/docs/errors#E0423 or file:///usr/lib/agent/README.md:12:1\n",
    "a=1 b=2 c=3 d:4 e:5 f::6 g=h=i j.k-l://m n_o_p=q\n",
);

/// Plain diagnostic lines that end in a non-ASCII character: the redactor reads a whole line of
/// ASCII before it meets the one byte that makes it look again.
const LATE_NON_ASCII_LINES: &str = concat!(
    "error: the vendor exited with status 1 while loading the workspace configuration at \u{2026}\n",
    "warning: retrying request 3 of 5 after 250 ms; the upstream said it was busy again \u{e9}\n",
    "  at Object.spawn (node:internal/child_process:420:11) in the module loader \u{2014}\n",
    "info: loaded 12 tools from 3 servers in 87 ms; cache hit rate 0.93, 4 stale entries \u{2713}\n",
);

/// Structured log lines: a quote stands before nearly every `:` and nothing here is a credential,
/// so a rule that reacts to quoted names has to look at each one and leave it.
const JSON_LINES: &str = concat!(
    r#"{"level":"info","msg":"done","status":200,"dur_ms":87,"path":"/v1/messages","ok":true}"#,
    "\n",
    r#"{"ts":"2026-10-11T10:57:33Z","level":"warn","msg":"retrying","attempt":3,"of":5,"after_ms":250}"#,
    "\n",
    r#"{"error":{"type":"overloaded_error","message":"Overloaded","request_id":"req_011CS"}}"#,
    "\n",
    r#"{"model":"claude-sonnet-5","usage":{"input":1200,"output":87},"stop":"end_turn","id":"msg_01"}"#,
    "\n",
);

/// The same shape with a credential in quoted form among the lines. Redacted only by a redactor
/// that reads a quoted name, so it checks the cost of that work and not its result.
const JSON_SECRET_LINES: &str = concat!(
    r#"{"level":"info","msg":"done","status":200,"dur_ms":87,"path":"/v1/messages","ok":true}"#,
    "\n",
    r#"{"headers":{"Authorization":"Bearer sk-live-0123456789abcdef","Accept":"text/plain"}}"#,
    "\n",
    r#"{"env":{"OPENAI_API_KEY":"sk-proj-0123456789","HOME":"/home/a"},"attempt":3}"#,
    "\n",
    r#"{"error":{"type":"overloaded_error","message":"Overloaded","request_id":"req_011CS"}}"#,
    "\n",
);

/// Quoted credentials inside JSON strings, two and three levels deep, as a vendor that serialises
/// its request into a field writes them.
const ESCAPED_JSON_LINES: &str = concat!(
    r#"{"error":"call failed {\"api_key\":\"sk-proj-0123456789\",\"retry\":3}"}"#,
    "\n",
    r#"{"msg":"{\"data\":\"{\\\"password\\\":\\\"hunter2\\\",\\\"n\\\":1}\"}"}"#,
    "\n",
    r#"{"error":"request {\"model\":\"m\",\"attempt\":3} was refused"}"#,
    "\n",
);

/// A pretty-printed object with credentials in containers that span lines, and a YAML sequence.
const PRETTY_LINES: &str = concat!(
    "{\n",
    "  \"model\": \"m\",\n",
    "  \"tokens\": [\n",
    "    \"sk-live-0123456789\",\n",
    "    \"sk-live-9876543210\"\n",
    "  ],\n",
    "  \"retry_after\": 30\n",
    "}\n",
    "api_key:\n",
    "  - sk-live-0123456789\n",
    "  - sk-live-9876543210\n",
);

/// Lines that carry well-known tokens with no name beside them, and the near misses.
const BARE_TOKEN_LINES: &str = concat!(
    "token: expired (gh",
    "p_0123456789abcdefghijklmnopqrstuvwxyz)\n",
    "request failed for key AK",
    "IA0123456789ABCDEF\n",
    "see sk-learn and task-ant-lines and npm_config_cache for details\n",
);

/// Lines dense in headers: nearly every line is a name the rules have to look at and redact.
const HEADER_LINES: &str = concat!(
    "  Authorization: Bearer abcdef0123456789\n",
    "  Proxy-Authorization: Basic dXNlcjpodW50ZXIy\n",
    "  Cookie: session=abc123; theme=dark\n",
    "  X-Api-Key: 0123456789abcdef\n",
    "  Set-Cookie: sid=hunter2; Path=/; HttpOnly\n",
    "  Content-Type: application/json\n",
);

/// A keyword repeated in a credential name that never gets a value: every word start in the name
/// reaches the same separator, and the redactor has to learn once that nothing follows it.
const NAME_UNIT: &str = "token_";

/// Dotted letters before a `://` with no password after it: every letter reaches the same scheme.
const SCHEME_UNIT: &str = "a.";

/// `unit` repeated to about `len` bytes and closed by `tail`.
fn repeated(unit: &str, tail: &str, len: usize) -> String {
    format!("{}{tail}", unit.repeat(len / unit.len()))
}

/// `lines` repeated and cut to exactly `len` bytes, at a character boundary.
fn tail(lines: &str, len: usize) -> String {
    let mut text = lines.repeat(len / lines.len() + 1);
    let mut end = len;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

/// Fails the run when a case did not do the work its name claims.
fn assert_redacted(label: &str, raw: &str, redacted: &str) {
    match label {
        "plain" | "separators" | "names" | "schemes" | "late-non-ascii" | "json"
        | "quoted-names" | "quotes" => assert!(
            redacted == raw,
            "expected a {label} tail of {} bytes returned unchanged | received {} bytes",
            raw.len(),
            redacted.len()
        ),
        "secrets" => assert!(
            redacted.contains("[REDACTED]")
                && !redacted.contains("hunter2")
                && !redacted.contains("0123"),
            "expected every credential in a {} byte tail redacted | received {redacted:?}",
            raw.len()
        ),
        "bounded-secrets" | "bounded-escaped-json" | "bounded-pretty" | "bounded-tokens" => {
            assert!(
                redacted.contains("[REDACTED]")
                    && !redacted.contains("hunter2")
                    && !redacted.contains("0123456789"),
                "expected every credential in a {} byte {label} tail redacted | received {redacted:?}",
                raw.len()
            )
        }
        "bounded-plain"
        | "sequence"
        | "containers"
        | "slashes"
        | "prefixes"
        | "pem"
        | "web"
        | "one-run"
        | "escaped-json"
        | "pretty"
        | "bounded-names"
        | "bounded-schemes"
        | "bounded-quoted-names"
        | "bounded-quotes"
        | "bounded-slashes"
        | "bounded-containers"
        | "bounded-sequence" => assert!(
            !redacted.is_empty(),
            "expected a {} byte {label} tail to come back",
            raw.len()
        ),
        "json-secrets" => assert!(
            !redacted.is_empty(),
            "expected a {} byte tail of quoted credentials to come back",
            raw.len()
        ),
        _ => assert!(
            !redacted.contains(|character: char| character.is_control() && character != '\n')
                && redacted.len() < raw.len(),
            "expected every control character taken out of a {} byte tail | received {redacted:?}",
            raw.len()
        ),
    }
}

fn main() {
    let bench = Bench::new("redact");
    let per_byte = Unit::new(BYTES_PER_SAMPLE as u64, "byte");

    for (size_label, len) in [("1KiB", KIB), ("16KiB", 16 * KIB)] {
        for (label, lines) in [
            ("plain", PLAIN_LINES),
            ("secrets", SECRET_LINES),
            ("controls", CONTROL_LINES),
            ("separators", SEPARATOR_LINES),
            ("late-non-ascii", LATE_NON_ASCII_LINES),
            ("json", JSON_LINES),
            ("json-secrets", JSON_SECRET_LINES),
            ("escaped-json", ESCAPED_JSON_LINES),
            ("pretty", PRETTY_LINES),
        ] {
            let raw = tail(lines, len);
            assert_redacted(label, &raw, &stderr_text(&raw));
            let calls = BYTES_PER_SAMPLE / len;
            bench.run(
                &format!("redact/stderr_text-{size_label}/{label}"),
                per_byte,
                || (),
                |()| {
                    (0..calls)
                        .map(|_| stderr_text(std::hint::black_box(&raw)).len())
                        .sum::<usize>()
                },
            );
        }
    }
    pathological(&bench);
    messages(&bench);
    token_shapes(&bench);
    tails(&bench);
}

/// `redact::stderr_tail` over 16 KiB tails, 320 KiB per sample, and `stderr_text` over the same
/// text for the cases `main` has no row for: the rules of the release before and the token pass.
fn tails(bench: &Bench) {
    let len = 16 * KIB;
    let per_byte = Unit::new(BYTES_PER_SAMPLE as u64, "byte");
    let calls = BYTES_PER_SAMPLE / len;
    for (label, lines, text_too) in [
        ("plain", PLAIN_LINES, false),
        ("secrets", SECRET_LINES, false),
        ("tokens", BARE_TOKEN_LINES, true),
        ("headers", HEADER_LINES, true),
    ] {
        let raw = tail(lines, len);
        let first = stderr_tail(&raw);
        assert_tail(label, &raw, &first);
        bench.run(
            &format!("redact/stderr_tail-16KiB/{label}"),
            per_byte,
            || (),
            |()| {
                (0..calls)
                    .map(|_| stderr_tail(std::hint::black_box(&raw)).len())
                    .sum::<usize>()
            },
        );
        if text_too {
            bench.run(
                &format!("redact/stderr_text-16KiB/{label}"),
                per_byte,
                || (),
                |()| {
                    (0..calls)
                        .map(|_| stderr_text(std::hint::black_box(&raw)).len())
                        .sum::<usize>()
                },
            );
        }
    }
    let len = KIB * KIB;
    let key_line = concat!(
        "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj",
        "\n"
    );
    for (label, raw) in [
        ("prefixes", repeated(concat!("gh", "p_abc "), "", len)),
        (
            "pem",
            repeated(concat!("-----BEGIN ", "PRIVATE KEY-----\n"), "", len),
        ),
        (
            "web",
            repeated(concat!("ey", "Jhbgcioijiuzi1nir. "), "", len),
        ),
        (
            "one-run",
            format!("{}{}", concat!("sk", "-"), "a".repeat(len)),
        ),
        // A cut key that never reaches its end marker: lines of base64 from the first byte.
        ("cut-key", repeated(key_line, "", len)),
        ("names", repeated(NAME_UNIT, "=", len)),
        ("quoted-names", repeated("\"token_", "\":", len)),
    ] {
        let name = format!("redact/stderr_tail-1MiB/{label}");
        if !bench.selected(&name) {
            continue;
        }
        assert_tail(label, &raw, &stderr_tail(&raw));
        bench.run(
            &name,
            Unit::new(raw.len() as u64, "byte"),
            || (),
            |()| stderr_tail(std::hint::black_box(&raw)).len(),
        );
    }
}

/// Fails the run when a tail case did not do the work its name claims.
fn assert_tail(label: &str, raw: &str, shown: &str) {
    match label {
        "plain" | "cut-key" | "names" | "quoted-names" => assert!(
            shown == raw,
            "expected a {label} tail of {} bytes returned unchanged | received {} bytes",
            raw.len(),
            shown.len()
        ),
        "secrets" | "headers" => assert!(
            shown.contains("[REDACTED]")
                && !shown.contains("hunter2")
                && !shown.contains("0123456789abcdef"),
            "expected every credential in a {} byte {label} tail redacted | received {shown:?}",
            raw.len()
        ),
        "tokens" => assert!(
            shown.contains("[REDACTED]") && !shown.contains("0123456789abcdefghijklmnopqrstuvwxyz"),
            "expected every token in a {} byte tail redacted | received {shown:?}",
            raw.len()
        ),
        _ => assert!(
            !shown.is_empty(),
            "expected a {} byte {label} tail to come back",
            raw.len()
        ),
    }
}

/// `redact::bounded`, which also reads the unnamed tokens and the reads added since the release
/// before: 16 KiB tails of plain lines and of lines that carry tokens, credentials, JSON in strings
/// and containers, 320 KiB per sample, to be read beside `stderr_text` over the same lines.
fn messages(bench: &Bench) {
    let len = 16 * KIB;
    let per_byte = Unit::new(BYTES_PER_SAMPLE as u64, "byte");
    for (label, lines) in [
        ("bounded-plain", PLAIN_LINES),
        ("bounded-tokens", BARE_TOKEN_LINES),
        ("bounded-secrets", SECRET_LINES),
        ("bounded-escaped-json", ESCAPED_JSON_LINES),
        ("bounded-pretty", PRETTY_LINES),
    ] {
        let raw = tail(lines, len);
        let first = bounded(&raw, TextLimit::ErrorMessage);
        assert_redacted(label, &raw, &first.text);
        let calls = BYTES_PER_SAMPLE / len;
        bench.run(
            &format!("redact/bounded-16KiB/{label}"),
            per_byte,
            || (),
            |()| {
                (0..calls)
                    .map(|_| {
                        bounded(std::hint::black_box(&raw), TextLimit::ErrorMessage)
                            .text
                            .len()
                    })
                    .sum::<usize>()
            },
        );
    }
}

/// The same call over text that is built to cost more than its length when a rule reads the same
/// bytes for each candidate. One call per sample: at 1 MiB a quadratic redactor does not finish.
fn pathological(bench: &Bench) {
    for (size_label, len) in [("16KiB", 16 * KIB), ("120KB", 120_000), ("1MiB", KIB * KIB)] {
        for (label, raw) in [
            ("names", repeated(NAME_UNIT, "=", len)),
            ("schemes", repeated(SCHEME_UNIT, "://", len)),
            // Quotes around names that never get a value: a rule that reads a quoted name must
            // not read the rest of the run again for each quote.
            ("quoted-names", repeated("\"token_", "\":", len)),
            ("quotes", repeated("\"", ":", len)),
            // Escaped quotes three levels deep before names that never get a value.
            ("slashes", repeated("{\\\\\\\"token_", "\\\\\\\":", len)),
            // An opener at the end of every line that never closes, and a long sequence.
            ("containers", repeated("token: [\n", "", len)),
            (
                "sequence",
                format!("tokens:\n{}", "  - a\n".repeat(len / 6)),
            ),
        ] {
            let name = format!("redact/stderr_text-{size_label}/{label}");
            // Checked only when the case runs: a filtered-out case must not pay for the call.
            if bench.selected(&name) {
                assert_redacted(label, &raw, &stderr_text(&raw));
                bench.run(
                    &name,
                    Unit::new(raw.len() as u64, "byte"),
                    || (),
                    |()| stderr_text(std::hint::black_box(&raw)).len(),
                );
            }
            // The same text through the entry point that reads the rest.
            let name = format!("redact/bounded-{size_label}/{label}");
            if bench.selected(&name) {
                let label = format!("bounded-{label}");
                assert_redacted(&label, &raw, &bounded(&raw, TextLimit::ErrorMessage).text);
                bench.run(
                    &name,
                    Unit::new(raw.len() as u64, "byte"),
                    || (),
                    |()| {
                        bounded(std::hint::black_box(&raw), TextLimit::ErrorMessage)
                            .text
                            .len()
                    },
                );
            }
        }
    }
}

/// `redact::bounded` over text built to make the token pass read a run again: a prefix too short to
/// be a token before every run, a private-key header with no end, web-token headers with no
/// claims, and one run as long as the text that opens like a token. One call per sample, 1 MiB.
fn token_shapes(bench: &Bench) {
    let len = KIB * KIB;
    for (label, raw) in [
        ("prefixes", repeated(concat!("gh", "p_abc "), "", len)),
        (
            "pem",
            repeated(concat!("-----BEGIN ", "PRIVATE KEY-----\n"), "", len),
        ),
        (
            "web",
            repeated(concat!("ey", "Jhbgcioijiuzi1nir. "), "", len),
        ),
        (
            "one-run",
            format!("{}{}", concat!("sk", "-"), "a".repeat(len)),
        ),
    ] {
        let name = format!("redact/bounded-1MiB/{label}");
        if !bench.selected(&name) {
            continue;
        }
        assert_redacted(label, &raw, &bounded(&raw, TextLimit::ErrorMessage).text);
        bench.run(
            &name,
            Unit::new(raw.len() as u64, "byte"),
            || (),
            |()| {
                bounded(std::hint::black_box(&raw), TextLimit::ErrorMessage)
                    .text
                    .len()
            },
        );
    }
}
