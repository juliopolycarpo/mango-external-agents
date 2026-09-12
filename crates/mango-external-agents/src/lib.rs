#![doc = include_str!("../README.md")]

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
