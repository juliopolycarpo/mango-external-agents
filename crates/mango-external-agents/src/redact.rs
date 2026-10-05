//! Credential-shaped text removed before stderr crosses a diagnostic boundary.
//!
//! A vendor CLI that fails while printing the request it was making prints the header it sent.
//! The unredacted tail is never handed out: [`stderr_text`] is what
//! [`StderrTail::read`](crate::StderrTail::read) returns, so a host that logs the tail logs this
//! form or nothing.
//!
//! Written as a scanner rather than as regular expressions. Three passes, in the same order and
//! with the same shapes as the patterns they replace, so each rule can be read on its own:
//! a bearer header, a `key = value` assignment, and the password in a URL's userinfo.
//!
//! Every pass is linear in the text. A rule is asked at each place a match can start, and the
//! places that share an end share everything after their start, so what depends on the end is
//! decided once, by the function that finds the anchor (see `scan::Candidates`), and not by
//! each start. The work tests in `redact::work` count the bytes read and hold that to the length.

#[cfg(test)]
mod boundaries;
#[cfg(test)]
mod differential;
#[cfg(test)]
mod reference;
mod scan;
mod strip;
#[cfg(test)]
mod work;

use std::borrow::Cow;
use std::ops::Range;

use scan::{Anchor, Candidates, count_steps, first_match, run_start};
pub(crate) use strip::{MAX_ESCAPE_BYTES, holds_string_terminator};
use strip::{remove_boundaries, strip_control_characters};

/// Redacts a stderr tail and strips terminal-unsafe control characters.
///
/// # Example
///
/// ```
/// use mango_external_agents::redact;
///
/// let tail = redact::stderr_text("Authorization: Bearer sk-live-42 API_KEY=hunter2");
/// assert_eq!(tail, "Authorization: Bearer [REDACTED] API_KEY=[REDACTED]");
/// ```
pub fn stderr_text(raw: &str) -> String {
    // Stripped first rather than last. A CLI that colours its output writes `Authorization:` and
    // ` Bearer sk-live-x` either side of an escape sequence, and a rule that reads the two as
    // neighbours never sees the token at all if the sequence is still sitting between them.
    let plain = strip_control_characters(raw);
    let bearer = redact_bearer(&plain);
    let assignments = redact_assignments(&bearer);
    remove_boundaries(redact_url_passwords(&assignments))
}

/// Whether `dropped`, the text a cut removed from the end of what came before, stops where a
/// rule here is still waiting for a credential's value.
///
/// The rules skip line breaks between a name, its separator and its value, so `Authorization:`
/// can end one line and `  Bearer x` be the credential on the next. A caller that discards the
/// first line and keeps the second returns a value whose name is gone. This is that check: it
/// appends each shape a continuation can take and asks the rules whether the appended value was
/// redacted, so the answer follows the rules and not a second list of names and separators.
/// It errs toward `true`: a trailing `password` counts as awaiting even when nothing follows.
pub(crate) fn ends_awaiting_value(dropped: &str) -> bool {
    // Each probe finishes one waiting state: a name that wants its separator, a separator that
    // wants its value, `Authorization` that wants `:`, `Authorization:` that wants a scheme and a
    // scheme that wants its token. `Z` is the value; it survives only when nothing claimed it.
    const PROBES: [&str; 4] = ["\n=Z", "\n:bearer Z", "\nbearer Z", "\nZ"];
    // Only the two rules that read across a line break: a URL password cannot span one.
    let plain = strip_control_characters(dropped);
    // Nothing is awaited without a keyword in the text, and most tails have none.
    let names_a_credential = (0..plain.len()).any(|at| {
        count_steps(1);
        match_credential_keyword(plain.as_bytes(), at).is_some()
            || match_word(plain.as_bytes(), at, AUTHORIZATION).is_some()
    });
    if !names_a_credential {
        return false;
    }
    PROBES.iter().any(|probe| {
        // The text is copied once with its probe, then redacted by the rules, which count their own.
        count_steps(plain.len());
        let probed = redact_assignments(&redact_bearer(&format!("{plain}{probe}")));
        !probed.ends_with('Z')
    })
}

/// A safe executable summary from a host-owned path.
///
/// A diagnostic can name known vendor programs, but an arbitrary executable basename is
/// host-provided text that can carry a secret. Both slash forms are accepted so a Windows path
/// stays safe when formatted on another platform. Unknown names report `custom executable`.
///
/// # Example
///
/// ```
/// use mango_external_agents::redact;
///
/// assert_eq!(redact::program_name("/private/bin/codex"), "codex");
/// assert_eq!(
///     redact::program_name("/private/bin/customer-secret-canary"),
///     "custom executable"
/// );
/// ```
pub fn program_name(raw: &str) -> String {
    let name = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
    if is_known_program(name) {
        return name.to_owned();
    }
    String::from("custom executable")
}

const REDACTED: &str = "[REDACTED]";

/// Every program a harness in this workspace launches on its own account.
///
/// The two first-party CLIs, then the `argv[0]` of each built-in ACP profile in
/// `mango-agent-acp`. A profile whose executable is missing here is reported as
/// `custom executable`, and a launch failure then names a cause without naming which agent it
/// belonged to. The ACP crate holds the list to this one in
/// `every_builtin_profile_is_named_rather_than_redacted_in_a_diagnostic`, so adding a profile
/// without adding its program fails there rather than degrading in a log.
const KNOWN_PROGRAMS: &[&str] = &[
    "claude",
    "codex",
    "cursor-agent",
    "grok",
    "opencode",
    "gemini",
    "copilot",
    "goose",
    "codex-acp",
    "claude-agent-acp",
];

fn is_known_program(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let bare = lower
        .strip_suffix(".exe")
        .or_else(|| lower.strip_suffix(".ps1"))
        .unwrap_or(&lower);
    KNOWN_PROGRAMS.contains(&bare)
}

/// The keyword that makes a variable name a credential's: `api_key` or one of a short list.
fn match_credential_keyword(bytes: &[u8], at: usize) -> Option<usize> {
    const KEYWORDS: &[&[u8]] = &[b"secret", b"token", b"password", b"passwd", b"credential"];
    match_api_key(bytes, at)
        .or_else(|| KEYWORDS.iter().find_map(|word| match_word(bytes, at, word)))
}

/// `authorization : bearer <token>`, however it was spaced and cased.
///
/// `basic` counts as well as `bearer`: it is the same header carrying the same credential, and a
/// base64 user:password is no less a secret for being the older spelling.
fn redact_bearer(raw: &str) -> String {
    rewrite(raw, header_colon, bearer_rule)
}

/// The header name the bearer rule starts at.
const AUTHORIZATION: &[u8] = b"authorization";

fn bearer_rule(bytes: &[u8], at: usize) -> Option<Rewrite> {
    let after_keyword = match_word(bytes, at, AUTHORIZATION)?;
    let after_colon = match_byte(bytes, skip_spaces(bytes, after_keyword), b':')?;
    let scheme_start = skip_spaces(bytes, after_colon);
    let after_scheme = match_word(bytes, scheme_start, b"bearer")
        .or_else(|| match_word(bytes, scheme_start, b"basic"))?;
    // A boundary marker stands where a byte was removed between the scheme and its token.
    let token_start = take_while(bytes, after_scheme, |byte| {
        is_space_byte(byte) || byte == strip::BOUNDARY_BYTE
    });
    if token_start == after_scheme {
        return None;
    }
    let token_end = take_while(bytes, token_start, is_value_byte);
    if token_end == token_start {
        return None;
    }
    Some(Rewrite {
        end: token_end,
        kept_end: after_scheme,
        joiner: " ",
    })
}

/// The next `:` and the one place [`bearer_rule`] can start to reach it.
///
/// The rule wants `authorization`, any spaces, then the colon. The name ends in a letter, so it
/// ends where the spaces before the colon begin and starts its own length before that.
fn header_colon(bytes: &[u8], from: usize) -> Option<Anchor> {
    let colon = first_match(bytes, from, |byte| byte == b':')?;
    let name_end = run_start(bytes, colon, is_space_byte);
    let starts = match name_end.checked_sub(AUTHORIZATION.len()) {
        Some(start) => start..start + 1,
        None => 0..0,
    };
    Some(Anchor { at: colon, starts })
}

/// `api_key=`, `secret:`, `token = `, … and whatever value follows.
fn redact_assignments(raw: &str) -> String {
    rewrite(raw, assignment_separator, assignment_rule)
}

fn assignment_rule(bytes: &[u8], at: usize) -> Option<Rewrite> {
    let after_keyword = match_credential_keyword(bytes, at)?;
    // `AWS_SECRET_ACCESS_KEY=` is a keyword with the rest of a name after it. Requiring the
    // separator to follow the keyword itself would redact only the spellings that happen to
    // end on one, which is a minority of the names credentials actually have.
    let name_end = take_while(bytes, after_keyword, is_name_byte);
    let separator = skip_spaces(bytes, name_end);
    let after_separator =
        match_byte(bytes, separator, b'=').or_else(|| match_byte(bytes, separator, b':'))?;
    let value_start = skip_spaces(bytes, after_separator);
    let value_end = take_while(bytes, value_start, is_value_byte);
    if value_end == value_start {
        return None;
    }
    Some(Rewrite {
        end: value_end,
        kept_end: name_end,
        joiner: "=",
    })
}

/// The next `=` or `:` and the places [`assignment_rule`] can start to reach it.
///
/// The rule wants a keyword, the rest of the name, any spaces, then the separator. A keyword is
/// made of name bytes, so the whole match up to the spaces lies in the run of name bytes that
/// ends where those spaces begin, and it can start anywhere in that run.
///
/// Every start in that run reaches the same separator and the same value: the rule reads on from
/// the keyword to the end of the run whichever start it came from. So whether a value follows is
/// decided here, once, and a separator with none allows no start. Left to the rule, each start
/// of a name like `token_token_token_…` read the rest of it again before finding that out. Only
/// the first byte of the value is read: a value that is there is read by the rule that claims it,
/// and the text is then consumed.
fn assignment_separator(bytes: &[u8], from: usize) -> Option<Anchor> {
    let separator = first_match(bytes, from, |byte| (byte == b'=') | (byte == b':'))?;
    let name_end = run_start(bytes, separator, is_space_byte);
    let name_start = run_start(bytes, name_end, is_name_byte);
    if name_start == name_end || !has_value(bytes, separator + 1) {
        return Some(Anchor {
            at: separator,
            starts: 0..0,
        });
    }
    Some(Anchor {
        at: separator,
        starts: name_start..name_end,
    })
}

/// Whether a value starts after `from`, past any spaces: the check [`assignment_rule`] makes
/// before it redacts anything.
fn has_value(bytes: &[u8], from: usize) -> bool {
    let value_start = skip_spaces(bytes, from);
    bytes
        .get(value_start)
        .is_some_and(|byte| is_value_byte(*byte))
}

/// The password half of `scheme://user:password@host`.
fn redact_url_passwords(raw: &str) -> String {
    rewrite(raw, scheme_separator, url_password_rule)
}

/// What ends a scheme and opens the authority of a URL.
const SCHEME_SEPARATOR: &[u8] = b"://";

fn url_password_rule(bytes: &[u8], at: usize) -> Option<Rewrite> {
    if !bytes.get(at)?.is_ascii_alphabetic() {
        return None;
    }
    let scheme_end = take_while(bytes, at + 1, is_scheme_byte);
    let after_scheme = match_word(bytes, scheme_end, SCHEME_SEPARATOR)?;
    let password = url_password(bytes, after_scheme)?;
    Some(Rewrite {
        end: password.end,
        kept_end: password.start,
        joiner: "",
    })
}

/// Where the password of the URL whose `://` ends at `after_scheme` starts and ends.
///
/// `None` unless the authority is `user:password@`, the user optional and the password not: a
/// URL that has no password, or whose userinfo is never closed by an `@`, is left as it is.
fn url_password(bytes: &[u8], after_scheme: usize) -> Option<Range<usize>> {
    let user_end = take_while(bytes, after_scheme, |byte| {
        !matches!(
            byte,
            b' ' | b'\t' | b'\n' | b'\r' | b':' | b'/' | b'?' | b'#'
        )
    });
    // The user half is optional: `redis://:hunter2@db/main` is what a URL looks like when
    // only a password was configured.
    let password_start = match_byte(bytes, user_end, b':')?;
    let password_end = take_while(bytes, password_start, |byte| {
        !matches!(
            byte,
            b' ' | b'\t' | b'\n' | b'\r' | b'@' | b'/' | b'?' | b'#'
        )
    });
    if password_end == password_start || bytes.get(password_end) != Some(&b'@') {
        return None;
    }
    Some(password_start..password_end)
}

/// What a URL's scheme is made of after its first letter, and the marker a removed byte left in
/// it, which parts nothing: see [`is_name_byte`].
fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        | (byte == b'+')
        | (byte == b'.')
        | (byte == b'-')
        | (byte == strip::BOUNDARY_BYTE)
}

/// The next `:` and the places [`url_password_rule`] can start to reach it.
///
/// The rule wants a letter, the rest of a scheme, then `://`, then a userinfo with a password in
/// it. A colon that does not open `://`, or opens a URL with no password to redact, allows no
/// start; one that does allows any start in the run of scheme bytes that ends at it.
///
/// Every start in that run reaches the same authority, so the answer is found here, once, and
/// not by each start of a scheme like `a.a.a.…` reading the rest of the text again. The
/// check stops at a `/`, and the next `://` has one right after its colon, so no byte is read
/// for two separators.
fn scheme_separator(bytes: &[u8], from: usize) -> Option<Anchor> {
    let colon = first_match(bytes, from, |byte| byte == b':')?;
    let starts = match match_word(bytes, colon, SCHEME_SEPARATOR) {
        Some(after_scheme) if url_password(bytes, after_scheme).is_some() => {
            run_start(bytes, colon, is_scheme_byte)..colon
        }
        _ => 0..0,
    };
    Some(Anchor { at: colon, starts })
}

/// What one rule matched: where the match ends, and what stands in its place.
///
/// The replacement is the start of the match up to `kept_end`, which stays, then `joiner` and
/// [`REDACTED`]. It is described rather than built, so a match costs no allocation.
struct Rewrite {
    end: usize,
    kept_end: usize,
    joiner: &'static str,
}

/// Scans left to right, letting `rule` claim a span starting at a word boundary.
///
/// The result is what trying `rule` at every word boundary in turn gives, the leftmost match
/// first and the next one looked for where it ends. `locate` is how the scan gets there without
/// trying every word: it finds the punctuation the rule cannot match without and the starts that
/// reach it (see [`Candidates`]), and the text between two matches is copied whole. Every rule
/// starts at an ASCII letter and ends at an ASCII byte or the end of the text, so both ends of a
/// copied span are character boundaries.
fn rewrite(
    raw: &str,
    locate: impl Fn(&[u8], usize) -> Option<Anchor>,
    rule: impl Fn(&[u8], usize) -> Option<Rewrite>,
) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut candidates = Candidates::new(locate);
    let mut copied = 0;
    let mut from = 0;
    while let Some(at) = candidates.next_from(bytes, from) {
        from = at + 1;
        if !starts_a_word(bytes, at) {
            continue;
        }
        let Some(found) = rule(bytes, at) else {
            continue;
        };
        out.push_str(&span(raw, copied, at));
        out.push_str(&as_text(bytes, at, found.kept_end));
        out.push_str(found.joiner);
        out.push_str(REDACTED);
        copied = found.end;
        from = found.end;
    }
    out.push_str(&span(raw, copied, raw.len()));
    out
}

/// The `\b` the patterns open with: a match may not start inside a word.
///
/// An underscore counts as a boundary here even though `\w` counts it as a word character. The
/// spelling that matters is `OPENAI_API_KEY`, and treating `_` as part of the preceding word is
/// what let every screaming-snake-case credential walk past the scanner untouched.
fn starts_a_word(bytes: &[u8], at: usize) -> bool {
    match at.checked_sub(1).and_then(|before| bytes.get(before)) {
        Some(byte) => !byte.is_ascii_alphanumeric(),
        None => true,
    }
}

fn match_word(bytes: &[u8], at: usize, word: &[u8]) -> Option<usize> {
    let end = at.checked_add(word.len())?;
    bytes
        .get(at..end)?
        .eq_ignore_ascii_case(word)
        .then_some(end)
}

/// `api[_-]?key`, the one keyword with a shape rather than a spelling.
fn match_api_key(bytes: &[u8], at: usize) -> Option<usize> {
    let after_api = match_word(bytes, at, b"api")?;
    let after_separator = match bytes.get(after_api) {
        Some(b'_' | b'-') => after_api + 1,
        _ => after_api,
    };
    match_word(bytes, after_separator, b"key")
}

fn match_byte(bytes: &[u8], at: usize, expected: u8) -> Option<usize> {
    (bytes.get(at) == Some(&expected)).then_some(at + 1)
}

/// The whitespace every rule here skips between a name, its separator and its value, line
/// breaks included.
pub(crate) fn is_space_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// Whether [`stderr_text`] removes this byte outright, joining the text on either side of it.
///
/// The controls it strips one at a time, a bare carriage return among them. A caller that cuts
/// text where the redactor sees no break can keep half of a credential whose name it dropped.
pub(crate) fn is_stripped_byte(byte: u8) -> bool {
    byte.is_ascii()
        && !matches!(byte, b'\t' | b'\n' | 0x1b)
        && is_unsafe_to_render(char::from(byte))
}

fn skip_spaces(bytes: &[u8], at: usize) -> usize {
    take_while(bytes, at, is_space_byte)
}

fn take_while(bytes: &[u8], at: usize, keep: impl Fn(u8) -> bool) -> usize {
    let mut end = at;
    while let Some(byte) = bytes.get(end) {
        if !keep(*byte) {
            break;
        }
        count_steps(1);
        end += 1;
    }
    end
}

/// What the rest of a variable's name is made of, after the keyword that identified it.
///
/// The stripper leaves a [`BOUNDARY`](strip::BOUNDARY_BYTE) where a removed byte stood in front of
/// a credential name or after `Bearer` or `Basic`, so that a header's token still reads as a word
/// of its own. That is a decision about where a word starts, made for the rules that look for one.
/// It says nothing about where a name ends: `SECRET_BASIC<ESC>[0m=v` is the name `SECRET_BASIC`
/// whatever sits in the middle of it, and a name run that stopped at the marker saw no name before
/// the `=` and let the value through. So a marker is part of the run it interrupts, here and in
/// [`is_scheme_byte`], and [`remove_boundaries`] takes it out of the redacted text afterwards.
///
/// The stripper only puts a marker after a letter or a digit, so a marker between a name and its
/// separator is always the end of the name run and the gap of spaces needs no case of its own.
fn is_name_byte(byte: u8) -> bool {
    // Comparisons joined with `|`, never a `match`: it is a branch per byte in the run loops.
    byte.is_ascii_alphanumeric() | (byte == b'_') | (byte == b'-') | (byte == strip::BOUNDARY_BYTE)
}

/// The `[^\s,;]+` every value in these patterns is.
fn is_value_byte(byte: u8) -> bool {
    !is_space_byte(byte) && !matches!(byte, b',' | b';')
}

/// `raw[from..to]`, borrowed, for a span whose ends are character boundaries.
///
/// Every rule starts at an ASCII letter and ends at an ASCII byte or the end of the text, so the
/// spans [`rewrite`] copies always are. Should one ever not be, the text is repaired the way
/// [`as_text`] repairs it rather than panicking in a diagnostic path. The boundary check is the
/// one slicing makes, so the common case costs the same.
///
/// # Example
///
/// ```ignore
/// assert_eq!(span("caf\u{e9}", 0, 3), "caf");
/// ```
fn span(raw: &str, from: usize, to: usize) -> Cow<'_, str> {
    match raw.get(from..to) {
        Some(text) => Cow::Borrowed(text),
        None => as_text(raw.as_bytes(), from, to),
    }
}

/// `bytes[from..to]` as text, borrowed unless it is not valid UTF-8.
fn as_text(bytes: &[u8], from: usize, to: usize) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes.get(from..to).unwrap_or_default())
}

/// Every code point a diagnostic must not carry across a boundary, tab and newline excepted.
///
/// One list rather than a second opinion: the ranges are the ones
/// [`normalize`](crate::normalize) applies to every other vendor-written string.
fn is_unsafe_to_render(character: char) -> bool {
    let code = u32::from(character);
    matches!(
        code,
        0x00..=0x1f | 0x7f..=0x9f | 0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

#[cfg(test)]
mod tests {
    use super::{
        Anchor, Rewrite, ends_awaiting_value, has_value, is_stripped_byte, program_name, rewrite,
        span, stderr_text, url_password,
    };

    /// The fixture a vendor child writes in the port's own process test.
    const FIXTURE: &str =
        "Authorization: Bearer top-secret API_KEY=another-secret redis://app:password@db/main";

    /// A rule that breaks the invariant `rewrite` relies on: it claims a span that begins in the
    /// middle of the two-byte `\u{e9}`. No rule in this module does, which is the point of not
    /// depending on it.
    #[test]
    fn rewrite_does_not_panic_when_a_span_starts_inside_a_character() {
        let outcome = std::panic::catch_unwind(|| {
            rewrite(
                "\u{e9}-ab",
                |_, from| {
                    (from == 0).then_some(Anchor {
                        at: 1,
                        starts: 1..2,
                    })
                },
                |_, at| {
                    (at == 1).then_some(Rewrite {
                        end: 3,
                        kept_end: 1,
                        joiner: "=",
                    })
                },
            )
        });

        let text = outcome.unwrap_or_else(|_| {
            panic!(
                "expected rewrite to copy the text before a span that starts mid-character | received a panic"
            )
        });
        assert_eq!(
            text, "\u{fffd}=[REDACTED]ab",
            "expected the split character repaired and the rest kept | received {text:?}"
        );
    }

    #[test]
    fn a_span_borrows_a_whole_slice_and_repairs_a_cut_character() {
        assert!(
            matches!(span("caf\u{e9}", 0, 3), std::borrow::Cow::Borrowed("caf")),
            "expected a borrowed slice between boundaries"
        );
        assert_eq!(span("caf\u{e9}", 0, 4), "caf\u{fffd}");
        assert_eq!(span("caf\u{e9}", 4, 5), "\u{fffd}");
        assert_eq!(
            span("abc", 2, 9),
            "",
            "expected an out-of-range span to be empty"
        );
    }

    #[test]
    fn redacts_every_credential_shape_in_the_fixture() {
        let redacted = stderr_text(FIXTURE);

        assert!(
            !redacted.contains("top-secret"),
            "expected no bearer token, received {redacted:?}"
        );
        assert!(
            !redacted.contains("another-secret"),
            "expected no api key, received {redacted:?}"
        );
        assert!(
            !redacted.contains("password@"),
            "expected no url password, received {redacted:?}"
        );
        assert_eq!(
            redacted,
            "Authorization: Bearer [REDACTED] API_KEY=[REDACTED] redis://app:[REDACTED]@db/main"
        );
    }

    #[test]
    fn program_name_keeps_known_vendors_and_omits_arbitrary_basenames() {
        assert_eq!(program_name("/private/bin/codex"), "codex");
        assert_eq!(program_name(r"C:\\private\\bin\\CLAUDE.EXE"), "CLAUDE.EXE");
        assert_eq!(
            program_name("/private/bin/customer-secret-canary"),
            "custom executable"
        );
        assert_eq!(
            program_name("/private/bin/token=secret"),
            "custom executable"
        );
    }

    /// The ACP adapters a launch failure could otherwise only call `custom executable`.
    ///
    /// This name is the only statement of *which* agent failed to start: the rest of a
    /// `Error::Launch` diagnostic says what stopped it, not whose CLI it was.
    #[test]
    fn program_name_keeps_every_built_in_acp_executable() {
        for program in [
            "gemini",
            "copilot",
            "goose",
            "codex-acp",
            "claude-agent-acp",
        ] {
            assert_eq!(
                program_name(&format!("/private/bin/{program}")),
                program,
                "expected {program} to be named, received a redacted stand-in"
            );
        }
    }

    #[test]
    fn matches_however_the_vendor_spaced_and_cased_it() {
        assert_eq!(
            stderr_text("authorization :  BEARER\tsk-live-1"),
            "authorization :  BEARER [REDACTED]"
        );
        assert_eq!(stderr_text("Api-Key : v"), "Api-Key=[REDACTED]");
        assert_eq!(stderr_text("apikey=v"), "apikey=[REDACTED]");
        assert_eq!(stderr_text("PASSWD:v"), "PASSWD=[REDACTED]");
    }

    #[test]
    fn leaves_a_keyword_inside_a_longer_word_alone() {
        assert_eq!(stderr_text("mytoken=v"), "mytoken=v");
        assert_eq!(
            stderr_text("a_token_count_of=3"),
            "a_token_count_of=[REDACTED]"
        );
    }

    /// The spellings credentials actually have. Each of these walked past the scanner while an
    /// underscore counted as a word character and the keyword had to touch the separator.
    #[test]
    fn redacts_a_credential_named_the_way_credentials_are_named() {
        for (line, expected) in [
            ("OPENAI_API_KEY=sk-proj-x", "OPENAI_API_KEY=[REDACTED]"),
            ("ANTHROPIC_API_KEY=sk-ant-x", "ANTHROPIC_API_KEY=[REDACTED]"),
            ("GITHUB_TOKEN=ghp_x", "GITHUB_TOKEN=[REDACTED]"),
            (
                "AWS_SECRET_ACCESS_KEY=wJalrX",
                "AWS_SECRET_ACCESS_KEY=[REDACTED]",
            ),
            ("CLIENT_SECRET=shh", "CLIENT_SECRET=[REDACTED]"),
            ("my_token=v", "my_token=[REDACTED]"),
        ] {
            assert_eq!(stderr_text(line), expected, "expected {expected:?}");
        }
    }

    #[test]
    fn a_colour_sequence_does_not_hide_the_token_that_follows_it() {
        assert_eq!(
            stderr_text("Authorization:\u{1b}[1m Bearer sk-live-x"),
            "Authorization: Bearer [REDACTED]"
        );
        assert_eq!(
            stderr_text("\u{1b}[31mAPI_KEY\u{1b}[0m=secret-value"),
            "API_KEY=[REDACTED]"
        );
    }

    #[test]
    fn a_value_ends_at_the_separator_that_follows_it() {
        assert_eq!(stderr_text("token=abc, next=1"), "token=[REDACTED], next=1");
        assert_eq!(stderr_text("secret=abc; more"), "secret=[REDACTED]; more");
    }

    #[test]
    fn a_url_without_a_password_is_left_intact() {
        assert_eq!(
            stderr_text("https://example.com/x"),
            "https://example.com/x"
        );
        assert_eq!(stderr_text("redis://db:6379/0"), "redis://db:6379/0");
    }

    #[test]
    fn a_url_whose_userinfo_is_only_a_password_is_redacted() {
        assert_eq!(
            stderr_text("redis://:hunter2@db/main"),
            "redis://:[REDACTED]@db/main"
        );
    }

    #[test]
    fn strips_control_characters_but_keeps_tabs_and_newlines() {
        let redacted = stderr_text("a\u{1b}[31mb\u{0}c\td\ne\u{9f}f");
        assert_eq!(redacted, "abc\td\nef");
    }

    /// The tail is vendor-written text rendered in a host's diagnostics, so it is bounded on the
    /// same terms as every other vendor string. An override left in a log renders a filename, or
    /// the command that produced it, in an order its code points do not have.
    #[test]
    fn strips_the_bidirectional_set_a_log_would_otherwise_render_backwards() {
        for code in [0x061c, 0x200e, 0x200f, 0x202a, 0x202e, 0x2066, 0x2069] {
            let character = char::from_u32(code).expect("expected a character");
            assert_eq!(
                stderr_text(&format!("error: cannot run {character}gpj.exe")),
                "error: cannot run gpj.exe",
                "expected U+{code:04X} to be stripped"
            );
        }
    }

    /// The same header carrying the same credential. A base64 `user:password` is no less a secret
    /// for being the older spelling, and it reached a log while only `bearer` was matched.
    #[test]
    fn redacts_a_basic_credential_as_well_as_a_bearer_one() {
        assert_eq!(
            stderr_text("Authorization: Basic dXNlcjpodW50ZXIy"),
            "Authorization: Basic [REDACTED]"
        );
        assert_eq!(
            stderr_text("authorization:basic\tdXNlcjpodW50ZXIy"),
            "authorization:basic [REDACTED]"
        );
    }

    #[test]
    fn redacts_multiline_credentials_after_stderr_arrives_in_chunks() {
        let tail = concat!(
            "request failed\nAuthorization:\n",
            "  Bearer multiline-secret\n",
            "retry at https://user:url-secret@agent.internal"
        );
        let redacted = stderr_text(tail);

        for secret in ["multiline-secret", "url-secret"] {
            assert!(
                !redacted.contains(secret),
                "expected no credential from chunked stderr, received {redacted:?}"
            );
        }
        assert!(
            redacted.contains("request failed"),
            "expected safe diagnostic context, received {redacted:?}"
        );
    }

    #[test]
    fn keeps_text_that_carries_no_credential() {
        let line = "error: the vendor exited with status 1 (no configuration found)";
        assert_eq!(stderr_text(line), line);
    }

    #[test]
    fn survives_multibyte_text() {
        assert_eq!(
            stderr_text("não encontrado: token=café"),
            "não encontrado: token=[REDACTED]"
        );
    }

    #[test]
    fn text_ending_where_a_value_is_awaited_is_recognised() {
        for dropped in [
            "noise API_KEY=",
            "noise API_KEY = ",
            "noise OPENAI_API_KEY",
            "noise secret:",
            "noise Authorization",
            "noise Authorization:",
            "noise authorization: Bearer",
            "noise Authorization: basic ",
        ] {
            assert!(
                ends_awaiting_value(dropped),
                "expected {dropped:?} to await a value | received false"
            );
        }
    }

    #[test]
    fn text_that_leaves_no_value_awaited_is_not() {
        for dropped in [
            "",
            "error: the vendor exited with status 1",
            "noise API_KEY=value",
            "Authorization: Bearer sk-live-42",
            "retry at https://user:url-secret@agent.internal",
            "an ordinary line ending in a colon:",
        ] {
            assert!(
                !ends_awaiting_value(dropped),
                "expected {dropped:?} to await nothing | received true"
            );
        }
    }

    #[test]
    fn a_byte_the_redactor_removes_is_reported_as_stripped() {
        for byte in [0x00, b'\r', 0x0b, 0x0c, 0x7f] {
            assert!(
                is_stripped_byte(byte),
                "expected {byte:#04x} to be reported as stripped | received false"
            );
        }
        for byte in [b' ', b'\t', b'\n', 0x1b, b'a', b'=', 0xc3] {
            assert!(
                !is_stripped_byte(byte),
                "expected {byte:#04x} to be kept | received true"
            );
        }
        assert_eq!(
            stderr_text("API_\rKEY=v"),
            "API_KEY=[REDACTED]",
            "expected the redactor to join the text around a carriage return"
        );
    }

    #[test]
    fn a_value_is_what_the_assignment_rule_would_claim() {
        for (text, expected) in [
            ("v", true),
            ("  \n\tv", true),
            ("=:", true),
            ("", false),
            ("   ", false),
            (",", false),
            ("  ;x", false),
        ] {
            assert_eq!(
                has_value(text.as_bytes(), 0),
                expected,
                "expected has_value({text:?}) to be {expected}"
            );
        }
        assert!(
            !has_value(b"k=  ", 2),
            "expected no value after the separator at 1 of \"k=  \""
        );
    }

    #[test]
    fn a_url_password_is_found_only_where_the_authority_closes_it_with_an_at() {
        for (text, expected) in [
            ("user:secret@host", Some(5..11)),
            (":secret@host", Some(1..7)),
            ("u:p@", Some(2..3)),
            ("user@host", None),
            ("user:@host", None),
            ("user:secret", None),
            ("user:secret/x@host", None),
            ("user:sec ret@host", None),
            ("a/b:c@d", None),
            ("", None),
        ] {
            assert_eq!(
                url_password(text.as_bytes(), 0),
                expected,
                "expected the password of {text:?} to span {expected:?}"
            );
        }
    }
}
