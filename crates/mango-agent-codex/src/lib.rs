//! OpenAI Codex harness for `mango-external-agents`.
//!
//! Drives OpenAI Codex (`codex app-server` JSON-RPC) through its documented programmatic surface only. The behaviour lands in later
//! plans; this crate currently declares its harness kind.

/// The harness kind this crate implements, as the core registry names it.
pub const HARNESS_KIND: &str = "codex";

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
