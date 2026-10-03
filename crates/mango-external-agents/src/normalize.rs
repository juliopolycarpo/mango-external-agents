//! Bounds for text a vendor process produced.
//!
//! Everything an external agent emits is attacker-adjacent in the ordinary sense: it is text the
//! host did not write, rendered in the host's interface and persisted in the host's database.
//! "Cap the length" is not a specification, so the caps live here as numbers and are applied once,
//! at the boundary, by the one [`EventSink`](crate::EventSink) a turn's events pass through.
//!
//! Three things happen, in this order:
//!
//! 1. Control characters are stripped — C0 and C1 both, keeping only tab and newline, because a
//!    lone `\r` or an escape sequence in a "tool name" is a terminal-rendering problem the moment
//!    anyone tails a log.
//! 2. Bidirectional formatting characters are stripped. They let a string render in an order its
//!    code points do not have, which is exactly how a benign-looking command label hides what it
//!    will run.
//! 3. What is left is cut to a **code-point** count, never to bytes: cutting a multi-byte
//!    character in half produces something no encoder can represent.
//!
//! Rust's `char` cannot hold an unpaired surrogate, so the lone-surrogate rule the TypeScript
//! original needed has no counterpart here: `serde_json` has already refused the input by then.

use crate::error::{Error, Result};

/// Every bounded field, and how many code points it may keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum TextLimit {
    /// The vendor's own tool name, rendered as an activity's label.
    ActivityName,
    /// An activity title or an approval title.
    Title,
    /// An activity detail or an approval detail.
    Detail,
    /// Label text the vendor supplied for one approval option.
    ApprovalOptionLabel,
    /// The title of a vendor session, when one is listed or adopted.
    SessionTitle,
    /// A failure message.
    ErrorMessage,
    /// An opaque vendor identifier: session, call, request and option ids, and vendor error codes.
    ///
    /// They cross the wire and are echoed back to the vendor, so they are bounded on the same
    /// terms as rendered text — and, unlike text, refused rather than cut when they do not fit.
    VendorId,
    /// A minimal account display label. Never a raw email address.
    AccountLabel,
    /// A slash command's invocable name, without its leading `/`.
    CommandName,
    /// One line of help for a slash command, as the vendor wrote it.
    ///
    /// Wider than a title because this is prose the vendor chose for a picker, not a label it
    /// derived: some CLIs ship descriptions past 250 code points, and cutting them at a title's
    /// length would truncate the half that says what the command does.
    CommandDescription,
}

impl TextLimit {
    /// How many code points this field may keep.
    pub const fn max_code_points(self) -> usize {
        match self {
            Self::ActivityName => 128,
            Self::Title => 256,
            Self::Detail => 4_096,
            Self::ApprovalOptionLabel => 128,
            Self::SessionTitle => 256,
            Self::ErrorMessage => 2_048,
            Self::VendorId => 128,
            Self::AccountLabel => 128,
            Self::CommandName => 128,
            Self::CommandDescription => 512,
        }
    }
}

/// How many commands one catalog may carry.
///
/// Sized off the observed ceiling with room to grow: a Claude Code install with plugins announces
/// around 56 and Cursor around 32, so this bounds a vendor that starts enumerating something else
/// without silently dropping a real catalog.
pub const COMMAND_CATALOG_MAX_ITEMS: usize = 256;

/// How many models one discovery may carry.
///
/// Sized the same way as the command catalog: a vendor enumerating a dozen is ordinary, and a
/// vendor enumerating thousands has started enumerating something else.
pub const MODEL_CATALOG_MAX_ITEMS: usize = 256;

/// How many reasoning choices one model may offer.
pub const REASONING_EFFORT_MAX_ITEMS: usize = 32;

/// How many choices one approval may carry.
///
/// A harness meeting a larger set refuses that one request rather than emitting it: an
/// unrenderable permission request is a bad answer, and ending the whole turn over it is worse.
pub const APPROVAL_MAX_OPTIONS: usize = 16;

/// The longest filesystem path the library will carry for a vendor session.
///
/// The unit depends on the check, and the two are not interchangeable for a non-ASCII path:
///
/// - [`vendor_path`], which admits a path a vendor reported, counts UTF-8 **bytes** of the
///   sanitised text. At most 4,096 bytes is 1,365 three-byte characters.
/// - [`is_argv_value_with_max`], given this constant by the Claude harness for the `--mcp-config`
///   path (and its generated scratch path) and by the ACP harness for MCP stdio command paths,
///   counts Unicode **code points**, so it accepts up to 4,096 characters however many bytes they
///   encode to.
///
/// For an all-ASCII path the two agree. This is documented rather than unified because unifying
/// them would change which paths are accepted.
pub const MAX_PATH_LENGTH: usize = 4_096;

/// The longest value the library accepts for one vendor command-line option.
///
/// This is a character limit because argv values are text rather than an encoded wire buffer.
/// Individual vendor positions may impose a stricter shape or a shorter limit.
pub const ARGV_VALUE_MAX_CODE_POINTS: usize = 128;

/// Vendor text after bounding, and whether anything was removed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BoundedText {
    /// What is safe to keep.
    pub text: String,
    /// True when stripping or truncation changed the input.
    pub truncated: bool,
}

impl BoundedText {
    /// Whether nothing survived.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Removes wire- and terminal-unsafe code points without imposing a field cap.
///
/// Streaming text — a delta the vendor is writing a token at a time — is bounded by the host's own
/// per-turn budget rather than by one of the small label limits, but it still must not carry
/// control sequences or bidirectional overrides across the boundary.
///
/// # Example
///
/// ```
/// use mango_external_agents::normalize;
///
/// let clean = normalize::sanitize_field("ls\u{202e}txt.exe");
/// assert_eq!(clean.text, "lstxt.exe");
/// assert!(clean.truncated);
/// ```
pub fn sanitize_field(raw: &str) -> BoundedText {
    sanitize_owned(raw.to_owned())
}

/// [`sanitize_field`] for text the caller already owns: nothing is copied.
///
/// Clean text comes back as it arrived. Dirty text is stripped in place, so the buffer that held
/// it is the buffer that keeps the survivors. The result is byte-identical to
/// `sanitize_field(&raw)`.
pub(crate) fn sanitize_owned(mut raw: String) -> BoundedText {
    if !may_need_stripping(&raw) {
        return BoundedText {
            text: raw,
            truncated: false,
        };
    }
    // One pass: a removal always shortens the text, so the length says whether anything went.
    let before = raw.len();
    raw.retain(|character| !is_strippable(character));
    BoundedText {
        truncated: raw.len() != before,
        text: raw,
    }
}

/// Applies one field's bound to vendor-supplied text.
///
/// # Example
///
/// ```
/// use mango_external_agents::normalize::{TextLimit, bound_text};
///
/// let bounded = bound_text(&"x".repeat(300), TextLimit::Title);
/// assert_eq!(bounded.text.chars().count(), 256);
/// assert!(bounded.truncated);
/// ```
pub fn bound_text(raw: &str, limit: TextLimit) -> BoundedText {
    let max = limit.max_code_points();
    // Plain ASCII is one byte and one code point per character and nothing in it is strippable, so
    // the clean prefix (up to the bound) is copied whole. The per-character loop then resumes at
    // the first byte the prefix scan refused, so it never re-reads what was already copied.
    let prefix = clean_ascii_prefix_len(&raw.as_bytes()[..raw.len().min(max)]);
    let mut text = String::with_capacity(raw.len().min(max));
    text.push_str(&raw[..prefix]);
    let mut kept = prefix;
    let mut truncated = false;
    for character in raw[prefix..].chars() {
        if is_strippable(character) {
            truncated = true;
            continue;
        }
        if kept == max {
            truncated = true;
            break;
        }
        text.push(character);
        kept += 1;
    }
    BoundedText { text, truncated }
}

/// How many leading bytes are printable ASCII, tab or newline: one code point each, none strippable.
///
/// Sixteen bytes at a time with no early exit inside a block, so the compiler can test a block
/// with vector instructions; the offending byte inside the first dirty block is then located
/// byte by byte. The result always ends on a character boundary because every byte it counts is
/// below 0x80.
fn clean_ascii_prefix_len(bytes: &[u8]) -> usize {
    let (blocks, remainder) = bytes.as_chunks::<16>();
    let mut clean = 0;
    for block in blocks {
        if block
            .iter()
            .fold(false, |dirty, byte| dirty | !is_clean_ascii_byte(*byte))
        {
            return clean + leading_clean_bytes(block);
        }
        clean += block.len();
    }
    clean + leading_clean_bytes(remainder)
}

fn leading_clean_bytes(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| is_clean_ascii_byte(**byte))
        .count()
}

/// Whether a byte is a whole character that [`is_strippable`] keeps: printable ASCII, tab or newline.
///
/// Comparisons joined by `|` and `&` for the reason [`is_flagged_byte`] gives: the block test in
/// [`clean_ascii_prefix_len`] is vectorized only while this has no branch, and a `matches!`
/// pattern is one optimizer change from having one.
const fn is_clean_ascii_byte(byte: u8) -> bool {
    let printable = (byte >= 0x20) & (byte <= 0x7e);
    printable | (byte == b'\t') | (byte == b'\n')
}

/// An opaque vendor identifier, refused rather than repaired.
///
/// Truncating a label is safe; truncating an id that is later echoed to the vendor would silently
/// point at a different object, so an id that does not survive bounding — or that is empty once
/// stripped — is an error rather than a shorter id.
///
/// # Errors
///
/// [`Error::InvalidVendorValue`] when the id is empty, is only strippable characters, or is longer
/// than [`TextLimit::VendorId`] allows.
///
/// # Example
///
/// ```
/// use mango_external_agents::normalize;
///
/// assert_eq!(normalize::opaque_id("sess_01H", "native session id").expect("usable"), "sess_01H");
/// assert!(normalize::opaque_id("   ", "native session id").is_err());
/// ```
pub fn opaque_id(raw: &str, field: &'static str) -> Result<String> {
    let bounded = bound_text(raw, TextLimit::VendorId);
    if bounded.truncated || bounded.text.trim().is_empty() {
        return Err(Error::InvalidVendorValue {
            field,
            received: bounded.text,
        });
    }
    Ok(bounded.text)
}

/// A filesystem path a vendor reported, sanitised but never shortened.
///
/// A path is something the vendor will be asked about again, and a shortened one names nothing —
/// so an over-long or unsanitisable path is dropped by the caller rather than cut here.
///
/// The length cap, [`MAX_PATH_LENGTH`], is measured in UTF-8 bytes of the sanitised text, not in
/// code points; see that constant for where the other unit applies.
pub fn vendor_path(raw: &str) -> Option<String> {
    let sanitised = sanitize_field(raw);
    if sanitised.truncated || sanitised.text.is_empty() || sanitised.text.len() > MAX_PATH_LENGTH {
        return None;
    }
    Some(sanitised.text)
}

/// Whether text can safely occupy a value position in a vendor argv array.
///
/// An argv array prevents a shell from interpreting its contents, but a value beginning with a
/// dash can still be parsed as the next CLI option. Controls and bidirectional formatting are
/// rejected because an argv value must be kept exactly as supplied; repairing it would change the
/// selected vendor setting. Callers add the vendor's own grammar and report the rejected field.
///
/// # Example
///
/// ```
/// use mango_external_agents::normalize::is_argv_value;
///
/// assert!(is_argv_value("claude-opus-5"));
/// assert!(!is_argv_value("--dangerously-skip-permissions"));
/// ```
#[must_use]
pub fn is_argv_value(raw: &str) -> bool {
    is_argv_value_with_max(raw, ARGV_VALUE_MAX_CODE_POINTS)
}

/// Whether text can safely occupy a value position in a vendor argv array with this field's cap.
///
/// Use [`is_argv_value`] for ordinary options. Filesystem paths retain their own documented cap,
/// so a harness may pass [`MAX_PATH_LENGTH`] here without shrinking a host-owned absolute path.
///
/// `max_code_points` counts Unicode code points, not bytes. A path near [`MAX_PATH_LENGTH`] that
/// [`vendor_path`] would refuse for its byte length can therefore still pass this check.
#[must_use]
pub fn is_argv_value_with_max(raw: &str, max_code_points: usize) -> bool {
    !raw.is_empty()
        && !raw.starts_with('-')
        && raw.chars().count() <= max_code_points
        && raw
            .chars()
            .all(|character| !character.is_control() && !is_strippable(character))
}

/// A byte scan that never misses text [`is_strippable`] would strip, and is much cheaper than
/// decoding every character to find out that ordinary text has nothing to strip.
///
/// It looks for the UTF-8 bytes a stripped character must contain: a C0 control or DEL as itself,
/// and the lead byte of each multi-byte sequence that holds a stripped character (`C2` for the C1
/// controls, `D8` for U+061C, `E2` for the marks, embeddings and isolates). Text without any of
/// them is clean; text with one may still be clean (an em dash leads with `E2`), so the caller
/// confirms with the exact per-character test.
fn may_need_stripping(text: &str) -> bool {
    contains_flagged_byte(text.as_bytes())
}

/// Bytes [`contains_flagged_byte`] tests between two chances to stop.
const SCAN_BLOCK: usize = 64;

/// Whether any byte is one [`is_flagged_byte`] flags.
///
/// Whole blocks with no early exit inside one, so the compiler can test a block with vector
/// instructions instead of branching on every byte. A flagged block still ends the scan, so dirty
/// text is not read past the block that proves it dirty.
fn contains_flagged_byte(bytes: &[u8]) -> bool {
    let (blocks, remainder) = bytes.as_chunks::<SCAN_BLOCK>();
    blocks.iter().any(|block| any_flagged(block)) || any_flagged(remainder)
}

/// [`is_flagged_byte`] over every byte, joined with `|` so the loop body has no branch.
fn any_flagged(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .fold(false, |found, byte| found | is_flagged_byte(*byte))
}

/// Whether a byte can belong to a stripped character.
///
/// Written as comparisons joined by `|` and `&`, never as a `match` or `matches!`: a pattern
/// compiles to a branch per byte, and the loop above is vectorized only while its body has none.
/// Rust 1.99 (LLVM 23) stopped vectorizing the pattern form, which made clean text six times
/// slower to pass; see `docs/benchmarks.md`.
const fn is_flagged_byte(byte: u8) -> bool {
    let control = (byte < 0x20) & (byte != b'\t') & (byte != b'\n');
    control | (byte == 0x7f) | (byte == 0xc2) | (byte == 0xd8) | (byte == 0xe2)
}

/// C0 and C1 controls except tab and newline, and every bidirectional formatting character.
///
/// Expressed as code-point tests rather than as a character class: a regular expression made of
/// escaped control characters is unreviewable, and this is exactly the code where a wrong range
/// would go unnoticed.
fn is_strippable(character: char) -> bool {
    if character == '\t' || character == '\n' {
        return false;
    }
    let code = u32::from(character);
    match code {
        0x00..=0x1f => true,     // C0
        0x7f..=0x9f => true,     // DEL and C1
        0x061c => true,          // arabic letter mark
        0x200e | 0x200f => true, // left-to-right and right-to-left marks
        0x202a..=0x202e => true, // the embedding and override set, with its terminator
        0x2066..=0x2069 => true, // the isolate set, with its terminator
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ARGV_VALUE_MAX_CODE_POINTS, BoundedText, MAX_PATH_LENGTH, TextLimit, bound_text,
        clean_ascii_prefix_len, is_argv_value, is_argv_value_with_max, opaque_id, sanitize_field,
        sanitize_owned, vendor_path,
    };
    use crate::error::Error;

    #[test]
    fn strips_control_characters_but_keeps_tabs_and_newlines() {
        let clean = sanitize_field("a\u{0}b\u{1b}c\td\ne\u{7f}f\u{9f}g");
        assert_eq!(clean.text, "abc\td\nefg");
        assert!(clean.truncated);
    }

    #[test]
    fn strips_every_bidirectional_formatting_character() {
        for code in [0x061c, 0x200e, 0x200f, 0x202a, 0x202e, 0x2066, 0x2069] {
            let character = char::from_u32(code).expect("expected a character");
            let clean = sanitize_field(&format!("safe{character}text"));
            assert_eq!(
                clean.text, "safetext",
                "expected U+{code:04X} to be stripped, received {:?}",
                clean.text
            );
            assert!(clean.truncated);
        }
    }

    /// The original character-by-character loop, kept as the reference the fast paths must match.
    fn reference_sanitize(raw: &str) -> (String, bool) {
        let mut text = String::with_capacity(raw.len());
        let mut truncated = false;
        for character in raw.chars() {
            if super::is_strippable(character) {
                truncated = true;
                continue;
            }
            text.push(character);
        }
        (text, truncated)
    }

    /// Inputs on both sides of every boundary `is_strippable` draws, alone and embedded.
    fn boundary_inputs() -> Vec<String> {
        let edges = [
            0x00, 0x08, 0x09, 0x0a, 0x0b, 0x1f, 0x20, 0x7e, 0x7f, 0x80, 0x9f, 0xa0, 0x061b, 0x061c,
            0x061d, 0x200d, 0x200e, 0x200f, 0x2010, 0x2029, 0x202a, 0x202e, 0x202f, 0x2065, 0x2066,
            0x2069, 0x206a, 0x1f34b,
        ];
        let mut inputs = vec![
            String::new(),
            String::from("the quick brown fox"),
            String::from("héllo wörld 日本語 🍋 — done\n"),
            String::from("\u{1b}[0m"),
            String::from("\u{0}\u{7f}\u{9f}\u{202e}"),
        ];
        // A stripped character at every offset around the prefilter's 16-byte blocks.
        for offset in 0..40 {
            for dirty in ['\u{1b}', '\u{202e}', '\u{85}'] {
                let mut text = "a".repeat(40);
                text.insert(offset, dirty);
                inputs.push(text);
            }
        }
        for code in edges {
            let character = char::from_u32(code).expect("expected a scalar value");
            inputs.push(character.to_string());
            inputs.push(format!("{character}tail"));
            inputs.push(format!("head{character}"));
            inputs.push(format!("héad {character} 日本 {character}"));
        }
        inputs
    }

    #[test]
    fn owned_and_borrowed_sanitising_match_the_reference_byte_for_byte() {
        for input in boundary_inputs() {
            let (expected_text, expected_truncated) = reference_sanitize(&input);
            let borrowed = sanitize_field(&input);
            let owned = sanitize_owned(input.clone());
            assert_eq!(
                (borrowed.text.as_bytes(), borrowed.truncated),
                (expected_text.as_bytes(), expected_truncated),
                "expected sanitize_field to match the reference | input: {input:?}"
            );
            assert_eq!(
                (owned.text.as_bytes(), owned.truncated),
                (expected_text.as_bytes(), expected_truncated),
                "expected sanitize_owned to match the reference | input: {input:?}"
            );
        }
    }

    #[test]
    fn the_byte_prefilter_flags_every_character_that_is_stripped() {
        let mut buffer = [0_u8; 4];
        for character in ('\0'..=char::MAX).filter(|character| super::is_strippable(*character)) {
            let encoded: &str = character.encode_utf8(&mut buffer);
            assert!(
                super::may_need_stripping(encoded),
                "expected the prefilter to flag U+{:04X} | received clean",
                u32::from(character)
            );
        }
    }

    /// The pattern `is_flagged_byte` was written as before it became comparisons.
    const fn reference_flagged(byte: u8) -> bool {
        matches!(byte, 0x00..=0x08 | 0x0b..=0x1f | 0x7f | 0xc2 | 0xd8 | 0xe2)
    }

    #[test]
    fn the_flagged_byte_test_agrees_with_the_pattern_for_every_byte() {
        for byte in 0..=u8::MAX {
            assert_eq!(
                super::is_flagged_byte(byte),
                reference_flagged(byte),
                "expected byte {byte:#04x} flagged: {} | received: {}",
                reference_flagged(byte),
                super::is_flagged_byte(byte)
            );
        }
    }

    #[test]
    fn the_block_scan_agrees_with_a_byte_by_byte_scan_at_every_offset() {
        // Both sides of every range and value the flag test draws, and bytes far from all of them.
        let probes = [
            0x00, 0x08, b'\t', b'\n', 0x0b, 0x1f, 0x20, b'a', 0x7e, 0x7f, 0x80, 0xc1, 0xc2, 0xc3,
            0xd7, 0xd8, 0xd9, 0xe1, 0xe2, 0xe3, 0xff,
        ];
        assert!(
            !super::contains_flagged_byte(&[]),
            "expected empty input clean | received flagged"
        );
        // No block, one block, two blocks, and every remainder length beside them.
        for length in 1..=2 * super::SCAN_BLOCK + 2 {
            let mut bytes = vec![b'a'; length];
            assert!(
                !super::contains_flagged_byte(&bytes),
                "expected {length} clean bytes to pass | received flagged"
            );
            for offset in 0..length {
                for probe in probes {
                    bytes[offset] = probe;
                    let expected = bytes.iter().any(|byte| reference_flagged(*byte));
                    assert_eq!(
                        super::contains_flagged_byte(&bytes),
                        expected,
                        "expected flagged: {expected} for byte {probe:#04x} at offset {offset} \
                         of {length} | received: {}",
                        !expected
                    );
                }
                bytes[offset] = b'a';
            }
        }
    }

    #[test]
    fn sanitising_matches_the_reference_for_every_class_at_every_offset_across_scan_blocks() {
        // One stripped character per flagged class (C0, DEL, a `C2`, the `D8` and an `E2` lead),
        // then characters that share a lead byte or sit beside one and must survive.
        let characters = [
            '\u{1b}', '\u{7f}', '\u{85}', '\u{61c}', '\u{202e}', '\t', '\n', 'é', '\u{a0}',
            '\u{620}', '—', '日', '🍋',
        ];
        let length = 2 * super::SCAN_BLOCK + 8;
        for offset in 0..=length {
            for character in characters {
                let mut input = "a".repeat(length);
                input.insert(offset, character);
                let (expected_text, expected_truncated) = reference_sanitize(&input);
                let owned = sanitize_owned(input.clone());
                assert_eq!(
                    (owned.text.as_bytes(), owned.truncated),
                    (expected_text.as_bytes(), expected_truncated),
                    "expected U+{:04X} at byte {offset} sanitised as the reference does | \
                     received text {:?}, truncated {}",
                    u32::from(character),
                    owned.text,
                    owned.truncated
                );
            }
        }
    }

    #[test]
    fn owned_sanitising_reuses_the_callers_buffer_for_clean_and_dirty_text() {
        for input in [
            "clean ascii text",
            "héllo 日本語",
            "dirty\u{1b}[0m\u{202e}text",
        ] {
            let owned = input.to_owned();
            let (pointer, capacity) = (owned.as_ptr(), owned.capacity());
            let cleaned = sanitize_owned(owned);
            assert_eq!(
                (cleaned.text.as_ptr(), cleaned.text.capacity()),
                (pointer, capacity),
                "expected {input:?} sanitised in the buffer it arrived in | received a new buffer"
            );
        }
    }

    #[test]
    fn leaves_text_that_needs_nothing_alone() {
        let clean = sanitize_field("Reading src/main.rs — line 12\n");
        assert_eq!(clean.text, "Reading src/main.rs — line 12\n");
        assert!(!clean.truncated);
    }

    #[test]
    fn cuts_to_code_points_rather_than_bytes() {
        // Every one of these is four bytes, so a byte-counting cut would produce an unencodable
        // half-character and a very different length.
        let bounded = bound_text(&"🍋".repeat(200), TextLimit::ActivityName);
        assert_eq!(bounded.text.chars().count(), 128);
        assert_eq!(bounded.text, "🍋".repeat(128));
        assert!(bounded.truncated);
    }

    /// The character-by-character loop `bound_text` ran before the clean-prefix fast path, kept as
    /// the reference the fast path must match byte for byte.
    fn reference_bound(raw: &str, max: usize) -> BoundedText {
        let mut text = String::new();
        let mut kept = 0;
        let mut truncated = false;
        for character in raw.chars() {
            if super::is_strippable(character) {
                truncated = true;
                continue;
            }
            if kept == max {
                truncated = true;
                break;
            }
            text.push(character);
            kept += 1;
        }
        BoundedText { text, truncated }
    }

    const LIMITS: [TextLimit; 10] = [
        TextLimit::ActivityName,
        TextLimit::Title,
        TextLimit::Detail,
        TextLimit::ApprovalOptionLabel,
        TextLimit::SessionTitle,
        TextLimit::ErrorMessage,
        TextLimit::VendorId,
        TextLimit::AccountLabel,
        TextLimit::CommandName,
        TextLimit::CommandDescription,
    ];

    fn assert_bound_matches_reference(input: &str, limit: TextLimit) {
        let expected = reference_bound(input, limit.max_code_points());
        let received = bound_text(input, limit);
        assert_eq!(
            (received.text.as_bytes(), received.truncated),
            (expected.text.as_bytes(), expected.truncated),
            "expected bound_text to match the reference | limit: {limit:?} | input length: {} | \
             input head: {:?}",
            input.len(),
            input.chars().take(24).collect::<String>()
        );
    }

    /// Shapes around a bound of `max` code points: clean ASCII, a late or early non-ASCII
    /// character, escape sequences before, at and after the bound, and multi-byte text that
    /// straddles it.
    fn bound_shapes(max: usize) -> Vec<String> {
        let mut shapes = vec![
            String::new(),
            "x".repeat(max / 2),
            "x".repeat(max - 1),
            "x".repeat(max),
            "x".repeat(max + 1),
            "x".repeat(max * 3),
            "line one\n\tline two\n".repeat(max),
            "é".repeat(max - 1),
            "é".repeat(max),
            "é".repeat(max + 1),
            "🍋".repeat(max + 1),
            "\u{1b}[0m".repeat(max),
            "\u{202e}".repeat(max + 1),
        ];
        // The first unclean byte at every offset around the bound and the 16-byte blocks.
        let offsets = (0..40)
            .chain(max.saturating_sub(20)..max + 20)
            .collect::<Vec<_>>();
        for offset in offsets {
            for unclean in ['é', '\u{1b}', '\u{7f}', '\u{85}', '\u{202e}', '🍋', '\0'] {
                for tail in ["", "y", "yyyy\u{1b}[0m", "🍋🍋"] {
                    let mut text = "a".repeat(offset);
                    text.push(unclean);
                    text.push_str(tail);
                    shapes.push(text);
                }
            }
        }
        // Colour codes through otherwise clean output, and a straddling run of multi-byte text.
        shapes.push("\u{1b}[32mok\u{1b}[0m done\n".repeat(max / 8));
        shapes.push(format!("{}{}", "a".repeat(max - 2), "日本語日本語"));
        shapes.push(format!("{}{}", "a".repeat(max - 1), "🍋🍋🍋"));
        shapes.push(format!("{}\u{1b}[0m{}", "a".repeat(max), "b".repeat(8)));
        shapes.push(format!("{}\u{1b}[0m", "a".repeat(max)));
        shapes
    }

    #[test]
    fn bound_text_matches_the_reference_for_every_shape_and_limit() {
        for limit in LIMITS {
            for shape in bound_shapes(limit.max_code_points()) {
                assert_bound_matches_reference(&shape, limit);
            }
        }
    }

    #[test]
    fn bound_text_matches_the_reference_for_scalar_values_inside_and_at_the_bound() {
        let max = TextLimit::VendorId.max_code_points();
        // Every scalar through the range that holds every stripped character, then a stride over
        // the rest (none of it is stripped) that still reaches each UTF-8 width and the last scalar.
        let scalars = ('\0'..='\u{2100}')
            .chain(('\u{2101}'..=char::MAX).step_by(997))
            .chain([char::MAX]);
        for character in scalars {
            for head in [0, 15, 16, max - 1, max] {
                let input = format!("{}{character}zz", "a".repeat(head));
                assert_bound_matches_reference(&input, TextLimit::VendorId);
            }
        }
    }

    #[test]
    fn bound_text_matches_the_reference_for_generated_mixed_text() {
        let alphabet: Vec<char> = [
            "\t", "\n", "\u{1b}", "\u{7f}", "\u{9f}", "é", "日", "🍋", "\u{202e}", "\u{61c}",
        ]
        .iter()
        .flat_map(|piece| piece.chars())
        .collect();
        // xorshift64: deterministic, so a failure names an input that reproduces.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3_000 {
            let length = usize::try_from(next() % 400).expect("expected a small length");
            // About one character in `rarity` is not plain ASCII, so a long clean prefix is the
            // common case and a late unclean character is too.
            let rarity = 1 + next() % 200;
            let input: String = (0..length)
                .map(|_| {
                    if next() % rarity != 0 {
                        return 'a';
                    }
                    let pick = usize::try_from(next()).unwrap_or(0) % alphabet.len();
                    alphabet[pick]
                })
                .collect();
            for limit in [TextLimit::VendorId, TextLimit::Title] {
                assert_bound_matches_reference(&input, limit);
            }
        }
    }

    #[test]
    fn the_clean_ascii_prefix_stops_at_the_first_byte_that_is_not_kept_whole() {
        for (input, expected) in [
            ("", 0),
            ("plain text\twith\nwhitespace", 26),
            ("abc\u{1b}[0m", 3),
            ("abcé", 3),
            ("abc\u{7f}", 3),
            ("\u{0}abc", 0),
            ("🍋", 0),
        ] {
            assert_eq!(
                clean_ascii_prefix_len(input.as_bytes()),
                expected,
                "expected the clean prefix of {input:?} to be {expected} bytes"
            );
        }
        // The unclean byte at every offset across several 16-byte blocks.
        for offset in 0..70 {
            let mut bytes = vec![b'a'; 70];
            bytes[offset] = 0x1b;
            assert_eq!(
                clean_ascii_prefix_len(&bytes),
                offset,
                "expected an ESC at byte {offset} to end the clean prefix there"
            );
        }
    }

    /// The pattern `is_clean_ascii_byte` was written as before it became comparisons.
    const fn reference_clean_ascii(byte: u8) -> bool {
        matches!(byte, 0x20..=0x7e | b'\t' | b'\n')
    }

    #[test]
    fn the_clean_ascii_byte_test_agrees_with_the_pattern_for_every_byte() {
        for byte in 0..=u8::MAX {
            assert_eq!(
                super::is_clean_ascii_byte(byte),
                reference_clean_ascii(byte),
                "expected byte {byte:#04x} clean: {} | received: {}",
                reference_clean_ascii(byte),
                super::is_clean_ascii_byte(byte)
            );
        }
    }

    #[test]
    fn the_clean_ascii_prefix_agrees_with_a_byte_by_byte_scan_at_every_offset() {
        // Both sides of every range and value the clean test draws, and bytes far from all of them.
        let probes = [
            0x00, 0x08, b'\t', b'\n', 0x0b, 0x1f, 0x20, b'a', 0x7e, 0x7f, 0x80, 0xc2, 0xff,
        ];
        // No block, one block, two blocks, and every remainder length beside them.
        for length in 0..=2 * 16 + 2 {
            for offset in 0..length {
                for probe in probes {
                    let mut bytes = vec![b'a'; length];
                    bytes[offset] = probe;
                    let expected = bytes
                        .iter()
                        .position(|byte| !reference_clean_ascii(*byte))
                        .unwrap_or(length);
                    let received = clean_ascii_prefix_len(&bytes);
                    assert_eq!(
                        received, expected,
                        "expected a clean prefix of {expected} bytes for byte {probe:#04x} at \
                         offset {offset} of {length} | received: {received}"
                    );
                }
            }
        }
        // Two unclean bytes in one block, and in neighbouring blocks: the first one ends the prefix.
        for (first, second) in [(3, 9), (15, 16), (17, 31), (0, 33)] {
            let mut bytes = vec![b'a'; 34];
            bytes[first] = 0x7f;
            bytes[second] = 0x00;
            let received = clean_ascii_prefix_len(&bytes);
            assert_eq!(
                received, first,
                "expected the prefix to end at the first unclean byte, offset {first}, with \
                 another at {second} | received: {received}"
            );
        }
    }

    #[test]
    fn every_limit_is_the_number_the_port_carried() {
        let cases = [
            (TextLimit::ActivityName, 128),
            (TextLimit::Title, 256),
            (TextLimit::Detail, 4_096),
            (TextLimit::ApprovalOptionLabel, 128),
            (TextLimit::SessionTitle, 256),
            (TextLimit::ErrorMessage, 2_048),
            (TextLimit::VendorId, 128),
            (TextLimit::AccountLabel, 128),
            (TextLimit::CommandName, 128),
            (TextLimit::CommandDescription, 512),
        ];
        for (limit, expected) in cases {
            assert_eq!(
                limit.max_code_points(),
                expected,
                "expected {expected} for {limit:?}, received {}",
                limit.max_code_points()
            );
        }
    }

    #[test]
    fn an_id_that_fits_survives_untouched() {
        assert_eq!(
            opaque_id("thread_01HQ8", "native session id").expect("expected a usable id"),
            "thread_01HQ8"
        );
    }

    #[test]
    fn an_over_long_id_is_refused_rather_than_shortened() {
        let error = opaque_id(&"i".repeat(129), "native session id")
            .expect_err("expected a refusal, received an id");
        assert!(
            matches!(
                error,
                Error::InvalidVendorValue {
                    field: "native session id",
                    ..
                }
            ),
            "expected an invalid-value refusal, received {error:?}"
        );
    }

    #[test]
    fn an_id_that_is_only_strippable_characters_is_refused() {
        for raw in ["", "   ", "\u{202e}\u{200f}"] {
            assert!(
                opaque_id(raw, "approval option id").is_err(),
                "expected {raw:?} to be refused, received an id"
            );
        }
    }

    /// `opaque_id` is identity-or-error, never sanitise-in-place.
    ///
    /// Load-bearing well beyond this module: a harness that parks a vendor question under the raw
    /// wire id and announces the normalised one to the host would correlate the host's answer
    /// against a key that no longer matches, if this ever returned a *repaired* id. `bound_text`
    /// counts a stripped character as truncation precisely so that cannot happen.
    #[test]
    fn an_id_with_one_strippable_character_is_refused_rather_than_repaired() {
        for raw in ["req\u{1}1", "sess_01H\u{202e}", "\u{200f}thread_7"] {
            let error = opaque_id(raw, "approval request id")
                .expect_err("expected a refusal, received a repaired id");
            assert!(
                matches!(
                    error,
                    Error::InvalidVendorValue {
                        field: "approval request id",
                        ..
                    }
                ),
                "expected {raw:?} to be refused, received {error:?}"
            );
        }
    }

    #[test]
    fn a_path_is_sanitised_but_never_shortened() {
        assert_eq!(
            vendor_path("/home/ada/projects/mango"),
            Some(String::from("/home/ada/projects/mango"))
        );
        assert_eq!(vendor_path(&"p".repeat(4_097)), None);
        assert_eq!(vendor_path("/home/\u{202e}ada"), None);
        assert_eq!(vendor_path(""), None);
    }

    #[test]
    fn argv_values_are_exact_or_rejected_before_they_can_be_another_option() {
        for accepted in [
            "opus",
            "publishers/anthropic/models/claude-opus-5",
            "a value",
        ] {
            assert!(
                is_argv_value(accepted),
                "expected {accepted:?} to be usable"
            );
        }
        for rejected in [
            "",
            "--dangerously-skip-permissions",
            "-p",
            "opus\tsonnet",
            "opus\nsonnet",
            "opus\u{1}sonnet",
            "opus\u{202e}sonnet",
        ] {
            assert!(
                !is_argv_value(rejected),
                "expected {rejected:?} to be refused"
            );
        }
        assert!(!is_argv_value(&"o".repeat(ARGV_VALUE_MAX_CODE_POINTS + 1)));
        assert!(is_argv_value_with_max(
            &format!("/{}", "p".repeat(ARGV_VALUE_MAX_CODE_POINTS)),
            MAX_PATH_LENGTH
        ));
    }
}
