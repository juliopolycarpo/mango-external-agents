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
//! Redact before bounding. A cut that lands inside a credential can keep its head where no rule
//! recognises it: a URL with a password, cut before the `@` that closes it, has no closing `@`
//! and so no password to the URL rule. A caller that both redacts and bounds redacts the whole
//! text first, as the core does for an error's message and a notice.
//!
//! The rules (in `redact::rules`) know the shape of a credential and not its meaning, so a message
//! that merely looks like an assignment loses its value: `session refused, token: abc123` reads
//! `token=[REDACTED]` afterwards. The name and the words around it stay readable, which is what a
//! host needs of a message beside its code. What is read:
//!
//! - a name or a value in quotes, with the quotes escaped or not: `{"api_key":"sk-x"}`,
//!   `'Authorization': 'Bearer x'`. A quoted value runs to its closing quote.
//! - a keyword that starts a word, follows a capital or ends a name: `accessToken`, `PGPASSWORD`,
//!   `csrftoken`, `OPENAI_API_KEY`, `API key`.
//! - any authorization scheme, a cookie header, a `password:` phrase, a query parameter named
//!   `key`, `sig` or `auth`, and a URL userinfo that is itself a token.
//!
//! Three kinds of value are kept, because they say why something failed and are never secrets: a
//! closed set of status words after a name (`OPENAI_API_KEY: not set`, `token: expired`,
//! `failed to refresh token: 401 Unauthorized`), a number under a name that counts tokens
//! (`max_tokens: 100000`, `input_tokens=1200`), and the rest of a query string after the one
//! parameter that is a credential. Anything outside those sets is redacted: when a value could be
//! either, it is a credential. A caller holding structure redacts by key first
//! ([`structured`]), and a JSON text inside a string is read here.
//!
//! The keyword list is the one the structured walk uses, so `pwd=x` as text and `"pwd":"x"` as
//! data agree.
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
mod rules;
mod scan;
#[cfg(test)]
mod shapes;
mod strip;
mod structured;
#[cfg(test)]
mod token_shapes;
mod tokens;
#[cfg(test)]
mod work;

use std::borrow::Cow;

use scan::{Anchor, Candidates, count_steps};
pub(crate) use strip::{MAX_ESCAPE_BYTES, holds_string_terminator};
use strip::{remove_boundaries, strip_control_characters};
pub use structured::structured;

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
    read(raw, false)
}

/// What a vendor's message is shown as: [`stderr_text`], and the well-known tokens with no name
/// beside them (`ghp_...`, `AKIA...`, a web token, a private-key block) taken out as well.
///
/// The extra pass reads the whole text a byte at a time, which a stderr tail, read line after line,
/// does not pay for (`docs/benchmarks.md`); a message is short and rare.
pub(crate) fn message_text(raw: &str) -> String {
    read(raw, true)
}

/// [`stderr_text`], with the token pass when `tokens` is set.
fn read(raw: &str, tokens: bool) -> String {
    // Stripped first rather than last. A CLI that colours its output writes `Authorization:` and
    // ` Bearer sk-live-x` either side of an escape sequence, and a rule that reads the two as
    // neighbours never sees the token at all if the sequence is still sitting between them.
    let (plain, marked) = strip::strip_marking(raw);
    let mut redacted = rules_round(&plain, tokens);
    // A marker the stripper left parts a name from the word before it for the rules, and is gone
    // from what they return, so a second call over that would read the same text differently.
    // Reading it again here, until it stops changing, makes the answer one a second call leaves
    // alone. Text with no removed byte, nearly all of it, pays for one round.
    if marked {
        for _ in 0..EXTRA_ROUNDS {
            let next = rules_round(&redacted, tokens);
            if next == redacted {
                break;
            }
            redacted = next;
        }
    }
    redacted
}

/// The most times text that carried a marker is read again.
const EXTRA_ROUNDS: usize = 3;

/// The three rules over text the stripper has handled, and the markers taken out after. The token
/// pass goes first when asked: a private-key block that a name redacts the first line of would
/// otherwise leave the rest of it behind.
fn rules_round(plain: &str, tokens: bool) -> String {
    let unnamed = tokens.then(|| tokens::redact(plain)).flatten();
    let bearer = redact_bearer(unnamed.as_deref().unwrap_or(plain));
    let assignments = redact_assignments(&bearer);
    let urls = redact_url_passwords(&assignments);
    // A URL password the last rule redacted leaves `user:[REDACTED]@`, which reads as an assignment
    // when the user is a credential's name (`token:;x@` gives `token:[REDACTED]@`). Reading it once
    // more gives the text a second call would, so a call leaves its own output alone. Only text
    // that held a URL password pays for it.
    let settled = if urls == assignments {
        urls
    } else {
        redact_assignments(&urls)
    };
    remove_boundaries(settled)
}

/// [`stderr_text`] without the rounds that follow a marker: what one reading of the rules gives.
#[cfg(test)]
fn single_round(raw: &str) -> String {
    rules_round(&strip_control_characters(raw), false)
}

/// Redacts `raw` and then bounds it to `limit`: the order every vendor-written diagnostic needs.
///
/// A bound that cuts first can leave the head of a credential where no rule recognises it, so the
/// whole text is redacted and the result is cut. `truncated` says that the bound removed
/// characters from the redacted text. Characters redaction itself stripped or replaced are not
/// counted, since the text no longer holds them.
///
/// # Example
///
/// ```
/// use mango_external_agents::normalize::TextLimit;
/// use mango_external_agents::redact;
///
/// let text = redact::bounded("refused: API_KEY=sk-live-42", TextLimit::ErrorMessage);
/// assert_eq!(text.text, "refused: API_KEY=[REDACTED]");
/// assert!(!text.truncated);
/// ```
pub fn bounded(raw: &str, limit: crate::normalize::TextLimit) -> crate::normalize::BoundedText {
    crate::normalize::bound_text(&message_text(raw), limit)
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
    // The fifth continues the line, as a cut inside a quoted JSON string does. The last closes a
    // container that a credential's name opened (a bracket closes either kind): `Z]` survives
    // only when no such container was open, since a bracket nothing opened stays text.
    const PROBES: [(&str, &str); 6] = [
        ("\n=Z", "Z"),
        ("\n:bearer Z", "Z"),
        ("\nbearer Z", "Z"),
        ("\nZ", "Z"),
        (" Z", "Z"),
        ("\nZ]", "Z]"),
    ];
    // Only the two rules that read across a line break: a URL password cannot span one.
    let plain = strip_control_characters(dropped);
    // Nothing is awaited without a keyword in the text, and most tails have none.
    let names_a_credential = (0..plain.len()).any(|at| {
        count_steps(1);
        rules::names_a_keyword(plain.as_bytes(), at)
            || match_word(plain.as_bytes(), at, rules::AUTHORIZATION).is_some()
    });
    if !names_a_credential {
        return false;
    }
    PROBES.iter().any(|(probe, survives)| {
        // The text is copied once with its probe, then redacted by the rules, which count their own.
        count_steps(plain.len());
        let probed = redact_assignments(&redact_bearer(&format!("{plain}{probe}")));
        !probed.ends_with(survives)
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

/// `authorization : <scheme> <token>`, however it was spaced, cased and quoted.
///
/// Any scheme counts, `bearer` and `basic` among them: it is the same header carrying the same
/// credential, and a base64 user:password is no less a secret for being the older spelling.
fn redact_bearer(raw: &str) -> String {
    rewrite(raw, rules::header_colon, rules::bearer_rule)
}

/// `api_key=`, `secret:`, `"token": "..."`, … and whatever value follows.
fn redact_assignments(raw: &str) -> String {
    let mut query = rules::QueryRun::default();
    rewrite(raw, rules::assignment_separator, |bytes, at| {
        rules::assignment_rule(bytes, at, &mut query)
    })
}

/// The password half of `scheme://user:password@host`.
fn redact_url_passwords(raw: &str) -> String {
    rewrite(raw, rules::scheme_separator, rules::url_password_rule)
}

/// What one rule matched: where the match ends, and what stands in its place.
///
/// The replacement is the start of the match up to `kept_end`, which stays, then `joiner` and
/// [`REDACTED`]. It is described rather than built, so a match costs no allocation.
struct Rewrite {
    end: usize,
    kept_end: usize,
    joiner: &'static str,
    /// What follows the marker: the closing quote of a value whose glued tail was redacted with it.
    tail: &'static str,
}

/// What a rule makes of a place it was asked about.
enum Outcome {
    /// Redact this span.
    Redact(Rewrite),
    /// A match that is not a credential, such as `token: expired` or `max_tokens: 100000`. The scan
    /// steps over it to `end` and leaves it, so the value is read once however many starts the
    /// name had.
    Keep { end: usize },
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
    mut rule: impl FnMut(&[u8], usize) -> Option<Outcome>,
) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut candidates = Candidates::new(locate);
    let mut copied = 0;
    let mut from = 0;
    while let Some(at) = candidates.next_from(bytes, from) {
        from = at + 1;
        let found = match rule(bytes, at) {
            None => continue,
            Some(Outcome::Keep { end }) => {
                from = end;
                continue;
            }
            Some(Outcome::Redact(found)) => found,
        };
        out.push_str(&span(raw, copied, at));
        out.push_str(&as_text(bytes, at, found.kept_end));
        out.push_str(found.joiner);
        out.push_str(REDACTED);
        out.push_str(found.tail);
        copied = found.end;
        from = found.end;
    }
    out.push_str(&span(raw, copied, raw.len()));
    out
}

fn match_word(bytes: &[u8], at: usize, word: &[u8]) -> Option<usize> {
    let end = at.checked_add(word.len())?;
    bytes
        .get(at..end)?
        .eq_ignore_ascii_case(word)
        .then_some(end)
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
    use super::rules::{has_value, url_password};
    use super::{
        Anchor, Outcome, Rewrite, ends_awaiting_value, is_stripped_byte, program_name, rewrite,
        span, stderr_text,
    };

    /// The fixture a vendor child writes in the port's own process test.
    const FIXTURE: &str =
        "Authorization: Bearer top-secret API_KEY=another-secret redis://app:password@db/main";

    /// The module documents what a message that only looks like an assignment comes to, and that
    /// the words around the credential stay readable.
    #[test]
    fn an_assignment_shaped_message_keeps_its_name_and_the_words_around_it() {
        let received = stderr_text("session refused, token: abc123, sign in again");
        let expected = "session refused, token=[REDACTED], sign in again";
        assert_eq!(
            received, expected,
            "expected the words around a redacted value to stay readable: {expected:?} | received: {received:?}"
        );
    }

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
                    (at == 1).then_some(Outcome::Redact(Rewrite {
                        end: 3,
                        kept_end: 1,
                        joiner: "=",
                        tail: "",
                    }))
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
        // The last part of a name is a credential's: `mytoken` is a name, `mytokenizer` is a word.
        assert_eq!(stderr_text("mytoken=v"), "mytoken=[REDACTED]");
        assert_eq!(stderr_text("mytokenizer=v"), "mytokenizer=v");
        // A number under a name that counts tokens is a count.
        assert_eq!(stderr_text("a_token_count_of=3"), "a_token_count_of=3");
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
            "noise {\"api_key\":",
            "noise {\"Authorization\": \"Bearer",
            "noise accessToken",
            "noise Cookie:",
            // A container a credential's name opened and nothing has closed.
            "noise {\"tokens\": [",
            "noise {\"password\": [\n  \"hunter2\",",
            "noise \"credentials\": {\"user\": \"u\",\n",
            "noise tokens: [\n  sk-aaa,",
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
            // A container that closed, and one a name that is no credential's opened.
            "noise {\"tokens\": [\"a\"],",
            "noise {\"password\": [\n  \"hunter2\"\n],\n",
            "noise {\"items\": [\n  \"a\",",
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
            // Whether it is a value is the rule's to say; the locator only asks if anything stands.
            (",", true),
            ("  ;x", true),
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

#[cfg(test)]
mod bounded_tests {
    use super::bounded;
    use crate::normalize::TextLimit;

    /// Text the redactor removes or replaces is not a cut: a host learns that a message was
    /// shortened only when the bound shortened it.
    #[test]
    fn only_the_bound_marks_a_message_as_cut() {
        let limit = TextLimit::ErrorMessage.max_code_points();
        let replaced = bounded(
            "\u{1b}[31mrefused\u{1b}[0m: API_KEY=sk-live-1234567890",
            TextLimit::ErrorMessage,
        );
        assert_eq!(
            (replaced.text.as_str(), replaced.truncated),
            ("refused: API_KEY=[REDACTED]", false),
            "expected stripped and replaced text not to count as cut"
        );

        let long = bounded(&"x".repeat(limit + 10), TextLimit::ErrorMessage);
        assert!(
            long.truncated && long.text.chars().count() <= limit,
            "expected the bound to cut {} code points to at most {limit} and say so | received truncated {} with {}",
            limit + 10,
            long.truncated,
            long.text.chars().count()
        );
    }
}

#[cfg(test)]
mod corpus_tests {
    use super::stderr_text;
    use crate::testing::redaction::{BENIGN, LEAKS};

    #[test]
    fn every_leak_loses_its_credential_and_keeps_its_words() {
        for leak in LEAKS {
            let shown = stderr_text(leak.text);
            assert!(
                !shown.contains(leak.secret),
                "expected {:?} without {:?} | received: {shown:?}",
                leak.text,
                leak.secret
            );
            for kept in leak.keeps {
                assert!(
                    shown.contains(kept),
                    "expected {:?} to keep {kept:?} | received: {shown:?}",
                    leak.text
                );
            }
        }
    }

    #[test]
    fn every_benign_message_comes_back_byte_for_byte() {
        for message in BENIGN {
            let shown = stderr_text(message);
            assert_eq!(
                &shown, message,
                "expected an ordinary message unchanged | received: {shown:?}"
            );
        }
    }

    #[test]
    fn redacting_what_was_redacted_changes_nothing() {
        for text in LEAKS
            .iter()
            .map(|leak| leak.text)
            .chain(BENIGN.iter().copied())
        {
            let once = stderr_text(text);
            let twice = stderr_text(&once);
            assert_eq!(
                once, twice,
                "expected one pass to be a fixed point for {text:?}"
            );
        }
    }
}
