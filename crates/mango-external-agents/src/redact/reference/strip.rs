//! Frozen from the 0.4.1 tag: `redact/strip.rs` as it shipped, tests removed. `MAX_ESCAPE_BYTES` and
//! `holds_string_terminator` are left out: no redaction path reads them. The three entry points are
//! `pub(in crate::redact)` so `reference` can hand them to the differential suite.

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
pub(in crate::redact) fn remove_boundaries(text: String) -> String {
    if !text.contains(BOUNDARY) {
        return text;
    }
    text.replace(BOUNDARY, "")
}

/// The longest OSC, DCS, PM, SOS or APC payload taken out whole. A payload past this is not a
/// title or a link; the introducer alone is removed and the text after it is kept, to be redacted
/// like any other.
const OSC_PAYLOAD_LIMIT: usize = 4096;

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
pub(in crate::redact) fn strip_control_characters(raw: &str) -> String {
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
pub(in crate::redact) fn strip_every_character(raw: &str) -> String {
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
