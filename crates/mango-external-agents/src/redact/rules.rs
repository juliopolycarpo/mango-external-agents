//! The three credential rules: a header, a name with a value, and a URL's userinfo.
//!
//! Each is a rule (tried at a place a match can start) and a locator (finds the punctuation a
//! rule cannot match without, and the starts that reach it; see [`Candidates`]). The scan in
//! `redact` runs a rule only where its locator allows, and `redact::differential` holds every
//! locator to the scan that tries the rule at every byte.
//!
//! A name may be quoted (`"api_key": "sk-..."`, `'Authorization': 'Bearer ...'`, or the same with
//! the quotes escaped inside a string), a value may be quoted and then runs to its closing quote
//! whatever it holds, and a few values are left alone because they say a credential is missing
//! and not what it is (see [`status_phrase_end`]).

use std::ops::Range;

use super::scan::{Anchor, count_steps, first_match, run_start};
use super::structured::{CREDENTIAL_NAMES, CREDENTIAL_WORDS};
use super::{
    Outcome, Rewrite, is_name_byte, is_space_byte, is_value_byte, match_byte, match_word,
    skip_spaces, strip, take_while,
};

/// The header name the bearer rule starts at.
pub(super) const AUTHORIZATION: &[u8] = b"authorization";

/// What a URL's scheme is made of after its first letter, and the marker a removed byte left in
/// it, which parts nothing: see [`is_name_byte`].
pub(super) fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        | (byte == b'+')
        | (byte == b'.')
        | (byte == b'-')
        | (byte == strip::BOUNDARY_BYTE)
}

/// What ends a scheme and opens the authority of a URL.
const SCHEME_SEPARATOR: &[u8] = b"://";

/// The `\b` the rules open with: a match may not start inside a word.
///
/// An underscore counts as a boundary here even though `\w` counts it as a word character. The
/// spelling that matters is `OPENAI_API_KEY`, and treating `_` as part of the preceding word is
/// what let every screaming-snake-case credential walk past the scanner untouched.
#[inline]
pub(super) fn starts_a_word(bytes: &[u8], at: usize) -> bool {
    match at.checked_sub(1).and_then(|before| bytes.get(before)) {
        Some(byte) => !byte.is_ascii_alphanumeric(),
        None => true,
    }
}

fn is_quote(byte: u8) -> bool {
    (byte == b'"') | (byte == b'\'')
}

/// Where a closing quote that ends at `end` begins, with the backslash that escapes it, or `end`
/// when the byte before it is not a quote.
fn before_quote(bytes: &[u8], end: usize) -> usize {
    match end.checked_sub(1) {
        Some(quote) if bytes.get(quote).is_some_and(|byte| is_quote(*byte)) => {
            match quote.checked_sub(1) {
                Some(slash) if bytes.get(slash) == Some(&b'\\') => slash,
                _ => quote,
            }
        }
        _ => end,
    }
}

/// The byte after a quote at `at`, or `at` when there is none. A backslash may come first: the
/// quotes of a JSON text inside a string are escaped.
fn skip_quote(bytes: &[u8], at: usize) -> usize {
    let quote = match bytes.get(at) {
        Some(b'\\') => at + 1,
        _ => at,
    };
    match bytes.get(quote) {
        Some(byte) if is_quote(*byte) => quote + 1,
        _ => at,
    }
}

// ---- keywords -------------------------------------------------------------------------------

/// How a keyword's value is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// One word, as a value has always been.
    Token,
    /// A password: after a `:` it runs to the end of the line or the next comma or semicolon,
    /// because a person writes `password: two words`.
    Phrase,
    /// A cookie header: to the end of the line, whatever it holds.
    Line,
}

/// A credential keyword at a place in the text.
struct Keyword {
    /// Where it ends.
    end: usize,
    kind: Kind,
    /// Whether the name must be this keyword and nothing more: a bare query parameter.
    whole: bool,
    /// Whether it counts tokens when its name says so (`tokens`, `token_limit`).
    token: bool,
}

/// `api`, `private` or `secret`, an optional separator or space, then `key`.
fn match_key_shape(bytes: &[u8], at: usize) -> Option<usize> {
    let after_word = ["api", "private", "secret"]
        .iter()
        .find_map(|word| match_word(bytes, at, word.as_bytes()))?;
    let after_separator = match bytes.get(after_word) {
        Some(b'_' | b'-' | b' ') => after_word + 1,
        _ => after_word,
    };
    match_word(bytes, after_separator, b"key")
}

/// The keyword that makes a name a credential's, from the one list the JSON walk uses too.
///
/// `authorization` is the header rule's; `apikey` and `privatekey` also read `api key` and
/// `private key`. A bare `key`, `auth`, `pass` or `sig` is a credential's only as a query parameter.
fn match_keyword(bytes: &[u8], at: usize) -> Option<Keyword> {
    let first = *bytes.get(at)? | 0x20;
    if matches!(first, b'a' | b'p' | b's')
        && let Some(end) = match_key_shape(bytes, at)
    {
        return Some(Keyword {
            end,
            kind: Kind::Token,
            whole: false,
            token: false,
        });
    }
    for word in CREDENTIAL_WORDS {
        // Most words start with another letter, and a string compare is dearer than this one.
        if word.as_bytes()[0] != first || matches!(word, "authorization" | "apikey" | "privatekey")
        {
            continue;
        }
        if let Some(end) = match_word(bytes, at, word.as_bytes()) {
            let kind = match word {
                "password" | "passwd" | "pwd" | "passphrase" => Kind::Phrase,
                "cookie" => Kind::Line,
                _ => Kind::Token,
            };
            return Some(Keyword {
                end,
                kind,
                whole: false,
                token: word == "token",
            });
        }
    }
    None
}

/// A query parameter that is a credential's only when it is the whole name.
fn match_query_name(bytes: &[u8], at: usize) -> Option<Keyword> {
    if !matches!(
        at.checked_sub(1).and_then(|before| bytes.get(before)),
        Some(b'?' | b'&')
    ) {
        return None;
    }
    let end = take_while(bytes, at, |byte| byte.is_ascii_alphanumeric());
    let name = &bytes[at..end];
    let credential = ["key", "signature"]
        .iter()
        .chain(CREDENTIAL_NAMES.iter())
        .any(|word| name.eq_ignore_ascii_case(word.as_bytes()));
    credential.then_some(Keyword {
        end,
        kind: Kind::Token,
        whole: true,
        token: false,
    })
}

/// Whether some keyword starts at `at`, for a caller that only asks whether a text names a
/// credential at all.
pub(super) fn names_a_keyword(bytes: &[u8], at: usize) -> bool {
    starts_a_keyword(bytes, at)
        && (match_keyword(bytes, at).is_some() || match_query_name(bytes, at).is_some())
}

/// Whether the two bytes at `at` open some keyword, of `key` or of `signature`: `se`cret,
/// `si`g, `to`ken, `pa`ssword, `pw`d, `pr`ivate, `cr`edential, `co`okie, `ap`i, `au`th, `ke`y.
///
/// Every byte of a name is a start, since a keyword may end a name or follow a capital, and most
/// are the beginning of nothing. This turns those away before any word is compared.
#[inline(always)]
fn starts_a_keyword(bytes: &[u8], at: usize) -> bool {
    let Some(&opening) = bytes.get(at) else {
        return false;
    };
    if !opens_a_keyword(opening) {
        return false;
    }
    let Some(&next) = bytes.get(at + 1) else {
        return false;
    };
    let second = next | 0x20;
    match opening | 0x20 {
        b's' => (second == b'e') | (second == b'i'),
        b't' => second == b'o',
        b'p' => (second == b'a') | (second == b'w') | (second == b'r'),
        b'c' => (second == b'r') | (second == b'o'),
        b'a' => (second == b'p') | (second == b'u'),
        _ => second == b'e',
    }
}

/// Whether a byte is the first letter of some keyword: `a`, `c`, `k`, `p`, `s` or `t`, either case.
fn opens_a_keyword(byte: u8) -> bool {
    /// Bit `n` is set when the letter `a + n` opens a keyword.
    const OPENING_LETTERS: u32 =
        (1 << 0) | (1 << 2) | (1 << 10) | (1 << 15) | (1 << 18) | (1 << 19);
    let letter = (byte | 0x20).wrapping_sub(b'a');
    (letter < 26) & ((OPENING_LETTERS >> (letter & 31)) & 1 == 1)
}

/// A lowercase letter then a capital: `accessToken`.
fn is_camel_boundary(bytes: &[u8], at: usize) -> bool {
    bytes.get(at).is_some_and(u8::is_ascii_uppercase)
        && at
            .checked_sub(1)
            .and_then(|before| bytes.get(before))
            .is_some_and(u8::is_ascii_lowercase)
}

/// Whether a keyword at `at`, ending at `end`, may begin there: at the start of a word, at a
/// capital inside one, or as the last part of a name. `PGPASSWORD`, `csrftoken` and `accessToken`
/// are credentials' names; `mytokenizer` and `passwordless` are words.
fn keyword_may_start(bytes: &[u8], at: usize, end: usize) -> bool {
    starts_a_word(bytes, at)
        || is_camel_boundary(bytes, at)
        || !bytes.get(end).is_some_and(|byte| is_name_byte(*byte))
}

/// Whether the rest of a name after `token` says it counts tokens: `tokens`, `token_limit`,
/// `tokenCount`. A number under such a name is a count and not a credential.
fn counts_tokens(bytes: &[u8], after_keyword: usize) -> bool {
    const COUNTS: [&str; 8] = [
        "count",
        "limit",
        "used",
        "usage",
        "budget",
        "remaining",
        "total",
        "max",
    ];
    if bytes
        .get(after_keyword)
        .is_some_and(|byte| byte.eq_ignore_ascii_case(&b's'))
        && !bytes
            .get(after_keyword + 1)
            .is_some_and(u8::is_ascii_lowercase)
    {
        return true;
    }
    let word = match bytes.get(after_keyword) {
        Some(b'_' | b'-') => after_keyword + 1,
        _ => after_keyword,
    };
    COUNTS
        .iter()
        .any(|count| match_word(bytes, word, count.as_bytes()).is_some())
}

// ---- values that say a credential is missing ------------------------------------------------

/// The longest status word, `unauthorized`, `unavailable` and `configured` among them.
const LONGEST_STATUS_WORD: usize = 12;

/// Whether a word is a status word or an HTTP error status (`401`, `503`).
///
/// Words that say a credential is absent or refused (`not`, `set`, `missing`, `expired`,
/// `invalid`, `required`, `revoked`, `unauthorized`, `forbidden`, `empty`, `denied`, ...) and the
/// words of an HTTP reason phrase (`too many requests`, `internal server error`, `bad request`).
/// A value made only of these is a status, not a credential. Every other word is a value, which
/// includes any word this list does not know: when in doubt the text is redacted.
fn is_status_word(word: &[u8]) -> bool {
    if (word.len() > LONGEST_STATUS_WORD) | word.is_empty() {
        return false;
    }
    let mut lower = [0u8; LONGEST_STATUS_WORD];
    for (to, from) in lower.iter_mut().zip(word) {
        *to = from.to_ascii_lowercase();
    }
    let word = &lower[..word.len()];
    let http =
        word.len() == 3 && matches!(word[0], b'4' | b'5') && word.iter().all(u8::is_ascii_digit);
    http || matches!(
        word,
        b"not"
            | b"set"
            | b"found"
            | b"provided"
            | b"configured"
            | b"missing"
            | b"expired"
            | b"invalid"
            | b"required"
            | b"revoked"
            | b"unauthorized"
            | b"forbidden"
            | b"unset"
            | b"empty"
            | b"unknown"
            | b"none"
            | b"null"
            | b"denied"
            | b"rejected"
            | b"failed"
            | b"refused"
            | b"absent"
            | b"undefined"
            | b"unavailable"
            | b"bad"
            | b"request"
            | b"requests"
            | b"too"
            | b"many"
            | b"internal"
            | b"server"
            | b"error"
            | b"service"
            | b"timeout"
    )
}

/// Where the value at `start` ends, when it is nothing but status words: `not set`, `missing`,
/// `401 Unauthorized`, up to the end of the text, a line break or a delimiter. Anything else after
/// the words, such as another word or an underscore, makes it a value.
///
/// Reads a handful of words at most, so it costs the same however much text follows.
fn status_phrase_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut at = start;
    let mut words = 0;
    let end = loop {
        let word_end = take_while(bytes, at, |byte| byte.is_ascii_alphanumeric());
        if word_end == at || !is_status_word(&bytes[at..word_end]) {
            return None;
        }
        words += 1;
        let next = take_while(bytes, word_end, |byte| (byte == b' ') | (byte == b'\t'));
        // Another word follows only if there are few enough of them to be all status words.
        if next == word_end || !bytes.get(next).is_some_and(u8::is_ascii_alphanumeric) {
            break word_end;
        }
        if words == 6 {
            return None;
        }
        at = next;
    };
    let delimited = match bytes.get(end) {
        None => true,
        Some(byte) => {
            is_space_byte(*byte)
                || matches!(byte, b',' | b';' | b'.' | b')' | b']' | b'}' | b'"' | b'\'')
        }
    };
    delimited.then_some(end)
}

/// Whether `text` is only digits, with the separators a number is written with.
fn only_digits(text: &[u8]) -> bool {
    !text.is_empty()
        && text
            .iter()
            .all(|byte| byte.is_ascii_digit() | (*byte == b',') | (*byte == b'_'))
}

// ---- the bearer rule ------------------------------------------------------------------------

/// `authorization : <scheme> <token>`, however it was spaced and cased, with the name and the
/// value optionally quoted.
///
/// Any scheme counts, not only `bearer` and `basic`: `token`, `digest`, an API gateway's own. The
/// words after the scheme are a token unless they say a credential is missing (`Authorization:
/// header missing`).
#[inline]
pub(super) fn bearer_rule(bytes: &[u8], at: usize) -> Option<Outcome> {
    if !starts_a_word(bytes, at) {
        return None;
    }
    let after_keyword = match_word(bytes, at, AUTHORIZATION)?;
    let after_quote = skip_quote(bytes, after_keyword);
    let after_colon = match_byte(bytes, skip_spaces(bytes, after_quote), b':')?;
    let scheme_start = skip_quote(bytes, skip_spaces(bytes, after_colon));
    let after_scheme = take_while(bytes, scheme_start, |byte| {
        byte.is_ascii_alphanumeric() | (byte == b'-') | (byte == b'_') | (byte == b'.')
    });
    if after_scheme == scheme_start {
        return None;
    }
    // A boundary marker stands where a byte was removed between the scheme and its token.
    let token_start = take_while(bytes, after_scheme, |byte| {
        is_space_byte(byte) || byte == strip::BOUNDARY_BYTE
    });
    if token_start == after_scheme {
        return None;
    }
    let quoted = scheme_start != skip_spaces(bytes, after_colon);
    let token_end = match (
        quoted,
        is_single_token_scheme(&bytes[scheme_start..after_scheme]),
    ) {
        // Inside quotes the value ends at the closing one.
        (true, _) => take_while(bytes, token_start, |byte| {
            is_value_byte(byte) && !is_quote(byte)
        }),
        (false, true) => take_while(bytes, token_start, is_value_byte),
        // `Digest username="a", response="b"` and `AWS4-HMAC-SHA256 Credential=..., Signature=...`
        // are several parts, each of which is a credential.
        (false, false) => line_end(bytes, token_start),
    };
    if token_end == token_start {
        return None;
    }
    // `Authorization: required` or `Authorization: 401 Unauthorized` is a status, not a header. Only
    // the words straight after the colon count: a scheme followed by anything is a credential.
    if let Some(end) = status_phrase_end(bytes, scheme_start) {
        return Some(Outcome::Keep { end });
    }
    Some(Outcome::Redact(Rewrite {
        end: token_end,
        kept_end: after_scheme,
        joiner: " ",
    }))
}

/// Whether an authorization scheme carries one token, so that a word after it is not part of it.
fn is_single_token_scheme(scheme: &[u8]) -> bool {
    [
        &b"bearer"[..],
        b"basic",
        b"token",
        b"bot",
        b"negotiate",
        b"ntlm",
    ]
    .iter()
    .any(|single| scheme.eq_ignore_ascii_case(single))
}

/// The next `:` and the one place [`bearer_rule`] can start to reach it.
///
/// The rule wants `authorization`, an optional quote, any spaces, then the colon. The name ends in
/// a letter, so it ends where the quote and the spaces before the colon begin and starts its own
/// length before that.
#[inline]
pub(super) fn header_colon(bytes: &[u8], from: usize) -> Option<Anchor> {
    let colon = first_match(bytes, from, |byte| byte == b':')?;
    let name_end = before_quote(bytes, run_start(bytes, colon, is_space_byte));
    // The name ends in `n`: nearly every colon is turned away on that byte alone.
    let ends_in_n = name_end
        .checked_sub(1)
        .and_then(|last| bytes.get(last))
        .is_some_and(|byte| (byte | 0x20) == b'n');
    let starts = match name_end.checked_sub(AUTHORIZATION.len()) {
        Some(start) if ends_in_n => start..start + 1,
        _ => 0..0,
    };
    Some(Anchor { at: colon, starts })
}

// ---- the assignment rule --------------------------------------------------------------------

/// `api_key=`, `secret:`, `"token": `, … and whatever value follows.
#[inline]
pub(super) fn assignment_rule(bytes: &[u8], at: usize) -> Option<Outcome> {
    // Every byte of a name is a start now, since a keyword may end a name or follow a capital.
    // Most are not the first letter of anything, and are turned away on that alone.
    if !starts_a_keyword(bytes, at) {
        return None;
    }
    let keyword = match_keyword(bytes, at)
        .filter(|keyword| keyword_may_start(bytes, at, keyword.end))
        .or_else(|| match_query_name(bytes, at))?;
    // `AWS_SECRET_ACCESS_KEY=` is a keyword with the rest of a name after it. Requiring the
    // separator to follow the keyword itself would redact only the spellings that happen to
    // end on one, which is a minority of the names credentials actually have.
    let name_end = if keyword.whole {
        keyword.end
    } else {
        take_while(bytes, keyword.end, is_name_byte)
    };
    // `cookies: 3 stored` is a count; only a header named `cookie` has a line of values.
    let keyword = Keyword {
        kind: if keyword.kind == Kind::Line && name_end != keyword.end {
            Kind::Token
        } else {
            keyword.kind
        },
        ..keyword
    };
    let after_name = skip_quote(bytes, name_end);
    // `{"token": 12345}`: a bare value in JSON ends where its object does.
    let quoted_name = after_name != name_end;
    let separator = skip_spaces(bytes, after_name);
    let colon = bytes.get(separator) == Some(&b':');
    let after_separator =
        match_byte(bytes, separator, b'=').or_else(|| match_byte(bytes, separator, b':'))?;
    let value_start = skip_spaces(bytes, after_separator);
    if let Some(opened) = open_quote(bytes, value_start) {
        return quoted_value(bytes, &keyword, opened);
    }
    let in_query = {
        let name_start = run_start(bytes, at, is_name_byte);
        matches!(
            name_start
                .checked_sub(1)
                .and_then(|before| bytes.get(before)),
            Some(b'?' | b'&')
        )
    };
    let value_end = match (keyword.kind, colon && !in_query) {
        (Kind::Line, _) => line_end(bytes, value_start),
        (Kind::Phrase, true) => phrase_end(bytes, value_start),
        _ => take_while(bytes, value_start, |byte| {
            is_value_byte(byte)
                && !(in_query && matches!(byte, b'&' | b'#'))
                && !(quoted_name && matches!(byte, b'}' | b']'))
        }),
    };
    if value_end == value_start {
        return None;
    }
    if let Some(end) = status_phrase_end(bytes, value_start) {
        return Some(Outcome::Keep { end });
    }
    if keyword.token
        && counts_tokens(bytes, keyword.end)
        && only_digits(&bytes[value_start..value_end])
    {
        return Some(Outcome::Keep { end: value_end });
    }
    // A name that was quoted keeps its quote and its separator as they stood, so a JSON text
    // keeps its shape. A bare name has always been written back as `name=[REDACTED]`.
    let (kept_end, joiner) = match (after_name == name_end, keyword.kind) {
        (true, Kind::Line) => (value_start, ""),
        (true, _) => (name_end, "="),
        (false, _) => (value_start, ""),
    };
    Some(Outcome::Redact(Rewrite {
        end: value_end,
        kept_end,
        joiner,
    }))
}

/// A quote that opens a value at `at`: where its text starts, the quote and whether it is escaped.
struct Opened {
    inner: usize,
    quote: u8,
    escaped: bool,
}

fn open_quote(bytes: &[u8], at: usize) -> Option<Opened> {
    let escaped = bytes.get(at) == Some(&b'\\');
    let quote_at = if escaped { at + 1 } else { at };
    let quote = *bytes.get(quote_at).filter(|byte| is_quote(**byte))?;
    Some(Opened {
        inner: quote_at + 1,
        quote,
        escaped,
    })
}

/// Where the text inside a quote ends: at its closing quote, or at the end of the line when it is
/// never closed.
fn close_quote(bytes: &[u8], opened: &Opened) -> usize {
    let mut at = opened.inner;
    while let Some(byte) = bytes.get(at) {
        match *byte {
            b'\n' | b'\r' => return at,
            b'\\' if opened.escaped && bytes.get(at + 1) == Some(&opened.quote) => return at,
            b'\\' if !opened.escaped => at += 1,
            byte if byte == opened.quote && !opened.escaped => return at,
            _ => {}
        }
        at += 1;
    }
    bytes.len()
}

/// A value inside quotes runs to the closing quote whatever it holds, spaces and punctuation
/// included, and the quotes and the separator stand as they were.
fn quoted_value(bytes: &[u8], keyword: &Keyword, opened: Opened) -> Option<Outcome> {
    let end = close_quote(bytes, &opened);
    if end == opened.inner {
        return None;
    }
    if status_phrase_end(bytes, opened.inner).is_some_and(|status| status == end) {
        return Some(Outcome::Keep { end });
    }
    if keyword.token && counts_tokens(bytes, keyword.end) && only_digits(&bytes[opened.inner..end])
    {
        return Some(Outcome::Keep { end });
    }
    Some(Outcome::Redact(Rewrite {
        end,
        kept_end: opened.inner,
        joiner: "",
    }))
}

/// The end of the line from `start`, without the spaces that trail it.
fn line_end(bytes: &[u8], start: usize) -> usize {
    trim_end(
        bytes,
        start,
        take_while(bytes, start, |byte| (byte != b'\n') & (byte != b'\r')),
    )
}

/// The end of a phrase from `start`: the end of the line or the next comma or semicolon.
fn phrase_end(bytes: &[u8], start: usize) -> usize {
    let end = take_while(bytes, start, |byte| {
        (byte != b'\n') & (byte != b'\r') & (byte != b',') & (byte != b';')
    });
    trim_end(bytes, start, end)
}

fn trim_end(bytes: &[u8], start: usize, mut end: usize) -> usize {
    while end > start && matches!(bytes[end - 1], b' ' | b'\t') {
        end -= 1;
    }
    end
}

/// The next `=` or `:` and the places [`assignment_rule`] can start to reach it.
///
/// The rule wants a keyword, the rest of the name, an optional quote, any spaces, then the
/// separator. A keyword is made of name bytes, so the whole match up to the quote and the spaces
/// lies in the run of name bytes that ends where those begin, and it can start anywhere in that
/// run (a keyword may begin inside a word; the rule decides whether it may).
///
/// Every start in that run reaches the same separator and the same value: the rule reads on from
/// the keyword to the end of the run whichever start it came from. So whether a value follows is
/// decided here, once, and a separator with none allows no start. A rule that leaves a value alone
/// (`Outcome::Keep`) steps over the whole assignment, so a long name does not read its value again
/// for each of its starts. The one name that spans a space, `api key`, also allows the word before.
#[inline]
pub(super) fn assignment_separator(bytes: &[u8], from: usize) -> Option<Anchor> {
    let separator = first_match(bytes, from, |byte| (byte == b'=') | (byte == b':'))?;
    let name_end = before_quote(bytes, run_start(bytes, separator, is_space_byte));
    let (mut name_start, mut first) = name_run(bytes, name_end);
    if name_start == name_end || !has_value(bytes, separator + 1) {
        return Some(Anchor {
            at: separator,
            starts: 0..0,
        });
    }
    // `api key` and `private key`: the run is `key` and the word before it is the other half.
    if name_end - name_start == 3
        && bytes[name_start..name_end].eq_ignore_ascii_case(b"key")
        && name_start >= 2
        && bytes[name_start - 1] == b' '
    {
        name_start = run_start(bytes, name_start - 1, |byte| byte.is_ascii_alphabetic());
        if starts_a_keyword(bytes, name_start) {
            first = name_start;
        }
    }
    Some(Anchor {
        at: separator,
        starts: first..name_end,
    })
}

/// Where the run of name bytes that ends at `end` starts, and the first place in it that can hold
/// a keyword (`end` when none can).
///
/// Most names hold none (`status`, `dur_ms`, `request_id`), and asking the rule at each byte of
/// them costs more than reading the name once. A start left out is one that [`starts_a_keyword`]
/// turns away, so the rule would have answered `None` there. One pass, from the end: the run is
/// measured and each byte is tried as it is passed, so the last one kept is the leftmost.
fn name_run(bytes: &[u8], end: usize) -> (usize, usize) {
    let mut start = end.min(bytes.len());
    let mut first = start;
    while let Some(before) = start.checked_sub(1)
        && bytes.get(before).is_some_and(|byte| is_name_byte(*byte))
    {
        count_steps(1);
        start = before;
        if starts_a_keyword(bytes, start) {
            first = start;
        }
    }
    (start, first)
}

/// Whether a value starts after `from`, past any spaces: the check [`assignment_rule`] makes
/// before it redacts anything.
pub(super) fn has_value(bytes: &[u8], from: usize) -> bool {
    let value_start = skip_spaces(bytes, from);
    bytes
        .get(value_start)
        .is_some_and(|byte| is_value_byte(*byte))
}

// ---- the URL rule ---------------------------------------------------------------------------

/// The password half of `scheme://user:password@host`, or the whole userinfo when it has no
/// password but is itself a token (`https://ghp_0123456789abcdefghij@host`).
#[inline]
pub(super) fn url_password_rule(bytes: &[u8], at: usize) -> Option<Outcome> {
    if !starts_a_word(bytes, at) || !bytes.get(at)?.is_ascii_alphabetic() {
        return None;
    }
    let scheme_end = take_while(bytes, at + 1, is_scheme_byte);
    let after_scheme = match_word(bytes, scheme_end, SCHEME_SEPARATOR)?;
    let password = url_password(bytes, after_scheme)?;
    Some(Outcome::Redact(Rewrite {
        end: password.end,
        kept_end: password.start,
        joiner: "",
    }))
}

/// Where the password of the URL whose `://` ends at `after_scheme` starts and ends.
///
/// `None` unless the authority is `user:password@`, the user optional and the password not: a
/// URL that has no password, or whose userinfo is never closed by an `@`, is left as it is. The
/// exception is a userinfo with no colon that is itself a token, which is long, has a digit and is
/// made of the characters a token is made of: `git@github.com` is a name, `ghp_0123...@host` is not.
pub(super) fn url_password(bytes: &[u8], after_scheme: usize) -> Option<Range<usize>> {
    let user_end = take_while(bytes, after_scheme, |byte| {
        !matches!(
            byte,
            b' ' | b'\t' | b'\n' | b'\r' | b':' | b'/' | b'?' | b'#'
        )
    });
    // The user half is optional: `redis://:hunter2@db/main` is what a URL looks like when
    // only a password was configured.
    let Some(password_start) = match_byte(bytes, user_end, b':') else {
        return token_userinfo(bytes, after_scheme);
    };
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

/// A userinfo with no colon, of 20 or more token characters with a digit among them, closed by `@`.
fn token_userinfo(bytes: &[u8], start: usize) -> Option<Range<usize>> {
    let end = take_while(bytes, start, |byte| {
        byte.is_ascii_alphanumeric() | matches!(byte, b'_' | b'-' | b'.' | b'~' | b'%')
    });
    let token_like = end - start >= 20 && bytes[start..end].iter().any(u8::is_ascii_digit);
    (token_like && bytes.get(end) == Some(&b'@')).then_some(start..end)
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
#[inline]
pub(super) fn scheme_separator(bytes: &[u8], from: usize) -> Option<Anchor> {
    let colon = first_match(bytes, from, |byte| byte == b':')?;
    let starts = match match_word(bytes, colon, SCHEME_SEPARATOR) {
        Some(after_scheme) if url_password(bytes, after_scheme).is_some() => {
            run_start(bytes, colon, is_scheme_byte)..colon
        }
        _ => 0..0,
    };
    Some(Anchor { at: colon, starts })
}
