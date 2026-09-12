#![doc = include_str!("../README.md")]

pub mod error;
pub mod harness;
pub mod session;
pub mod transport;

pub use error::{Error, ErrorCode, Result, VendorError};
pub use harness::{
    AcpProfileId, Capabilities, Capability, HarnessDescriptor, HarnessKind, VendorInfo,
};
pub use session::{CancelReason, CloseReason};
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
