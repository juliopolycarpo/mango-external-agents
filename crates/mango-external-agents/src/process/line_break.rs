//! Where the next line of a child's output ends.
//!
//! Every byte a child writes to stdout is searched for a line feed once, so the search is the
//! per-byte cost of framing. `iter().position` compiles to a loop that tests and branches on one
//! byte at a time; the workspace forbids `unsafe`, so there are no intrinsics to reach for, and
//! the search is instead shaped so the compiler can test a block of bytes with vector
//! instructions.

/// Bytes [`first_line_feed`] tests between two chances to stop.
///
/// 32 rather than the 64 of `normalize`'s clean-text scan: a line feed is found by a byte-wise
/// search of the block that holds it, and that search is what a short line pays for.
const SCAN_BLOCK: usize = 32;

/// The index of the first line feed in `bytes`, or `None` when there is none.
///
/// Returns exactly what `bytes.iter().position(|byte| *byte == b'\n')` returns. Whole blocks are
/// tested with no early exit inside one, so the compiler can use vector instructions; the block
/// that holds a line feed, and the bytes past the last whole block, are then searched a byte at a
/// time.
///
/// # Example
///
/// ```ignore
/// assert_eq!(first_line_feed(b"{\"id\":1}\n{\"id\":2}\n"), Some(8));
/// assert_eq!(first_line_feed(b"no terminator yet"), None);
/// ```
pub(super) fn first_line_feed(bytes: &[u8]) -> Option<usize> {
    let (blocks, _) = bytes.as_chunks::<SCAN_BLOCK>();
    let clean = blocks
        .iter()
        .take_while(|block| !holds_line_feed(block.as_slice()))
        .count();
    let from = clean * SCAN_BLOCK;
    let found = bytes[from..].iter().position(|byte| *byte == b'\n');
    found.map(|offset| from + offset)
}

/// Whether any byte is a line feed, joined with `|` so the loop body has no branch.
///
/// A comparison, never a `match` or `matches!`: Rust 1.99 (LLVM 23) stopped vectorizing the
/// pattern form of a byte test; see `docs/benchmarks.md`.
fn holds_line_feed(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .fold(false, |found, byte| found | (*byte == b'\n'))
}

#[cfg(test)]
mod tests {
    use super::{SCAN_BLOCK, first_line_feed, holds_line_feed};

    /// The search this module replaces, kept as the definition of the right answer.
    fn reference(bytes: &[u8]) -> Option<usize> {
        bytes.iter().position(|byte| *byte == b'\n')
    }

    /// No block, one block, two blocks, and every remainder length beside them.
    const LONGEST: usize = 2 * SCAN_BLOCK + 2;

    /// The example in the documentation of [`first_line_feed`], which a private function cannot
    /// run as a doctest.
    #[test]
    fn the_documented_example_holds() {
        assert_eq!(
            first_line_feed(b"{\"id\":1}\n{\"id\":2}\n"),
            Some(8),
            "expected the end of the first of two records"
        );
        assert_eq!(
            first_line_feed(b"no terminator yet"),
            None,
            "expected no line feed in an unterminated line"
        );
    }

    #[test]
    fn finds_no_line_feed_in_text_without_one_at_every_length() {
        for length in 0..=LONGEST {
            let bytes = vec![b'a'; length];
            assert_eq!(
                first_line_feed(&bytes),
                None,
                "expected no line feed in {length} clean bytes"
            );
        }
    }

    #[test]
    fn finds_a_single_line_feed_at_every_offset_of_every_length() {
        for length in 1..=LONGEST {
            for offset in 0..length {
                let mut bytes = vec![b'a'; length];
                bytes[offset] = b'\n';
                assert_eq!(
                    first_line_feed(&bytes),
                    Some(offset),
                    "expected the line feed at offset {offset} of {length} bytes"
                );
            }
        }
    }

    #[test]
    fn the_first_of_two_line_feeds_wins_wherever_the_second_falls() {
        for length in 2..=LONGEST {
            for first in 0..length {
                for second in first + 1..length {
                    let mut bytes = vec![b'a'; length];
                    bytes[first] = b'\n';
                    bytes[second] = b'\n';
                    assert_eq!(
                        first_line_feed(&bytes),
                        Some(first),
                        "expected the first line feed at offset {first}, not the one at \
                         {second}, of {length} bytes"
                    );
                }
            }
        }
    }

    #[test]
    fn matches_the_byte_wise_search_for_bytes_that_resemble_a_line_feed() {
        // Bytes one bit away from 0x0a, its neighbours, and both ends of the range: a comparison
        // done on the wrong lane width or with the wrong sign would take one of these for it.
        let probes = [
            0x00, 0x02, 0x08, 0x09, 0x0b, 0x0d, 0x1a, 0x2a, 0x4a, 0x8a, 0xff,
        ];
        for length in 1..=LONGEST {
            for offset in 0..length {
                for probe in probes {
                    let mut bytes = vec![probe; length];
                    assert_eq!(
                        first_line_feed(&bytes),
                        None,
                        "expected no line feed in {length} bytes of {probe:#04x}"
                    );
                    bytes[offset] = b'\n';
                    assert_eq!(
                        first_line_feed(&bytes),
                        reference(&bytes),
                        "expected the line feed at offset {offset} among {length} bytes of \
                         {probe:#04x}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_block_holds_a_line_feed_only_when_one_of_its_bytes_is_one() {
        assert!(
            !holds_line_feed(&[]),
            "expected no line feed in an empty block"
        );
        for offset in 0..SCAN_BLOCK {
            let mut block = [b'a'; SCAN_BLOCK];
            assert!(
                !holds_line_feed(&block),
                "expected no line feed in a clean block"
            );
            block[offset] = b'\n';
            assert!(
                holds_line_feed(&block),
                "expected the line feed at offset {offset} of a block to be seen"
            );
        }
    }
}
