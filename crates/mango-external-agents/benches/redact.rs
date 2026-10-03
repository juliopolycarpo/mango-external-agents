//! `redact::stderr_text`: what a stderr tail costs to cross a diagnostic boundary.
//!
//! The redactor reads the whole tail several times: once to take control text out, once per
//! credential rule. Its cost follows what the text holds, so each size is run over four kinds of
//! tail: lines with nothing to remove, lines that carry credentials, coloured output with control
//! characters throughout, and lines dense in the punctuation and lead bytes the redactor has to
//! stop at without finding anything. The fixtures are synthetic; no vendor wrote them. Run with:
//!
//! ```sh
//! cargo bench -p mango-external-agents --bench redact
//! ```

mod support;

use mango_external_agents::redact::stderr_text;
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
        "plain" | "separators" => assert!(
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
}
