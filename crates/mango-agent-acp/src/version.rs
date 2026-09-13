//! Comparing what an agent printed against the floor a profile pins.
//!
//! Dotted numbers, compared component by component, with no assumption that there are three of
//! them. ACP agents version themselves however they like — `0.14.2`, `2026.08.04`, `1.0` — and a
//! parser that insisted on semver would read Cursor's date as unparseable and gate a build it
//! could have driven.
//!
//! What a version string is *not* allowed to do is decide the answer by being unreadable. An
//! unparseable version is [`Comparison::Unknown`], which the harness reports as
//! [`GateVerdict::Unknown`](mango_external_agents::GateVerdict) — "the probe could not tell",
//! never "too old" and never "fine".

/// How a reported version compares to a pinned floor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    /// It meets the floor.
    AtLeast,
    /// It is below the floor.
    Below,
    /// One of the two could not be read as dotted numbers.
    Unknown,
}

/// Whether `found` is at least `minimum`.
///
/// Both sides are read as dotted numbers. A missing component counts as zero, so `1.2` and `1.2.0`
/// compare equal — an agent that drops a trailing zero has not shipped a different build.
///
/// # Example
///
/// ```
/// use mango_agent_acp::version::{Comparison, compare};
///
/// assert_eq!(compare("0.14.2", "0.14.0"), Comparison::AtLeast);
/// assert_eq!(compare("2026.07.31", "2026.08.04"), Comparison::Below);
/// assert_eq!(compare("1.2", "1.2.0"), Comparison::AtLeast);
/// assert_eq!(compare("nightly", "1.0.0"), Comparison::Unknown);
/// ```
#[must_use]
pub fn compare(found: &str, minimum: &str) -> Comparison {
    let Some(found) = components(found) else {
        return Comparison::Unknown;
    };
    let Some(minimum) = components(minimum) else {
        return Comparison::Unknown;
    };
    let width = found.len().max(minimum.len());
    for index in 0..width {
        let left = found.get(index).copied().unwrap_or(0);
        let right = minimum.get(index).copied().unwrap_or(0);
        if left != right {
            return if left > right {
                Comparison::AtLeast
            } else {
                Comparison::Below
            };
        }
    }
    Comparison::AtLeast
}

/// The dotted numbers in a version, or nothing when it is not made of them.
///
/// A pre-release or build suffix is cut at the first `-` or `+` so `1.2.3-beta.1` reads as `1.2.3`.
/// Ordering pre-releases below their release would need the rest of the semver grammar, and the
/// only thing this comparison is asked is whether a build is old enough to refuse.
fn components(version: &str) -> Option<Vec<u64>> {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?
        .trim();
    if core.is_empty() {
        return None;
    }
    core.split('.')
        .map(|part| part.parse::<u64>().ok())
        .collect()
}

/// The first dotted number in a line of a CLI's own `--version` output.
///
/// Agents print anything from a bare `0.14.2` to `goose 1.10.0 (rev abc123)`, so the first token
/// that reads as dotted numbers is taken and the rest of the line ignored. Returns nothing when no
/// token does, which becomes [`GateVerdict::Unknown`](mango_external_agents::GateVerdict) rather
/// than a refusal.
///
/// # Example
///
/// ```
/// use mango_agent_acp::version::parse;
///
/// assert_eq!(parse("opencode 0.14.2"), Some(String::from("0.14.2")));
/// assert_eq!(parse("cursor-agent 2026.08.04 (linux-x64)"), Some(String::from("2026.08.04")));
/// assert_eq!(parse("v1.0.3\n"), Some(String::from("1.0.3")));
/// assert_eq!(parse("built from source"), None);
/// ```
#[must_use]
pub fn parse(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .map(|token| token.trim_matches(|character: char| !character.is_ascii_alphanumeric()))
        .find_map(|token| {
            let candidate = token.trim_start_matches('v');
            // At least one dot, so a bare build number is not mistaken for a version.
            (candidate.contains('.') && components(candidate).is_some())
                .then(|| candidate.to_owned())
        })
}

#[cfg(test)]
mod tests {
    use super::{Comparison, compare, parse};

    #[test]
    fn a_longer_version_compares_against_a_shorter_floor_without_padding_lies() {
        assert_eq!(compare("1.2.3", "1.2"), Comparison::AtLeast);
        assert_eq!(compare("1.2", "1.2.3"), Comparison::Below);
        assert_eq!(compare("1.2.0", "1.2"), Comparison::AtLeast);
    }

    /// Cursor's versions are dates. A semver parser would call these unparseable and gate a build
    /// it could have driven, so the comparison is over dotted numbers rather than major/minor/patch.
    #[test]
    fn date_shaped_versions_compare_as_dotted_numbers() {
        assert_eq!(compare("2026.08.04", "2026.08.04"), Comparison::AtLeast);
        assert_eq!(compare("2026.08.05", "2026.08.04"), Comparison::AtLeast);
        assert_eq!(compare("2026.08.03", "2026.08.04"), Comparison::Below);
        assert_eq!(compare("2025.12.31", "2026.01.01"), Comparison::Below);
    }

    /// Numeric, not lexicographic: `2026.08.10` is newer than `2026.08.9`, and a string compare
    /// would say the opposite.
    #[test]
    fn components_compare_numerically_rather_than_as_text() {
        assert_eq!(compare("2026.08.10", "2026.08.9"), Comparison::AtLeast);
        assert_eq!(compare("0.9.0", "0.10.0"), Comparison::Below);
    }

    #[test]
    fn an_unreadable_version_is_unknown_rather_than_old_or_fine() {
        assert_eq!(compare("nightly", "1.0.0"), Comparison::Unknown);
        assert_eq!(compare("1.0.0", "latest"), Comparison::Unknown);
        assert_eq!(compare("", "1.0.0"), Comparison::Unknown);
        assert_eq!(compare("1.x.3", "1.0.0"), Comparison::Unknown);
    }

    #[test]
    fn a_prerelease_suffix_is_cut_rather_than_making_the_version_unreadable() {
        assert_eq!(compare("1.2.3-beta.1", "1.2.3"), Comparison::AtLeast);
        assert_eq!(compare("1.2.3+build.7", "1.2.3"), Comparison::AtLeast);
    }

    #[test]
    fn a_version_is_read_out_of_whatever_else_the_cli_printed() {
        assert_eq!(parse("0.14.2"), Some(String::from("0.14.2")));
        assert_eq!(parse("  v1.0.3  "), Some(String::from("1.0.3")));
        assert_eq!(
            parse("goose 1.10.0 (rev abc123)"),
            Some(String::from("1.10.0"))
        );
        assert_eq!(
            parse("cursor-agent version 2026.08.04"),
            Some(String::from("2026.08.04"))
        );
    }

    /// A bare integer is a build number, not a version, and treating one as `N.0.0` would let a
    /// gate pass on a string that says nothing about the build.
    #[test]
    fn output_without_a_dotted_number_parses_to_nothing() {
        assert_eq!(parse("built from source"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("build 4821"), None);
    }
}
