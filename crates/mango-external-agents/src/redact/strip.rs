//! Taking terminal control text out ahead of the credential rules.
//!
//! The rules find a credential by its name, and a name is only recognised at the start of a word.
//! What is removed here therefore has to leave the text either side of it as a reader would see it
//! after a terminal drew it: a name is not to be joined onto a letter that used to sit before an
//! escape, and not to lose its first letter to a sequence that was never one. Both are decided at
//! each escape on its own, by asking whether a credential name starts at the place each reading
//! ends.
//!
//! One pass over the text. Nothing here looks back, and a scan for an OSC terminator stops at the
//! next escape, so the work is linear in the length of the text.

use super::{is_unsafe_to_render, match_credential_keyword, match_word};

/// Marks where a removed byte stood in front of a credential name. Every C0 control is stripped
/// from the input, so it cannot already be in the text; it is not a letter, so a name after it
/// starts a word, and it is not a space, so it does not end a value.
const BOUNDARY: char = '\u{1}';

/// The text with every [`BOUNDARY`] taken out, once the rules have run.
pub(super) fn remove_boundaries(text: String) -> String {
    if !text.contains(BOUNDARY) {
        return text;
    }
    text.replace(BOUNDARY, "")
}

/// The longest OSC payload taken out whole. A payload past this is not a title or a link; the
/// introducer alone is removed and the text after it is kept, to be redacted like any other.
const OSC_PAYLOAD_LIMIT: usize = 4096;

/// The byte after `ESC` in the two-byte forms a terminal program writes: save and restore cursor,
/// the keypad modes, reset, the line and tab movers, and ST.
const TWO_BYTE_FINALS: &[u8] = b"78=>cDEHMZ\\";

/// The byte after `ESC` that opens a character-set designation, `ESC ( B` and its kin.
const CHARSET_INTRODUCERS: &[u8] = b"()*+";

/// Keeps tab and newline, drops every other C0 control, DEL, the C1 block and every bidirectional
/// formatting character, and takes a complete escape sequence out whole rather than leaving its
/// parameters behind as text.
///
/// A lone `\r` or an escape sequence in a vendor's diagnostic is a terminal-rendering problem the
/// moment anyone tails a log. Dropping only the `ESC` would leave `[31m` sitting in the middle of
/// a header, which reads as noise and hides a token from the rules that run after this.
///
/// The bidirectional set goes for the reason it goes everywhere else in this crate — see
/// [`normalize::is_strippable`](crate::normalize) — and this tail is rendered in a host's
/// diagnostics like any other vendor-written string, so the answer has to be the same one.
///
/// A byte removed just before a credential name leaves a [`BOUNDARY`] in its place when the
/// character before it is a letter or digit, because the name would otherwise be joined onto that
/// character and no longer start a word. Text that is not followed by a name is joined as it
/// always was, so a name a colour code split in two still reads as one. The boundary is not a
/// space: a value the rules are reading goes on through it, and a half-credential after a removed
/// byte is not left in the clear. [`remove_boundaries`] takes it out of the redacted text.
pub(super) fn strip_control_characters(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut at = 0;
    let mut removed = false;
    while let Some(character) = raw.get(at..).and_then(|rest| rest.chars().next()) {
        let after = at + character.len_utf8();
        if let Some(end) = escape_end(bytes, at, character) {
            at = end;
            removed = true;
            continue;
        }
        let kept = character == '\t' || character == '\n' || !is_unsafe_to_render(character);
        if !kept {
            at = after;
            removed = true;
            continue;
        }
        if removed
            && out.ends_with(|last: char| last.is_ascii_alphanumeric())
            && starts_credential_name(bytes, at)
        {
            out.push(BOUNDARY);
        }
        removed = false;
        out.push(character);
        at = after;
    }
    out
}

/// Whether a credential's name starts at `at`: `api_key` or one of the keywords, or the
/// `Authorization` header.
fn starts_credential_name(bytes: &[u8], at: usize) -> bool {
    match_credential_keyword(bytes, at).is_some()
        || match_word(bytes, at, b"authorization").is_some()
}

/// Where the escape that starts at `at` ends, or `None` when `character` does not start one.
fn escape_end(bytes: &[u8], at: usize, character: char) -> Option<usize> {
    match character {
        '\u{1b}' => Some(after_escape(bytes, at + 1)),
        '\u{9b}' => Some(csi_end(bytes, at + character.len_utf8())),
        '\u{9d}' => Some(osc_end(bytes, at + character.len_utf8())),
        _ => None,
    }
}

/// The end of an escape whose `ESC` ends at `start`. An `ESC` followed by anything that is not a
/// form known here is removed alone.
fn after_escape(bytes: &[u8], start: usize) -> usize {
    let Some(next) = bytes.get(start) else {
        return start;
    };
    if *next == b'[' {
        return csi_end(bytes, start + 1);
    }
    if *next == b']' {
        return osc_end(bytes, start + 1);
    }
    if CHARSET_INTRODUCERS.contains(next) {
        // The designator is one byte from `0x30..=0x7e`, `B` for ASCII and `0` for line drawing.
        return pick(bytes, start + 1, final_end(bytes, start + 1, 0x30..=0x7e));
    }
    if TWO_BYTE_FINALS.contains(next) {
        return pick(bytes, start, start + 1);
    }
    start
}

/// The end of one byte in `range` at `at`, or `at` when the byte is not in it.
fn final_end(bytes: &[u8], at: usize, range: std::ops::RangeInclusive<u8>) -> usize {
    match bytes.get(at) {
        Some(byte) if range.contains(byte) => at + 1,
        _ => at,
    }
}

/// The end of a CSI whose introducer ends at `start`: parameter and intermediate bytes
/// (`0x20..=0x3f`), then one final byte (`0x40..=0x7e`).
///
/// The standard puts every parameter byte before every intermediate one. A sequence that does not
/// (`ESC [ SP 1 m`) is malformed, and it still ends at its final byte here: stopping at the first
/// misplaced byte would leave `1m` in front of whatever follows and glue it onto a name. A byte
/// outside that range ends the sequence and is left in place, so a line break after `ESC [` cannot
/// carry the sequence on into the next word.
fn csi_end(bytes: &[u8], start: usize) -> usize {
    let mut at = start;
    while bytes
        .get(at)
        .is_some_and(|byte| (0x20..=0x3f).contains(byte))
    {
        at += 1;
    }
    pick(bytes, at, final_end(bytes, at, 0x40..=0x7e))
}

/// The end of an OSC whose introducer ends at `start`: everything up to a BEL or an ST, `ESC \`
/// or the 8-bit `0x9c`.
///
/// An OSC with no terminator inside [`OSC_PAYLOAD_LIMIT`] bytes, or one that meets another
/// escape first, is not taken out: only its introducer is, so an unterminated title cannot hide
/// the rest of the output.
fn osc_end(bytes: &[u8], start: usize) -> usize {
    let limit = bytes.len().min(start.saturating_add(OSC_PAYLOAD_LIMIT));
    let mut at = start;
    while at < limit {
        match bytes[at] {
            0x07 => return at + 1,
            0x1b if bytes.get(at + 1) == Some(&b'\\') => return at + 2,
            0x1b => return start,
            0xc2 if bytes.get(at + 1) == Some(&0x9c) => return at + 2,
            0xc2 if matches!(bytes.get(at + 1), Some(0x9b | 0x9d)) => return start,
            _ => at += 1,
        }
    }
    start
}

/// Chooses where an escape ends when its last byte might be the first letter of a name.
///
/// `intro_end` is the end without that byte and `full_end` the end with it. The byte is left in
/// when a credential name starts there and does not start after it: `ESC [ API_KEY=x` is a broken
/// sequence and a name, `ESC [ 1 m API_KEY=x` is a sequence and a name. Anywhere else the whole
/// sequence goes, as the standard has it.
fn pick(bytes: &[u8], intro_end: usize, full_end: usize) -> usize {
    if full_end > intro_end
        && starts_credential_name(bytes, intro_end)
        && !starts_credential_name(bytes, full_end)
    {
        return intro_end;
    }
    full_end
}

#[cfg(test)]
mod tests {
    use crate::redact::stderr_text;

    /// Redacts each of `inputs` and fails naming the one that showed `secret`.
    fn assert_hidden(inputs: &[&str], what: &str) {
        for raw in inputs {
            let redacted = stderr_text(raw);
            assert!(
                !redacted.contains("secret"),
                "expected the value redacted after {what} | input {raw:?} received {redacted:?}"
            );
        }
    }

    #[test]
    fn a_broken_introducer_does_not_hide_a_credential_name() {
        assert_hidden(
            &[
                "\u{1b}[ API_KEY=secret",
                "\u{1b}[\nAPI_KEY=secret",
                "\u{1b}[1;\nAPI_KEY=secret",
                "\u{1b}[\u{7}TOKEN=secret",
                "\u{1b}[API_KEY=secret",
                "\u{1b}[1;TOKEN=secret",
                "\u{1b}[ 1mAPI_KEY=secret",
                "\u{1b}[ 1API_KEY=secret",
                "\u{1b}[1 2;mTOKEN=secret",
            ],
            "an escape sequence broken by the name that follows",
        );
    }

    #[test]
    fn a_complete_sequence_with_an_intermediate_byte_does_not_hide_a_name() {
        assert_hidden(
            &[
                "\u{1b}[ qAPI_KEY=secret",
                "\u{1b}[0 qTOKEN=secret",
                "\u{1b}[1;2 @password=secret",
            ],
            "a complete sequence with an intermediate byte",
        );
        assert_eq!(
            stderr_text("\u{1b}[0 qready"),
            "ready",
            "expected a complete intermediate sequence taken out whole"
        );
    }

    #[test]
    fn the_reading_is_chosen_at_each_escape_on_its_own() {
        assert_hidden(
            &[
                "\u{1b}[ qAPI_KEY=secret\n\u{1b}[ API_KEY=secret",
                "\u{1b}[ API_KEY=secret\n\u{1b}[ qAPI_KEY=secret",
                "\u{1b}[ qTOKEN=secret \u{1b}[ TOKEN=secret password=secret",
                "\u{1b}[31mAPI_KEY=secret \u{1b}[31API_KEY=secret \u{1b}[API_KEY=secret",
            ],
            "two escapes that each need a different reading",
        );
    }

    #[test]
    fn other_escape_forms_do_not_join_or_strand_a_letter_before_a_name() {
        assert_hidden(
            &[
                "\u{1b}(BAPI_KEY=secret",
                "\u{1b}(API_KEY=secret",
                "\u{1b}cAPI_KEY=secret",
                "\u{1b}7API_KEY=secret",
                "\u{1b}8TOKEN=secret",
                "\u{1b}=password=secret",
                "\u{1b}credential=secret",
            ],
            "a character-set or two-byte escape",
        );
    }

    #[test]
    fn an_eight_bit_introducer_is_taken_out_like_its_seven_bit_form() {
        assert_hidden(
            &[
                "\u{9b}1mAPI_KEY=secret",
                "\u{9b}API_KEY=secret",
                "\u{9d}0;title\u{7}API_KEY=secret",
            ],
            "an 8-bit introducer",
        );
        assert_eq!(stderr_text("\u{9b}31mred"), "red");
    }

    #[test]
    fn a_removed_escape_is_a_boundary_before_a_name_and_only_there() {
        assert_hidden(
            &[
                "x\u{1b}[31mAPI_KEY=secret",
                "path\u{7}TOKEN=secret",
                "x\u{9b}mpassword=secret",
            ],
            "a letter joined onto a name by a removed escape",
        );
        assert_eq!(
            stderr_text("x\u{1b}[31mAPI_KEY=secret"),
            "xAPI_KEY=[REDACTED]"
        );
        // A name a colour code split in two, and text with no name after the escape, still join.
        assert_eq!(stderr_text("API\u{1b}[0m_KEY=secret"), "API_KEY=[REDACTED]");
        assert_eq!(stderr_text("a\u{1b}[31mb"), "ab");
        assert_eq!(stderr_text("sec\rret=hunter2"), "secret=[REDACTED]");
    }

    /// A boundary must not end a value: the half of a credential after a removed byte is still
    /// the credential.
    #[test]
    fn a_value_goes_on_through_a_removed_byte_before_a_name() {
        assert_hidden(
            &[
                "OPENAI_API_KEY=sk-proj-AAAA\rsecretpart",
                "TOKEN=abc\u{1b}[0msecretXYZ",
                "Authorization: Bearer abc\u{7}tokenXYZ",
            ],
            "a value split by a removed byte in front of a keyword",
        );
        let redacted = stderr_text("OPENAI_API_KEY=sk-proj-AAAA\rsecretpart");
        assert_eq!(
            redacted, "OPENAI_API_KEY=[REDACTED]",
            "expected the whole value hidden, without a boundary marker | received {redacted:?}"
        );
    }

    #[test]
    fn an_osc_is_taken_out_up_to_its_terminator_and_no_further() {
        assert_hidden(
            &[
                "\u{1b}]0;title\u{7}API_KEY=secret",
                "\u{1b}]0;title\u{1b}\\API_KEY=secret",
                "\u{1b}]0;title\u{9c}API_KEY=secret",
                "\u{1b}]8;;http://example\u{7}link\u{1b}]8;;\u{7}TOKEN=secret",
            ],
            "an OSC",
        );
        assert_eq!(stderr_text("\u{1b}]0;title\u{7}shown"), "shown");
        // No terminator: only the introducer goes, and the rest stays visible and redacted.
        assert_eq!(
            stderr_text("\u{1b}]0;API_KEY=secret"),
            "0;API_KEY=[REDACTED]"
        );
        let payload = "x".repeat(super::OSC_PAYLOAD_LIMIT + 8);
        assert_eq!(
            stderr_text(&format!("\u{1b}]{payload}\u{7}kept")),
            format!("{payload}kept"),
            "expected an over-long payload kept, with only the introducer and terminator removed"
        );
    }

    #[test]
    fn a_complete_sequence_is_still_taken_out_whole() {
        assert_eq!(
            stderr_text("\u{1b}[1;31mred\u{1b}[0m and \u{1b}[2Kdone\u{1b}[31"),
            "red and done"
        );
    }

    #[test]
    fn work_stays_linear_when_escapes_never_terminate() {
        let text = "\u{1b}]0;".repeat(20_000);
        let started = std::time::Instant::now();
        let redacted = stderr_text(&text);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "expected repeated unterminated OSC introducers in linear time | received {:?} for {} bytes ({} kept)",
            started.elapsed(),
            text.len(),
            redacted.len()
        );
    }
}
