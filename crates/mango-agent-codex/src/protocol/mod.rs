//! The slice of the `codex app-server` wire contract this harness speaks.
//!
//! Hand-written rather than vendored, and the measurement is in `docs/harness-codex.md`: the
//! transitive closure of `codex-app-server-protocol` and `codex-protocol` is 32 in-repo crates and
//! 177,655 lines of Rust behind 107 third-party crates — among them `native-tls`, which
//! `deny.toml` bans outright, plus `sqlx`, `tree-sitter`, `opentelemetry`, `landlock` and
//! `seccompiler`. Copying that tree in to obtain a few dozen structs would import a TLS stack this
//! workspace refuses and a telemetry stack it promises not to have.
//!
//! What is vendored instead is the vendor's own description of the wire: `vendor/PIN` names the
//! tag, and `vendor/schema/` holds the JSON Schema bundle `codex app-server
//! generate-json-schema` produced at that tag. [`crate::protocol::schema`]'s tests deserialise
//! this module's types from the schema's own examples, so a field the vendor renames fails a test
//! here rather than a turn on a user's machine.
//!
//! Every type is the shape the server actually writes, not the shape it could write: fields this
//! harness never reads are left out, and `serde` ignores them on the way in. Unknown enum variants
//! are kept as data rather than refused, because an app-server newer than the pin is a normal
//! thing to meet and a turn is not worth ending over a variant nobody has to understand.

pub mod approvals;
pub mod items;
pub mod notifications;
pub mod requests;
pub mod schema;

pub use approvals::{
    ApprovalDecisionValue, CommandExecutionApprovalParams, FileChangeApprovalParams, ServerRequest,
};
pub use items::{CommandExecutionStatus, ItemStatus, ThreadItem};
pub use notifications::{
    AgentMessageDelta, ErrorNotification, ItemNotification, Notification, RateLimitSnapshot,
    RateLimitWindow, ReasoningDelta, ServerRequestResolved, ThreadStarted, ThreadTokenUsage,
    TokenUsageBreakdown, TurnNotification, TurnTokenUsage,
};
pub use requests::{
    Account, AccountReadResponse, AskForApproval, ClientInfo, InitializeParams, InitializeResponse,
    Model, ModelListParams, ModelListResponse, RateLimitsReadResponse, ReasoningEffortOption,
    ReviewStartParams, ReviewStartResponse, ReviewTarget, SandboxMode, ThreadListParams,
    ThreadListResponse, ThreadStartParams, ThreadStartResponse, ThreadSummary, TurnHandle,
    TurnStartParams, TurnStatus, TurnSteerParams, TurnSteerResponse, UserInput,
};

/// The methods this harness calls, exactly as the app-server spells them.
///
/// Named rather than written inline so the drift check in `schema` has one list to compare
/// against the pinned bundle, and so a typo is a missing constant rather than a `-32601` at run
/// time.
pub mod method {
    /// The one call that must precede every other on a connection.
    pub const INITIALIZE: &str = "initialize";
    /// The acknowledgement that finishes the handshake.
    pub const INITIALIZED: &str = "initialized";
    /// Opens a new conversation.
    pub const THREAD_START: &str = "thread/start";
    /// Continues an existing one.
    pub const THREAD_RESUME: &str = "thread/resume";
    /// Lists the conversations this machine already has.
    pub const THREAD_LIST: &str = "thread/list";
    /// Starts a turn.
    pub const TURN_START: &str = "turn/start";
    /// Adds to the turn that is already running.
    pub const TURN_STEER: &str = "turn/steer";
    /// Stops the turn that is running.
    pub const TURN_INTERRUPT: &str = "turn/interrupt";
    /// Starts the vendor's own review.
    pub const REVIEW_START: &str = "review/start";
    /// The models this build will accept.
    pub const MODEL_LIST: &str = "model/list";
    /// Who is signed in, without reading what they signed in with.
    pub const ACCOUNT_READ: &str = "account/read";
    /// The account's plan quota.
    pub const ACCOUNT_RATE_LIMITS_READ: &str = "account/rateLimits/read";
}

/// The peer as a person would name it, in errors and log lines.
pub const PEER_NAME: &str = "Codex app-server";

#[cfg(test)]
mod tests {
    use super::method;

    /// Every method this harness calls sits under a namespace the app-server README documents.
    #[test]
    fn every_method_is_namespaced_the_way_the_app_server_spells_it() {
        let methods = [
            method::INITIALIZE,
            method::INITIALIZED,
            method::THREAD_START,
            method::THREAD_RESUME,
            method::THREAD_LIST,
            method::TURN_START,
            method::TURN_STEER,
            method::TURN_INTERRUPT,
            method::REVIEW_START,
            method::MODEL_LIST,
            method::ACCOUNT_READ,
            method::ACCOUNT_RATE_LIMITS_READ,
        ];
        for name in methods {
            assert!(
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '/'),
                "expected an app-server method name, received {name:?}"
            );
        }
    }
}
