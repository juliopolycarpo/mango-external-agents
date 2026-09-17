//! Claude's model catalog, built from the only place the CLI publishes one.
//!
//! There is no `models list` command and no handshake, so the aliases in `--model`'s own
//! description are the whole catalog: the vendor states that each resolves to the latest model of
//! that family, which is exactly the promise a picker needs. Full model names are accepted by the
//! flag too, but they are not advertised — the help offers one as an *example* of the other form,
//! and an account that lacks it gets a failed turn.
//!
//! The effort levels ride on every entry rather than on the session. `--effort` is session-scoped
//! in the CLI, so every model accepts the same list; carrying it per model is the shape
//! [`Model`] asks for and costs nothing to keep true.
//!
//! **Nothing is marked default, and that is deliberate.** The help declares no default model and
//! no default effort. A catalog that named one would put `--model` on every argv and quietly
//! override whatever default the account itself is on — turning a display concern into a behaviour
//! change. With no default, an unchosen model resolves to nothing, no flag is passed, and the
//! vendor decides.

use mango_external_agents::{Configuration, Error, Model, ReasoningEffort, Result, normalize};

use crate::cli_surface::CliSurface;

/// The catalog this build advertises, or nothing when it advertises none.
///
/// Absent rather than empty, and the two are not interchangeable: nothing leaves
/// [`Capabilities::model_catalog`](mango_external_agents::Capabilities) false and a host's picker
/// hidden, which is what a build predating the alias prose should do. An empty catalog would be a
/// picker that offers nothing.
///
/// # Example
///
/// ```
/// use mango_agent_claude::{cli_surface::CliSurface, models};
///
/// let help = "Options:\n  --model <model>   Model (e.g. 'opus', or 'sonnet')\n";
/// let catalog = models::catalog(Some(&CliSurface::parse(help))).expect("expected a catalog");
/// assert_eq!(catalog.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), vec!["opus", "sonnet"]);
/// assert!(catalog.iter().all(|model| !model.is_default));
/// ```
pub fn catalog(surface: Option<&CliSurface>) -> Option<Vec<Model>> {
    if !advertises_catalog(surface) {
        return None;
    }
    let aliases = surface?.model_aliases()?;
    let reasoning_efforts: Vec<ReasoningEffort> = surface
        .and_then(CliSurface::effort_levels)
        .unwrap_or_default()
        .iter()
        .map(|id| ReasoningEffort {
            id: id.clone(),
            // The vendor prints bare identifiers and nothing else. A prettier label invented here
            // would be this library's text wearing the vendor's name in a control that describes
            // the vendor's behaviour.
            display_name: None,
            description: None,
        })
        .collect();

    Some(
        aliases
            .iter()
            .map(|alias| Model {
                id: alias.clone(),
                reasoning_efforts: reasoning_efforts.clone(),
                ..Model::default()
            })
            .collect(),
    )
}

/// Whether this build advertises a model catalog, without building it.
///
/// For a caller that only needs [`Capabilities::model_catalog`](mango_external_agents::Capabilities)
/// and not the catalog itself: building the full `Vec<Model>` only to check `.is_some()` clones the
/// alias list and its reasoning efforts for nothing.
pub fn advertises_catalog(surface: Option<&CliSurface>) -> bool {
    surface.is_some_and(|surface| {
        surface
            .model_aliases()
            .is_some_and(|aliases| !aliases.is_empty())
    })
}

/// Refuses explicit model and effort settings that this turn cannot put on argv exactly.
///
/// The parsed surface is per installed CLI build, so effort membership is checked against it rather
/// than asking the child to start and then quietly dropping an option it did not declare.
pub fn validate_configuration(
    configuration: &Configuration,
    surface: Option<&CliSurface>,
) -> Result<()> {
    if let Some(model) = configuration.model.as_deref() {
        validate_model(model)?;
    }
    if let Some(effort) = configuration.effort.as_deref() {
        validate_effort(effort, surface.and_then(CliSurface::effort_levels))?;
    }
    Ok(())
}

/// Refuses a model identifier that cannot safely occupy Claude's `--model` value position.
pub fn validate_model(model: &str) -> Result<()> {
    if model_accepted(model) {
        return Ok(());
    }
    Err(Error::HostConfiguration {
        expected: "a non-empty Claude model identifier that can occupy a --model value",
        received: value_summary(model),
    })
}

/// Refuses an effort that this build did not explicitly advertise.
pub fn validate_effort(effort: &str, accepted: Option<&[String]>) -> Result<()> {
    if effort_accepted(Some(effort), accepted) {
        return Ok(());
    }
    Err(Error::HostConfiguration {
        expected: "a Claude --effort value this build advertises",
        received: value_summary(effort),
    })
}

/// Whether this build declared the effort level a configuration asked for.
///
/// Membership in the parsed list is the whole guard, and it is stricter than [`model_accepted`]'s
/// because it can be: the vendor publishes the complete list, so there is no need to accept a shape
/// and hope. A build that declared no levels therefore never sees the flag, which is what keeps a
/// stored per-chat effort from breaking a downgrade.
pub fn effort_accepted(effort: Option<&str>, accepted: Option<&[String]>) -> bool {
    let (Some(effort), Some(accepted)) = (effort, accepted) else {
        return false;
    };
    normalize::is_argv_value(effort) && accepted.iter().any(|level| level == effort)
}

/// A model identifier, as a shape rather than as a promise.
///
/// The model is caller-owned: a build that advertises no aliases has nothing to vet a requested
/// value against, so the value is passed straight through. An argv array stops *shell* injection,
/// not **argument** injection — a value beginning with `-` is read by the CLI's parser as a new
/// flag rather than as `--model`'s value, which is how a stored configuration could put
/// `--dangerously-skip-permissions` on the command line.
///
/// The caller refuses an unrecognised value rather than dropping it. An explicit host choice that
/// disappears from argv would start a turn under a model the host did not choose.
///
/// # Example
///
/// ```
/// use mango_agent_claude::models::model_accepted;
///
/// assert!(model_accepted("claude-opus-5"));
/// assert!(!model_accepted("--dangerously-skip-permissions"));
/// assert!(!model_accepted("opus sonnet"));
/// ```
pub fn model_accepted(model: &str) -> bool {
    if !normalize::is_argv_value(model) {
        return false;
    }
    let mut characters = model.chars();
    let starts_well = characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric());
    let rest_is_safe = characters.all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | ':' | '@' | '/' | '-')
    });
    starts_well && rest_is_safe
}

/// Summarises a rejected host value without persisting or rendering its contents.
fn value_summary(value: &str) -> String {
    format!("{} code points", value.chars().count())
}

#[cfg(test)]
mod tests {
    use super::{catalog, effort_accepted, model_accepted, validate_configuration};
    use crate::cli_surface::CliSurface;
    use mango_external_agents::{Configuration, Error};

    const HELP_2_1_227: &str = include_str!("../../../fixtures/claude/help/2.1.227.txt");
    const HELP_2_1_260: &str = include_str!("../../../fixtures/claude/help/2.1.260.txt");

    #[test]
    fn offers_the_aliases_a_build_advertises_each_with_its_effort_levels() {
        let surface = CliSurface::parse(HELP_2_1_260);
        let catalog = catalog(Some(&surface)).expect("expected a catalog");
        assert_eq!(
            catalog
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["fable", "opus", "sonnet"]
        );
        for model in &catalog {
            assert_eq!(
                model
                    .reasoning_efforts
                    .iter()
                    .map(|effort| effort.id.as_str())
                    .collect::<Vec<_>>(),
                vec!["low", "medium", "high", "xhigh", "max"]
            );
            assert!(
                model
                    .reasoning_efforts
                    .iter()
                    .all(|effort| effort.display_name.is_none()),
                "expected the vendor's bare identifiers and no invented labels"
            );
        }
    }

    #[test]
    fn names_no_default_so_an_unchosen_model_stays_the_vendors() {
        let surface = CliSurface::parse(HELP_2_1_260);
        let catalog = catalog(Some(&surface)).expect("expected a catalog");
        assert!(catalog.iter().all(|model| !model.is_default));
        assert!(
            catalog
                .iter()
                .all(|model| model.default_reasoning_effort.is_none())
        );
    }

    #[test]
    fn offers_no_catalog_at_all_on_a_build_that_advertises_no_aliases() {
        assert_eq!(catalog(Some(&CliSurface::parse(HELP_2_1_227))), None);
        assert_eq!(catalog(None), None);
    }

    #[test]
    fn passes_an_effort_level_only_when_this_build_declared_it() {
        let levels = ["low", "medium", "high"].map(String::from);
        assert!(effort_accepted(Some("high"), Some(&levels)));
        assert!(!effort_accepted(Some("ultra"), Some(&levels)));
        assert!(
            !effort_accepted(Some("high"), None),
            "expected a build that declared no levels never to see the flag"
        );
        assert!(!effort_accepted(None, Some(&levels)));
    }

    #[test]
    fn never_lets_a_model_value_become_another_flag() {
        for injected in [
            "--dangerously-skip-permissions",
            "-p",
            "",
            " opus",
            "opus --print",
            "opus\nsonnet",
        ] {
            assert!(
                !model_accepted(injected),
                "expected {injected:?} to be refused rather than passed on"
            );
        }
    }

    #[test]
    fn keeps_the_identifier_shapes_the_vendor_actually_uses() {
        for accepted in [
            "opus",
            "claude-opus-5",
            "claude-haiku-4-5-20251001",
            "anthropic.claude-opus-5-v1:0",
            "publishers/anthropic/models/claude-opus-5",
        ] {
            assert!(model_accepted(accepted));
        }
    }

    #[test]
    fn drops_an_absurdly_long_value_rather_than_passing_it_on() {
        let long = "o".repeat(129);
        assert!(!model_accepted(&long));
        let at_the_limit = "o".repeat(128);
        assert!(model_accepted(&at_the_limit));
    }

    #[test]
    fn refuses_an_explicit_unsupported_value_instead_of_dropping_it() {
        let surface = CliSurface::parse(HELP_2_1_260);
        for configuration in [
            Configuration::unknown().with_model("--dangerously-skip-permissions"),
            Configuration::unknown().with_effort("ultra"),
        ] {
            let error = validate_configuration(&configuration, Some(&surface))
                .expect_err("expected the explicit value to be refused");
            assert!(
                matches!(error, Error::HostConfiguration { ref received, .. } if !received.contains("dangerously") && !received.contains("ultra")),
                "received {error:?}"
            );
        }
    }
}
