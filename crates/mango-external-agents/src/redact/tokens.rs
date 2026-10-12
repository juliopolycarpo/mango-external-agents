//! Well-known credentials with no name beside them.
//!
//! The assignment rules find a credential by its name. A token printed alone, `token: expired
//! (ghp_...)` or a key quoted in prose, has no name to find, but most vendors give theirs away with
//! a prefix, a length and an alphabet. This pass reads those, and nothing else: a closed list, each
//! entry case-sensitive, with a minimum length and a body made of the characters that vendor's
//! tokens are made of, so that `sk-learn`, `task-ant-...` and `npm_config_cache` are words. A short
//! body of a prefix that allows `-` or `_` also has a digit, so a hyphenated name that opens like a
//! prefix (`sk-live-reload-development-server`) is a word too.
//!
//! A token has to start where a run of name characters (`A-Za-z0-9_-`) starts, so a prefix inside a
//! longer word never matches, and it takes the whole run it is in. The two letters of a JSON line
//! break (`\n`) are the exception, since a string is full of them and the letter would otherwise
//! be part of the run in front of the token. A match is replaced by `[REDACTED]`, prefix included.
//!
//! Every run is read once by the scan and at most once more by the match that starts at it, and a
//! match ends at the end of a run or past it by a bounded number of runs (a web token is three),
//! so the work is linear. A private-key block with no end marker takes the rest of the text, which
//! is read once. See `redact::work` for the shapes that hold this.
//!
//! The pass costs a byte-wise read of the whole text, so `stderr_text`, the hot path of a stderr
//! tail, does not run it; `redact::bounded` and `redact::structured`, which carry a vendor's
//! messages, do (see `docs/benchmarks.md`).

use super::scan::count_steps;
use super::{REDACTED, take_while};

/// The characters a token is made of, at most: letters, digits, `_` and `-`.
fn is_run_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() | (byte == b'_') | (byte == b'-')
}

/// The letters that follow a backslash as a JSON line break, tab, backspace or form feed.
fn is_escape_letter(byte: u8) -> bool {
    matches!(byte, b'n' | b'r' | b't' | b'b' | b'f')
}

/// Which tokens a pass reads: the private-key blocks, which have to go before the name rules (a
/// name that redacts the block's first line would leave the rest of it), or every other prefix,
/// which has to go after them (a name that opens like a prefix is a name).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Pass {
    Blocks,
    Prefixes,
}

/// `text` with every well-known token of `pass` replaced, or `None` when it holds none.
///
/// # Example
///
/// ```ignore
/// let found = redact("see ghp_0123456789abcdefghijklmnopqrstuv now", Pass::Prefixes);
/// assert_eq!(found.as_deref(), Some("see [REDACTED] now"));
/// ```
pub(super) fn redact(text: &str, pass: Pass) -> Option<String> {
    match pass {
        Pass::Blocks => redact_blocks(text),
        Pass::Prefixes => redact_prefixes(text),
    }
}

/// What stands for a redacted private-key block while the name rules read the text.
///
/// It is read the way the rules read the marker it replaces, `-----BEGIN PRIVATE KEY-----`: a word
/// that a value in front of it may take, then a space, then a name with its dashes glued on
/// (`PRIVATE KEY-----`). A name glued to the end of it (`-----END PRIVATE KEY-----abc: x`), a
/// separator right after it or one on the next line is an assignment to the rules, and the block
/// must not change that by being gone. The marker itself (`[REDACTED]`) would read as a closed
/// container to a rule that is reading a value (`password: [REDACTED] more`).
const BLOCK_MARK: &str = "\u{e000} \u{e001}PRIVATE_KEY-----";

/// What is left of [`BLOCK_MARK`] when a value in front of it took its first word. It is a
/// redaction of its own and not nothing: what was glued to the key stays a word apart from the
/// value, as the space in the key's header kept it.
const BLOCK_TAIL: &str = " \u{e001}PRIVATE_KEY-----";

/// What the redacted text is left with once the rules are done: a mark that is whole is a
/// `[REDACTED]`, and what a value in front of one left of it is a `[REDACTED]` of its own.
pub(super) fn settle_marks(text: &str) -> String {
    text.replace(BLOCK_MARK, REDACTED)
        .replace(BLOCK_TAIL, concat!(" ", "[REDACTED]"))
}

/// The private-key blocks of `text`, found by a search for the begin marker and not a read of
/// every run, each replaced by [`BLOCK_MARK`].
fn redact_blocks(text: &str) -> Option<String> {
    const BEGIN: &str = "-----BEGIN ";
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut from = 0;
    let mut ends = Ends::default();
    while let Some(offset) = text[from..].find(BEGIN) {
        count_steps(offset + 1);
        let start = from + offset;
        from = start + 1;
        // A block that starts where the one before it ended is another block, not a word.
        let adjoins = start == copied && copied > 0;
        if start < copied || !(adjoins || starts_a_run(bytes, start)) {
            continue;
        }
        let Some(end) = private_key_block(text, start, &mut ends) else {
            continue;
        };
        let out = out.get_or_insert_with(|| String::with_capacity(text.len()));
        out.push_str(&text[copied..start]);
        out.push_str(BLOCK_MARK);
        copied = end;
        from = end;
    }
    out.map(|mut out| {
        out.push_str(&text[copied..]);
        out
    })
}

/// Whether a token may start at `at`: no name character in front, or the letter of a JSON line
/// break that follows a backslash.
fn starts_a_run(bytes: &[u8], at: usize) -> bool {
    let Some(&before) = at.checked_sub(1).and_then(|place| bytes.get(place)) else {
        return true;
    };
    !is_run_byte(before) || (is_escape_letter(before) && at >= 2 && bytes[at - 2] == b'\\')
}

fn redact_prefixes(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        count_steps(1);
        if !is_run_byte(bytes[at]) {
            at += 1;
            continue;
        }
        let run_end = take_while(bytes, at, is_run_byte);
        let escaped = at > 0 && bytes[at - 1] == b'\\' && is_escape_letter(bytes[at]);
        // A token glued to a mark starts a word as it would after the header the mark stands for.
        let first = at + glued_to_a_mark(bytes, at, run_end);
        let found = token_at(text, first, run_end)
            .map(|end| (first, end))
            .or_else(|| {
                escaped
                    .then(|| token_at(text, at + 1, run_end).map(|end| (at + 1, end)))
                    .flatten()
            });
        let Some((start, end)) = found else {
            at = run_end;
            continue;
        };
        let out = out.get_or_insert_with(|| String::with_capacity(text.len()));
        out.push_str(&text[copied..start]);
        out.push_str(REDACTED);
        copied = end;
        at = end;
    }
    out.map(|mut out| {
        out.push_str(&text[copied..]);
        out
    })
}

/// How many bytes of the run at `at` are the name a mark ends in (`PRIVATE_KEY-----`), when it is
/// one: a token behind it is the start of a word, since the header it stands for ended a word.
fn glued_to_a_mark(bytes: &[u8], at: usize, run_end: usize) -> usize {
    const NAME: &[u8] = b"PRIVATE_KEY-----";
    let after_the_mark = at >= 3 && bytes[..at].ends_with("\u{e001}".as_bytes());
    if after_the_mark && bytes[at..run_end].starts_with(NAME) {
        NAME.len()
    } else {
        0
    }
}

/// Where the token that starts at `start` ends, when one does. The run of name characters that
/// holds `start` ends at `run_end`.
fn token_at(text: &str, start: usize, run_end: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let run = bytes.get(start..run_end)?;
    match *run.first()? {
        b'g' => github(run).then_some(run_end),
        b'x' => slack(run).then_some(run_end),
        b's' => s_token(run).then_some(run_end),
        b'r' | b'p' => stripe(run).then_some(run_end),
        b'A' => aws_or_google(run).then_some(run_end),
        b'y' => google_oauth(bytes, run, run_end),
        b'e' => web_token(bytes, run, run_end),
        b'n' => with_body(run, b"npm_", 30, is_alphanumeric).then_some(run_end),
        b'h' => with_body(run, b"hf_", 30, is_alphanumeric).then_some(run_end),
        b'd' => digitalocean(run).then_some(run_end),
        _ => None,
    }
}

fn is_alphanumeric(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
}

fn is_lower_hex(byte: u8) -> bool {
    byte.is_ascii_digit() | matches!(byte, b'a'..=b'f')
}

/// Whether the body of a token is random enough to be one: a body under 40 characters has a digit.
/// Real tokens of these prefixes are random over 62 or 64 characters, so a short body with no
/// digit in it is a hyphenated name (`sk-live-reload-development-server`) one time in a thousand
/// tokens lost against many identifiers kept.
fn looks_random(body: &[u8]) -> bool {
    body.len() >= 40 || body.iter().any(u8::is_ascii_digit)
}

/// Whether `run` is `prefix` and then at least `min` bytes that `body` accepts, and nothing else.
fn with_body(run: &[u8], prefix: &[u8], min: usize, body: fn(u8) -> bool) -> bool {
    run.strip_prefix(prefix)
        .is_some_and(|rest| rest.len() >= min && rest.iter().all(|byte| body(*byte)))
}

/// `ghp_`, `gho_`, `ghu_`, `ghs_` and `ghr_` and 20 or more letters and digits (36 as GitHub issues
/// them), the fine-grained `github_pat_` and 22 or more, and GitLab's `glpat-` and 20 or more.
fn github(run: &[u8]) -> bool {
    if let Some(rest) = run.strip_prefix(b"gh")
        && let [kind, b'_', body @ ..] = rest
    {
        return matches!(kind, b'p' | b'o' | b'u' | b's' | b'r')
            && body.len() >= 20
            && body.iter().all(|byte| byte.is_ascii_alphanumeric());
    }
    if let Some(body) = run.strip_prefix(b"github_pat_") {
        return body.len() >= 22 && looks_random(body);
    }
    run.strip_prefix(b"glpat-")
        .is_some_and(|body| body.len() >= 20 && looks_random(body))
}

/// Slack's `xoxb-`, `xoxp-`, `xoxa-`, `xoxs-` and `xoxr-` and 20 or more letters, digits and
/// hyphens.
fn slack(run: &[u8]) -> bool {
    if let Some(rest) = run.strip_prefix(b"xox")
        && let [kind, b'-', body @ ..] = rest
    {
        return matches!(kind, b'b' | b'p' | b'a' | b's' | b'r')
            && body.len() >= 20
            && !body.contains(&b'_')
            && looks_random(body);
    }
    false
}

/// `sk-ant-`, `sk-proj-` and `sk-live-` and 16 or more, Stripe's `sk_live_` and `sk_test_` and 16
/// or more letters and digits, Shopify's `shpat_` family and 32 hex digits, and OpenAI's older
/// `sk-` and 40 or more letters and digits with a digit and a letter among them.
fn s_token(run: &[u8]) -> bool {
    for prefix in [&b"sk-ant-"[..], b"sk-proj-", b"sk-live-"] {
        if let Some(body) = run.strip_prefix(prefix) {
            return body.len() >= 16 && looks_random(body);
        }
    }
    if stripe(run) {
        return true;
    }
    for prefix in [&b"shpat_"[..], b"shpss_", b"shpca_", b"shppa_"] {
        if with_body(run, prefix, 32, is_lower_hex) {
            return true;
        }
    }
    run.strip_prefix(b"sk-").is_some_and(|body| {
        body.len() >= 40
            && body.iter().all(|byte| byte.is_ascii_alphanumeric())
            && body.iter().any(u8::is_ascii_digit)
            && body.iter().any(u8::is_ascii_alphabetic)
    })
}

/// `sk_`, `rk_` and `pk_` with `live_` or `test_` and 16 or more letters and digits.
fn stripe(run: &[u8]) -> bool {
    let Some(rest) = run
        .strip_prefix(b"sk_")
        .or_else(|| run.strip_prefix(b"rk_"))
        .or_else(|| run.strip_prefix(b"pk_"))
    else {
        return false;
    };
    for mode in [&b"live_"[..], b"test_"] {
        if with_body(rest, mode, 16, is_alphanumeric) {
            return true;
        }
    }
    false
}

/// An AWS access key id, `AKIA` or `ASIA` and 16 capitals or digits and no more, and a Google API
/// key, `AIza` and 35 or more letters, digits, `_` and `-`.
fn aws_or_google(run: &[u8]) -> bool {
    if run.len() == 20
        && (run.starts_with(b"AKIA") || run.starts_with(b"ASIA"))
        && run[4..]
            .iter()
            .all(|byte| byte.is_ascii_uppercase() | byte.is_ascii_digit())
    {
        return true;
    }
    run.strip_prefix(b"AIza")
        .is_some_and(|body| body.len() >= 35 && looks_random(body))
}

/// `ya29.` and 20 or more letters, digits, `_` and `-`: the run is `ya29`, a dot ends it, and the
/// body is the run after the dot.
fn google_oauth(bytes: &[u8], run: &[u8], run_end: usize) -> Option<usize> {
    if run != b"ya29" || bytes.get(run_end) != Some(&b'.') {
        return None;
    }
    let end = take_while(bytes, run_end + 1, is_run_byte);
    (end - (run_end + 1) >= 20).then_some(end)
}

/// DigitalOcean's `dop_v1_`, `doo_v1_` and `dor_v1_` and 40 or more hex digits.
fn digitalocean(run: &[u8]) -> bool {
    [&b"dop_v1_"[..], b"doo_v1_", b"dor_v1_"]
        .iter()
        .any(|prefix| with_body(run, prefix, 40, is_lower_hex))
}

/// A signed web token: a header and a claims part, each base64url JSON (`eyJ`, which is `{"`) of
/// 10 or more characters, then a signature that may be empty. The run is the header; the others
/// follow a dot each.
fn web_token(bytes: &[u8], run: &[u8], run_end: usize) -> Option<usize> {
    if !run.starts_with(b"eyJ") || run.len() < 10 || bytes.get(run_end) != Some(&b'.') {
        return None;
    }
    let claims = run_end + 1;
    let claims_end = take_while(bytes, claims, is_run_byte);
    if !bytes[claims..claims_end].starts_with(b"eyJ")
        || claims_end - claims < 10
        || bytes.get(claims_end) != Some(&b'.')
    {
        return None;
    }
    Some(take_while(bytes, claims_end + 1, is_run_byte))
}

/// The longest label inside `-----BEGIN ...-----`: `ENCRYPTED PRIVATE KEY` is 21.
const MAX_LABEL: usize = 40;

/// The farthest an end marker can be from its begin marker and still close the block: the longest
/// private key in use, a 16384-bit RSA key, is 12 KiB of PEM, and a 4096-bit one is 3.3 KiB.
const MAX_BLOCK_BYTES: usize = 16 * 1024;

/// The most lines of base64 under a header that has no end marker in reach.
const MAX_BODY_LINES: usize = 128;

/// The shortest line of base64 that is a line of a key: PEM lines are 64 characters, 76 in older
/// writers; a shorter one is only the last line of a body that has longer ones above it.
const MIN_BODY_LINE: usize = 16;

/// Where the next end marker is, kept between blocks so that no text is searched twice: a search
/// that finds nothing means there is none past that place.
#[derive(Default)]
struct Ends {
    found: Option<usize>,
    none_from: Option<usize>,
}

impl Ends {
    /// The first `-----END ` at or after `from`.
    fn after(&mut self, text: &str, from: usize) -> Option<usize> {
        if self.none_from.is_some_and(|none| from >= none) {
            return None;
        }
        if let Some(at) = self.found.filter(|at| *at >= from) {
            return Some(at);
        }
        let rest = &text[from..];
        let Some(offset) = rest.find("-----END ") else {
            count_steps(rest.len());
            (self.found, self.none_from) = (None, Some(from));
            return None;
        };
        count_steps(offset);
        self.found = Some(from + offset);
        self.found
    }
}

/// A private-key block: `-----BEGIN ... PRIVATE KEY-----` to the line that ends it. A certificate
/// and a public key are not secrets and are left.
///
/// Only a private key's own end marker closes it: `-----END PASSWORD-----` is a label that names a
/// credential, and the rules have to read it. Nor does a marker close a block whose inside leaves a
/// name waiting for its value: that value would run on past the marker, and the rules, which run
/// after this pass, would never see the name. A header with no end marker within
/// [`MAX_BLOCK_BYTES`] is a key that was cut, or a sentence that mentions one: the marker goes, and
/// the lines of base64 directly under it, and nothing else.
fn private_key_block(text: &str, start: usize, ends: &mut Ends) -> Option<usize> {
    let bytes = text.as_bytes();
    let label_start = start + b"-----BEGIN ".len();
    if !bytes[start..].starts_with(b"-----BEGIN ") {
        return None;
    }
    let after_begin = closing_dashes(bytes, label_start, true)?;
    if !is_private_key_label(&bytes[label_start..after_begin - 5]) {
        return None;
    }
    // A name can be made of the header and what is glued to it (`PRIVATE KEY-----spo=`), and then
    // the header belongs to the text a value is waited for in.
    let from = if header_joins_the_text(bytes, after_begin) {
        start
    } else {
        after_begin
    };
    let closing = ends
        .after(text, after_begin)
        .filter(|at| at - after_begin <= MAX_BLOCK_BYTES)
        .filter(|at| !waits_on_a_value(&text[from..*at]))
        .and_then(|at| {
            let label = at + b"-----END ".len();
            let close = closing_dashes(bytes, label, true)?;
            is_private_key_label(&bytes[label..close - 5]).then_some(close)
        });
    Some(closing.unwrap_or_else(|| body_lines(text, after_begin, from)))
}

/// Whether the text after a header makes it part of a name: something is glued to the dashes, or
/// a `:` or an `=` follows them, after blanks and breaks as the rules skip them.
fn header_joins_the_text(bytes: &[u8], after: usize) -> bool {
    let mut at = after;
    while let Some(&byte) = bytes.get(at) {
        count_steps(1);
        if let Some(next) = line_break(bytes, at) {
            at = next;
        } else if byte.is_ascii_whitespace() {
            at += 1;
        } else {
            return at == after || matches!(byte, b':' | b'=');
        }
    }
    false
}

/// Whether a label between `BEGIN ` or `END ` and the dashes is a private key's.
fn is_private_key_label(label: &[u8]) -> bool {
    label.ends_with(b"PRIVATE KEY") || label.ends_with(b"PRIVATE KEY BLOCK")
}

/// Where the lines of base64 that start at `from`, right under a header, end. A line break is a
/// real one or the two characters a JSON string writes it as. Lines of [`MIN_BODY_LINE`]
/// characters or more are the body, up to [`MAX_BODY_LINES`]; one shorter line after them is the
/// last. The first line may follow the header without a break.
///
/// A line counts only when it ends where a line ends: at a break, at the end of the text or at the
/// quote of a JSON string. A run that stops at a `:` or a space is a word, or the name of a
/// credential glued to base64, and the rules need to read it. For the same reason a line that a
/// name is waiting on (`pwd=` and a value on the next line) is left: this pass runs before the
/// rules, so whatever it takes they never see.
fn body_lines(text: &str, from: usize, header: usize) -> usize {
    let bytes = text.as_bytes();
    let (mut end, mut long) = (from, 0);
    for line in 0..MAX_BODY_LINES {
        let start = match line_break(bytes, end) {
            Some(after) => after,
            None if line == 0 => end,
            None => break,
        };
        let run_end = take_while(bytes, start, is_base64);
        count_steps(run_end - start + 1);
        let length = run_end - start;
        let accepted = length >= MIN_BODY_LINE || (long > 0 && length >= 4);
        if !accepted
            || !is_padded_base64(&bytes[start..run_end])
            || !ends_a_line(bytes, run_end)
            || waits_on_a_value(&text[if line == 0 { header } else { start }..run_end])
        {
            break;
        }
        end = run_end;
        if length < MIN_BODY_LINE {
            break;
        }
        long += 1;
    }
    end
}

/// Whether a line of base64 that stops at `at` ends where a line ends: at a break, at the end of
/// the text, or at the quote of a JSON string, when no `:` or `=` follows the quote (that would
/// make the run the name of a quoted pair).
fn ends_a_line(bytes: &[u8], at: usize) -> bool {
    if at == bytes.len() || line_break(bytes, at).is_some() {
        return true;
    }
    let rest = &bytes[at..];
    let after = match rest {
        [b'"' | b'\'', tail @ ..] => tail,
        [b'\\', b'"' | b'\'', tail @ ..] => tail,
        _ => return false,
    };
    let next = after
        .iter()
        .find(|byte| !matches!(byte, b' ' | b'\t' | b'\r' | b'\n'));
    !matches!(next, Some(b':' | b'='))
}

/// Whether `run` is base64 with `=` only as padding: one or two at its end. An `=` in the middle
/// makes it an assignment (`ApiKey=value`), which the rules read.
fn is_padded_base64(run: &[u8]) -> bool {
    let padding = run.iter().rev().take_while(|byte| **byte == b'=').count();
    padding <= 2 && !run[..run.len() - padding].contains(&b'=')
}

/// Whether a credential's name stands at the end of `line` with its value still to come.
fn waits_on_a_value(line: &str) -> bool {
    // The text is stripped already, and most lines of a key's body name nothing.
    super::names_a_credential(line) && super::ends_awaiting_value(line)
}

fn is_base64(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() | matches!(byte, b'+' | b'/' | b'=')
}

/// The place after the line break at `at`: `\n`, `\r\n` or `\r`, or the same written as the
/// escapes `\\n` and `\\r\\n`.
fn line_break(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = bytes.get(at..)?;
    for pattern in [&b"\r\n"[..], b"\n", b"\r", b"\\r\\n", b"\\n"] {
        if rest.starts_with(pattern) {
            return Some(at + pattern.len());
        }
    }
    None
}

/// The place after the `-----` that closes a label starting at `from`, when 1 to [`MAX_LABEL`]
/// capitals and spaces come first (and, for the `BEGIN` label, nothing else).
fn closing_dashes(bytes: &[u8], from: usize, required: bool) -> Option<usize> {
    let window = bytes.get(from..)?;
    let label = window
        .iter()
        .take(MAX_LABEL)
        .take_while(|byte| byte.is_ascii_uppercase() | (**byte == b' '))
        .count();
    count_steps(label + 1);
    let end = from + label;
    let closed = bytes.get(end..end + 5) == Some(b"-----");
    if required && (!closed || label == 0) {
        return None;
    }
    closed.then_some(end + 5)
}
