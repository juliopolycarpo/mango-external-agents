#![doc = include_str!("../README.md")]

pub mod argv;
pub mod auth;
pub mod cli_surface;
pub mod commands;
pub mod harness;
pub mod help;
pub mod mcp;
pub mod models;
pub mod permissions;
pub mod pinned;
pub mod probe;
pub mod protocol;
pub mod reducer;
pub mod session;
pub mod version;

pub use harness::{ClaudeHarness, harness};
pub use session::ClaudeSession;

/// The id this crate's harness registers under.
///
/// A function rather than a `&str` constant, so a caller holds the type the registry keys on
/// instead of a string it has to re-validate — and so a caller cannot look one up with a value no
/// harness could have registered.
///
/// # Example
///
/// ```
/// assert_eq!(mango_agent_claude::harness_id().as_str(), "claude");
/// ```
pub fn harness_id() -> mango_external_agents::HarnessId {
    mango_external_agents::HarnessId::claude()
}

#[cfg(test)]
mod tests {
    use super::harness_id;

    #[test]
    fn the_harness_registers_under_the_id_this_crate_publishes() {
        let expected = env!("CARGO_PKG_NAME").trim_start_matches("mango-agent-");
        assert_eq!(harness_id().as_str(), expected);
        assert_eq!(
            super::harness().descriptor().id(),
            &harness_id(),
            "expected the published id to be the one the descriptor registers under"
        );
    }
}
