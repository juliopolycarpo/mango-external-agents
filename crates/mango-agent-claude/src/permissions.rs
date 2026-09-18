//! The core's two product axes, collapsed onto Claude's single `--permission-mode`.
//!
//! Claude is the vendor where the axes are *not* independent. Codex keeps "what may run" and "who
//! answers" as separate fields; Claude has one flag whose members mix both, so some pairs are
//! simply unrepresentable. Those come back unsupported with a reason rather than being quietly
//! rounded to the nearest mode — a picker that silently swapped `default` + auto-review for
//! `acceptEdits` would offer the same label over a different risk profile.
//!
//! | Mode                | What runs without asking                            |
//! | ------------------- | --------------------------------------------------- |
//! | `manual`            | Reads only; everything else asks                    |
//! | `acceptEdits`       | Reads, file edits, common filesystem commands       |
//! | `plan`              | Reads, plus classifier-approved commands under auto |
//! | `auto`              | Everything, with a classifier reviewing each action |
//! | `dontAsk`           | Only pre-approved tools                             |
//! | `bypassPermissions` | Everything                                          |
//!
//! `auto` and `dontAsk` point in opposite directions and are never substituted for one another.
//! `auto` is the genuine auto-review analogue; `dontAsk` is for locked-down CI and would silently
//! *narrow* what a user asked to widen, which is why nothing here ever selects it.
//!
//! ## Why this is resolved per account rather than declared
//!
//! `auto` is not a property of the binary. It needs a qualifying plan tier, an administrator can
//! remove it with `disableAutoMode` in managed settings — which makes the CLI **reject
//! `--permission-mode auto` at startup** — and the CLI ignores `defaultMode: "auto"` coming from
//! project settings. A static table would be wrong on some machines and right on others, and the
//! failure would surface as a turn that died at startup rather than as a choice nobody was offered.

use std::collections::BTreeSet;

use mango_external_agents::{
    ApprovalRouting, Configuration, ConfigurationVerdict, Error, PermissionLevel, PermissionMatrix,
    Result, UnsupportedReason,
};
use serde_json::Value;

use crate::auth::AccountKind;

/// The `--permission-mode` values this harness may pass.
///
/// [`CliMode::Manual`] is the command-line spelling of the mode the vendor's own configuration
/// calls `default`. The two vocabularies are kept apart rather than unified: this harness passes
/// `manual` on the argv and reports `default` as the vendor id, which is the value a host persists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CliMode {
    /// Ask about everything that changes the machine.
    Manual,
    /// Accept file edits and common filesystem commands without asking.
    AcceptEdits,
    /// Read and plan, changing nothing.
    Plan,
    /// Act, with the vendor's own classifier reviewing each action.
    Auto,
    /// Run only pre-approved tools, asking nobody.
    DontAsk,
    /// Act without asking.
    BypassPermissions,
}

impl CliMode {
    /// The value as `--permission-mode` takes it.
    pub const fn as_arg(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::Auto => "auto",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
        }
    }

    /// The value the vendor's own configuration files persist.
    ///
    /// Identical to [`as_arg`](Self::as_arg) everywhere except [`CliMode::Manual`], whose canonical
    /// spelling is `default`.
    pub const fn canonical(self) -> &'static str {
        match self {
            Self::Manual => "default",
            other => other.as_arg(),
        }
    }
}

/// Everything discovery learned that narrows the matrix.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModeAvailability {
    /// How the account is paid for. Absent when `auth status` established nothing.
    pub account_kind: Option<AccountKind>,
    /// True when managed settings set `disableAutoMode` to `"disable"`.
    pub auto_mode_disabled_by_policy: bool,
    /// `--permission-mode`'s own choice list, when the CLI surface could be read.
    ///
    /// Absent narrows nothing: a discovery that could not run `--help` must not conclude that the
    /// binary offers no modes. Present means a mode outside the set is refused per pair, which
    /// turns "the vendor renamed a mode" into one unsupported row with a reason instead of a turn
    /// that dies at startup.
    pub accepted_modes: Option<BTreeSet<String>>,
}

impl ModeAvailability {
    /// Whether this build's `--permission-mode` will take a mode.
    ///
    /// An unread vocabulary accepts everything. The probe is an improvement on trusting the pin,
    /// not a precondition for running at all, so a machine where it could not run behaves exactly
    /// as it did before the probe existed.
    pub fn accepts(&self, mode: CliMode) -> bool {
        self.accepted_modes
            .as_ref()
            .is_none_or(|accepted| accepted.contains(mode.as_arg()))
    }

    /// Why `auto` may not be passed at all, or nothing when it may.
    ///
    /// Fails closed on an unknown account. An unavailable `auto` that this harness passes anyway is
    /// a turn that dies at startup with a message the user cannot act on; an `auto` marked
    /// unsupported when it would in fact have worked is one unsupported row with a reason. Only the
    /// second is recoverable by the person reading it.
    pub fn auto_refusal(&self) -> Option<UnsupportedReason> {
        if self.auto_mode_disabled_by_policy {
            // The one cell an administrator switched off, and the cell is unattended by
            // construction — the routing is what makes it so.
            return Some(UnsupportedReason::UnattendedNotPermitted);
        }
        match self.account_kind {
            Some(AccountKind::Subscription) => None,
            Some(_) => Some(UnsupportedReason::RequiresAccountUpgrade),
            // The only case no named reason describes: nothing is wrong with the account, the
            // probe simply could not establish one, so no claim about `auto` is safe either way.
            None => Some(UnsupportedReason::Other(String::from(
                "claude auth status established no account, so whether this plan offers auto mode is unknown",
            ))),
        }
    }
}

/// The `--permission-mode` value one (level, routing) pair resolves to.
///
/// `None` means the pair is unrepresentable, which is the honest answer for every routing choice
/// other than the one Claude's chosen mode already implies.
///
/// # Example
///
/// ```
/// use mango_agent_claude::permissions::{CliMode, ModeAvailability, permission_mode};
/// use mango_external_agents::{ApprovalRouting, PermissionLevel};
///
/// let availability = ModeAvailability::default();
/// assert_eq!(
///     permission_mode(PermissionLevel::ReadOnly, ApprovalRouting::User, &availability),
///     Some(CliMode::Plan)
/// );
/// assert_eq!(
///     permission_mode(PermissionLevel::ReadOnly, ApprovalRouting::AutoReview, &availability),
///     None
/// );
/// ```
pub fn permission_mode(
    level: PermissionLevel,
    routing: ApprovalRouting,
    availability: &ModeAvailability,
) -> Option<CliMode> {
    if routing == ApprovalRouting::AutoReview {
        // Only `default` + auto-review has a mode behind it. `plan` already means read-only, so
        // there is nothing for a classifier to review inside it, and `bypassPermissions` has
        // already permitted everything.
        if level != PermissionLevel::Default {
            return None;
        }
        return availability
            .auto_refusal()
            .is_none()
            .then_some(CliMode::Auto);
    }
    Some(match level {
        PermissionLevel::ReadOnly => CliMode::Plan,
        PermissionLevel::Default => CliMode::Manual,
        // The supported spelling. `--dangerously-skip-permissions` and
        // `--allow-dangerously-skip-permissions` are never passed: they are the interactive escape
        // hatches, and this is the documented flag value.
        PermissionLevel::FullAccess => CliMode::BypassPermissions,
    })
}

/// Resolves Claude's inseparable permission-mode pair when the host supplied both axes.
///
/// Unlike Codex, Claude exposes one `--permission-mode` flag rather than independent sandbox and
/// reviewer settings. It therefore rejects a partial pair instead of silently selecting a mode
/// for the omitted axis, while an omitted pair leaves all permission flags out of argv.
pub(crate) fn configuration_mode(
    configuration: &Configuration,
    availability: &ModeAvailability,
) -> Result<Option<CliMode>> {
    let (Some(level), Some(routing)) = (configuration.level, configuration.routing) else {
        if configuration.level.is_none() && configuration.routing.is_none() {
            return Ok(None);
        }
        return Err(Error::HostConfiguration {
            expected: "both permission level and approval routing for Claude's one permission-mode flag, or neither",
            received: format!(
                "level {:?} with routing {:?}",
                configuration.level, configuration.routing
            ),
        });
    };
    permission_mode(level, routing, availability)
        .filter(|mode| availability.accepts(*mode))
        .map(Some)
        .ok_or_else(|| Error::HostConfiguration {
            expected: "a permission level and routing this Claude build and account can run",
            received: format!("{level:?} with {routing:?}"),
        })
}

/// The whole two-by-three matrix, minus whatever this account and this machine forbid.
pub fn matrix(availability: &ModeAvailability) -> PermissionMatrix {
    PermissionMatrix::build(|level, routing| {
        let Some(mode) = permission_mode(level, routing, availability) else {
            return refusal_for(level, availability);
        };
        let vendor_id = Some(String::from(mode.canonical()));
        if availability.accepts(mode) {
            return ConfigurationVerdict::Supported { vendor_id };
        }
        // A mode the account allows but this build does not list. Refusing the pair is the whole
        // point: the alternative is passing a mode the CLI rejects at startup, which reaches the
        // user as a turn that died for no stated reason rather than as a choice never offered.
        ConfigurationVerdict::Unsupported {
            reason: UnsupportedReason::RequiresNewerVersion,
            vendor_id,
        }
    })
}

/// Why a pair with no mode behind it cannot be selected.
///
/// Called only for `AutoReview`: every `User` routing resolves to a mode in [`permission_mode`],
/// so this only ever reasons about auto-review's own cell per level.
fn refusal_for(level: PermissionLevel, availability: &ModeAvailability) -> ConfigurationVerdict {
    match level {
        // Read-only is a whole session mode in Claude, so nothing acts inside it to review; full
        // access has already permitted everything, so nothing is left to review either.
        PermissionLevel::ReadOnly | PermissionLevel::FullAccess => {
            ConfigurationVerdict::unsupported(UnsupportedReason::NotOfferedByVendor)
        }
        PermissionLevel::Default => ConfigurationVerdict::Unsupported {
            reason: availability
                .auto_refusal()
                .unwrap_or(UnsupportedReason::NotOfferedByVendor),
            vendor_id: Some(String::from(CliMode::Auto.canonical())),
        },
    }
}

/// Reads `disableAutoMode` out of a managed-settings document.
///
/// Only the literal `"disable"` counts. The setting is administrator-authored JSON this library
/// does not own, so anything else — a boolean, a typo, a missing file — leaves `auto` decided by
/// the account instead of by a guess about what an unrecognised value meant.
///
/// # Example
///
/// ```
/// use mango_agent_claude::permissions::auto_mode_disabled;
/// use serde_json::json;
///
/// assert!(auto_mode_disabled(&json!({"disableAutoMode": "disable"})));
/// assert!(!auto_mode_disabled(&json!({"disableAutoMode": true})));
/// assert!(!auto_mode_disabled(&json!({})));
/// ```
pub fn auto_mode_disabled(managed_settings: &Value) -> bool {
    managed_settings.get("disableAutoMode") == Some(&Value::String(String::from("disable")))
}

#[cfg(test)]
mod tests {
    use super::{
        CliMode, ModeAvailability, auto_mode_disabled, configuration_mode, matrix, permission_mode,
    };
    use crate::auth::AccountKind;
    use mango_external_agents::{
        ApprovalRouting, Configuration, Error, PermissionLevel, PermissionMatrix,
        SupportedConfiguration, UnsupportedReason,
    };
    use serde_json::json;

    fn subscription() -> ModeAvailability {
        ModeAvailability {
            account_kind: Some(AccountKind::Subscription),
            ..ModeAvailability::default()
        }
    }

    fn cell(
        availability: &ModeAvailability,
        level: PermissionLevel,
        routing: ApprovalRouting,
    ) -> SupportedConfiguration {
        matrix(availability)
            .cell(level, routing)
            .expect("expected every pair to be described")
            .clone()
    }

    #[test]
    fn uses_plan_for_read_only_and_bypass_permissions_for_full_access() {
        let availability = subscription();
        assert_eq!(
            permission_mode(
                PermissionLevel::ReadOnly,
                ApprovalRouting::User,
                &availability
            ),
            Some(CliMode::Plan)
        );
        assert_eq!(
            permission_mode(
                PermissionLevel::FullAccess,
                ApprovalRouting::User,
                &availability
            ),
            Some(CliMode::BypassPermissions)
        );
    }

    #[test]
    fn only_claude_rejects_a_partial_permission_pair_because_its_flag_is_inseparable() {
        let availability = subscription();
        assert_eq!(
            configuration_mode(&Configuration::default(), &availability)
                .expect("expected omitted permissions to be accepted"),
            None
        );
        let explicit = Configuration::unknown()
            .with_level(PermissionLevel::Default)
            .with_routing(ApprovalRouting::User);
        assert_eq!(
            configuration_mode(&explicit, &availability)
                .expect("expected the complete pair to resolve"),
            Some(CliMode::Manual)
        );
        let partial = Configuration::unknown().with_level(PermissionLevel::Default);
        let error = configuration_mode(&partial, &availability)
            .expect_err("expected Claude to reject its partial mode pair");
        assert!(
            matches!(error, Error::HostConfiguration { expected, .. }
                if expected.contains("Claude's one permission-mode flag")),
            "expected the Claude-specific refusal, received {error:?}"
        );
    }

    #[test]
    fn passes_manual_on_the_command_line_while_default_is_what_is_persisted() {
        assert_eq!(CliMode::Manual.as_arg(), "manual");
        assert_eq!(CliMode::Manual.canonical(), "default");
        let supported = cell(
            &subscription(),
            PermissionLevel::Default,
            ApprovalRouting::User,
        );
        assert!(supported.supported);
        assert_eq!(supported.vendor_id.as_deref(), Some("default"));
    }

    #[test]
    fn never_substitutes_dont_ask_for_the_mode_a_user_asked_to_widen() {
        for level in PermissionLevel::ALL {
            for routing in ApprovalRouting::ALL {
                assert_ne!(
                    permission_mode(level, routing, &subscription()),
                    Some(CliMode::DontAsk),
                    "expected {level:?}/{routing:?} never to resolve to dontAsk"
                );
            }
        }
    }

    #[test]
    fn auto_review_has_a_mode_behind_it_only_at_the_default_level() {
        let availability = subscription();
        assert_eq!(
            permission_mode(
                PermissionLevel::Default,
                ApprovalRouting::AutoReview,
                &availability
            ),
            Some(CliMode::Auto)
        );
        for level in [PermissionLevel::ReadOnly, PermissionLevel::FullAccess] {
            assert_eq!(
                permission_mode(level, ApprovalRouting::AutoReview, &availability),
                None,
                "expected {level:?} to have nothing for a classifier to review"
            );
            let refused = cell(&availability, level, ApprovalRouting::AutoReview);
            assert_eq!(
                refused.unsupported_reason,
                Some(UnsupportedReason::NotOfferedByVendor)
            );
        }
    }

    #[test]
    fn an_administrator_who_disabled_auto_mode_takes_that_cell_away() {
        let availability = ModeAvailability {
            auto_mode_disabled_by_policy: true,
            ..subscription()
        };
        assert_eq!(
            permission_mode(
                PermissionLevel::Default,
                ApprovalRouting::AutoReview,
                &availability
            ),
            None
        );
        let refused = cell(
            &availability,
            PermissionLevel::Default,
            ApprovalRouting::AutoReview,
        );
        assert_eq!(
            refused.unsupported_reason,
            Some(UnsupportedReason::UnattendedNotPermitted)
        );
        assert_eq!(refused.vendor_id.as_deref(), Some("auto"));
    }

    #[test]
    fn an_account_without_a_qualifying_plan_is_told_which_way_to_fix_it() {
        for kind in [AccountKind::ApiKey, AccountKind::CloudProvider] {
            let availability = ModeAvailability {
                account_kind: Some(kind),
                ..ModeAvailability::default()
            };
            let refused = cell(
                &availability,
                PermissionLevel::Default,
                ApprovalRouting::AutoReview,
            );
            assert_eq!(
                refused.unsupported_reason,
                Some(UnsupportedReason::RequiresAccountUpgrade),
                "expected {kind:?} to be told the plan is the blocker"
            );
        }
    }

    #[test]
    fn an_unestablished_account_refuses_auto_rather_than_promising_it() {
        let refused = cell(
            &ModeAvailability::default(),
            PermissionLevel::Default,
            ApprovalRouting::AutoReview,
        );
        assert!(
            !refused.supported,
            "expected auto to fail closed on an unknown account"
        );
        assert!(matches!(
            refused.unsupported_reason,
            Some(UnsupportedReason::Other(_))
        ));
    }

    #[test]
    fn narrows_the_matrix_not_the_harness_when_a_permission_mode_is_gone() {
        let availability = ModeAvailability {
            accepted_modes: Some(
                ["manual", "acceptEdits", "auto", "bypassPermissions"]
                    .map(String::from)
                    .into_iter()
                    .collect(),
            ),
            ..subscription()
        };
        let refused = cell(
            &availability,
            PermissionLevel::ReadOnly,
            ApprovalRouting::User,
        );
        assert!(
            !refused.supported,
            "expected a build without `plan` to lose read-only"
        );
        assert_eq!(
            refused.unsupported_reason,
            Some(UnsupportedReason::RequiresNewerVersion)
        );
        assert_eq!(refused.vendor_id.as_deref(), Some("plan"));

        let kept = cell(
            &availability,
            PermissionLevel::Default,
            ApprovalRouting::User,
        );
        assert!(kept.supported, "expected every other pair to survive");
    }

    #[test]
    fn an_unread_vocabulary_narrows_nothing() {
        let availability = subscription();
        assert!(availability.accepted_modes.is_none());
        for mode in [
            CliMode::Plan,
            CliMode::Manual,
            CliMode::BypassPermissions,
            CliMode::Auto,
        ] {
            assert!(
                availability.accepts(mode),
                "expected {mode:?} to be allowed unproven"
            );
        }
    }

    #[test]
    fn every_pair_is_described_even_when_none_can_be_selected() {
        let refused = PermissionMatrix::none(UnsupportedReason::RequiresNewerVersion);
        assert_eq!(refused.cells().len(), 6);
        assert!(refused.cells().iter().all(|cell| !cell.supported));
    }

    #[test]
    fn only_the_literal_disable_counts_as_a_policy() {
        assert!(auto_mode_disabled(&json!({"disableAutoMode": "disable"})));
        for ignored in [
            json!({"disableAutoMode": true}),
            json!({"disableAutoMode": "Disable"}),
            json!({"disableAutoMode": "disabled"}),
            json!({"disableAutoMode": null}),
            json!({}),
            json!("not an object"),
        ] {
            assert!(
                !auto_mode_disabled(&ignored),
                "expected {ignored} to leave auto decided by the account"
            );
        }
    }
}
