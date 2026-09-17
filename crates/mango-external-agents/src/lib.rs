#![doc = include_str!("../README.md")]

pub mod approval;
pub mod configuration;
pub mod content;
pub mod discovery;
pub mod env;
pub mod error;
pub mod event;
pub mod extension;
pub mod harness;
pub mod host;
pub mod identity;
pub mod interaction;
pub mod jsonrpc;
pub mod launcher;
pub mod lifecycle;
pub mod link;
pub mod normalize;
pub mod operation;
pub mod permission;
pub mod process;
pub mod redact;
pub mod registry;
pub mod session;
pub mod state;
pub mod stream;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod transport;
pub mod transports;

pub use configuration::{
    Configuration, ConfigurationCatalog, ConfigurationCategory, ConfigurationChange,
    ConfigurationOption, ConfigurationOptionId, ConfigurationOptionValue, ConfigurationOutcome,
    ConfigurationPatch, ConfigurationSource, ConfigurationState, ConfigurationValue,
    ConfigurationValueType, RejectedSetting, Rollback, SettingRejection,
};
pub use content::{ActivityContent, FileChange, FileChangeKind, PlanStep, PlanStepStatus};
pub use discovery::{
    AuthMode, AuthState, Discovery, DiscoveryReceipt, GateVerdict, Model, ReasoningEffort,
};
pub use env::EnvSource;
pub use error::{Error, ErrorCode, Result, VendorError};
pub use event::{
    AccountLimits, Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate,
    AgentEvent, Command, EventKind, RateLimitWindow, SessionId, ThreadUsage, TurnId, Usage,
};
pub use extension::{ExtensionValue, Extensions};
pub use harness::{
    Capabilities, Capability, CapabilityCeiling, DiscoveredCapabilities, Harness,
    HarnessDescriptor, SessionCapabilities, VendorInfo,
};
pub use host::{
    CancelToken, ClientInfo, Clock, HostContext, HostContextBuilder, Limits, SystemClock,
};
pub use identity::{HarnessId, HarnessIdentity, ProfileId, ProtocolFamily};
pub use interaction::{
    Answer, AnswerValue, Interaction, InteractionId, InteractionKind, InteractionStatus, Question,
    QuestionForm, QuestionId, QuestionOption, QuestionOptionId, QuestionOutcome, QuestionRequest,
    QuestionResponse, UnsupportedQuestion,
};
pub use jsonrpc::{
    Client as JsonRpcClient, ClientOptions as JsonRpcOptions, JsonRpcError, PeerHandler, RequestId,
    ServerRequestOutcome,
};
pub use lifecycle::{SessionLifecycle, SessionLifecycleGuard};
pub use link::{Link, LinkReceiver, LinkSender};
pub use operation::{AttemptId, Dispatch, OperationRef};
pub use permission::{
    ApprovalDecision, ApprovalRouting, BrokerDecision, ConfigurationVerdict, DecisionSource,
    PermissionBroker, PermissionEffect, PermissionLevel, PermissionMatrix, PermissionOption,
    PermissionRequest, PermissionResponse, PermissionRisk, PermissionScope, SupportedConfiguration,
    UnsupportedReason,
};
pub use process::{
    ByteSink, ByteSource, ExitStatus, LaunchSpec, LineLimits, LineStream, ManagedProcess,
    ProcessControl, ProcessLauncher, StderrTail,
};
pub use registry::HarnessRegistry;
pub use session::{
    resume_fallback_reason,
    AccountUsage, Attachment, AttachmentKind, CancelReason, CloseReason, McpServer, McpTransport,
    NativeSession, OpenSession, Resume, ResumeMode, ReviewRequest, ReviewTarget, Session,
    SessionIds, SessionPage, SessionQuery, Steer, SteerOutcome, SteerRejection, TurnRequest,
};
pub use state::{
    SessionRevision, SessionSnapshot, SessionState, SessionStatus, SessionSubscription,
    TransportSelection,
};
pub use stream::{EventSink, ReviewStream, TurnStream};
pub use transport::{AcpSpec, ExecutablePath, StdioSpec, TransportKind, TransportSpec, WsSpec};

/// Semantic version of this crate, kept in lockstep with every workspace crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn version_is_a_semver_triple() {
        let parts: Vec<&str> = VERSION.split('.').collect();
        assert_eq!(
            parts.len(),
            3,
            "expected major.minor.patch, received {VERSION:?}"
        );
        for part in parts {
            assert!(
                part.parse::<u32>().is_ok(),
                "expected a number, received {part:?}"
            );
        }
    }
}
