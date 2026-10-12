//! Text a vendor prints, for proving a harness redacts what it must and keeps what a host needs.
//!
//! Four lists. [`LEAKS`] are credential-shaped messages in the shapes vendors really print: a
//! header, an assignment, JSON, a URL, a query string, a cookie. Each says which part is the
//! credential and which words around it must survive. [`BENIGN`] are ordinary vendor errors that
//! only look like they hold a credential, such as a rate limit with token counts or a variable
//! that is not set. They come back byte for byte.
//!
//! [`BARE_TOKENS`] are credentials with no name beside them, which only a well-known prefix gives
//! away (`ghp_`, `AKIA`, a JWT, a private-key block), and [`BENIGN_NEAR_TOKENS`] are ordinary text
//! that comes close to one (`sk-learn`, `npm_config_cache`, a git SHA, a certificate). The prefixes
//! are read by [`redact::bounded`](crate::redact::bounded) and
//! [`redact::structured`](crate::redact::structured), the entry points for a vendor's messages, and
//! not by [`redact::stderr_text`](crate::redact::stderr_text), which stays on the hot path of a
//! stderr tail. A harness test that runs [`LEAKS`] through `stderr_text` therefore must not run
//! [`BARE_TOKENS`] through it; [`BENIGN_NEAR_TOKENS`] come back unchanged from every entry point.
//!
//! Every token here is synthetic and written in pieces, so that no source line holds a complete
//! credential shape.
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
        "login failed password=\"pw-25\".Zq-25x for ann",
        "Zq-25x",
        &["login failed password=\"", "for ann"],
    ),
    leak(
        "bad config password=\"pw-26\"b}c-26x\" for ann",
        "c-26x",
        &["bad config password=\"", "for ann"],
    ),
    leak(
        "run cmd &password=pw-24x&ss-24y after",
        "ss-24y",
        &["run cmd &password=", "after"],
    ),
    // A number of twelve digits or more under a name that counts tokens is not a count.
    leak(
        "quota check: tokens: 123456789012 exceeded",
        "123456789012",
        &["quota check: tokens", "exceeded"],
    ),
    leak(
        r#"usage {"total_tokens":"123456789012","retry_after":30}"#,
        "123456789012",
        &["usage", r#""retry_after":30"#],
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
    // The 13-digit millisecond timestamps the vendors' own streams carry sit under names that say
    // `At`, not under a name that counts tokens, so the twelve-digit rule does not touch them.
    r#"{"emittedAtMs":1789334985511,"startedAtMs":1789334985128,"completedAtMs":1789334989478,"input_tokens":120}"#,
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
    "x-functions-keyboard: layout-us",
    "subscription-keyword: alerts",
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
    "quota check: tokens: 4294967295 allowed, max_tokens=4,294,967,295",
];

/// Credentials with no name beside them, in the messages a vendor prints them in.
///
/// Redacted by [`redact::bounded`](crate::redact::bounded) and
/// [`redact::structured`](crate::redact::structured); see the module documentation for why
/// [`redact::stderr_text`](crate::redact::stderr_text) is not asked. The `secret` of each row is the
/// whole token, prefix included.
pub const BARE_TOKENS: &[Leak] = &[
    leak(
        concat!(
            "token: expired (gh",
            "p_0123456789abcdefghijklmnopqrstuvwxyz)"
        ),
        concat!("gh", "p_0123456789abcdefghijklmnopqrstuvwxyz"),
        &["token: expired ("],
    ),
    leak(
        concat!("api_key: 500 sk-li", "ve-0123456789abcdefghijklmn"),
        concat!("sk-li", "ve-0123456789abcdefghijklmn"),
        &["api_key"],
    ),
    leak(
        concat!(
            "push rejected for gh",
            "p_0123456789abcdefghijklmnopqrstuvwxyz by the remote"
        ),
        concat!("gh", "p_0123456789abcdefghijklmnopqrstuvwxyz"),
        &["push rejected for ", " by the remote"],
    ),
    leak(
        concat!("oauth gh", "o_0123456789abcdefghijklmnopqrstuvwxyz refused"),
        concat!("gh", "o_0123456789abcdefghijklmnopqrstuvwxyz"),
        &["oauth ", " refused"],
    ),
    leak(
        concat!("user gh", "u_0123456789abcdefghijklmnopqrstuvwxyz refused"),
        concat!("gh", "u_0123456789abcdefghijklmnopqrstuvwxyz"),
        &["user ", " refused"],
    ),
    leak(
        concat!(
            "server gh",
            "s_0123456789abcdefghijklmnopqrstuvwxyz refused"
        ),
        concat!("gh", "s_0123456789abcdefghijklmnopqrstuvwxyz"),
        &["server ", " refused"],
    ),
    leak(
        concat!(
            "refresh gh",
            "r_0123456789abcdefghijklmnopqrstuvwxyz refused"
        ),
        concat!("gh", "r_0123456789abcdefghijklmnopqrstuvwxyz"),
        &["refresh ", " refused"],
    ),
    leak(
        concat!(
            "fine-grained github_p",
            "at_0123456789abcdefghijkl_0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVW",
            " refused"
        ),
        concat!(
            "github_p",
            "at_0123456789abcdefghijkl_0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVW"
        ),
        &["fine-grained ", " refused"],
    ),
    leak(
        concat!("gitlab said glp", "at-0123456789abcdefghij is revoked"),
        concat!("glp", "at-0123456789abcdefghij"),
        &["gitlab said ", " is revoked"],
    ),
    leak(
        concat!(
            "slack: xo",
            "xb-123456789012-1234567890123-abcdefghijklmnopqrstuvwx invalid"
        ),
        concat!(
            "xo",
            "xb-123456789012-1234567890123-abcdefghijklmnopqrstuvwx"
        ),
        &["slack: ", " invalid"],
    ),
    leak(
        concat!(
            "slack: xo",
            "xp-123456789012-1234567890123-abcdefghijklmnopqrstuvwx invalid"
        ),
        concat!(
            "xo",
            "xp-123456789012-1234567890123-abcdefghijklmnopqrstuvwx"
        ),
        &["slack: ", " invalid"],
    ),
    leak(
        concat!(
            "slack: xo",
            "xa-123456789012-1234567890123-abcdefghijklmnopqrstuvwx invalid"
        ),
        concat!(
            "xo",
            "xa-123456789012-1234567890123-abcdefghijklmnopqrstuvwx"
        ),
        &["slack: ", " invalid"],
    ),
    leak(
        concat!(
            "anthropic rejected s",
            "k-ant-api03-0123456789abcdefghijklmnopqrstuvwxyz-ABCDEF_ghij today"
        ),
        concat!(
            "s",
            "k-ant-api03-0123456789abcdefghijklmnopqrstuvwxyz-ABCDEF_ghij"
        ),
        &["anthropic rejected ", " today"],
    ),
    leak(
        concat!(
            "openai rejected s",
            "k-proj-0123456789abcdefghijklmnopqrstuvwxyz-ABCDEF_ghij today"
        ),
        concat!(
            "s",
            "k-proj-0123456789abcdefghijklmnopqrstuvwxyz-ABCDEF_ghij"
        ),
        &["openai rejected ", " today"],
    ),
    leak(
        concat!(
            "openai rejected s",
            "k-0123456789abcdefghijklmnopqrstuvwxyzABCDEFGH today"
        ),
        concat!("s", "k-0123456789abcdefghijklmnopqrstuvwxyzABCDEFGH"),
        &["openai rejected ", " today"],
    ),
    leak(
        concat!("stripe: s", "k_live_0123456789abcdefghijklmn declined"),
        concat!("s", "k_live_0123456789abcdefghijklmn"),
        &["stripe: ", " declined"],
    ),
    leak(
        concat!("stripe: r", "k_live_0123456789abcdefghijklmn declined"),
        concat!("r", "k_live_0123456789abcdefghijklmn"),
        &["stripe: ", " declined"],
    ),
    leak(
        concat!("stripe: p", "k_live_0123456789abcdefghijklmn declined"),
        concat!("p", "k_live_0123456789abcdefghijklmn"),
        &["stripe: ", " declined"],
    ),
    leak(
        concat!("aws: AK", "IA0123456789ABCDEF is not authorised"),
        concat!("AK", "IA0123456789ABCDEF"),
        &["aws: ", " is not authorised"],
    ),
    leak(
        concat!("aws: AS", "IA0123456789ABCDEF is not authorised"),
        concat!("AS", "IA0123456789ABCDEF"),
        &["aws: ", " is not authorised"],
    ),
    // The documentation key is redacted like any other: its shape is the shape of a real one, and
    // a list of documentation keys to allow would be a way past the rule.
    leak(
        concat!("aws: AK", "IAIOSFODNN7EXAMPLE is not authorised"),
        concat!("AK", "IAIOSFODNN7EXAMPLE"),
        &["aws: ", " is not authorised"],
    ),
    leak(
        concat!("google: AI", "zaSyA0123456789abcdefghijklmnopqrstuv quota"),
        concat!("AI", "zaSyA0123456789abcdefghijklmnopqrstuv"),
        &["google: ", " quota"],
    ),
    leak(
        concat!("google: ya", "29.a0AfB_byC0123456789abcdefghij expired"),
        concat!("ya", "29.a0AfB_byC0123456789abcdefghij"),
        &["google: ", " expired"],
    ),
    leak(
        concat!(
            "session ey",
            "JhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzeW50aGV0aWMifQ.c3ludGhldGljc2lnbmF0dXJl",
            " rejected"
        ),
        concat!(
            "ey",
            "JhbGciOiJIUzI1NiJ9.eyJzdWIiOiJzeW50aGV0aWMifQ.c3ludGhldGljc2lnbmF0dXJl"
        ),
        &["session ", " rejected"],
    ),
    leak(
        concat!(
            "unsigned ey",
            "JhbGciOiJub25lIn0.eyJzdWIiOiJzeW50aGV0aWMifQ. rejected"
        ),
        concat!("ey", "JhbGciOiJub25lIn0.eyJzdWIiOiJzeW50aGV0aWMifQ."),
        &["unsigned ", " rejected"],
    ),
    leak(
        concat!(
            "loaded -----BEGIN PRIV",
            "ATE KEY-----\nc3ludGhldGljLWtleS1ib2R5\n-----END PRIV",
            "ATE KEY-----\n then failed"
        ),
        "c3ludGhldGljLWtleS1ib2R5",
        &["loaded ", " then failed"],
    ),
    leak(
        concat!(
            "key file:\n-----BEGIN RSA PRIV",
            "ATE KEY-----\nc3ludGhldGljLWtleS1ib2R5\nbm90LWEtcmVhbC1rZXk=\n-----END RSA PRIV",
            "ATE KEY-----"
        ),
        "bm90LWEtcmVhbC1rZXk=",
        &["key file:"],
    ),
    leak(
        concat!(
            "key file:\n-----BEGIN OPENSSH PRIV",
            "ATE KEY-----\nc3ludGhldGljLWtleS1ib2R5\nbm90LWEtcmVhbC1rZXk=\n"
        ),
        "bm90LWEtcmVhbC1rZXk=",
        &["key file:"],
    ),
    leak(
        concat!(
            "private key=-----BEGIN PRIV",
            "ATE KEY-----\nc3ludGhldGljLWtleS1ib2R5\n-----END PRIV",
            "ATE KEY-----"
        ),
        "c3ludGhldGljLWtleS1ib2R5",
        &["private key"],
    ),
    leak(
        concat!(
            "npm ERR! 401 np",
            "m_0123456789abcdefghijklmnopqrstuvwxyz for the registry"
        ),
        concat!("np", "m_0123456789abcdefghijklmnopqrstuvwxyz"),
        &["npm ERR! 401 ", " for the registry"],
    ),
    leak(
        concat!(
            "hub said h",
            "f_abcdefghijklmnopqrstuvwxyzABCDEFGH is invalid"
        ),
        concat!("h", "f_abcdefghijklmnopqrstuvwxyzABCDEFGH"),
        &["hub said ", " is invalid"],
    ),
    leak(
        concat!(
            "digitalocean do",
            "p_v1_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef is invalid"
        ),
        concat!(
            "do",
            "p_v1_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        ),
        &["digitalocean ", " is invalid"],
    ),
    leak(
        concat!(
            "shopify sh",
            "pat_0123456789abcdef0123456789abcdef is invalid"
        ),
        concat!("sh", "pat_0123456789abcdef0123456789abcdef"),
        &["shopify ", " is invalid"],
    ),
    // The same block inside a JSON string, its line breaks written as two characters.
    leak(
        concat!(
            "{\"key\":\"-----BEGIN PRIV",
            "ATE KEY-----\\nc3ludGhldGljLWtleS1ib2R5\\n-----END PRIV",
            "ATE KEY-----\\n\",\"id\":7}"
        ),
        "c3ludGhldGljLWtleS1ib2R5",
        &["{\"key\":\"", "\"id\":7}"],
    ),
    // After the two characters of a JSON line break, which a run of name characters would swallow.
    leak(
        concat!(
            "{\"note\":\"line one\\ngh",
            "p_0123456789abcdefghijklmnopqrstuvwxyz\\nline two\"}"
        ),
        concat!("gh", "p_0123456789abcdefghijklmnopqrstuvwxyz"),
        &["line one", "line two"],
    ),
];

/// Text that comes close to a well-known token and is not one: hyphenated and underscored
/// ordinary words, base64 that is not a JWT, hashes and ids, paths, model and package names, cloud
/// resource names, certificates and the prefixes with nothing after them. Every entry comes back
/// byte for byte from [`redact::stderr_text`](crate::redact::stderr_text) and from
/// [`redact::bounded`](crate::redact::bounded).
pub const BENIGN_NEAR_TOKENS: &[&str] = &[
    "pip install scikit-learn failed: sk-learn is not a package",
    "serving sk-live-reload-development-server on port 8080",
    "see sk-proj-ects-directory-listing for the layout",
    "sk-ant-colony-optimization-solver finished",
    "using github_pat_tern_matching_helpers_module_x in the build",
    "xoxb-is-not-a-real-token-prefix-here was a test name",
    "task-ant-0123456789abcdefghijklmnop was queued",
    "risk-live-0123456789abcdefghijklmnop is the dashboard",
    "disk-proj-0123456789abcdefghijklmnop mounted",
    "state-of-the-art-model-evaluation-pipeline-version-two ran",
    "pre-commit-hooks-are-not-installed-in-this-checkout",
    "npm_config_cache=/home/u/.npm npm_package_name=demo npm_lifecycle_event=build",
    "HF_HOME=/home/u/.cache/huggingface hf_hub_download retried",
    "the prefixes ghp_ gho_ ghu_ ghs_ glpat- xoxb- sk-ant- AKIA ya29. are documented",
    "github_pat_ alone, and github_actions_runner_version_2.300",
    "ghs_cache_directory_for_the_runner_is_full",
    "base64 header eyJhbGciOiJIUzI1NiJ9 without a body",
    "decoded dGhpcyBpcyBub3QgYSB0b2tlbiwganVzdCB0ZXh0IQ== fine",
    "two parts eyJhbGciOiJIUzI1NiJ9.notapayload is not a jwt",
    "commit 9fceb02d0ae598e95dc970b74767f19372d61af8 builds",
    "request 550e8400-e29b-41d4-a716-446655440000 timed out",
    "file not found: /usr/lib/x86_64-linux-gnu/libssl.so.3",
    "model gpt-4o-mini and claude-sonnet-5-20260101 and gpt-4.1-nano are available",
    "package @types/node@22.1.0 and tokio-util-0.7.12 and sk-learn-1.5 resolved",
    "arn:aws:iam::123456789012:role/deploy in us-east-1 and eu-west-2",
    "AWS_REGION=eu-west-2 AWS_DEFAULT_OUTPUT=json",
    "ASIAN_PACIFIC_REGION is not a key and neither is AKIAnotakey",
    "ASIAPACIFICREGION1 and AKIA0123456789ABCDEFG and XAKIA0123456789ABCDEF",
    "AIzaSyShort is too short and ya29.x too",
    "short hf_abc and npm_abc and shpat_abc and dop_v1_abc",
    "-----BEGIN CERTIFICATE-----\nMIIBsynthetic\n-----END CERTIFICATE-----",
    "-----BEGIN PUBLIC KEY-----\nMFkwsynthetic\n-----END PUBLIC KEY-----",
    "separator ----- and ----------- and ----- BEGIN -----",
    "sk- alone and sk-ant- and sk-live-short",
    "the build tool task-runner-sk-ant-config reads it",
];
