//! Searches over generated text, too long for the default run: `cargo nextest run --release
//! --run-ignored only redact::search`.
//!
//! Three properties are asked of every input: no entry point panics (nothing on a stderr tail may
//! take the host down, whatever the bytes), one pass leaves its output alone (a host that redacts
//! again must find nothing more), and the text rules still return what 0.4.1 returned for every
//! input that carries nothing a later rule reads (see `redact::differential`).
//!
//! `MEA_REDACT_SEARCH_COUNT` sets how many inputs each test generates, 2,000 by default so that a
//! plain `--run-ignored all` stays quick; the receipt in `docs/benchmarks.md` was taken with
//! 4,000,000 for the fixed-point search and 3,000,000 for the panic search. Release builds with
//! `CARGO_PROFILE_RELEASE_OVERFLOW_CHECKS=true` also trap an arithmetic overflow. The seed is
//! printed with a failure and `MEA_REDACT_SEARCH_SEED` replays it.

use super::differential::{FRAGMENTS, SHAPES, assert_matches_the_replaced_scan};
use super::{bounded, message_text, stderr_text, structured};
use crate::normalize::TextLimit;
use crate::testing::redaction::{BARE_TOKENS, BENIGN, BENIGN_NEAR_TOKENS, LEAKS};
use serde_json::Value;

const DEFAULT_COUNT: u64 = 2_000;
const THREADS: u64 = 4;

/// How many inputs each test generates: `MEA_REDACT_SEARCH_COUNT`, or [`DEFAULT_COUNT`].
fn count() -> u64 {
    match std::env::var("MEA_REDACT_SEARCH_COUNT") {
        Ok(value) => value.trim().replace('_', "").parse().unwrap_or_else(|_| {
            panic!("expected MEA_REDACT_SEARCH_COUNT to be a whole number | received: {value:?}")
        }),
        Err(_) => DEFAULT_COUNT,
    }
}

fn seed() -> u64 {
    match std::env::var("MEA_REDACT_SEARCH_SEED") {
        Ok(value) => value.trim().parse().unwrap_or_else(|_| {
            panic!("expected MEA_REDACT_SEARCH_SEED to be a whole number | received: {value:?}")
        }),
        Err(_) => 0x9e37_79b9_7f4a_7c15,
    }
}

/// xorshift64*: a fixed sequence for a seed, so a failure replays.
struct Random(u64);

impl Random {
    fn new(seed: u64, thread: u64) -> Self {
        let mut random = Self(seed ^ thread.wrapping_add(1).wrapping_mul(0xd1b5_4a32_d192_ed03));
        for _ in 0..4 {
            random.next();
        }
        random
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() >> 33).map_or(0, |value| value % bound.max(1))
    }

    fn pick<'a, T>(&mut self, from: &'a [T]) -> &'a T {
        &from[self.below(from.len())]
    }
}

/// Bytes the rules, the stripper and the token pass read, and the ones that break text: multibyte
/// characters, lead bytes alone, escapes, NUL, lone backslashes, quotes, brackets, hyphens.
const SOUP: &[u8] = b"authorizationbearerbasicapikeysecrettokenpasswordcredential \
    ::==  \n\n\r\t,;-_.+/@?#19^AUTHZBS\x00\x01\x07\x0b\x0c\x1b\x1b[]\\\\\\()PXcmq\x7f\"\"''{}[]ghp_sk-eyJ.\
    \xc2\x9b\xc2\x9c\xc2\x9d\xc2\x90\xc2\xa0\xc3\xa9\xd8\x9c\xe3\x81\x82\xe2\x80\xae\xe2\x80\x94\xf0\x9f\x98\x80\xff\xc3";

/// Pieces of the well-known tokens, split so no line holds a whole one.
const TOKEN_PIECES: &[&str] = &[
    concat!("gh", "p_"),
    concat!("github_p", "at_"),
    concat!("glp", "at-"),
    concat!("xo", "xb-"),
    concat!("sk", "-ant-"),
    concat!("sk", "-proj-"),
    concat!("sk", "_live_"),
    concat!("AK", "IA"),
    concat!("AI", "za"),
    concat!("ya", "29."),
    concat!("ey", "J"),
    concat!("np", "m_"),
    concat!("-----BEGIN ", "PRIVATE KEY-----"),
    concat!("-----BEGIN ", "RSA PRIVATE KEY-----"),
    "-----END PRIVATE KEY-----",
    "0123456789abcdefghijklmnopqrstuvwxyz",
    "ABCDEFGHIJKLMNOPQRST",
    "0123456789ABCDEFGHIJ",
    "xxxxxxxxxxxxxxxxxxxx",
    "\\n",
    "\\\\n",
    "\n",
    ".",
];

/// One input: fragments of the differential's lists, token pieces and byte soup, mixed.
fn generate(random: &mut Random) -> String {
    let length = 1 + random.below(24);
    let mut text = String::new();
    for _ in 0..length {
        match random.below(10) {
            0..=3 => text.push_str(random.pick(FRAGMENTS)),
            4 => text.push_str(random.pick(SHAPES)),
            5..=6 => text.push_str(random.pick(TOKEN_PIECES)),
            _ => {
                let bytes: Vec<u8> = (0..1 + random.below(12))
                    .map(|_| *random.pick(SOUP))
                    .collect();
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    text
}

/// Runs `check` on `count()` generated inputs on several threads, naming the input that fails.
fn search(check: fn(&str)) {
    let (seed, count) = (seed(), count());
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..THREADS)
            .map(|thread| {
                scope.spawn(move || {
                    let mut random = Random::new(seed, thread);
                    for _ in 0..count / THREADS {
                        let text = generate(&mut random);
                        check(&text);
                    }
                })
            })
            .collect();
        for worker in workers {
            if let Err(panic) = worker.join() {
                std::panic::resume_unwind(panic);
            }
        }
    });
}

fn assert_fixed_point(name: &str, raw: &str, pass: fn(&str) -> String) {
    let once = pass(raw);
    let twice = pass(&once);
    assert!(
        once == twice,
        "expected {name} to be a fixed point | input {raw:?} (seed {}) first pass {once:?}, second pass {twice:?}",
        seed()
    );
}

fn bounded_text(raw: &str) -> String {
    bounded(raw, TextLimit::ErrorMessage).text
}

fn assert_every_entry_point_is_a_fixed_point(raw: &str) {
    assert_fixed_point("stderr_text", raw, stderr_text);
    assert_fixed_point("message_text", raw, message_text);
    assert_fixed_point("bounded", raw, bounded_text);
}

fn assert_nothing_panics(raw: &str) {
    let _ = stderr_text(raw);
    let _ = message_text(raw);
    let _ = bounded(raw, TextLimit::ErrorMessage);
    let value = Value::String(raw.to_owned());
    let _ = structured(&value);
    let _ = super::ends_awaiting_value(raw);
    // The text as a key, in an object and an array, and as JSON inside a string, which the walk
    // parses when it can.
    let wrapped =
        serde_json::json!({ raw: raw, "list": [raw, raw], "text": format!("{{\"k\":\"{raw}\"}}") });
    let _ = structured(&wrapped);
    let embedded = Value::String(format!("[{raw:?},{{\"password\":{raw:?}}}]"));
    let _ = structured(&embedded);
}

#[test]
#[ignore = "a long search; see the module docs"]
fn one_pass_over_generated_text_is_a_fixed_point() {
    search(assert_every_entry_point_is_a_fixed_point);
}

#[test]
#[ignore = "a long search; see the module docs"]
fn no_entry_point_panics_on_generated_text() {
    search(assert_nothing_panics);
}

#[test]
#[ignore = "a long search; see the module docs"]
fn the_text_rules_match_the_replaced_scan_on_generated_text() {
    // The reference is slower, so this leg runs a tenth of the count.
    let (seed, count) = (seed(), count() / 10);
    let mut random = Random::new(seed, 7);
    for _ in 0..count {
        assert_matches_the_replaced_scan(&generate(&mut random));
    }
}

#[test]
fn both_corpora_and_their_pieces_are_fixed_points_and_do_not_panic() {
    let corpus = LEAKS
        .iter()
        .chain(BARE_TOKENS)
        .map(|leak| leak.text)
        .chain(BENIGN.iter().copied())
        .chain(BENIGN_NEAR_TOKENS.iter().copied())
        .chain(FRAGMENTS.iter().copied())
        .chain(SHAPES.iter().copied());
    for text in corpus {
        assert_every_entry_point_is_a_fixed_point(text);
        assert_nothing_panics(text);
        // Every prefix and suffix of it: a stderr tail starts and ends anywhere.
        for (at, _) in text.char_indices() {
            assert_every_entry_point_is_a_fixed_point(&text[at..]);
            assert_nothing_panics(&text[..at]);
        }
    }
}

#[test]
fn the_search_is_deterministic_for_a_seed() {
    let (mut first, mut second) = (Random::new(5, 0), Random::new(5, 0));
    for _ in 0..50 {
        let (a, b) = (generate(&mut first), generate(&mut second));
        assert_eq!(
            a, b,
            "expected one seed to generate one sequence | received: {a:?} then {b:?}"
        );
    }
}

#[test]
fn the_generator_makes_multibyte_escape_and_backslash_text() {
    let mut random = Random::new(1, 0);
    let all: String = (0..2_000).map(|_| generate(&mut random)).collect();
    for (what, found) in [
        (
            "a multibyte character",
            all.chars().any(|c| c.len_utf8() > 1),
        ),
        ("an escape", all.contains('\u{1b}')),
        ("a NUL", all.contains('\0')),
        (
            "a lone backslash",
            all.contains("\\ ") || all.contains("\\\\"),
        ),
        ("a replacement character", all.contains('\u{fffd}')),
    ] {
        assert!(
            found,
            "expected generated text to hold {what} | received none in 2,000 inputs"
        );
    }
}
