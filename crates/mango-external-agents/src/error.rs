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
/// Part of a code can still be vendor text — `claude-{subtype}` takes its tail from a result
/// frame — so [`Display`](fmt::Display) prints only what has a label's shape. `as_str` is the
/// protocol field and keeps the code as written.
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

/// The longest a code may be before it stops being a label and starts being a sentence.
const CODE_MAX_LENGTH: usize = 48;

impl fmt::Display for ErrorCode {
    /// Writes the code when it has a label's shape, and `vendor-code` when it does not.
    ///
    /// Shape, not allocation. [`ErrorCode::new`] is how this crate mints its own codes —
    /// `claude-{subtype}` from a result frame, `{peer}-call-failed` from a JSON-RPC client — and
    /// `#[serde(transparent)]` hands back a `Cow::Owned` for a code that was
    /// [`from_static`](ErrorCode::from_static) before it crossed a wire, so where the bytes live
    /// says nothing about where they came from. What is safe to print is a short lowercase label:
    /// at most 48 bytes of `a-z`, `0-9`, `-` and `_`. A code carrying a space, a
    /// capital, a quote or more length than that is vendor prose wearing a code's field, and it
    /// is reported as `vendor-code` instead. [`as_str`](ErrorCode::as_str) stays the protocol
    /// field and is never bounded.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if is_label_shaped(&self.0) {
            return formatter.write_str(&self.0);
        }
        formatter.write_str("vendor-code")
    }
}

impl fmt::Debug for ErrorCode {
    /// Prints only label-shaped codes; vendor prose remains data, not diagnostics.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ErrorCode")
            .field(&self.to_string())
            .finish()
    }
}

/// Whether a code reads as a label a log line can carry rather than as vendor text.
fn is_label_shaped(code: &str) -> bool {
    !code.is_empty()
        && code.len() <= CODE_MAX_LENGTH
        && code.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        })
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
        ///
        /// Vendor stdout: a harness that could not parse a version falls back to the line the CLI
        /// printed, so [`Display`](fmt::Display) reports its size rather than its text.
        found: String,
        /// The oldest version this harness drives.
        ///
        /// The harness's own compile-time constant, and the half of this error a user can act on,
        /// so [`Display`](fmt::Display) names it.
        minimum: String,
    },

    /// The CLI is installed but nobody is signed in.
    ///
    /// `login_hint` is the vendor's own command as text, for the host to show. The library never
    /// runs it: it does not handle logins, and it never reads or forwards a credential.
    AuthRequired {
        /// The vendor's login command, verbatim, for the host to display.
        ///
        /// Every harness sets this from a constant of its own — `claude auth login`,
        /// `codex login`, an ACP profile's literal — so it is a published command rather than
        /// vendor output, and [`Display`](fmt::Display) shows it.
        login_hint: String,
    },

    /// The CLI could not be found, or could not be started.
    Launch {
        /// The executable the launcher was asked for.
        ///
        /// Host-provided text: [`Display`](fmt::Display) reports it through
        /// [`redact::program_name`], which names the programs this workspace drives and calls
        /// anything else a custom executable.
        program: String,
        /// A payload-free summary of what stopped the launch, written verbatim by
        /// [`Display`](fmt::Display).
        ///
        /// Named for the same reason `HostConfiguration`'s `received` is: an absent executable, a
        /// permission refusal and a process tree that would not end are the operator's own to
        /// fix, and "a launcher failure" does not distinguish them. Every construction site
        /// therefore owes a summary built from a structured value — an [`std::io::ErrorKind`], a
        /// duration, a static phrase — and never a path, an argv or a line a program printed.
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
        /// The shape the dialect documents, written verbatim by [`Display`](fmt::Display).
        ///
        /// The library's own sentence about what it was reading — `a turn/start result with a
        /// non-empty turn id`, `protocol version 1` — and without it a host is told a frame was
        /// wrong but not which field. Every construction site therefore owes a shape it wrote
        /// itself, and summarises anything a vendor filled in: `one of the 3 options this request
        /// offered`, never the ids themselves.
        expected: String,
        /// What arrived instead, bounded. Vendor data: never written to a diagnostic.
        received: String,
    },

    /// A call did not answer inside its deadline.
    Timeout {
        /// The call that stalled, written verbatim by [`Display`](fmt::Display).
        ///
        /// A peer name and a method name, both protocol identifiers the library or a profile
        /// names — `Codex app-server thread/resume`, `session/prompt on ACP agent gemini`. Which
        /// call stalled is the whole content of a timeout, so every construction site owes a name
        /// it wrote itself rather than a value a vendor sent.
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
                "expected version {minimum} or newer, received a vendor-reported version ({} bytes)",
                found.len()
            ),
            Self::AuthRequired { login_hint } => write!(
                formatter,
                "expected a signed-in CLI, received a signed-out one; the vendor's own command is `{login_hint}`"
            ),
            Self::Launch { program, message } => write!(
                formatter,
                "expected to launch {}, received {message}",
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
                write!(
                    formatter,
                    "expected a usable {field}, received invalid vendor data"
                )
            }
            Self::Protocol { expected, .. } => write!(
                formatter,
                "expected {expected}, received invalid vendor data"
            ),
            Self::Timeout { operation, after } => write!(
                formatter,
                "expected {operation} to answer within {after:?}, received nothing"
            ),
            Self::Cancelled { reason } => write!(formatter, "cancelled: {reason}"),
            Self::Closed { subject } => {
                write!(
                    formatter,
                    "expected an open {subject}, received a closed one"
                )
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
    use std::time::Duration;

    use super::{CODE_MAX_LENGTH, Error, ErrorCode, VendorError, jsonrpc_code_is_retryable};
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

    /// The two arms whose `expected` half is the library's own sentence about what it was reading.
    ///
    /// A host told "invalid vendor data" and nothing else knows a frame was wrong but not which
    /// field, and a timeout that will not say which call stalled is not actionable at all. Both
    /// name the library's own shape and keep the vendor's data out.
    #[test]
    fn a_protocol_refusal_and_a_timeout_name_the_shape_they_were_reading() {
        let protocol = Error::Protocol {
            expected: String::from("a turn/start result with a non-empty turn id"),
            received: String::from("Authorization: Bearer diagnostic-secret"),
        };
        let timeout = Error::Timeout {
            operation: String::from("Codex app-server thread/resume"),
            after: Duration::from_secs(30),
        };

        assert_eq!(
            protocol.to_string(),
            "expected a turn/start result with a non-empty turn id, received invalid vendor data"
        );
        assert_eq!(
            timeout.to_string(),
            "expected Codex app-server thread/resume to answer within 30s, received nothing"
        );

        for rendered in [protocol.to_string(), format!("{protocol:?}")] {
            assert!(
                !rendered.contains("diagnostic-secret"),
                "expected the vendor's own data to stay out, received {rendered}"
            );
        }
    }

    /// The two arms whose fields are the library's own, not a vendor's.
    ///
    /// A version gate and a signed-out CLI are the errors a person is meant to fix, and the
    /// instruction is the whole content: `minimum` is a harness constant and `login_hint` is a
    /// published vendor command. Only `found` is vendor stdout — a harness that cannot parse a
    /// version falls back to the line the CLI printed — so only `found` is reduced to its size.
    #[test]
    fn a_gate_and_a_signed_out_cli_still_say_what_to_do_about_them() {
        let gate = Error::VersionGate {
            found: String::from("claude-code/9.9.9 (banner-secret)"),
            minimum: String::from("2.1.211"),
        };
        assert_eq!(
            gate.to_string(),
            "expected version 2.1.211 or newer, received a vendor-reported version (33 bytes)"
        );
        assert!(
            !gate.to_string().contains("banner-secret"),
            "expected the reported banner line to stay out, received {gate}"
        );

        let signed_out = Error::AuthRequired {
            login_hint: String::from("claude auth login"),
        };
        assert!(
            signed_out.to_string().contains("`claude auth login`"),
            "expected the vendor command a user is meant to run, received {signed_out}"
        );
    }

    /// A launch failure that says only "a launcher failure" cannot be acted on.
    ///
    /// An absent executable, a permission refusal and a process tree that outlived its kill are
    /// three different fixes. The summary is the library's own — the launcher builds it from an
    /// `io::ErrorKind` or a duration, never from the path the operating system was handed — so it
    /// is named, while the program it belonged to still goes through `program_name`.
    #[test]
    fn launch_diagnostics_keep_the_cause_and_redact_the_program() {
        let absent = Error::Launch {
            program: String::from("/opt/secret-canary/claude"),
            message: String::from("a launcher failure (NotFound)"),
        };
        assert_eq!(
            absent.to_string(),
            "expected to launch claude, received a launcher failure (NotFound)"
        );

        let host_program = Error::Launch {
            program: String::from("/opt/bin/customer-secret-canary"),
            message: String::from("a launcher failure (PermissionDenied)"),
        };
        assert_eq!(
            host_program.to_string(),
            "expected to launch custom executable, received a launcher failure (PermissionDenied)"
        );
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

    /// Whether a code is safe to print is a fact about its shape, not about where it is allocated.
    ///
    /// Both codes this crate mints at run time go through `ErrorCode::new`, and
    /// `#[serde(transparent)]` hands every code back as `Cow::Owned` after a round trip. Gating
    /// `Display` on ownership made all four of those print `vendor-code`, so a resume refusal and
    /// a Claude turn failure read identically in a log.
    #[test]
    fn a_label_shaped_code_names_itself_however_it_was_built() {
        for minted in [
            "claude-error_during_execution",
            "codex-call-failed",
            "acp-request-failed",
        ] {
            assert_eq!(
                ErrorCode::new(minted).to_string(),
                minted,
                "expected a run-time code to name itself, received a stand-in"
            );
        }

        let json = serde_json::to_string(&ErrorCode::from_static("acp-request-failed"))
            .expect("a code serialises as a string");
        let round_tripped: ErrorCode =
            serde_json::from_str(&json).expect("a code deserialises from a string");
        assert_eq!(
            round_tripped.to_string(),
            "acp-request-failed",
            "expected a round trip to leave the display form alone, received a stand-in"
        );
    }

    /// The half of a minted code a vendor fills in is still vendor text.
    #[test]
    fn a_code_carrying_vendor_prose_is_reported_as_a_stand_in() {
        let payloads = [
            String::new(),
            String::from("claude-Authorization: Bearer code-secret"),
            String::from("claude-CODE_SECRET"),
            format!("claude-{}", "a".repeat(CODE_MAX_LENGTH)),
        ];

        for payload in payloads {
            let code = ErrorCode::new(payload.clone());
            assert_eq!(
                code.to_string(),
                "vendor-code",
                "expected a stand-in for {payload:?}, received the code itself"
            );
            assert_eq!(
                code.as_str(),
                payload,
                "expected the protocol field to keep the code as written, received a bounded one"
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
