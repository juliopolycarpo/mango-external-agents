//! Text a vendor prints, for proving a harness redacts what it must and keeps what a host needs.
//!
//! Two lists. [`LEAKS`] are credential-shaped messages in the shapes vendors really print: a
//! header, an assignment, JSON, a URL, a query string, a cookie. Each says which part is the
//! credential and which words around it must survive. [`BENIGN`] are ordinary vendor errors that
//! only look like they hold a credential, such as a rate limit with token counts or a variable
//! that is not set. They come back byte for byte.
//!
//! A harness test scripts one of these as an agent's or a CLI's error and asserts that what the
//! host receives holds none of [`Leak::secret`], keeps every [`Leak::keeps`], and leaves a
//! [`BENIGN`] message as it was.
//!
//! # Example
//!
//! ```
//! use mango_external_agents::redact::stderr_text;
//! use mango_external_agents::testing::redaction::LEAKS;
//!
//! for leak in LEAKS {
//!     let shown = stderr_text(leak.text);
//!     assert!(!shown.contains(leak.secret), "{shown}");
//!     assert!(leak.keeps.iter().all(|kept| shown.contains(kept)), "{shown}");
//! }
//! ```

/// One credential-shaped message.
#[derive(Clone, Copy, Debug)]
pub struct Leak {
    /// The message as a vendor wrote it.
    pub text: &'static str,
    /// The credential in it, which must not survive.
    pub secret: &'static str,
    /// Words around the credential that a host needs, which must survive.
    pub keeps: &'static [&'static str],
}

const fn leak(text: &'static str, secret: &'static str, keeps: &'static [&'static str]) -> Leak {
    Leak {
        text,
        secret,
        keeps,
    }
}

/// Messages that carry a credential in each shape the redactor reads.
pub const LEAKS: &[Leak] = &[
    leak(
        "request refused: Authorization: Bearer top-secret-token unreachable",
        "top-secret-token",
        &["request refused: ", "Authorization: Bearer", "unreachable"],
    ),
    leak(
        "request refused: API_KEY=another-secret unreachable",
        "another-secret",
        &["request refused: ", "API_KEY=", "unreachable"],
    ),
    leak(
        "request refused: redis://app:password@db/main unreachable",
        "password",
        &[
            "request refused: ",
            "redis://app:",
            "@db/main",
            "unreachable",
        ],
    ),
    leak(
        r#"upstream said {"Authorization":"Bearer sk-quoted-1","Accept":"text/plain"}"#,
        "sk-quoted-1",
        &["upstream said", r#""Accept":"text/plain""#],
    ),
    leak(
        "headers were {'Authorization': 'Bearer sk-quoted-2', 'Accept': 'text/plain'}",
        "sk-quoted-2",
        &["headers were", "'Accept': 'text/plain'"],
    ),
    leak(
        r#"bad request {"api_key":"sk-json-3","model":"claude-sonnet-5"}"#,
        "sk-json-3",
        &["bad request", r#""model":"claude-sonnet-5""#],
    ),
    leak(
        r#"bad request {\"api_key\":\"sk-escaped-4\",\"model\":\"m\"}"#,
        "sk-escaped-4",
        &["bad request", r#"\"model\":\"m\""#],
    ),
    leak(
        "denied: Authorization: token ghp_0123456789abcdef for the repository",
        "ghp_0123456789abcdef",
        &["denied: ", "Authorization: token"],
    ),
    leak(
        "GET /v1/models?key=AIzaSyA0123456789abcd&page=2 returned 403",
        "AIzaSyA0123456789abcd",
        &["GET /v1/models?key=", "&page=2 returned 403"],
    ),
    leak(
        "invalid key. API key: sk-spaced-5 was rejected upstream",
        "sk-spaced-5",
        &["invalid key. ", "API key", "rejected upstream"],
    ),
    leak(
        "replayed\nCookie: sid=ck-6; theme=dark\ndone",
        "ck-6",
        &["replayed", "Cookie:", "done"],
    ),
    leak(
        "replayed\nSet-Cookie: session=sc-7; Path=/; HttpOnly\ndone",
        "sc-7",
        &["replayed", "Set-Cookie:", "done"],
    ),
    leak(
        "clone failed for https://ghp_0123456789abcdefghijkl@github.com/o/r.git",
        "ghp_0123456789abcdefghijkl",
        &["clone failed for https://", "@github.com/o/r.git"],
    ),
    leak(
        "login failed, password: two words here",
        "two words here",
        &["login failed, ", "password"],
    ),
    leak(
        r#"login failed {"password":"two words here","user":"ann"}"#,
        "two words here",
        &["login failed", r#""user":"ann""#],
    ),
    leak(
        "env accessToken=at-8 clientSecret: cs-9 PGPASSWORD=pg-10 _authToken=an-11 csrftoken=cs-12 ok",
        "at-8",
        &["env accessToken=", "ok"],
    ),
    leak(
        "env clientSecret: cs-9 ok",
        "cs-9",
        &["env clientSecret", "ok"],
    ),
    leak(
        "env PGPASSWORD=pg-10 ok",
        "pg-10",
        &["env PGPASSWORD=", "ok"],
    ),
    leak(
        "env _authToken=an-11 ok",
        "an-11",
        &["env _authToken=", "ok"],
    ),
    leak("env csrftoken=cs-12 ok", "cs-12", &["env csrftoken=", "ok"]),
    leak("env pwd=pw-13 ok", "pw-13", &["env pwd=", "ok"]),
    leak(
        "env private key: pk-14 and the rest",
        "pk-14",
        &["env private key"],
    ),
    leak(
        r#"signed {"Authorization":"AWS4-HMAC-SHA256 Credential=AKIAX/1, Signature=beef16","Host":"h"}"#,
        "beef16",
        &["signed", r#""Host":"h""#],
    ),
    leak(
        "upstream said Authorization: sk-live-17 was rejected\nHost: example.com",
        "sk-live-17",
        &["upstream said Authorization: ", "Host: example.com"],
    ),
    leak(
        r#"call failed {"tokens":["sk-18a","sk-18b"],"retry_after":30}"#,
        "sk-18b",
        &["call failed", r#""retry_after":30"#],
    ),
    leak(
        "presigned GET /b/k?X-Amz-Signature=sig19abc&X-Amz-Expires=60 failed",
        "sig19abc",
        &["presigned GET /b/k?", "X-Amz-Expires=60 failed"],
    ),
    leak(
        "denied Authorization=Bearer sk-20abc on retry",
        "sk-20abc",
        &["denied Authorization=Bearer", "on retry"],
    ),
    leak(
        "request headers x-functions-key: fk-21abc\nHost: h",
        "fk-21abc",
        &["request headers x-functions-key", "Host: h"],
    ),
    leak(
        "Ocp-Apim-Subscription-Key: ak-22abc and Host",
        "ak-22abc",
        &["Ocp-Apim-Subscription-Key", "and Host"],
    ),
    leak(
        r#"login failed password="pa"ss-23word" for ann"#,
        "ss-23word",
        &["login failed password=\"", "for ann"],
    ),
    leak(
        "run cmd &password=pw-24x&ss-24y after",
        "ss-24y",
        &["run cmd &password=", "after"],
    ),
    leak(
        r#"call failed {"error":"bad","env":[{"name":"HOME","value":"/h"}],"token":"tk-15"}"#,
        "tk-15",
        &["call failed", r#""error":"bad""#, r#""value":"/h""#],
    ),
];

/// Ordinary vendor errors that look as if they might hold a credential and do not. A redactor
/// returns each byte for byte.
pub const BENIGN: &[&str] = &[
    "rate limit reached: 429 Too Many Requests, retry after 30s",
    "Rate limit: 90000 tokens per minute exceeded; used 91234 tokens",
    "max_tokens: 100000 > 64000, which is the maximum allowed for this model",
    "prompt is too long: 215000 tokens > 200000 maximum",
    "input_tokens=1200 output_tokens=87 total_tokens: 1287",
    "failed to refresh token: 401 Unauthorized",
    "token expired, sign in again",
    "token: expired",
    "OPENAI_API_KEY: not set",
    "OPENAI_API_KEY is missing",
    "ANTHROPIC_API_KEY: missing",
    "invalid api key format",
    "API key: invalid",
    "password: required",
    "credentials: invalid",
    "authorization failed",
    "Authorization header is required",
    "GET https://api.example.com/v1/models?page=2&limit=10 returned 404",
    "model claude-sonnet-5-20260101 was not found",
    "file not found: /home/user/project/src/main.rs:42:9",
    "request id req_011CS3xK9 failed with status 529",
    "clone failed for ssh://git@github.com/org/repo.git",
    "contact user@example.com about the quota",
    r#"{"level":"info","msg":"done","status":200,"dur_ms":87}"#,
    r#"{"error":{"type":"overloaded_error","message":"Overloaded"},"request_id":"req_1"}"#,
    "Authorization: 401 Unauthorized",
    "token: 429 Too Many Requests",
    "env OLDPWD=/home/u PWD=/home/u/project",
    "clone failed for ssh://jenkins-deploy-agent-01@build.example.com/repo.git",
    r#"{"access_token":"[REDACTED]","token_type":"Bearer","expires_in":3600}"#,
    "tokenizer: gpt-4o-mini",
    "secretary: bob",
    "tokenization: bpe",
    "passwordless: enabled",
    "the secret service is unavailable",
    "key not found in cache",
    "cache_key: layer-3",
    "Unexpected token '<' in JSON at position 0",
    "connection refused (os error 111) while connecting to 127.0.0.1:8080",
    "turn failed: context_length_exceeded (-32000)",
];
