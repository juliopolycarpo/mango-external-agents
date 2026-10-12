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
    count_steps(1);
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
    /// Whether it is a key by its shape (`api_key`, `x-functions-key`), which a status may follow
    /// (`OPENAI_API_KEY: not set`) without another credential word in the name vetoing it.
    key_shape: bool,
}

/// `api`, `private`, `secret` or `access`, an optional separator, a dot or spaces, then `key`.
fn match_key_shape(bytes: &[u8], at: usize) -> Option<usize> {
    let after_word = ["api", "private", "secret", "access"]
        .iter()
        .find_map(|word| match_word(bytes, at, word.as_bytes()))?;
    let after_separator = match bytes.get(after_word) {
        Some(b'_' | b'-' | b'.') => after_word + 1,
        Some(b' ' | b'\t') => take_while(bytes, after_word, is_blank_byte),
        _ => after_word,
    };
    match_word(bytes, after_separator, b"key")
}

/// A space or a tab: the gap that may stand inside a name such as `api key`.
fn is_blank_byte(byte: u8) -> bool {
    (byte == b' ') | (byte == b'\t')
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
            key_shape: true,
        });
    }
    if first == b'k'
        && let Some(end) = match_header_key(bytes, at)
    {
        return Some(Keyword {
            end,
            kind: Kind::Token,
            whole: false,
            token: false,
            key_shape: true,
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
                key_shape: false,
            });
        }
    }
    None
}

/// Whether a query parameter's name starts at `at`: right after a `?` or a `&`.
fn starts_a_parameter(bytes: &[u8], at: usize) -> bool {
    matches!(
        at.checked_sub(1).and_then(|before| bytes.get(before)),
        Some(b'?' | b'&')
    )
}

/// Vendor header names that are a key without a credential word in them, each read at its last
/// word so that no new letter opens a start: `x-functions-key` (Azure Functions) and
/// `Ocp-Apim-Subscription-Key` (Azure API Management).
const KEY_HEADER_PREFIXES: [&str; 2] = ["x-functions-", "subscription-"];

/// The end of the `key` at `at` when a header name above leads up to it.
fn match_header_key(bytes: &[u8], at: usize) -> Option<usize> {
    let end = match_word(bytes, at, b"key")?;
    // The name ends there: `x-functions-keyboard` is not the header.
    if bytes.get(end).copied().is_some_and(is_name_byte) {
        return None;
    }
    let before = &bytes[..at];
    KEY_HEADER_PREFIXES
        .iter()
        .any(|prefix| {
            before.len() >= prefix.len()
                && before[before.len() - prefix.len()..].eq_ignore_ascii_case(prefix.as_bytes())
        })
        .then_some(end)
}

/// What the rewrite pass has read of the run of non-blank text it is in: whether a `?` stands in it
/// before the place read up to. A `&` joins two parameters of a query only after a `?` in the
/// same run (`cmd &password=pa&ss` is a shell's ampersand, and its value runs to the next blank).
///
/// The pass asks at starts that only move forward, so the text is read once, from where the last
/// answer stood to where the next is asked, and a `?` at any distance in the run counts. A caller
/// that asks from an earlier place starts over from the beginning.
#[derive(Default)]
pub(super) struct QueryRun {
    /// The text before this place has been read.
    read_to: usize,
    /// Whether the last `?` or blank read was a `?`.
    question_mark: bool,
}

impl QueryRun {
    /// Whether the parameter whose name starts at `start`, asked about from `at` (`start` or a
    /// later place in the same name), is in a query string: right after a `?`, or after a `&`
    /// with a `?` earlier in the same run of non-blank bytes.
    pub(super) fn is_query_parameter(&mut self, bytes: &[u8], start: usize, at: usize) -> bool {
        match start.checked_sub(1).and_then(|before| bytes.get(before)) {
            Some(b'?') => true,
            Some(b'&') => self.has_a_question_mark_before(bytes, at),
            _ => false,
        }
    }

    fn has_a_question_mark_before(&mut self, bytes: &[u8], at: usize) -> bool {
        if at < self.read_to {
            *self = Self::default();
        }
        let unread = &bytes[self.read_to..at];
        count_steps(unread.len());
        if let Some(last) = last_question_mark_or_blank(unread) {
            self.question_mark = last == b'?';
        }
        self.read_to = at;
        self.question_mark
    }
}

/// The last `?` or blank in `window`, whichever it is, read a block at a time so that text with
/// neither costs a vector compare for each block and not a branch for each byte.
pub(super) fn last_question_mark_or_blank(window: &[u8]) -> Option<u8> {
    for block in window.rchunks(32) {
        let holds = block.iter().fold(false, |any, byte| {
            any | (*byte == b'?') | is_space_byte(*byte)
        });
        if holds {
            return block
                .iter()
                .rev()
                .find(|byte| (**byte == b'?') | is_space_byte(**byte))
                .copied();
        }
    }
    None
}

/// A query parameter that is a credential's only when it is the whole name (`key`, `auth`,
/// `sig`), or when it ends in `signature` or `sig` (`X-Amz-Signature`). Whether the parameter is in
/// a query is the caller's to ask ([`QueryRun`]).
fn match_query_name(bytes: &[u8], at: usize) -> Option<Keyword> {
    if !starts_a_parameter(bytes, at) {
        return None;
    }
    let end = take_while(bytes, at, |byte| {
        byte.is_ascii_alphanumeric() | (byte == b'-') | (byte == b'_')
    });
    let name = &bytes[at..end];
    let whole = std::iter::once(&"key")
        .chain(CREDENTIAL_NAMES.iter())
        .any(|word| name.eq_ignore_ascii_case(word.as_bytes()));
    let last = name
        .rsplit(|byte| (*byte == b'-') | (*byte == b'_'))
        .next()
        .unwrap_or(name);
    let signed = last.eq_ignore_ascii_case(b"signature") || last.eq_ignore_ascii_case(b"sig");
    (whole || signed).then_some(Keyword {
        end,
        kind: Kind::Token,
        whole: true,
        token: false,
        key_shape: false,
    })
}

/// Whether some keyword starts at `at`, for a caller that only asks whether a text names a
/// credential at all.
pub(super) fn names_a_keyword(bytes: &[u8], at: usize) -> bool {
    if starts_a_parameter(bytes, at) && match_query_name(bytes, at).is_some() {
        return true;
    }
    starts_a_keyword(bytes, at) && match_keyword(bytes, at).is_some()
}

/// Whether the two bytes at `at` open some keyword, of `key` or of `signature`: `se`cret,
/// `si`g, `to`ken, `pa`ssword, `pw`d, `pr`ivate, `cr`edential, `co`okie, `ap`i, `au`th, `ac`cess,
/// `ke`y.
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
        b'a' => (second == b'p') | (second == b'u') | (second == b'c'),
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

/// Words that begin with a keyword and are not credentials' names: `tokenizer`, `secretary`.
const PLAIN_WORDS: [&str; 11] = [
    "tokenizer",
    "tokenizers",
    "tokenization",
    "tokenize",
    "tokenized",
    "tokenise",
    "tokenised",
    "secretary",
    "secretariat",
    "secretaries",
    "passwordless",
];

/// Whether the word that starts at `at` and runs longer than its keyword is one of [`PLAIN_WORDS`].
fn is_a_plain_word(bytes: &[u8], at: usize, keyword_end: usize) -> bool {
    let end = take_while(bytes, at, |byte| byte.is_ascii_alphabetic());
    // A word of 12 letters at most; a longer run cannot be one of them, and is not copied.
    if end == keyword_end || end - at > 12 {
        return false;
    }
    let word = &bytes[at..end];
    PLAIN_WORDS
        .iter()
        .any(|plain| word.eq_ignore_ascii_case(plain.as_bytes()))
}

/// Whether the name that runs from `from` to `name_end` ends in a cookie keyword that starts
/// within [`NEARBY`] bytes: `session_token_cookie`.
fn ends_in_cookie(bytes: &[u8], from: usize, name_end: usize) -> bool {
    if name_end <= from {
        return false;
    }
    (from..name_end.min(from + NEARBY)).any(|place| {
        starts_a_keyword(bytes, place)
            && match_keyword(bytes, place).is_some_and(|other| {
                other.kind == Kind::Line
                    && other.end == name_end
                    && keyword_may_start(bytes, place, other.end)
            })
    })
}

/// How far from a keyword another is looked for.
const NEARBY: usize = 64;

/// Whether the name around the keyword at `at` holds another credential word, which vetoes a keep:
/// `session_token_cookie: expired` names two credentials, and a status that suits one does not
/// excuse the other. The same word again and a key by its shape (`OPENAI_API_KEY`) do not count.
/// Reads at most [`NEARBY`] bytes each way.
fn another_keyword_nearby(bytes: &[u8], at: usize, keyword: &Keyword) -> bool {
    let own = &bytes[at..keyword.end];
    let mut low = at;
    while low > 0 && at - low < NEARBY && is_name_byte(bytes[low - 1]) {
        count_steps(1);
        low -= 1;
    }
    let mut high = keyword.end;
    while high < bytes.len() && high - keyword.end < NEARBY && is_name_byte(bytes[high]) {
        count_steps(1);
        high += 1;
    }
    (low..high)
        .filter(|place| *place < at || *place >= keyword.end)
        .any(|place| {
            starts_a_keyword(bytes, place)
                && match_keyword(bytes, place).is_some_and(|other| {
                    !other.key_shape
                        && keyword_may_start(bytes, place, other.end)
                        && !bytes[place..other.end].eq_ignore_ascii_case(own)
                })
        })
}

/// Whether the rest of a name after `token` says it counts tokens: `tokens`, `token_limit`,
/// `tokenCount`. A number under such a name is a count and not a credential.
fn counts_tokens(bytes: &[u8], after_keyword: usize) -> bool {
    // `auth_tokens_secret` is a secret's name with tokens in it: nothing else may follow.
    let name_end = take_while(bytes, after_keyword, is_name_byte);
    if (after_keyword..name_end)
        .any(|at| starts_a_keyword(bytes, at) && match_keyword(bytes, at).is_some())
    {
        return false;
    }
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

/// How a word counts toward a status.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
    /// Not a status word: the text is a value.
    Value,
    /// Says a credential is absent or refused: `not`, `set`, `missing`, `expired`, `invalid`.
    State,
    /// Part of an HTTP reason phrase (`too many requests`, `internal server error`): a status only
    /// after the number that goes with it.
    Reason,
    /// An HTTP error status, `401` or `503`.
    Number,
}

/// What a word says. Every word this list does not know is a value, so in doubt the text is
/// redacted.
fn status_word(word: &[u8]) -> Word {
    if (word.len() > LONGEST_STATUS_WORD) | word.is_empty() {
        return Word::Value;
    }
    let mut lower = [0u8; LONGEST_STATUS_WORD];
    for (to, from) in lower.iter_mut().zip(word) {
        *to = from.to_ascii_lowercase();
    }
    let word = &lower[..word.len()];
    if word.len() == 3 && matches!(word[0], b'4' | b'5') && word.iter().all(u8::is_ascii_digit) {
        return Word::Number;
    }
    match word {
        b"not" | b"set" | b"found" | b"provided" | b"configured" | b"missing" | b"expired"
        | b"invalid" | b"required" | b"revoked" | b"unauthorized" | b"forbidden" | b"unset"
        | b"empty" | b"unknown" | b"none" | b"null" | b"denied" | b"rejected" | b"failed"
        | b"refused" | b"absent" | b"undefined" | b"unavailable" => Word::State,
        b"bad" | b"request" | b"requests" | b"too" | b"many" | b"internal" | b"server"
        | b"error" | b"service" | b"timeout" => Word::Reason,
        _ => Word::Value,
    }
}

/// Where the value at `start` ends, when it is nothing but status words: `not set`, `missing`,
/// `401 Unauthorized`, closed by the end of the text, white space, or a few marks of punctuation
/// and then one of those. Anything else after the words, such as another word, a digit, an
/// underscore, or a mark with text glued to it (`set.hunter2`), makes it a value. The words of an
/// HTTP reason phrase count only after the number.
///
/// Reads a handful of words at most, so it costs the same however much text follows.
fn status_phrase_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut at = start;
    let mut words = 0;
    let mut reason = false;
    let mut numbered = false;
    let end = loop {
        let word_end = take_while(bytes, at, |byte| byte.is_ascii_alphanumeric());
        match status_word(&bytes[at..word_end]) {
            Word::Value => return None,
            Word::Reason => reason = true,
            Word::Number if words > 0 => return None,
            Word::Number => numbered = true,
            Word::State => {}
        }
        words += 1;
        let next = take_while(bytes, word_end, is_blank_byte);
        // Another word follows only if there are few enough of them to be all status words.
        if next == word_end || !bytes.get(next).is_some_and(u8::is_ascii_alphanumeric) {
            break word_end;
        }
        if words == 6 {
            return None;
        }
        at = next;
    };
    if reason && !numbered {
        return None;
    }
    // Up to two marks of punctuation, then the text must end or a gap begin.
    let marks = bytes[end..]
        .iter()
        .take(3)
        .take_while(|byte| matches!(byte, b'.' | b',' | b';' | b')' | b']' | b'}' | b'"' | b'\''))
        .count();
    let delimited = bytes
        .get(end + marks)
        .is_none_or(|byte| is_space_byte(*byte));
    (delimited && marks < 3).then_some(end)
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
/// Any scheme counts, not only `bearer` and `basic`. A scheme that takes one token (`bearer`,
/// `basic`, `token`, ...) has that token redacted. One that takes several (`Digest`, `AWS4-HMAC-
/// SHA256`) has the rest of its line redacted. One this list does not know is as likely to be a
/// credential as a scheme, so everything after the colon is, to the end of the line or a quote:
/// `Authorization: sk-live-1 was rejected` leaves no key behind. A value in quotes ends at its
/// closing quote.
///
/// Words that only say a credential is missing (`Authorization: required`, `Authorization: 401
/// Unauthorized`) are kept.
#[inline]
pub(super) fn bearer_rule(bytes: &[u8], at: usize) -> Option<Outcome> {
    read_header(bytes, at, b':')
}

/// [`bearer_rule`] for the separator `separator`: a colon, or the equals sign that the assignment
/// rule hands over (`Authorization=Bearer x`).
fn read_header(bytes: &[u8], at: usize, separator: u8) -> Option<Outcome> {
    if !(starts_a_word(bytes, at) || is_camel_boundary(bytes, at)) {
        return None;
    }
    let after_keyword = match_word(bytes, at, AUTHORIZATION)?;
    let after_quote = skip_quote(bytes, after_keyword);
    let after_colon = match_byte(bytes, skip_spaces(bytes, after_quote), separator)?;
    let value = skip_spaces(bytes, after_colon);
    let scheme_start = skip_quote(bytes, value);
    let quoted = scheme_start != value;
    // The bounded read first: what follows may be a megabyte, and a status is a few words.
    if !quoted && let Some(end) = status_phrase_end(bytes, scheme_start) {
        return Some(Outcome::Keep { end });
    }
    let after_scheme = take_while(bytes, scheme_start, |byte| {
        byte.is_ascii_alphanumeric() | (byte == b'-') | (byte == b'_') | (byte == b'.')
    });
    let scheme = &bytes[scheme_start..after_scheme];
    // A boundary marker stands where a byte was removed between the scheme and its token.
    let token_start = take_while(bytes, after_scheme, |byte| {
        is_space_byte(byte) || byte == strip::BOUNDARY_BYTE
    });
    let gap = token_start > after_scheme;
    // An unknown scheme takes the rest of its line. When the header's name was quoted the line is a
    // JSON text, and a quote or a bracket ends it too.
    let json = after_quote != after_keyword;
    // After an equals sign `authorization` is a variable (`authorization=true mode=strict`), and a
    // scheme this does not know is one token of the line's several; after a colon it is a header
    // line, and the rest of it is the credential.
    let one_token = separator == b'=';
    let unknown_end = || {
        let end = take_while(bytes, scheme_start, |byte| {
            let in_line = if one_token {
                is_value_byte(byte)
            } else {
                !matches!(byte, b'\n' | b'\r')
            };
            in_line && !(json && matches!(byte, b'"' | b'\'' | b'}' | b']'))
        });
        trim_end(bytes, scheme_start, end)
    };
    let (end, takes_a_token) = if quoted {
        let opened = opened_before(bytes, scheme_start)?;
        let end = close_quote(bytes, &opened);
        if status_phrase_end(bytes, scheme_start).is_some_and(|status| status == end) {
            return Some(Outcome::Keep { end });
        }
        (end, gap && token_start < end && is_known_scheme(scheme))
    } else if scheme.is_empty() {
        // A bare credential that opens with another byte (`+Zx9/abc==`), but not an escape's
        // backslash or a quote or bracket: those are the text around a value, not a value.
        let opens = bytes.get(scheme_start).copied();
        if !opens.is_some_and(|byte| {
            is_value_byte(byte) && !matches!(byte, b'\\' | b'"' | b'\'' | b'[' | b']' | b'{' | b'}')
        }) {
            return None;
        }
        (unknown_end(), false)
    } else if is_known_scheme(scheme) {
        if !gap {
            // Nothing follows the scheme, or a byte is glued to it (`Bearer+abc`): the second is
            // a credential, as an unknown scheme is.
            if !bytes
                .get(after_scheme)
                .is_some_and(|byte| is_value_byte(*byte))
            {
                return None;
            }
            (unknown_end(), false)
        } else if is_single_token_scheme(scheme) {
            (take_while(bytes, token_start, is_value_byte), true)
        } else {
            (line_end(bytes, token_start), true)
        }
    } else {
        (unknown_end(), false)
    };
    // A scheme with nothing after it carries no credential.
    if end <= scheme_start || (takes_a_token && end <= token_start) {
        return None;
    }
    let (kept_end, joiner) = if takes_a_token {
        (after_scheme, " ")
    } else {
        (scheme_start, "")
    };
    Some(Outcome::Redact(Rewrite {
        end,
        kept_end,
        joiner,
        tail: "",
    }))
}

/// The quote that opens the value whose text starts at `inner`.
fn opened_before(bytes: &[u8], inner: usize) -> Option<Opened> {
    let quote = *bytes.get(inner.checked_sub(1)?)?;
    is_quote(quote).then_some(Opened {
        inner,
        quote,
        escaped: inner >= 2 && bytes[inner - 2] == b'\\',
    })
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

/// Whether an authorization scheme carries several parts, each of which is a credential.
fn is_multi_part_scheme(scheme: &[u8]) -> bool {
    [
        &b"digest"[..],
        b"aws4-hmac-sha256",
        b"oauth",
        b"hmac",
        b"hawk",
        b"mac",
        b"signature",
        b"vapid",
    ]
    .iter()
    .any(|multi| scheme.eq_ignore_ascii_case(multi))
}

/// Whether a scheme is one of those above.
fn is_known_scheme(scheme: &[u8]) -> bool {
    is_single_token_scheme(scheme) || is_multi_part_scheme(scheme)
}

/// The next `:` and the one place [`bearer_rule`] can start to reach it.
///
/// The rule wants `authorization`, an optional quote, any spaces, then the colon. (With an equals
/// sign the header is found by the assignment locator, which already looks at every `=`: a second
/// scan for it here made the scan of every tail dearer; see [`assignment_rule`].) The name ends in
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
///
/// Every start in a name reaches the same separator and the same value, so a rule that decides
/// about the value alone (an empty one, a status, a count) answers [`Outcome::Keep`] past the
/// separator, and the other starts of the name are not asked. The bounded reads come first: the
/// value may be a megabyte of text and a status is a few words.
///
/// A name that holds more than one keyword is answered for by the leftmost that may start there,
/// so a keep for it (`session_token_cookie: ;a=b`, a name that holds both a token and a cookie)
/// also skips the later ones. Such names are not ones a vendor writes.
#[inline]
pub(super) fn assignment_rule(bytes: &[u8], at: usize, query: &mut QueryRun) -> Option<Outcome> {
    // Every byte of a name is a start now, since a keyword may end a name or follow a capital.
    // Most are not the first letter of anything, and are turned away on that alone.
    if !(starts_a_keyword(bytes, at) || starts_a_parameter(bytes, at)) {
        return None;
    }
    // `Authorization=Bearer x`: the bearer rule's header, written with an equals sign.
    if bytes[at] | 0x20 == b'a'
        && let Some(after_name) = match_word(bytes, at, AUTHORIZATION)
        && starts_a_word(bytes, at)
        && bytes.get(skip_spaces(bytes, skip_quote(bytes, after_name))) == Some(&b'=')
    {
        return read_header(bytes, at, b'=');
    }
    let keyword = match_keyword(bytes, at)
        .filter(|keyword| keyword_may_start(bytes, at, keyword.end))
        .or_else(|| {
            match_query_name(bytes, at).filter(|_| query.is_query_parameter(bytes, at, at))
        })?;
    if !keyword.whole && is_a_plain_word(bytes, at, keyword.end) {
        return None;
    }
    // `AWS_SECRET_ACCESS_KEY=` is a keyword with the rest of a name after it. Requiring the
    // separator to follow the keyword itself would redact only the spellings that happen to
    // end on one, which is a minority of the names credentials actually have.
    let name_end = if keyword.whole {
        keyword.end
    } else {
        take_while(bytes, keyword.end, is_name_byte)
    };
    // `cookies: 3 stored` is a count; only a header named `cookie` has a line of values. And a name
    // that ends in `cookie` is a cookie header whatever opens it (`session_token_cookie`), so its
    // line is redacted from whichever of its starts is asked first.
    let after_name = skip_quote(bytes, name_end);
    // `{"token": 12345}`: a bare value in JSON ends where its object does.
    let quoted_name = after_name != name_end;
    // A cookie header's line is its value; a quoted member named for a cookie
    // (`{"accept_cookie": true, "path": "/"}`) has the value of any other member.
    let kind = match keyword.kind {
        _ if quoted_name => Kind::Token,
        Kind::Line if name_end != keyword.end => Kind::Token,
        Kind::Line => Kind::Line,
        _ if ends_in_cookie(bytes, keyword.end, name_end) => Kind::Line,
        kind => kind,
    };
    let keyword = Keyword { kind, ..keyword };
    let separator = skip_spaces(bytes, after_name);
    let colon = bytes.get(separator) == Some(&b':');
    let after_separator =
        match_byte(bytes, separator, b'=').or_else(|| match_byte(bytes, separator, b':'))?;
    let value_start = skip_spaces(bytes, after_separator);
    let mut context = Context {
        query,
        start: run_start(bytes, at, is_name_byte),
        at,
        colon,
        quoted_name,
        in_query: None,
    };
    if let Some(opened) = open_quote(bytes, value_start) {
        return quoted_value(bytes, &keyword, at, opened, after_separator, &mut context);
    }
    // A `&` ends a value only in a query string, and a query string separates with `=`.
    let in_query = context.in_query(bytes);
    let vetoed = || another_keyword_nearby(bytes, at, &keyword);
    let ends_value = |byte: u8| {
        !is_value_byte(byte)
            || (in_query && matches!(byte, b'&' | b'#'))
            || (quoted_name && matches!(byte, b'}' | b']'))
    };
    let phrase = keyword.kind == Kind::Phrase && colon && !in_query;
    if let Some(end) = status_phrase_end(bytes, value_start)
        && (!phrase || phrase_closes(bytes, end))
        && !vetoed()
    {
        return Some(Outcome::Keep { end });
    }
    let digits_end = take_while(bytes, value_start, is_number_byte);
    if keyword.token
        && digits_end > value_start
        && bytes.get(digits_end).is_none_or(|byte| ends_value(*byte))
        && counts_tokens(bytes, keyword.end)
        && !vetoed()
    {
        return Some(Outcome::Keep { end: digits_end });
    }
    if is_a_path_for_pwd(bytes, at, &keyword, value_start) && !vetoed() {
        return Some(Outcome::Keep { end: value_start });
    }
    // `token_type: Bearer` names a scheme; `token_type: Bearer eyJ...` has the credential after it,
    // and the credential is the whole line.
    let with_a_token = match token_type_scheme(bytes, &keyword, value_start) {
        Some(scheme_end) => {
            let after = take_while(bytes, scheme_end, is_blank_byte);
            let alone = bytes
                .get(after)
                .is_none_or(|byte| matches!(byte, b'\n' | b'\r') || ends_value(*byte));
            if alone && !vetoed() {
                return Some(Outcome::Keep { end: scheme_end });
            }
            true
        }
        None => false,
    };
    let value_end = match (keyword.kind, bytes.get(value_start)) {
        _ if with_a_token => line_end(bytes, value_start),
        (Kind::Line, _) => line_end(bytes, value_start),
        // The brackets close the value, and whatever is glued after them goes with it.
        (_, Some(b'[' | b'{')) => take_while(bytes, closing_bracket(bytes, value_start), |byte| {
            !ends_value(byte)
        }),
        (Kind::Phrase, _) if phrase => phrase_end(bytes, value_start),
        _ => take_while(bytes, value_start, |byte| !ends_value(byte)),
    };
    if value_end == value_start {
        return Some(Outcome::Keep {
            end: after_separator,
        });
    }
    // A name that was quoted keeps its quote and its separator as they stood, so a JSON text
    // keeps its shape. A bare name has always been written back as `name=[REDACTED]`.
    let (kept_end, joiner) = match (quoted_name, keyword.kind) {
        (false, Kind::Line) => (value_start, ""),
        (false, _) => (name_end, "="),
        (true, _) => (value_start, ""),
    };
    Some(Outcome::Redact(Rewrite {
        end: value_end,
        kept_end,
        joiner,
        tail: "",
    }))
}

/// Whether nothing is left of a phrase after the status that ends at `end`: the text ends, or a
/// line break, comma or semicolon comes next, after its marks and blanks. `password: not set (was
/// hunter2)` has more phrase after its status, and that is the password.
fn phrase_closes(bytes: &[u8], end: usize) -> bool {
    let marks = take_while(bytes, end, |byte| {
        matches!(byte, b'.' | b')' | b']' | b'}' | b'"' | b'\'')
    });
    let next = take_while(bytes, marks, is_blank_byte);
    bytes
        .get(next)
        .is_none_or(|byte| matches!(byte, b'\n' | b'\r' | b',' | b';'))
}

/// A digit or one of the marks a number is written with.
fn is_number_byte(byte: u8) -> bool {
    byte.is_ascii_digit() | (byte == b',') | (byte == b'_')
}

/// `PWD` and `OLDPWD` are the shell's names for the working directory: `PWD=/home/u/project`. Any
/// other name that ends in `pwd` (`MYSQL_PWD`) is a password.
fn is_a_path_for_pwd(bytes: &[u8], at: usize, keyword: &Keyword, value_start: usize) -> bool {
    if keyword.end - at != 3 || bytes.get(value_start) != Some(&b'/') {
        return false;
    }
    let name_start = run_start(bytes, at, is_name_byte);
    let name_end = take_while(bytes, keyword.end, is_name_byte);
    matches!(&bytes[name_start..name_end], b"PWD" | b"OLDPWD")
}

/// Where the scheme ends, when the name is `token_type` and the value starts with `Bearer`, `Basic`
/// or `MAC`: that names a scheme and not a credential, unless something follows it.
fn token_type_scheme(bytes: &[u8], keyword: &Keyword, value_start: usize) -> Option<usize> {
    if !keyword.token {
        return None;
    }
    let rest = &bytes[keyword.end..];
    let kind = rest
        .strip_prefix(b"_")
        .or(rest.strip_prefix(b"-"))
        .unwrap_or(rest);
    if kind.len() < 4 || !kind[..4].eq_ignore_ascii_case(b"type") {
        return None;
    }
    if kind.get(4).is_some_and(|byte| is_name_byte(*byte)) {
        return None;
    }
    let value_end = take_while(bytes, value_start, |byte| byte.is_ascii_alphabetic());
    let value = &bytes[value_start..value_end];
    let scheme = ["bearer", "basic", "mac"]
        .iter()
        .any(|scheme| value.eq_ignore_ascii_case(scheme.as_bytes()));
    (scheme && bytes.get(value_end).is_none_or(|byte| !is_name_byte(*byte))).then_some(value_end)
}

/// Where the bracket that opens at `start` closes, counting the brackets and the strings inside:
/// a JSON array or object that is one credential's value. A string opens with a quote, or with a
/// backslash and a quote when the JSON is itself inside a string (`{\\"a\\":1}`). An unclosed one
/// ends with its line.
fn closing_bracket(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0usize;
    // The quote that opened the string we are in, and whether it was written with a backslash.
    let mut string: Option<(u8, bool)> = None;
    let mut at = start;
    while let Some(&byte) = bytes.get(at) {
        count_steps(1);
        at += 1;
        if matches!(byte, b'\n' | b'\r') {
            return at - 1;
        }
        match (string, byte) {
            (Some((quote, true)), b'\\') if bytes.get(at) == Some(&quote) => {
                string = None;
                at += 1;
            }
            (Some((_, false)), b'\\') if !matches!(bytes.get(at), Some(b'\n' | b'\r')) => at += 1,
            (Some((quote, false)), byte) if byte == quote => string = None,
            (Some(_), _) => {}
            (None, b'\\') if bytes.get(at).is_some_and(|byte| is_quote(*byte)) => {
                string = Some((bytes[at], true));
                at += 1;
            }
            (None, b'"' | b'\'') => string = Some((byte, false)),
            (None, b'[' | b'{') => depth += 1,
            (None, b']' | b'}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return at;
                }
            }
            (None, _) => {}
        }
    }
    bytes.len()
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
        count_steps(1);
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

/// What the assignment rule knows about the name it is reading, for the value rules that need it.
struct Context<'a> {
    query: &'a mut QueryRun,
    /// Where the name starts, and the place the rule was asked about.
    start: usize,
    at: usize,
    colon: bool,
    /// Whether the name was in quotes, which makes the text a JSON member.
    quoted_name: bool,
    in_query: Option<bool>,
}

impl Context<'_> {
    /// Whether the name is a query parameter. A `&` ends a value only there, and a query string
    /// separates with `=`. Asked once.
    fn in_query(&mut self, bytes: &[u8]) -> bool {
        if let Some(known) = self.in_query {
            return known;
        }
        let known = !self.colon && self.query.is_query_parameter(bytes, self.start, self.at);
        self.in_query = Some(known);
        known
    }
}

/// A value inside quotes runs to the closing quote whatever it holds, spaces and punctuation
/// included, and the quotes and the separator stand as they were.
fn quoted_value(
    bytes: &[u8],
    keyword: &Keyword,
    at: usize,
    opened: Opened,
    after_separator: usize,
    context: &mut Context,
) -> Option<Outcome> {
    let end = close_quote(bytes, &opened);
    // Text glued to the closing quote (`"pa"ss"word"`) is more of the value, not the end of it:
    // the value is redacted through the glued tail, and no keep below applies.
    if let Some(tail_end) = glued_tail(bytes, &opened, end, context) {
        // The closing quote goes back after the marker, so that the text still reads as a quoted
        // value and a second pass over it leaves it as it is.
        let tail = match (opened.quote, opened.escaped) {
            (b'"', false) => "\"",
            (b'"', true) => "\\\"",
            (_, false) => "'",
            (_, true) => "\\'",
        };
        return Some(Outcome::Redact(Rewrite {
            end: tail_end,
            kept_end: opened.inner,
            joiner: "",
            tail,
        }));
    }
    if end == opened.inner {
        return Some(Outcome::Keep {
            end: after_separator,
        });
    }
    let vetoed = || another_keyword_nearby(bytes, at, keyword);
    if status_phrase_end(bytes, opened.inner).is_some_and(|status| status == end) && !vetoed() {
        return Some(Outcome::Keep { end });
    }
    // `"token_type": "Bearer"`; `"Bearer eyJ..."` is a credential and is redacted whole.
    if token_type_scheme(bytes, keyword, opened.inner) == Some(end) && !vetoed() {
        return Some(Outcome::Keep { end });
    }
    if keyword.token
        && counts_tokens(bytes, keyword.end)
        && only_digits(&bytes[opened.inner..end])
        && !vetoed()
    {
        return Some(Outcome::Keep { end });
    }
    Some(Outcome::Redact(Rewrite {
        end,
        kept_end: opened.inner,
        joiner: "",
        tail: "",
    }))
}

/// Where the value ends when text is glued to the quote that closes it: the closing quote at `end`
/// (a backslash and a quote when the value was written escaped), then text that goes on with the
/// value instead of ending it.
///
/// After a JSON member's name (a quoted name) a letter, a digit, another quote or an operator
/// glues, and a `}` or `]` ends the value. After a bare name the value is the run to the next
/// blank, comma or semicolon, as it always was, so it is glued when a word follows even behind
/// marks (`"x".C`, `"x")C`, `"a"b}c"`) and the brackets belong to it; marks alone (`"x").`) end
/// it. In a query a `&` or `#` ends the value in both.
fn glued_tail(bytes: &[u8], opened: &Opened, end: usize, context: &mut Context) -> Option<usize> {
    if !bytes
        .get(end)
        .is_some_and(|byte| *byte == opened.quote || *byte == b'\\')
    {
        return None;
    }
    let after = end + if opened.escaped { 2 } else { 1 };
    let first = *bytes.get(after)?;
    let glued = is_word_or_quote(first)
        || is_glue_operator(first)
        || (!context.quoted_name && marks_then_a_word(bytes, after, context));
    if !glued {
        return None;
    }
    let mut run_end = after;
    while let Some(&byte) = bytes.get(run_end) {
        let ends = !is_value_byte(byte)
            || (context.quoted_name && matches!(byte, b'}' | b']'))
            || (matches!(byte, b'&' | b'#') && context.in_query(bytes));
        if ends {
            break;
        }
        count_steps(1);
        run_end += 1;
    }
    Some(run_end)
}

/// Whether a byte is part of a word or a quote, which marks before it do not end a value for.
fn is_word_or_quote(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte >= 0x80 || is_quote(byte)
}

/// An operator a value may go on with, glued to a closing quote.
fn is_glue_operator(byte: u8) -> bool {
    matches!(
        byte,
        b'_' | b'-'
            | b'+'
            | b'/'
            | b'='
            | b'@'
            | b'!'
            | b'#'
            | b'$'
            | b'%'
            | b'^'
            | b'*'
            | b'~'
            | b'|'
            | b'?'
            | b'\\'
    )
}

/// Whether the marks that begin at `from` are followed by a word before a blank, a comma or a
/// semicolon: `.C`, `)C`, `}c"`. A `&` or `#` in a query ends it.
fn marks_then_a_word(bytes: &[u8], from: usize, context: &mut Context) -> bool {
    let mut at = from;
    while let Some(&byte) = bytes.get(at) {
        if !is_value_byte(byte) || (matches!(byte, b'&' | b'#') && context.in_query(bytes)) {
            return false;
        }
        if is_word_or_quote(byte) {
            return true;
        }
        count_steps(1);
        at += 1;
    }
    false
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
        count_steps(1);
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
    // The value is looked at first: a separator with none allows no start, and a long name before
    // one is then not read at all.
    if !has_value(bytes, separator + 1) {
        return Some(Anchor {
            at: separator,
            starts: 0..0,
        });
    }
    let (name_start, mut first) = name_run(bytes, name_end);
    if name_start == name_end {
        return Some(Anchor {
            at: separator,
            starts: 0..0,
        });
    }
    // `api key`, `private key` and `secret  key`: the run is `key`, and the other half is the word
    // before the blanks. A keyword is at most seven letters long, so only the last eight bytes
    // before the blanks are starts: the word may be a megabyte long, and each start reads on to
    // the end of it.
    if bytes[name_start..name_end]
        .get(..3)
        .is_some_and(|word| word.eq_ignore_ascii_case(b"key"))
    {
        let gap = run_start(bytes, name_start, |byte| {
            is_blank_byte(byte) || (byte == b'.')
        });
        if gap < name_start {
            first = first.min(gap.saturating_sub(MOST_BEFORE_A_GAP));
        }
    }
    Some(Anchor {
        at: separator,
        starts: first..name_end,
    })
}

/// The longest keyword that a gap can follow, `private` or `secret ` with its blank, and a byte for
/// the capital a glued name starts with (`xAPI key`).
const MOST_BEFORE_A_GAP: usize = 8;

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
    // A query parameter is a start of its own: `?X-Amz-Signature=` opens with a letter that is no
    // keyword's.
    if starts_a_parameter(bytes, start) && start < end {
        first = start;
    }
    (start, first)
}

/// Whether anything stands after `from`, past any spaces. Whether it is a value is for the rule,
/// which answers [`Outcome::Keep`] for the whole name when it is not: a cookie's line may open
/// with a `;`, and a locator that required more would never ask.
pub(super) fn has_value(bytes: &[u8], from: usize) -> bool {
    skip_spaces(bytes, from) < bytes.len()
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
        tail: "",
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
    let user = &bytes[start..end];
    // A name such as `jenkins-deploy-agent-01` is long and has a digit; a token also has a capital
    // or an underscore, is long enough to be random, or opens like one.
    let looks_random = user.iter().any(u8::is_ascii_uppercase)
        || user.contains(&b'_')
        || user.len() >= 32
        || [&b"xox"[..], b"sk-", b"pat-"].iter().any(|prefix| {
            user.len() >= prefix.len() && user[..prefix.len()].eq_ignore_ascii_case(prefix)
        });
    let token_like = user.len() >= 20 && user.iter().any(u8::is_ascii_digit) && looks_random;
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
