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

use super::strip::strip_marking;
use super::tokens::{KeyMark, MAX_BLOCK_BYTES, MAX_MARK_BYTES, last_key_mark, last_key_mark_after};

/// The raw bytes kept from what was fed, so that a marker, or an escape in front of or inside one,
/// that a drop splits is read whole.
const CARRY: usize = 256;

/// How many bytes of the carry a marker split by a drop can be in.
const WINDOW: usize = MAX_MARK_BYTES;

/// How many bytes of a drop are read at a time.
const BLOCK: usize = 4096;

/// How many bytes after a begin marker an open key stays open: as far as an end marker closes a
/// block. A key that is still open after this much was a sentence that mentioned a marker.
const REACH: usize = MAX_BLOCK_BYTES;

/// The state of the private-key markers in the text dropped so far.
///
/// Fed every byte the tail drops, in order. Its state is a flag, a counter and a fixed carry of
/// [`CARRY`] bytes whatever is fed to it. Text of printable ASCII is searched for `-----` and
/// nothing else; text with anything the stripper takes out is stripped first, with the carry in
/// front of it.
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
#[derive(Debug)]
pub(crate) struct OpenKey {
    open: bool,
    /// Bytes fed since the marker that set the state.
    since: usize,
    carry: [u8; CARRY],
    carried: usize,
}

impl Default for OpenKey {
    fn default() -> Self {
        Self {
            open: false,
            since: 0,
            carry: [0; CARRY],
            carried: 0,
        }
    }
}

/// Whether `bytes` are printable ASCII and line breaks: text the stripper would leave as it is.
fn is_plain(bytes: &[u8]) -> bool {
    // Not `all`: a scan with no early exit is the one the compiler turns into vector code.
    bytes.iter().fold(true, |plain, byte| {
        plain & matches!(byte, b' '..=b'~' | b'\n' | b'\t')
    })
}

/// Whether a drop can end a marker. A marker ends in a hyphen, so a drop with none is skipped by the
/// standard library's byte search. Otherwise, in one pass that the compiler turns into vector code: the
/// closing dashes of a marker are five hyphens after a capital or a space (a label's last
/// character), a byte the stripper takes out can sit inside the dashes or in front of them, and
/// a hyphen first can be the rest of dashes the carry ends with. Without one, no marker ends in
/// the drop. A line of dashes and nothing else is none of these.
fn may_end_a_marker(dropped: &[u8]) -> bool {
    if !dropped.contains(&b'-') {
        return false;
    }
    let plain_byte = |byte: u8| matches!(byte, b' '..=b'~' | b'\n' | b'\t');
    let (dashes, stripped) = dropped
        .windows(6)
        .fold((false, false), |(dashes, stripped), at| {
            let after_label = at[0].is_ascii_uppercase() | (at[0] == b' ');
            let five = (at[1] == b'-') & (at[2] == b'-') & (at[3] == b'-') & (at[4] == b'-');
            (
                dashes | (after_label & five & (at[5] == b'-')),
                stripped | !plain_byte(at[0]),
            )
        });
    // The last five bytes start no window.
    let end = dropped.len().saturating_sub(5);
    dashes
        || stripped
        || dropped.first() == Some(&b'-')
        || !dropped[end..].iter().all(|byte| plain_byte(*byte))
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
    /// costs one pass and a marker costs the read of its block and no more.
    pub(crate) fn feed(&mut self, dropped: &[u8]) {
        for block in dropped.chunks(BLOCK) {
            self.feed_block(block);
        }
    }

    fn feed_block(&mut self, dropped: &[u8]) {
        let kept = &self.carry[..self.carried];
        let marker = if !may_end_a_marker(dropped) {
            None
        } else if is_plain(dropped) && is_plain(kept) {
            // A marker inside `dropped` is read there; one that the drop splits is read in a
            // window over the end of the carry and the start of the drop.
            let tail = &kept[kept.len().saturating_sub(WINDOW)..];
            let mut window = [0u8; 2 * WINDOW];
            let head = dropped.len().min(WINDOW);
            window[..tail.len()].copy_from_slice(tail);
            window[tail.len()..tail.len() + head].copy_from_slice(&dropped[..head]);
            let split = last_key_mark_after(&window[..tail.len() + head], tail.len());
            last_key_mark(dropped).or(split)
        } else if dropped.contains(&b'-') {
            // A marker is dashes, and the stripper adds none: a drop with no hyphen in it ends
            // no marker, whatever the carry holds.
            let joined = [kept, dropped].concat();
            let (plain, _) = strip_marking(&String::from_utf8_lossy(&joined));
            let (before, _) = strip_marking(&String::from_utf8_lossy(kept));
            // The carry may end inside a multibyte character or an escape, which the two decodes
            // read as different lengths; a marker that closes within a few bytes of the end of the
            // carry is read again, and reading it again changes nothing.
            last_key_mark_after(plain.as_bytes(), before.len().saturating_sub(4))
        } else {
            None
        };
        match marker {
            Some(KeyMark::Begin) => (self.open, self.since) = (true, 0),
            Some(KeyMark::End) => (self.open, self.since) = (false, 0),
            None => {}
        }
        // The bytes of the block after the marker are not counted: the count errs towards a key
        // that is still in reach, which hides more and never less.
        if marker.is_none() {
            self.since = self.since.saturating_add(dropped.len());
        }
        self.keep(dropped);
        if self.since > REACH {
            // A key this old was a sentence that mentioned a marker. The carry stays: a marker that
            // starts in it and ends in the next drop is a new one, and the old marker is farther
            // back than the carry reaches.
            (self.open, self.since) = (false, 0);
        }
    }

    /// The last [`CARRY`] bytes of everything fed.
    fn keep(&mut self, dropped: &[u8]) {
        if dropped.len() >= CARRY {
            self.carry
                .copy_from_slice(&dropped[dropped.len() - CARRY..]);
            self.carried = CARRY;
            return;
        }
        let keep = (CARRY - dropped.len()).min(self.carried);
        self.carry.copy_within(self.carried - keep..self.carried, 0);
        self.carry[keep..keep + dropped.len()].copy_from_slice(dropped);
        self.carried = keep + dropped.len();
    }
}

#[cfg(test)]
mod tests {
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
            "loading 10%\r",
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
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
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
                let (piece, mark) = match next(5) {
                    0 => (begins[next(begins.len())].to_owned(), Some(true)),
                    1 => (ends[next(ends.len())].to_owned(), Some(false)),
                    2 => (
                        "x".repeat(if next(6) == 0 { 22_000 } else { 1 + next(900) }) + "\n",
                        None,
                    ),
                    _ => (notes[next(notes.len())].to_owned(), None),
                };
                shape.push((mark, text.len(), piece.len()));
                text.push_str(&piece);
                match mark {
                    Some(open) => state = (open, 0),
                    None => state.1 += piece.len(),
                }
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
        assert_eq!(
            std::mem::size_of_val(&key.carry),
            CARRY,
            "expected a fixed carry of {CARRY} bytes"
        );
    }
}
