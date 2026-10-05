//! The redactor held to the scan it replaced and to the release before this one.
//!
//! [`stderr_text`](super::stderr_text) copies clean text whole and tries a rule only where
//! [`Candidates`](super::scan::Candidates) says it can match. The scan it replaced read every
//! character and tried each rule at every word. That scan is kept here as the definition of the
//! right answer for where a rule is tried: each stage has to return what it returns, byte for
//! byte, for every input below. The bearer rule is tried by the 0.4.1 rule in `reference`, which
//! it still is. The assignment and URL rules are tried by the rules under test, because a
//! removed byte no longer parts a name from its separator in them (see `redact::boundaries`) and
//! the 0.4.1 rules do not read it that way.
//!
//! The whole redactor is held to the 0.4.1 pipeline as shipped, with one narrow exception: where a
//! removed byte stands in a name or a scheme it may differ, because 0.4.1 stopped a name at
//! the marker the stripper leaves there and let the credential after it through. That is checked,
//! not listed (see [`Agreement`]): for every input where the two differ, the text must hold a
//! marker, and the answer must be the one 0.4.1's rules give with the marker read as a hyphen,
//! with the hyphens of both taken out. A difference anywhere else is a credential left in a
//! diagnostic, or text altered for nothing, and the failure names the input.

use super::reference::{
    self, Rewrite, bearer_rule, remove_boundaries, starts_a_word, strip_every_character,
};
use super::strip::strip_control_characters;
use super::{
    REDACTED, as_text, ends_awaiting_value, redact_assignments, redact_bearer,
    redact_url_passwords, stderr_text,
};

/// A rule under test, described the way the replaced scan wants it: where the match ends and the
/// text that stands in its place.
fn described(
    rule: impl Fn(&[u8], usize) -> Option<super::Rewrite>,
) -> impl Fn(&[u8], usize) -> Option<Rewrite> {
    move |bytes, at| {
        let found = rule(bytes, at)?;
        let kept = as_text(bytes, at, found.kept_end);
        Some(Rewrite {
            end: found.end,
            replacement: format!("{kept}{}{REDACTED}", found.joiner),
        })
    }
}

/// How this redactor's answer relates to the one 0.4.1 shipped.
#[derive(Debug, PartialEq, Eq)]
enum Agreement {
    /// The same bytes.
    Same,
    /// Different, because a removed byte stands in a name or a scheme and this redactor
    /// reads it as part of the run it interrupts. See [`as_a_hyphen`].
    ReadsAMarkerAsPartOfAName,
}

/// What 0.4.1's own rules return when the marker in a name or a scheme is a hyphen, with
/// every hyphen taken out.
///
/// The marker is a byte that is not a letter, so a name after it starts a word, and not a space,
/// so it does not end a value. A hyphen is the one ASCII byte with those properties that 0.4.1's
/// name, scheme and value runs also step over. So this is what it means for a marker to be part of
/// the run it interrupts, said in 0.4.1's code and not in this redactor's: the bearer rule runs
/// first with the markers as they are, since it reads them as a gap and still does, and the
/// assignment and URL rules then run over the same text with each marker a hyphen.
///
/// A hyphen of the text's own cannot be told from one that stood for a marker afterwards, so all
/// of them are taken out, and a caller compares against text that has had the same done to it. A
/// difference that is only hyphens is the one thing this cannot see; any other, such as a value
/// that runs further than 0.4.1's rules would run it, is seen.
fn as_a_hyphen(raw: &str) -> String {
    let plain = reference::strip_control_characters(raw);
    let bearer = reference::redact_bearer(&plain).replace('\u{1}', "-");
    let assignments = reference::redact_assignments(&bearer);
    reference::redact_url_passwords(&assignments).replace('-', "")
}

/// The relation, or the reason there is none, for `raw`.
///
/// The two may differ only when the stripped text holds a marker: without a removed byte the rules
/// read what they always did, and the answer has to be 0.4.1's own. Where there is one, the answer
/// has to be the one 0.4.1's rules give with the marker read as a hyphen, compared with the hyphens
/// of both taken out.
fn agreement_with_0_4_1(raw: &str) -> Result<Agreement, String> {
    let (shipped, head) = (reference::stderr_text(raw), stderr_text(raw));
    if shipped == head {
        return Ok(Agreement::Same);
    }
    let marked = reference::strip_control_characters(raw).contains('\u{1}');
    let hyphen = as_a_hyphen(raw);
    if marked && hyphen == head.replace('-', "") {
        return Ok(Agreement::ReadsAMarkerAsPartOfAName);
    }
    Err(format!(
        "expected the whole redactor to return what 0.4.1 shipped, or, where a removed byte stands \
         in the text, what 0.4.1's rules return with that byte read as a hyphen, hyphens taken out \
         of both | input {raw:?} 0.4.1 returned {shipped:?}, read as a hyphen {hyphen:?}, a marker \
         in the text: {marked}, received {head:?}"
    ))
}

/// `rewrite` as it was first written: `rule` tried at every word boundary, the text pushed a
/// character at a time.
fn rewrite_every_word(raw: &str, rule: impl Fn(&[u8], usize) -> Option<Rewrite>) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut at = 0;
    while at < bytes.len() {
        if starts_a_word(bytes, at)
            && let Some(found) = rule(bytes, at)
        {
            out.push_str(&found.replacement);
            at = found.end;
            continue;
        }
        let next = next_char_boundary(raw, at);
        out.push_str(&raw[at..next]);
        at = next;
    }
    out
}

fn next_char_boundary(raw: &str, at: usize) -> usize {
    let mut next = at + 1;
    while next < raw.len() && !raw.is_char_boundary(next) {
        next += 1;
    }
    next.min(raw.len())
}

/// Fails, naming `raw`, unless redacting what one pass returned changes nothing.
///
/// A host that redacts again, as the harnesses do with a tail from a control that may not have
/// redacted it, must find nothing more to take out of text this redactor already handled. Where a
/// second pass redacts more, the first one left a credential in the clear.
fn assert_one_pass_is_a_fixed_point(raw: &str) {
    let once = stderr_text(raw);
    let twice = stderr_text(&once);
    assert!(
        once == twice,
        "expected one pass of the redactor to be a fixed point | input {raw:?} first pass \
         {once:?}, second pass {twice:?}"
    );
}

/// Fails, naming `raw` and the stage, unless every stage returns what the replaced scan returns.
///
/// Each rule is also run on `raw` itself, not only on what the stage before it left: a rule then
/// sees the control characters and markers the stripper would have taken out.
fn assert_matches_the_replaced_scan(raw: &str) {
    assert_one_pass_is_a_fixed_point(raw);
    let expect = |stage: &str, input: &str, run: fn(&str) -> String, expected: String| {
        // Caught, so a stage that panics is reported with the input that made it.
        let received = match std::panic::catch_unwind(|| run(input)) {
            Ok(text) => format!("{text:?}"),
            Err(_) => String::from("a panic"),
        };
        assert!(
            received == format!("{expected:?}"),
            "expected {stage} to return {expected:?} | input {input:?} (from {raw:?}) received \
             {received}"
        );
    };
    let plain = strip_every_character(raw);
    expect(
        "the control-character stripper",
        raw,
        strip_control_characters,
        plain.clone(),
    );
    let bearer = rewrite_every_word(&plain, bearer_rule);
    let assignments = rewrite_every_word(&bearer, described(super::assignment_rule));
    let urls = rewrite_every_word(&assignments, described(super::url_password_rule));
    for input in [raw, plain.as_str()] {
        expect(
            "the bearer rule",
            input,
            redact_bearer,
            rewrite_every_word(input, bearer_rule),
        );
    }
    for input in [raw, bearer.as_str()] {
        expect(
            "the assignment rule",
            input,
            redact_assignments,
            rewrite_every_word(input, described(super::assignment_rule)),
        );
    }
    for input in [raw, assignments.as_str()] {
        expect(
            "the URL password rule",
            input,
            redact_url_passwords,
            rewrite_every_word(input, described(super::url_password_rule)),
        );
    }
    expect(
        "the whole redactor",
        raw,
        stderr_text,
        remove_boundaries(urls),
    );
    if let Err(why) = agreement_with_0_4_1(raw) {
        panic!("{why}");
    }
    assert_awaiting_agrees_with_0_4_1(raw);
}

/// Whether a cut after `raw` awaits a value, held to what 0.4.1 answered.
///
/// Awaiting more than 0.4.1 did is always allowed: it drops one more line and leaks nothing. Not
/// awaiting what 0.4.1 awaited is allowed only where the whole redactor also differs, because then
/// a rule reads the text differently and the answer follows it. `apikeybearer<VT>=password` is
/// the example: 0.4.1 saw no name before the `=`, so it took the trailing `password` for a name
/// still waiting for its value, where this reads `password` as the value of `apikeybearer` and the
/// line as complete, as it does for the same text with nothing removed.
fn assert_awaiting_agrees_with_0_4_1(raw: &str) {
    let (shipped, received) = (
        reference::ends_awaiting_value(raw),
        ends_awaiting_value(raw),
    );
    if shipped == received || (received && !shipped) {
        return;
    }
    assert!(
        agreement_with_0_4_1(raw) == Ok(Agreement::ReadsAMarkerAsPartOfAName),
        "expected ends_awaiting_value to return {shipped} as 0.4.1 did, unless the redactor also \
         differs from 0.4.1 | input {raw:?} received {received}"
    );
}

/// Every keyword in each case, every byte a rule reads as punctuation or space, the pieces of a
/// URL, every escape form and stripped class, and characters of two, three and four bytes that
/// are kept, some of them behind a lead byte that removed characters share.
const FRAGMENTS: &[&str] = &[
    "authorization",
    "Authorization",
    "AUTHORIZATION",
    "bearer",
    "Bearer",
    "BEARER",
    "basic",
    "Basic",
    "api",
    "key",
    "api_key",
    "API-KEY",
    "ApiKey",
    "secret",
    "SECRET",
    "token",
    "Token",
    "password",
    "PASSWORD",
    "passwd",
    "Passwd",
    "credential",
    "CREDENTIAL",
    ":",
    "=",
    " ",
    "  ",
    "\t",
    "\n",
    "\r",
    "\u{b}",
    "\u{c}",
    ",",
    ";",
    "_",
    "-",
    ".",
    "+",
    "/",
    "://",
    "@",
    "?",
    "#",
    "x",
    "Z",
    "7",
    "v4lue",
    "https",
    "redis",
    "user",
    "é",
    "\u{a0}",
    "\u{620}",
    "^",
    "—",
    "\u{2028}",
    "日",
    "🍋",
    "\u{1b}",
    "\u{1b}[",
    "\u{1b}[31m",
    "\u{1b}[ q",
    "\u{1b}]0;",
    "\u{1b}P",
    "\u{1b}(B",
    "\u{1b}c",
    "\u{1b}\\",
    "\u{7}",
    "\u{0}",
    "\u{1}",
    "\u{7f}",
    "\u{85}",
    "\u{90}",
    "\u{9b}",
    "\u{9c}",
    "\u{9d}",
    "\u{61c}",
    "\u{200e}",
    "\u{202e}",
    "\u{2066}",
];

/// A credential, or a control character, in each shape a rule or the stripper acts on.
const SHAPES: &[&str] = &[
    "Authorization: Bearer sk-live-1",
    "authorization:basic dXNlcg",
    "AUTHORIZATION \n:\tBEARER\n sk",
    "Authorization: Bearer\rsk-live-1",
    "api_key=v",
    "API-KEY: v",
    "apikey =v",
    "ApiKey=v",
    "secret=v",
    "SECRET : v",
    "token = v",
    "TOKEN:v",
    "password:v",
    "PASSWORD=v",
    "passwd=v",
    "PASSWD\n=\nv",
    "credential=v",
    "CREDENTIALS=v",
    "AWS_SECRET_ACCESS_KEY=wJalrX",
    "a_token_count_of=3",
    "mytoken=v",
    "token=café, next=1",
    "https://user:hunter2@host/x",
    "redis://:hunter2@db/main",
    "git+ssh://u.v-w:pw@h",
    "https://example.com/x:y",
    "basic\u{1b}[0m://user:hunter2@host/x",
    "Bearer\r://:hunter2@db/main",
    "BASIC\0://u:p@h",
    "x\u{1b}[0mbasic\u{1b}[0m://u:p@h",
    "\u{1b}[31mAPI_KEY\u{1b}[0m=v",
    "x\u{1b}[31mTOKEN=v",
    "sec\rret=v",
    "\u{1b}]0;title\u{7}TOKEN=v",
    "\u{1b}PASSWORD=v",
    "\u{9b}1mAPI_KEY=v",
    "\u{202e}gpj.exe",
    "é",
    "—",
    "\u{0}",
    "\u{7f}",
    ":",
    "=",
    "://",
];

/// A fixed sequence of numbers, so a failure reproduces: xorshift64 from a seed.
struct Sequence(u64);

impl Sequence {
    fn next(&mut self) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % 1_000_003).expect("expected a value under 1,000,003 to fit")
    }

    fn pick<'a>(&mut self, from: &[&'a str]) -> &'a str {
        from[self.next() % from.len()]
    }
}

#[test]
fn empty_text_and_the_documented_fixture_match_the_replaced_scan() {
    for raw in [
        "",
        "Authorization: Bearer top-secret API_KEY=another-secret redis://app:password@db/main",
        "error: the vendor exited with status 1 (no configuration found)",
    ] {
        assert_matches_the_replaced_scan(raw);
    }
}

#[test]
fn every_fragment_alone_and_every_pair_of_fragments_matches_the_replaced_scan() {
    for first in FRAGMENTS {
        assert_matches_the_replaced_scan(first);
        for second in FRAGMENTS {
            assert_matches_the_replaced_scan(&format!("{first}{second}"));
            assert_matches_the_replaced_scan(&format!("{first}{second}=v"));
            assert_matches_the_replaced_scan(&format!("x {first}{second}: Bearer v"));
        }
    }
}

/// The searches test 32 bytes at a time, so each shape is put at every offset of the first two
/// blocks and one past, behind text that does and does not let a word start, and both at the end
/// of the text and ahead of more of it.
#[test]
fn every_shape_at_every_offset_across_scan_blocks_matches_the_replaced_scan() {
    for shape in SHAPES {
        for filler in [" ", "a", ".", "é"] {
            for before in 0..=66 {
                for after in [0, 40] {
                    let raw = format!("{}{shape}{}", filler.repeat(before), filler.repeat(after));
                    assert_matches_the_replaced_scan(&raw);
                }
            }
        }
    }
}

#[test]
fn two_shapes_at_every_distance_match_the_replaced_scan() {
    for first in SHAPES {
        for second in SHAPES {
            for gap in ["", " ", "\n", ", ", "x", &" ".repeat(31), &"a ".repeat(33)] {
                assert_matches_the_replaced_scan(&format!("{first}{gap}{second}"));
            }
        }
    }
}

#[test]
fn generated_runs_of_fragments_match_the_replaced_scan() {
    let mut sequence = Sequence(0x9e37_79b9_7f4a_7c15);
    for _ in 0..40_000 {
        let length = 1 + sequence.next() % 24;
        let raw: String = (0..length).map(|_| sequence.pick(FRAGMENTS)).collect();
        assert_matches_the_replaced_scan(&raw);
    }
}

/// Text no fragment list would spell: the bytes the rules and the stripper read, in any order,
/// repaired into text the way a stderr tail is. A lead byte without its continuation becomes
/// U+FFFD, and one with it becomes a removed or a kept character.
#[test]
fn generated_runs_of_bytes_match_the_replaced_scan() {
    const BYTES: &[u8] = b"authorizationbearerbasicapikeysecrettokenpasswordcredentialAPI_KEY \
        ::==  \n\n\r\t,;-_.+/@?#19^AUTHZBS\x00\x01\x07\x0b\x0c\x1b\x1b[]\\()PXcmq\x7f\
        \xc2\x9b\xc2\x9c\xc2\x9d\xc2\x90\xc2\xa0\xc3\xa9\xd8\x9c\xd8\xa0\xe3\x81\x82\xe2\x80\xae\xe2\x80\x94\xe2\x81\xa6\xf0\x9f";
    let mut sequence = Sequence(0x0123_4567_89ab_cdef);
    for _ in 0..20_000 {
        let length = sequence.next() % 72;
        let bytes: Vec<u8> = (0..length)
            .map(|_| BYTES[sequence.next() % BYTES.len()])
            .collect();
        assert_matches_the_replaced_scan(&String::from_utf8_lossy(&bytes));
    }
}

/// Long lines of plain words with a shape now and then: what the block search skips, and the
/// matches it must still stop for.
#[test]
fn generated_diagnostic_lines_match_the_replaced_scan() {
    const WORDS: &[&str] = &[
        "error",
        "warning:",
        "the",
        "vendor",
        "exited",
        "with",
        "status",
        "1",
        "(no",
        "configuration",
        "found)",
        "src/main.rs:42:9",
        "tokens",
        "apiary",
        "author",
        "https://example.com/",
        "a.b-c",
        "naïve",
        "→",
    ];
    let mut sequence = Sequence(0x2545_f491_4f6c_dd1d);
    for _ in 0..2_000 {
        let mut raw = String::new();
        for _ in 0..1 + sequence.next() % 60 {
            let word = match sequence.next() % 12 {
                0 => sequence.pick(SHAPES),
                _ => sequence.pick(WORDS),
            };
            raw.push_str(word);
            raw.push_str(sequence.pick(&[" ", " ", " ", "\n", "", "\t", ": ", "="]));
        }
        assert_matches_the_replaced_scan(&raw);
    }
}

/// What a name or a scheme is repeated from: the unit a rule reads again for each of many starts.
const REPEATED_UNITS: &[&str] = &[
    "token_",
    "secret-",
    "api_key_",
    "API-KEY.",
    "password_",
    "Credential_",
    "token_a.",
    "a.",
    "a.b-",
    "x1+",
    "ab",
    "_",
    ".",
    "Z",
];

/// What follows a repeated unit: every way a value, a separator or an authority can be missing,
/// empty, present, or cut short by the byte that ends it.
const AFTER_REPEATED: &[&str] = &[
    "",
    "=",
    ":",
    " =",
    "= ",
    "=v",
    ":v",
    "= v",
    "=  ,x",
    "=;x",
    "=\n",
    "= \n v",
    "=\rv",
    ":::",
    "==",
    "=:",
    "://",
    "://u",
    "://u:p@h",
    "://:p@h",
    "://u:p",
    "://u:p/x@h",
    "://u@h",
    "://u:@h",
    "://u:p@",
    "://u:p @h",
    "://a.b://c:d@e",
    "=://u:p@h",
    "=v://u:p@h",
    "bearer v",
    ": bearer v",
    " ",
    "\n",
];

/// Names and schemes repeated to every length from none to past a scan block, each ending every
/// way and behind text that does and does not let a word start. The reference is quadratic on
/// these, so the lengths stay short; what matters is that every start of a repeated unit is asked.
#[test]
fn repeated_names_and_schemes_match_the_replaced_scan() {
    for unit in REPEATED_UNITS {
        for repeats in (0..=12).chain([20, 33]) {
            for after in AFTER_REPEATED {
                for before in ["", "x ", "1", "\n", "é", ":"] {
                    let raw = format!("{before}{}{after}", unit.repeat(repeats));
                    assert_matches_the_replaced_scan(&raw);
                }
            }
        }
    }
}

/// Repeated units joined to each other and to shapes at random: a rejected anchor beside an
/// accepted one, a match that ends in the middle of a run, a second run that starts inside the
/// value of the first.
#[test]
fn generated_runs_of_repeated_units_match_the_replaced_scan() {
    let mut sequence = Sequence(0xd1b5_4a32_d192_ed03);
    for _ in 0..30_000 {
        let mut raw = String::new();
        for _ in 0..1 + sequence.next() % 5 {
            match sequence.next() % 3 {
                0 => raw.push_str(&sequence.pick(REPEATED_UNITS).repeat(sequence.next() % 9)),
                1 => raw.push_str(sequence.pick(AFTER_REPEATED)),
                _ => raw.push_str(sequence.pick(SHAPES)),
            }
            raw.push_str(sequence.pick(&["", "", " ", "\n", ", ", "x"]));
        }
        assert_matches_the_replaced_scan(&raw);
    }
}

/// The shapes the exception exists for, each of which 0.4.1 left in the clear, beside text it must
/// not touch.
#[test]
fn the_exception_is_taken_only_where_0_4_1_left_a_credential_in_the_clear() {
    for raw in [
        "SECRET_BASIC\u{1b}[0m=hunter2",
        "TOKEN_BEARER\r: sk-live-42",
        "auth_token_bearer\0 = sk-live-42",
        "password_basic\u{1b}[0m: hunter2",
        "client_secret\u{1b}[0mauthorization=hunter2",
        "basic\u{1b}[0m://user:hunter2@host/x",
    ] {
        assert_eq!(
            agreement_with_0_4_1(raw),
            Ok(Agreement::ReadsAMarkerAsPartOfAName),
            "expected this input to differ from 0.4.1 only by redacting what it left | input {raw:?}"
        );
    }
    for raw in [
        "Authorization: Bearer\u{1b}[0m sk-live-42",
        "Authorization: Basic\rdXNlcjpwdw==",
        "x\u{1b}[0mAuthorization: Bearer sk-live-42",
        "tok\u{1b}[1men=sk-live-42",
        "SECRET_OTHER\u{1b}[0m=hunter2",
        "plain diagnostic text",
    ] {
        assert_eq!(
            agreement_with_0_4_1(raw),
            Ok(Agreement::Same),
            "expected this input to match 0.4.1 byte for byte | input {raw:?}"
        );
    }
}
