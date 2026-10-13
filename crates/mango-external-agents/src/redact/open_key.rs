//! Whether the text a stderr tail dropped ended inside a private key.
//!
//! A tail is cut on a line, so a key written over many lines can lose its begin line and keep the
//! lines under it. The block read starts from the begin marker, and the tail no longer has it:
//! [`OpenKey`] remembers that the bytes dropped before the tail's first byte opened a key and did
//! not close it, so the reader can be told the tail starts inside one.

use super::tokens::{KeyMark, MAX_MARK_BYTES, last_key_mark};

/// The bytes of the dropped text kept to find a marker that a drop splits in two.
const CARRY: usize = MAX_MARK_BYTES;

/// How many bytes after a begin marker an open key stays open. A block is closed by an end marker
/// at most 16 KiB after its begin marker, and a key's lines of base64 are 64 or 76 characters, so a
/// key that is still open after this much was a sentence that mentioned a marker, not a key.
const REACH: usize = 64 * 1024;

/// The begin line to put in front of a tail that starts inside a key, so the block read hides the
/// lines under it, up to the end marker, as it would have with the begin line the cut dropped.
pub(crate) const BEGIN_LINE: &str = "-----BEGIN PRIVATE KEY-----\n";

/// The state of the private-key markers in the text dropped so far.
///
/// Fed every byte the tail drops, in order. Its state is two integers, a flag and a fixed carry of
/// [`CARRY`] bytes whatever is fed to it, and feeding it costs a read of the bytes fed.
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
    /// Bytes fed since the begin marker that opened the key.
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

impl OpenKey {
    /// Whether the text fed ends inside a private key: its last marker is a begin marker, which
    /// no end marker has closed and [`REACH`] has not passed.
    pub(crate) fn is_open(&self) -> bool {
        self.open
    }

    /// The marker the state is the result of.
    fn state(&self) -> KeyMark {
        if self.open {
            KeyMark::Begin
        } else {
            KeyMark::End
        }
    }

    /// Reads the next bytes the tail dropped.
    pub(crate) fn feed(&mut self, dropped: &[u8]) {
        if dropped.is_empty() {
            return;
        }
        // A marker the drop splits is read in a window over the end of what was fed and the start
        // of what is fed now; the markers inside `dropped` are read in `dropped` itself.
        let head = dropped.len().min(CARRY);
        let mut window = [0u8; 2 * CARRY];
        window[..self.carried].copy_from_slice(&self.carry[..self.carried]);
        window[self.carried..self.carried + head].copy_from_slice(&dropped[..head]);
        let joined = last_key_mark(&window[..self.carried + head]);
        // The marker the window finds is new only when it is not the state the carry already left.
        let marker = last_key_mark(dropped).or(joined.filter(|mark| *mark != self.state()));
        match marker {
            Some(KeyMark::Begin) => (self.open, self.since) = (true, 0),
            Some(KeyMark::End) => (self.open, self.since) = (false, 0),
            None => self.since = self.since.saturating_add(dropped.len()),
        }
        if self.since > REACH {
            // A key this old was a sentence that mentioned a marker. What the carry holds is not
            // read again, or the marker in it would open the key once more.
            (self.open, self.since, self.carried) = (false, 0, 0);
        }
        self.keep(dropped);
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
