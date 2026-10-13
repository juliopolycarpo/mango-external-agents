//! Well-known credentials with no name beside them, pinned one prefix at a time.
//!
//! The table below is the specification: a prefix, what its body is made of, and the fewest
//! characters that make it a token. The tests build a token for each row at that length and one
//! character below it, behind a letter and behind a space, so a rule that matches a bare prefix, or
//! drifts from the table, fails with the row it drifted on. The corpus in `testing::redaction` holds
//! the tokens as vendors print them and the ordinary text that comes close.
//!
//! The prefixes are read by `redact::bounded`, `redact::structured` and `redact::stderr_tail`, and
//! not by `stderr_text`.

use serde_json::json;

use super::{bounded, stderr_tail, stderr_text, structured};
use crate::normalize::TextLimit;
use crate::testing::redaction::{BARE_TOKENS, BENIGN, BENIGN_NEAR_TOKENS, LEAKS};

const ALNUM: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
const URL_SAFE: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-_";
const SLACK: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ-";
const GITHUB_PAT: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_";
const HEX: &str = "0123456789abcdef";
const UPPER: &str = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// How a body ends: any length from the minimum up, or exactly the minimum.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Length {
    AtLeast,
    Exactly,
}

/// A prefix, what follows it, and how much of it makes a token.
struct Row {
    prefix: &'static str,
    alphabet: &'static str,
    min: usize,
    length: Length,
}

const fn row(prefix: &'static str, alphabet: &'static str, min: usize, length: Length) -> Row {
    Row {
        prefix,
        alphabet,
        min,
        length,
    }
}

/// Each prefix is split, so no line of this file holds a complete token prefix.
fn rows() -> Vec<Row> {
    use Length::{AtLeast, Exactly};
    vec![
        row(concat!("gh", "p_"), ALNUM, 20, AtLeast),
        row(concat!("gh", "o_"), ALNUM, 20, AtLeast),
        row(concat!("gh", "u_"), ALNUM, 20, AtLeast),
        row(concat!("gh", "s_"), ALNUM, 20, AtLeast),
        row(concat!("gh", "r_"), ALNUM, 20, AtLeast),
        row(concat!("github_p", "at_"), GITHUB_PAT, 22, AtLeast),
        row(concat!("glp", "at-"), URL_SAFE, 20, AtLeast),
        row(concat!("xo", "xb-"), SLACK, 20, AtLeast),
        row(concat!("xo", "xp-"), SLACK, 20, AtLeast),
        row(concat!("xo", "xa-"), SLACK, 20, AtLeast),
        row(concat!("xo", "xs-"), SLACK, 20, AtLeast),
        row(concat!("xo", "xr-"), SLACK, 20, AtLeast),
        row(concat!("s", "k-ant-api03-"), URL_SAFE, 80, AtLeast),
        row(concat!("s", "k-ant-admin01-"), URL_SAFE, 80, AtLeast),
        row(concat!("s", "k-proj-"), URL_SAFE, 40, AtLeast),
        row(concat!("s", "k-svcacct-"), URL_SAFE, 40, AtLeast),
        row(concat!("s", "k-admin-"), URL_SAFE, 40, AtLeast),
        row(concat!("s", "k-live-"), URL_SAFE, 16, AtLeast),
        row(concat!("s", "k-"), ALNUM, 40, AtLeast),
        row(concat!("s", "k_live_"), ALNUM, 16, AtLeast),
        row(concat!("s", "k_test_"), ALNUM, 16, AtLeast),
        row(concat!("r", "k_live_"), ALNUM, 16, AtLeast),
        row(concat!("r", "k_test_"), ALNUM, 16, AtLeast),
        row(concat!("AK", "IA"), UPPER, 16, Exactly),
        row(concat!("AS", "IA"), UPPER, 16, Exactly),
        row(concat!("AI", "za"), URL_SAFE, 35, AtLeast),
        row(concat!("ya", "29."), URL_SAFE, 30, AtLeast),
        row(concat!("np", "m_"), ALNUM, 30, AtLeast),
        row(concat!("h", "f_"), ALNUM, 30, AtLeast),
        row(concat!("do", "p_v1_"), HEX, 40, AtLeast),
        row(concat!("do", "o_v1_"), HEX, 40, AtLeast),
        row(concat!("do", "r_v1_"), HEX, 40, AtLeast),
        row(concat!("sh", "pat_"), HEX, 32, AtLeast),
        row(concat!("sh", "pss_"), HEX, 32, AtLeast),
        row(concat!("sh", "pca_"), HEX, 32, AtLeast),
        row(concat!("sh", "ppa_"), HEX, 32, AtLeast),
    ]
}

/// `len` characters of `alphabet`, starting with a letter and holding a digit from the third
/// character on, so that it reads as random text and not as a word.
fn body(alphabet: &str, len: usize) -> String {
    let letters: Vec<char> = alphabet.chars().filter(char::is_ascii_alphabetic).collect();
    let digits: Vec<char> = alphabet.chars().filter(char::is_ascii_digit).collect();
    (0..len)
        .map(|at| match at {
            0 | 1 => letters[at % letters.len()],
            _ if at % 3 == 0 => digits[at % digits.len()],
            _ => letters[at % letters.len()],
        })
        .collect()
}

/// What the message of a vendor is shown as.
fn shown(text: &str) -> String {
    bounded(text, TextLimit::ErrorMessage).text
}

/// Fails, naming the row and the text, unless `text` comes back with `token` gone.
fn assert_redacted(what: &str, text: &str, token: &str) {
    let received = shown(text);
    assert!(
        !received.contains(token) && received.contains("[REDACTED]"),
        "expected {what} redacted | input {text:?} received: {received:?}"
    );
}

/// Fails, naming the row and the text, unless `text` comes back as it was.
fn assert_kept(what: &str, text: &str) {
    let received = shown(text);
    assert_eq!(
        received, text,
        "expected {what} left alone | input {text:?} received: {received:?}"
    );
}

#[test]
fn every_prefix_is_a_token_at_its_length_and_not_below_it() {
    for row in rows() {
        let at_length = body(row.alphabet, row.min);
        let token = format!("{}{at_length}", row.prefix);
        for (before, after) in [
            ("failed with ", " today"),
            ("(", ")"),
            ("/x/", "/y"),
            ("\"", "\""),
            ("", ""),
            ("\n", "\n"),
            ("see ", ". Retry."),
        ] {
            assert_redacted(
                &format!("{} at its length of {}", row.prefix, row.min),
                &format!("{before}{token}{after}"),
                &token,
            );
        }
        let short = format!("{}{}", row.prefix, body(row.alphabet, row.min - 1));
        assert_kept(
            &format!("{} one character short", row.prefix),
            &format!("failed with {short} today"),
        );
        let bare = row.prefix;
        assert_kept(
            &format!("{} with nothing after it", row.prefix),
            &format!("failed with {bare} today"),
        );
    }
}

#[test]
fn a_prefix_inside_a_longer_word_is_not_a_token() {
    for row in rows() {
        let token = format!("{}{}", row.prefix, body(row.alphabet, row.min));
        for before in ["x", "9", "Q"] {
            assert_kept(
                &format!("{} behind {before}", row.prefix),
                &format!("see {before}{token} here"),
            );
        }
    }
}

#[test]
fn a_token_that_goes_on_is_redacted_to_the_end_of_its_run() {
    for row in rows().iter().filter(|row| row.length == Length::AtLeast) {
        let long = format!("{}{}", row.prefix, body(row.alphabet, row.min + 25));
        assert_redacted(
            &format!("{} longer than its minimum", row.prefix),
            &format!("failed with {long} today"),
            &long,
        );
    }
}

#[test]
fn an_access_key_id_is_exactly_twenty_characters() {
    let longer = format!("{}{}", concat!("AK", "IA"), body(UPPER, 17));
    assert_kept("an access key id with a character too many", &longer);
    assert_kept(
        "an access key id with a lower-case character",
        concat!("see AK", "IA0123456789abcdef here"),
    );
}

#[test]
fn a_token_after_a_json_line_break_is_found() {
    let token = format!("{}{}", concat!("gh", "p_"), body(ALNUM, 36));
    for escape in ["\\n", "\\r", "\\t", "\\b", "\\f"] {
        let text = format!("{{\"note\":\"one{escape}{token}{escape}two\"}}");
        assert_redacted(&format!("a token behind {escape}"), &text, &token);
    }
    assert_kept(
        "a word that happens to follow a backslash",
        "C:\\ghp_notes and \\nothing here",
    );
}

#[test]
fn a_web_token_is_three_parts_and_the_first_two_are_json() {
    let head = concat!("ey", "JhbGciOiJIUzI1NiJ9");
    let claims = concat!("ey", "JzdWIiOiJzeW50aGV0aWMifQ");
    for (what, text, token) in [
        (
            "a signed token",
            format!("jwt {head}.{claims}.c2lnbmF0dXJl end"),
            format!("{head}.{claims}.c2lnbmF0dXJl"),
        ),
        (
            "an unsigned token",
            format!("jwt {head}.{claims}. end"),
            format!("{head}.{claims}."),
        ),
        (
            "a token in a URL",
            format!("GET /cb?id={head}.{claims}.c2lnbmF0dXJl&x=1"),
            format!("{head}.{claims}.c2lnbmF0dXJl"),
        ),
    ] {
        assert_redacted(what, &text, &token);
    }
    for (what, text) in [
        ("one part", format!("jwt {head} end")),
        ("two parts", format!("jwt {head}.{claims} end")),
        (
            "a second part that is not JSON",
            format!("jwt {head}.cGxhaW4.c2ln end"),
        ),
        (
            "a first part that is not JSON",
            format!("jwt cGxhaW4.{claims}.c2ln end"),
        ),
        (
            "short parts",
            String::from(concat!("jwt ey", "J.ey", "J.x end")),
        ),
    ] {
        assert_kept(what, &text);
    }
}

#[test]
fn a_private_key_block_is_redacted_to_its_end_marker() {
    let (begin, end) = (
        concat!("-----BEGIN PRIV", "ATE KEY-----"),
        concat!("-----END PRIV", "ATE KEY-----"),
    );
    let closed = format!("before\n{begin}\nbGluZTEK\nbGluZTIK\n{end}\nafter");
    assert_eq!(shown(&closed), "before\n[REDACTED]\nafter");
    // Without an end marker the header and the lines of base64 under it go, and the text after.
    let line = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj";
    let open = format!("before\n{begin}\n{line}\n{line}\nQUJDRA==\nafter: ok");
    assert_eq!(shown(&open), "before\n[REDACTED]\nafter: ok");
    for label in [
        concat!("RSA PRIV", "ATE KEY"),
        concat!("EC PRIV", "ATE KEY"),
        concat!("DSA PRIV", "ATE KEY"),
        concat!("OPENSSH PRIV", "ATE KEY"),
        concat!("ENCRYPTED PRIV", "ATE KEY"),
        concat!("PGP PRIV", "ATE KEY BLOCK"),
    ] {
        let text = format!("x -----BEGIN {label}-----\nbGluZTEK\n-----END {label}----- y");
        assert_eq!(shown(&text), "x [REDACTED] y", "for the label {label}");
    }
    // Two blocks are two redactions; the text between them stays.
    let two = format!("{begin}\nQQ==\n{end} and {begin}\nQQ==\n{end}");
    assert_eq!(shown(&two), "[REDACTED] and [REDACTED]");
    // Public material is not a secret.
    for label in ["CERTIFICATE", "PUBLIC KEY", "CERTIFICATE REQUEST"] {
        assert_kept(
            label,
            &format!("-----BEGIN {label}-----\nbGluZTEK\n-----END {label}-----"),
        );
    }
}

#[test]
fn every_token_in_the_corpus_is_redacted_and_its_words_are_kept() {
    for leak in BARE_TOKENS {
        let received = shown(leak.text);
        assert!(
            !received.contains(leak.secret),
            "expected {:?} without {:?} | received: {received:?}",
            leak.text,
            leak.secret
        );
        for kept in leak.keeps {
            assert!(
                received.contains(kept),
                "expected {:?} to keep {kept:?} | received: {received:?}",
                leak.text
            );
        }
    }
}

#[test]
fn nothing_near_a_token_in_the_benign_corpus_changes() {
    for text in BENIGN_NEAR_TOKENS.iter().chain(BENIGN) {
        assert_kept("ordinary text", text);
        let by_the_tail = stderr_text(text);
        assert_eq!(
            &by_the_tail, text,
            "expected the stderr entry point to leave ordinary text alone | received: {by_the_tail:?}"
        );
    }
}

#[test]
fn the_stderr_entry_point_reads_names_and_not_prefixes() {
    // The hot path of a stderr tail stays as it was (see `docs/benchmarks.md`): a bare token with
    // no name beside it is the message entry points' to find. This pins the split, so a change to
    // it is a decision and not a drift.
    let text = concat!(
        "token: expired (gh",
        "p_0123456789abcdefghijklmnopqrstuvwxyz)"
    );
    assert_eq!(stderr_text(text), text);
    assert_ne!(shown(text), text);
}

#[test]
fn a_second_pass_changes_nothing_in_either_corpus() {
    for text in BARE_TOKENS
        .iter()
        .map(|leak| leak.text)
        .chain(LEAKS.iter().map(|leak| leak.text))
        .chain(BENIGN_NEAR_TOKENS.iter().copied())
        .chain(BENIGN.iter().copied())
    {
        let once = shown(text);
        let twice = shown(&once);
        assert_eq!(
            once, twice,
            "expected one pass over {text:?} to be a fixed point | received: {twice:?}"
        );
    }
}

#[test]
fn a_string_inside_a_structured_value_is_read_for_tokens() {
    let token = format!("{}{}", concat!("gh", "p_"), body(ALNUM, 36));
    let data = json!({
        "detail": format!("rejected {token} by the remote"),
        "nested": [{ "note": format!("also {token}") }],
        "retry_after": 30,
    });
    let received = structured(&data);
    assert!(
        !received.to_string().contains(&token),
        "expected no token in a structured value | received: {received}"
    );
    assert_eq!(received["retry_after"], 30);
    assert_eq!(received["detail"], "rejected [REDACTED] by the remote");
}

#[test]
fn the_text_around_multibyte_characters_is_kept_whole() {
    let token = format!("{}{}", concat!("gh", "p_"), body(ALNUM, 36));
    let text = format!("não encontrado \u{e9}{token}\u{e9} e {token}\u{1f34b} fim");
    let received = shown(&text);
    assert!(
        !received.contains(&token),
        "expected no token | received: {received:?}"
    );
    assert!(
        received.starts_with("não encontrado ") && received.ends_with(" fim"),
        "expected the words kept | received: {received:?}"
    );
}

/// A name that opens like a token prefix is a name: its value is the secret, and the name has to
/// reach the assignment rules whole, as it does through `stderr_text`.
#[test]
fn a_name_that_opens_like_a_token_prefix_keeps_its_value_redacted() {
    for text in [
        concat!("github_p", "at_for_actions_runner_token = \"SEKRET1\""),
        concat!("sk", "-proj-prod-openai-api-key: SEKRET1value"),
        concat!("sk", "-live-stripe-webhook-secret=SEKRET1value"),
        concat!("sk", "-ant-api-key-for-claude: SEKRET1"),
        concat!("glp", "at-renovate-bot-access-token = SEKRET1"),
        concat!("AI", "zaSy_google_maps_browser_restricted_api_key=SEKRET1"),
    ] {
        let received = shown(text);
        assert!(
            !received.contains("SEKRET1") && received.contains("[REDACTED]"),
            "expected the value after a name that opens like a token redacted | input {text:?} received: {received:?}"
        );
    }
}

/// A token after a name that takes the rest of its line is part of that value; the marker it
/// leaves must not end the value early.
#[test]
fn a_token_after_a_name_that_takes_the_rest_of_the_line_leaves_nothing_behind() {
    for name in [
        "password:",
        "passphrase:",
        "Authorization:",
        "Proxy-Authorization:",
    ] {
        let text = format!(
            "{name} {}0123456789abcdefghijklmnopqrstuvwxyz SEKRET1 more",
            concat!("gh", "p_")
        );
        let received = shown(&text);
        assert!(
            !received.contains("SEKRET1"),
            "expected the rest of the value after a token under {name:?} redacted | input {text:?} received: {received:?}"
        );
    }
}

/// Whatever `stderr_text` takes out, the message entry points take out too: the token pass adds
/// to the rules and never gets in their way.
#[test]
fn the_message_entry_points_redact_everything_stderr_text_redacts() {
    for leak in LEAKS.iter().chain(BARE_TOKENS) {
        if stderr_text(leak.text).contains(leak.secret) {
            continue;
        }
        let received = shown(leak.text);
        assert!(
            !received.contains(leak.secret),
            "expected {:?} gone from the message form as it is from stderr_text | received: {received:?}",
            leak.secret
        );
    }
}

/// A block that begins where another ends is read as a block, so one pass leaves nothing for a
/// second: `[REDACTED]-----BEGIN ...` would otherwise be a block behind a bracket.
#[test]
fn private_key_blocks_that_adjoin_are_redacted_in_one_pass() {
    let (begin, end) = (
        concat!("-----BEGIN ", "PRIVATE KEY-----"),
        "-----END PRIVATE KEY-----",
    );
    let text = format!("{begin}\nAAAA\n{end}{begin}\nBBBB\n{end} after");
    let once = shown(&text);
    assert_eq!(
        once, "[REDACTED][REDACTED] after",
        "expected two adjoining blocks redacted in one pass | input {text:?} received: {once:?}"
    );
}

/// A sentence that mentions a header is not a key: with no end marker in reach only the marker
/// goes, and the base64 lines directly under it, so the rest of the message is kept.
#[test]
fn a_header_quoted_in_prose_takes_only_itself() {
    let begin = concat!("-----BEGIN OPENSSH PRIV", "ATE KEY-----");
    let text = format!(
        "ssh: handshake failed: expected {begin} header, got garbage; see docs. next: retry later. final line"
    );
    assert_eq!(
        shown(&text),
        "ssh: handshake failed: expected [REDACTED] header, got garbage; see docs. next: retry later. final line",
        "expected only the marker redacted | input {text:?}"
    );
}

/// A key cut before its end marker (a message bound, a truncated log) loses its header and the
/// lines of base64 under it, in real line breaks and in the escaped form a JSON string carries.
#[test]
fn a_key_without_an_end_marker_loses_its_header_and_its_base64_lines() {
    let begin = concat!("-----BEGIN PRIV", "ATE KEY-----");
    let line = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj";
    let real = format!("{begin}\n{line}\r\n{line}\n{line}\nAbCd+/==\nnext line: ok\nword");
    assert_eq!(shown(&real), "[REDACTED]\nnext line: ok\nword");
    let escaped = format!("{{\"error\":\"{begin}\\n{line}\\n{line}\\nAbCd+/==\\nnext\"}}");
    assert_eq!(shown(&escaped), "{\"error\":\"[REDACTED]\\nnext\"}");
    // A short word under the header is a word, and a marker with nothing under it is only itself.
    assert_eq!(shown(&format!("{begin}\nafter")), "[REDACTED]\nafter");
    // Base64 in the middle of a sentence is not under the header.
    let sentence = format!("{begin} and then {line} here");
    assert_eq!(shown(&sentence), format!("[REDACTED] and then {line} here"));
}

/// An end marker farther away than a key can be belongs to something else.
#[test]
fn an_end_marker_past_the_window_does_not_close_the_block() {
    let begin = concat!("-----BEGIN PRIV", "ATE KEY-----");
    let far = format!(
        "{begin} oops{}-----END PRIVATE KEY-----",
        " filler".repeat(4000)
    );
    let received = crate::redact::message_text(&far);
    assert!(
        received.starts_with("[REDACTED] oops filler") && received.contains("-----END PRIV"),
        "expected only the header redacted when the end marker is 28 KiB away | received: {:?}",
        received.get(..60)
    );
}

const KEY_LINE: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj";

/// A credential name under a key that has no end marker is still a name: the block takes the header
/// and the lines of base64, and the rules read what is left as they read it with no key above it.
#[test]
fn a_name_under_an_unclosed_key_keeps_its_value_redacted() {
    let begin = concat!("-----BEGIN RSA PRIV", "ATE KEY-----");
    for (what, after, expected_after) in [
        ("a name on its own line", "password: CNRY1\n", None),
        // `pwd=` is a short last line of base64 with its padding, and the key takes it; the floor
        // hides the value on the next line, and the two spans make one.
        (
            "a name whose value is on the next line",
            "pwd=\nCNRY2",
            Some(""),
        ),
        (
            "a name glued to the base64 before it",
            "AAAAAAAAAAAAAsecret: CNRY4",
            None,
        ),
        (
            "an end label that is not a key's",
            "-----END PASSWORD-----: CNRY5",
            None,
        ),
    ] {
        let text = format!("{begin}\n{KEY_LINE}\n{after}");
        let received = shown(&text);
        let read_alone = stderr_text(after);
        let expected_after =
            expected_after.map_or_else(|| format!("\n{read_alone}"), |after| after.to_owned());
        let expected = format!("[REDACTED]{expected_after}");
        assert!(
            !received.contains("CNRY") && received == expected,
            "expected {what} read as a name after the key | input {text:?} received: {received:?}, expected: {expected:?}"
        );
    }
}

/// The lines under a header end where a line ends. A run that stops in the middle of a line is a
/// word, or the name of a credential, and no part of the key.
#[test]
fn a_base64_run_that_stops_mid_line_is_not_a_line_of_the_key() {
    let begin = concat!("-----BEGIN PRIV", "ATE KEY-----");
    let text = format!("{begin}\n{KEY_LINE}\n{KEY_LINE} trailing words\nAbCd+/==\n");
    assert_eq!(
        shown(&text),
        format!("[REDACTED]\n{KEY_LINE} trailing words\nAbCd+/==\n"),
        "expected a line followed by words to stay | input {text:?}"
    );
    // A JSON string ends its last line with a quote, and the key is still gone.
    let json = format!("{{\"error\":\"{begin}\\n{KEY_LINE}\\n{KEY_LINE}\"}}");
    assert_eq!(shown(&json), "{\"error\":\"[REDACTED]\"}");
}

/// Only a private key's end marker closes a private key.
#[test]
fn an_end_marker_with_another_label_does_not_close_the_block() {
    let begin = concat!("-----BEGIN PRIV", "ATE KEY-----");
    let text = format!("{begin}\n{KEY_LINE}\n-----END CERTIFICATE-----\nkept: words\n");
    assert_eq!(
        shown(&text),
        "[REDACTED]\n-----END CERTIFICATE-----\nkept: words\n"
    );
}

/// A `pk_` key is the publishable half of Stripe's pair, which every page that takes a card carries.
#[test]
fn a_publishable_stripe_key_is_no_secret() {
    for mode in ["live", "test"] {
        let key = format!("{}k_{mode}_{}", "p", body(ALNUM, 24));
        assert_kept(
            &format!("a publishable {mode} key"),
            &format!("checkout page uses {key} today"),
        );
    }
}

/// A body of hyphenated words is a name, however many digits one of its words ends in; a random
/// body has a segment with both cases or two digits between any two of its hyphens.
#[test]
fn a_hyphenated_body_of_words_is_a_name_and_a_random_one_is_a_token() {
    let live = concat!("s", "k-live-");
    for name in [
        "streaming-server-1",
        "streaming-server-2-main",
        "reload-development-server-v2",
    ] {
        assert_kept(
            "a hyphenated name",
            &format!("serving {live}{name} on port 8080"),
        );
    }
    for random in [
        "a1b2c3d4-e5f6g7h8-i9j0k1l2",
        "Ab3dEf6hIj9lMn2pQr5t",
        "streamingServer1",
    ] {
        let token = format!("{live}{random}");
        assert_redacted(
            "a random body",
            &format!("serving {token} on port 8080"),
            &token,
        );
    }
}

/// An Anthropic key is `sk-ant-`, a type and two digits, a hyphen, and a long base64url body.
#[test]
fn an_anthropic_key_has_its_type_two_digits_and_a_long_body() {
    let prefix = concat!("s", "k-ant-");
    let long = body(URL_SAFE, 90);
    for (what, key, expected_redacted) in [
        ("the console's shape", format!("{prefix}api03-{long}"), true),
        ("an admin key", format!("{prefix}admin01-{long}"), true),
        ("no type", format!("{prefix}03-{long}"), false),
        ("one digit", format!("{prefix}api3-{long}"), false),
        ("three digits", format!("{prefix}api003-{long}"), false),
        (
            "no hyphen after the digits",
            format!("{prefix}api03{long}"),
            false,
        ),
        (
            "a body of 79",
            format!("{prefix}api03-{}", body(URL_SAFE, 79)),
            false,
        ),
    ] {
        let text = format!("rejected {key} today");
        if expected_redacted {
            assert_redacted(what, &text, &key);
        } else {
            assert_kept(what, &text);
        }
    }
}

/// The private-key markers, split so that no line of this file holds a whole one.
const BEGIN: &str = concat!("-----BEGIN PRIV", "ATE KEY-----");
const END: &str = concat!("-----END PRIV", "ATE KEY-----");

/// Fails, naming the row and the text, unless a stderr tail of `text` has `token` gone.
fn assert_tail_redacted(what: &str, text: &str, token: &str) {
    let received = stderr_tail(text);
    assert!(
        !received.contains(token) && received.contains("[REDACTED]"),
        "expected {what} redacted in a stderr tail | input {text:?} received: {received:?}"
    );
}

/// Fails, naming the row and the text, unless a stderr tail of `text` is `text`.
fn assert_tail_kept(what: &str, text: &str) {
    let received = stderr_tail(text);
    assert_eq!(
        received, text,
        "expected {what} left alone by a stderr tail | input {text:?} received: {received:?}"
    );
}

#[test]
fn a_stderr_tail_reads_every_prefix_at_its_length_and_not_below_it() {
    for row in rows() {
        let token = format!("{}{}", row.prefix, body(row.alphabet, row.min));
        for (before, after) in [
            ("failed with ", " today"),
            ("(", ")"),
            ("", ""),
            ("\n", "\n"),
        ] {
            assert_tail_redacted(
                &format!("{} at its length of {}", row.prefix, row.min),
                &format!("{before}{token}{after}"),
                &token,
            );
        }
        let short = format!("{}{}", row.prefix, body(row.alphabet, row.min - 1));
        assert_tail_kept(
            &format!("{} one character short", row.prefix),
            &format!("failed with {short} today"),
        );
        let inside = format!("see x{token} here");
        assert_tail_kept(&format!("{} inside a word", row.prefix), &inside);
    }
}

#[test]
fn a_stderr_tail_hides_every_token_of_the_corpus_that_is_not_a_number() {
    for leak in BARE_TOKENS {
        if leak.secret.bytes().all(|byte| byte.is_ascii_digit()) {
            // A count of twelve digits under a name that counts tokens is a newer read than the
            // tail runs: the message entry points have it, and the tail does not.
            assert_eq!(
                stderr_tail(leak.text),
                stderr_text(leak.text),
                "expected the number row left to the message entry points | input {:?}",
                leak.text
            );
            continue;
        }
        let received = stderr_tail(leak.text);
        assert!(
            !received.contains(leak.secret),
            "expected {:?} without {:?} in a stderr tail | received: {received:?}",
            leak.text,
            leak.secret
        );
        for kept in leak.keeps {
            assert!(
                received.contains(kept),
                "expected a stderr tail of {:?} to keep {kept:?} | received: {received:?}",
                leak.text
            );
        }
    }
}

#[test]
fn nothing_near_a_token_or_in_ordinary_stderr_changes_in_a_stderr_tail() {
    for text in BENIGN_NEAR_TOKENS.iter().chain(BENIGN) {
        assert_tail_kept("ordinary text", text);
    }
}

#[test]
fn a_stderr_tail_is_stderr_text_where_it_finds_no_token() {
    for text in LEAKS
        .iter()
        .map(|leak| leak.text)
        .chain(BENIGN.iter().copied())
        .chain(BENIGN_NEAR_TOKENS.iter().copied())
    {
        let (tail, text_form) = (stderr_tail(text), stderr_text(text));
        assert_eq!(
            tail, text_form,
            "expected a stderr tail to be stderr_text where no token is read | input {text:?}"
        );
    }
}

#[test]
fn a_stderr_tail_hides_what_stderr_text_hides_beside_a_token() {
    let token = format!("{}{}", concat!("gh", "p_"), body(ALNUM, 36));
    let text = format!("Authorization: Bearer top-secret {token} API_KEY=hunter2 redis://:pw@db/0");
    let received = stderr_tail(&text);
    assert_eq!(
        received,
        "Authorization: Bearer [REDACTED] [REDACTED] API_KEY=[REDACTED] redis://:[REDACTED]@db/0",
        "expected the names and the token hidden together"
    );
    assert!(stderr_text(&text).contains(&token));
}

#[test]
fn a_second_stderr_tail_pass_changes_nothing_in_either_corpus() {
    for text in BARE_TOKENS
        .iter()
        .map(|leak| leak.text)
        .chain(LEAKS.iter().map(|leak| leak.text))
        .chain(BENIGN_NEAR_TOKENS.iter().copied())
        .chain(BENIGN.iter().copied())
    {
        let once = stderr_tail(text);
        let twice = stderr_tail(&once);
        assert_eq!(
            once, twice,
            "expected one stderr tail pass over {text:?} to be a fixed point | received: {twice:?}"
        );
    }
}

/// A tail is cut on a line, so a private key can lose its begin marker and the lines above the
/// last ones it kept. What is left of it is lines of base64 and the end marker.
#[test]
fn a_key_whose_begin_line_was_cut_loses_its_lines_and_its_end_marker() {
    let cut = format!("{KEY_LINE}\n{KEY_LINE}\nQUJDRA==\n{END}\nnext: ordinary line\n");
    assert_eq!(stderr_tail(&cut), "[REDACTED]\nnext: ordinary line\n");
    // The same with the end marker the first thing the tail has.
    assert_eq!(stderr_tail(&format!("{END}\nnext\n")), "[REDACTED]\nnext\n");
    // A key with its begin marker is the block read's, and gives the same.
    let whole = format!("{BEGIN}\n{KEY_LINE}\n{END}\nnext: ordinary line\n");
    assert_eq!(stderr_tail(&whole), "[REDACTED]\nnext: ordinary line\n");
    // Windows line breaks, and a JSON string's.
    let crlf = format!("{KEY_LINE}\r\n{END}\r\nnext");
    // The stripper takes a carriage return out of every text it reads.
    assert_eq!(stderr_tail(&crlf), "[REDACTED]\nnext");
    let json = format!("{KEY_LINE}\\n{END}\\n\",\"id\":7}}");
    assert_eq!(stderr_tail(&json), "[REDACTED]\\n\",\"id\":7}");
}

/// Found by the review of the branch: a key written by an indented log, an encrypted PEM key with
/// its headers, and a PGP block with its checksum line are a key's body too.
#[test]
fn a_cut_key_is_read_through_indentation_headers_and_a_checksum_line() {
    let pgp_end = concat!("-----END PGP PRIV", "ATE KEY BLOCK-----");
    for (what, text, expected) in [
        (
            "indented lines",
            format!("  {KEY_LINE}\n  {END}\nafter"),
            "[REDACTED]\nafter",
        ),
        (
            "tab indent and a trailing space",
            format!("\t{KEY_LINE} \n\t{END}\nafter"),
            "[REDACTED]\nafter",
        ),
        (
            "a PGP checksum line",
            format!("{KEY_LINE}\n=AbCd\n{pgp_end}\nafter"),
            "[REDACTED]\nafter",
        ),
        (
            "the headers of an encrypted key",
            format!("DEK-Info: AES-128-CBC,0123456789ABCDEF\n\n{KEY_LINE}\n{END}\nafter"),
            "[REDACTED]\nafter",
        ),
        (
            "blank lines between",
            format!("{KEY_LINE}\n\n{KEY_LINE}\n{END}\nafter"),
            "[REDACTED]\nafter",
        ),
    ] {
        let received = stderr_tail(&text);
        assert_eq!(
            received, expected,
            "expected {what} read as the body of a cut key | input {text:?}"
        );
    }
}

#[test]
fn only_base64_lines_at_the_start_and_a_private_keys_end_marker_are_read_as_a_cut_key() {
    let certificate_end = "-----END CERTIFICATE-----";
    let public_end = "-----END PUBLIC KEY-----";
    let password_end = "-----END PASSWORD-----";
    for (what, text) in [
        (
            "a certificate's end marker",
            format!("{KEY_LINE}\n{certificate_end}\n"),
        ),
        (
            "a public key's end marker",
            format!("{KEY_LINE}\n{public_end}\n"),
        ),
        (
            "a credential's name as the label",
            format!("{KEY_LINE}\n{password_end}\n"),
        ),
        (
            "a sentence above the marker",
            format!("loaded the key\n{KEY_LINE}\n{END}\n"),
        ),
        (
            "a path above the marker",
            format!("/home/u/.ssh/id\n{END}\n"),
        ),
        (
            "an assignment above the marker",
            format!("{KEY_LINE}\nA=B\n{END}\n"),
        ),
        (
            "a line that is not padded base64",
            format!("{KEY_LINE}==x\n{END}\n"),
        ),
        (
            "a marker in the middle of a line",
            format!("{KEY_LINE} {END}\n"),
        ),
        (
            "a label that only ends like one",
            format!("{KEY_LINE}\n-----END PRIVATE KEYS-----\n"),
        ),
    ] {
        let received = stderr_tail(&text);
        assert!(
            received.contains(KEY_LINE)
                || received.contains("loaded")
                || received.contains("/home"),
            "expected {what} not read as a cut key | input {text:?} received: {received:?}"
        );
    }
    // A key's body lines below an ordinary line, with no begin marker, are not read either: the
    // start of the tail is the only place a cut can have left them.
    let below = format!("the build failed\n{KEY_LINE}\n{END}\n");
    assert_eq!(stderr_tail(&below).lines().next(), Some("the build failed"));
}

#[test]
fn a_cut_key_is_read_within_the_distance_a_block_is() {
    let line = format!("{KEY_LINE}\n");
    let near = format!("{}{END}", line.repeat(200));
    assert_eq!(stderr_tail(&near), "[REDACTED]");
    // Past 16 KiB of base64 the marker is too far for a block, and a cut key.
    let far = format!("{}{END}", line.repeat(300));
    assert!(
        stderr_tail(&far).contains(KEY_LINE),
        "expected an end marker past the 16 KiB a block is searched not to close a cut key"
    );
}

/// Found by the long search: `stderr_text` reads a URL password under a credential's name and then
/// the assignments once more, and the tail's token read hides the key's lines up to where the URL
/// begins, so a union built on the rules as the release before ran them left `QXZ1J` showing.
#[test]
fn a_stderr_tail_hides_what_stderr_text_hides_after_a_key_and_a_url_under_a_name() {
    let text = format!("'token': {BEGIN}{KEY_LINE}-----END\\\"x://token:;QXZ0J@host}}QXZ1J ");
    let (tail, text_form) = (stderr_tail(&text), stderr_text(&text));
    assert!(
        !text_form.contains("QXZ1J"),
        "expected stderr_text to hide the canary, the premise of this test | received {text_form:?}"
    );
    assert!(
        !tail.contains("QXZ1J") && !tail.contains("QXZ0J"),
        "expected a stderr tail to hide what stderr_text hides | received: {tail:?}, stderr_text: {text_form:?}"
    );
}
