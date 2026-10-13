//! Whether the text a stderr tail dropped ended inside a private key.
//!
//! A tail is cut on a line, so a key written over many lines can lose its begin line and keep the
//! lines under it. The block read starts from the begin marker, and the tail no longer has it:
//! [`OpenKey`] remembers that the bytes dropped before the tail's first byte opened a key and did
//! not close it, so the reader can be told the tail starts inside one
//! (`redact::stderr_tail_inside_key`).
//!
//! The markers are looked for in the text as the redactor reads it, with the escapes and the
//! control characters the stripper takes out removed: `ESC[36m-----BEGIN PRIVATE KEY-----` is a
//! begin marker to the redactor, so it is one here.
//!
//! No escape and no marker spans a line feed (a string escape that meets one is not an escape),
//! so the state keeps the line being written, and reads that line again, whole, whenever a drop
//! can change what it says. A marker split by a drop, behind an escape that the drop split, or in
//! a line that an unfinished escape has not yet explained is read as the whole line reads.

use super::strip::{MAX_ESCAPE_BYTES, may_strip, starts_a_removed_character, strip_marking};
use super::tokens::{KeyMark, MAX_BLOCK_BYTES, MAX_MARK_BYTES, key_marks};

/// The longest stretch of one line kept: a string escape taken out whole, the marker it can sit in
/// front of or inside, and a byte either side. A line is longer than this only when it is not a
/// log line; the part before the last `CARRY` bytes is read once and forgotten.
const CARRY: usize = MAX_ESCAPE_BYTES + MAX_MARK_BYTES + 8;

/// How many bytes of a drop are read at a time.
const BLOCK: usize = 4096;

/// How many bytes after a begin marker an open key stays open: as far as an end marker closes a
/// block. A key that is still open after this much was a sentence that mentioned a marker.
const REACH: usize = MAX_BLOCK_BYTES;

/// The state of the private-key markers in the text dropped so far.
///
/// Fed every byte the tail drops, in order. Its state is two flags, a counter and the line being
/// written, of at most twice [`CARRY`] bytes whatever is fed to it. A drop with nothing in it that
/// can end a marker costs one pass; one that can is read together with the line it continues.
///
/// # Example
///
/// ```ignore
/// let mut key = OpenKey::default();
/// key.feed(b"-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg\n");
/// assert!(key.is_open());
/// key.feed(b"-----END PRIVATE KEY-----\n");
/// assert!(!key.is_open());
/// ```
#[derive(Debug, Default)]
pub(crate) struct OpenKey {
    /// Whether the text fed so far, the line being written included, ends inside a key.
    open: bool,
    /// Whether the key was open when the line being written began.
    open_at_line_start: bool,
    line_marked: bool,
    /// Whether the line being written holds a byte the stripper acts on, so that an escape in it
    /// may be unfinished and a drop that finishes it can put a marker behind it.
    line_has_escape: bool,
    /// Bytes fed since the marker that set the state.
    since: usize,
    /// The line being written, or its last [`CARRY`] bytes and more.
    line: Vec<u8>,
}

impl OpenKey {
    /// Whether the text fed ends inside a private key: its last marker is a begin marker, which
    /// no end marker has closed and [`REACH`] has not passed.
    #[cfg(test)]
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// How many bytes after the text fed so far still lie within the reach of the begin marker
    /// that opened the key, or `None` when no key is open. A block closes a key only when its end
    /// marker starts within that many bytes of the begin marker.
    pub(crate) fn left(&self) -> Option<usize> {
        self.open.then(|| REACH.saturating_sub(self.since))
    }

    /// Reads the next bytes the tail dropped.
    ///
    /// A long drop is read a block at a time, so that a block with nothing that can end a marker
    /// costs one pass and a marker costs the read of its block and the line before it.
    pub(crate) fn feed(&mut self, dropped: &[u8]) {
        for block in dropped.chunks(BLOCK) {
            self.feed_block(block);
        }
    }

    fn feed_block(&mut self, block: &[u8]) {
        let can_change = self.line_marked
            || (self.line_has_escape && block.contains(&b'-'))
            || may_end_a_marker(block);
        let marked = if can_change {
            self.read_with_line(block)
        } else {
            false
        };
        if !marked {
            self.since = self.since.saturating_add(block.len());
        }
        self.keep(block);
        if self.since > REACH {
            // A key this old was a sentence that mentioned a marker.
            (self.open, self.open_at_line_start, self.since) = (false, false, 0);
        }
    }

    /// Reads `block` after the line it continues, and sets the state to what the whole text says.
    /// Whether the text holds a marker.
    fn read_with_line(&mut self, block: &[u8]) -> bool {
        let joined = [self.line.as_slice(), block].concat();
        let stripped;
        let text: &[u8] = if may_strip(&joined) {
            stripped = strip_marking(&String::from_utf8_lossy(&joined)).0;
            stripped.as_bytes()
        } else {
            &joined
        };
        // A string escape does not span a line feed, so the text up to the last one is lines that
        // are over, and what follows is the line still being written.
        let last_feed = text.iter().rposition(|byte| *byte == b'\n');
        let split = last_feed.map_or(0, |at| at + 1);
        let (done_mark, current_mark) = key_marks(text, 0, split);
        // The marker that decides the state is the last one, in the line being written when it
        // has one and in the lines before it otherwise.
        let any = current_mark.or(done_mark);
        let open_at_current = mark_state(done_mark, self.open_at_line_start);
        self.open = mark_state(any, self.open_at_line_start);
        self.line_marked = current_mark.is_some();
        if last_feed.is_some() {
            self.open_at_line_start = open_at_current;
        }
        if any.is_some() {
            self.since = 0;
        }
        any.is_some()
    }

    /// Keeps the line being written: what follows the last line feed of everything fed.
    fn keep(&mut self, block: &[u8]) {
        match block.iter().rposition(|byte| *byte == b'\n') {
            Some(at) => {
                self.line.clear();
                self.line.extend_from_slice(&block[at + 1..]);
                self.line_has_escape = block[at + 1..]
                    .iter()
                    .any(|b| starts_a_removed_character(*b));
                // The lines that ended are over: what the state says now is the state at the
                // start of the next one, unless the line being written says otherwise.
                if !self.line_marked {
                    self.open_at_line_start = self.open;
                }
            }
            None => {
                self.line.extend_from_slice(block);
                self.line_has_escape |= may_strip(block);
            }
        }
        self.cap_line();
    }

    /// Forgets the start of a line longer than twice [`CARRY`], after taking what it says.
    fn cap_line(&mut self) {
        if self.line.len() <= 2 * CARRY {
            return;
        }
        self.open_at_line_start = self.open;
        self.line_marked = false;
        self.line.drain(..self.line.len() - CARRY);
    }
}

/// The state a marker leaves: open after a begin marker, closed after an end marker, and what it
/// was before when there is none.
fn mark_state(mark: Option<KeyMark>, before: bool) -> bool {
    match mark {
        Some(KeyMark::Begin) => true,
        Some(KeyMark::End) => false,
        None => before,
    }
}

/// Whether a drop can end a marker. A marker ends in a hyphen, so a drop with none is skipped by
/// the standard library's byte search. Otherwise, in one pass that the compiler turns into vector
/// code: the closing dashes of a marker are five hyphens after a capital or a space (a label's
/// last character), a byte the stripper can take out sits inside the dashes or in front of them,
/// and a hyphen first can be the rest of dashes the line ends with. Without one, no marker ends
/// in the drop. A line of dashes and nothing else is none of these.
fn may_end_a_marker(dropped: &[u8]) -> bool {
    if !dropped.contains(&b'-') {
        return false;
    }
    let dashes = dropped.windows(6).fold(false, |found, at| {
        let after_label = at[0].is_ascii_uppercase() | (at[0] == b' ');
        let five = (at[1] == b'-') & (at[2] == b'-') & (at[3] == b'-') & (at[4] == b'-');
        found | (after_label & five & (at[5] == b'-'))
    });
    // The last five bytes start no window.
    let end = dropped.len().saturating_sub(5);
    dashes
        || dropped.first() == Some(&b'-')
        // The rest of a character that the block before cut in two.
        || dropped.first().is_some_and(|byte| byte & 0xc0 == 0x80)
        || dropped[end..]
            .iter()
            .any(|byte| starts_a_removed_character(*byte))
        || may_strip(dropped)
}

#[cfg(test)]
mod tests {
    use super::super::tokens::last_key_mark;
    use super::*;

    const BEGIN: &[u8] = b"-----BEGIN PRIVATE KEY-----\n";
    const END: &[u8] = b"-----END PRIVATE KEY-----\n";

    fn fed(parts: &[&[u8]]) -> OpenKey {
        let mut key = OpenKey::default();
        for part in parts {
            key.feed(part);
        }
        key
    }

    #[test]
    fn a_begin_marker_opens_a_key_and_an_end_marker_closes_it() {
        assert!(
            fed(&[BEGIN]).is_open(),
            "expected open after a begin marker"
        );
        assert!(
            !fed(&[BEGIN, b"QUJD\n", END]).is_open(),
            "expected closed after the end marker"
        );
        assert!(
            fed(&[BEGIN, END, BEGIN]).is_open(),
            "expected a second key to open again"
        );
        assert!(
            !fed(&[b"plain\n"]).is_open(),
            "expected closed with no marker"
        );
    }

    #[test]
    fn a_public_key_and_a_certificate_open_nothing() {
        for label in ["PUBLIC KEY", "CERTIFICATE", "CERTIFICATE REQUEST"] {
            let marker = format!("-----BEGIN {label}-----\nQUJD\n");
            assert!(
                !fed(&[marker.as_bytes()]).is_open(),
                "expected `{label}` not to open a key | received open"
            );
        }
    }

    #[test]
    fn a_marker_split_by_a_drop_is_read_at_every_split_point() {
        let all: Vec<u8> = [BEGIN, b"QUJD\n"].concat();
        for cut in 0..=all.len() {
            let key = fed(&[&all[..cut], &all[cut..]]);
            assert!(
                key.is_open(),
                "expected open when the drop splits at {cut} | received closed"
            );
        }
        let one_byte: Vec<&[u8]> = all.chunks(1).collect();
        assert!(
            fed(&one_byte).is_open(),
            "expected open with one-byte drops"
        );
        let closed: Vec<u8> = [BEGIN, b"QUJD\n", END].concat();
        let one_byte: Vec<&[u8]> = closed.chunks(1).collect();
        assert!(
            !fed(&one_byte).is_open(),
            "expected closed with one-byte drops"
        );
    }

    #[test]
    fn a_marker_behind_an_escape_or_with_one_inside_is_a_marker() {
        for text in [
            "\u{1b}[36m-----BEGIN PRIVATE KEY-----\n",
            "\u{1b}[1m-----BEGIN PRIVATE KEY-----\u{1b}[0m\n",
            "-----BEGIN \u{1b}[0mPRIVATE KEY-----\n",
            "-----BEGIN PRIVATE\u{1b}[0m KEY-----\n",
            "\u{1b}]0;title\u{7}-----BEGIN PRIVATE KEY-----\n",
            "\u{0}-----BEGIN PRIVATE KEY-----\n",
        ] {
            assert!(
                fed(&[text.as_bytes()]).is_open(),
                "expected {text:?} to open a key | received closed"
            );
            let one_piece = [text.as_bytes(), b"QUJD\n", b"\x1b[36m", END].concat();
            assert!(
                !fed(&[&one_piece]).is_open(),
                "expected the end marker behind an escape to close it | received open"
            );
        }
    }

    #[test]
    fn only_a_drop_that_can_end_a_marker_is_read() {
        for (drop, expected) in [
            ("2026-10-13T10:00:00Z request 7 finished in 12ms\n", false),
            ("a-b -c d- e--f ---- g\n", false),
            ("x\n-----\n", false),
            ("x\n----------------------------------------\n", false),
            ("-----BEGIN PRIVATE KEY-----\n", true),
            ("PRIVATE KEY-----", true),
            ("stderr | -----", true),
            ("-- and the rest of the dashes", true),
            ("text\u{1b}[0m---", true),
            ("text\u{7}-", true),
            ("text\u{7}\u{1b}[0m", false),
            ("-----", true),
            ("abc", false),
            ("", false),
        ] {
            assert_eq!(
                may_end_a_marker(drop.as_bytes()),
                expected,
                "expected {drop:?} to be {}",
                if expected { "read" } else { "skipped" }
            );
        }
    }

    /// The oracle: after any feed sequence, the state is open exactly when the last private-key
    /// marker of the whole history is a begin marker and no more than [`REACH`] bytes follow it,
    /// whatever the pieces are written with (escapes before and inside a marker, multibyte text,
    /// dashes that are not a marker) and wherever the feeds split them (inside a marker, an escape
    /// or a character; across the carry and the block).
    #[test]
    fn the_state_is_open_exactly_when_the_last_marker_is_a_begin_within_reach() {
        let begin = "-----BEGIN PRIVATE KEY-----\n";
        let begin_rsa = "-----BEGIN RSA PRIVATE KEY-----\n";
        let end = "-----END PRIVATE KEY-----\n";
        let end_ec = "-----END EC PRIVATE KEY-----\n";
        let colour_begin = "\u{1b}[36m-----BEGIN PRIVATE KEY-----\n";
        let inside_begin = "-----BEGIN \u{1b}[0mPRIVATE KEY-----\n";
        let bold_begin = "\u{1b}[1m-----BEGIN OPENSSH PRIVATE KEY-----\u{1b}[0m\n";
        let title_end = "\u{1b}]0;title\u{7}-----END PRIVATE KEY-----\n";
        let wide_begin = "\u{2713} -----BEGIN PRIVATE KEY-----\n";
        let notes: [&str; 11] = [
            "-----BEGIN CERTIFICATE-----\n",
            "-----BEGIN PUBLIC KEY-----\n",
            "-----END CERTIFICATE-----\n",
            "----------------------------------------\n",
            "-----BEGIN PRIVATE KEY----\n",
            "na\u{ef}ve caf\u{e9} \u{65e5}\u{672c}\u{8a9e} \u{2713} done\n",
            "request id 7f3c2a1e-9b4d-4c8a-a1b2-0123456789ab\n",
            "loading 10%\r\n",
            "\u{1f980}\u{1f980} e\u{301}e\u{301}\n",
            "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo=\n",
            "e--f -- ---- -----\n",
        ];
        let begins = [
            begin,
            begin_rsa,
            colour_begin,
            inside_begin,
            bold_begin,
            wide_begin,
        ];
        let ends = [end, end_ec, title_end];
        let escape_sizes = [0, 200, 300, 600, 2000, 4095, 4096, 4097, 6000];
        let string_escape = |kind: usize, size: usize| {
            let payload = "t".repeat(size);
            match kind {
                0 => format!("\u{1b}]0;{payload}\u{7}"),
                1 => format!("\u{1b}]8;;http://e/{payload}\u{1b}\\text\u{1b}]8;;\u{1b}\\"),
                2 => format!("\u{1b}P{payload}\u{1b}\\"),
                3 => format!("\u{1b}]0;{payload}"),
                4 => format!("\u{9d}0;{payload}\u{9c}"),
                _ => format!("\u{1b}]0;{payload}\u{1b}\\"),
            }
        };
        let mut seed: u64 = std::env::var("MEA_OPEN_KEY_SEED")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0x2545_f491_4f6c_dd1d_u64);
        let mut next = move |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % below as u64).unwrap_or(0)
        };
        let sizes = [
            1,
            2,
            3,
            7,
            55,
            56,
            57,
            64,
            255,
            256,
            257,
            1000,
            4095,
            4096,
            4097,
            usize::MAX,
        ];
        let mut open_cases = 0;
        let cases: usize = std::env::var("MEA_OPEN_KEY_CASES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4000);
        for case in 0..cases {
            let mut text = String::new();
            let mut state = (false, 0usize); // open, raw bytes since the last marker
            let mut shape = Vec::new();
            for _ in 0..1 + next(8) {
                let mut piece = match next(5) {
                    0 => begins[next(begins.len())].to_owned(),
                    1 => ends[next(ends.len())].to_owned(),
                    2 => "x".repeat(if next(6) == 0 { 22_000 } else { 1 + next(900) }) + "\n",
                    _ => notes[next(notes.len())].to_owned(),
                };
                // A marker behind a long string escape, or with a dash glued in front of it.
                if piece.starts_with("-----") || piece.contains("-----BEGIN") {
                    match next(10) {
                        0 => {
                            let size = escape_sizes[next(escape_sizes.len())];
                            piece = format!("{}{piece}", string_escape(next(6), size));
                        }
                        1 => piece = format!("-{piece}"),
                        // A long line in front of it, as a progress bar or a minified blob is.
                        2 => piece = format!("{}{piece}", "x".repeat(next(9000))),
                        // The marker inside a string escape the same line ends, which hides it.
                        3 => {
                            let size = escape_sizes[next(escape_sizes.len())];
                            let marker = piece.trim_end();
                            piece = format!("\u{1b}]0;{}{marker}\u{7}\n", "t".repeat(size));
                        }
                        _ => {}
                    }
                }
                // What the whole piece says, read as the redactor reads it. Every piece is a line.
                let mark = last_key_mark(strip_marking(&piece).0.as_bytes())
                    .map(|mark| mark == KeyMark::Begin);
                shape.push((mark, text.len(), piece.len()));
                text.push_str(&piece);
                match mark {
                    Some(open) => state = (open, 0),
                    None => state.1 += piece.len(),
                }
            }
            // The count of bytes since a marker leaves out the rest of the block that held it
            // (it errs towards open), so a key that expires within two blocks of the reach is
            // either answer.
            if state.0 && state.1 > REACH - 2 * BLOCK && state.1 <= REACH + 2 * BLOCK {
                continue;
            }
            let expected = state.0 && state.1 <= REACH;
            open_cases += usize::from(expected);
            for _ in 0..3 {
                let mut key = OpenKey::default();
                let mut rest = text.as_bytes();
                let mut splits = Vec::new();
                while !rest.is_empty() {
                    let size = sizes[next(sizes.len())].min(rest.len());
                    splits.push(size);
                    key.feed(&rest[..size]);
                    rest = &rest[size..];
                }
                assert_eq!(
                    key.is_open(),
                    expected,
                    "expected the key {} after case {case} | received {} | splits {splits:?} | pieces {shape:?} | history {:?}",
                    if expected { "open" } else { "closed" },
                    if expected { "closed" } else { "open" },
                    text.chars().take(160).collect::<String>()
                        + &format!(" ... {} bytes", text.len())
                );
            }
        }
        assert!(
            open_cases > cases / 20,
            "expected the oracle to reach the open state | received {open_cases} of {cases}"
        );
    }

    /// What a reader that strips every block says: the strip path is taken only when a block holds
    /// something the stripper acts on, and the answer must not depend on which path a block took.
    #[test]
    fn skipping_the_strip_of_text_it_leaves_alone_changes_no_answer() {
        let pool = [
            "-----BEGIN PRIVATE KEY-----\n",
            "-----BEGIN \u{1b}[0mPRIVATE KEY-----\n",
            "-----END PRIVATE KEY-----\n",
            "\u{1b}[36m",
            "\u{1b}]0;title\u{7}",
            "\u{9d}0;t\u{9c}",
            "\u{200e}",
            "\u{2066}",
            "\u{61c}",
            "\u{85}",
            "\u{2713} ",
            "\u{2014}",
            "na\u{ef}ve ",
            "\u{65e5}\u{672c}\u{8a9e}",
            "\u{1f980}",
            "-----",
            "---",
            "-",
            "BEGIN ",
            "line of text ",
            "\n",
            "\r",
        ];
        let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
        let mut next = move |below: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            usize::try_from(seed % below as u64).unwrap_or(0)
        };
        for case in 0..20_000 {
            let text: String = (0..1 + next(14)).map(|_| pool[next(pool.len())]).collect();
            let mark = last_key_mark(strip_marking(&text).0.as_bytes());
            let expected = mark == Some(KeyMark::Begin);
            let mut key = OpenKey::default();
            let mut rest = text.as_bytes();
            let mut splits = Vec::new();
            while !rest.is_empty() {
                let size = (1 + next(9)).min(rest.len());
                splits.push(size);
                key.feed(&rest[..size]);
                rest = &rest[size..];
            }
            assert_eq!(
                key.is_open(),
                expected,
                "expected the key {} after case {case} | received {} | splits {splits:?} | text {text:?}",
                if expected { "open" } else { "closed" },
                if expected { "closed" } else { "open" },
            );
        }
    }

    #[test]
    fn a_marker_split_where_the_old_key_expires_is_still_read_whole() {
        let mut first = BEGIN.to_vec();
        first.resize(6 * BLOCK - 20, b'x');
        first.extend_from_slice(b"\n-----BEGIN PRIVATE");
        let key = fed(&[&first, b" KEY-----\n"]);
        assert!(
            key.is_open(),
            "expected the second key open although the first expired inside the block that holds the start of its marker | received closed"
        );
    }

    #[test]
    fn a_marker_after_multibyte_text_is_read_in_a_second_feed() {
        // The search for a split marker starts a few dozen bytes before the end of the carry, at
        // every alignment against the three-byte character at the start of it.
        for padding in 0..8 {
            let first = format!(
                "\u{2713} done\nrequest id 7f3c2a1e-9b4d-4c8a-a1b2-0123456789ab{}\n",
                "a".repeat(padding)
            );
            let key = fed(&[first.as_bytes(), BEGIN]);
            assert!(
                key.is_open(),
                "expected open after multibyte text and a begin marker in a second feed, padding {padding} | received closed"
            );
        }
    }

    #[test]
    fn an_escape_split_by_a_drop_does_not_hide_the_marker_behind_it() {
        let all = "\u{1b}[36m-----BEGIN PRIVATE \u{1b}[0mKEY-----\nQUJD\n".as_bytes();
        for cut in 0..=all.len() {
            let key = fed(&[&all[..cut], &all[cut..]]);
            assert!(
                key.is_open(),
                "expected open when the drop splits at {cut} of {all:?} | received closed"
            );
        }
        let one_byte: Vec<&[u8]> = all.chunks(1).collect();
        assert!(
            fed(&one_byte).is_open(),
            "expected open with one-byte drops"
        );
    }

    #[test]
    fn a_key_left_open_past_the_reach_of_a_block_expires() {
        let mut key = fed(&[BEGIN]);
        key.feed(&vec![b'x'; REACH + 1]);
        assert!(
            !key.is_open(),
            "expected a key open past {REACH} bytes to expire | received open"
        );
    }

    #[test]
    fn the_state_is_bounded_whatever_is_fed() {
        let mut key = OpenKey::default();
        key.feed(&vec![b'-'; 10 * 1024 * 1024]);
        assert!(!key.is_open());
        assert!(
            key.line.len() <= 2 * CARRY,
            "expected the line kept to stay within {} bytes | received {}",
            2 * CARRY,
            key.line.len()
        );
        for _ in 0..10_000 {
            key.feed(b"x");
        }
        assert!(
            key.line.len() <= 2 * CARRY,
            "expected a one-byte feed to stay bounded"
        );
    }
}
