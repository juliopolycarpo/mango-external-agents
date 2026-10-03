//! Finding the few places in a tail where the redactor has work, without reading every word.
//!
//! Most of a stderr tail is plain text: no control character to take out and no credential to
//! hide. Each credential rule needs a punctuation byte after the name it matches, a `:` or a `=`,
//! and a removed character starts with one of a handful of bytes. Searching for those bytes a
//! block at a time, and trying a rule only at the starts one of them allows, leaves the plain text
//! between them to be copied whole.
//!
//! The workspace forbids `unsafe`, so there are no intrinsics to reach for; the search is shaped
//! so the compiler can test a block of bytes with vector instructions.

use std::ops::Range;

/// Bytes [`first_match`] tests between two chances to stop.
///
/// 32, as in the line-feed search of `process::line_break`: the wanted byte is found by a
/// byte-wise search of the block that holds it, and that search is what dense text pays for.
const SCAN_BLOCK: usize = 32;

/// The index of the first byte at or after `from` that `wanted` accepts, or `None` when there is
/// none.
///
/// Returns exactly what `bytes[from..].iter().position(wanted)` returns, offset by `from`. Whole
/// blocks are tested with no early exit inside one, so the compiler can use vector instructions;
/// the block that holds a wanted byte, and the bytes past the last whole block, are then searched
/// a byte at a time. `wanted` has to be comparisons joined with `|` and `&`, never a `match` or
/// `matches!`: a pattern compiles to a branch per byte, and Rust 1.99 (LLVM 23) no longer removes
/// it; see `docs/benchmarks.md`.
///
/// # Example
///
/// ```ignore
/// assert_eq!(first_match(b"error: no key", 0, |byte| byte == b':'), Some(5));
/// assert_eq!(first_match(b"error: no key", 6, |byte| byte == b':'), None);
/// ```
pub(super) fn first_match(bytes: &[u8], from: usize, wanted: impl Fn(u8) -> bool) -> Option<usize> {
    let rest = bytes.get(from..)?;
    let (blocks, _) = rest.as_chunks::<SCAN_BLOCK>();
    let clean = blocks
        .iter()
        .take_while(|block| {
            !block
                .iter()
                .fold(false, |found, byte| found | wanted(*byte))
        })
        .count();
    let skipped = clean * SCAN_BLOCK;
    let found = rest[skipped..].iter().position(|byte| wanted(*byte));
    found.map(|offset| from + skipped + offset)
}

/// The start of the run of bytes `keep` accepts that ends just before `end`, or `end` when the
/// byte before it is not one.
///
/// # Example
///
/// ```ignore
/// assert_eq!(run_start(b"x  :", 3, |byte| byte == b' '), 1);
/// ```
pub(super) fn run_start(bytes: &[u8], end: usize, keep: impl Fn(u8) -> bool) -> usize {
    let mut start = end.min(bytes.len());
    while let Some(before) = start.checked_sub(1)
        && bytes.get(before).is_some_and(|byte| keep(*byte))
    {
        start = before;
    }
    start
}

/// One byte a rule cannot match without, and the starts a match that uses it can have.
pub(super) struct Anchor {
    /// Where the byte is.
    pub(super) at: usize,
    /// Every index a match reaching this byte can start at. A rule is still asked at each one:
    /// this is a list of places it may match, not of places it does.
    pub(super) starts: Range<usize>,
}

/// The places a rule may match, in ascending order, drawn from one anchor after another.
///
/// `locate(bytes, from)` returns the first anchor at or after `from`. It must report every anchor,
/// and `starts` must hold every start whose match reaches that anchor, or a credential goes
/// unredacted; `redact::differential` holds each rule's `locate` to the scan that tries every
/// word. Anchors are visited once each, so the work is linear in the text as long as `locate`
/// reads only the bytes between the anchor before and the one it returns.
pub(super) struct Candidates<Locate> {
    locate: Locate,
    /// Where the search for the next anchor resumes.
    searched: usize,
    /// The starts of the current anchor not handed out yet.
    starts: Range<usize>,
}

impl<Locate: Fn(&[u8], usize) -> Option<Anchor>> Candidates<Locate> {
    /// Candidates of the rule whose anchors `locate` finds.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut candidates = Candidates::new(locate);
    /// while let Some(at) = candidates.next_from(bytes, from) { /* try the rule at `at` */ }
    /// ```
    pub(super) fn new(locate: Locate) -> Self {
        Self {
            locate,
            searched: 0,
            starts: 0..0,
        }
    }

    /// The next place to try the rule at or after `from`, or `None` when the text has none left.
    ///
    /// `from` never moves backwards between calls: it is one past the last place tried, or the
    /// end of the last match.
    pub(super) fn next_from(&mut self, bytes: &[u8], from: usize) -> Option<usize> {
        loop {
            let start = self.starts.start.max(from);
            if start < self.starts.end {
                self.starts.start = start + 1;
                return Some(start);
            }
            let anchor = (self.locate)(bytes, self.searched.max(from))?;
            self.searched = anchor.at + 1;
            self.starts = anchor.starts;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Anchor, Candidates, SCAN_BLOCK, first_match, run_start};

    fn is_colon(byte: u8) -> bool {
        byte == b':'
    }

    /// No block, one block, two blocks, and every remainder length beside them.
    const LONGEST: usize = 2 * SCAN_BLOCK + 2;

    #[test]
    fn the_documented_examples_hold() {
        assert_eq!(first_match(b"error: no key", 0, is_colon), Some(5));
        assert_eq!(first_match(b"error: no key", 6, is_colon), None);
        assert_eq!(run_start(b"x  :", 3, |byte| byte == b' '), 1);
    }

    #[test]
    fn a_wanted_byte_is_found_where_a_bytewise_search_finds_it_at_every_offset_and_start() {
        for length in 0..=LONGEST {
            let mut bytes = vec![b'a'; length];
            for from in 0..=length + 1 {
                let received = first_match(&bytes, from, is_colon);
                assert_eq!(
                    received, None,
                    "expected no match in {length} clean bytes from {from} | received {received:?}"
                );
            }
            for first in 0..length {
                bytes[first] = b':';
                // A second wanted byte later on must not be the one reported.
                let second = (first + SCAN_BLOCK).min(length - 1);
                bytes[second] = b':';
                for from in 0..=length {
                    let expected = bytes
                        .get(from..)
                        .and_then(|rest| rest.iter().position(|byte| is_colon(*byte)))
                        .map(|offset| from + offset);
                    let received = first_match(&bytes, from, is_colon);
                    assert_eq!(
                        received, expected,
                        "expected {expected:?} for colons at {first} and {second} of {length} \
                         bytes searched from {from} | received {received:?}"
                    );
                }
                bytes[first] = b'a';
                bytes[second] = b'a';
            }
        }
    }

    #[test]
    fn a_run_starts_after_the_last_byte_that_is_not_part_of_it() {
        let is_space = |byte: u8| byte == b' ';
        for (text, end, expected) in [
            (&b"ab   :"[..], 5, 2),
            (b"ab:", 2, 2),
            (b"   :", 3, 0),
            (b"", 0, 0),
            (b"a  ", 9, 1),
        ] {
            let received = run_start(text, end, is_space);
            assert_eq!(
                received, expected,
                "expected the spaces before {end} in {text:?} to start at {expected} | received \
                 {received}"
            );
        }
    }

    /// Anchors at each `:`, each allowing the two bytes before it.
    fn two_before_each_colon(bytes: &[u8], from: usize) -> Option<Anchor> {
        let at = first_match(bytes, from, is_colon)?;
        Some(Anchor {
            at,
            starts: at.saturating_sub(2)..at,
        })
    }

    fn drain(text: &[u8], mut from: usize, step: impl Fn(usize) -> usize) -> Vec<usize> {
        let mut candidates = Candidates::new(two_before_each_colon);
        let mut seen = Vec::new();
        while let Some(at) = candidates.next_from(text, from) {
            seen.push(at);
            from = step(at);
        }
        seen
    }

    #[test]
    fn candidates_come_in_order_from_every_anchor_and_never_before_the_asked_start() {
        let text = b"ab: c:defg:  :";
        let received = drain(text, 0, |at| at + 1);
        assert_eq!(
            received,
            [0, 1, 3, 4, 8, 9, 11, 12],
            "expected the two starts before each colon of {text:?} in order | received \
             {received:?}"
        );

        // A match that ends past an anchor takes that anchor's remaining starts with it.
        let received = drain(text, 0, |at| if at == 3 { 9 } else { at + 1 });
        assert_eq!(
            received,
            [0, 1, 3, 9, 11, 12],
            "expected no start before 9 once a match from 3 ended there in {text:?} | received \
             {received:?}"
        );

        let received = drain(text, 4, |at| at + 1);
        assert_eq!(
            received,
            [4, 8, 9, 11, 12],
            "expected no start before 4 when asked from 4 in {text:?} | received {received:?}"
        );
    }
}
