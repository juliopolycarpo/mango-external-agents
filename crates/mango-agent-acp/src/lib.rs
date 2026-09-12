//! Agent Client Protocol harness for `mango-external-agents`.
//!
//! Drives any Agent Client Protocol agent over the official `agent-client-protocol` crate through its documented programmatic surface only. The behaviour lands with the harness itself;
//! this crate currently declares its harness kind.

/// The harness kind this crate implements, as the core registry names it.
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
