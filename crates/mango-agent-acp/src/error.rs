//! ACP's JSON-RPC failures as the core's typed ones.
//!
//! Two things survive the crossing that a flattened string would lose: the agent's own numeric code,
//! and whether retrying the identical call could work. The reserved JSON-RPC range means *this
//! client sent something wrong*, so an identical retry fails identically; an agent's own code sits
//! outside it, where a retry can legitimately succeed. The core owns that rule
//! ([`jsonrpc_code_is_retryable`]) rather than this crate guessing it per method.
//!
//! One code is not a vendor failure at all: `-32000`, which ACP defines as "authentication
//! required". That becomes [`Error::AuthRequired`] carrying the profile's own documented login
//! command as text — the library does not send `authenticate`, does not open a browser and does not
//! read a credential, so the only useful thing it can do is say what a person should run.
//!
//! The same code answered to `session/prompt`, mid-session, is a turn's failure and not a setup
//! error: a turn's terminal is a [`VendorError`], which has no login-hint field. It ends as
//! [`AUTH_REQUIRED_CODE`] instead of `acp-request-failed`, with the profile's login command in
//! its message and none of the agent's own text.
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/authentication>, whose "Active
//! Sessions" passage lets an agent "return `auth_required` errors for future session activity". The
//! number `-32000` is the schema crate's `ErrorCode::AuthRequired`.

use agent_client_protocol_schema::v1::ErrorCode as AcpErrorCode;
use mango_external_agents::error::jsonrpc_code_is_retryable;
use mango_external_agents::{Error, ErrorCode, VendorError};

/// The code a turn's terminal carries when the agent answered `session/prompt` with ACP's
/// authentication-required error (`-32000`).
///
/// Stable. A host branches on it the way it branches on [`Error::AuthRequired`] from
/// `open_session`: the person has to sign in again before the next prompt can work.
///
/// # Example
///
/// ```
/// use mango_agent_acp::error::AUTH_REQUIRED_CODE;
///
/// assert_eq!(AUTH_REQUIRED_CODE, "acp-auth-required");
/// ```
pub const AUTH_REQUIRED_CODE: &str = "acp-auth-required";

/// One ACP failure as a vendor failure, correlated to the method that produced it.
///
/// # Example
///
/// ```
/// use agent_client_protocol_schema::v1::Error as AcpError;
/// use mango_agent_acp::error::vendor_error;
///
/// let failure = vendor_error("session/prompt", &AcpError::internal_error());
/// assert_eq!(failure.code.as_str(), "acp-request-failed");
/// assert_eq!(failure.request_id.as_deref(), Some("session/prompt"));
/// assert!(!failure.retryable, "the reserved range is this client's own mistake");
/// ```
#[must_use]
pub fn vendor_error(method: &str, error: &agent_client_protocol_schema::v1::Error) -> VendorError {
    let code = i32::from(error.code);
    VendorError::new(ErrorCode::from_static("acp-request-failed"), message(error))
        .with_request_id(method)
        .with_vendor_code(code.to_string(), jsonrpc_code_is_retryable(i64::from(code)))
}

/// One ACP failure as the core error a session method returns.
///
/// `login_hint` is the profile's own, and is used only for the authentication code — every other
/// code is the agent's business and reaches the host as a [`VendorError`]. The hint remains exact
/// for the host UI; [`std::fmt::Display`] deliberately does not render it because a custom profile
/// can supply caller-owned text.
///
/// # Example
///
/// ```
/// use agent_client_protocol_schema::v1::Error as AcpError;
/// use mango_agent_acp::error::request_error;
/// use mango_external_agents::Error;
///
/// let error = request_error("session/new", &AcpError::auth_required(), "opencode auth login");
/// assert!(matches!(error, Error::AuthRequired { login_hint } if login_hint == "opencode auth login"));
/// ```
#[must_use]
pub fn request_error(
    method: &str,
    error: &agent_client_protocol_schema::v1::Error,
    login_hint: &str,
) -> Error {
    if error.code == AcpErrorCode::AuthRequired {
        return Error::AuthRequired {
            login_hint: login_hint.to_owned(),
        };
    }
    Error::Vendor(vendor_error(method, error))
}

/// One ACP failure that answered `session/prompt`, as the failure a turn's terminal carries.
///
/// Any code but authentication-required is [`vendor_error`]. That one becomes
/// [`AUTH_REQUIRED_CODE`], keeping the request and the agent's code, with a message made of the
/// profile's `login_hint` and nothing the agent wrote: the hint is host-owned text a person can
/// act on, while the agent's message and `data` are whatever it held when its token failed. The
/// core bounds the message like any other. The library never signs in; the text only says how.
///
/// ```ignore
/// let failure = prompt_failure("session/prompt", &AcpError::auth_required(), "opencode auth login");
/// assert_eq!(failure.code.as_str(), "acp-auth-required");
/// ```
pub(crate) fn prompt_failure(
    method: &str,
    error: &agent_client_protocol_schema::v1::Error,
    login_hint: &str,
) -> VendorError {
    if error.code != AcpErrorCode::AuthRequired {
        return vendor_error(method, error);
    }
    let code = i32::from(error.code);
    VendorError::new(
        ErrorCode::from_static(AUTH_REQUIRED_CODE),
        format!("authentication required; to sign in: {login_hint}"),
    )
    .with_request_id(method)
    .with_vendor_code(code.to_string(), false)
}

/// The agent's message, with whatever it put in `data` appended, redacted of credential-shaped text.
///
/// `data` is where an agent explains itself — which file, which command, which limit — and dropping
/// it is what makes "internal error" the only thing anyone can say afterwards. It is rendered
/// compactly. A turn's terminal is bounded and sanitised downstream by the core's event sink, like
/// any other vendor text; a refusal of a setup request is returned as an error and never passes it.
///
/// Redacted here, whole, with the data appended and before anything cuts it, so both reach the
/// host redacted. The core's sink redacts a terminal again, which changes nothing a second time.
///
/// An explicit `"data": null` says nothing, so it reads as no data. The connection already
/// drops it while parsing; this arm holds the rule for an error built any other way, where the
/// message would otherwise grow a `(null)` suffix.
fn message(error: &agent_client_protocol_schema::v1::Error) -> String {
    let text = match &error.data {
        Some(data) if !data.is_null() => {
            format!("{} ({})", error.message, redact_credential_keys(data))
        }
        _ => error.message.clone(),
    };
    mango_external_agents::redact::stderr_text(&text)
}

/// `data` with every string under a credential-named key replaced, at any depth, and every other
/// string redacted on its own.
///
/// The redactor reads `api_key=value`, and a JSON object writes the same thing as
/// `"api_key":"value"`, with a quote between the name and the colon, which it does not read. So the
/// structure is used while it is still structure. A string anywhere under a key that names a
/// secret, a token, a password, a credential, an API key or an authorization header becomes
/// `[REDACTED]`, and so does the `value` of an object whose `name` is such a name, which is how an
/// environment variable or an HTTP header is written. Every other string is redacted as text before
/// it is rendered, because rendering escapes a line break into two characters the redactor does not
/// skip. Numbers and booleans are kept, so `"retry_after":30` and a sibling `"path"` stay readable.
fn redact_credential_keys(value: &serde_json::Value) -> serde_json::Value {
    redact_under(value, false)
}

/// [`redact_credential_keys`] below a point that may already be a credential's.
fn redact_under(value: &serde_json::Value, credential: bool) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(members) => {
            let named = members
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(names_a_credential);
            Value::Object(
                members
                    .iter()
                    .map(|(key, member)| {
                        let secret =
                            credential || names_a_credential(key) || (named && key == "value");
                        (key.clone(), redact_under(member, secret))
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| redact_under(item, credential))
                .collect(),
        ),
        Value::String(_) if credential => Value::String(String::from("[REDACTED]")),
        Value::String(text) => Value::String(mango_external_agents::redact::stderr_text(text)),
        other => other.clone(),
    }
}

/// Whether a JSON key names a credential, however it is cased or separated: `api_key`, `apiKey`,
/// `refresh-token`, `Authorization`, `client_secret`, `pass`.
fn names_a_credential(key: &str) -> bool {
    const NAMES: [&str; 8] = [
        "secret",
        "token",
        "password",
        "passwd",
        "pwd",
        "credential",
        "apikey",
        "authorization",
    ];
    let squashed: String = key
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .collect();
    squashed == "pass" || NAMES.iter().any(|name| squashed.contains(name))
}

#[cfg(test)]
mod tests {
    use super::{
        AUTH_REQUIRED_CODE, names_a_credential, prompt_failure, redact_credential_keys,
        request_error, vendor_error,
    };
    use agent_client_protocol_schema::v1::Error as AcpError;
    use mango_external_agents::Error;

    /// The reserved range says this client sent something wrong, so an identical retry fails
    /// identically. An agent's own code sits outside it, where a retry can work.
    #[test]
    fn retryability_follows_the_json_rpc_range_rather_than_the_method() {
        assert!(!vendor_error("initialize", &AcpError::method_not_found()).retryable);
        assert!(!vendor_error("initialize", &AcpError::invalid_params()).retryable);
        assert!(
            vendor_error("session/prompt", &AcpError::new(-31_000, "model busy")).retryable,
            "an agent's own code is outside the reserved range"
        );
    }

    #[test]
    fn the_agents_own_numeric_code_survives_the_crossing() {
        let failure = vendor_error("session/new", &AcpError::new(-31_004, "no workspace"));
        assert_eq!(failure.vendor_code.as_deref(), Some("-31004"));
        assert_eq!(failure.message, "no workspace");
        assert_eq!(failure.request_id.as_deref(), Some("session/new"));
    }

    /// `data` is where an agent says which file or which limit, and dropping it leaves "internal
    /// error" as the only thing anyone can report.
    #[test]
    fn whatever_the_agent_put_in_data_reaches_the_message() {
        let failure = vendor_error(
            "session/prompt",
            &AcpError::internal_error().data(serde_json::json!({ "path": "/repo/x.rs" })),
        );
        assert!(
            failure.message.contains("/repo/x.rs"),
            "received {:?}",
            failure.message
        );
    }

    #[test]
    fn null_data_adds_nothing_to_the_message() {
        let failure = vendor_error(
            "session/new",
            &AcpError::new(-31_004, "no workspace").data(serde_json::Value::Null),
        );
        assert_eq!(
            failure.message, "no workspace",
            "expected vendor message: \"no workspace\" | received: {:?}",
            failure.message
        );
    }

    #[test]
    fn the_authentication_code_becomes_a_login_hint_rather_than_a_vendor_failure() {
        let error = request_error("session/new", &AcpError::auth_required(), "agent login");
        let Error::AuthRequired { login_hint } = error else {
            panic!("expected AuthRequired, received {error:?}");
        };
        assert_eq!(login_hint, "agent login");
    }

    #[test]
    fn a_custom_profiles_login_hint_stays_typed_but_out_of_diagnostics() {
        let hostile = format!(
            "run\u{7}\x1b[31m login credential=profile-secret {}",
            "x".repeat(4_000)
        );
        let error = request_error("session/new", &AcpError::auth_required(), &hostile);
        let Error::AuthRequired { login_hint } = &error else {
            panic!("expected AuthRequired, received {error:?}");
        };
        assert!(
            login_hint == &hostile,
            "expected the host UI to receive its exact hint, received {login_hint:?}"
        );
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("profile-secret"),
                "expected no caller-owned login hint in diagnostics, received {rendered:?}"
            );
        }
    }

    #[test]
    fn every_other_code_stays_the_agents_own_business() {
        let error = request_error("session/new", &AcpError::internal_error(), "agent login");
        assert!(
            matches!(error, Error::Vendor(_)),
            "expected a vendor failure, received {error:?}"
        );
    }

    #[test]
    fn an_authentication_refusal_of_a_prompt_carries_the_hint_and_none_of_the_agents_text() {
        let refusal =
            AcpError::auth_required().data(serde_json::json!({ "token": "sk-agent-secret" }));
        let failure = prompt_failure("session/prompt", &refusal, "agent login");
        let received = (
            failure.code.as_str().to_owned(),
            failure.request_id.clone(),
            failure.vendor_code.clone(),
            failure.retryable,
            failure.message.clone(),
        );
        let expected = (
            String::from(AUTH_REQUIRED_CODE),
            Some(String::from("session/prompt")),
            Some(String::from("-32000")),
            false,
            String::from("authentication required; to sign in: agent login"),
        );
        assert_eq!(
            received, expected,
            "expected (code, request, vendor code, retryable, message): {expected:?} | received: {received:?}"
        );
    }

    #[test]
    fn any_other_refusal_of_a_prompt_is_the_plain_vendor_failure() {
        let refusal = AcpError::new(-31_000, "model busy");
        let failure = prompt_failure("session/prompt", &refusal, "agent login");
        assert_eq!(
            failure,
            vendor_error("session/prompt", &refusal),
            "expected the unchanged vendor failure for a non-authentication code"
        );
    }

    #[test]
    fn a_key_names_a_credential_however_it_is_cased_or_separated() {
        let received: Vec<bool> = [
            "api_key",
            "apiKey",
            "API-KEY",
            "refresh_token",
            "clientSecret",
            "Authorization",
            "password",
            "pass",
            "pwd",
            "path",
            "retry_after",
            "key",
        ]
        .map(names_a_credential)
        .to_vec();
        let expected = vec![
            true, true, true, true, true, true, true, true, true, false, false, false,
        ];
        assert_eq!(
            received, expected,
            "expected which keys name a credential: {expected:?} | received: {received:?}"
        );
    }

    #[test]
    fn only_text_under_a_credential_key_is_replaced_at_any_depth() {
        let data = serde_json::json!({
            "token": "t-1",
            "expires": 30,
            "nested": [{ "password": "p-1", "path": "/repo" }],
            "tokens": ["t-2", { "inner": "t-3" }],
            "env": [{ "name": "OPENAI_API_KEY", "value": "v-1" }, { "name": "HOME", "value": "/h" }],
            "tokens_used": { "count": 4 },
        });
        let received = redact_credential_keys(&data);
        let expected = serde_json::json!({
            "token": "[REDACTED]",
            "expires": 30,
            "nested": [{ "password": "[REDACTED]", "path": "/repo" }],
            "tokens": ["[REDACTED]", { "inner": "[REDACTED]" }],
            "env": [{ "name": "OPENAI_API_KEY", "value": "[REDACTED]" }, { "name": "HOME", "value": "/h" }],
            "tokens_used": { "count": 4 },
        });
        assert_eq!(
            received, expected,
            "expected text under credential keys replaced: {expected} | received: {received}"
        );
    }

    #[test]
    fn a_credential_in_the_message_or_in_data_is_redacted_in_the_vendor_failure() {
        let failure = vendor_error(
            "session/new",
            &AcpError::new(-31_004, "no workspace API_KEY=sk-live-1")
                .data(serde_json::json!({ "secret": "s-1" })),
        );
        assert!(
            !failure.message.contains("sk-live-1") && !failure.message.contains("s-1"),
            "expected a message without the credentials | received: {:?}",
            failure.message
        );
        assert!(
            failure.message.starts_with("no workspace "),
            "expected the words to stay readable | received: {:?}",
            failure.message
        );
    }
}
