#![doc = include_str!("../README.md")]

pub mod env;
pub mod error;
pub mod event;
pub mod harness;
pub mod host;
pub mod normalize;
pub mod process;
pub mod redact;
pub mod session;
pub mod stream;
pub mod transport;

pub use env::EnvSource;
pub use error::{Error, ErrorCode, Result, VendorError};
pub use event::{
    AccountLimits, Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate,
    AgentEvent, Command, EventKind, RateLimitWindow, SessionId, ThreadUsage, TurnId, Usage,
};
pub use harness::{
    AcpProfileId, Capabilities, Capability, HarnessDescriptor, HarnessKind, VendorInfo,
};
pub use host::{
    CancelToken, ClientInfo, Clock, HostContext, HostContextBuilder, Limits, SystemClock,
};
pub use process::{
    ByteSink, ByteSource, ExitStatus, LaunchSpec, LineLimits, LineStream, ManagedProcess,
    ProcessControl, ProcessLauncher, StderrTail,
};
pub use session::{CancelReason, CloseReason};
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
