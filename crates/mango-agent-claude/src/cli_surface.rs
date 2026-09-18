//! What `claude --help` says this build offers, and whether that is enough.
//!
//! Claude has no handshake. Codex answers `initialize` and an ACP agent negotiates a version, but
//! a `claude --print` turn is a one-shot process whose first feedback about an argument it does
//! not recognise is a non-zero exit after the user already pressed send. `--help` is the only place
//! the CLI describes its own surface before anything is spawned in anger, so it is what this
//! harness probes.
//!
//! This is deliberately a better gate than the version number. [`MINIMUM_VERSION`](crate::pinned::MINIMUM_VERSION) records 2.1.211
//! because that is where `--forward-subagent-text` arrived, but the version is a proxy for the flag
//! and the flag is the thing that matters: a repackaged build, a vendor that backports, or a pin
//! that went stale all make the number disagree with the binary. Reading the surface asks the
//! question directly, so a below-pin install that has everything this harness passes keeps
//! working, and an at-pin install that lost a flag is caught before a turn is attempted.
//!
//! Two different failures come out of here, and they are not interchangeable:
//!
//! - A **missing flag** is fatal for the whole harness. Every flag listed below is on every turn's
//!   argv, so there is no configuration that avoids it.
//! - A **missing permission mode** is fatal only for the configurations that need that mode, which
//!   is why [`permissions`](crate::permissions) narrows the matrix with it instead of refusing the
//!   harness.
//!
//! Unknown *extra* flags and modes are ignored on purpose. Claude gains options constantly;
//! treating an unrecognised one as drift would break this harness on every vendor release.

use std::collections::BTreeSet;

use crate::help::{self, Option_};

/// The option whose choice list is Claude's permission vocabulary.
const PERMISSION_MODE_FLAG: &str = "--permission-mode";

/// The two options whose vocabulary is stated in prose rather than as choices.
const MODEL_FLAG: &str = "--model";
const EFFORT_FLAG: &str = "--effort";

/// Who answers a permission prompt. Declared from 2.1.259; absent before it.
const PERMISSION_PROMPTS_FLAG: &str = "--permission-prompts";

/// The MCP servers a turn passes through. Declared for as long as the CLI has had MCP.
const MCP_CONFIG_FLAG: &str = "--mcp-config";

/// Long flags every turn puts on the wire.
///
/// Every one of these is unconditional except `--model`, which is passed only when a model was
/// chosen — it is still required here, because losing it silently removes model selection rather
/// than failing where it can be seen.
///
/// Short aliases are not listed: `-p` and `--print` are the same option and this harness passes the
/// long form, so matching the long form is matching what is actually sent.
pub const REQUIRED_FLAGS: &[&str] = &[
    "--print",
    "--input-format",
    "--output-format",
    "--verbose",
    "--include-partial-messages",
    "--forward-subagent-text",
    PERMISSION_MODE_FLAG,
    "--resume",
    "--session-id",
    MODEL_FLAG,
];

/// The parsed surface, reduced to the things this harness reads off it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CliSurface {
    flags: BTreeSet<String>,
    permission_modes: BTreeSet<String>,
    model_aliases: Option<Vec<String>>,
    effort_levels: Option<Vec<String>>,
}

impl CliSurface {
    /// Reads `claude --help` into the surface this harness depends on.
    ///
    /// Three vocabularies, three shapes the vendor happens to print them in, and none of the
    /// shape-reading here: `(choices: …)` for the permission modes, a bare list for the effort
    /// levels, a first `(e.g. …)` group for the model aliases. Which flag carries which vocabulary
    /// is this harness's knowledge; how each shape is read is [`crate::help`]'s.
    pub fn parse(help_text: &str) -> Self {
        let options = help::declared_options(help_text);
        let flags = options
            .iter()
            .flat_map(|option| option.flags.iter().cloned())
            .collect();
        let permission_modes = option_block(&options, PERMISSION_MODE_FLAG)
            .and_then(help::choice_list)
            .unwrap_or_default()
            .into_iter()
            .collect();
        Self {
            flags,
            permission_modes,
            model_aliases: option_block(&options, MODEL_FLAG).and_then(help::quoted_examples),
            effort_levels: option_block(&options, EFFORT_FLAG).and_then(help::bare_choice_list),
        }
    }

    /// Which required flags this build does not offer, in declaration order.
    ///
    /// Empty means every argument this harness passes exists — the answer that lets a below-pin
    /// binary keep working.
    pub fn missing_required_flags(&self) -> Vec<&'static str> {
        REQUIRED_FLAGS
            .iter()
            .copied()
            .filter(|flag| !self.flags.contains(*flag))
            .collect()
    }

    /// Whether a parsed surface is worth trusting at all.
    ///
    /// A help text that yielded no permission modes and none of the required flags is far more
    /// likely to be a probe that failed — a spawn that produced nothing, a CLI that printed to
    /// stderr, a wrapper that swallowed the output — than a build with no options. Treating that as
    /// "everything is missing" would make a flaky spawn look like vendor drift and refuse a working
    /// install, so callers fall back to the version comparison instead.
    pub fn is_usable(&self) -> bool {
        !self.permission_modes.is_empty() || self.missing_required_flags().is_empty()
    }

    /// The modes this build declared, or nothing when it declared none.
    ///
    /// An empty choice list is **unproven**, not "this build accepts no mode", and the two have to
    /// stay distinguishable: a build that offers every required flag and whose `(choices: …)` list
    /// moved or wrapped differently parses as usable with no modes, and passing that empty set
    /// through as authoritative would refuse every configuration on a binary that can run them all.
    /// Narrowing belongs to a probe that saw the vocabulary, never to one that failed to read it.
    pub fn accepted_modes(&self) -> Option<&BTreeSet<String>> {
        (!self.permission_modes.is_empty()).then_some(&self.permission_modes)
    }

    /// The model aliases `--model`'s description advertises, or nothing when it advertises none.
    pub fn model_aliases(&self) -> Option<&[String]> {
        self.model_aliases.as_deref()
    }

    /// `--effort`'s levels, from the same prose, with the same absent-is-not-empty rule.
    pub fn effort_levels(&self) -> Option<&[String]> {
        self.effort_levels.as_deref()
    }

    /// Whether this build lets the caller say who answers permission prompts.
    ///
    /// `false` for an unreadable surface, which is the **opposite** default from
    /// [`accepted_modes`](Self::accepted_modes), and deliberately so — the two answer different
    /// questions. A probe that failed may not *narrow* what the matrix offers, so that one fails
    /// open; it also may not *promise* an option exists, so this one fails closed. Passing an
    /// undeclared flag is a startup failure on every turn, which is the one outcome worth being
    /// pessimistic to avoid.
    pub fn declares_permission_prompts(&self) -> bool {
        self.flags.contains(PERMISSION_PROMPTS_FLAG)
    }

    /// Whether this build accepts MCP servers the host configured.
    ///
    /// Fails closed for the same reason [`declares_permission_prompts`](Self::declares_permission_prompts)
    /// does: the flag goes on the argv only when the binary said it exists.
    pub fn declares_mcp_config(&self) -> bool {
        self.flags.contains(MCP_CONFIG_FLAG)
    }
}

fn option_block<'a>(options: &'a [Option_], flag: &str) -> Option<&'a str> {
    help::option_for(options, flag).map(|option| option.block.as_str())
}

#[cfg(test)]
mod tests {
    use super::CliSurface;

    const HELP_2_1_227: &str = include_str!("../../../fixtures/claude/help/2.1.227.txt");
    /// `claude --help` in full, from the build installed when this harness was written.
    const HELP_2_1_270: &str = include_str!("../../../fixtures/claude/help/2.1.270.txt");
    const HELP_2_1_260: &str = include_str!("../../../fixtures/claude/help/2.1.260.txt");

    #[test]
    fn every_flag_every_turn_passes_is_present_on_both_captured_builds() {
        for (label, help) in [("2.1.227", HELP_2_1_227), ("2.1.260", HELP_2_1_260)] {
            let surface = CliSurface::parse(help);
            assert!(
                surface.is_usable(),
                "expected {label} to parse into a usable surface"
            );
            assert_eq!(
                surface.missing_required_flags(),
                Vec::<&str>::new(),
                "expected {label} to offer every required flag"
            );
        }
    }

    /// The drift check: the whole surface of a real build, not an excerpt.
    ///
    /// An excerpt can only prove the parser reads what somebody trimmed for it. This one fails the
    /// day the vendor renames or removes something every turn depends on, which is the earliest a
    /// maintainer could hear about it.
    #[test]
    fn the_whole_surface_of_the_installed_build_parses() {
        let surface = CliSurface::parse(HELP_2_1_270);
        assert!(surface.is_usable());
        assert_eq!(surface.missing_required_flags(), Vec::<&str>::new());
        assert!(
            surface.declares_permission_prompts(),
            "expected 2.1.270 to declare --permission-prompts"
        );
        assert!(
            surface.declares_mcp_config(),
            "expected 2.1.270 to declare --mcp-config"
        );
        assert_eq!(
            surface.model_aliases(),
            Some(["fable", "opus", "sonnet"].map(String::from).as_slice())
        );
        assert!(
            surface
                .accepted_modes()
                .is_some_and(|modes| modes.contains("plan") && modes.contains("manual")),
            "received {:?}",
            surface.accepted_modes()
        );
    }

    #[test]
    fn reads_the_permission_vocabulary_the_vendor_wrapped_across_three_lines() {
        let modes = CliSurface::parse(HELP_2_1_260)
            .accepted_modes()
            .expect("expected a mode vocabulary")
            .clone();
        for mode in [
            "acceptEdits",
            "auto",
            "bypassPermissions",
            "manual",
            "dontAsk",
            "plan",
        ] {
            assert!(modes.contains(mode), "expected {mode:?} in {modes:?}");
        }
    }

    #[test]
    fn reports_a_build_that_states_no_aliases_as_absent_rather_than_empty() {
        assert_eq!(
            CliSurface::parse(HELP_2_1_227).model_aliases(),
            None,
            "expected the bare 2.1.227 --model line to advertise no catalog"
        );
        assert_eq!(
            CliSurface::parse(HELP_2_1_260).model_aliases(),
            Some(["fable", "opus", "sonnet"].map(String::from).as_slice())
        );
    }

    #[test]
    fn reports_a_build_without_the_effort_option_as_absent() {
        assert_eq!(CliSurface::parse(HELP_2_1_227).effort_levels(), None);
        assert_eq!(
            CliSurface::parse(HELP_2_1_260).effort_levels(),
            Some(
                ["low", "medium", "high", "xhigh", "max"]
                    .map(String::from)
                    .as_slice()
            )
        );
    }

    #[test]
    fn only_a_build_that_declares_the_flag_is_told_who_answers_prompts() {
        assert!(!CliSurface::parse(HELP_2_1_227).declares_permission_prompts());
        assert!(CliSurface::parse(HELP_2_1_260).declares_permission_prompts());
    }

    #[test]
    fn an_unreadable_surface_narrows_nothing_and_promises_nothing() {
        let surface = CliSurface::parse("claude: command not found");
        assert!(!surface.is_usable());
        assert_eq!(
            surface.accepted_modes(),
            None,
            "expected an unread vocabulary to narrow nothing"
        );
        assert!(
            !surface.declares_permission_prompts(),
            "expected an unread surface to promise no flag"
        );
    }

    /// The captured contract, read by something rather than only committed.
    ///
    /// `fixtures/claude/contract/cli-surface.json` is the whole option and mode vocabulary of the
    /// build this harness was written against, and until it is read by a test it is a document
    /// that can drift out of agreement with the code beside it without anybody hearing. Read as a
    /// floor, not as an equality: the vendor adds options constantly, and every `REQUIRED_FLAGS`
    /// entry and every [`CliMode`](crate::permissions::CliMode) spelling has to still be one the
    /// captured build offers.
    #[test]
    fn the_captured_contract_still_names_everything_this_harness_passes() {
        use crate::permissions::CliMode;

        const CONTRACT: &str = include_str!("../../../fixtures/claude/contract/cli-surface.json");

        let contract: serde_json::Value =
            serde_json::from_str(CONTRACT).expect("expected the captured contract to be JSON");
        let listed = |key: &str| -> Vec<String> {
            contract[key]
                .as_array()
                .unwrap_or_else(|| panic!("expected {key} to be an array"))
                .iter()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect()
        };

        let flags = listed("flags");
        for required in super::REQUIRED_FLAGS {
            assert!(
                flags.iter().any(|flag| flag == required),
                "expected the captured surface to declare {required:?}, received {flags:?}"
            );
        }

        let modes = listed("permissionModes");
        for mode in [
            CliMode::Manual,
            CliMode::AcceptEdits,
            CliMode::Plan,
            CliMode::Auto,
            CliMode::DontAsk,
            CliMode::BypassPermissions,
        ] {
            assert!(
                modes.iter().any(|listed| listed == mode.as_arg()),
                "expected the captured surface to offer {:?}, received {modes:?}",
                mode.as_arg()
            );
        }
    }
}
