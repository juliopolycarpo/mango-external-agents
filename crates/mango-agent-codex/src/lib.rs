#![doc = include_str!("../README.md")]

pub mod activity;
pub mod approvals;
mod configuration;
pub mod discovery;
pub mod harness;
mod mcp;
pub mod permissions;
pub mod protocol;
pub mod rate_limits;
pub mod reducer;
pub mod session;
pub mod turn_reducer;

pub use harness::CodexHarness;
pub use protocol::schema::{MINIMUM_CODEX_VERSION, PIN};
pub use session::CodexSession;

/// The id this crate's harness registers under.
///
/// A function rather than a `&str` constant, so a caller holds the type the registry keys on
/// instead of a string it has to re-validate — and so a caller cannot look one up with a value no
/// harness could have registered.
///
/// # Example
///
/// ```
/// assert_eq!(mango_agent_codex::harness_id().as_str(), "codex");
/// ```
pub fn harness_id() -> mango_external_agents::HarnessId {
    mango_external_agents::HarnessId::codex()
}

#[cfg(test)]
mod tests {
    use super::harness_id;
    use mango_external_agents::Harness;

    #[test]
    fn the_harness_registers_under_the_id_this_crate_publishes() {
        let expected = env!("CARGO_PKG_NAME").trim_start_matches("mango-agent-");
        assert_eq!(harness_id().as_str(), expected);
        assert_eq!(
            super::CodexHarness::new().descriptor().id(),
            &harness_id(),
            "expected the published id to be the one the descriptor registers under"
        );
    }
}
