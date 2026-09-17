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
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/initialization>

use agent_client_protocol::schema::v1::ErrorCode as AcpErrorCode;
use mango_external_agents::error::jsonrpc_code_is_retryable;
use mango_external_agents::normalize::{TextLimit, bound_text};
use mango_external_agents::{Error, ErrorCode, VendorError};

/// One ACP failure as a vendor failure, correlated to the method that produced it.
///
/// # Example
///
/// ```
/// use agent_client_protocol::schema::v1::Error as AcpError;
/// use mango_agent_acp::error::vendor_error;
///
/// let failure = vendor_error("session/prompt", &AcpError::internal_error());
/// assert_eq!(failure.code.as_str(), "acp-request-failed");
/// assert_eq!(failure.request_id.as_deref(), Some("session/prompt"));
/// assert!(!failure.retryable, "the reserved range is this client's own mistake");
/// ```
#[must_use]
pub fn vendor_error(method: &str, error: &agent_client_protocol::Error) -> VendorError {
    let code = i32::from(error.code);
    VendorError::new(ErrorCode::from_static("acp-request-failed"), message(error))
        .with_request_id(method)
        .with_vendor_code(code.to_string(), jsonrpc_code_is_retryable(i64::from(code)))
}

/// One ACP failure as the core error a session method returns.
///
/// `login_hint` is the profile's own, and is used only for the authentication code — every other
/// code is the agent's business and reaches the host as a [`VendorError`]. A custom profile's hint
/// is host-authored text, not a library constant, so it is bounded the same way a vendor-observed
/// one is in [`AuthState::normalized`](mango_external_agents::AuthState::normalized): stripped of
/// control characters and cut to a label's length before it can reach a diagnostic.
///
/// # Example
///
/// ```
/// use agent_client_protocol::schema::v1::Error as AcpError;
/// use mango_agent_acp::error::request_error;
/// use mango_external_agents::Error;
///
/// let error = request_error("session/new", &AcpError::auth_required(), "opencode auth login");
/// assert!(matches!(error, Error::AuthRequired { login_hint } if login_hint == "opencode auth login"));
/// ```
#[must_use]
pub fn request_error(
    method: &str,
    error: &agent_client_protocol::Error,
    login_hint: &str,
) -> Error {
    if error.code == AcpErrorCode::AuthRequired {
        return Error::AuthRequired {
            login_hint: bound_text(login_hint, TextLimit::Title).text,
        };
    }
    Error::Vendor(vendor_error(method, error))
}

/// The agent's message, with whatever it put in `data` appended.
///
/// `data` is where an agent explains itself — which file, which command, which limit — and dropping
/// it is what makes "internal error" the only thing anyone can say afterwards. It is rendered
/// compactly and the core bounds and sanitises it downstream like any other vendor text.
fn message(error: &agent_client_protocol::Error) -> String {
    match &error.data {
        Some(data) => format!("{} ({data})", error.message),
        None => error.message.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{request_error, vendor_error};
    use agent_client_protocol::schema::v1::Error as AcpError;
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
    fn the_authentication_code_becomes_a_login_hint_rather_than_a_vendor_failure() {
        let error = request_error("session/new", &AcpError::auth_required(), "agent login");
        let Error::AuthRequired { login_hint } = error else {
            panic!("expected AuthRequired, received {error:?}");
        };
        assert_eq!(login_hint, "agent login");
    }

    #[test]
    fn a_custom_profiles_login_hint_is_bounded_like_any_other_label() {
        let hostile = format!("run\u{7}\x1b[31m login {}", "x".repeat(4_000));
        let error = request_error("session/new", &AcpError::auth_required(), &hostile);
        let Error::AuthRequired { login_hint } = error else {
            panic!("expected AuthRequired, received {error:?}");
        };
        assert!(
            !login_hint.contains('\u{7}') && !login_hint.contains('\x1b'),
            "received a login hint with unstripped control characters: {login_hint:?}"
        );
        assert_eq!(login_hint.chars().count(), 256);
    }

    #[test]
    fn every_other_code_stays_the_agents_own_business() {
        let error = request_error("session/new", &AcpError::internal_error(), "agent login");
        assert!(
            matches!(error, Error::Vendor(_)),
            "expected a vendor failure, received {error:?}"
        );
    }
}
