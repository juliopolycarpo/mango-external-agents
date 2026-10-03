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

use super::scan::first_match;
use super::{AUTHORIZATION, is_unsafe_to_render, match_credential_keyword, match_word};

/// Marks where a removed byte stood in front of a credential name. Every C0 control is stripped
/// from the input, so it cannot already be in the text; it is not a letter, so a name after it
/// starts a word, and it is not a space, so it does not end a value.
const BOUNDARY: char = '\u{1}';

/// [`BOUNDARY`] as a byte, for the rules that skip the gap between a scheme and its token.
pub(super) const BOUNDARY_BYTE: u8 = 0x01;

/// The text with every [`BOUNDARY`] taken out, once the rules have run.
pub(super) fn remove_boundaries(text: String) -> String {
    if !text.contains(BOUNDARY) {
        return text;
    }
    text.replace(BOUNDARY, "")
}

/// The longest OSC, DCS, PM, SOS or APC payload taken out whole. A payload past this is not a
/// title or a link; the introducer alone is removed and the text after it is kept, to be redacted
/// like any other.
const OSC_PAYLOAD_LIMIT: usize = 4096;

/// The most raw bytes one string escape can take out: the longest payload, its introducer and its
/// terminator. A caller that looks back over text the redactor will strip needs this much reach
/// to see a name on the far side of one.
pub(crate) const MAX_ESCAPE_BYTES: usize = OSC_PAYLOAD_LIMIT + 8;

/// Whether `bytes` holds something that ends a string escape: a BEL, `ESC \` or the 8-bit ST.
/// Text with none of them has no string escape ending in it, however long the escape was.
pub(crate) fn holds_string_terminator(bytes: &[u8]) -> bool {
    bytes.iter().enumerate().any(|(at, byte)| match byte {
        0x07 => true,
        0x1b => bytes.get(at + 1) == Some(&b'\\'),
        0xc2 => bytes.get(at + 1) == Some(&0x9c),
        _ => false,
    })
}

/// The byte after `ESC` that opens a DCS, SOS, PM or APC string: `ESC P`, `ESC X`, `ESC ^` and
/// `ESC _`. Each runs to an ST.
const STRING_INTRODUCERS: &[u8] = b"PX^_";

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
    loop {
        // Nothing is removed from, or marked in, text that follows a kept character and holds
        // no byte a removed character starts with, so it is copied whole.
        if !removed {
            let end = clean_end(bytes, at);
            out.push_str(&raw[at..end]);
            at = end;
        }
        let Some(character) = raw.get(at..).and_then(|rest| rest.chars().next()) else {
            break;
        };
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
            && (starts_credential_name(bytes, at) || ends_with_scheme(&out))
        {
            out.push(BOUNDARY);
        }
        removed = false;
        out.push(character);
        at = after;
    }
    out
}

/// [`strip_control_characters`] as it was before clean text was copied whole: every character
/// read and pushed on its own. Kept as the definition of the right answer for
/// `redact::differential`.
#[cfg(test)]
pub(super) fn strip_every_character(raw: &str) -> String {
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
            && (starts_credential_name(bytes, at) || ends_with_scheme(&out))
        {
            out.push(BOUNDARY);
        }
        removed = false;
        out.push(character);
        at = after;
    }
    out
}

/// Where the text from `at` first holds a byte a removed character can start with: the end of
/// what [`strip_control_characters`] may copy without reading it a character at a time.
fn clean_end(bytes: &[u8], at: usize) -> usize {
    // A kept character that shares a lead byte with a removed one seldom comes alone (a drawn
    // table is a run of them), and a block search that stops where it started only adds work.
    if bytes
        .get(at)
        .is_some_and(|byte| starts_a_removed_character(*byte))
    {
        return at;
    }
    first_match(bytes, at, starts_a_removed_character).unwrap_or(bytes.len())
}

/// Whether a byte can be the first of a character [`strip_control_characters`] removes: a C0
/// control or DEL as itself, and the lead byte of each multi-byte sequence that holds one (`C2`
/// for the C1 controls, `D8` for U+061C, `E2` for the marks, embeddings and isolates). Every
/// escape starts with one of them. Tab and line feed are kept, so they are not counted; a kept
/// character can share a lead byte (an em dash leads with `E2`), and the caller then decides with
/// the exact per-character test.
///
/// Comparisons joined by `|` and `&`, as [`first_match`] needs.
fn starts_a_removed_character(byte: u8) -> bool {
    let control = (byte < 0x20) & (byte != b'\t') & (byte != b'\n');
    control | (byte == 0x7f) | (byte == 0xc2) | (byte == 0xd8) | (byte == 0xe2)
}

/// Whether a credential's name starts at `at`: `api_key` or one of the keywords, or the
/// `Authorization` header.
fn starts_credential_name(bytes: &[u8], at: usize) -> bool {
    match_credential_keyword(bytes, at).is_some() || match_word(bytes, at, AUTHORIZATION).is_some()
}

/// Whether `text` ends in an authorization scheme, `Bearer` or `Basic`, whose token the bearer
/// rule reads after a gap. A removed byte in that gap is a boundary, not a join.
fn ends_with_scheme(text: &str) -> bool {
    let bytes = text.as_bytes();
    [&b"bearer"[..], b"basic"].iter().any(|scheme| {
        bytes
            .len()
            .checked_sub(scheme.len())
            .and_then(|from| bytes.get(from..))
            .is_some_and(|tail| tail.eq_ignore_ascii_case(scheme))
    })
}

/// Where the escape that starts at `at` ends, or `None` when `character` does not start one.
fn escape_end(bytes: &[u8], at: usize, character: char) -> Option<usize> {
    let after = at + character.len_utf8();
    match character {
        '\u{1b}' => Some(after_escape(bytes, after)),
        '\u{9b}' => Some(csi_end(bytes, after)),
        '\u{9d}' => Some(string_end(bytes, after, true).unwrap_or(after)),
        '\u{90}' | '\u{98}' | '\u{9e}' | '\u{9f}' => {
            Some(string_end(bytes, after, false).unwrap_or(after))
        }
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
        return string_end(bytes, start + 1, true).unwrap_or(start + 1);
    }
    if STRING_INTRODUCERS.contains(next) {
        // Unterminated, only `ESC` goes if the letter begins a name: `ESC PASSWORD=x`.
        return string_end(bytes, start + 1, false)
            .unwrap_or_else(|| pick(bytes, start, start + 1));
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

/// The end of an OSC, DCS, SOS, PM or APC string whose introducer ends at `start`: everything up
/// to an ST (`ESC \` or the 8-bit `0x9c`), or a BEL as well when `bel_ends` is set, which is an
/// OSC's own way to end. `None` when the string does not end.
///
/// A string is not taken out when no terminator comes within [`OSC_PAYLOAD_LIMIT`] bytes, or when
/// a line feed or another escape comes first. Only its introducer goes then, so a stray one cannot
/// hide the lines after it. A title or a link has no line feed in it. The scan ends at the next
/// escape, so the work over a whole text stays linear.
fn string_end(bytes: &[u8], start: usize, bel_ends: bool) -> Option<usize> {
    let limit = bytes
        .len()
        .min(start.saturating_add(OSC_PAYLOAD_LIMIT).saturating_add(1));
    let mut at = start;
    while at < limit {
        match bytes[at] {
            0x07 if bel_ends => return Some(at + 1),
            b'\n' => return None,
            0x1b if bytes.get(at + 1) == Some(&b'\\') => return Some(at + 2),
            0x1b => return None,
            0xc2 => match bytes.get(at + 1) {
                Some(0x9c) => return Some(at + 2),
                Some(0x80..=0x9b | 0x9d..=0x9f) => return None,
                _ => at += 1,
            },
            _ => at += 1,
        }
    }
    None
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

    /// The clean-text copy skips every character whose first byte is not counted, so each
    /// character the stripper removes, and each one that starts an escape, has to be counted.
    #[test]
    fn every_removed_character_starts_with_a_counted_byte() {
        for character in (0..=u32::from(char::MAX)).filter_map(char::from_u32) {
            let mut buffer = [0; 4];
            let encoded = character.encode_utf8(&mut buffer).as_bytes();
            let kept =
                character == '\t' || character == '\n' || !super::is_unsafe_to_render(character);
            let removed = !kept || super::escape_end(encoded, 0, character).is_some();
            assert!(
                !removed || super::starts_a_removed_character(encoded[0]),
                "expected the first byte of removed U+{:04X} counted | received {:#04x} not \
                 counted",
                u32::from(character),
                encoded[0]
            );
        }
        for byte in [b'\t', b'\n', b' ', b'a', b'~', 0x80, 0xc3, 0xe1, 0xe3, 0xff] {
            assert!(
                !super::starts_a_removed_character(byte),
                "expected {byte:#04x} not counted, so text made of it is copied whole | received \
                 counted"
            );
        }
    }

    #[test]
    fn clean_text_ends_at_the_first_counted_byte_or_the_end_of_the_text() {
        for (text, at, expected) in [
            ("plain\ttext\n", 0, 11),
            ("ab\u{1b}[0m", 0, 2),
            ("ab\u{1b}[0m", 2, 2),
            ("ab\u{1b}[0m", 3, 6),
            ("é—x", 0, 2),
            ("", 0, 0),
            ("abc", 3, 3),
        ] {
            let received = super::clean_end(text.as_bytes(), at);
            assert_eq!(
                received, expected,
                "expected the clean text of {text:?} from {at} to end at {expected} | received \
                 {received}"
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

    /// A stray introducer must not take a diagnostic with it: a line break is not part of a title
    /// or a link, so the scan for a terminator ends there.
    #[test]
    fn an_osc_introducer_does_not_hide_lines_up_to_a_stray_bell() {
        assert_eq!(
            stderr_text("\u{1b}]\nERROR: real failure\n\u{7}"),
            "\nERROR: real failure\n",
            "expected the lines between a stray introducer and a later BEL kept"
        );
    }

    #[test]
    fn an_osc_payload_of_exactly_the_limit_is_still_terminated() {
        let payload = "x".repeat(super::OSC_PAYLOAD_LIMIT);
        for terminator in ["\u{7}", "\u{1b}\\", "\u{9c}"] {
            let redacted = stderr_text(&format!("\u{1b}]{payload}{terminator}kept"));
            assert_eq!(
                redacted,
                "kept",
                "expected a payload of exactly {} bytes ended by {terminator:?} taken out | received {} bytes",
                super::OSC_PAYLOAD_LIMIT,
                redacted.len()
            );
        }
    }

    #[test]
    fn a_scheme_and_its_token_parted_only_by_a_removed_byte_are_still_redacted() {
        assert_hidden(
            &[
                "Authorization: Bearer\rsk-live-secret",
                "Authorization: Bearer\u{1b}[0msk-live-secret",
                "Authorization: bearer\u{7}sk-live-secret",
                "Authorization: Basic\u{1b}[0msecret-value",
                "Authorization:\rBearer\rsk-live-secret",
            ],
            "a scheme parted from its token by a removed byte",
        );
        assert_eq!(
            stderr_text("Authorization: Bearer\rsk-live-x"),
            "Authorization: Bearer [REDACTED]"
        );
    }

    #[test]
    fn dcs_pm_sos_and_apc_strings_are_taken_out_like_an_osc_but_end_only_at_st() {
        assert_hidden(
            &[
                "\u{1b}P1$r TOKEN=secret \u{1b}\\",
                "\u{1b}P1$rTOKEN=secret\u{1b}\\",
                "\u{90}1$rTOKEN=secret\u{9c}",
                "\u{1b}^TOKEN=secret\u{1b}\\",
                "\u{1b}_password=secret\u{1b}\\",
                "\u{1b}Xtoken=secret\u{1b}\\",
                "\u{90}1$r TOKEN=secret \u{9c}",
                "\u{9e}TOKEN=secret\u{9c}",
                "\u{98}TOKEN=secret\u{9c}",
                "\u{9f}TOKEN=secret\u{9c}",
                // Not ended by BEL, so only the introducer goes, and what follows is redacted.
                "\u{1b}Pabc\u{7}TOKEN=secret",
                // A name that begins with the byte after `ESC` keeps it.
                "\u{1b}PASSWORD=secret",
                "\u{1b}Xtoken=secret",
            ],
            "a DCS, PM, SOS or APC string",
        );
        assert_eq!(stderr_text("\u{1b}P1$rdata\u{1b}\\shown"), "shown");
        assert_eq!(
            stderr_text("\u{1b}P\nERROR: real failure\n\u{1b}\\"),
            "\nERROR: real failure\n",
            "expected a line break to end the scan as it does for an OSC"
        );
    }

    #[test]
    fn a_string_terminator_is_found_wherever_it_ends_a_string_escape() {
        for (bytes, expected) in [
            (&b"title\x07"[..], true),
            (b"title\x1b\\", true),
            ("title\u{9c}".as_bytes(), true),
            (b"plain text", false),
            (b"colour \x1b[31m", false),
            (b"\x1b", false),
            (b"", false),
        ] {
            assert_eq!(
                super::holds_string_terminator(bytes),
                expected,
                "expected a terminator verdict of {expected} | input {bytes:?}"
            );
        }
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
