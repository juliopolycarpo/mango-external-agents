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
        (r#"{"token_type":"Bearer"}"#, r#"{"token_type":"Bearer"}"#),
        ("token_type: Bearer", "token_type: Bearer"),
        (
            "token_type=Bearer, expires_in=60",
            "token_type=Bearer, expires_in=60",
        ),
        // A scheme with a credential after it is a credential.
        (
            r#"{"token_type":"Bearer eyJhbGciOi.payload.sig"}"#,
            r#"{"token_type":"[REDACTED]"}"#,
        ),
        (
            "token_type: Bearer eyJhbGciOi.payload.sig\nnext",
            "token_type=[REDACTED]\nnext",
        ),
    ]);
}

#[test]
fn only_the_shells_own_names_for_the_working_directory_keep_a_path() {
    assert_shown(&[
        ("PWD=/home/u/project", "PWD=/home/u/project"),
        ("OLDPWD=/home/u", "OLDPWD=/home/u"),
        ("MYSQL_PWD=/Zk3x9aQ+hunter2", "MYSQL_PWD=[REDACTED]"),
        ("export MYSQL_PWD=/x9y8z7", "export MYSQL_PWD=[REDACTED]"),
        ("DB_PWD=/s3cret/pass", "DB_PWD=[REDACTED]"),
        ("pwd: /secret", "pwd=[REDACTED]"),
        ("pwd_secret=/shh", "pwd_secret=[REDACTED]"),
        ("token_typesigtoken=basic", "token_typesigtoken=[REDACTED]"),
    ]);
}

#[test]
fn an_unknown_scheme_keeps_its_quoted_parameters_in_the_redaction() {
    assert_shown(&[
        (
            "Authorization: MyScheme user=\"alice\", sig=\"deadbeef\"",
            "Authorization: [REDACTED]",
        ),
        (
            "Authorization: Foo abc, def=\"x\"",
            "Authorization: [REDACTED]",
        ),
        ("Authorization: +Zx9/abc==", "Authorization: [REDACTED]"),
        ("Authorization: /9j/4AAQ", "Authorization: [REDACTED]"),
        ("Authorization: Bearer+abc", "Authorization: [REDACTED]"),
        (
            r#"{"Authorization": "+Zx9/abc==", "Accept": "a"}"#,
            r#"{"Authorization": "[REDACTED]", "Accept": "a"}"#,
        ),
        // The text around a value is not one.
        ("Authorization:\\nBearer x", "Authorization:\\nBearer x"),
        ("Authorization:", "Authorization:"),
    ]);
}

#[test]
fn a_password_or_a_token_array_is_redacted_whole_and_not_at_its_first_comma() {
    assert_shown(&[
        (
            r#"{"password":["hunter2","swordfish"]}"#,
            r#"{"password":[REDACTED]}"#,
        ),
        (r#"{"pwd": ["one", "two"]}"#, r#"{"pwd": [REDACTED]}"#),
        (
            r#"{"passphrase": {"a":"hunter2","b":"swordfish"}}"#,
            r#"{"passphrase": [REDACTED]}"#,
        ),
        // JSON inside a string: the escaped quotes open and close strings too.
        (
            r#"error: {\"tokens\":[\"a\"],\"detail\":\"rate limited\"} then retry"#,
            r#"error: {\"tokens\":[REDACTED],\"detail\":\"rate limited\"} then retry"#,
        ),
    ]);
}

#[test]
fn a_password_phrase_that_goes_on_after_its_status_is_a_value() {
    assert_shown(&[
        ("password: invalid \"hunter2\"", "password=[REDACTED]"),
        ("password: not set (was hunter2)", "password=[REDACTED]"),
        (
            "password: required, try again",
            "password: required, try again",
        ),
        ("password: required.", "password: required."),
        // A token is one word, as it always was.
        ("token: expired (ghp_abc)", "token: expired (ghp_abc)"),
    ]);
}

#[test]
fn text_glued_to_a_quoted_values_closing_quote_goes_with_the_value() {
    assert_shown(&[
        ("password=\"pa\"ss\"word\"", "password=\"[REDACTED]\""),
        ("token='abc'XYZ'", "token='[REDACTED]'"),
        ("PASSWORD=\"p@ss\"XYZ\"", "PASSWORD=\"[REDACTED]\""),
        ("API_KEY: \"expired\"XYZ", "API_KEY: \"[REDACTED]\""),
        ("token=\"\"XYZ", "token=\"[REDACTED]\""),
        ("token=\\\"a\\\"XYZ", "token=\\\"[REDACTED]\\\""),
        ("max_tokens=\"401\"XYZ", "max_tokens=\"[REDACTED]\""),
        ("token_type=\"Bearer\"XYZ", "token_type=\"[REDACTED]\""),
        // A closing quote followed by the end of a value is still the end of it.
        (
            r#"{"token":"abc","x":1}"#,
            r#"{"token":"[REDACTED]","x":1}"#,
        ),
        (r#"{"token":"abc"}"#, r#"{"token":"[REDACTED]"}"#),
        ("(token=\"abc\")", "(token=\"[REDACTED]\")"),
        ("token=\"abc\".", "token=\"[REDACTED]\"."),
        ("?token='a'&page=2", "?token='[REDACTED]'&page=2"),
    ]);
}

#[test]
fn only_a_real_query_string_ends_a_value_at_an_ampersand() {
    assert_shown(&[
        ("cmd &password=pa&ss", "cmd &password=[REDACTED]"),
        ("x &token: abc&def", "x &token=[REDACTED]"),
        ("run a&token=abc&page=2", "run a&token=[REDACTED]"),
        (
            "https://h/p?a=1&token=abc&page=2",
            "https://h/p?a=1&token=[REDACTED]&page=2",
        ),
        ("GET /v1?token=a&page=2", "GET /v1?token=[REDACTED]&page=2"),
    ]);
}

#[test]
fn a_header_with_an_equals_sign_or_a_vendors_own_name_is_a_credential() {
    assert_shown(&[
        ("Authorization=Bearer x1", "Authorization=Bearer [REDACTED]"),
        ("AUTHORIZATION=Bearer x1", "AUTHORIZATION=Bearer [REDACTED]"),
        ("x-functions-key: abc", "x-functions-key=[REDACTED]"),
        (
            "Ocp-Apim-Subscription-Key: abc",
            "Ocp-Apim-Subscription-Key=[REDACTED]",
        ),
        ("x-api-key: abc", "x-api-key=[REDACTED]"),
        ("x-auth-token: abc", "x-auth-token=[REDACTED]"),
        (
            "x-amz-security-token: abc",
            "x-amz-security-token=[REDACTED]",
        ),
        ("x-goog-api-key: abc", "x-goog-api-key=[REDACTED]"),
        (
            "Proxy-Authorization: Basic abc",
            "Proxy-Authorization: Basic [REDACTED]",
        ),
        // Not every name that ends in `key` is a credential.
        ("sort-key: name", "sort-key: name"),
        ("x-request-id: abc", "x-request-id: abc"),
    ]);
}

#[test]
fn words_that_start_with_a_keyword_are_words() {
    assert_shown(&[
        ("tokenizer: gpt-4o-mini", "tokenizer: gpt-4o-mini"),
        ("tokenizers=2", "tokenizers=2"),
        ("tokenization: bpe", "tokenization: bpe"),
        ("tokenize=true", "tokenize=true"),
        ("secretary: bob", "secretary: bob"),
        ("secretariat=un", "secretariat=un"),
        ("passwordless: enabled", "passwordless: enabled"),
        ("keyboard: us", "keyboard: us"),
        ("tokenizer_path=/x", "tokenizer_path=/x"),
        // A credential word later in the name is still asked.
        ("tokenizer_secret=abc", "tokenizer_secret=[REDACTED]"),
    ]);
}

#[test]
fn a_credential_word_beside_the_matched_one_vetoes_a_keep() {
    assert_shown(&[
        (
            "session_token_cookie: ;a=b",
            "session_token_cookie: [REDACTED]",
        ),
        ("password_token: expired", "password_token=[REDACTED]"),
        ("secret_token=401", "secret_token=[REDACTED]"),
        ("token_password: not set", "token_password=[REDACTED] set"),
        ("api_key_secret: missing", "api_key_secret=[REDACTED]"),
        // One keyword, repeated or in a key shape, is one credential.
        ("OPENAI_API_KEY: not set", "OPENAI_API_KEY: not set"),
        (
            "AWS_SECRET_ACCESS_KEY: not set",
            "AWS_SECRET_ACCESS_KEY: not set",
        ),
        ("token_token: expired", "token_token: expired"),
    ]);
}

/// The `?` that makes a `&` a query's join can be far behind it: a redirect URI, a state value or
/// a JWT in the query ahead of the key is hundreds of bytes, and there is no limit to how far. `key`, `auth` and `sig` are
/// credentials only as query parameters, so one lost here is a credential in the clear.
#[test]
fn a_query_credential_is_found_however_long_the_query_before_it_is() {
    for filler in [80, 700, 1500, 5000, 100_000] {
        let state = "s".repeat(filler);
        let text = format!("GET /v1?state={state}&key=AIzaSyA1&page=2");
        let expected = format!("GET /v1?state={state}&key=[REDACTED]&page=2");
        let shown = stderr_text(&text);
        assert!(
            shown == expected,
            "expected a query key after {filler} bytes of query to be redacted | received: {}",
            shown.replace(&state, "<state>")
        );
    }
}

#[test]
fn the_last_question_mark_or_blank_is_found_at_every_offset_around_a_block() {
    use super::rules::last_question_mark_or_blank;
    for len in [0, 1, 31, 32, 33, 64, 65, 100] {
        assert_eq!(last_question_mark_or_blank(&vec![b'a'; len]), None);
        for at in 0..len {
            let mut text = vec![b'a'; len];
            text[at] = b'?';
            assert_eq!(
                last_question_mark_or_blank(&text),
                Some(b'?'),
                "expected a `?` at {at} of {len} bytes to be found | received a different byte"
            );
            text[at] = b'\t';
            assert_eq!(
                last_question_mark_or_blank(&text),
                Some(b'\t'),
                "expected a tab at {at} of {len} bytes to be found | received a different byte"
            );
            // The later of the two is the answer.
            let mut both = vec![b'a'; len];
            both[at] = b'?';
            if at + 1 < len {
                both[len - 1] = b' ';
                assert_eq!(
                    last_question_mark_or_blank(&both),
                    Some(b' '),
                    "expected a blank after a `?` in {len} bytes to win | received the `?`"
                );
            }
        }
    }
}

/// The run is read forward once; each answer is what a look back over the whole run gives.
#[test]
fn a_run_of_text_answers_for_every_ampersand_as_a_look_back_would() {
    use super::rules::QueryRun;
    let text = "a&k=1 ?x=1&k=2&k=3 b&k=4 ?&k=5 c\n&k=6&?&k=7";
    let bytes = text.as_bytes();
    let mut run = QueryRun::default();
    for at in 1..bytes.len() {
        if bytes[at - 1] != b'&' {
            continue;
        }
        let run_start = text[..at]
            .rfind(|c: char| c.is_ascii_whitespace())
            .map_or(0, |blank| blank + 1);
        let expected = text[run_start..at].contains('?');
        let received = run.is_query_parameter(bytes, at, at);
        assert_eq!(
            received, expected,
            "expected the parameter at {at} of {text:?} to be in a query: {expected} | received: \
             {received}"
        );
    }
}

/// After an equals sign `authorization` is a variable (`authorization=true mode=strict`), not a
/// header line, so a scheme this does not know takes one token and not the rest of the line. A
/// header written with a colon keeps the line: `Authorization: sk-live-1 was rejected` leaves no
/// key behind.
#[test]
fn an_unknown_scheme_after_an_equals_sign_takes_one_token() {
    assert_shown(&[
        (
            "authorization=true mode=strict path=/repo",
            "authorization=[REDACTED] mode=strict path=/repo",
        ),
        (
            "Authorization=sk-live-1 was rejected",
            "Authorization=[REDACTED] was rejected",
        ),
        (
            "Authorization=Bearer x1 mode=strict",
            "Authorization=Bearer [REDACTED] mode=strict",
        ),
        (
            "Authorization=Basic dTpw mode=strict",
            "Authorization=Basic [REDACTED] mode=strict",
        ),
        (
            "Authorization=Digest user=u, response=r mode=strict",
            "Authorization=Digest [REDACTED]",
        ),
        (
            "Authorization: sk-live-1 was rejected",
            "Authorization: [REDACTED]",
        ),
        (
            r#"{"authorization": "sk-live-1 was", "path": "/repo"}"#,
            r#"{"authorization": "[REDACTED]", "path": "/repo"}"#,
        ),
    ]);
}

/// A cookie header is a line of values. A member of a JSON text whose name ends in `cookie` is a
/// member like any other: its value ends where the member does.
#[test]
fn a_quoted_cookie_member_is_a_member_and_a_cookie_header_is_a_line() {
    assert_shown(&[
        (
            r#"{"accept_cookie": true, "path": "/repo"}"#,
            r#"{"accept_cookie": [REDACTED], "path": "/repo"}"#,
        ),
        (
            r#"{"cookie": "a=1; b=2", "path": "/repo"}"#,
            r#"{"cookie": "[REDACTED]", "path": "/repo"}"#,
        ),
        (
            r#"{"set-cookie": "sid=1", "path": "/repo"}"#,
            r#"{"set-cookie": "[REDACTED]", "path": "/repo"}"#,
        ),
        ("Cookie: a=1; b=2 path=/x", "Cookie: [REDACTED]"),
        ("Set-Cookie: sid=1; Path=/", "Set-Cookie: [REDACTED]"),
        (
            "x-session-cookie: sid=1; Path=/",
            "x-session-cookie: [REDACTED]",
        ),
    ]);
}

/// A vendor's header name is matched only when the name ends at its key.
#[test]
fn a_header_name_that_only_starts_like_a_vendors_key_header_is_not_one() {
    assert_shown(&[
        ("x-functions-keyboard: abc", "x-functions-keyboard: abc"),
        ("subscription-keyword: abc", "subscription-keyword: abc"),
        ("x-functions-keys: abc", "x-functions-keys: abc"),
        ("x-functions-key: abc", "x-functions-key=[REDACTED]"),
        (
            "Ocp-Apim-Subscription-Key: abc",
            "Ocp-Apim-Subscription-Key=[REDACTED]",
        ),
    ]);
}

/// `text` as the content of a JSON string: the backslashes and the double quotes escaped.
fn escaped(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `text` escaped once for each string it is inside.
fn escaped_levels(text: &str, levels: usize) -> String {
    (0..levels).fold(text.to_owned(), |inner, _| escaped(&inner))
}

/// How many strings deep a JSON text is read: `{\\\"api_key\\\":...}` is two, five is the most.
const MOST_LEVELS: usize = 5;

/// A JSON text that was serialised into a string, and that string into another, has a quote
/// written with a backslash for every level (`\"`, `\\\"`, `\\\\\\\"`). The credential is as much
/// a credential at the fifth level as at the first, and the words around it stay readable.
#[test]
fn json_inside_json_strings_is_read_at_every_level_of_escaping() {
    for (plain, redacted, own_levels) in [
        (
            r#"{"api_key":"sk-live-x1","model":"m"}"#,
            r#"{"api_key":"[REDACTED]","model":"m"}"#,
            0,
        ),
        // Already one string deep before it is escaped further.
        (
            r#"{"data":"{\"api_key\":\"sk-live-x2\"}","id":1}"#,
            r#"{"data":"{\"api_key\":\"[REDACTED]\"}","id":1}"#,
            1,
        ),
        (
            r#"{"Authorization":"Bearer sk-live-x3","Accept":"a"}"#,
            r#"{"Authorization":"Bearer [REDACTED]","Accept":"a"}"#,
            0,
        ),
        (
            r#"{"password":"two words, here","user":"ann"}"#,
            r#"{"password":"[REDACTED]","user":"ann"}"#,
            0,
        ),
        (
            r#"{"token": 12345, "n": 1}"#,
            r#"{"token": [REDACTED], "n": 1}"#,
            0,
        ),
        (
            r#"{"tokens":["sk-18a","sk-18b"],"retry_after":30}"#,
            r#"{"tokens":[REDACTED],"retry_after":30}"#,
            0,
        ),
        (
            r#"{"headers":{"cookie":"sid=1; b=2"},"ok":true}"#,
            r#"{"headers":{"cookie":"[REDACTED]"},"ok":true}"#,
            0,
        ),
        (
            r#"{"password":"pa"ss"word","user":"ann"}"#,
            r#"{"password":"[REDACTED]","user":"ann"}"#,
            0,
        ),
    ] {
        for level in 0..=MOST_LEVELS - own_levels {
            let (text, expected) = (
                format!("upstream said {}", escaped_levels(plain, level)),
                format!("upstream said {}", escaped_levels(redacted, level)),
            );
            let received = stderr_text(&text);
            assert_eq!(
                received, expected,
                "expected the credential {level} strings deep redacted | input {text:?} received: {received:?}"
            );
        }
    }
}

/// The reading stops at the bound and does not run on: text nested past it is a value of
/// backslashes and quotes, as it was before the bound existed, and it costs a bounded read.
#[test]
fn escaping_deeper_than_the_bound_is_not_read_as_a_quote() {
    let deeper = escaped_levels(r#"{"api_key":"sk-live-x4","m":1}"#, MOST_LEVELS + 1);
    let received = stderr_text(&deeper);
    assert!(
        received.len() <= deeper.len() + "[REDACTED]".len(),
        "expected the text no larger than the bound allows | received {} bytes from {}",
        received.len(),
        deeper.len()
    );
}

/// A value in single quotes that were escaped, once or three times: the closing quote goes back
/// the way it stood.
#[test]
fn escaped_single_quotes_close_a_value_with_the_backslashes_that_opened_it() {
    assert_shown(&[
        (
            r"x=1 password=\'pa\'ss-word\' tail",
            r"x=1 password=\'[REDACTED]\' tail",
        ),
        (
            r"x=1 password=\\\'pa\\\'ss-word\\\' tail",
            r"x=1 password=\\\'[REDACTED]\\\' tail",
        ),
    ]);
}

/// A pretty-printed array or object under a credential's name is one value, however many lines it
/// takes, and a YAML sequence under one is a value of its items. The text rules used to read a
/// line at a time, so only the line with the name was redacted and the entries below it stayed.
#[test]
fn a_container_that_spans_lines_is_one_value() {
    assert_shown(&[
        (
            "{\"tokens\":\n[\n\"sk-aaa\",\n\"sk-bbb\"\n]}",
            "{\"tokens\":\n[REDACTED]}",
        ),
        (
            "{\n  \"password\": [\n    \"sk-aaa\",\n    \"sk-bbb\"\n  ],\n  \"user\": \"ann\"\n}",
            "{\n  \"password\": [REDACTED],\n  \"user\": \"ann\"\n}",
        ),
        (
            "{\n  \"credentials\": {\n    \"user\": \"u\",\n    \"pass\": \"p\"\n  },\n  \"ok\": true\n}",
            "{\n  \"credentials\": [REDACTED],\n  \"ok\": true\n}",
        ),
        (
            "{\"tokens\": [\"sk-aaa\",\n \"sk-bbb\"], \"retry_after\": 30}",
            "{\"tokens\": [REDACTED], \"retry_after\": 30}",
        ),
        // A bracket inside a string and a nested array do not close the value early.
        (
            "{\"secret\": [\n  \"a]b\",\n  [\"c\"],\n  \"d\"\n], \"n\": 1}",
            "{\"secret\": [REDACTED], \"n\": 1}",
        ),
        (
            "tokens: [\n  sk-aaa,\n  sk-bbb\n]\nnext: ok",
            "tokens=[REDACTED]\nnext: ok",
        ),
        // The carriage returns go with the rest of what the redactor strips.
        (
            "{\r\n  \"password\": [\r\n    \"sk-aaa\"\r\n  ]\r\n}",
            "{\n  \"password\": [REDACTED]\n}",
        ),
        // One that never closes ends with its first line, as it always did.
        (
            "{\"token\": [\n\"sk-aaa\",\n\"sk-bbb\"",
            "{\"token\": [REDACTED]\n\"sk-aaa\",\n\"sk-bbb\"",
        ),
    ]);
}

/// The container is read to the line that closes it, and no further than 64 lines on: a longer one
/// is read as its first line, as an array that never closes is.
#[test]
fn a_container_past_the_line_bound_is_read_as_its_first_line() {
    let body = |lines: usize| format!("{{\"token\": [\n{}\"z\"\n]}}", "\"a\",\n".repeat(lines));
    let received = stderr_text(&body(60));
    assert_eq!(
        received, "{\"token\": [REDACTED]}",
        "expected a 62 line array redacted whole | received: {received:?}"
    );
    let text = body(100);
    let received = stderr_text(&text);
    assert!(
        received.starts_with("{\"token\": [REDACTED]\n\"a\","),
        "expected a 102 line array read as its first line | received: {received:?}"
    );
}

#[test]
fn a_yaml_sequence_under_a_credential_name_is_redacted_item_by_item() {
    assert_shown(&[
        (
            "api_key:\n  - sk-live-aaa\n  - sk-live-bbb\nnext: ok",
            "api_key=[REDACTED]\nnext: ok",
        ),
        (
            "env:\n  tokens:\n    - sk-aaa\n    - sk-bbb\n  other: 1\n",
            "env:\n  tokens=[REDACTED]\n  other: 1\n",
        ),
        // A sequence at another indentation, or under another name, is not this one's.
        (
            "tokens:\n  - a\nother:\n  - b",
            "tokens=[REDACTED]\nother:\n  - b",
        ),
        ("tokens:\n  - a\n  - b\n - c", "tokens=[REDACTED]\n - c"),
        ("api_key: - sk-live-aaa\nnext", "api_key=[REDACTED]\nnext"),
        // A hyphen that starts a word is a value, as it was.
        ("api_key:\n  -sk-live-aaa\nnext", "api_key=[REDACTED]\nnext"),
        // A status stays.
        ("token:\n  - expired\nnext", "token:\n  - expired\nnext"),
    ]);
}

/// A number under a name that counts tokens is a count: `max_tokens: 100000`. A count has at most
/// ten digits (4,294,967,295 is the largest a 32-bit counter holds, and no model has a window
/// near it), so a run of twelve or more is a secret that happens to be digits, and is redacted.
#[test]
fn a_count_of_twelve_digits_or_more_is_a_credential_and_a_shorter_one_is_a_count() {
    assert_shown(&[
        ("tokens: 4294967295", "tokens: 4294967295"),
        ("tokens: 99999999999", "tokens: 99999999999"),
        (
            "max_tokens=4,294,967,295 next",
            "max_tokens=4,294,967,295 next",
        ),
        ("tokens: 100000000000", "tokens=[REDACTED]"),
        ("tokens: 123456789012", "tokens=[REDACTED]"),
        ("token_limit=999_999_999_999", "token_limit=[REDACTED]"),
        // A comma ends a value under every name, so only the digits before it are the value.
        (
            "tokens_used: 1,234,567,890,123 left",
            "tokens_used=[REDACTED],234,567,890,123 left",
        ),
        (
            r#"{"max_tokens": 123456789012, "n": 1}"#,
            r#"{"max_tokens": [REDACTED], "n": 1}"#,
        ),
        (
            r#"{"max_tokens": 4294967295, "n": 1}"#,
            r#"{"max_tokens": 4294967295, "n": 1}"#,
        ),
        (
            r#"{"total_tokens":"123456789012","n":1}"#,
            r#"{"total_tokens":"[REDACTED]","n":1}"#,
        ),
        (
            r#"{"total_tokens":"4294967295","n":1}"#,
            r#"{"total_tokens":"4294967295","n":1}"#,
        ),
        // A name that does not count tokens never kept a number, and the status words stay kept.
        ("api_token: 12345", "api_token=[REDACTED]"),
        ("secret: 503", "secret: 503"),
    ]);
}

/// A hyphen and a line break is not a sequence marker: the item must start on the same line, so a
/// dash left as a value does not take the line after it.
#[test]
fn a_hyphen_at_the_end_of_a_line_is_a_value_and_not_a_sequence_marker() {
    assert_shown(&[
        (
            "password: -\nuser: bob\nhost: example.com",
            "password=[REDACTED]\nuser: bob\nhost: example.com",
        ),
        ("tokens:\n  -\nother: x", "tokens=[REDACTED]\nother: x"),
    ]);
}
