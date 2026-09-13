#![doc = include_str!("../README.md")]

pub mod protocol;

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
