//! Reading and comparing the version of whatever `claude` the host resolved.
//!
//! The version gates the **flags**, not the protocol. The record vocabulary on stdout is additive
//! and the reducer ignores what it does not recognise, so what a version comparison is actually
//! for here is knowing whether an argument this harness is about to pass exists —
//! `--forward-subagent-text` above all, which a pre-2.1.211 build rejects outright.
//!
//! Which is why [`CliSurface`](crate::cli_surface::CliSurface) is the better gate and this is the
//! fallback: asking whether the flag is *there* answers the real question, and the number is only
//! a proxy for it.

use semver::Version;

/// The oldest build this harness drives, parsed.
///
/// Built rather than parsed from [`MINIMUM_VERSION`] so there is no panic path in a library; a test
/// asserts the two agree.
pub fn minimum() -> Version {
    Version::new(2, 1, 211)
}

/// Pulls the version out of a `claude --version` line.
///
/// The CLI prints `2.1.270 (Claude Code)`, so the line is scanned for the first token that is a
/// version rather than parsed whole. A leading `v` is tolerated because a repackaged build may add
/// one.
///
/// `None` means "not established", and callers must treat that as neither "old enough" nor "new
/// enough" — an unknown version is the one case where both answers are wrong.
///
/// # Example
///
/// ```
/// use mango_agent_claude::version::parse;
///
/// assert_eq!(parse("2.1.270 (Claude Code)").map(|v| v.to_string()).as_deref(), Some("2.1.270"));
/// assert_eq!(parse("Claude Code v2.1.270").map(|v| v.to_string()).as_deref(), Some("2.1.270"));
/// assert!(parse("command not found").is_none());
/// ```
pub fn parse(raw: &str) -> Option<Version> {
    raw.split_whitespace()
        .find_map(|token| Version::parse(token.strip_prefix('v').unwrap_or(token)).ok())
}

/// Whether an observed version may drive this harness.
///
/// Newer than the pin is allowed: the stream vocabulary is additive, the reducer tolerates unknown
/// record types, and refusing a Claude the user upgraded themselves would turn a drift warning
/// into an outage. Older is refused, because the argv this harness builds names flags that binary
/// does not have.
///
/// A version that could not be read is refused too, but only by callers that have nothing else to
/// go on: [`CliSurface`](crate::cli_surface::CliSurface) answers first when it could be read.
///
/// # Example
///
/// ```
/// use mango_agent_claude::version::{is_supported, parse};
///
/// assert!(is_supported(parse("2.1.270").as_ref()));
/// assert!(is_supported(parse("3.0.0").as_ref()));
/// assert!(!is_supported(parse("2.1.200").as_ref()));
/// assert!(!is_supported(None));
/// ```
pub fn is_supported(observed: Option<&Version>) -> bool {
    observed.is_some_and(|observed| *observed >= minimum())
}

#[cfg(test)]
mod tests {
    use super::{is_supported, minimum, parse};
    use crate::pinned::MINIMUM_VERSION;

    #[test]
    fn the_parsed_minimum_is_the_one_the_pin_names() {
        assert_eq!(minimum().to_string(), MINIMUM_VERSION);
    }

    #[test]
    fn reads_the_version_out_of_the_vendors_own_banner() {
        assert_eq!(
            parse("2.1.226 (Claude Code)").expect("expected a version"),
            semver::Version::new(2, 1, 226)
        );
    }

    #[test]
    fn reads_a_prerelease_build_rather_than_refusing_it() {
        let parsed = parse("2.2.0-beta.1 (Claude Code)").expect("expected a version");
        assert_eq!(parsed.major, 2);
        assert!(!parsed.pre.is_empty());
    }

    #[test]
    fn an_unreadable_banner_is_not_established_rather_than_old_or_new() {
        for unreadable in ["", "Claude Code", "bash: claude: command not found", "2.1"] {
            assert!(
                parse(unreadable).is_none(),
                "expected {unreadable:?} to read as unestablished"
            );
        }
        assert!(!is_supported(None));
    }

    #[test]
    fn a_build_newer_than_the_pin_is_kept_rather_than_refused() {
        assert!(is_supported(parse("2.1.211").as_ref()));
        assert!(is_supported(parse("2.1.270").as_ref()));
        assert!(is_supported(parse("9.9.9").as_ref()));
    }

    #[test]
    fn a_build_older_than_the_pin_is_refused_because_it_lacks_a_flag_every_turn_passes() {
        assert!(!is_supported(parse("2.1.210").as_ref()));
        assert!(!is_supported(parse("2.1.200").as_ref()));
        assert!(!is_supported(parse("1.9.9").as_ref()));
    }
}
