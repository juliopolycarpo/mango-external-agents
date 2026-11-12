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

use mango_external_agents::version::{Comparison, compare, components};

use crate::pinned::MINIMUM_VERSION;

/// Pulls the version out of a `claude --version` line.
///
/// The CLI prints `2.1.270 (Claude Code)`, so its line is scanned for the first token that is a
/// version rather than parsed whole. A leading `v` is tolerated because a repackaged build may add
/// one. A wrapper can print its own version first, so only a line that identifies Claude Code is
/// considered; accepting the first version token in all of stdout can gate the wrong executable.
///
/// The token is returned as the CLI wrote it, without the `v`: `2.2.0-beta.1` stays
/// `2.2.0-beta.1`. Claude Code versions have three components, so `2.1` and `2.1.270.4` are not
/// read as one.
///
/// `None` means "not established", and callers must treat that as neither "old enough" nor "new
/// enough" — an unknown version is the one case where both answers are wrong.
///
/// # Example
///
/// ```
/// use mango_agent_claude::version::parse;
///
/// assert_eq!(parse("2.1.270 (Claude Code)").as_deref(), Some("2.1.270"));
/// assert_eq!(parse("Claude Code v2.1.270").as_deref(), Some("2.1.270"));
/// assert!(parse("command not found").is_none());
/// ```
pub fn parse(raw: &str) -> Option<String> {
    let mut lines = raw.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next()?;
    let second = lines.next();
    if second.is_none() {
        return parse_version_tokens(first);
    }
    std::iter::once(first)
        .chain(second)
        .chain(lines)
        .find_map(parse_claude_line)
}

/// Pulls the version out of a `--version` read that may have been cut short.
///
/// Only a line that identifies Claude Code counts. [`parse`] also accepts a single bare version as
/// the documented compact form, but that is only safe when the read finished: a lone line that
/// arrived before a read error may be a wrapper's own preamble, and the Claude Code line after it
/// was never read.
pub(crate) fn parse_incomplete(raw: &str) -> Option<String> {
    raw.lines().map(str::trim).find_map(parse_claude_line)
}

/// Parses a banner line that identifies Claude Code.
fn parse_claude_line(line: &str) -> Option<String> {
    if !line.to_ascii_lowercase().contains("claude code") {
        return None;
    }
    parse_version_tokens(line)
}

/// Reads the first three-component version token from one already-selected version line.
///
/// Three, because that is what Claude Code prints and what [`MINIMUM_VERSION`] is: a `1.0` or a
/// date elsewhere on the line belongs to something else. The core's dotted-number reading decides
/// what a component is, so the token this returns is one [`is_supported`] can compare.
fn parse_version_tokens(line: &str) -> Option<String> {
    line.split_whitespace().find_map(|token| {
        let token = token.strip_prefix('v').unwrap_or(token);
        let core = token.split(['-', '+']).next().unwrap_or(token);
        let is_claude_version = token.starts_with(|first: char| first.is_ascii_digit())
            && components(core).is_some_and(|numbers| numbers.len() == 3)
            && core
                .split('.')
                .all(|component| component == "0" || !component.starts_with('0'))
            && valid_semver_suffix(&token[core.len()..]);
        is_claude_version.then(|| token.to_owned())
    })
}

/// Accepts only SemVer pre-release and build metadata after Claude's three numeric components.
fn valid_semver_suffix(suffix: &str) -> bool {
    if suffix.is_empty() {
        return true;
    }

    let (prerelease, build) = if let Some(metadata) = suffix.strip_prefix('+') {
        (None, Some(metadata))
    } else if let Some(prerelease) = suffix.strip_prefix('-') {
        match prerelease.split_once('+') {
            Some((prerelease, metadata)) => (Some(prerelease), Some(metadata)),
            None => (Some(prerelease), None),
        }
    } else {
        return false;
    };
    if build.is_some_and(|metadata| !valid_semver_identifiers(metadata, false)) {
        return false;
    }
    prerelease.is_none_or(|identifiers| valid_semver_identifiers(identifiers, true))
}

/// Checks dot-separated SemVer identifiers, including the numeric pre-release zero rule.
fn valid_semver_identifiers(identifiers: &str, prerelease: bool) -> bool {
    !identifiers.is_empty()
        && identifiers.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!prerelease
                    || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                    || identifier == "0"
                    || !identifier.starts_with('0'))
        })
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
/// The comparison is the core's, over dotted numbers with any pre-release suffix dropped, so a
/// pre-release of the floor itself counts as the floor.
///
/// # Example
///
/// ```
/// use mango_agent_claude::version::{is_supported, parse};
///
/// assert!(is_supported(parse("2.1.270").as_deref()));
/// assert!(is_supported(parse("3.0.0").as_deref()));
/// assert!(!is_supported(parse("2.1.200").as_deref()));
/// assert!(!is_supported(None));
/// ```
pub fn is_supported(observed: Option<&str>) -> bool {
    observed.is_some_and(|observed| compare(observed, MINIMUM_VERSION) == Comparison::AtLeast)
}

#[cfg(test)]
mod tests {
    use super::{
        is_supported, parse, parse_incomplete, valid_semver_identifiers, valid_semver_suffix,
    };
    use crate::pinned::MINIMUM_VERSION;

    #[test]
    fn the_pinned_minimum_reads_as_a_claude_version_and_supports_itself() {
        assert_eq!(parse(MINIMUM_VERSION).as_deref(), Some(MINIMUM_VERSION));
        assert!(is_supported(Some(MINIMUM_VERSION)));
    }

    #[test]
    fn reads_the_version_out_of_the_vendors_own_banner() {
        assert_eq!(parse("2.1.226 (Claude Code)").as_deref(), Some("2.1.226"));
    }

    #[test]
    fn an_incomplete_read_trusts_only_a_line_that_names_claude_code() {
        assert_eq!(
            parse_incomplete("2.1.270 (Claude Code)").as_deref(),
            Some("2.1.270")
        );
        assert_eq!(
            parse_incomplete("wrapper 1.0.0\nClaude Code v2.1.270").as_deref(),
            Some("2.1.270")
        );
        assert_eq!(
            parse_incomplete("wrapper 1.0.0"),
            None,
            "expected a lone wrapper line not to be taken for Claude Code's version"
        );
        assert_eq!(
            parse_incomplete("2.1.270"),
            None,
            "expected the bare compact form to need a finished read"
        );
        assert_eq!(parse_incomplete(""), None);
    }

    #[test]
    fn ignores_an_unrelated_wrapper_version_before_the_claude_banner() {
        let banner = "npm 11.4.2\nClaude Code v2.1.270";
        assert_eq!(parse(banner).as_deref(), Some("2.1.270"));
    }

    #[test]
    fn reads_a_prerelease_build_rather_than_refusing_it() {
        assert_eq!(
            parse("2.2.0-beta.1 (Claude Code)").as_deref(),
            Some("2.2.0-beta.1"),
            "expected the pre-release suffix to stay on the reported token"
        );
        assert!(is_supported(Some("2.2.0-beta.1")));
    }

    /// The core comparison drops the suffix, so a pre-release of the floor is the floor. The flags
    /// this harness passes arrived with the floor's own development builds, and
    /// `CliSurface` answers before this comparison whenever help could be read.
    #[test]
    fn a_prerelease_of_the_floor_itself_counts_as_the_floor() {
        assert!(is_supported(Some("2.1.211-beta.1")));
        assert!(!is_supported(Some("2.1.210-beta.1")));
    }

    /// What `semver` refused, this refuses too: Claude Code prints three numeric components.
    #[test]
    fn a_token_is_a_claude_version_only_with_exactly_three_numeric_components() {
        for not_a_claude_version in [
            "2.1 (Claude Code)",
            "2.1.270.4 (Claude Code)",
            "2.x.270 (Claude Code)",
            "(2.1.270) Claude Code",
            "2.1.270abc (Claude Code)",
            "2.1.200-api_key=fixture-secret (Claude Code)",
            "2.01.270 (Claude Code)",
            "-2.1.270 (Claude Code)",
            "+2.1.270 (Claude Code)",
        ] {
            assert_eq!(
                parse(not_a_claude_version),
                None,
                "expected {not_a_claude_version:?} to hold no three-component version | received one"
            );
        }
        assert_eq!(
            parse("build 2026.08 2.1.270 (Claude Code)").as_deref(),
            Some("2.1.270"),
            "expected the two-component token to be skipped for the three-component one"
        );
    }

    #[test]
    fn semver_suffix_accepts_prerelease_and_build_metadata() {
        for suffix in ["", "-beta.1", "+build.7", "-beta.1+build.7"] {
            assert!(
                valid_semver_suffix(suffix),
                "expected {suffix:?} to be valid"
            );
        }
    }

    #[test]
    fn semver_suffix_rejects_empty_identifiers_and_extra_separators() {
        for suffix in ["-", "-beta.", "-beta..1", "+", "+build.", "-beta+one+two"] {
            assert!(
                !valid_semver_suffix(suffix),
                "expected {suffix:?} to be invalid"
            );
        }
    }

    #[test]
    fn semver_identifiers_apply_numeric_prerelease_zero_rules() {
        assert!(valid_semver_identifiers("alpha.0", true));
        assert!(!valid_semver_identifiers("alpha.01", true));
        assert!(valid_semver_identifiers("01", false));
        assert!(!valid_semver_identifiers("alpha_1", false));
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
        assert!(
            !is_supported(Some("nightly")),
            "expected a version that is not dotted numbers to be unsupported, never assumed fine"
        );
    }

    #[test]
    fn a_build_newer_than_the_pin_is_kept_rather_than_refused() {
        assert!(is_supported(parse("2.1.211").as_deref()));
        assert!(is_supported(parse("2.1.270").as_deref()));
        assert!(is_supported(parse("9.9.9").as_deref()));
    }

    #[test]
    fn a_build_older_than_the_pin_is_refused_because_it_lacks_a_flag_every_turn_passes() {
        assert!(!is_supported(parse("2.1.210").as_deref()));
        assert!(!is_supported(parse("2.1.200").as_deref()));
        assert!(!is_supported(parse("1.9.9").as_deref()));
    }
}
