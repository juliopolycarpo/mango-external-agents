//! Typed failures that survive the crossing from a vendor process to a host.
//!
//! Flattening a vendor failure to a string is what makes "it failed" the only thing anyone can
//! say afterwards, including the code deciding whether to try again. The vendor's own code, the
//! id of the request that failed and its retryability all survive into [`VendorError`]; the
//! library's own refusals are separate variants that carry the value they received.

use std::borrow::Cow;
use std::fmt;
use std::time::Duration;

use crate::harness::{Capability, HarnessKind};
use crate::redact;
use crate::transport::TransportKind;

/// The result of every fallible call in this crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A failure code in the harness's own vocabulary, prefixed by its vendor.
///
/// `codex-session-missing`, `claude-version-gate`, `acp-handshake-failed`: a log line names which
/// harness refused without needing a second field. The library never translates one, and a host
/// maps it to its own copy.
///
/// # Example
///
/// ```
/// use mango_external_agents::ErrorCode;
///
/// const MISSING: ErrorCode = ErrorCode::from_static("codex-session-missing");
/// assert_eq!(MISSING.as_str(), "codex-session-missing");
/// ```
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ErrorCode(Cow<'static, str>);

impl ErrorCode {
    /// Wraps a code a harness knows at compile time.
    pub const fn from_static(code: &'static str) -> Self {
        Self(Cow::Borrowed(code))
    }

    /// Wraps a code built at run time, such as one derived from a vendor frame.
    pub fn new(code: impl Into<String>) -> Self {
        Self(Cow::Owned(code.into()))
    }

    /// The code as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Cow::Borrowed(code) => formatter.write_str(code),
            Cow::Owned(_) => formatter.write_str("vendor-code"),
        }
    }
}

impl fmt::Debug for ErrorCode {
    /// Prints only static harness codes; vendor-provided codes remain data, not diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ErrorCode")
            .field(&self.to_string())
            .finish()
    }
}

/// A failure a vendor reported, with its own structure intact.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VendorError {
    /// The harness's own code for this failure.
    pub code: ErrorCode,
    /// The vendor's message as payload data. Event normalization bounds and strips it before a
    /// harness emits it; diagnostic formatting reports only metadata.
    pub message: String,
    /// The id of the request that failed, when the dialect correlates one.
    pub request_id: Option<String>,
    /// The vendor's own code, verbatim: a JSON-RPC number as text, or an enum it named.
    pub vendor_code: Option<String>,
    /// Whether an identical retry could plausibly succeed.
    pub retryable: bool,
}

impl VendorError {
    /// A vendor failure with nothing correlated to it.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{ErrorCode, VendorError};
    ///
    /// let error = VendorError::new(ErrorCode::from_static("claude-stream-broken"), "stream ended");
    /// assert!(!error.retryable);
    /// ```
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            request_id: None,
            vendor_code: None,
            retryable: false,
        }
    }

    /// Names the request this failure answers.
    #[must_use]
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// Records the vendor's own code and whether retrying it could work.
    #[must_use]
    pub fn with_vendor_code(mut self, vendor_code: impl Into<String>, retryable: bool) -> Self {
        self.vendor_code = Some(vendor_code.into());
        self.retryable = retryable;
        self
    }
}

impl fmt::Display for VendorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: vendor failure", self.code)
    }
}

impl fmt::Debug for VendorError {
    /// Formats vendor text for logs without exposing its unstructured payload.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VendorError")
            .field("code", &self.code)
            .field("message_bytes", &self.message.len())
            .field("has_request_id", &self.request_id.is_some())
            .field("has_vendor_code", &self.vendor_code.is_some())
            .field("retryable", &self.retryable)
            .finish()
    }
}

impl std::error::Error for VendorError {}

/// Everything that can go wrong between a host and a vendor CLI.
#[non_exhaustive]
pub enum Error {
    /// The vendor answered with a failure of its own.
    Vendor(VendorError),

    /// An optional method this harness does not implement.
    ///
    /// Every optional method on [`Session`](crate::Session) defaults to this, so a host that calls
    /// one on a harness that cannot do it receives a typed refusal rather than a panic.
    NotSupported {
        /// The capability the caller asked for.
        capability: Capability,
    },

    /// A (harness, transport) pair the harness does not declare.
    ///
    /// Refused before anything is spawned: the pair is a fact about the two kinds, knowable
    /// without touching the machine.
    UnsupportedTransport {
        /// The harness that was asked.
        harness: HarnessKind,
        /// The transport kind it does not speak.
        transport: TransportKind,
    },

    /// The installed CLI is older than the harness's pinned floor.
    VersionGate {
        /// The version the CLI reported.
        found: String,
        /// The oldest version this harness drives.
        minimum: String,
    },

    /// The CLI is installed but nobody is signed in.
    ///
    /// `login_hint` is the vendor's own command as text, for the host to show. The library never
    /// runs it: it does not handle logins, and it never reads or forwards a credential.
    AuthRequired {
        /// The vendor's login command, verbatim, for the host to display.
        login_hint: String,
    },

    /// The CLI could not be found, or could not be started.
    Launch {
        /// The executable the launcher was asked for.
        program: String,
        /// What the host's launcher reported.
        message: String,
    },

    /// The byte link to the vendor failed, or was closed under a caller.
    Link {
        /// The peer as a user would name it, such as `Codex app-server`.
        peer: String,
        /// What went wrong.
        message: String,
    },

    /// A line or a buffer passed the cap the library reads vendors under.
    ///
    /// A vendor that prints a 100 MB line is a bug, not a request to allocate 100 MB.
    LimitExceeded {
        /// What was being read, such as `one stdout line`.
        subject: &'static str,
        /// The cap that was passed.
        limit: usize,
        /// How much had been read when the cap was passed.
        received: usize,
    },

    /// A vendor value could not be made safe to keep, so it was refused rather than repaired.
    ///
    /// Truncating a label is safe; truncating an opaque id that is later echoed to the vendor
    /// would silently point at a different object.
    InvalidVendorValue {
        /// Which field the vendor filled in, such as `native session id`.
        field: &'static str,
        /// What arrived, already bounded so the message itself stays small.
        received: String,
    },

    /// A vendor frame did not have the shape its dialect promises.
    Protocol {
        /// The shape the dialect documents.
        expected: String,
        /// What arrived instead, bounded.
        received: String,
    },

    /// A call did not answer inside its deadline.
    Timeout {
        /// The call that stalled, such as a JSON-RPC method name.
        operation: String,
        /// How long it was given.
        after: Duration,
    },

    /// The caller's cancellation token fired, or the turn was cancelled under it.
    Cancelled {
        /// Why it stopped.
        reason: crate::session::CancelReason,
    },

    /// The session or the link is closed and cannot serve this call.
    Closed {
        /// What was closed, such as `session` or `link`.
        subject: &'static str,
    },

    /// The host's configuration was incomplete or contradictory.
    HostConfiguration {
        /// What the library needed.
        expected: &'static str,
        /// A summary of what it was given, written verbatim by [`Display`](fmt::Display).
        ///
        /// This is the one diagnostic that still names what arrived, because a host
        /// configuration failure is the host's own to fix and "invalid" tells it nothing. Every
        /// construction site in the library crates therefore owes a payload-free summary — a
        /// count, a shape, a static phrase — and never a value a host or a vendor supplied.
        received: String,
    },
}

impl fmt::Display for Error {
    /// Formats failures for diagnostics without including raw vendor or host-provided payloads.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vendor(error) => error.fmt(formatter),
            Self::NotSupported { capability } => write!(
                formatter,
                "expected a harness that supports {capability}, received one that does not"
            ),
            Self::UnsupportedTransport { harness, transport } => write!(
                formatter,
                "expected a transport {harness} supports, received {transport}"
            ),
            Self::VersionGate { minimum, found } => write!(
                formatter,
                "expected a configured minimum version, received a vendor-reported version ({} and {} bytes)",
                minimum.len(),
                found.len()
            ),
            Self::AuthRequired { .. } => formatter.write_str(
                "expected a signed-in CLI, received a signed-out one; the vendor login command is available on the error",
            ),
            Self::Launch { program, .. } => write!(
                formatter,
                "expected to launch {}, received a launcher failure",
                redact::program_name(program)
            ),
            Self::Link { message, .. } => write!(
                formatter,
                "the vendor link failed: {}",
                safe_link_context(message)
            ),
            Self::LimitExceeded {
                subject,
                limit,
                received,
            } => write!(
                formatter,
                "expected at most {limit} bytes of {subject}, received {received}"
            ),
            Self::InvalidVendorValue { field, .. } => {
                write!(formatter, "expected a usable {field}, received invalid vendor data")
            }
            Self::Protocol { expected, .. } => write!(
                formatter,
                "expected a protocol response shape ({} bytes), received invalid vendor data",
                expected.len()
            ),
            Self::Timeout { operation, after } => write!(
                formatter,
                "expected an operation ({} bytes) to answer within {after:?}, received nothing",
                operation.len()
            ),
            Self::Cancelled { reason } => write!(formatter, "cancelled: {reason}"),
            Self::Closed { subject } => {
                write!(formatter, "expected an open {subject}, received a closed one")
            }
            Self::HostConfiguration { expected, received } => {
                write!(formatter, "expected {expected}, received {received}")
            }
        }
    }
}

impl fmt::Debug for Error {
    /// Delegates debug formatting to the log-safe display form.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("Error")
            .field(&self.to_string())
            .finish()
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Vendor(error) => Some(error),
            _ => None,
        }
    }
}

impl From<VendorError> for Error {
    fn from(error: VendorError) -> Self {
        Self::Vendor(error)
    }
}

fn safe_link_context(message: &str) -> &'static str {
    if message.contains("EPIPE") {
        return "EPIPE";
    }
    if message.contains("exited") {
        return "peer exited";
    }
    "no safe detail"
}

impl Error {
    /// The typed refusal every unimplemented optional method returns.
    pub fn not_supported(capability: Capability) -> Self {
        Self::NotSupported { capability }
    }

    /// Whether an identical retry could plausibly succeed.
    ///
    /// Only a vendor failure can say so; everything else is a statement about this call being
    /// wrong, or about a link that is already gone.
    pub fn retryable(&self) -> bool {
        match self {
            Self::Vendor(error) => error.retryable,
            _ => false,
        }
    }
}

/// Whether a JSON-RPC code is worth a second attempt.
///
/// The reserved range is protocol-level and means this client sent something wrong; retrying the
/// identical call would fail identically. A vendor's own application codes sit outside it, where a
/// retry can legitimately succeed.
///
/// # Example
///
/// ```
/// use mango_external_agents::error::jsonrpc_code_is_retryable;
///
/// assert!(!jsonrpc_code_is_retryable(-32601)); // method not found
/// assert!(jsonrpc_code_is_retryable(-31000)); // a vendor's own code
/// ```
pub const fn jsonrpc_code_is_retryable(code: i64) -> bool {
    code > -32000 || code < -32768
}

#[cfg(test)]
mod tests {
    use super::{Error, ErrorCode, VendorError, jsonrpc_code_is_retryable};
    use crate::harness::Capability;

    #[test]
    fn reserved_jsonrpc_codes_are_not_retryable() {
        for code in [-32768, -32700, -32600, -32601, -32602, -32603, -32000] {
            assert!(
                !jsonrpc_code_is_retryable(code),
                "expected {code} to be non-retryable, received retryable"
            );
        }
    }

    #[test]
    fn vendor_codes_outside_the_reserved_range_are_retryable() {
        for code in [-32769, -31999, -1, 0, 1000] {
            assert!(
                jsonrpc_code_is_retryable(code),
                "expected {code} to be retryable, received non-retryable"
            );
        }
    }

    #[test]
    fn only_a_vendor_failure_can_be_retryable() {
        let vendor = Error::Vendor(
            VendorError::new(ErrorCode::from_static("codex-busy"), "busy")
                .with_vendor_code("-31000", true),
        );
        assert!(vendor.retryable());
        assert!(!Error::not_supported(Capability::Steering).retryable());
    }

    #[test]
    fn messages_name_the_expected_and_the_received_value() {
        let error = Error::InvalidVendorValue {
            field: "native session id",
            received: String::from("   "),
        };
        assert_eq!(
            error.to_string(),
            "expected a usable native session id, received invalid vendor data"
        );
    }

    #[test]
    fn protocol_diagnostics_omit_vendor_text_and_expected_shape_contents() {
        let error = Error::Protocol {
            expected: String::from("a captured vendor response diagnostic-secret"),
            received: String::from("Authorization: Bearer diagnostic-secret"),
        };

        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("diagnostic-secret"),
                "expected redacted error diagnostic, received {rendered}"
            );
            assert!(
                rendered.contains("protocol response shape (44 bytes)"),
                "expected useful shape metadata, received {rendered}"
            );
        }
    }

    #[test]
    fn host_configuration_diagnostics_keep_the_summary_the_caller_built() {
        let error = Error::HostConfiguration {
            expected: "an absolute UTF-8 scratch directory visible to the Claude child",
            received: String::from("a relative path"),
        };

        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                rendered.contains("a relative path"),
                "expected the remediation summary to survive, received {rendered}"
            );
        }
    }

    #[test]
    fn vendor_error_diagnostics_redact_vendor_messages() {
        let error = VendorError::new(
            ErrorCode::from_static("vendor-failed"),
            "vendor-message-secret",
        );

        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(
                !rendered.contains("vendor-message-secret"),
                "expected redacted vendor diagnostic, received {rendered}"
            );
            assert!(
                rendered.contains("vendor-failed"),
                "expected the vendor code, received {rendered}"
            );
        }
    }
}
