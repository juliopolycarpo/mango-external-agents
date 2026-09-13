//! Reading a commander CLI's own `--help` into the options it declares.
//!
//! Generic on purpose: nothing here knows which flags Claude Code has, only how the library it
//! uses prints them. What a particular vocabulary *means* belongs to
//! [`cli_surface`](crate::cli_surface).
//!
//! Every scan here is a hand-written pass rather than a regular expression, and not only because
//! the workspace has no regex dependency. Every unanchored spelling of "the text between
//! `(choices:` and the next `)`" is quadratic on an input that repeats `(choices:` without ever
//! closing it: the engine runs the inner scan to the end of the string once per prefix. This parses
//! a subprocess's stdout, so "the vendor would never print that" is not a property it gets to rely
//! on.

/// The marker commander prints before an option's machine-readable choice list.
const CHOICES_PREFIX: &str = "(choices:";

/// The marker a vendor prints before its own examples, when it documents a vocabulary in prose.
const EXAMPLE_PREFIX: &str = "e.g.";

/// Every long flag the text **declares**, plus the block each one's description occupies.
///
/// Commander indents every option two spaces and wraps descriptions much further, so an option
/// line is the only thing that starts at exactly two spaces followed by a dash. Matching that
/// rather than every `--token` in the text is the whole correctness argument: the description of
/// `--forward-subagent-text` names `--output-format=stream-json`, and a parser that scanned the
/// full text would report flags the binary does not offer — which means it could never notice one
/// going away.
pub fn declared_options(help: &str) -> Vec<Option_> {
    let lines: Vec<&str> = help.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let declaration = option_line(line)?;
            Some(Option_ {
                flags: long_flags(before_description(declaration)),
                block: block_at(&lines, index),
            })
        })
        .collect()
}

/// One declared option: the long flags on its own line, and its whole wrapped description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Option_ {
    /// Every long flag the declaration names, each including its leading `--`.
    pub flags: Vec<String>,
    /// The declaring line plus the continuation lines beneath it, joined by single spaces.
    pub block: String,
}

/// The option that declares `flag`, or nothing when the text never declares it.
///
/// Crucially **not** "the text mentions it": a flag named inside a neighbour's description is not
/// declared, and answering with the neighbour's block would invent a surface the binary lacks.
pub fn option_for<'a>(options: &'a [Option_], flag: &str) -> Option<&'a Option_> {
    options
        .iter()
        .find(|option| option.flags.iter().any(|declared| declared == flag))
}

/// An option's `(choices: "a", "b")` list, as commander prints it.
pub fn choice_list(block: &str) -> Option<Vec<String>> {
    let start = block.find(CHOICES_PREFIX)? + CHOICES_PREFIX.len();
    let end = start + block[start..].find(')')?;
    Some(quoted(&block[start..end], '"'))
}

/// An option's vocabulary when it is printed as a bare list rather than as commander's own
/// `(choices: …)`.
///
/// `(low, medium, high, xhigh, max)` is a real thing a commander CLI prints for an option
/// commander does not know the choices of, and it is invisible to [`choice_list`]. The first group
/// whose contents are a comma-separated run of bare identifiers wins, which rejects
/// `(only works with --print …)` and `(choices: "host", "none")` by the same rule rather than by
/// special-casing either.
///
/// `None` means the option states no such list — never an empty list, so a caller can tell "this
/// build says nothing" from "this build accepts nothing".
pub fn bare_choice_list(block: &str) -> Option<Vec<String>> {
    paren_groups(block).into_iter().find_map(bare_identifiers)
}

/// The quoted values in an option's **first** `(e.g. …)` group.
///
/// Bounded to one group, and that bound is the whole correctness argument:
///
/// - Prose around these lists contains apostrophes ("a model's full name"). A scan for quoted
///   tokens across the block opens a quote on the apostrophe and closes it on the next one,
///   inventing a value out of the words in between.
/// - A description with two example groups is describing two *different* things. Claude's
///   `--model` names its aliases in the first and one full model name in the second; merging them
///   advertises a specific model as though it were an alias the vendor promises to resolve.
pub fn quoted_examples(block: &str) -> Option<Vec<String>> {
    let group = paren_groups(block)
        .into_iter()
        .find(|group| group.trim_start().starts_with(EXAMPLE_PREFIX))?;
    let examples: Vec<String> = quoted(group, '\'')
        .into_iter()
        .filter(|value| is_example_identifier(value))
        .collect();
    (!examples.is_empty()).then_some(examples)
}

/// The declaration part of an option line: everything before the first run of two or more spaces.
fn before_description(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut run = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if byte.is_ascii_whitespace() {
            run += 1;
            if run == 2 {
                return &line[..index - 1];
            }
        } else {
            run = 0;
        }
    }
    line
}

/// A line that declares an option, without its indent, or nothing for any other line.
fn option_line(line: &str) -> Option<&str> {
    let declaration = line.strip_prefix("  ")?;
    let mut characters = declaration.chars();
    if characters.next()? != '-' {
        return None;
    }
    characters
        .next()
        .filter(|next| !next.is_whitespace())
        .map(|_| declaration)
}

/// One option's own line plus the wrapped continuation beneath it.
///
/// A choice list long enough to wrap — Claude's `--permission-mode` is — spans three lines in the
/// middle of a description, so reading only the declaring line would find no choices at all.
fn block_at(lines: &[&str], start: usize) -> String {
    let mut block = vec![lines[start].trim()];
    for line in &lines[start + 1..] {
        // A blank line ends the Options section, and with it this option.
        if option_line(line).is_some() || line.trim().is_empty() {
            break;
        }
        block.push(line.trim());
    }
    block.join(" ")
}

/// Every `--name` in a declaration, matching commander's own `--[a-zA-Z][\w-]*`.
fn long_flags(declaration: &str) -> Vec<String> {
    let bytes = declaration.as_bytes();
    let mut flags = Vec::new();
    let mut index = 0;
    while index + 2 < bytes.len() {
        if bytes[index] != b'-'
            || bytes[index + 1] != b'-'
            || !bytes[index + 2].is_ascii_alphabetic()
        {
            index += 1;
            continue;
        }
        let start = index;
        index += 2;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric()
                || bytes[index] == b'-'
                || bytes[index] == b'_')
        {
            index += 1;
        }
        flags.push(declaration[start..index].to_owned());
    }
    flags
}

/// Every `( … )` group in a block, in order, without its delimiters.
fn paren_groups(block: &str) -> Vec<&str> {
    let mut groups = Vec::new();
    let mut cursor = 0;
    while let Some(open) = block[cursor..].find('(') {
        let open = cursor + open + 1;
        let Some(close) = block[open..].find(')') else {
            break;
        };
        groups.push(&block[open..open + close]);
        cursor = open + close + 1;
    }
    groups
}

/// Every value between a matched pair of `delimiter`, in order.
fn quoted(text: &str, delimiter: char) -> Vec<String> {
    let mut values = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find(delimiter) {
        let after = &rest[open + delimiter.len_utf8()..];
        let Some(close) = after.find(delimiter) else {
            break;
        };
        if close > 0 {
            values.push(after[..close].to_owned());
        }
        rest = &after[close + delimiter.len_utf8()..];
    }
    values
}

/// The group's contents as a comma-separated identifier list, or nothing.
fn bare_identifiers(group: &str) -> Option<Vec<String>> {
    let parts: Vec<&str> = group.split(',').map(str::trim).collect();
    if parts.len() < 2 || !parts.iter().all(|part| is_bare_identifier(part)) {
        return None;
    }
    Some(parts.into_iter().map(str::to_owned).collect())
}

/// A single bare lowercase identifier: `[a-z][a-z0-9-]*`.
fn is_bare_identifier(value: &str) -> bool {
    is_identifier(value, &[])
}

/// An example value: `[a-z][a-z0-9.-]*`, which is what keeps a prose apostrophe from becoming one.
fn is_example_identifier(value: &str) -> bool {
    is_identifier(value, &['.'])
}

/// A lowercase identifier starting `[a-z]`, continuing `[a-z0-9-]` plus whatever `extra` allows.
fn is_identifier(value: &str, extra: &[char]) -> bool {
    let mut characters = value.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && characters.all(|next| {
            next.is_ascii_lowercase()
                || next.is_ascii_digit()
                || next == '-'
                || extra.contains(&next)
        })
}

#[cfg(test)]
mod tests {
    use super::{bare_choice_list, choice_list, declared_options, option_for, quoted_examples};

    const HELP: &str = "\
Usage: claude [options]

Options:
  --add-dir <directories...>            Additional directories to allow tool
                                        access to
  --allowedTools, --allowed-tools <tools...>
      Comma or space-separated list of tool names to allow (e.g. \"Bash(git *)
      Edit\")
  --effort <level>                      Effort level for the current session
                                        (low, medium, high, xhigh, max)
  --forward-subagent-text               Forward subagent text and thinking
                                        blocks as assistant/user messages with
                                        parent_tool_use_id set (only works with
                                        --print and --output-format=stream-json)
  --model <model>                       Model for the current session. Provide
                                        an alias for the latest model (e.g.
                                        'fable', 'opus', or 'sonnet') or a
                                        model's full name (e.g.
                                        'claude-fable-5').
  --permission-mode <mode>              Permission mode to use for the session
                                        (choices: \"acceptEdits\", \"auto\",
                                        \"bypassPermissions\", \"manual\",
                                        \"dontAsk\", \"plan\")
  -p, --print                           Print response and exit (useful for
                                        pipes).

Commands:
  auth <subcommand>                     Manage authentication
";

    fn option(flag: &str) -> super::Option_ {
        let options = declared_options(HELP);
        option_for(&options, flag)
            .unwrap_or_else(|| panic!("expected {flag} to be declared"))
            .clone()
    }

    #[test]
    fn reads_only_the_flags_an_option_line_declares() {
        let options = declared_options(HELP);
        let declared: Vec<&str> = options
            .iter()
            .flat_map(|option| option.flags.iter().map(String::as_str))
            .collect();
        assert!(declared.contains(&"--forward-subagent-text"));
        assert!(declared.contains(&"--print"));
        assert!(
            declared.contains(&"--allowedTools") && declared.contains(&"--allowed-tools"),
            "expected both spellings on one declaration, received {declared:?}"
        );
        assert!(
            !declared.contains(&"--output-format"),
            "expected a flag named inside a neighbour's description not to count as declared"
        );
    }

    #[test]
    fn an_undeclared_flag_answers_with_nothing_rather_than_a_neighbours_block() {
        let options = declared_options(HELP);
        assert_eq!(option_for(&options, "--output-format"), None);
        assert_eq!(option_for(&options, "--bare"), None);
    }

    #[test]
    fn reads_a_choice_list_commander_wrapped_across_three_lines() {
        let choices = choice_list(&option("--permission-mode").block).expect("expected choices");
        assert_eq!(
            choices,
            vec![
                "acceptEdits",
                "auto",
                "bypassPermissions",
                "manual",
                "dontAsk",
                "plan"
            ]
        );
    }

    #[test]
    fn reads_a_bare_parenthesised_list() {
        let levels = bare_choice_list(&option("--effort").block).expect("expected levels");
        assert_eq!(levels, vec!["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn does_not_mistake_a_prose_group_for_a_bare_choice_list() {
        assert_eq!(
            bare_choice_list(&option("--forward-subagent-text").block),
            None,
            "expected \"(only works with …)\" to be rejected as a vocabulary"
        );
        assert_eq!(
            bare_choice_list(&option("--permission-mode").block),
            None,
            "expected a quoted choice list to be rejected as a bare one"
        );
    }

    #[test]
    fn reads_the_aliases_the_vendor_advertises() {
        let aliases = quoted_examples(&option("--model").block).expect("expected aliases");
        assert_eq!(aliases, vec!["fable", "opus", "sonnet"]);
    }

    #[test]
    fn offers_the_aliases_only_not_the_full_name_example_beside_them() {
        let aliases = quoted_examples(&option("--model").block).expect("expected aliases");
        assert!(
            !aliases.contains(&String::from("claude-fable-5")),
            "expected the second example group to stay out, received {aliases:?}"
        );
    }

    #[test]
    fn does_not_mistake_the_apostrophe_in_models_for_a_quoted_alias() {
        let aliases = quoted_examples(&option("--model").block).expect("expected aliases");
        assert!(
            aliases.iter().all(|alias| !alias.contains(' ')),
            "expected no prose to be captured between two apostrophes, received {aliases:?}"
        );
    }

    #[test]
    fn an_option_with_no_examples_advertises_none() {
        assert_eq!(quoted_examples(&option("--effort").block), None);
        assert_eq!(quoted_examples(&option("--print").block), None);
    }

    #[test]
    fn an_unclosed_group_ends_the_scan_rather_than_running_away() {
        assert_eq!(choice_list("(choices: \"a\""), None);
        assert_eq!(bare_choice_list("(low, medium"), None);
        assert_eq!(quoted_examples("(e.g. 'opus'"), None);
    }
}
