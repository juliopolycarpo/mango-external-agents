#![doc = include_str!("../README.md")]

mod approval_events;
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

/// The protocol family every harness in this crate speaks.
///
/// This crate publishes no single harness id, and deliberately: one ACP harness drives one agent,
/// so the id always names the profile too — `acp:cursor`, `acp:opencode`. A bare `acp` is not a
/// value any registry lookup would find, which is why it is not offered as one.
///
/// # Example
///
/// ```
/// use mango_external_agents::HarnessId;
///
/// assert_eq!(mango_agent_acp::protocol_family().as_str(), "agent-client-protocol");
/// let cursor = mango_agent_acp::builtin_profile("cursor").expect("a built-in profile");
/// assert_eq!(HarnessId::acp(&cursor.id).as_str(), "acp:cursor");
/// ```
pub fn protocol_family() -> mango_external_agents::ProtocolFamily {
    mango_external_agents::ProtocolFamily::acp()
}

#[cfg(test)]
mod tests {
    use super::{builtin_profiles, protocol_family};
    use mango_external_agents::HarnessId;

    /// The bare crate name is not a registerable id here, and a test that asserted it was would be
    /// pinning a lookup that finds nothing.
    #[test]
    fn every_built_in_registers_under_its_own_profile_inside_one_family() {
        for harness in super::AcpHarness::builtins() {
            let identity = &harness.descriptor().identity;
            let profile = identity
                .profile
                .as_ref()
                .expect("expected every ACP harness to name its profile");

            assert_eq!(identity.protocol, protocol_family());
            assert_eq!(identity.id, HarnessId::acp(profile));
            assert_eq!(
                identity.id.acp_profile().as_ref(),
                Some(profile),
                "expected the profile to read back off the id it was composed into"
            );
        }
        assert_eq!(
            super::AcpHarness::builtins().len(),
            builtin_profiles().len(),
            "expected one harness per built-in profile"
        );
    }
}
