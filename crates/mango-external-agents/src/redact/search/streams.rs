//! The tail against the stream: what `StderrTail::read` shows of a stream that was pushed in
//! pieces and cut to a capacity must not be a canary that `redact::stderr_tail` hides when it reads
//! the whole stream pushed so far.
//!
//! The stream is lines: filler, tokens, assignments, and private keys (several labels, with and
//! without an end marker, with a prefix on every line or on none, LF or CRLF), each canary named
//! `QXZ<n>J` and unique to the stream. A tail is read after every push, so a read in the middle of
//! a key is held to the same rule as the last one.
//!
//! One-sided on purpose: the tail may hide more than the whole stream (it drops lines at a cut),
//! never less.

use super::{Random, labels};
use crate::StderrTail;
use crate::redact::stderr_tail;

const PREFIXES: [&str; 6] = [
    "",
    "",
    "stderr | ",
    "2026-10-13T10:00:00Z [agent] ",
    "  ",
    "> ",
];
const KEY_LABELS: [&str; 7] = [
    "PRIVATE KEY",
    "RSA PRIVATE KEY",
    "ENCRYPTED PRIVATE KEY",
    "OPENSSH PRIVATE KEY",
    "PGP PRIVATE KEY BLOCK",
    "PUBLIC KEY",
    "CERTIFICATE",
];
const CAPACITIES: [usize; 11] = [8, 16, 32, 48, 64, 96, 128, 200, 300, 600, 1200];

/// A stream, the capacity to cut it to, and the sizes it is pushed in.
pub(super) struct Stream {
    pub(super) text: String,
    pub(super) capacity: usize,
    pub(super) pushes: Vec<usize>,
}

/// A canary line of base64: long enough to be a body line, and unique.
fn body(n: usize) -> String {
    format!("AAAAAAAAAAAAAAAAQXZ{n}J==")
}

fn marker(kind: &str, label: &str) -> String {
    format!("-----{kind} {label}-----")
}

/// One stream from `random`.
pub(super) fn stream(random: &mut Random) -> Stream {
    let prefix = *random.pick(&PREFIXES);
    let eol = if random.below(4) == 0 { "\r\n" } else { "\n" };
    let mut text = String::new();
    let mut canary = 0;
    let mut line = |text: &mut String, with: &str| {
        text.push_str(prefix);
        text.push_str(with);
        text.push_str(eol);
    };
    for _ in 0..1 + random.below(7) {
        canary += 1;
        match random.below(6) {
            0 => line(&mut text, &format!("filler line {canary} is fine")),
            1 => line(
                &mut text,
                &format!("push rejected for ghp_0123456789abcdefghijklmnQXZ{canary}Jabcdef"),
            ),
            2 => line(&mut text, &format!("password=QXZ{canary}J")),
            _ => {
                let label = *random.pick(&KEY_LABELS);
                line(&mut text, &marker("BEGIN", label));
                for _ in 0..random.below(9) {
                    canary += 1;
                    line(&mut text, &body(canary));
                }
                if random.below(4) != 0 {
                    let end = if random.below(8) == 0 {
                        *random.pick(&KEY_LABELS)
                    } else {
                        label
                    };
                    line(&mut text, &marker("END", end));
                }
            }
        }
    }
    let capacity = *random.pick(&CAPACITIES);
    let mut pushes = Vec::new();
    let mut left = text.len();
    while left > 0 {
        let size = match random.below(4) {
            0 => 1,
            1 => 2 + random.below(40),
            2 => 28 + random.below(3),
            _ => left,
        }
        .min(left);
        pushes.push(size);
        left -= size;
    }
    Stream {
        text,
        capacity,
        pushes,
    }
}

/// Pushes `stream` and holds every read to the whole stream's reading.
pub(super) fn assert_the_tail_hides_what_the_stream_hides(stream: &Stream) {
    let tail = StderrTail::with_capacity(stream.capacity);
    let bytes = stream.text.as_bytes();
    let mut pushed = 0;
    for size in &stream.pushes {
        tail.push(&bytes[pushed..pushed + size]);
        pushed += size;
        let shown = tail.read();
        let whole = stderr_tail(&String::from_utf8_lossy(&bytes[..pushed]));
        let (shown_labels, whole_labels) = (labels(&shown), labels(&whole));
        for canary in shown_labels {
            if whole_labels.contains(&canary) {
                continue;
            }
            panic!(
                "expected {canary} hidden by the tail, as the whole stream hides it | capacity {}, pushes {:?}, pushed {pushed} bytes of {:?}, tail read {shown:?}, whole stream {whole:?}",
                stream.capacity, stream.pushes, stream.text
            );
        }
    }
}
