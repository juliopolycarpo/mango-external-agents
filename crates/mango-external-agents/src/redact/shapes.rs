//! What the rules read beyond the plain `name=value` and `Authorization: Bearer` shapes, pinned
//! one shape at a time.
//!
//! Each case is a vendor-written message and what a host receives. A shape that is a credential is
//! redacted, and the words around it stay; a shape that only looks like one comes back as it was.
//! Where the text is ambiguous the case is on the redacted side, and says so.
//! `redact::differential` holds every input without one of these shapes to the 0.4.1 answer.

use super::stderr_text;

/// Fails, naming `text`, unless redacting it gives `expected`.
fn assert_shown(cases: &[(&str, &str)]) {
    for (text, expected) in cases {
        let received = stderr_text(text);
        assert_eq!(
            &received, expected,
            "expected {text:?} to be shown as {expected:?} | received: {received:?}"
        );
    }
}

#[test]
fn a_quoted_name_and_value_are_read_like_a_bare_pair() {
    assert_shown(&[
        (
            r#"{"Authorization":"Bearer x1","Accept":"a"}"#,
            r#"{"Authorization":"Bearer [REDACTED]","Accept":"a"}"#,
        ),
        (
            "{'Authorization': 'Bearer x1', 'Accept': 'a'}",
            "{'Authorization': 'Bearer [REDACTED]', 'Accept': 'a'}",
        ),
        (
            r#"{"api_key":"sk-x","model":"m"}"#,
            r#"{"api_key":"[REDACTED]","model":"m"}"#,
        ),
        (
            r#"{\"api_key\":\"sk-x\",\"model\":\"m\"}"#,
            r#"{\"api_key\":\"[REDACTED]\",\"model\":\"m\"}"#,
        ),
        // The value runs to its closing quote, spaces and punctuation included.
        (
            r#"{"password":"a b; c, d","user":"ann"}"#,
            r#"{"password":"[REDACTED]","user":"ann"}"#,
        ),
        // A value that was never closed ends with its line.
        ("token='unterminated\nnext", "token='[REDACTED]\nnext"),
        // Multi-byte text inside the quotes.
        ("token=\"café ü\" tail", "token=\"[REDACTED]\" tail"),
        // An empty value is no value.
        (r#"{"token":""}"#, r#"{"token":""}"#),
        (r#"{"token": null}"#, r#"{"token": null}"#),
        // A number under a credential name is a credential.
        (r#"{"token": 12345}"#, r#"{"token": [REDACTED]}"#),
    ]);
}

#[test]
fn a_keyword_that_ends_a_name_or_follows_a_capital_names_a_credential() {
    assert_shown(&[
        ("accessToken=at1", "accessToken=[REDACTED]"),
        ("clientSecret: cs1", "clientSecret=[REDACTED]"),
        ("PGPASSWORD=pg1", "PGPASSWORD=[REDACTED]"),
        ("_authToken=an1", "_authToken=[REDACTED]"),
        ("csrftoken=c1", "csrftoken=[REDACTED]"),
        ("CSRFToken: c1", "CSRFToken=[REDACTED]"),
        ("XSRF-TOKEN=c1", "XSRF-TOKEN=[REDACTED]"),
        ("aws_session_token=s1", "aws_session_token=[REDACTED]"),
        ("pwd=p1", "pwd=[REDACTED]"),
        ("x-api-key: k1", "x-api-key=[REDACTED]"),
        ("secretKey: k1", "secretKey=[REDACTED]"),
        ("SECRET KEY: k1", "SECRET KEY=[REDACTED]"),
        ("API key: sk-1", "API key=[REDACTED]"),
        ("private key: pk-1", "private key=[REDACTED]"),
        // Words that only contain a keyword stay words.
        ("passwordless login: enabled", "passwordless login: enabled"),
        ("keyboard: us", "keyboard: us"),
        ("author: me", "author: me"),
        ("a pi key: abc", "a pi key: abc"),
    ]);
}

#[test]
fn any_authorization_scheme_is_a_credential() {
    assert_shown(&[
        (
            "denied: Authorization: token ghp_0123 for the repository",
            "denied: Authorization: token [REDACTED] for the repository",
        ),
        (
            "Authorization: Digest username=\"a\", response=\"b\" tail\nnext",
            "Authorization: Digest [REDACTED]\nnext",
        ),
        (
            "proxy-authorization: Basic dXNlcjpwYXNz",
            "proxy-authorization: Basic [REDACTED]",
        ),
        // A scheme with nothing after it carries nothing.
        ("authorization: Bearer", "authorization: Bearer"),
        // A status where a header would be.
        (
            "Authorization: 401 Unauthorized",
            "Authorization: 401 Unauthorized",
        ),
        ("Authorization: required", "Authorization: required"),
    ]);
}

#[test]
fn a_query_parameter_is_redacted_alone() {
    assert_shown(&[
        (
            "GET /v1/models?key=AIza0123&page=2 returned 403",
            "GET /v1/models?key=[REDACTED]&page=2 returned 403",
        ),
        (
            "https://h/p?page=2&token=abc#frag",
            "https://h/p?page=2&token=[REDACTED]#frag",
        ),
        (
            "https://h/p?api_key=abc&b=1",
            "https://h/p?api_key=[REDACTED]&b=1",
        ),
        ("/v1?sig=abc&x=1", "/v1?sig=[REDACTED]&x=1"),
        ("/v1?auth=abc&x=1", "/v1?auth=[REDACTED]&x=1"),
        // Outside a query string `key` is a word, and `&` is part of a value.
        ("key=abc", "key=abc"),
        ("see key: layer", "see key: layer"),
        ("token=a&b", "token=[REDACTED]"),
        // Nothing here is a credential.
        ("GET /v1?page=2&limit=10", "GET /v1?page=2&limit=10"),
    ]);
}

#[test]
fn a_cookie_header_is_redacted_to_the_end_of_its_line() {
    assert_shown(&[
        (
            "replayed\nCookie: sid=1; theme=dark\ndone",
            "replayed\nCookie: [REDACTED]\ndone",
        ),
        (
            "Set-Cookie: session=1; Path=/; HttpOnly",
            "Set-Cookie: [REDACTED]",
        ),
        // A count named for cookies is not a cookie.
        ("cookies: 3 stored", "cookies=[REDACTED] stored"),
        ("cookie", "cookie"),
    ]);
}

#[test]
fn a_userinfo_that_is_itself_a_token_is_redacted() {
    assert_shown(&[
        (
            "https://ghp_0123456789abcdefghij@github.com/o/r.git",
            "https://[REDACTED]@github.com/o/r.git",
        ),
        // A user name, an email and a git user are names.
        ("https://user@host/path", "https://user@host/path"),
        (
            "clone ssh://git@github.com/org/repo.git",
            "clone ssh://git@github.com/org/repo.git",
        ),
        ("git@github.com:org/repo.git", "git@github.com:org/repo.git"),
        // Twenty letters with no digit is a name too.
        (
            "https://abcdefghijklmnopqrstuvwx@host",
            "https://abcdefghijklmnopqrstuvwx@host",
        ),
    ]);
}

#[test]
fn a_password_after_a_colon_runs_to_the_end_of_its_phrase() {
    assert_shown(&[
        (
            "login failed, password: two words here",
            "login failed, password=[REDACTED]",
        ),
        ("password: two words, next", "password=[REDACTED], next"),
        ("passphrase: a b; c", "passphrase=[REDACTED]; c"),
        // With `=` a value has always been one word.
        ("password=hunter2 next", "password=[REDACTED] next"),
        ("token: ghp_abc next", "token=[REDACTED] next"),
    ]);
}

#[test]
fn a_status_after_a_credential_name_is_kept() {
    assert_shown(&[
        ("OPENAI_API_KEY: not set", "OPENAI_API_KEY: not set"),
        ("ANTHROPIC_API_KEY: missing", "ANTHROPIC_API_KEY: missing"),
        ("token: expired", "token: expired"),
        ("password: required.", "password: required."),
        (
            "failed to refresh token: 401 Unauthorized",
            "failed to refresh token: 401 Unauthorized",
        ),
        ("credentials: invalid", "credentials: invalid"),
        ("api_key: Invalid", "api_key: Invalid"),
        // Anything more than a status is a value.
        ("token: not a real one", "token=[REDACTED] a real one"),
        ("token: expired-abc123", "token=[REDACTED]"),
        ("password: invalid hunter2", "password=[REDACTED]"),
        ("api_key: 401abc", "api_key=[REDACTED]"),
        ("token: 4010", "token=[REDACTED]"),
    ]);
}

#[test]
fn a_count_of_tokens_is_kept_and_a_number_that_is_a_token_is_not() {
    assert_shown(&[
        (
            "max_tokens: 100000 > 64000, which is the maximum",
            "max_tokens: 100000 > 64000, which is the maximum",
        ),
        (
            "input_tokens=1200 output_tokens=87 total_tokens: 1287",
            "input_tokens=1200 output_tokens=87 total_tokens: 1287",
        ),
        ("token_count: 100", "token_count: 100"),
        ("tokens_used=5", "tokens_used=5"),
        // The same names with a value that is not a number.
        ("max_tokens: abc123", "max_tokens=[REDACTED]"),
        ("tokens: 12 34", "tokens: 12 34"),
        // A singular name with a number is a credential.
        ("api_token: 12345678", "api_token=[REDACTED]"),
        ("access_token: 12345678", "access_token=[REDACTED]"),
        ("token: 12345678", "token=[REDACTED]"),
    ]);
}

#[test]
fn a_removed_byte_inside_a_quoted_shape_does_not_hide_it() {
    assert_shown(&[
        (
            "{\"api_\u{1b}[0mkey\":\"sk-x\"}",
            "{\"api_key\":\"[REDACTED]\"}",
        ),
        (
            "{\"Authorization\": \u{1b}[1m\"Bearer sk-x\"}",
            "{\"Authorization\": \"Bearer [REDACTED]\"}",
        ),
    ]);
}

#[test]
fn a_quoted_authorization_value_ends_at_its_closing_quote() {
    assert_shown(&[
        (
            r#"{"Authorization":"AWS4-HMAC-SHA256 Credential=AKIAX/1, SignedHeaders=host, Signature=beef01"}"#,
            r#"{"Authorization":"AWS4-HMAC-SHA256 [REDACTED]"}"#,
        ),
        (
            r#"{"Authorization":"Digest username=\"a\", response=\"beef01\""}"#,
            r#"{"Authorization":"Digest [REDACTED]"}"#,
        ),
        (
            r#"{"Authorization":"Bearer abc def","Accept":"a"}"#,
            r#"{"Authorization":"Bearer [REDACTED]","Accept":"a"}"#,
        ),
        // The escaping backslash of a JSON text inside a string stays where it was.
        (
            r#"{\"Authorization\":\"Bearer abc\",\"Accept\":\"a\"}"#,
            r#"{\"Authorization\":\"Bearer [REDACTED]\",\"Accept\":\"a\"}"#,
        ),
        // A status in quotes is a status.
        (
            r#"{"Authorization":"401 Unauthorized"}"#,
            r#"{"Authorization":"401 Unauthorized"}"#,
        ),
    ]);
}

#[test]
fn an_unknown_scheme_redacts_its_line_and_not_the_next_one() {
    assert_shown(&[
        (
            "Authorization: sk-live-1 was rejected by the server",
            "Authorization: [REDACTED]",
        ),
        (
            "Authorization: ghp_abc123\nContent-Type: application/json\nHost: x",
            "Authorization: [REDACTED]\nContent-Type: application/json\nHost: x",
        ),
        (
            "ProxyAuthorization: Bearer sk-live-1",
            "ProxyAuthorization: Bearer [REDACTED]",
        ),
        (
            "Authorization: Digest username=\"a\", response=\"b\"\nHost: x",
            "Authorization: Digest [REDACTED]\nHost: x",
        ),
    ]);
}

#[test]
fn a_status_that_ends_in_a_mark_with_text_glued_to_it_is_a_value() {
    assert_shown(&[
        ("secret: set.hunter2", "secret=[REDACTED]"),
        ("token: none.abc123", "token=[REDACTED]"),
        ("password: null)Passw0rd", "password=[REDACTED]"),
        ("api_key: error.sk-abc123", "api_key=[REDACTED]"),
        ("Cookie: set.sid=abc; x=y", "Cookie: [REDACTED]"),
        ("token: expired.password=abc", "token=[REDACTED]"),
        // The same words closed by a mark and then a gap, or the end, are a status.
        (
            "token: expired. Sign in again",
            "token: expired. Sign in again",
        ),
        ("secret: missing)", "secret: missing)"),
        // Reason words are a status only after the number.
        ("PASSWORD=server", "PASSWORD=[REDACTED]"),
        (
            "secret: internal server error",
            "secret=[REDACTED] server error",
        ),
        (
            "token: 500 Internal Server Error",
            "token: 500 Internal Server Error",
        ),
        (
            "token: 429 Too Many Requests",
            "token: 429 Too Many Requests",
        ),
        ("token: 401", "token: 401"),
    ]);
}

#[test]
fn a_count_is_kept_only_when_the_whole_name_is_a_count() {
    assert_shown(&[
        ("verification_tokens=123456", "verification_tokens=123456"),
        ("auth_tokens_secret=12345", "auth_tokens_secret=[REDACTED]"),
        (
            "token_limit_password=424242",
            "token_limit_password=[REDACTED]",
        ),
        ("secret_tokens=123", "secret_tokens=[REDACTED]"),
    ]);
}

#[test]
fn a_json_array_or_object_under_a_credential_is_redacted_whole() {
    assert_shown(&[
        (r#"{"tokens":["sk-a","sk-b"]}"#, r#"{"tokens":[REDACTED]}"#),
        (
            r#"{"credentials":{"user":"u","pass":"p-secret"},"x":1}"#,
            r#"{"credentials":[REDACTED],"x":1}"#,
        ),
        (r#"{"tokens": [1,2,3]}"#, r#"{"tokens": [REDACTED]}"#),
        (
            r#"{"secret":["a]b","c"],"k":2}"#,
            r#"{"secret":[REDACTED],"k":2}"#,
        ),
        // What was redacted stays redacted, and stays one `[REDACTED]`.
        (r#"{"token": [REDACTED]}"#, r#"{"token": [REDACTED]}"#),
    ]);
    let once = stderr_text(r#"{"token": 12345}"#);
    assert_eq!(
        stderr_text(&once),
        once,
        "expected a second pass over {once:?} to change nothing"
    );
}

#[test]
fn a_cookie_line_and_a_glued_key_name_are_found_by_their_locators() {
    assert_shown(&[
        (
            "Cookie: ;sid=hunter2-secret; theme=dark",
            "Cookie: [REDACTED]",
        ),
        ("Cookie: , sid=hunter2-secret", "Cookie: [REDACTED]"),
        ("xAPI key: sk-live-hunter2", "xAPI key=[REDACTED]"),
        (
            r#"{"myApi key": "sk-live-hunter2"}"#,
            r#"{"myApi key": "[REDACTED]"}"#,
        ),
        ("api  key: sk-1", "api  key=[REDACTED]"),
        ("api.key=sk-1", "api.key=[REDACTED]"),
        ("openai.api.key=sk-1", "openai.api.key=[REDACTED]"),
        ("access_key=AKIA1", "access_key=[REDACTED]"),
        (
            "GET /v1?X-Amz-Signature=abc&x=1",
            "GET /v1?X-Amz-Signature=[REDACTED]&x=1",
        ),
    ]);
}

#[test]
fn ordinary_text_that_looks_like_a_name_is_kept() {
    assert_shown(&[
        // The shell's working directory.
        (
            "OLDPWD=/home/u PWD=/home/u/proj",
            "OLDPWD=/home/u PWD=/home/u/proj",
        ),
        ("pwd=hunter2", "pwd=[REDACTED]"),
        // A user name that is long and has a digit.
        (
            "ssh://jenkins-deploy-agent-01@build.example.com/x",
            "ssh://jenkins-deploy-agent-01@build.example.com/x",
        ),
        (
            r#"{"token_type":"Bearer","expires_in":3600}"#,
            r#"{"token_type":"Bearer","expires_in":3600}"#,
        ),
        (r#"{"token_type":"abc"}"#, r#"{"token_type":"[REDACTED]"}"#),
    ]);
}
