//! The redactor of the release before this one, kept as the floor under the reads added since.
//!
//! These are the rules and the three passes of `redact::stderr_text` as `main` had them before the
//! well-known tokens, the deeper escaping, the containers across lines, the sequences and the block
//! scalars were added, copied rather than shared. The reads added since live in `redact::rules`
//! and change as the redactor does; this copy does not, so a change to a rule there cannot make the
//! answer here, or the answer of [`message_text`](super::message_text), hide less than the release
//! before. Only the list of credential words (`structured::CREDENTIAL_WORDS`) and the scanner
//! (`scan`) are the live ones, as the stripper (`strip`) is; they are the vocabulary of every
//! reader, and a word added to the list is a word every reader gains.
//!
//! [`stderr_text`](super::stderr_text) is this, with one more read of a URL password that a name
//! made redactable. [`message_text`](super::message_text) runs this beside the newer reads, over the
//! same text, and hides what any of them hides: see `redact::union`. [`hides`] is this as the list
//! of spans it replaces, which the union needs and the rewrite below does not build.
//!
//! The module is held to the 0.4.1 definition of the right answer (`redact::reference`) by the
//! differential tests, on every input they generate.

#[allow(dead_code)]
pub(in crate::redact) mod rules;

use std::borrow::Cow;

use super::REDACTED;
use super::scan::{Anchor, Candidates, count_steps};
use super::strip::{self, remove_boundaries, strip_control_characters};
use super::union::{Hide, apply, compose};

/// The most times text that carried a marker is read again.
pub(super) const EXTRA_ROUNDS: usize = 3;

/// The three rules over text the stripper has handled, and the markers taken out after. It answers
/// whether a URL password was redacted from a URL whose scheme or user holds a keyword: the
/// `user:[REDACTED]@` it leaves reads as an assignment, and the text then wants another round.
pub(super) fn rules_round(plain: &str) -> (String, bool) {
    let bearer = redact_bearer(plain);
    let assignments = redact_assignments(&bearer);
    let (urls, named) = redact_url_passwords(&assignments);
    // A URL password the last rule redacted leaves `user:[REDACTED]@`, which reads as an assignment
    // when the user is a credential's name (`token:;x@` gives `token:[REDACTED]@`). Reading it once
    // more gives the text a second call would, so a call leaves its own output alone. Only text
    // with such a URL pays for it: a URL with an ordinary user does not.
    let settled = if named {
        redact_assignments(&urls)
    } else {
        urls
    };
    (remove_boundaries(settled), named)
}

/// [`stderr_text`](super::stderr_text) without the rounds that follow a marker: what one reading
/// of the rules gives.
#[cfg(test)]
pub(in crate::redact) fn single_round(raw: &str) -> String {
    unsettled_round(&strip::strip_control_characters(raw))
}

/// `main`'s `stderr_text`, to the letter: what the release before this one answered.
#[cfg(test)]
pub(super) fn stderr_text(raw: &str) -> String {
    let (plain, marked) = strip::strip_marking(raw);
    let mut redacted = unsettled_round(&plain);
    if marked {
        for _ in 0..EXTRA_ROUNDS {
            let next = unsettled_round(&redacted);
            if next == redacted {
                break;
            }
            redacted = next;
        }
    }
    redacted
}

/// One round of the three rules with nothing added: `main`'s `rules_round`.
#[cfg(test)]
fn unsettled_round(plain: &str) -> String {
    let bearer = redact_bearer(plain);
    let assignments = redact_assignments(&bearer);
    remove_boundaries(redact_url_passwords(&assignments).0)
}

/// The spans of `plain`, the text the stripper left, that the three rules replace, in the rounds
/// `main` read it in. Applying them to `plain` gives `main`'s answer for the text.
///
/// `marked` is whether the stripper took a byte out: such a text is read again, as `main` did.
pub(super) fn hides(plain: &str, marked: bool) -> Vec<Hide> {
    let (mut all, mut text, _) = round_hides(plain, false);
    if marked {
        for _ in 0..EXTRA_ROUNDS {
            let (next, after, _) = round_hides(&text, false);
            if after == text {
                break;
            }
            all = compose(all, &text, next);
            text = after;
        }
    }
    all
}

/// The spans of `plain` that [`stderr_text`](super::stderr_text) replaces today: [`hides`], and the
/// reading of the assignments that follows a URL password redacted under a credential's name, and
/// the extra rounds that text gets when that happened or the stripper took a byte out. Applying
/// them to `plain` gives `stderr_text`'s answer, which is what a reader that must hide at least
/// what `stderr_text` hides (`redact::stderr_tail`) is built on.
pub(super) fn current_hides(plain: &str, marked: bool) -> Vec<Hide> {
    let (mut all, mut text, reread) = round_hides(plain, true);
    if marked || reread {
        for _ in 0..EXTRA_ROUNDS {
            let (next, after, _) = round_hides(&text, true);
            if after == text {
                break;
            }
            all = compose(all, &text, next);
            text = after;
        }
    }
    all
}

/// One round as spans of `plain`, the text it leaves, and whether a URL password was redacted
/// under a credential's name. With `settle`, the assignments are read once more after the URL
/// passwords when one was, as [`rules_round`] does.
fn round_hides(plain: &str, settle: bool) -> (Vec<Hide>, String, bool) {
    let mut bearer = Vec::new();
    let after_bearer = bearer_stage(plain, Some(&mut bearer));
    let mut assignments = Vec::new();
    let after_assignments = assignment_stage(&after_bearer, Some(&mut assignments));
    let mut urls = Vec::new();
    let (after_urls, named) = url_stage(&after_assignments, Some(&mut urls));
    let mut again = Vec::new();
    let settled = if settle && named {
        assignment_stage(&after_urls, Some(&mut again))
    } else {
        after_urls.clone()
    };
    let marks = boundary_hides(&settled);
    let after = apply(&settled, &marks);
    let first = compose(bearer, &after_bearer, assignments);
    let second = compose(first, &after_assignments, urls);
    let third = compose(second, &after_urls, again);
    (compose(third, &settled, marks), after, named)
}

/// The bytes the stripper left to part words, which the rules take out when they are done.
fn boundary_hides(text: &str) -> Vec<Hide> {
    if !text.bytes().any(|byte| byte == strip::BOUNDARY_BYTE) {
        return Vec::new();
    }
    text.bytes()
        .enumerate()
        .filter(|(_, byte)| *byte == strip::BOUNDARY_BYTE)
        .map(|(at, _)| Hide::taken_out(at, at + 1))
        .collect()
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
pub(super) fn ends_awaiting_value(dropped: &str) -> bool {
    // Each probe finishes one waiting state: a name that wants its separator, a separator that
    // wants its value, `Authorization` that wants `:`, `Authorization:` that wants a scheme and a
    // scheme that wants its token. `Z` is the value; it survives only when nothing claimed it.
    // The last continues the line, as a cut inside a quoted JSON string does.
    const PROBES: [&str; 5] = ["\n=Z", "\n:bearer Z", "\nbearer Z", "\nZ", " Z"];
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
    PROBES.iter().any(|probe| {
        // The text is copied once with its probe, then redacted by the rules, which count their own.
        count_steps(plain.len());
        let probed = redact_assignments(&redact_bearer(&format!("{plain}{probe}")));
        !probed.ends_with('Z')
    })
}

/// `authorization : <scheme> <token>`, however it was spaced, cased and quoted.
///
/// Any scheme counts, `bearer` and `basic` among them: it is the same header carrying the same
/// credential, and a base64 user:password is no less a secret for being the older spelling.
pub(in crate::redact) fn redact_bearer(raw: &str) -> String {
    bearer_stage(raw, None)
}

/// `api_key=`, `secret:`, `"token": "..."`, … and whatever value follows.
pub(in crate::redact) fn redact_assignments(raw: &str) -> String {
    assignment_stage(raw, None)
}

/// The password half of `scheme://user:password@host`.
pub(in crate::redact) fn redact_url_passwords(raw: &str) -> (String, bool) {
    url_stage(raw, None)
}

// The three stages are the one place each rule is scanned from: the rewrite that gives the text
// and the one that also gives the spans are the same code, so the scan is compiled once, as it
// was before the spans existed, and a tail pays for nothing the spans cost.

#[inline(never)]
fn bearer_stage(raw: &str, spans: Option<&mut Vec<Hide>>) -> String {
    rewrite_recording(raw, rules::header_colon, rules::bearer_rule, spans)
}

#[inline(never)]
fn assignment_stage(raw: &str, spans: Option<&mut Vec<Hide>>) -> String {
    let mut query = rules::QueryRun::default();
    rewrite_recording(
        raw,
        rules::assignment_separator,
        |bytes, at| rules::assignment_rule(bytes, at, &mut query),
        spans,
    )
}

#[inline(never)]
fn url_stage(raw: &str, spans: Option<&mut Vec<Hide>>) -> (String, bool) {
    let mut named = false;
    let text = rewrite_recording(
        raw,
        rules::scheme_separator,
        |bytes, at| {
            let outcome = rules::url_password_rule(bytes, at);
            if let Some(Outcome::Redact(found)) = &outcome {
                named = named || holds_a_keyword(bytes, at, found.kept_end);
            }
            outcome
        },
        spans,
    );
    (text, named)
}

/// What one rule matched: where the match ends, and what stands in its place.
///
/// The replacement is the start of the match up to `kept_end`, which stays, then `joiner` and
/// [`REDACTED`]. It is described rather than built, so a match costs no allocation.
pub(in crate::redact) struct Rewrite {
    pub(in crate::redact) end: usize,
    pub(in crate::redact) kept_end: usize,
    pub(in crate::redact) joiner: &'static str,
    /// What follows the marker: the closing quote of a value whose glued tail was redacted with it.
    pub(in crate::redact) tail: &'static str,
}

/// What a rule makes of a place it was asked about.
pub(in crate::redact) enum Outcome {
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
#[cfg(test)]
pub(in crate::redact) fn rewrite(
    raw: &str,
    locate: impl Fn(&[u8], usize) -> Option<Anchor>,
    rule: impl FnMut(&[u8], usize) -> Option<Outcome>,
) -> String {
    rewrite_recording(raw, locate, rule, None)
}

/// [`rewrite`], and the spans it replaces when `spans` asks for them.
#[inline]
fn rewrite_recording(
    raw: &str,
    locate: impl Fn(&[u8], usize) -> Option<Anchor>,
    mut rule: impl FnMut(&[u8], usize) -> Option<Outcome>,
    mut spans: Option<&mut Vec<Hide>>,
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
        if let Some(spans) = spans.as_deref_mut() {
            spans.push(Hide::written(
                found.kept_end,
                found.end,
                found.joiner,
                found.tail,
            ));
        }
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
fn is_space_byte(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

pub(in crate::redact) fn skip_spaces(bytes: &[u8], at: usize) -> usize {
    take_while(bytes, at, is_space_byte)
}

pub(in crate::redact) fn take_while(bytes: &[u8], at: usize, keep: impl Fn(u8) -> bool) -> usize {
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
pub(in crate::redact) fn span(raw: &str, from: usize, to: usize) -> Cow<'_, str> {
    match raw.get(from..to) {
        Some(text) => Cow::Borrowed(text),
        None => as_text(raw.as_bytes(), from, to),
    }
}

/// `bytes[from..to]` as text, borrowed unless it is not valid UTF-8.
pub(in crate::redact) fn as_text(bytes: &[u8], from: usize, to: usize) -> Cow<'_, str> {
    String::from_utf8_lossy(bytes.get(from..to).unwrap_or_default())
}

/// Whether a keyword starts anywhere in `bytes[from..to]`: the userinfo of a URL, say.
pub(in crate::redact) fn holds_a_keyword(bytes: &[u8], from: usize, to: usize) -> bool {
    (from..to).any(|at| rules::names_a_keyword(bytes, at))
}
