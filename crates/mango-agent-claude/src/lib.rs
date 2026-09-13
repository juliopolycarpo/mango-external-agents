#![doc = include_str!("../README.md")]

pub mod argv;
pub mod auth;
pub mod cli_surface;
pub mod commands;
pub mod harness;
pub mod help;
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

/// The harness kind this crate implements, as the core registry names it.
pub const HARNESS_KIND: &str = "claude";

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
