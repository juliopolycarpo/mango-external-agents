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

use std::fmt;

use crate::normalize::{self, TextLimit};

/// How many steps one plan may carry.
pub const PLAN_MAX_STEPS: usize = 128;

/// How many files one diff summary may carry.
pub const DIFF_MAX_FILES: usize = 256;

/// How many code points of file *contents* one diff summary may carry in total.
///
/// Each of [`FileChange`]'s three bodies is bounded on its own, which is enough to stop any one of
/// them being a payload channel and not enough to stop [`DIFF_MAX_FILES`] of them together: the
/// per-file ceilings multiply out to megabytes, against a turn buffer a host sizes in megabytes.
/// So the bodies share a budget. Files past it keep their row — the path and the counts, which is
/// what a host lists — and lose their contents, because knowing *which* files changed is worth more
/// than the first few bodies and costs almost nothing.
pub const DIFF_MAX_CONTENT_LENGTH: usize = 256 * 1024;

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

impl fmt::Display for PlanStepStatus {
    /// The status as one lowercase word, for a host rendering a checklist.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "pending",
            Self::InProgress => "in progress",
            Self::Completed => "completed",
            Self::Dropped => "dropped",
        })
    }
}

/// How important one plan step is, as the vendor ranked it.
///
/// Absent where the vendor states no ranking. An unranked step is one nobody ranked, not a
/// low-priority one, which is why this is an [`Option`] on [`PlanStep`] rather than a default.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum PlanStepPriority {
    /// Critical to the goal.
    High,
    /// Important, not critical.
    Medium,
    /// Nice to have.
    Low,
}

/// One step of a plan the vendor wrote.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// How the vendor ranked it, when it ranked it at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<PlanStepPriority>,
}

impl fmt::Debug for PlanStep {
    /// Reports a plan step's status without replaying vendor text or ids.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PlanStep")
            .field("has_id", &self.id.is_some())
            .field("status", &self.status)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

impl PlanStep {
    /// A pending step.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            id: None,
            title: title.into(),
            status: PlanStepStatus::Pending,
            priority: None,
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

    /// Records how the vendor ranked it.
    #[must_use]
    pub fn with_priority(mut self, priority: PlanStepPriority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// This step with its title bounded and an unusable id dropped.
    ///
    /// The id is dropped rather than refusing the step: a step nobody can address is still a step
    /// somebody can read, and losing the whole plan over one unusable id would be worse.
    #[must_use]
    pub fn normalized(self) -> Self {
        self.normalized_with_truncation().0
    }

    /// The same, saying whether anything was cut.
    ///
    /// The caller is an [`ActivityContent`], which is carried by an event that publishes a
    /// `truncated` flag. A cut that never reaches that flag is a host told its payload is whole.
    #[must_use]
    fn normalized_with_truncation(self) -> (Self, bool) {
        let title = normalize::bound_text(&self.title, TextLimit::Title);
        let truncated = title.truncated;
        (
            Self {
                id: self
                    .id
                    .and_then(|id| normalize::opaque_id(&id, "plan step id").ok()),
                title: title.text,
                status: self.status,
                priority: self.priority,
            },
            truncated,
        )
    }
}

/// What happened to one file.
///
/// Carried as an [`Option`] on [`FileChange`]: two of the three vendors this library drives send a
/// path and a body without ever saying which of these it is, and deriving one by re-reading the
/// diff text would be the host depending on a vendor's prose that this module exists to prevent.
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

impl fmt::Display for FileChangeKind {
    /// What happened, as one lowercase word.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
        })
    }
}

/// One file an activity touched.
///
/// A description, never an instruction. Nothing in this library applies one, and a host that reads
/// `unified_diff` is reading a record of what the vendor did inside its own authorised working
/// directory.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct FileChange {
    /// The path, as the vendor spelled it.
    pub path: String,
    /// What happened to it, when the vendor said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<FileChangeKind>,
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
    /// The contents before the change, when the vendor sent contents rather than a diff.
    ///
    /// Absent for a file the vendor says is new, and absent for a vendor that sends a diff: one of
    /// the two shapes is never derived from the other. Rendering a diff from these two is the
    /// host's own choice; computing one here would put bytes no vendor wrote into a transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// The contents after it, on the same terms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
}

impl fmt::Debug for FileChange {
    /// Reports a file change's shape without logging paths or diff content.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileChange")
            .field("kind", &self.kind)
            .field("has_previous_path", &self.previous_path.is_some())
            .field("added_lines", &self.added_lines)
            .field("removed_lines", &self.removed_lines)
            .field("has_unified_diff", &self.unified_diff.is_some())
            .field("has_old_text", &self.old_text.is_some())
            .field("has_new_text", &self.new_text.is_some())
            .finish_non_exhaustive()
    }
}

impl FileChange {
    /// One file, with nothing said about it beyond its path.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{FileChange, FileChangeKind};
    ///
    /// let change = FileChange::new("src/lib.rs").with_kind(FileChangeKind::Modified);
    /// assert_eq!(change.kind, Some(FileChangeKind::Modified));
    /// ```
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            kind: None,
            previous_path: None,
            added_lines: None,
            removed_lines: None,
            unified_diff: None,
            old_text: None,
            new_text: None,
        }
    }

    /// Records what the vendor said happened to it.
    #[must_use]
    pub fn with_kind(mut self, kind: FileChangeKind) -> Self {
        self.kind = Some(kind);
        self
    }

    /// Records where the file was before a rename, and that it was one.
    #[must_use]
    pub fn moved_from(mut self, previous_path: impl Into<String>) -> Self {
        self.previous_path = Some(previous_path.into());
        self.kind = Some(FileChangeKind::Renamed);
        self
    }

    /// Carries the contents after the change, with nothing claimed about what happened to the file.
    ///
    /// For a vendor whose write tool both creates and overwrites: the body is a fact, and which of
    /// the two it was is not one this library has. Use [`with_texts`](Self::with_texts) where the
    /// vendor says whether there was a before.
    #[must_use]
    pub fn with_new_text(mut self, new: impl Into<String>) -> Self {
        self.new_text = Some(new.into());
        self
    }

    /// Carries the contents the vendor sent on either side of the change.
    ///
    /// `None` for `old` is the vendor saying the file is new, which is the one kind this can be
    /// read off a body rather than guessed.
    #[must_use]
    pub fn with_texts(mut self, old: Option<String>, new: impl Into<String>) -> Self {
        self.kind = Some(match old {
            Some(_) => FileChangeKind::Modified,
            None => FileChangeKind::Created,
        });
        self.old_text = old;
        self.new_text = Some(new.into());
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
        self.normalized_with_truncation().0
    }

    /// This change with every body dropped, keeping the row that says which file it was.
    #[must_use]
    fn without_contents(self) -> Self {
        Self {
            unified_diff: None,
            old_text: None,
            new_text: None,
            ..self
        }
    }

    /// How many code points its three bodies hold together.
    fn content_length(&self) -> usize {
        [
            self.unified_diff.as_deref(),
            self.old_text.as_deref(),
            self.new_text.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(|body| body.chars().count())
        .sum()
    }

    /// The same, saying whether any of its three bodies was cut.
    ///
    /// A dropped row is reported by the caller, which is the only one that knows a row went
    /// missing; this reports only what it shortened itself.
    #[must_use]
    fn normalized_with_truncation(self) -> (Option<Self>, bool) {
        let unified_diff = self
            .unified_diff
            .map(|diff| normalize::bound_text(&diff, TextLimit::Detail));
        let old_text = self
            .old_text
            .map(|text| normalize::bound_text(&text, TextLimit::Detail));
        let new_text = self
            .new_text
            .map(|text| normalize::bound_text(&text, TextLimit::Detail));
        let truncated = [unified_diff.as_ref(), old_text.as_ref(), new_text.as_ref()]
            .into_iter()
            .flatten()
            .any(|bounded| bounded.truncated);
        let Some(path) = normalize::vendor_path(&self.path) else {
            return (None, truncated);
        };
        (
            Some(Self {
                path,
                kind: self.kind,
                previous_path: self
                    .previous_path
                    .and_then(|path| normalize::vendor_path(&path)),
                unified_diff: unified_diff.map(|bounded| bounded.text),
                old_text: old_text.map(|bounded| bounded.text),
                new_text: new_text.map(|bounded| bounded.text),
                ..self
            }),
            truncated,
        )
    }
}

/// The structured thing an activity produced.
///
/// One arm per shape the library has agreed to carry. A vendor producing something else is carried
/// as an [`Activity`](crate::Activity) with its `detail` and whatever
/// [`Extensions`](crate::Extensions) the harness kept — not as an arm nobody can render.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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

impl fmt::Debug for ActivityContent {
    /// Reports activity content shape without replaying vendor payloads.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plan { steps } => formatter
                .debug_struct("Plan")
                .field("step_count", &steps.len())
                .finish(),
            Self::Diff { files } => formatter
                .debug_struct("Diff")
                .field("file_count", &files.len())
                .finish(),
            Self::Output { .. } => formatter.write_str("Output"),
        }
    }
}

impl ActivityContent {
    /// This content with every vendor-written value bounded and unusable rows dropped.
    #[must_use]
    pub fn normalized(self) -> Self {
        self.normalized_with_truncation().0
    }

    /// The same, saying whether anything was cut, shortened or dropped on the way through.
    ///
    /// Every event that carries content also publishes a `truncated` flag, and that flag is a
    /// promise: a host reading `false` is entitled to treat what it received as whole. A plan cut
    /// to its ceiling, a file whose path could not be carried, a body shortened to fit — all three
    /// are the flag's business, and none of them shows up in the title or the detail the flag used
    /// to be derived from.
    #[must_use]
    pub fn normalized_with_truncation(self) -> (Self, bool) {
        match self {
            Self::Plan { steps } => {
                let dropped = steps.len() > PLAN_MAX_STEPS;
                let mut truncated = dropped;
                let steps = steps
                    .into_iter()
                    .take(PLAN_MAX_STEPS)
                    .map(|step| {
                        let (step, cut) = step.normalized_with_truncation();
                        truncated |= cut;
                        step
                    })
                    .collect();
                (Self::Plan { steps }, truncated)
            }
            Self::Diff { files } => {
                let offered = files.len();
                let mut truncated = false;
                let mut spent = 0usize;
                let mut carried: Vec<FileChange> = Vec::with_capacity(offered.min(DIFF_MAX_FILES));
                for file in files {
                    if carried.len() == DIFF_MAX_FILES {
                        break;
                    }
                    let (file, cut) = file.normalized_with_truncation();
                    truncated |= cut;
                    let Some(file) = file else {
                        continue;
                    };
                    spent = spent.saturating_add(file.content_length());
                    if spent <= DIFF_MAX_CONTENT_LENGTH {
                        carried.push(file);
                        continue;
                    }
                    truncated = true;
                    carried.push(file.without_contents());
                }
                let files = carried;
                truncated |= files.len() < offered;
                (Self::Diff { files }, truncated)
            }
            Self::Output { text } => {
                let bounded = normalize::bound_text(&text, TextLimit::Detail);
                (Self::Output { text: bounded.text }, bounded.truncated)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ActivityContent, DIFF_MAX_CONTENT_LENGTH, DIFF_MAX_FILES, FileChange, FileChangeKind,
        PLAN_MAX_STEPS, PlanStep, PlanStepStatus,
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
                FileChange::new("src/lib.rs")
                    .with_kind(FileChangeKind::Modified)
                    .with_line_counts(10, 2),
                FileChange::new("src/new.rs").with_kind(FileChangeKind::Created),
                FileChange::new("src/moved.rs").moved_from("src/old.rs"),
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
                FileChange::new("p".repeat(4_097)).with_kind(FileChangeKind::Modified),
                FileChange::new("src/lib.rs").with_kind(FileChangeKind::Modified),
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
                .map(|index| {
                    FileChange::new(format!("file{index}.rs")).with_kind(FileChangeKind::Modified)
                })
                .collect(),
        })
        .normalized() else {
            panic!("expected a diff");
        };
        assert_eq!(files.len(), DIFF_MAX_FILES);
    }

    /// Per-file bounds stop any one body being a payload channel; they do not stop 256 of them
    /// together, which multiply out to megabytes against a turn buffer a host sizes in megabytes.
    /// The rows survive — which files changed is what a host lists — and the bodies stop.
    #[test]
    fn a_diffs_bodies_share_a_budget_and_the_rows_past_it_keep_their_paths() {
        let body = "x".repeat(4_000);
        let ActivityContent::Diff { files } = (ActivityContent::Diff {
            files: (0..200)
                .map(|index| {
                    FileChange::new(format!("src/file{index}.rs"))
                        .with_new_text(body.clone())
                        .with_line_counts(10, 2)
                })
                .collect(),
        })
        .normalized() else {
            panic!("expected a diff");
        };

        assert_eq!(files.len(), 200, "every row survives");
        assert!(
            files[0].new_text.is_some(),
            "the first bodies are carried, received {:?}",
            files[0]
        );
        assert_eq!(
            files[199].new_text, None,
            "a body past the budget is dropped, received {:?}",
            files[199]
        );
        assert_eq!(
            files[199].added_lines,
            Some(10),
            "the row keeps what a host lists it by"
        );
        let carried: usize = files
            .iter()
            .filter_map(|file| file.new_text.as_deref())
            .map(|text| text.chars().count())
            .sum();
        assert!(
            carried <= DIFF_MAX_CONTENT_LENGTH,
            "expected at most {DIFF_MAX_CONTENT_LENGTH} code points of contents, received {carried}"
        );
    }

    #[test]
    fn content_round_trips_through_serialization_under_its_own_tag() {
        for content in [
            ActivityContent::Plan {
                steps: vec![PlanStep::new("one")],
            },
            ActivityContent::Diff {
                files: vec![FileChange::new("a.rs").with_kind(FileChangeKind::Deleted)],
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
                FileChange::new("src/lib.rs")
                    .with_kind(FileChangeKind::Modified)
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
