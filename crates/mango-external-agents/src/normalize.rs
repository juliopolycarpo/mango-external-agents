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

/// How many choices one approval may carry.
///
/// A harness meeting a larger set refuses that one request rather than emitting it: an
/// unrenderable permission request is a bad answer, and ending the whole turn over it is worse.
pub const APPROVAL_MAX_OPTIONS: usize = 16;

/// The longest filesystem path the library will carry for a vendor session.
pub const MAX_PATH_LENGTH: usize = 4_096;

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
    let mut text = String::with_capacity(raw.len());
    let mut truncated = false;
    for character in raw.chars() {
        if is_strippable(character) {
            truncated = true;
            continue;
        }
        text.push(character);
    }
    BoundedText { text, truncated }
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
    let mut text = String::new();
    let mut kept = 0;
    let mut truncated = false;
    for character in raw.chars() {
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
pub fn vendor_path(raw: &str) -> Option<String> {
    let sanitised = sanitize_field(raw);
    if sanitised.truncated || sanitised.text.is_empty() || sanitised.text.len() > MAX_PATH_LENGTH {
        return None;
    }
    Some(sanitised.text)
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
    use super::{TextLimit, bound_text, opaque_id, sanitize_field, vendor_path};
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
}
