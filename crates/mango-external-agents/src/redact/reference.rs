//! The redactor exactly as 0.4.1 shipped it, kept as the definition of the right answer.
//!
//! The rules and the scan that finds where to try them were rewritten so that no text is read
//! twice on behalf of a candidate that cannot match. Every change of that kind has to return what
//! this copy returns, byte for byte, so a rewrite that moved a boundary or dropped a credential
//! fails `redact::differential` with the input that showed it. This copy is quadratic on some
//! text (the reason for the rewrite), so only short text is run through it. It is frozen: do not
//! edit it to follow a change to the rules, edit the rules and keep this one as it is.
//!
//! Nothing is shared with the code under test. The scanners (`scan`, with `Anchor` and
//! `Candidates`) and the control-character stripper (`strip`, with its character-at-a-time oracle)
//! are the files of the tag too, so a change to a leaf function of the live redactor is held to
//! the same answer as a change to a rule. Only `BOUNDARY_BYTE`, a byte the stripper writes, is
//! named here a second time.

mod scan;
mod strip;

use scan::{Anchor, Candidates, first_match, run_start};
use strip::BOUNDARY_BYTE;
pub(super) use strip::strip_every_character;
pub(super) use strip::{remove_boundaries, strip_control_characters};

/// Every code point a diagnostic must not carry across a boundary, tab and newline excepted, as
/// 0.4.1 listed them.
fn is_unsafe_to_render(character: char) -> bool {
    let code = u32::from(character);
    matches!(
        code,
        0x00..=0x1f | 0x7f..=0x9f | 0x061c | 0x200e | 0x200f | 0x202a..=0x202e | 0x2066..=0x2069
    )
}

/// [`super::stderr_text`] as 0.4.1 shipped it.
pub(super) fn stderr_text(raw: &str) -> String {
    let plain = strip_control_characters(raw);
    let bearer = redact_bearer(&plain);
    let assignments = redact_assignments(&bearer);
    remove_boundaries(redact_url_passwords(&assignments))
}

/// [`super::ends_awaiting_value`] as 0.4.1 shipped it.
pub(super) fn ends_awaiting_value(dropped: &str) -> bool {
    const PROBES: [&str; 4] = ["\n=Z", "\n:bearer Z", "\nbearer Z", "\nZ"];
    let plain = strip_control_characters(dropped);
    let names_a_credential = (0..plain.len()).any(|at| {
        match_credential_keyword(plain.as_bytes(), at).is_some()
            || match_word(plain.as_bytes(), at, AUTHORIZATION).is_some()
    });
    if !names_a_credential {
        return false;
    }
    PROBES.iter().any(|probe| {
        let probed = redact_assignments(&redact_bearer(&format!("{plain}{probe}")));
        !probed.ends_with('Z')
    })
}

const REDACTED: &str = "[REDACTED]";

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
pub(super) fn redact_bearer(raw: &str) -> String {
    rewrite(raw, header_colon, bearer_rule)
}

/// The header name the bearer rule starts at.
const AUTHORIZATION: &[u8] = b"authorization";

pub(super) fn bearer_rule(bytes: &[u8], at: usize) -> Option<Rewrite> {
    let after_keyword = match_word(bytes, at, AUTHORIZATION)?;
    let after_colon = match_byte(bytes, skip_spaces(bytes, after_keyword), b':')?;
    let scheme_start = skip_spaces(bytes, after_colon);
    let after_scheme = match_word(bytes, scheme_start, b"bearer")
        .or_else(|| match_word(bytes, scheme_start, b"basic"))?;
    // A boundary marker stands where a byte was removed between the scheme and its token.
    let token_start = take_while(bytes, after_scheme, |byte| {
        is_space_byte(byte) || byte == BOUNDARY_BYTE
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
        replacement: format!("{} {REDACTED}", as_text(bytes, at, after_scheme)),
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
pub(super) fn redact_assignments(raw: &str) -> String {
    rewrite(raw, assignment_separator, assignment_rule)
}

pub(super) fn assignment_rule(bytes: &[u8], at: usize) -> Option<Rewrite> {
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
        replacement: format!("{}={REDACTED}", as_text(bytes, at, name_end)),
    })
}

/// The next `=` or `:` and the places [`assignment_rule`] can start to reach it.
///
/// The rule wants a keyword, the rest of the name, any spaces, then the separator. A keyword is
/// made of name bytes, so the whole match up to the spaces lies in the run of name bytes that
/// ends where those spaces begin, and it can start anywhere in that run.
fn assignment_separator(bytes: &[u8], from: usize) -> Option<Anchor> {
    let separator = first_match(bytes, from, |byte| (byte == b'=') | (byte == b':'))?;
    let name_end = run_start(bytes, separator, is_space_byte);
    let name_start = run_start(bytes, name_end, is_name_byte);
    Some(Anchor {
        at: separator,
        starts: name_start..name_end,
    })
}

/// The password half of `scheme://user:password@host`.
pub(super) fn redact_url_passwords(raw: &str) -> String {
    rewrite(raw, scheme_separator, url_password_rule)
}

/// What ends a scheme and opens the authority of a URL.
const SCHEME_SEPARATOR: &[u8] = b"://";

pub(super) fn url_password_rule(bytes: &[u8], at: usize) -> Option<Rewrite> {
    if !bytes.get(at)?.is_ascii_alphabetic() {
        return None;
    }
    let scheme_end = take_while(bytes, at + 1, is_scheme_byte);
    let after_scheme = match_word(bytes, scheme_end, SCHEME_SEPARATOR)?;
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
    Some(Rewrite {
        end: password_end,
        replacement: format!("{}{REDACTED}", as_text(bytes, at, password_start)),
    })
}

/// What a URL's scheme is made of after its first letter.
fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'.' | b'-')
}

/// The next `:` and the places [`url_password_rule`] can start to reach it.
///
/// The rule wants a letter, the rest of a scheme, then `://`. A colon that does not open `://`
/// allows no start; one that does allows any start in the run of scheme bytes that ends at it.
fn scheme_separator(bytes: &[u8], from: usize) -> Option<Anchor> {
    let colon = first_match(bytes, from, |byte| byte == b':')?;
    let starts = match match_word(bytes, colon, SCHEME_SEPARATOR) {
        Some(_) => run_start(bytes, colon, is_scheme_byte)..colon,
        None => 0..0,
    };
    Some(Anchor { at: colon, starts })
}

/// What one rule matched: where the match ends, and what stands in its place.
pub(super) struct Rewrite {
    pub(super) end: usize,
    pub(super) replacement: String,
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
        out.push_str(&raw[copied..at]);
        out.push_str(&found.replacement);
        copied = found.end;
        from = found.end;
    }
    out.push_str(&raw[copied..]);
    out
}

/// The `\b` the patterns open with: a match may not start inside a word.
///
/// An underscore counts as a boundary here even though `\w` counts it as a word character. The
/// spelling that matters is `OPENAI_API_KEY`, and treating `_` as part of the preceding word is
/// what let every screaming-snake-case credential walk past the scanner untouched.
pub(super) fn starts_a_word(bytes: &[u8], at: usize) -> bool {
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
fn is_space_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
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
        end += 1;
    }
    end
}

/// What the rest of a variable's name is made of, after the keyword that identified it.
fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

/// The `[^\s,;]+` every value in these patterns is.
fn is_value_byte(byte: u8) -> bool {
    !is_space_byte(byte) && !matches!(byte, b',' | b';')
}

fn as_text(bytes: &[u8], from: usize, to: usize) -> String {
    String::from_utf8_lossy(bytes.get(from..to).unwrap_or_default()).into_owned()
}
