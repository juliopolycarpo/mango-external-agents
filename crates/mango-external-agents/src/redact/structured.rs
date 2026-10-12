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

use super::embedded::Embedded;

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

/// How many digits make a number under a name that counts tokens a secret and not a count. The
/// largest count is a 32-bit counter's 4,294,967,295, ten digits, and no window or budget comes
/// near it; twelve digits is a secret that is made of digits. The text rules read the same number.
pub(super) const SECRET_DIGITS: usize = 12;

/// Whether `text`, a number written with separators or not, has few enough digits to be a count.
pub(super) fn digits_make_a_count(text: &str) -> bool {
    text.bytes().filter(u8::is_ascii_digit).count() < SECRET_DIGITS
}

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
pub(super) fn only_digits(text: &str) -> bool {
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
/// - a string that is itself a JSON object or array (up to 256 KiB, four strings deep) is walked
///   as structure, and written again, compactly, when the walk changed it; any other string is
///   returned as it came, `"123"` and `"null"` included;
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
    walk(value, Under::Nothing, 0, leaf)
}

/// [`structured`] with the rules of the release before this one reading every string that is not
/// under a credential's name: the floor the walk is held to in `redact::search`.
#[cfg(test)]
pub(super) fn structured_by_the_floor(value: &Value) -> Value {
    walk(value, Under::Nothing, 0, |text, _| {
        super::floor::stderr_text(text)
    })
}

/// How a string that is not under a credential's name is read, given how many JSON strings deep it
/// stands.
type Leaf = fn(&str, usize) -> String;

/// What the walk is below.
#[derive(Clone, Copy)]
pub(super) enum Under {
    /// Nothing that names a credential.
    Nothing,
    /// A member whose name is a credential's. The flag is whether the name counts something, so
    /// that a plain number below it is a count and stays.
    Credential { counts: bool },
}

impl Under {
    pub(super) fn credential(name: &str) -> Option<Self> {
        names_a_credential(name).then(|| Self::Credential {
            counts: names_a_count(name),
        })
    }
}

/// The walk below `value`, which is `embedded` strings deep inside JSON texts that were themselves
/// a string of the value.
fn walk(value: &Value, under: Under, embedded: usize, leaf: Leaf) -> Value {
    match (value, under) {
        // A container under a credential name is one marker, so that redacting never makes the
        // value larger: a megabyte of `1,` under `"token"` would otherwise come back as six.
        (Value::Object(_), Under::Credential { .. }) => Value::String(String::from(REDACTED)),
        (Value::Array(items), Under::Credential { counts: true })
            if items.iter().all(is_a_count) =>
        {
            value.clone()
        }
        (Value::Array(_), Under::Credential { .. }) => Value::String(String::from(REDACTED)),
        _ => walk_leaf_or_free(value, under, embedded, leaf),
    }
}

fn walk_leaf_or_free(value: &Value, under: Under, embedded: usize, leaf: Leaf) -> Value {
    match value {
        Value::Object(members) => Value::Object(object(members, embedded, leaf)),
        Value::Array(items) => Value::Array(array(items, under, embedded, leaf)),
        Value::String(text) => Value::String(string(text, under, embedded, leaf)),
        Value::Number(_) => match under {
            Under::Credential { counts: false } => Value::String(String::from(REDACTED)),
            Under::Credential { counts: true } if !is_a_count(value) => {
                Value::String(String::from(REDACTED))
            }
            _ => value.clone(),
        },
        other => other.clone(),
    }
}

/// Whether `value` is a number with few enough digits to be a count.
fn is_a_count(value: &Value) -> bool {
    value
        .as_number()
        .is_some_and(|number| digits_make_a_count(&number.to_string()))
}

fn string(text: &str, under: Under, embedded: usize, leaf: Leaf) -> String {
    match under {
        Under::Credential { counts: true } if only_digits(text) && digits_make_a_count(text) => {
            text.to_owned()
        }
        Under::Credential { .. } => String::from(REDACTED),
        Under::Nothing => leaf(text, embedded),
    }
}

/// A string that is not under a credential's name: read as the text it is by every reader, and,
/// when it is a JSON object or array, as the structure it holds.
///
/// The text rules read it as it is written (that is the floor: what the release before hid, this
/// hides), and the structure only adds to that: see [`embedded`](super::embedded).
fn leaf(text: &str, embedded: usize) -> String {
    super::read_until_stable(text, &Embedded { depth: embedded })
}

/// The member of `members` whose key, in any case, is one of `names`.
fn member<'a>(members: &'a Map<String, Value>, names: &[&str]) -> Option<(&'a String, &'a Value)> {
    members
        .iter()
        .find(|(key, _)| names.iter().any(|name| key.eq_ignore_ascii_case(name)))
}

fn object(members: &Map<String, Value>, embedded: usize, leaf: Leaf) -> Map<String, Value> {
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
            (key.clone(), walk(member, below, embedded, leaf))
        })
        .collect()
}

fn array(items: &[Value], under: Under, embedded: usize, leaf: Leaf) -> Vec<Value> {
    // `["Authorization", "Bearer ..."]`: the second entry is the credential.
    let pair = matches!(items, [Value::String(name), Value::String(_)] if Under::credential(name).is_some())
        && matches!(under, Under::Nothing);
    items
        .iter()
        .enumerate()
        .map(|(index, item)| match (pair, index) {
            (true, 1) => walk(item, Under::Credential { counts: false }, embedded, leaf),
            _ => walk(item, under, embedded, leaf),
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

    /// A count has at most ten digits; twelve or more under a name that counts tokens is a secret
    /// that is made of digits, as the text rules read it.
    /// An object under a name that counts tokens is one marker, as it is in the release before this
    /// one, however small its numbers are: Claude's `output_tokens_details` is hidden with it, and
    /// showing it is a decision of its own. A list of counts is kept.
    #[test]
    fn an_object_under_a_count_name_is_redacted_whole_and_a_list_of_counts_is_kept() {
        let usage = json!({
            "output_tokens_details": { "reasoning_tokens": 12 },
            "input_tokens": [1, 2, 3],
            "max_tokens": { "limit": 100_000_000_000_000_i64 },
            "api_key": { "n": 1 },
        });
        let shown = structured(&usage);
        assert_eq!(shown["input_tokens"], usage["input_tokens"]);
        for name in ["output_tokens_details", "max_tokens", "api_key"] {
            assert_eq!(
                shown[name], "[REDACTED]",
                "expected {name} redacted whole | received: {shown}"
            );
        }
    }

    #[test]
    fn a_number_of_twelve_digits_under_a_count_name_is_redacted() {
        let shown = structured(&json!({
            "max_tokens": 4294967295_u64,
            "tokens_used": 123456789012_u64,
            "total_tokens": "123456789012",
            "input_tokens": "4294967295",
            "tokens": [1, 2, 123456789012_u64],
            "output_tokens": [1, 2, 3],
        }));
        let expected = json!({
            "max_tokens": 4294967295_u64,
            "tokens_used": "[REDACTED]",
            "total_tokens": "[REDACTED]",
            "input_tokens": "4294967295",
            "tokens": "[REDACTED]",
            "output_tokens": [1, 2, 3],
        });
        assert_eq!(
            shown, expected,
            "expected counts of twelve digits redacted and shorter ones kept | received: {shown}"
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

    /// The string a value holds, or an empty one that fails the comparison next to it.
    fn text_of(value: &serde_json::Value) -> &str {
        value.as_str().unwrap_or_default()
    }

    /// A vendor that serialises its request into a field sends JSON as a string. The structure
    /// in it (a pair of members, a pair of entries) is as much a credential as one outside it.
    #[test]
    fn a_json_text_inside_a_string_is_read_as_structure() {
        let env = r#"[{"name":"OPENAI_API_KEY","value":"sk-live-1"},{"name":"HOME","value":"/h"}]"#;
        let headers =
            r#"{"headers":[["Authorization","Bearer sk-live-2"],["Accept","text/plain"]]}"#;
        let shown = structured(&json!({ "env": env, "request": headers, "retry_after": 30 }));
        let env_shown: serde_json::Value =
            serde_json::from_str(text_of(&shown["env"])).unwrap_or_default();
        assert_eq!(
            env_shown,
            json!([{ "name": "OPENAI_API_KEY", "value": "[REDACTED]" }, { "name": "HOME", "value": "/h" }]),
            "expected the value beside a credential name in an embedded array redacted | received: {shown}"
        );
        let request_shown: serde_json::Value =
            serde_json::from_str(text_of(&shown["request"])).unwrap_or_default();
        assert_eq!(
            request_shown,
            json!({ "headers": [["Authorization", "[REDACTED]"], ["Accept", "text/plain"]] }),
            "expected the entry beside a credential name in an embedded object redacted | received: {shown}"
        );
        assert_eq!(shown["retry_after"], 30);
    }

    #[test]
    fn a_json_text_inside_a_json_text_is_read_to_the_depth_the_bound_allows() {
        let innermost = json!([["Authorization", "Bearer sk-live-3"]]).to_string();
        let middle = json!({ "inner": innermost }).to_string();
        let shown = structured(&json!({ "outer": middle }));
        assert!(
            !shown.to_string().contains("sk-live-3"),
            "expected a credential two strings deep redacted | received: {shown}"
        );
        // Deeper than the bound is the text rules' alone, and costs no more than they do.
        let mut deep = json!("x");
        for _ in 0..8 {
            deep = json!({ "next": deep.to_string() });
        }
        let received = structured(&deep);
        assert!(
            received.to_string().len() <= deep.to_string().len(),
            "expected nesting to be redacted without growing | received: {received}"
        );
    }

    /// Only a text that is a container is read, and only when something in it changes is it written
    /// again: every other string comes back as it went in, spacing and key order included.
    #[test]
    fn a_string_that_is_no_json_container_or_holds_nothing_to_redact_is_kept_as_it_was() {
        for text in [
            r#"{"b" :  1, "a": [1,  2]}"#,
            "[ 1, 2 ]",
            "123",
            "null",
            "true",
            r#""quoted""#,
            "{not json}",
            "[1, 2,",
            "{\"a\": 1} trailing",
            "1e3",
        ] {
            let shown = structured(&json!({ "note": text }));
            assert_eq!(
                text_of(&shown["note"]),
                text,
                "expected {text:?} kept as it was | received: {shown}"
            );
        }
    }

    /// What is not a credential in an embedded text comes back as written: a number is not rounded
    /// into a float, a number is not trimmed of its zeros, a duplicate key is not dropped, and the
    /// members stay in the order they were written (not the order a map keeps them in).
    #[test]
    fn an_embedded_text_keeps_what_is_not_a_credential_byte_for_byte() {
        let text = concat!(
            r#"{"z": 100000000000000000000000000001, "big": 18446744073709551616, "#,
            r#""price": 1.50, "dup": 1, "dup": 2, "name": "API_KEY", "value": "sk-live-9", "#,
            r#""a": "caf\u00e9 \n", "second": 1e2}"#
        );
        let shown = structured(&json!({ "request": text }));
        let received = text_of(&shown["request"]);
        let expected = text.replace("sk-live-9", "[REDACTED]");
        assert_eq!(
            received, expected,
            "expected only the credential's value written again, every number, key and escape as it was | input {text:?}"
        );
    }

    /// A walk that finds nothing leaves the string alone, whitespace included.
    #[test]
    fn an_embedded_text_with_nothing_to_redact_is_returned_as_it_was() {
        let text = " {\"z\" : 100000000000000000000000000001,\n \"price\": 1.50} ";
        let shown = structured(&json!({ "request": text }));
        assert_eq!(
            text_of(&shown["request"]),
            text,
            "expected a text with no credential in it returned as it was"
        );
    }

    /// The whitespace around a text and in it stays where a value is redacted.
    #[test]
    fn an_embedded_text_keeps_its_spacing_where_a_value_is_redacted() {
        let text = "  [ [ \"Authorization\" , \"Bearer sk-live-8\" ] ,\n [ \"Accept\", 1.0 ] ]\n";
        let shown = structured(&json!({ "request": text }));
        assert_eq!(
            text_of(&shown["request"]),
            text.replace("\"Bearer sk-live-8\"", "\"[REDACTED]\""),
            "expected only the value after the header's name written again"
        );
    }

    /// A count that serde would read as a float is read as the digits it is written with.
    #[test]
    fn a_count_in_an_embedded_text_is_judged_by_the_digits_it_is_written_with() {
        let secret = r#"{"max_tokens": 100000000000000000000000000001, "retry": 30}"#;
        let shown = structured(&json!({ "request": secret }));
        assert_eq!(
            text_of(&shown["request"]),
            r#"{"max_tokens": "[REDACTED]", "retry": 30}"#,
            "expected thirty digits under a name that counts tokens redacted, as digits and not as 1e+29"
        );
        let count = r#"{"max_tokens": 4096, "retry": 30}"#;
        let shown = structured(&json!({ "request": count }));
        assert_eq!(
            text_of(&shown["request"]),
            count,
            "expected a count under a name that counts tokens kept"
        );
    }

    /// A string inside an embedded text is read as it is written (`\n` two characters) and then as
    /// it decodes (`\n` a line break), so that decoding never lets a value through that reading
    /// the text as written would have redacted.
    #[test]
    fn an_embedded_string_is_read_as_written_and_then_decoded() {
        let text = r#"{"name":"x","note":"pwd:\n- \"a\"\n  SEKRET0 \ntokens: \n12\r\nSEKRET1 "}"#;
        let by_text = crate::redact::message_text(text);
        assert!(
            !by_text.contains("SEKRET0"),
            "expected the text rules to redact the value in the text as written | received: {by_text:?}"
        );
        let shown = structured(&json!({ "request": text }));
        let received = text_of(&shown["request"]);
        assert!(
            !received.contains("SEKRET0"),
            "expected the value the text rules redact redacted in the walk's answer too | received: {received:?}"
        );
    }

    #[test]
    fn an_embedded_text_over_the_size_bound_is_left_to_the_text_rules() {
        let pair = r#"{"name":"X_API_KEY","value":"sk-live-4"},"#;
        let within = format!(
            "[{}{{}}]",
            pair.repeat(crate::redact::embedded::MAX_BYTES / pair.len() - 1)
        );
        let over = format!(
            "[{}{{}}]",
            pair.repeat(crate::redact::embedded::MAX_BYTES / pair.len() + 1)
        );
        assert!(
            within.len() <= crate::redact::embedded::MAX_BYTES
                && over.len() > crate::redact::embedded::MAX_BYTES
        );
        let read = structured(&json!({ "a": within }));
        assert!(
            !text_of(&read["a"]).contains("sk-live-4"),
            "expected a text of {} bytes read as structure | received a value that still holds it",
            within.len()
        );
        let unread = structured(&json!({ "a": over }));
        assert_eq!(
            text_of(&unread["a"]),
            crate::redact::stderr_text(&over),
            "expected a text of {} bytes left to the text rules",
            over.len()
        );
    }

    #[test]
    fn structure_read_inside_a_string_is_a_fixed_point() {
        let env = r#"[{"name":"OPENAI_API_KEY","value":"sk-live-5"},["Cookie","sid=1"]]"#;
        let once = structured(&json!({ "env": env, "b": [env] }));
        let twice = structured(&once);
        assert_eq!(once, twice, "expected a second walk to change nothing");
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
