//! Credentials in a JSON value, redacted while it is still structure.
//!
//! The text rules read `name=value`, `name: value` and a header. A JSON object writes the same
//! thing with a quote between the name and the colon, or as a pair of members (`{"name": ...,
//! "value": ...}`) or as a pair of array entries (`["Authorization", "Bearer ..."]`), and the
//! meaning is in the structure. A caller that holds a [`Value`] and is about to render it into a
//! message redacts it here first.
//!
//! One list of credential words serves the text rules and this walk (see [`CREDENTIAL_WORDS`]).

use serde_json::{Map, Value};

use super::message_text;

/// What replaces a credential.
const REDACTED: &str = "[REDACTED]";

/// The words that make a name a credential's, lower case, in the form a name takes with its
/// separators taken out: `refresh_token` and `refreshToken` are both `refreshtoken`.
///
/// The one list: the text rules match these words case-insensitively inside a name, and this walk
/// matches them inside a key or a field name.
pub(super) const CREDENTIAL_WORDS: [&str; 11] = [
    "secret",
    "token",
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "credential",
    "apikey",
    "privatekey",
    "authorization",
    "cookie",
];

/// Names that are a credential's only when they are the whole name: too short or too ordinary to
/// be one inside another word (`passenger`, `author`, `monkey`).
pub(super) const CREDENTIAL_NAMES: [&str; 3] = ["pass", "auth", "sig"];

/// A name with its separators and case taken out.
fn squashed(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect()
}

/// Whether a name (a JSON key, a header or a variable) names a credential, however it is cased or
/// separated: `api_key`, `apiKey`, `refresh-token`, `Authorization`, `client_secret`, `PGPASSWORD`.
pub(super) fn names_a_credential(name: &str) -> bool {
    let name = squashed(name);
    CREDENTIAL_NAMES.contains(&name.as_str())
        || CREDENTIAL_WORDS.iter().any(|word| name.contains(word))
}

/// Whether a name counts something that is named like a token: `max_tokens`, `tokens_used`,
/// `token_limit`. Its number is a count and not a credential.
///
/// The text rules' reading (`redact::rules`): the name holds the one credential word `token`, and
/// either the plural follows it (`tokens`, `tokensUsed`) or a count word does (`token_limit`,
/// `tokenCount`). A name that only contains a count word elsewhere (`account_token`,
/// `pin_token_unused`) or holds another credential word (`secret_tokens`, `used_token_secret`)
/// counts nothing.
pub(super) fn names_a_count(name: &str) -> bool {
    const COUNTS: [&str; 8] = [
        "count",
        "limit",
        "used",
        "usage",
        "budget",
        "remaining",
        "total",
        "max",
    ];
    let squashed = squashed(name);
    let credential_words: usize = CREDENTIAL_WORDS
        .iter()
        .map(|word| squashed.matches(word).count())
        .sum();
    let lower = name.to_ascii_lowercase();
    let (true, Some(at)) = (credential_words == 1, lower.find("token")) else {
        return false;
    };
    let rest = &name[at + "token".len()..];
    let plural = rest
        .strip_prefix(['s', 'S'])
        .is_some_and(|after| !after.starts_with(|c: char| c.is_ascii_lowercase()));
    let after_separator = rest
        .strip_prefix(['_', '-'])
        .unwrap_or(rest)
        .to_ascii_lowercase();
    plural
        || COUNTS
            .iter()
            .any(|count| after_separator.starts_with(count))
}

/// Whether `text` is only digits, with the separators numbers are written with.
fn only_digits(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || b",_.".contains(&byte))
}

/// `value` with every credential in it replaced by `[REDACTED]`, and the rest kept.
///
/// What it replaces:
///
/// - every string and number under a member whose name is a credential's, and an array or object
///   there becomes one `"[REDACTED]"`, so `{"tokens": ["a"]}` and `{"credentials": {"user": "u",
///   "pass": "p"}}` lose their contents and the result is never larger than the input; a number
///   under a name that counts tokens (`max_tokens`, `tokens_used`) is a count and stays, as does a
///   plain list of them. A name counts tokens by the text rules' reading: it holds `token` and no
///   other credential word, and the plural or a count word follows (`account_token` counts
///   nothing);
/// - the `value` of an object whose `name` (or `key`, `header`, `field`) is a credential's, which
///   is how an environment variable or an HTTP header is written: `{"name": "OPENAI_API_KEY",
///   "value": "sk-..."}`, in any case;
/// - the second entry of a two-entry array whose first is a credential's name:
///   `["Authorization", "Bearer ..."]`;
/// - credential-shaped text in every other string, by the rules of [`stderr_text`](super::stderr_text)
///   and the well-known tokens with no name beside them (`ghp_...`, `AKIA...`), done before the value
///   is written out because writing escapes a line break into two characters the text rules do not
///   skip.
///
/// Numbers, booleans, nulls and the shape of the value are otherwise kept, so `"retry_after": 30`
/// and a sibling `"path"` stay readable. The depth of a [`Value`] is bounded by the parser that
/// made it; this walk recurses to that depth.
///
/// # Example
///
/// ```
/// use mango_external_agents::redact;
/// use serde_json::json;
///
/// let data = json!({
///     "api_key": "sk-live-42",
///     "env": [{ "name": "OPENAI_API_KEY", "value": "sk-live-43" }],
///     "headers": [["Authorization", "Bearer abc"]],
///     "retry_after": 30,
/// });
/// let shown = redact::structured(&data);
/// assert_eq!(shown["api_key"], "[REDACTED]");
/// assert_eq!(shown["env"][0]["value"], "[REDACTED]");
/// assert_eq!(shown["headers"][0][1], "[REDACTED]");
/// assert_eq!(shown["retry_after"], 30);
/// ```
#[must_use]
pub fn structured(value: &Value) -> Value {
    walk(value, Under::Nothing)
}

/// What the walk is below.
#[derive(Clone, Copy)]
enum Under {
    /// Nothing that names a credential.
    Nothing,
    /// A member whose name is a credential's. The flag is whether the name counts something, so
    /// that a plain number below it is a count and stays.
    Credential { counts: bool },
}

impl Under {
    fn credential(name: &str) -> Option<Self> {
        names_a_credential(name).then(|| Self::Credential {
            counts: names_a_count(name),
        })
    }
}

fn walk(value: &Value, under: Under) -> Value {
    match (value, under) {
        // A container under a credential name is one marker, so that redacting never makes the
        // value larger: a megabyte of `1,` under `"token"` would otherwise come back as six.
        (Value::Object(_), Under::Credential { .. }) => Value::String(String::from(REDACTED)),
        (Value::Array(items), Under::Credential { counts: true })
            if items.iter().all(Value::is_number) =>
        {
            value.clone()
        }
        (Value::Array(_), Under::Credential { .. }) => Value::String(String::from(REDACTED)),
        _ => walk_leaf_or_free(value, under),
    }
}

fn walk_leaf_or_free(value: &Value, under: Under) -> Value {
    match value {
        Value::Object(members) => Value::Object(object(members)),
        Value::Array(items) => Value::Array(array(items, under)),
        Value::String(text) => Value::String(string(text, under)),
        Value::Number(_) => match under {
            Under::Credential { counts: false } => Value::String(String::from(REDACTED)),
            _ => value.clone(),
        },
        other => other.clone(),
    }
}

fn string(text: &str, under: Under) -> String {
    match under {
        Under::Credential { counts: true } if only_digits(text) => text.to_owned(),
        Under::Credential { .. } => String::from(REDACTED),
        Under::Nothing => message_text(text),
    }
}

/// The member of `members` whose key, in any case, is one of `names`.
fn member<'a>(members: &'a Map<String, Value>, names: &[&str]) -> Option<(&'a String, &'a Value)> {
    members
        .iter()
        .find(|(key, _)| names.iter().any(|name| key.eq_ignore_ascii_case(name)))
}

fn object(members: &Map<String, Value>) -> Map<String, Value> {
    // `{"name": "Authorization", "value": "Bearer ..."}`: the value is the credential.
    let pair_value = member(members, &["name", "key", "header", "field"])
        .and_then(|(_, name)| name.as_str())
        .and_then(Under::credential)
        .and_then(|_| member(members, &["value", "val"]))
        .map(|(key, _)| key.as_str());
    members
        .iter()
        .map(|(key, member)| {
            let below = if pair_value == Some(key.as_str()) {
                Under::Credential { counts: false }
            } else {
                Under::credential(key).unwrap_or(Under::Nothing)
            };
            (key.clone(), walk(member, below))
        })
        .collect()
}

fn array(items: &[Value], under: Under) -> Vec<Value> {
    // `["Authorization", "Bearer ..."]`: the second entry is the credential.
    let pair = matches!(items, [Value::String(name), Value::String(_)] if Under::credential(name).is_some())
        && matches!(under, Under::Nothing);
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match (pair, index) {
            (true, 1) => walk(item, Under::Credential { counts: false }),
            _ => walk(item, under),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{names_a_count, names_a_credential, structured};
    use serde_json::json;

    #[test]
    fn a_name_is_a_credentials_however_it_is_cased_or_separated() {
        for name in [
            "api_key",
            "apiKey",
            "API-KEY",
            "refresh_token",
            "accessToken",
            "clientSecret",
            "PGPASSWORD",
            "_authToken",
            "csrftoken",
            "Authorization",
            "Set-Cookie",
            "private_key",
            "privateKey",
            "password",
            "pwd",
            "pass",
            "auth",
        ] {
            assert!(
                names_a_credential(name),
                "expected {name:?} to name a credential"
            );
        }
        for name in [
            "path",
            "retry_after",
            "key",
            "author",
            "passenger",
            "monkey",
            "authority",
            "cache_key",
        ] {
            assert!(
                !names_a_credential(name),
                "expected {name:?} not to name a credential"
            );
        }
    }

    #[test]
    fn a_name_that_counts_tokens_is_not_a_secret_count() {
        for name in [
            "max_tokens",
            "tokens",
            "input_tokens",
            "tokens_used",
            "token_limit",
            "maxTokens",
            "token_count",
        ] {
            assert!(names_a_count(name), "expected {name:?} to count tokens");
        }
        for name in [
            "api_token",
            "token",
            "access_token",
            "bearer_token",
            "secret",
        ] {
            assert!(!names_a_count(name), "expected {name:?} not to count");
        }
    }

    #[test]
    fn everything_under_a_credential_name_is_replaced_at_any_depth() {
        let data = json!({
            "token": "t-1",
            "tokens": ["t-2", { "inner": "t-3", "n": 7 }],
            "credentials": { "user": "u-1", "pass": "p-1", "sk-abc": "x" },
            "api_key": 123456789,
            "expires": 30,
            "path": "/repo",
        });
        let shown = structured(&data);
        let expected = json!({
            "token": "[REDACTED]",
            "tokens": "[REDACTED]",
            "credentials": "[REDACTED]",
            "api_key": "[REDACTED]",
            "expires": 30,
            "path": "/repo",
        });
        assert_eq!(
            shown, expected,
            "expected credential names to take their whole subtree: {expected} | received: {shown}"
        );
    }

    #[test]
    fn a_count_named_like_a_token_keeps_its_number() {
        let data =
            json!({ "max_tokens": 100000, "usage": { "tokens_used": 5, "total_tokens": "812" } });
        let shown = structured(&data);
        assert_eq!(
            shown, data,
            "expected counts of tokens to stay | received: {shown}"
        );
        let secret = structured(&json!({ "api_token": 12345678, "token": "12345678" }));
        assert_eq!(
            secret,
            json!({ "api_token": "[REDACTED]", "token": "[REDACTED]" }),
            "expected a numeric secret under a singular name redacted"
        );
    }

    /// A count keeps its number only when the name counts tokens and names no other credential:
    /// the text rules' reading (`redact::rules`), not a substring of the name.
    #[test]
    fn a_number_under_a_name_that_only_contains_a_count_word_is_a_credential() {
        for name in [
            "account_token",
            "accountToken",
            "secret_tokens",
            "used_token_secret",
            "pin_token_unused",
            "token",
            "token_secret",
        ] {
            assert!(
                !names_a_count(name),
                "expected {name:?} not to be a count of tokens"
            );
            let shown = structured(&json!({ name: 123456789012_u64, "same": "x" }));
            assert_eq!(
                shown,
                json!({ name: "[REDACTED]", "same": "x" }),
                "expected the number under {name:?} redacted | received: {shown}"
            );
            let as_text = structured(&json!({ name: "123456789012" }));
            assert_eq!(
                as_text,
                json!({ name: "[REDACTED]" }),
                "expected the digit string under {name:?} redacted | received: {as_text}"
            );
        }
        let nested = structured(&json!({ "max_tokens": { "password": 123456 } }));
        assert_eq!(
            nested,
            json!({ "max_tokens": "[REDACTED]" }),
            "expected a credential below a count to be redacted | received: {nested}"
        );
    }

    /// A redacted array or object is one marker, so the output cannot be larger than the input.
    #[test]
    fn a_container_under_a_credential_name_becomes_one_marker() {
        let items = vec![json!(1); 10_000];
        let data = json!({ "token": items, "credentials": { "a": 1, "b": [1, 2] }, "ok": 1 });
        let shown = structured(&data);
        assert_eq!(
            shown,
            json!({ "token": "[REDACTED]", "credentials": "[REDACTED]", "ok": 1 }),
            "expected one marker per container | received: {}",
            shown.to_string().len()
        );
        let wide = structured(&json!({ "token": vec![1; 100_000] }));
        assert!(
            wide.to_string().len() < 64,
            "expected a 100,000 element array under a credential name to render in under 64 bytes | received: {}",
            wide.to_string().len()
        );
        // A count that is a plain list of numbers stays a list.
        let counts = structured(&json!({ "tokens": [1, 2, 3] }));
        assert_eq!(
            counts,
            json!({ "tokens": [1, 2, 3] }),
            "expected a list of counts to stay | received: {counts}"
        );
    }

    #[test]
    fn a_pair_of_members_or_of_entries_redacts_the_value_beside_a_credential_name() {
        let data = json!({
            "env": [
                { "name": "OPENAI_API_KEY", "value": "v-1" },
                { "Name": "Authorization", "Value": "Bearer v-2" },
                { "key": "PGPASSWORD", "val": "v-3" },
                { "name": "HOME", "value": "/home/a" },
            ],
            "headers": [["Authorization", "Bearer v-4"], ["Accept", "text/plain"]],
        });
        let shown = structured(&data);
        let expected = json!({
            "env": [
                { "name": "OPENAI_API_KEY", "value": "[REDACTED]" },
                { "Name": "Authorization", "Value": "[REDACTED]" },
                { "key": "PGPASSWORD", "val": "[REDACTED]" },
                { "name": "HOME", "value": "/home/a" },
            ],
            "headers": [["Authorization", "[REDACTED]"], ["Accept", "text/plain"]],
        });
        assert_eq!(
            shown, expected,
            "expected the value beside a credential name replaced: {expected} | received: {shown}"
        );
    }

    #[test]
    fn a_string_that_holds_text_is_redacted_as_text() {
        let shown = structured(&json!({ "note": "line one\nAuthorization:\nBearer nl-0123" }));
        let text = shown["note"].as_str().unwrap_or_default();
        assert!(
            !text.contains("nl-0123") && text.starts_with("line one"),
            "expected the bearer token redacted and the words kept | received: {text:?}"
        );
    }
}
