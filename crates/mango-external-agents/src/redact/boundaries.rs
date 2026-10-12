//! A byte the redactor removes must not part a credential's name from its value.
//!
//! The stripper takes terminal control text out before the rules run, and marks the place with a
//! [`BOUNDARY`](super::strip) where the join would be ambiguous: in front of a credential name, and
//! after `Bearer` or `Basic`, so that `Authorization: Bearer<ESC>[0m sk-live-42` still reads as a
//! scheme and a token. The marker is not a name byte. A rule that stopped at it saw a name with
//! nothing in front of its separator, and the value went through in the clear. These tests hold
//! every such shape to one pass: what one call returns is what it returns again.

use super::reference;
use super::{ends_awaiting_value, stderr_text};

/// Fails, naming every case, unless one pass redacts the credential and keeps the name.
fn assert_redacted(cases: &[(&str, &str, &str)]) {
    let failures: Vec<String> = cases
        .iter()
        .filter_map(|(what, raw, expected)| {
            let received = stderr_text(raw);
            (received != *expected).then(|| {
                format!("expected {what} redacted to {expected:?} | input {raw:?} received: {received:?}")
            })
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_name_ending_in_a_scheme_word_is_redacted_across_a_removed_byte() {
    assert_redacted(&[
        (
            "a name ending in basic before an escape and an equals sign",
            "SECRET_BASIC\x1b[0m=hunter2",
            "SECRET_BASIC=[REDACTED]",
        ),
        (
            "a name ending in bearer before a carriage return and a colon",
            "TOKEN_BEARER\r: sk-live-42",
            "TOKEN_BEARER=[REDACTED]",
        ),
        (
            "a name ending in bearer before a NUL and a spaced equals sign",
            "auth_token_bearer\0 = sk-live-42",
            "auth_token_bearer=[REDACTED]",
        ),
        (
            "a name ending in basic before an escape and a colon",
            "password_basic\x1b[0m: hunter2",
            "password_basic=[REDACTED]",
        ),
    ]);
}

#[test]
fn a_name_run_is_redacted_across_a_removed_byte_before_authorization() {
    assert_redacted(&[
        (
            "a name joined to authorization by a removed byte",
            "client_secret\x1b[0mauthorization=hunter2",
            "client_secretauthorization=[REDACTED]",
        ),
        (
            "a name joined to authorization by a removed byte, spaced separator",
            "client_secret\x1b[0mauthorization : hunter2",
            "client_secretauthorization=[REDACTED]",
        ),
    ]);
}

#[test]
fn a_url_scheme_ending_in_a_scheme_word_is_redacted_across_a_removed_byte() {
    assert_redacted(&[
        (
            "a url whose scheme ends in basic before an escape",
            "basic\x1b[0m://user:hunter2@host/x",
            "basic://user:[REDACTED]@host/x",
        ),
        (
            "a url whose scheme ends in bearer before a carriage return",
            "bearer\r://user:hunter2@host/x",
            "bearer://user:[REDACTED]@host/x",
        ),
    ]);
}

/// What the marker is for: a scheme and its token are two words even when the byte between them is
/// removed, so the token is read as one and redacted.
#[test]
fn an_authorization_header_is_still_redacted_across_a_removed_byte() {
    assert_redacted(&[
        (
            "a bearer token after an escape and a space",
            "Authorization: Bearer\x1b[0m sk-live-42",
            "Authorization: Bearer [REDACTED]",
        ),
        (
            "a bearer token directly after an escape",
            "Authorization: Bearer\x1b[0msk-live-42",
            "Authorization: Bearer [REDACTED]",
        ),
        (
            "a basic token after a carriage return",
            "Authorization: Basic\rdXNlcjpwdw==",
            "Authorization: Basic [REDACTED]",
        ),
        (
            "a header whose name follows a removed byte",
            "x\x1b[0mAuthorization: Bearer sk-live-42",
            "xAuthorization: Bearer [REDACTED]",
        ),
        (
            "a name split by an escape",
            "tok\x1b[1men=sk-live-42",
            "token=[REDACTED]",
        ),
        (
            "a credential name after a letter and an escape",
            "foo\x1b[0mtoken=sk-live-42",
            "footoken=[REDACTED]",
        ),
    ]);
}

#[test]
fn a_cut_after_a_name_ending_in_a_scheme_word_is_still_awaiting_its_value() {
    for (what, dropped) in [
        (
            "a name ending in basic, an escape and an equals sign",
            "SECRET_BASIC\x1b[0m=\n",
        ),
        (
            "a name ending in bearer, a carriage return and a colon",
            "TOKEN_BEARER\r:\n",
        ),
        ("the same name with no removed byte", "SECRET_OTHER=\n"),
    ] {
        assert!(
            ends_awaiting_value(dropped),
            "expected a cut after {what} to await its value | input {dropped:?} received: false"
        );
    }
}

/// Whether a cut awaits a value does not depend on a removed byte that parts nothing: the line
/// `apikeybearer<VT>=password` is complete, as `apikeybearer=password` is.
#[test]
fn a_removed_byte_in_a_name_does_not_change_whether_a_cut_awaits_a_value() {
    for (plain, removed) in [
        ("apikeybearer=password", "apikeybearer\x0b=password"),
        ("SECRET_BASIC=", "SECRET_BASIC\x1b[0m="),
        ("TOKEN_BEARER :", "TOKEN_BEARER\r :"),
    ] {
        let (without, with) = (ends_awaiting_value(plain), ends_awaiting_value(removed));
        assert_eq!(
            with, without,
            "expected a cut after {removed:?} to await a value as one after {plain:?} does ({without}) | received: {with}"
        );
    }
}

/// A URL whose password starts with a separator byte, `;` or `,`, and whose user is a credential
/// name. The assignment rule reads `;` as the end of a value and leaves the userinfo to the
/// password rule, which redacts it as `NAME:[REDACTED]@`; that text is itself `NAME` and a `:`
/// with a value, which a second pass rewrote as an assignment. The rules now read the text the
/// password rule wrote once more, so one pass gives what two did, and 0.4.1, which stopped at
/// `NAME:[REDACTED]@`, is the only reader that differs.
#[test]
fn a_url_whose_user_is_a_credential_name_reaches_its_final_shape_in_one_pass() {
    for (raw, old, settled) in [
        (
            "i://PASSWORD:;@",
            "i://PASSWORD:[REDACTED]@",
            "i://PASSWORD=[REDACTED]",
        ),
        (
            "l://ApiKey:,@",
            "l://ApiKey:[REDACTED]@",
            "l://ApiKey=[REDACTED]",
        ),
        (
            "x://token:;hunter2@host",
            "x://token:[REDACTED]@host",
            "x://token=[REDACTED]",
        ),
    ] {
        let first = stderr_text(raw);
        let second = stderr_text(&first);
        assert_eq!(
            first, settled,
            "expected one pass over {raw:?} to give {settled:?} | received: {first:?}"
        );
        assert_eq!(
            second, first,
            "expected a second pass over {first:?} to change nothing | received: {second:?}"
        );
        assert!(
            !first.contains("hunter2"),
            "expected no password after the pass over {raw:?} | received: {first:?}"
        );
        let shipped = reference::stderr_text(raw);
        assert_eq!(
            shipped, old,
            "expected 0.4.1 to give {old:?} for {raw:?} | received: {shipped:?}"
        );
    }
}

/// The URL rule says whether it redacted a password under a user or a scheme with a keyword in it,
/// which is the only text the assignment rule reads once more. An ordinary user is not charged.
#[test]
fn the_url_rule_flags_only_a_password_under_a_credential_name() {
    use super::redact_url_passwords;
    for (raw, flagged) in [
        ("x://token:;hunter2@host", true),
        ("x://PASSWORD:,@h", true),
        ("x://my_api_key:p@h", true),
        ("x://ann:hunter2@host", false),
        ("redis://:hunter2@db/main", false),
        ("x://token@host", false),
        ("token://ann:hunter2@host", true),
        ("see https://ann:p@h and tokens=1", false),
        ("no url here token=1", false),
    ] {
        let (text, received) = redact_url_passwords(raw);
        assert_eq!(
            received, flagged,
            "expected the flag for {raw:?} to be {flagged} | received: {received} (text {text:?})"
        );
    }
}

/// The keyword check over a range asks the keyword rule alone, anywhere in the range and nowhere
/// outside it.
#[test]
fn a_keyword_is_found_inside_the_range_and_not_outside_it() {
    use super::rules::holds_a_keyword;
    let text = b"https://user:hunter2@host/?token=x";
    for (from, to, expected) in [
        (0, 13, false),
        (0, text.len(), true),
        (24, text.len(), true),
        (0, 24, false),
        (8, 8, false),
    ] {
        let received = holds_a_keyword(text, from, to);
        assert_eq!(
            received,
            expected,
            "expected a keyword in {:?} to be {expected} | received: {received}",
            String::from_utf8_lossy(&text[from..to])
        );
    }
    let user = b"x://my_api_key:p@h";
    assert!(
        holds_a_keyword(user, 0, 16),
        "expected `api_key` found inside a userinfo | received: none"
    );
}

/// A value that is itself a credential name, followed by whitespace and a separator, is the value
/// of the name before it, as it always was in text with no removed byte: the rules read across the
/// line break, take `password` as the value and stop at the space, so `hunter2` is left. 0.4.1 read
/// the same text with a removed byte in the first name as the second credential only, because it
/// did not see the first name. This pins both, and that the text without the byte is unchanged.
#[test]
fn a_value_that_is_a_credential_name_is_consumed_as_the_value_across_a_removed_byte() {
    let marked = "token_bearer\x1b[0m:\n  password: hunter2";
    let plain = "token_bearer:\n  password: hunter2";
    assert_eq!(
        stderr_text(plain),
        "token_bearer=[REDACTED] hunter2",
        "expected the unmarked text to consume the name as the value | received: {:?}",
        stderr_text(plain)
    );
    assert_eq!(
        reference::stderr_text(plain),
        stderr_text(plain),
        "expected 0.4.1 to read the unmarked text as this does | received: {:?}",
        reference::stderr_text(plain)
    );
    assert_eq!(
        stderr_text(marked),
        "token_bearer=[REDACTED] hunter2",
        "expected the marked text to read as the unmarked text does | received: {:?}",
        stderr_text(marked)
    );
    assert_eq!(
        reference::stderr_text(marked),
        "token_bearer:\n  password=[REDACTED]",
        "expected 0.4.1 to have read only the second name | received: {:?}",
        reference::stderr_text(marked)
    );
}

/// A removed byte leaves a marker in the text the rules read, and the marker is gone from what
/// they return, so a name that the marker parted from the word before it (`Passwd<ESC>Cookie`) was
/// read as two names the first time and one the second, and a second call took more out. The
/// redactor reads again while the text still changes, when it carried a marker, so a second call
/// finds nothing more. Each input here changed on a second call before that.
#[test]
fn text_that_carried_a_removed_byte_is_left_alone_by_a_second_call() {
    for raw in [
        "PasswdCookie:\u{61c},\u{1b}(B{\u{85}  \u{9d}x]\u{1}:",
        "Cookie=\"TokenPasswd\u{7f}\0\"cookieexpiredredis\u{1b}]0;'\"",
        "Bearer\u{1b}[tokensCookie://Basicnot setAccesspwdaccess",
        "BEARER\u{200e}Passwdcookie:\u{1b}]0;-@?API-KEY\u{1f34b}api",
        "\u{1b}(BZ\u{9b}:PasswdCookie=\u{1b}[ qPASSWORD  \u{2028}",
        "Signaturepasswordsecret\u{1b}[31m\\\"\\\u{1b}apiauthorizationSECRETCookie:,credential",
        "@\r401Basic\u{1}tokensCookie:\rSECRETkeynot set",
        "\u{1b}(B\u{9b}secret\u{1b}[ qcookie=:,+xDigest",
        // A name that ends in `cookie` is a cookie header from whichever of its starts is asked.
        "TokenZZZZZZxSet-Cookie: ;sid=1 ",
        "session_token_cookie: ;a=b",
    ] {
        let once = stderr_text(raw);
        let twice = stderr_text(&once);
        assert_eq!(
            once, twice,
            "expected a second call over {once:?} (from {raw:?}) to change nothing | received: \
             {twice:?}"
        );
    }
}
