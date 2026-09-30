//! What a stderr tail keeps dropping after a cut.
//!
//! [`redact::stderr_text`] can only hide a value it can still see the name of. A cut that drops
//! a credential's name and keeps its value hands the value back verbatim, so a cut is not done
//! where the bytes run out: it goes on to a place the redactor's rules cannot reach across.
//! Two things reach across a cut. The rest of a line whose start was dropped, and a value the
//! rules read on the next line, because they skip line breaks between a name, its separator and
//! its value. Bytes are only ever dropped here; nothing is cut before redaction.

use crate::redact::{self, is_space_byte};

/// How much of what was dropped is remembered to decide whether a value is still awaited.
///
/// A credential name longer than this, or one padded with escape sequences that the redactor
/// strips but the collapse does not, is outside what the check can see, and the line after it is
/// kept. A credential name is a few dozen bytes.
const TAIL_BYTES: usize = 512;

/// Where the discard is inside the text it is dropping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// In a line whose start is gone: drop to its CR or LF.
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
            let Some(offset) = chunk[at..]
                .iter()
                .position(|byte| matches!(byte, b'\n' | b'\r'))
            else {
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
        redact::ends_awaiting_value(&String::from_utf8_lossy(&self.tail))
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
        if self.tail.len() > TAIL_BYTES {
            self.tail.drain(..self.tail.len() - TAIL_BYTES);
        }
    }
}

/// The last [`TAIL_BYTES`] of `bytes` with every blank run collapsed to a single space.
///
/// Scans back from the end and stops once it has enough, so a long dropped line costs the tail
/// and not the line.
fn tail_of(bytes: &[u8]) -> Vec<u8> {
    let mut tail = Vec::with_capacity(TAIL_BYTES.min(bytes.len()));
    for byte in bytes.iter().rev() {
        if tail.len() == TAIL_BYTES {
            break;
        }
        if !is_space_byte(*byte) {
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
        assert_eq!(tail_of(b"a \r\n\t b"), b"a b", "expected one space per run");
        let long = vec![b'x'; TAIL_BYTES * 2];
        assert_eq!(
            tail_of(&long).len(),
            TAIL_BYTES,
            "expected the last {TAIL_BYTES} bytes of a longer line"
        );
        assert_eq!(tail_of(b""), b"", "expected an empty tail for no bytes");
    }

    #[test]
    fn a_line_that_leaves_no_value_awaited_ends_the_discard_at_its_terminator() {
        let mut discard = Discard::mid_line(b"noise API_KEY=value");
        assert_eq!(
            discard.consume(b"more\r\nnext"),
            Some(5),
            "expected the discard to end right after the CR"
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
}
