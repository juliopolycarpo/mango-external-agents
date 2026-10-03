//! Frozen from the 0.4.1 tag: `redact/scan.rs` as it shipped, tests removed.

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
