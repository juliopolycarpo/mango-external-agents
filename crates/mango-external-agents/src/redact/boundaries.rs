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

/// A second pass over a URL whose password starts with a separator byte, `;` or `,`, and whose user
/// is a credential name, rewrites the redacted userinfo as an assignment. The password rule takes
/// the whole password, but the assignment rule reads `;` as the end of a value, so the first pass
/// leaves `NAME:[REDACTED]@` and the second reads `NAME` and its `:` as a name and a separator. It
/// reveals nothing: the value was already gone. 0.4.1 does the same, and this pins that it stays a
/// change of shape.
#[test]
fn a_url_whose_user_is_a_credential_name_changes_shape_on_a_second_pass_and_reveals_nothing() {
    for (raw, once, twice) in [
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
            first, once,
            "expected one pass over {raw:?} to give {once:?} | received: {first:?}"
        );
        assert_eq!(
            second, twice,
            "expected a second pass to give {twice:?} | received: {second:?}"
        );
        assert!(
            !first.contains("hunter2") && !second.contains("hunter2"),
            "expected no password after either pass over {raw:?} | received: {first:?} then {second:?}"
        );
        let shipped = reference::stderr_text(raw);
        assert_eq!(
            shipped, once,
            "expected 0.4.1 to give {once:?} for {raw:?} as well | received: {shipped:?}"
        );
    }
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
