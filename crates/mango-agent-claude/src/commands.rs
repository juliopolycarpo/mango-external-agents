//! Which of a run's announced `/name` commands this harness is willing to publish.
//!
//! A statement about **provenance**, not a guess about interactivity. Claude Code announces one
//! flat `slash_commands` list that mixes user commands read off disk, plugin and MCP commands, the
//! skills a build exposes, and the CLI's own terminal builtins. A host driving `claude -p` has no
//! terminal, so publishing a builtin like `/doctor` offers a command that does nothing where it
//! would be typed.
//!
//! Newer builds say which names those are, in `terminal_slash_commands`. Older ones say nothing,
//! and **absence counts as unreadable**: reading it as "this run excluded nothing" would publish
//! exactly those names on every build that predates the field.

use std::collections::BTreeSet;

use mango_external_agents::Command;

use crate::protocol::InitRecord;

/// The prefix the CLI namespaces its own private plumbing with.
///
/// `__remote-workflow` is not something a person types, and it is announced on every run.
const PRIVATE_PREFIX: &str = "__";

/// The prefix the CLI namespaces an MCP server's own commands with.
///
/// A protocol convention rather than a name, which is why it cannot collide with a builtin.
pub(crate) const MCP_PREFIX: &str = "mcp__";

/// The commands one run can expand, in the order the CLI announced them.
///
/// `None` means this run announced nothing publishable, which is **not** the same as an empty
/// catalog: the last announcement wins wherever it lands, so announcing `[]` on a build that
/// cannot state its own exclusions would erase a real catalog an earlier run published. Returning
/// nothing leaves whatever the host already knew in place.
///
/// # Example
///
/// ```
/// use mango_agent_claude::{commands, protocol::StreamRecord};
///
/// let record = StreamRecord::parse(
///     r#"{"slash_commands":["clear","deploy"],"terminal_slash_commands":["clear"]}"#,
/// )
/// .expect("expected a record");
/// let catalog = commands::catalog(&record.init()).expect("expected a catalog");
/// assert_eq!(catalog.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["deploy"]);
/// ```
pub fn catalog(init: &InitRecord<'_>) -> Option<Vec<Command>> {
    let announced = init.slash_commands()?;
    let usable: Vec<&str> = announced
        .into_iter()
        .filter(|name| !name.is_empty() && !name.starts_with(PRIVATE_PREFIX))
        .collect();

    // The run's own statement of what needs the terminal this harness does not give it. Present
    // means the list is authoritative, including when it excludes nothing.
    if let Some(terminal_only) = init.terminal_slash_commands() {
        let excluded: BTreeSet<&str> = terminal_only.into_iter().collect();
        return Some(named(
            usable.into_iter().filter(|name| !excluded.contains(name)),
        ));
    }

    let skills: BTreeSet<&str> = init.skills().into_iter().collect();
    let plugins: BTreeSet<&str> = init.plugin_names().into_iter().collect();
    let attributable = named(
        usable
            .into_iter()
            .filter(|name| origin_is_known(name, &skills, &plugins)),
    );
    // Nothing to attribute is not an empty catalog — see the doc comment.
    (!attributable.is_empty()).then_some(attributable)
}

fn named<'a>(names: impl Iterator<Item = &'a str>) -> Vec<Command> {
    names
        .map(|name| Command {
            name: name.to_owned(),
            description: None,
        })
        .collect()
}

/// Whether this record states where `name` came from, rather than leaving it to be guessed.
///
/// Three shapes, each a statement about where the CLI read the name from: a skill (`skills`
/// repeats every skill name also present in `slash_commands`), a plugin's own command
/// (`plugin:command`, namespaced with the `:` Claude Code uses, matched against the plugin names
/// the same record announced), or an MCP server's (`mcp__*`). All three are content the vendor
/// read off disk or off a server; none is a terminal builtin, which is what makes them publishable
/// without the exclusion list that would normally vouch for that.
///
/// A user skill named the same as a terminal-only builtin still publishes, because the record says
/// it is a skill.
fn origin_is_known(name: &str, skills: &BTreeSet<&str>, plugins: &BTreeSet<&str>) -> bool {
    if skills.contains(name) || name.starts_with(MCP_PREFIX) {
        return true;
    }
    match name.split_once(':') {
        Some((plugin, _)) if !plugin.is_empty() => plugins.contains(plugin),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::catalog;
    use crate::protocol::StreamRecord;

    fn names(line: &str) -> Option<Vec<String>> {
        let record = StreamRecord::parse(line).expect("expected a parseable record");
        catalog(&record.init())
            .map(|commands| commands.into_iter().map(|command| command.name).collect())
    }

    #[test]
    fn withholds_the_names_the_run_marked_terminal_only() {
        let published = names(
            r#"{"slash_commands":["doctor","color","deploy"],"terminal_slash_commands":["doctor","color"]}"#,
        );
        assert_eq!(published, Some(vec![String::from("deploy")]));
    }

    #[test]
    fn says_nothing_when_the_terminal_only_list_is_unreadable() {
        let published =
            names(r#"{"slash_commands":["doctor","deploy"],"terminal_slash_commands":"doctor"}"#);
        assert_eq!(
            published, None,
            "expected an unreadable exclusion list to withhold, received {published:?}"
        );
    }

    #[test]
    fn says_nothing_when_the_run_predates_the_terminal_only_list() {
        assert_eq!(names(r#"{"slash_commands":["doctor","clear"]}"#), None);
    }

    #[test]
    fn falls_back_to_the_names_whose_provenance_the_record_states() {
        let published = names(
            r#"{"slash_commands":["doctor","dataviz","code-review:code-review","mcp__design__design"],
                "skills":["dataviz"],
                "plugins":[{"name":"code-review"}]}"#,
        );
        assert_eq!(
            published,
            Some(vec![
                String::from("dataviz"),
                String::from("code-review:code-review"),
                String::from("mcp__design__design"),
            ])
        );
    }

    #[test]
    fn does_not_treat_a_colon_in_an_unrelated_name_as_a_plugin_prefix() {
        let published = names(
            r#"{"slash_commands":["notaplugin:thing","dataviz"],"skills":["dataviz"],"plugins":[{"name":"code-review"}]}"#,
        );
        assert_eq!(published, Some(vec![String::from("dataviz")]));
    }

    #[test]
    fn stays_silent_rather_than_announcing_an_empty_provenance_subset() {
        assert_eq!(
            names(r#"{"slash_commands":["clear","compact"],"skills":[],"plugins":[]}"#),
            None
        );
    }

    #[test]
    fn still_announces_an_empty_catalog_when_the_run_stated_its_own_exclusions() {
        let published =
            names(r#"{"slash_commands":["doctor"],"terminal_slash_commands":["doctor"]}"#);
        assert_eq!(
            published,
            Some(Vec::new()),
            "expected an authoritative exclusion list to be honoured even when it empties the catalog"
        );
    }

    #[test]
    fn withholds_the_clis_private_plumbing() {
        let published = names(
            r#"{"slash_commands":["__remote-workflow","deploy"],"terminal_slash_commands":[]}"#,
        );
        assert_eq!(published, Some(vec![String::from("deploy")]));
    }

    #[test]
    fn says_nothing_when_the_run_announced_no_list_at_all() {
        assert_eq!(names(r#"{"type":"system","subtype":"init"}"#), None);
    }

    #[test]
    fn a_skill_named_like_a_builtin_still_publishes_because_the_record_says_it_is_a_skill() {
        let published = names(r#"{"slash_commands":["doctor"],"skills":["doctor"]}"#);
        assert_eq!(published, Some(vec![String::from("doctor")]));
    }
}
