//! The newer reads of two credential rules: a header, and a name with a value. (The URL's userinfo
//! rule is not read twice; it is the release before's.)
//!
//! `stderr_text` does not run these: it runs the rules of the release before, kept as they were in
//! `redact::floor`. These are read beside them by the entry points for a vendor's messages, over
//! the same text, and what either hides is hidden (`redact::union`). That is why a rule here may
//! read more (escape depth, containers and sequences across lines, block scalars, counts of twelve
//! digits) and never has to account for what the release before did with the same place.
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

use super::scan::{Anchor, count_steps, first_match, run_start};
use super::structured::{CREDENTIAL_NAMES, CREDENTIAL_WORDS, SECRET_DIGITS};
use super::{
    Outcome, Rewrite, is_name_byte, is_space_byte, is_value_byte, match_byte, match_word,
    skip_spaces, strip, take_while,
};

/// The header name the bearer rule starts at.
pub(super) const AUTHORIZATION: &[u8] = b"authorization";

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

/// The most backslashes read in front of a quote: a JSON text serialised into a string, that string
/// into another, and so on, writes each quote with `2^n - 1` of them for the `n`th string. Five
/// strings deep is 31. More is a run of backslashes and not a quote, and costs the same bounded read.
const MAX_SLASHES: usize = 31;

/// How many backslashes stand at `at`, counting no further than one past [`MAX_SLASHES`].
fn slashes_from(bytes: &[u8], at: usize) -> usize {
    let mut count = 0;
    while count <= MAX_SLASHES && bytes.get(at + count) == Some(&b'\\') {
        count_steps(1);
        count += 1;
    }
    count
}

/// How many backslashes stand just before `end`, counting no further than one past [`MAX_SLASHES`].
fn slashes_before(bytes: &[u8], end: usize) -> usize {
    let mut count = 0;
    while count <= MAX_SLASHES
        && end
            .checked_sub(count + 1)
            .and_then(|before| bytes.get(before))
            == Some(&b'\\')
    {
        count_steps(1);
        count += 1;
    }
    count
}

/// Where a closing quote that ends at `end` begins, with the backslashes that escape it, or `end`
/// when the byte before it is not a quote.
///
/// Inlined, with the backslash count behind a call: nearly every separator is turned away on the
/// byte before it, and a locator that grows past what the compiler inlines costs the plain path.
#[inline(always)]
fn before_quote(bytes: &[u8], end: usize) -> usize {
    count_steps(1);
    match end.checked_sub(1) {
        Some(quote) if bytes.get(quote).is_some_and(|byte| is_quote(*byte)) => {
            if quote > 0 && bytes[quote - 1] == b'\\' {
                quote - escaping_slashes(bytes, quote)
            } else {
                quote
            }
        }
        _ => end,
    }
}

/// The backslashes in front of the quote at `quote`, or none when there are more than
/// [`MAX_SLASHES`].
#[inline(never)]
fn escaping_slashes(bytes: &[u8], quote: usize) -> usize {
    match slashes_before(bytes, quote) {
        slashes if slashes <= MAX_SLASHES => slashes,
        _ => 0,
    }
}

/// The quote at `at`, with the backslashes in front of it: the quote, how many there are, and the
/// byte after it. A JSON text inside a string has its quotes escaped, once for each string it is
/// inside.
fn quote_at(bytes: &[u8], at: usize) -> Option<(u8, usize, usize)> {
    let slashes = slashes_from(bytes, at);
    let quote = *bytes.get(at + slashes).filter(|byte| is_quote(**byte))?;
    (slashes <= MAX_SLASHES).then_some((quote, slashes, at + slashes + 1))
}

/// The byte after a quote at `at`, or `at` when there is none.
fn skip_quote(bytes: &[u8], at: usize) -> usize {
    quote_at(bytes, at).map_or(at, |(_, _, after)| after)
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
    /// The bytes that containers spanning lines have been read for, past their first line: the
    /// budget of [`closing_bracket`]. It belongs to the pass and is not reset with the query run.
    container_bytes: usize,
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
            self.read_to = 0;
            self.question_mark = false;
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

/// Whether the digits in `text` are few enough for a count: twelve or more are a secret, as in the
/// JSON walk (see [`SECRET_DIGITS`]).
fn is_a_count(text: &[u8]) -> bool {
    text.iter().filter(|byte| byte.is_ascii_digit()).count() < SECRET_DIGITS
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
    let slashes = slashes_before(bytes, inner - 1);
    (is_quote(quote) && slashes <= MAX_SLASHES).then_some(Opened {
        inner,
        quote,
        slashes,
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

// ---- the assignment rule --------------------------------------------------------------------

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
#[inline(always)]
pub(super) fn assignment_rule(bytes: &[u8], at: usize, query: &mut QueryRun) -> Option<Outcome> {
    // Every byte of a name is a start now, since a keyword may end a name or follow a capital.
    // Most are not the first letter of anything, and are turned away on that alone, before the
    // frame the rest of the rule needs is set up.
    if !(starts_a_keyword(bytes, at) || starts_a_parameter(bytes, at)) {
        return None;
    }
    read_assignment(bytes, at, query)
}

/// [`assignment_rule`] for a place that may open a name.
#[inline(never)]
fn read_assignment(bytes: &[u8], at: usize, query: &mut QueryRun) -> Option<Outcome> {
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
    let (mut value_start, mut list) = list_item(bytes, skip_spaces(bytes, after_separator));
    // A cookie header's value is its line, marker and all.
    if keyword.kind == Kind::Line && list.is_some() {
        (value_start, list) = (skip_spaces(bytes, after_separator), None);
    }
    let mut context = Context {
        query,
        start: run_start(bytes, at, is_name_byte),
        at,
        colon,
        quoted_name,
        in_query: None,
    };
    // `password: |` and the lines indented under it are one value.
    let block = if list.is_none() && keyword.kind != Kind::Line {
        block_scalar_end(bytes, value_start, context.start)
    } else {
        None
    };
    // A sequence item reads as the sequence whatever it opens with. A cookie header's line, and a
    // password's phrase after a colon, are read to the end of the line however deep the quote that
    // opens them is escaped: the reading of an escape deeper than one belongs to this redactor,
    // and the line is what the release before it redacted.
    let takes_the_line = keyword.kind == Kind::Line || (keyword.kind == Kind::Phrase && colon);
    if list.is_none()
        && block.is_none()
        && let Some(opened) = open_quote(bytes, value_start)
        && !(takes_the_line && opened.slashes > 1)
    {
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
        && is_a_count(&bytes[value_start..digits_end])
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
        _ if block.is_some() => block.unwrap_or(value_start),
        _ if list.is_some() => list_end(bytes, value_start, list.flatten()),
        _ if with_a_token => line_end(bytes, value_start),
        (Kind::Line, _) => line_end(bytes, value_start),
        // The brackets close the value, and whatever is glued after them goes with it.
        (_, Some(b'[' | b'{')) => take_while(
            bytes,
            closing_bracket(bytes, value_start, &mut context.query.container_bytes),
            |byte| !ends_value(byte),
        ),
        (Kind::Phrase, _) if phrase => phrase_end(bytes, value_start),
        _ => take_while(bytes, value_start, |byte| !ends_value(byte)),
    };
    let value_end = {
        let mut end = value_end;
        loop {
            let past = whole_marker(bytes, value_start, end);
            if past == end {
                break end;
            }
            end = take_while(bytes, past, |byte| !ends_value(byte));
        }
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

/// `end`, moved past the `]` of a `[REDACTED]` that it falls inside: a value that ends at a bracket
/// (a quoted member's does) must not end in the middle of a marker an earlier rule wrote, which
/// would leave `[REDACTED]]` and a text that the next reading redacts differently.
fn whole_marker(bytes: &[u8], start: usize, end: usize) -> usize {
    const OPEN: &[u8] = b"[REDACTED";
    let inside = bytes.get(end) == Some(&b']')
        && end >= start + OPEN.len()
        && &bytes[end - OPEN.len()..end] == OPEN;
    if inside { end + 1 } else { end }
}

/// Where the YAML block scalar that opens at `at` ends: `|` or `>`, a chomping or indentation
/// indicator and a comment, then the lines under it that are indented deeper than the line of
/// the name at `name`. Blank lines between them belong to it, and those after the last do not.
///
/// Read across no more than [`MAX_CONTAINER_LINES`] lines and [`MAX_CONTAINER_BYTES`] bytes, as a
/// container is. A longer scalar is redacted as far as that, and nothing past it. `None` when
/// `at` is no header (`password: |abc` is a value), or nothing is indented under it.
fn block_scalar_end(bytes: &[u8], at: usize, name: usize) -> Option<usize> {
    if !matches!(bytes.get(at), Some(b'|' | b'>')) {
        return None;
    }
    let mut header = at + 1;
    while header < at + 3 && matches!(bytes.get(header), Some(b'+' | b'-' | b'0'..=b'9')) {
        header += 1;
    }
    let blanks = take_while(bytes, header, is_blank_byte);
    let comment = blanks > header && bytes.get(blanks) == Some(&b'#');
    let header_end = if comment {
        raw_line_end(bytes, blanks)
    } else {
        blanks
    };
    if !matches!(bytes.get(header_end), Some(b'\n' | b'\r')) {
        return None;
    }
    // The key's own indentation: what is under it must be deeper.
    let line_start = run_start(bytes, name, |byte| (byte != b'\n') & (byte != b'\r'));
    let indent = take_while(bytes, line_start, is_blank_byte) - line_start;
    let mut end = None;
    let mut line = next_line(bytes, header_end);
    for _ in 0..MAX_CONTAINER_LINES {
        if line > bytes.len() || line - at > MAX_CONTAINER_BYTES {
            break;
        }
        let content = take_while(bytes, line, is_blank_byte);
        let stop = raw_line_end(bytes, content);
        if content < stop {
            if content - line <= indent {
                break;
            }
            end = Some(trim_end(bytes, content, stop));
        }
        if stop >= bytes.len() {
            break;
        }
        line = next_line(bytes, stop);
    }
    end
}

/// The end of the line that `start` is on, before its break and with its trailing blanks.
fn raw_line_end(bytes: &[u8], start: usize) -> usize {
    take_while(bytes, start, |byte| (byte != b'\n') & (byte != b'\r'))
}

/// The start of the line after the break at `at`: `\n`, `\r\n` or `\r`.
fn next_line(bytes: &[u8], at: usize) -> usize {
    match (bytes.get(at), bytes.get(at + 1)) {
        (Some(b'\r'), Some(b'\n')) => at + 2,
        _ => at + 1,
    }
}

/// The most items of a YAML sequence that a credential's name takes.
const MAX_LIST_ITEMS: usize = 64;

/// Where the value of a YAML sequence item starts, when the value at `at` opens with the `- ` that
/// marks one: the item's text, and, as `Some(Some(indent))`, the column of the marker when it
/// stands first on its line, so that the items below it can be found.
///
/// A hyphen glued to its word (`-sk-live-x`) is part of a value, as it always was.
fn list_item(bytes: &[u8], at: usize) -> (usize, Option<Option<usize>>) {
    // Nearly every value opens with something else, so this is a byte and a return.
    if bytes.get(at) != Some(&b'-') || !bytes.get(at + 1).is_some_and(|b| is_blank_byte(*b)) {
        return (at, None);
    }
    let item = take_while(bytes, at + 1, is_blank_byte);
    if bytes
        .get(item)
        .is_none_or(|byte| matches!(byte, b'\n' | b'\r'))
    {
        return (at, None);
    }
    let blanks = run_start(bytes, at, is_blank_byte);
    let first_on_line = blanks == 0 || matches!(bytes[blanks - 1], b'\n' | b'\r');
    (item, Some(first_on_line.then_some(at - blanks)))
}

/// Where the sequence whose first item starts at `item` ends: the end of that line, and of every
/// line below it that opens with a marker in the same column (up to [`MAX_LIST_ITEMS`]). A marker
/// in any other column belongs to another sequence.
fn list_end(bytes: &[u8], item: usize, indent: Option<usize>) -> usize {
    let mut end = line_end(bytes, item);
    let Some(indent) = indent else {
        return end;
    };
    for _ in 0..MAX_LIST_ITEMS {
        let marker = skip_spaces(bytes, end);
        let blanks = run_start(bytes, marker, is_blank_byte);
        let in_column =
            blanks > 0 && matches!(bytes[blanks - 1], b'\n' | b'\r') && marker - blanks == indent;
        let marked = bytes.get(marker) == Some(&b'-')
            && bytes.get(marker + 1).is_some_and(|b| is_blank_byte(*b));
        if !(in_column && marked) {
            break;
        }
        end = line_end(bytes, take_while(bytes, marker + 1, is_blank_byte));
    }
    end
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

/// The most lines a container under a credential name is read across.
const MAX_CONTAINER_LINES: usize = 64;

/// The most bytes a container is read across.
const MAX_CONTAINER_BYTES: usize = 16 * 1024;

/// What a pass may read for containers past their first line, over and above one byte for each
/// byte of the text it has passed: a text of containers that never close must not read
/// [`MAX_CONTAINER_BYTES`] for each of them.
const CONTAINER_ALLOWANCE: usize = 8 * 1024;

/// Where the bracket that opens at `start` closes, counting the brackets and the strings inside:
/// a JSON array or object that is one credential's value. A string opens with a quote, or with
/// backslashes and a quote when the JSON is itself inside a string (`{\\"a\\":1}`, and
/// `{\\\\\\"a\\\\\\":1}` one string deeper), and a string opened with `n` backslashes ends at
/// a quote with exactly `n`.
///
/// A pretty-printed container takes many lines, so the bracket is looked for across line breaks, up
/// to [`MAX_CONTAINER_LINES`] lines and [`MAX_CONTAINER_BYTES`] bytes, and a string ends with its
/// line. One that does not close within that, or when the pass has used its allowance (`spent`,
/// see [`CONTAINER_ALLOWANCE`]), ends with its first line: an array that never closes must not
/// take the rest of the text with it. Everything read past the first line is added to `spent`.
fn closing_bracket(bytes: &[u8], start: usize, spent: &mut usize) -> usize {
    let across_lines = *spent < CONTAINER_ALLOWANCE.saturating_add(start);
    let mut depth = 0usize;
    // The quote that opened the string we are in, and the backslashes written in front of it.
    let mut string: Option<(u8, usize)> = None;
    // Where the first line ends, and how many lines have been read.
    let mut first_break: Option<usize> = None;
    let mut lines = 1;
    let mut at = start;
    let charge = |spent: &mut usize, first: Option<usize>, end: usize| {
        *spent = spent.saturating_add(first.map_or(0, |first| end.saturating_sub(first)));
    };
    while let Some(&byte) = bytes.get(at) {
        count_steps(1);
        at += 1;
        if matches!(byte, b'\n' | b'\r') {
            let first = *first_break.get_or_insert(at - 1);
            if byte == b'\r' && bytes.get(at) == Some(&b'\n') {
                at += 1;
            }
            if !across_lines || lines >= MAX_CONTAINER_LINES || at - start > MAX_CONTAINER_BYTES {
                charge(spent, first_break, at);
                return first;
            }
            lines += 1;
            string = None;
            continue;
        }
        match (string, byte) {
            (Some((_, 0)), b'\\') if !matches!(bytes.get(at), Some(b'\n' | b'\r')) => at += 1,
            (Some((quote, 0)), byte) if byte == quote => string = None,
            (Some((quote, slashes)), b'\\') => {
                let run = 1 + take_while(bytes, at, |byte| byte == b'\\') - at;
                at += run - 1;
                if run == slashes && bytes.get(at) == Some(&quote) {
                    string = None;
                    at += 1;
                }
            }
            (Some(_), _) => {}
            (None, b'\\') => {
                let run = 1 + take_while(bytes, at, |byte| byte == b'\\') - at;
                at += run - 1;
                let quoted = bytes.get(at).is_some_and(|byte| is_quote(*byte));
                if run <= MAX_SLASHES && quoted {
                    string = Some((bytes[at], run));
                    at += 1;
                } else if quoted {
                    // Deeper than a string is read, and not a string: the line is the value, as it
                    // is for a container that never closes.
                    let first = *first_break.get_or_insert_with(|| {
                        take_while(bytes, at, |byte| (byte != b'\n') & (byte != b'\r'))
                    });
                    charge(spent, first_break, at);
                    return first;
                }
            }
            (None, b'"' | b'\'') => string = Some((byte, 0)),
            (None, b'[' | b'{') => depth += 1,
            (None, b']' | b'}') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    charge(spent, first_break, at);
                    return at;
                }
            }
            (None, _) => {}
        }
    }
    charge(spent, first_break, at);
    first_break.unwrap_or(bytes.len())
}

/// A quote that opens a value at `at`: where its text starts, the quote and the backslashes that
/// escape it (none for a plain string, one for JSON inside a string, `2^n - 1` for the `n`th).
struct Opened {
    inner: usize,
    quote: u8,
    slashes: usize,
}

fn open_quote(bytes: &[u8], at: usize) -> Option<Opened> {
    let (quote, slashes, inner) = quote_at(bytes, at)?;
    Some(Opened {
        inner,
        quote,
        slashes,
    })
}

/// Where the text inside a quote ends: at its closing quote, or at the end of the line when it is
/// never closed.
///
/// A plain string ends at its quote, and a backslash escapes the byte after it. One written with
/// `n` backslashes in front of its quote ends at a quote with exactly `n`: more are the quotes of
/// a JSON text inside it, written one level deeper. The count is kept as the text is read, so the
/// work is the length of the string however long a run of backslashes is.
fn close_quote(bytes: &[u8], opened: &Opened) -> usize {
    let mut at = opened.inner;
    let mut run = 0;
    while let Some(byte) = bytes.get(at) {
        count_steps(1);
        match *byte {
            b'\n' | b'\r' => return at,
            b'\\' if opened.slashes == 0 => at += 1,
            b'\\' => run += 1,
            byte if byte == opened.quote && run == opened.slashes => return at - run,
            _ => run = 0,
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
        let tail = closing_quote_text(&opened);
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
        && is_a_count(&bytes[opened.inner..end])
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

/// The backslashes and the quote that close `opened`, as text that outlives the match: a suffix of
/// [`MAX_SLASHES`] backslashes and a quote.
fn closing_quote_text(opened: &Opened) -> &'static str {
    const DOUBLE: &str = "\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\"";
    const SINGLE: &str = "\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\\'";
    let all = if opened.quote == b'"' { DOUBLE } else { SINGLE };
    &all[MAX_SLASHES - opened.slashes.min(MAX_SLASHES)..]
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
    let after = end + opened.slashes + 1;
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
