//! The structured things a vendor produces, kept as structure rather than as a rendered string.
//!
//! An [`Activity`](crate::Activity) says *what* the agent is doing. This module says what came out
//! of it in a shape a host can act on: a plan with steps that have their own state, a diff with
//! per-file counts, a structured result. Flattening any of them into `detail` text is lossy in a
//! way that cannot be undone — a host cannot render a checklist from a paragraph, and re-parsing
//! one would make the host depend on a vendor's prose.
//!
//! What this module is not is a passthrough. Every field here is one the library named, bounded
//! and normalised. Vendor detail with no field of its own goes to
//! [`Extensions`](crate::Extensions), which is scalar-only and capped; nothing anywhere carries a
//! raw vendor frame, and nothing here is executable — a [`FileChange`] is a description of an edit
//! the vendor already made or proposes to make, never an instruction a host applies.

use crate::normalize::{self, TextLimit};

/// How many steps one plan may carry.
pub const PLAN_MAX_STEPS: usize = 128;

/// How many files one diff summary may carry.
pub const DIFF_MAX_FILES: usize = 256;

/// Where one plan step stands.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PlanStepStatus {
    /// Not started.
    #[default]
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
    /// Abandoned, skipped or otherwise not going to happen.
    Dropped,
}

/// One step of a plan the vendor wrote.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PlanStep {
    /// The vendor's own id for this step, when it has one.
    ///
    /// What lets a later update change one step rather than replace the whole plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// What the step says, as plain text.
    pub title: String,
    /// Where it stands.
    pub status: PlanStepStatus,
}

impl PlanStep {
    /// A pending step.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: None,
            title: title.into(),
            status: PlanStepStatus::Pending,
        }
    }

    /// Carries the vendor's own id for it.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Records where it stands.
    #[must_use]
    pub fn with_status(mut self, status: PlanStepStatus) -> Self {
        self.status = status;
        self
    }

    /// This step with its title bounded and an unusable id dropped.
    ///
    /// The id is dropped rather than refusing the step: a step nobody can address is still a step
    /// somebody can read, and losing the whole plan over one unusable id would be worse.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            id: self
                .id
                .and_then(|id| normalize::opaque_id(&id, "plan step id").ok()),
            title: normalize::bound_text(&self.title, TextLimit::Title).text,
            status: self.status,
        }
    }
}

/// What happened to one file.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum FileChangeKind {
    /// It did not exist before.
    Created,
    /// It existed and changed.
    Modified,
    /// It existed and does not now.
    Deleted,
    /// It is at a different path.
    Renamed,
}

/// One file an activity touched.
///
/// A description, never an instruction. Nothing in this library applies one, and a host that reads
/// `unified_diff` is reading a record of what the vendor did inside its own authorised working
/// directory.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FileChange {
    /// The path, as the vendor spelled it.
    pub path: String,
    /// What happened to it.
    pub kind: FileChangeKind,
    /// Where it was before, for a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_path: Option<String>,
    /// Lines added, when the vendor counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added_lines: Option<u32>,
    /// Lines removed, when the vendor counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_lines: Option<u32>,
    /// The diff itself, when the vendor sent one and it fits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unified_diff: Option<String>,
}

impl FileChange {
    /// One file, with nothing counted.
    pub fn new(path: impl Into<String>, kind: FileChangeKind) -> Self {
        Self {
            path: path.into(),
            kind,
            previous_path: None,
            added_lines: None,
            removed_lines: None,
            unified_diff: None,
        }
    }

    /// Records where the file was before a rename.
    #[must_use]
    pub fn moved_from(mut self, previous_path: impl Into<String>) -> Self {
        self.previous_path = Some(previous_path.into());
        self
    }

    /// Records the vendor's own line counts.
    #[must_use]
    pub fn with_line_counts(mut self, added: u32, removed: u32) -> Self {
        self.added_lines = Some(added);
        self.removed_lines = Some(removed);
        self
    }

    /// Carries the diff the vendor sent.
    #[must_use]
    pub fn with_unified_diff(mut self, unified_diff: impl Into<String>) -> Self {
        self.unified_diff = Some(unified_diff.into());
        self
    }

    /// This change with its paths and diff bounded, or nothing when the path cannot be carried.
    ///
    /// A path is sanitised but never shortened, on the same terms as everywhere else: a shortened
    /// path names a different file, and a row naming the wrong file is worse than a missing row.
    #[must_use]
    pub fn normalized(self) -> Option<Self> {
        Some(Self {
            path: normalize::vendor_path(&self.path)?,
            kind: self.kind,
            previous_path: self
                .previous_path
                .and_then(|path| normalize::vendor_path(&path)),
            unified_diff: self
                .unified_diff
                .map(|diff| normalize::bound_text(&diff, TextLimit::Detail).text),
            ..self
        })
    }
}

/// The structured thing an activity produced.
///
/// One arm per shape the library has agreed to carry. A vendor producing something else is carried
/// as an [`Activity`](crate::Activity) with its `detail` and whatever
/// [`Extensions`](crate::Extensions) the harness kept — not as an arm nobody can render.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ActivityContent {
    /// A plan, with steps that carry their own state.
    Plan {
        /// The steps, in the vendor's own order.
        steps: Vec<PlanStep>,
    },
    /// Files the activity changed.
    Diff {
        /// The files, in the vendor's own order.
        files: Vec<FileChange>,
    },
    /// Text the activity produced, kept as its own thing rather than folded into a title.
    Output {
        /// What it produced, bounded.
        text: String,
    },
}

impl ActivityContent {
    /// This content with every vendor-written value bounded and unusable rows dropped.
    #[must_use]
    pub fn normalized(self) -> Self {
        match self {
            Self::Plan { steps } => Self::Plan {
                steps: steps
                    .into_iter()
                    .take(PLAN_MAX_STEPS)
                    .map(PlanStep::normalized)
                    .collect(),
            },
            Self::Diff { files } => Self::Diff {
                files: files
                    .into_iter()
                    .filter_map(FileChange::normalized)
                    .take(DIFF_MAX_FILES)
                    .collect(),
            },
            Self::Output { text } => Self::Output {
                text: normalize::bound_text(&text, TextLimit::Detail).text,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActivityContent, DIFF_MAX_FILES, FileChange, FileChangeKind, PLAN_MAX_STEPS, PlanStep,
        PlanStepStatus,
    };

    /// A host cannot render a checklist from a paragraph, which is what folding a plan into detail
    /// text costs.
    #[test]
    fn a_plan_keeps_its_steps_their_ids_and_their_own_states() {
        let content = ActivityContent::Plan {
            steps: vec![
                PlanStep::new("read the reducer")
                    .with_id("step-1")
                    .with_status(PlanStepStatus::Completed),
                PlanStep::new("write the test").with_id("step-2"),
            ],
        }
        .normalized();

        let ActivityContent::Plan { steps } = content else {
            panic!("expected a plan");
        };
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id.as_deref(), Some("step-1"));
        assert_eq!(steps[0].status, PlanStepStatus::Completed);
        assert_eq!(steps[1].status, PlanStepStatus::Pending);
    }

    /// One unusable id must not cost a host the plan. The step still reads; it just cannot be
    /// addressed by a later update.
    #[test]
    fn a_step_whose_id_cannot_be_carried_keeps_its_text() {
        let step = PlanStep::new("do the thing")
            .with_id("s".repeat(129))
            .normalized();
        assert_eq!(step.id, None);
        assert_eq!(step.title, "do the thing");
    }

    #[test]
    fn a_diff_keeps_per_file_counts_and_the_vendors_own_order() {
        let content = ActivityContent::Diff {
            files: vec![
                FileChange::new("src/lib.rs", FileChangeKind::Modified).with_line_counts(10, 2),
                FileChange::new("src/new.rs", FileChangeKind::Created),
                FileChange::new("src/moved.rs", FileChangeKind::Renamed).moved_from("src/old.rs"),
            ],
        }
        .normalized();

        let ActivityContent::Diff { files } = content else {
            panic!("expected a diff");
        };
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].added_lines, Some(10));
        assert_eq!(files[0].removed_lines, Some(2));
        assert_eq!(files[1].added_lines, None, "uncounted is unknown, not zero");
        assert_eq!(files[2].previous_path.as_deref(), Some("src/old.rs"));
    }

    /// A shortened path names a different file, so the row goes rather than the path being cut.
    #[test]
    fn a_file_whose_path_cannot_be_carried_whole_is_dropped() {
        let content = ActivityContent::Diff {
            files: vec![
                FileChange::new("p".repeat(4_097), FileChangeKind::Modified),
                FileChange::new("src/lib.rs", FileChangeKind::Modified),
            ],
        }
        .normalized();

        let ActivityContent::Diff { files } = content else {
            panic!("expected a diff");
        };
        assert_eq!(files.len(), 1, "received {files:?}");
        assert_eq!(files[0].path, "src/lib.rs");
    }

    #[test]
    fn a_diff_and_a_plan_are_cut_to_their_own_ceilings() {
        let ActivityContent::Plan { steps } = (ActivityContent::Plan {
            steps: (0..PLAN_MAX_STEPS + 10)
                .map(|index| PlanStep::new(format!("step {index}")))
                .collect(),
        })
        .normalized() else {
            panic!("expected a plan");
        };
        assert_eq!(steps.len(), PLAN_MAX_STEPS);

        let ActivityContent::Diff { files } = (ActivityContent::Diff {
            files: (0..DIFF_MAX_FILES + 10)
                .map(|index| FileChange::new(format!("file{index}.rs"), FileChangeKind::Modified))
                .collect(),
        })
        .normalized() else {
            panic!("expected a diff");
        };
        assert_eq!(files.len(), DIFF_MAX_FILES);
    }

    #[test]
    fn content_round_trips_through_serialization_under_its_own_tag() {
        for content in [
            ActivityContent::Plan {
                steps: vec![PlanStep::new("one")],
            },
            ActivityContent::Diff {
                files: vec![FileChange::new("a.rs", FileChangeKind::Deleted)],
            },
            ActivityContent::Output {
                text: String::from("done"),
            },
        ] {
            let json = serde_json::to_value(&content).expect("expected serializable content");
            assert!(json.get("type").is_some(), "received {json}");
            assert_eq!(
                serde_json::from_value::<ActivityContent>(json).expect("expected the content back"),
                content
            );
        }
    }

    #[test]
    fn an_over_long_diff_body_is_cut_rather_than_dropping_the_file_it_describes() {
        let content = ActivityContent::Diff {
            files: vec![
                FileChange::new("src/lib.rs", FileChangeKind::Modified)
                    .with_unified_diff("+".repeat(9_000)),
            ],
        }
        .normalized();

        let ActivityContent::Diff { files } = content else {
            panic!("expected a diff");
        };
        assert_eq!(files.len(), 1);
        assert_eq!(
            files[0]
                .unified_diff
                .as_ref()
                .map(|diff| diff.chars().count()),
            Some(4_096)
        );
    }
}
