#![doc = include_str!("../README.md")]

mod client;
pub mod content;
pub mod error;
pub mod harness;
pub mod permission;
pub mod profile;
pub mod reducer;
pub mod session;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod transport;
pub mod version;

pub use harness::AcpHarness;
pub use profile::{AcpProfile, SessionModeIds, builtin_profile, builtin_profiles};
pub use session::AcpSession;

/// The harness kind this crate implements, as the core registry names it.
///
/// A whole [`HarnessKind`](mango_external_agents::HarnessKind) also names the profile — `acp:cursor`,
/// `acp:opencode` — because one ACP harness drives one agent.
pub const HARNESS_KIND: &str = "acp";

#[cfg(test)]
mod tests {
    use super::HARNESS_KIND;

    #[test]
    fn harness_kind_matches_the_crate_name() {
        let expected = env!("CARGO_PKG_NAME").trim_start_matches("mango-agent-");
        assert_eq!(
            HARNESS_KIND, expected,
            "expected {expected:?}, received {HARNESS_KIND:?}"
        );
    }
}
