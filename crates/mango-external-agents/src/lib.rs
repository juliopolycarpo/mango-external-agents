#![doc = include_str!("../README.md")]

pub mod approval;
pub mod discovery;
pub mod env;
pub mod error;
pub mod event;
pub mod harness;
pub mod host;
pub mod jsonrpc;
pub mod launcher;
pub mod lifecycle;
pub mod link;
pub mod normalize;
pub mod permission;
pub mod process;
pub mod redact;
pub mod registry;
pub mod session;
pub mod stream;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod transport;
pub mod transports;

pub use discovery::{AuthMode, AuthState, Discovery, GateVerdict, Model, ReasoningEffort};
pub use env::EnvSource;
pub use error::{Error, ErrorCode, Result, VendorError};
pub use event::{
    AccountLimits, Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate,
    AgentEvent, Command, EventKind, RateLimitWindow, SessionId, ThreadUsage, TurnId, Usage,
};
pub use harness::{
    AcpProfileId, Capabilities, Capability, Harness, HarnessDescriptor, HarnessKind, VendorInfo,
};
pub use host::{
    CancelToken, ClientInfo, Clock, HostContext, HostContextBuilder, Limits, SystemClock,
};
pub use jsonrpc::{
    Client as JsonRpcClient, ClientOptions as JsonRpcOptions, JsonRpcError, PeerHandler, RequestId,
    ServerRequestOutcome,
};
pub use lifecycle::{SessionLifecycle, SessionLifecycleGuard};
pub use link::{Link, LinkReceiver, LinkSender};
pub use permission::{
    ApprovalDecision, ApprovalRouting, BrokerDecision, ConfigurationVerdict, DecisionSource,
    PermissionBroker, PermissionLevel, PermissionMatrix, PermissionOption, PermissionOptionKind,
    PermissionRequest, PermissionResponse, SupportedConfiguration, UnsupportedReason,
};
pub use process::{
    ByteSink, ByteSource, ExitStatus, LaunchSpec, LineLimits, LineStream, ManagedProcess,
    ProcessControl, ProcessLauncher, StderrTail,
};
pub use registry::HarnessRegistry;
pub use session::{
    AccountUsage, Attachment, AttachmentKind, CancelReason, CloseReason, Configuration, McpServer,
    McpTransport, NativeSession, OpenSession, Resume, ResumeMode, ReviewRequest, ReviewTarget,
    Session, SessionIds, SessionInfo, SessionPage, SessionQuery, Steer, SteerOutcome,
    SteerRejection, TurnRequest,
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
