//! Well-known credentials with no name beside them.
//!
//! The assignment rules find a credential by its name. A token printed alone, `token: expired
//! (ghp_...)` or a key quoted in prose, has no name to find, but most vendors give theirs away with
//! a prefix, a length and an alphabet. This pass reads those, and nothing else: a closed list, each
//! entry case-sensitive, with a minimum length and a body made of the characters that vendor's
//! tokens are made of, so that `sk-learn`, `task-ant-...` and `npm_config_cache` are words. A short
//! body of a prefix that allows `-` or `_` also has a digit and is not made of words, so a
//! hyphenated name that opens like a prefix (`sk-live-reload-development-server`,
//! `sk-live-streaming-server-1`) is a word too. Anthropic's, OpenAI's project and Google's OAuth
//! keys are held to the length and the alphabet their issuers use.
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
//! The pass costs a byte-wise read of the whole text, so `stderr_text`, which a host may run over
//! every line of a stream, does not run it; `redact::bounded` and `redact::structured`, which carry
//! a vendor's messages, do, and so does `redact::stderr_tail`, which reads a tail once, when a
//! failure is reported (see `docs/benchmarks.md`).

use super::scan::count_steps;
use super::take_while;
use super::union::{Hide, union};

/// The characters a token is made of, at most: letters, digits, `_` and `-`.
fn is_run_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() | (byte == b'_') | (byte == b'-')
}

/// The letters that follow a backslash as a JSON line break, tab, backspace or form feed.
fn is_escape_letter(byte: u8) -> bool {
    matches!(byte, b'n' | b'r' | b't' | b'b' | b'f')
}

/// The spans of `text` that hold a well-known token: the private-key blocks, then every other
/// prefix. Both are read over the text as it stands, beside the name rules and not before or
/// after them, so a name that opens like a prefix (`sk-live-api-key: x`) reaches the rules whole
/// and a block does not hide the name glued to its end from them; the union takes both.
///
/// # Example
///
/// ```ignore
/// let found = hides("see ghp_0123456789abcdefghijklmnopqrstuv now");
/// assert_eq!(found, [Hide::marker(4, 40)]);
/// ```
pub(super) fn hides(text: &str) -> Vec<Hide> {
    union(vec![block_hides(text), prefix_hides(text)])
}

/// [`hides`] and the end of a private key whose begin marker is not in `text`: what a stderr tail
/// is read with. A tail is cut on a line (`StderrTail::push`), so a key written over many lines can
/// lose its begin line and keep the lines of base64 under it, which have no marker for the block
/// read to start from.
///
/// # Example
///
/// ```ignore
/// let found = tail_hides("c3ludGhldGljLWtleS1ib2R5\n-----END PRIVATE KEY-----\nnext");
/// assert_eq!(found, [Hide::marker(0, 54)]);
/// ```
pub(super) fn tail_hides(text: &str) -> Vec<Hide> {
    union(vec![orphaned_end_hides(text), hides(text)])
}

/// The lines of a key's body at the start of `text` and the private key's end marker that closes
/// them: the part of a key a cut left, when the cut dropped the begin marker and the lines above it.
///
/// Only the start of the text is read this way. A key with its begin marker is the block read's;
/// a line of base64 anywhere else, with no begin marker, is as likely a hash or an attachment as a
/// key. The first end marker of a private key within [`MAX_BLOCK_BYTES`], the farthest a block is
/// searched for its end, closes it, and every line before it has to be a line a key's body has (see
/// [`is_key_body_line`]), so a sentence, a path or an assignment above it is not a key's body.
///
/// A log that writes each line behind a tag (`stderr | `, a timestamp and an agent's name) writes
/// the key that way, so what stands in front of the marker on its line is the prefix, and every
/// line above it carries the same one: at most [`MAX_PREFIX`] bytes, identical on each line, after
/// the blanks a log may indent them with. A prefix of blanks alone is an indented key.
fn orphaned_end_hides(text: &str) -> Vec<Hide> {
    let bytes = text.as_bytes();
    let Some((line, marker, close)) = first_end_line(text) else {
        return Vec::new();
    };
    let core = bytes[line..marker].trim_ascii();
    if core.len() > MAX_PREFIX {
        return Vec::new();
    }
    let (mut at, mut start) = (0, None);
    while at < line {
        let end = take_while(bytes, at, |byte| !matches!(byte, b'\n' | b'\r' | b'\\'));
        count_steps(end - at + 1);
        let content = bytes[at..end].trim_ascii_start();
        let lead = end - at - content.len();
        let rest = match content.strip_prefix(core) {
            Some(rest) => rest,
            None if content.trim_ascii().is_empty() => &[],
            None => return Vec::new(),
        };
        if !is_key_body_line(rest.trim_ascii_start()) {
            return Vec::new();
        }
        if start.is_none() && !content.trim_ascii().is_empty() {
            // What the marker takes over starts after the prefix of the first line that has one.
            let after = at + lead + core.len();
            start = Some(after + take_while(bytes, after, is_blank) - after);
        }
        match line_break(bytes, end) {
            Some(after) => at = after,
            None => return Vec::new(),
        }
    }
    // A prefix of blanks leaves nothing in front of the span: the whole of the lines is the key's.
    let from = if core.is_empty() {
        0
    } else {
        start.unwrap_or(marker)
    };
    vec![Hide::marker(from, close)]
}

/// The start of the first line within [`MAX_BLOCK_BYTES`] that holds the end marker of a private
/// key, where the marker starts and where it closes.
fn first_end_line(text: &str) -> Option<(usize, usize, usize)> {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at <= MAX_BLOCK_BYTES {
        let end = take_while(bytes, at, |byte| !matches!(byte, b'\n' | b'\r' | b'\\'));
        count_steps(end - at + 1);
        if let Some(offset) = text[at..end].find("-----END ") {
            let marker = at + offset;
            if let Some(close) = end_marker_close(bytes, marker) {
                return Some((at, marker, close));
            }
        }
        at = line_break(bytes, end)?;
    }
    None
}

/// The longest prefix a log line may carry in front of a key's line: a timestamp, a level and an
/// agent's name, with room to spare.
const MAX_PREFIX: usize = 64;

fn is_blank(byte: u8) -> bool {
    (byte == b' ') | (byte == b'\t')
}

/// The headers an encrypted PEM key carries between its begin marker and its base64 (RFC 1421), and
/// the armor headers of a PGP key block (RFC 4880).
const BODY_HEADERS: [&[u8]; 6] = [
    b"Proc-Type:",
    b"DEK-Info:",
    b"Version:",
    b"Comment:",
    b"Charset:",
    b"Hash:",
];

/// Whether `line`, without the blanks around it, is a line the body of a private key has: nothing,
/// a line of padded base64, one of its headers, or the checksum line of a PGP block (`=` and four
/// base64 characters).
fn is_key_body_line(line: &[u8]) -> bool {
    let line = line.trim_ascii_end();
    if line.is_empty() || (line.iter().all(|byte| is_base64(*byte)) && is_padded_base64(line)) {
        return true;
    }
    if BODY_HEADERS.iter().any(|header| line.starts_with(header)) {
        return true;
    }
    matches!(line, [b'=', checksum @ ..]
        if checksum.len() == 4 && checksum.iter().all(|byte| is_base64(*byte) && *byte != b'='))
}

/// Where the end marker of a private key that starts at `start` closes, when it is one.
fn end_marker_close(bytes: &[u8], start: usize) -> Option<usize> {
    let label = start + b"-----END ".len();
    let close = closing_dashes(bytes, label, true)?;
    is_private_key_label(&bytes[label..close - 5]).then_some(close)
}

/// A private key's marker line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KeyMark {
    /// `-----BEGIN ... PRIVATE KEY-----`, where a block starts.
    Begin,
    /// `-----END ... PRIVATE KEY-----`, which closes it.
    End,
}

/// The longest a private key's marker can be: `-----BEGIN `, the longest label and the dashes.
pub(super) const MAX_MARK_BYTES: usize = 11 + MAX_LABEL + 5;

/// The last private-key marker in `bytes`, read as the block read reads one: a begin marker that
/// starts a run, and an end marker of a private key. A certificate's and a public key's are not
/// markers. Linear in `bytes`, with no allocation.
///
/// # Example
///
/// ```ignore
/// let bytes = b"-----BEGIN PRIVATE KEY-----\nabcd\n";
/// assert_eq!(last_key_mark(bytes), Some(KeyMark::Begin));
/// ```
pub(super) fn last_key_mark(bytes: &[u8]) -> Option<KeyMark> {
    let (mut last, mut at) = (None, 0);
    while let Some(offset) = bytes[at..].iter().position(|byte| *byte == b'-') {
        let start = at + offset;
        at = start + 1;
        count_steps(offset + 1);
        let rest = &bytes[start..];
        if rest.starts_with(b"-----BEGIN ") {
            let label = start + b"-----BEGIN ".len();
            let marked = starts_a_run(bytes, start)
                && closing_dashes(bytes, label, true)
                    .is_some_and(|close| is_private_key_label(&bytes[label..close - 5]));
            if marked {
                last = Some(KeyMark::Begin);
            }
        } else if rest.starts_with(b"-----END ") && end_marker_close(bytes, start).is_some() {
            last = Some(KeyMark::End);
        }
    }
    last
}

/// The private-key blocks of `text`, found by a search for the begin marker and not a read of
/// every run.
fn block_hides(text: &str) -> Vec<Hide> {
    const BEGIN: &str = "-----BEGIN ";
    let bytes = text.as_bytes();
    let mut found = Vec::new();
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
        found.push(Hide::marker(start, end));
        copied = end;
        from = end;
    }
    found
}

/// Whether a token may start at `at`: no name character in front, or the letter of a JSON line
/// break that follows a backslash.
fn starts_a_run(bytes: &[u8], at: usize) -> bool {
    let Some(&before) = at.checked_sub(1).and_then(|place| bytes.get(place)) else {
        return true;
    };
    !is_run_byte(before) || (is_escape_letter(before) && at >= 2 && bytes[at - 2] == b'\\')
}

fn prefix_hides(text: &str) -> Vec<Hide> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        count_steps(1);
        if !is_run_byte(bytes[at]) {
            at += 1;
            continue;
        }
        let run_end = take_while(bytes, at, is_run_byte);
        let escaped = at > 0 && bytes[at - 1] == b'\\' && is_escape_letter(bytes[at]);
        let hit = token_at(text, at, run_end)
            .map(|end| (at, end))
            .or_else(|| {
                escaped
                    .then(|| token_at(text, at + 1, run_end).map(|end| (at + 1, end)))
                    .flatten()
            });
        let Some((start, end)) = hit else {
            at = run_end;
            continue;
        };
        found.push(Hide::marker(start, end));
        at = end;
    }
    found
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
    !is_made_of_words(body) && (body.len() >= 40 || body.iter().any(u8::is_ascii_digit))
}

/// Whether a body is hyphenated or underscored words and nothing else: two segments or more, none
/// with both cases in it or two digits or more. A random body has such a segment between any two
/// of its separators, and a name (`streaming-server-1`, `is-a-prefix-of-google-tokens`) does not.
fn is_made_of_words(body: &[u8]) -> bool {
    let mut segments = 0;
    for segment in body
        .split(|byte| matches!(byte, b'-' | b'_'))
        .filter(|segment| !segment.is_empty())
    {
        segments += 1;
        let mixed = segment.iter().any(u8::is_ascii_lowercase)
            && segment.iter().any(u8::is_ascii_uppercase);
        let digits = segment.iter().filter(|byte| byte.is_ascii_digit()).count();
        if mixed || digits >= 2 {
            return false;
        }
    }
    segments >= 2
}

/// Whether `body` is made of the letters, digits, `_` and `-` a base64url body is.
fn is_base64url(body: &[u8]) -> bool {
    body.iter()
        .all(|byte| byte.is_ascii_alphanumeric() | matches!(byte, b'-' | b'_'))
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

/// `sk-ant-` and a type and two digits (`api03-`) and 80 or more base64url characters, as Anthropic's
/// console issues them; `sk-proj-`, `sk-svcacct-` and `sk-admin-` (OpenAI's project, service-account and admin keys) and
/// 40 or more base64url characters with both cases and three digits among them; `sk-live-` and 16 or more that look random; Stripe's `sk_live_` and `sk_test_`
/// and 16 or more letters and digits (a `pk_` key is published and no secret); Shopify's `shpat_`
/// family and 32 hex digits; and OpenAI's older `sk-` and 40 or more letters and digits with a
/// digit and a letter among them.
fn s_token(run: &[u8]) -> bool {
    if let Some(body) = run.strip_prefix(b"sk-ant-") {
        return anthropic(body);
    }
    if let Some(body) = [&b"sk-proj-"[..], b"sk-svcacct-", b"sk-admin-"]
        .iter()
        .find_map(|prefix| run.strip_prefix(*prefix))
    {
        return body.len() >= 40
            && is_base64url(body)
            && body.iter().filter(|byte| byte.is_ascii_digit()).count() >= 3
            && body.iter().any(u8::is_ascii_lowercase)
            && body.iter().any(u8::is_ascii_uppercase)
            && !is_made_of_words(body);
    }
    if let Some(body) = run.strip_prefix(b"sk-live-") {
        return body.len() >= 16 && looks_random(body);
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

/// The body of an Anthropic key after `sk-ant-`: a type of letters and two digits (`api03`, `admin01`),
/// a hyphen, and 80 or more base64url characters.
fn anthropic(body: &[u8]) -> bool {
    let kind = body
        .iter()
        .take_while(|byte| byte.is_ascii_lowercase())
        .count();
    let digits = body[kind..]
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if kind == 0 || digits != 2 || body.get(kind + digits) != Some(&b'-') {
        return false;
    }
    let rest = &body[kind + digits + 1..];
    rest.len() >= 80 && is_base64url(rest)
}

/// `sk_` and `rk_` with `live_` or `test_` and 16 or more letters and digits. A `pk_` key is the
/// publishable half of the pair, which a page carries by design, and is no secret.
fn stripe(run: &[u8]) -> bool {
    let Some(rest) = run
        .strip_prefix(b"sk_")
        .or_else(|| run.strip_prefix(b"rk_"))
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

/// `ya29.` and 30 or more letters, digits, `_` and `-` with both cases and a digit among them: the
/// run is `ya29`, a dot ends it, and the body is the run after the dot.
fn google_oauth(bytes: &[u8], run: &[u8], run_end: usize) -> Option<usize> {
    if run != b"ya29" || bytes.get(run_end) != Some(&b'.') {
        return None;
    }
    let end = take_while(bytes, run_end + 1, is_run_byte);
    let body = &bytes[run_end + 1..end];
    let random = body.len() >= 30
        && is_base64url(body)
        && body.iter().any(u8::is_ascii_digit)
        && body.iter().any(u8::is_ascii_lowercase)
        && body.iter().any(u8::is_ascii_uppercase)
        && !is_made_of_words(body);
    random.then_some(end)
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
/// credential, and the rules have to read it. A header with no end marker within
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
    let closing = ends
        .after(text, after_begin)
        .filter(|at| at - after_begin <= MAX_BLOCK_BYTES)
        .and_then(|at| end_marker_close(bytes, at));
    Some(closing.unwrap_or_else(|| body_lines(text, after_begin)))
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
/// credential glued to base64, and the rules read it.
fn body_lines(text: &str, from: usize) -> usize {
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
        if !accepted || !is_padded_base64(&bytes[start..run_end]) || !ends_a_line(bytes, run_end) {
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
