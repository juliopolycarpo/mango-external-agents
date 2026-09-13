//! The two product axes, as the three settings the app-server actually takes.
//!
//! Codex spells "what may the agent do" as two fields that move together — a sandbox and an
//! approval policy — and "who answers" as a third. A level that set only one of the first two
//! would be a configuration nobody chose: `workspace-write` with `never` is full access inside the
//! workspace, and `read-only` with `on-request` is an agent that asks for permissions it will not
//! be given.

use mango_external_agents::permission::{
    ApprovalRouting, ConfigurationVerdict, PermissionLevel, PermissionMatrix,
};

use crate::protocol::requests::{ApprovalsReviewer, AskForApproval, SandboxMode};

/// The three settings one (level, routing) pair becomes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VendorConfiguration {
    /// What the agent may touch.
    pub sandbox: SandboxMode,
    /// How much it has to ask.
    pub approval_policy: AskForApproval,
    /// Who answers when it does.
    pub approvals_reviewer: ApprovalsReviewer,
}

impl VendorConfiguration {
    /// The settings this pair runs under.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::permissions::VendorConfiguration;
    /// use mango_agent_codex::protocol::{AskForApproval, SandboxMode};
    /// use mango_external_agents::{ApprovalRouting, PermissionLevel};
    ///
    /// let configuration =
    ///     VendorConfiguration::for_pair(PermissionLevel::Default, ApprovalRouting::User);
    /// assert_eq!(configuration.sandbox, SandboxMode::WorkspaceWrite);
    /// assert_eq!(configuration.approval_policy, AskForApproval::OnRequest);
    /// ```
    #[must_use]
    pub fn for_pair(level: PermissionLevel, routing: ApprovalRouting) -> Self {
        let (sandbox, approval_policy) = match level {
            // Nothing may change the machine, so there is nothing to approve. `on-request` here
            // would be an agent that asks to escalate out of a sandbox this level exists to keep
            // it in — a prompt whose only honest answer is no.
            PermissionLevel::ReadOnly => (SandboxMode::ReadOnly, AskForApproval::Never),
            PermissionLevel::Default => (SandboxMode::WorkspaceWrite, AskForApproval::OnRequest),
            PermissionLevel::FullAccess => (SandboxMode::DangerFullAccess, AskForApproval::Never),
        };
        Self {
            sandbox,
            approval_policy,
            approvals_reviewer: match routing {
                ApprovalRouting::User => ApprovalsReviewer::User,
                ApprovalRouting::AutoReview => ApprovalsReviewer::AutoReview,
            },
        }
    }

    /// How this harness spells the pair to the vendor, for the matrix cell to carry.
    #[must_use]
    pub fn vendor_id(&self) -> String {
        let sandbox = match self.sandbox {
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
            SandboxMode::DangerFullAccess => "danger-full-access",
        };
        let policy = match self.approval_policy {
            AskForApproval::Untrusted => "untrusted",
            AskForApproval::OnRequest => "on-request",
            AskForApproval::Never => "never",
        };
        let reviewer = match self.approvals_reviewer {
            ApprovalsReviewer::User => "user",
            ApprovalsReviewer::AutoReview => "auto_review",
        };
        format!("{sandbox}/{policy}/{reviewer}")
    }
}

/// Which (level, routing) pairs this harness can run.
///
/// All six. The app-server takes every sandbox with every reviewer, and a smoke run against a
/// consumer subscription accepted `approvals_reviewer: "auto_review"` on `thread/start` — so
/// refusing a cell here would be this harness narrowing what the vendor offers rather than
/// reporting it.
#[must_use]
pub fn matrix() -> PermissionMatrix {
    PermissionMatrix::build(|level, routing| ConfigurationVerdict::Supported {
        vendor_id: Some(VendorConfiguration::for_pair(level, routing).vendor_id()),
    })
}

#[cfg(test)]
mod tests {
    use super::{VendorConfiguration, matrix};
    use crate::protocol::requests::{ApprovalsReviewer, AskForApproval, SandboxMode};
    use mango_external_agents::permission::{ApprovalRouting, PermissionLevel};

    /// The sandbox and the policy move together. A pair that set one and not the other would
    /// produce a configuration nobody chose.
    #[test]
    fn each_level_sets_both_halves_of_what_the_agent_may_do() {
        let cases = [
            (
                PermissionLevel::ReadOnly,
                SandboxMode::ReadOnly,
                AskForApproval::Never,
            ),
            (
                PermissionLevel::Default,
                SandboxMode::WorkspaceWrite,
                AskForApproval::OnRequest,
            ),
            (
                PermissionLevel::FullAccess,
                SandboxMode::DangerFullAccess,
                AskForApproval::Never,
            ),
        ];
        for (level, sandbox, policy) in cases {
            let configuration = VendorConfiguration::for_pair(level, ApprovalRouting::User);
            assert_eq!(
                (configuration.sandbox, configuration.approval_policy),
                (sandbox, policy),
                "expected {sandbox:?}/{policy:?} for {level:?}, received {configuration:?}"
            );
        }
    }

    /// Read-only is the cell worth asserting on its own: `on-request` there is an agent asking to
    /// leave the sandbox that defines the level.
    #[test]
    fn read_only_never_asks_to_escalate_out_of_its_own_sandbox() {
        let configuration =
            VendorConfiguration::for_pair(PermissionLevel::ReadOnly, ApprovalRouting::User);
        assert_eq!(configuration.approval_policy, AskForApproval::Never);
        assert_eq!(configuration.vendor_id(), "read-only/never/user");
    }

    #[test]
    fn routing_chooses_the_vendors_own_reviewer() {
        for (routing, reviewer) in [
            (ApprovalRouting::User, ApprovalsReviewer::User),
            (ApprovalRouting::AutoReview, ApprovalsReviewer::AutoReview),
        ] {
            let configuration = VendorConfiguration::for_pair(PermissionLevel::Default, routing);
            assert_eq!(configuration.approvals_reviewer, reviewer);
        }
    }

    /// Six cells, every one supported, every one naming the three settings it becomes.
    #[test]
    fn the_matrix_supports_every_pair_and_says_how_it_spells_each_one() {
        let matrix = matrix();
        assert_eq!(matrix.cells().len(), 6);
        for cell in matrix.cells() {
            assert!(
                cell.supported,
                "expected every pair to be supported, received {cell:?}"
            );
            assert_eq!(cell.unsupported_reason, None);
            let vendor_id = cell
                .vendor_id
                .as_deref()
                .unwrap_or_else(|| panic!("expected a vendor id on {cell:?}"));
            assert_eq!(
                vendor_id.split('/').count(),
                3,
                "expected sandbox/policy/reviewer, received {vendor_id}"
            );
        }
    }

    /// `unattended` is the core's to decide, never the harness's. Full access is unattended
    /// whoever is nominally reviewing, and so is auto-review at any level.
    #[test]
    fn the_matrix_marks_unattended_cells_the_way_the_core_defines_them() {
        let matrix = matrix();
        let unattended = |level, routing| {
            matrix
                .cell(level, routing)
                .expect("expected a cell")
                .unattended
        };
        assert!(!unattended(
            PermissionLevel::ReadOnly,
            ApprovalRouting::User
        ));
        assert!(unattended(
            PermissionLevel::ReadOnly,
            ApprovalRouting::AutoReview
        ));
        assert!(unattended(
            PermissionLevel::FullAccess,
            ApprovalRouting::User
        ));
    }
}
