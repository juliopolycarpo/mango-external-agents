//! What a stderr tail keeps dropping after a cut.
//!
//! [`redact::stderr_text`] can only hide a value it can still see the name of. A cut that drops
//! a credential's name and keeps its value hands the value back verbatim, so a cut is not done
//! where the bytes run out: it goes on to a place the redactor's rules cannot reach across.
//! Two things reach across a cut. The rest of a line whose start was dropped, and a value the
//! rules read on the next line, because they skip line breaks between a name, its separator and
//! its value. Bytes are only ever dropped here; nothing is cut before redaction.

use super::line_break::first_line_feed;
use crate::redact::{
    self, MAX_ESCAPE_BYTES, holds_string_terminator, is_space_byte, is_stripped_byte,
};

/// How much of what was dropped is remembered to decide whether a value is still awaited.
///
/// A credential name longer than this, or one padded with escape sequences that the redactor
/// strips but the collapse does not, is outside what the check can see, and the line after it is
/// kept. A credential name is a few dozen bytes. The exception is a string escape (an OSC, say)
/// ending inside the window: the redactor takes the whole string out, so a name can sit that far
/// behind, and the look-back reaches [`REACH_BYTES`] to cover it.
const TAIL_BYTES: usize = 512;

/// The look-back when a string escape ends inside the last [`TAIL_BYTES`].
const REACH_BYTES: usize = TAIL_BYTES + MAX_ESCAPE_BYTES;

/// Where the discard is inside the text it is dropping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// In a line whose start is gone: drop to its LF.
    Line,
    /// Past a line that ended awaiting a value: drop blank lines, then the next non-empty line.
    Blank,
}

/// The state of a cut that is not finished.
///
/// Holds unredacted stderr, so it has no `Debug`: nothing here is fit to print.
pub(super) struct Discard {
    /// The end of what was dropped, blank runs collapsed to one space, for the redactor to read.
    tail: Vec<u8>,
    phase: Phase,
}

impl Discard {
    /// A discard that starts inside a line, after `dropped` was removed from its front.
    pub(super) fn mid_line(dropped: &[u8]) -> Self {
        Self {
            tail: tail_of(dropped),
            phase: Phase::Line,
        }
    }

    /// A discard for a cut that ended on a line terminator, or `None` when `dropped`, the line
    /// and everything before it, leaves no value awaited and the bytes after it are safe to keep.
    pub(super) fn after_line(dropped: &[u8]) -> Option<Self> {
        let mut discard = Self {
            tail: tail_of(dropped),
            phase: Phase::Blank,
        };
        discard.push_blank();
        discard.awaits_value().then_some(discard)
    }

    /// Drops the front of `chunk` and returns where the bytes worth keeping begin, or `None` when
    /// the whole chunk was dropped and the discard goes on.
    pub(super) fn consume(&mut self, chunk: &[u8]) -> Option<usize> {
        let mut at = 0;
        loop {
            if self.phase == Phase::Blank {
                at += chunk[at..]
                    .iter()
                    .take_while(|byte| is_space_byte(**byte))
                    .count();
                if at == chunk.len() {
                    return None;
                }
                self.phase = Phase::Line;
            }
            let Some(offset) = first_line_feed(&chunk[at..]) else {
                self.push_text(&chunk[at..]);
                return None;
            };
            self.push_text(&chunk[at..at + offset]);
            self.push_blank();
            at += offset + 1;
            if !self.awaits_value() {
                return Some(at);
            }
            self.phase = Phase::Blank;
        }
    }

    fn awaits_value(&self) -> bool {
        let window = &self.tail[self.tail.len().saturating_sub(TAIL_BYTES)..];
        let seen = if holds_string_terminator(window) {
            &self.tail[..]
        } else {
            window
        };
        redact::ends_awaiting_value(&String::from_utf8_lossy(seen))
    }

    fn push_blank(&mut self) {
        if self.tail.last() != Some(&b' ') {
            self.tail.push(b' ');
        }
    }

    fn push_text(&mut self, text: &[u8]) {
        let piece = tail_of(text);
        let skip = usize::from(self.tail.last() == Some(&b' ') && piece.first() == Some(&b' '));
        self.tail.extend_from_slice(&piece[skip..]);
        if self.tail.len() > REACH_BYTES {
            self.tail.drain(..self.tail.len() - REACH_BYTES);
        }
    }
}

/// The last [`TAIL_BYTES`] of `bytes` (or [`REACH_BYTES`] when a string escape ends in them), with
/// every run of spaces, tabs and line feeds collapsed to
/// a single space and every run of bytes the redactor removes (a bare CR among them) to one of
/// them.
///
/// Those bytes stay in the text raw. Whether the text on either side of one joins or is parted by
/// a boundary is the redactor's call, made when `ends_awaiting_value` runs its own stripper over
/// the tail, and a copy of that rule here would drift from it.
///
/// Scans back from the end and stops once it has enough, so a long dropped line costs the tail
/// and not the line.
fn tail_of(bytes: &[u8]) -> Vec<u8> {
    let tail = collapsed_tail(bytes, TAIL_BYTES);
    if holds_string_terminator(&tail) {
        return collapsed_tail(bytes, REACH_BYTES);
    }
    tail
}

/// The last `limit` bytes of `bytes` after the collapse [`tail_of`] describes.
fn collapsed_tail(bytes: &[u8], limit: usize) -> Vec<u8> {
    let mut tail = Vec::with_capacity(limit.min(bytes.len()));
    for byte in bytes.iter().rev() {
        if tail.len() == limit {
            break;
        }
        if is_stripped_byte(*byte) {
            if !tail.last().is_some_and(|last| is_stripped_byte(*last)) {
                tail.push(*byte);
            }
        } else if !is_space_byte(*byte) {
            tail.push(*byte);
        } else if tail.last() != Some(&b' ') {
            tail.push(b' ');
        }
    }
    tail.reverse();
    tail
}

#[cfg(test)]
mod tests {
    use super::{Discard, TAIL_BYTES, tail_of};

    #[test]
    fn a_tail_collapses_blank_runs_and_keeps_only_the_end() {
        assert_eq!(
            tail_of(b"a \n\t b\r\r\x0b"),
            b"a b\x0b",
            "expected one space per blank run and one byte per removed run"
        );
        let long = vec![b'x'; TAIL_BYTES * 2];
        assert_eq!(
            tail_of(&long).len(),
            TAIL_BYTES,
            "expected the last {TAIL_BYTES} bytes of a longer line"
        );
        assert_eq!(tail_of(b""), b"", "expected an empty tail for no bytes");
    }

    #[test]
    fn the_look_back_reaches_over_a_string_escape_that_ends_in_the_window() {
        let mut text = b"API_KEY \x1b]0;".to_vec();
        text.extend(std::iter::repeat_n(b'x', 2 * TAIL_BYTES));
        let plain = tail_of(&text);
        assert_eq!(
            plain.len(),
            TAIL_BYTES,
            "expected the ordinary window when no string escape ends in it"
        );

        text.push(0x07);
        let reached = tail_of(&text);
        assert!(
            reached.len() > TAIL_BYTES && reached.starts_with(b"API_KEY"),
            "expected the look-back to reach the name behind the OSC | received {} bytes",
            reached.len()
        );
        assert!(
            Discard::after_line(&text).is_some(),
            "expected a name behind a long OSC to await its separator"
        );
    }

    /// A name far behind a string escape is only awaiting a value when nothing but blanks follows
    /// it, and blanks collapse to one byte, so the terminator is then always in the window. Text
    /// that follows the name after the escape ended ends the wait, and the look-back need not
    /// reach the escape.
    #[test]
    fn a_name_behind_an_escape_that_ended_lines_ago_is_not_awaiting_a_value() {
        let mut dropped = b"API_KEY \x1b]0;".to_vec();
        dropped.extend(std::iter::repeat_n(b'x', 400));
        dropped.push(0x07);
        dropped.extend_from_slice(b"=\n");
        for line in 0..30 {
            dropped.extend(format!("line{line} noise\n").bytes());
        }
        assert!(
            Discard::after_line(&dropped).is_none(),
            "expected the assignment settled by the lines after it, leaving no value awaited"
        );

        let mut awaiting = b"API_KEY \x1b]0;".to_vec();
        awaiting.extend(std::iter::repeat_n(b'x', 400));
        awaiting.push(0x07);
        awaiting.extend_from_slice(b" \n\n\r \n\t=\n\n  \n");
        assert!(
            Discard::after_line(&awaiting).is_some(),
            "expected blanks after the separator to leave the value awaited"
        );
    }

    #[test]
    fn a_line_that_leaves_no_value_awaited_ends_the_discard_at_its_terminator() {
        let mut discard = Discard::mid_line(b"noise API_KEY=value");
        assert_eq!(
            discard.consume(b"more\r\nnext"),
            Some(6),
            "expected the discard to end right after the LF, not the CR"
        );
    }

    #[test]
    fn a_line_break_after_a_separator_keeps_the_next_non_empty_line_dropped() {
        let mut discard = Discard::mid_line(b"noise API_KEY=");
        let chunk = b"\n\n  \nsk-secret-value\nkept";
        let at = discard.consume(chunk).expect("expected the discard to end");
        assert_eq!(
            &chunk[at..],
            b"kept",
            "expected the blank lines and the value line dropped"
        );
    }

    #[test]
    fn a_value_line_split_across_chunks_stays_dropped() {
        let mut discard = Discard::mid_line(b"noise Authorization:");
        assert_eq!(
            discard.consume(b"\n  "),
            None,
            "expected to await the value"
        );
        assert_eq!(
            discard.consume(b"Bearer sk-"),
            None,
            "expected the half token line dropped"
        );
        assert_eq!(
            discard.consume(b"live-secret\nkept"),
            Some(12),
            "expected the discard to end after the token line"
        );
    }

    #[test]
    fn a_scheme_on_its_own_line_awaits_its_token_on_the_next() {
        let mut discard = Discard::mid_line(b"Authorization:");
        let chunk = b"\nBearer\ntoken-line\nkept";
        let at = discard.consume(chunk).expect("expected the discard to end");
        assert_eq!(&chunk[at..], b"kept", "expected the token line dropped");
    }

    #[test]
    fn after_line_awaits_only_when_the_dropped_text_does() {
        assert!(
            Discard::after_line(b"noise Authorization:").is_some(),
            "expected a dangling Authorization: to be awaited"
        );
        assert!(
            Discard::after_line(b"noise API_KEY").is_some(),
            "expected a name still wanting its separator to be awaited"
        );
        assert!(
            Discard::after_line(b"ordinary diagnostic").is_none(),
            "expected an ordinary line to leave nothing awaited"
        );
        assert!(
            Discard::after_line(b"API_KEY=value").is_none(),
            "expected a whole assignment to leave nothing awaited"
        );
    }

    #[test]
    fn a_long_blank_run_after_a_name_does_not_hide_it() {
        let mut dropped = b"noise API_KEY".to_vec();
        dropped.extend(std::iter::repeat_n(b' ', TAIL_BYTES * 4));
        assert!(
            Discard::after_line(&dropped).is_some(),
            "expected a name followed by a long blank run to stay awaited"
        );
    }

    #[test]
    fn a_bare_carriage_return_joins_a_name_as_the_redactor_reads_it() {
        assert_eq!(
            tail_of(b"noise API_\rKEY"),
            b"noise API_\rKEY",
            "expected the CR kept raw in the tail"
        );
        assert!(
            Discard::after_line(b"noise API_\rKEY").is_some(),
            "expected a name split by a CR to await its separator"
        );
    }

    #[test]
    fn a_removed_byte_before_a_name_is_left_to_the_redactor_to_read() {
        assert!(
            Discard::after_line(b"aaaaaaaa xyz\rtoken=").is_some(),
            "expected xyz, a CR and token= to read as a name awaiting its value"
        );
        assert!(
            Discard::after_line(b"aaaaaaaa xyz\rtoken=value").is_none(),
            "expected a whole assignment to leave nothing awaited"
        );
    }
}
