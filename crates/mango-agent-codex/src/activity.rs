//! A thread item, as something to render.
//!
//! The neutral vocabulary is observational on purpose: an [`Activity`] picks an icon and a line of
//! text, and nothing here can be handed to a tool executor because nothing here names a host tool.
//! What the vendor called its own tool travels verbatim in [`Activity::name`].

use mango_external_agents::content::{ActivityContent, FileChange};
use mango_external_agents::event::{Activity, ActivityKind, ActivityResult, ActivityStatus};

use crate::protocol::items::{FileUpdateChange, ItemStatus, ThreadItem};

/// What one item is, as something to show while it runs.
///
/// `None` for an item whose lifecycle is the turn's own text — the answer and the reasoning arrive
/// as deltas and again as a finished item, and rendering both would double every sentence — and
/// for a family this harness does not render.
#[must_use]
pub fn started(item: &ThreadItem) -> Option<Activity> {
    if item.is_streamed_text() {
        return None;
    }
    let (name, kind, title, detail, content) = match item {
        ThreadItem::CommandExecution { command, cwd, .. } => (
            "shell",
            ActivityKind::Command,
            command.clone(),
            cwd.clone().map(|cwd| format!("in {cwd}")),
            None,
        ),
        ThreadItem::FileChange { changes, .. } => (
            "apply_patch",
            ActivityKind::FileChange,
            match changes.len() {
                1 => changes[0].path.clone(),
                count => format!("{count} files"),
            },
            file_change_detail(changes),
            file_change_content(changes),
        ),
        ThreadItem::McpToolCall { server, tool, .. } => (
            "mcp",
            ActivityKind::Mcp,
            format!("{server}: {tool}"),
            None,
            None,
        ),
        ThreadItem::WebSearch { query, .. } => (
            "web_search",
            ActivityKind::WebSearch,
            query.clone(),
            None,
            None,
        ),
        // At this pin the app-server sends a plan as freeform `text`, never a step array — see
        // `ThreadItem::Plan`. There is no structure to lift into `ActivityContent::Plan`, so the
        // whole plan stays in `detail` rather than this harness inventing steps by splitting it.
        ThreadItem::Plan { text, .. } => (
            "update_plan",
            ActivityKind::Plan,
            String::from("Plan"),
            Some(text.clone()),
            None,
        ),
        ThreadItem::SubAgentActivity { kind, .. } => (
            "subagent",
            ActivityKind::Subagent,
            kind.clone().unwrap_or_else(|| String::from("Subagent")),
            None,
            None,
        ),
        ThreadItem::EnteredReviewMode { review, .. }
        | ThreadItem::ExitedReviewMode { review, .. } => {
            ("review", ActivityKind::Review, review.clone(), None, None)
        }
        ThreadItem::ContextCompaction { .. } => (
            "compact",
            ActivityKind::Compaction,
            String::from("Compacting the conversation"),
            None,
            None,
        ),
        ThreadItem::AgentMessage { .. } | ThreadItem::Reasoning { .. } | ThreadItem::Other => {
            return None;
        }
    };
    let activity = Activity::new(name, kind, title);
    let activity = match detail {
        Some(detail) => activity.with_detail(detail),
        None => activity,
    };
    let activity = match content {
        Some(content) => activity.with_content(content),
        None => activity,
    };
    // Every family reaching this point has an id: the three without one — the turn's own text and
    // the families this harness does not render — already returned above.
    Some(match item.id() {
        Some(id) => activity.with_item_id(id),
        None => activity,
    })
}

/// How one item ended.
///
/// `None` on the same terms as [`started`]: an item that was never announced as an activity must
/// not be completed as one, or the host receives a completion for a `call_id` it never saw.
#[must_use]
pub fn completed(item: &ThreadItem) -> Option<ActivityResult> {
    // Both halves have to agree: an item never announced as an activity must not be completed
    // as one, or the host receives a completion for a call id it was never told about.
    started(item)?;
    let (status, detail, content) = match item {
        ThreadItem::CommandExecution {
            status,
            aggregated_output,
            exit_code,
            ..
        } => (
            *status,
            command_detail(aggregated_output.as_deref(), *exit_code),
            // The exit code has no home here: it only becomes known once the command finishes,
            // and `Extensions` lives on the started `Activity`, not on this result — so it stays
            // folded into `detail` above rather than this harness inventing somewhere else to put
            // it.
            aggregated_output
                .clone()
                .map(|text| ActivityContent::Output { text }),
        ),
        ThreadItem::FileChange {
            status, changes, ..
        } => (
            *status,
            file_change_detail(changes),
            file_change_content(changes),
        ),
        ThreadItem::McpToolCall { status, .. } => (*status, None, None),
        // Families with no status of their own: reaching a completion notification is the whole
        // report, so they end as completed rather than as an unknown this harness invented.
        _ => (ItemStatus::Completed, None, None),
    };
    let result = ActivityResult::new(match status {
        ItemStatus::Completed => ActivityStatus::Completed,
        ItemStatus::Failed => ActivityStatus::Failed,
        ItemStatus::Declined => ActivityStatus::Cancelled,
        // An item that reached a completion notification while still claiming to be running,
        // or carrying a spelling this build does not know, has finished by some route nobody
        // here can name. `Completed` would assert success it has no evidence of.
        ItemStatus::InProgress | ItemStatus::Unknown => ActivityStatus::Failed,
    })
    .with_optional_detail(detail);
    Some(match content {
        Some(content) => result.with_content(content),
        None => result,
    })
}

fn command_detail(output: Option<&str>, exit_code: Option<i64>) -> Option<String> {
    match (output, exit_code) {
        (Some(output), Some(code)) => Some(format!("exit {code}\n{output}")),
        (Some(output), None) => Some(output.to_owned()),
        (None, Some(code)) => Some(format!("exit {code}")),
        (None, None) => None,
    }
}

pub(crate) fn file_change_detail(changes: &[FileUpdateChange]) -> Option<String> {
    if changes.is_empty() {
        return None;
    }
    Some(
        changes
            .iter()
            .map(|change| change.path.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// The files a patch touched, as structure a host can render rather than a joined path list.
///
/// `FileChange::kind` stays absent: at this pin the vendor states no per-file kind, and reading
/// one off the diff text would be re-parsing vendor prose this module exists to avoid.
pub(crate) fn file_change_content(changes: &[FileUpdateChange]) -> Option<ActivityContent> {
    if changes.is_empty() {
        return None;
    }
    Some(ActivityContent::Diff {
        files: changes
            .iter()
            .map(|change| {
                let file = FileChange::new(change.path.clone());
                if change.diff.is_empty() {
                    file
                } else {
                    file.with_unified_diff(change.diff.clone())
                }
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::{completed, started};
    use crate::protocol::items::ThreadItem;
    use mango_external_agents::content::ActivityContent;
    use mango_external_agents::event::{ActivityKind, ActivityStatus};
    use serde_json::json;

    fn item(raw: serde_json::Value) -> ThreadItem {
        serde_json::from_value(raw).expect("expected an item")
    }

    #[test]
    fn a_command_shows_what_it_runs_and_where() {
        let activity = started(&item(json!({
            "type": "commandExecution", "id": "exec-1",
            "command": "/bin/bash -lc 'echo mango'", "cwd": "/workspace", "status": "inProgress"
        })))
        .expect("expected an activity");

        assert_eq!(activity.kind, ActivityKind::Command);
        assert_eq!(activity.name, "shell");
        assert_eq!(activity.title, "/bin/bash -lc 'echo mango'");
        assert_eq!(activity.detail.as_deref(), Some("in /workspace"));
    }

    #[test]
    fn a_finished_command_reports_its_exit_code_and_output() {
        let result = completed(&item(json!({
            "type": "commandExecution", "id": "exec-1", "command": "echo mango",
            "status": "completed", "aggregatedOutput": "mango\n", "exitCode": 0
        })))
        .expect("expected a result");

        assert_eq!(result.status, ActivityStatus::Completed);
        assert_eq!(result.detail.as_deref(), Some("exit 0\nmango\n"));
    }

    /// The output is structure a host can render on its own, not only a paragraph glued to the
    /// exit code. `detail` keeps carrying both together, since that is what a host renders today.
    #[test]
    fn a_finished_commands_output_becomes_structured_content_alongside_its_detail() {
        let result = completed(&item(json!({
            "type": "commandExecution", "id": "exec-1", "command": "echo mango",
            "status": "completed", "aggregatedOutput": "mango\n", "exitCode": 0
        })))
        .expect("expected a result");

        assert_eq!(result.detail.as_deref(), Some("exit 0\nmango\n"));
        let Some(ActivityContent::Output { text }) = &result.content else {
            panic!("expected output content, received {:?}", result.content);
        };
        assert_eq!(text, "mango\n");
    }

    /// A command with no output the vendor reported carries none: absent is unknown, not empty.
    #[test]
    fn a_command_with_no_reported_output_carries_no_output_content() {
        let result = completed(&item(json!({
            "type": "commandExecution", "id": "exec-1", "command": "true",
            "status": "completed", "exitCode": 0
        })))
        .expect("expected a result");
        assert_eq!(result.content, None, "received {:?}", result.content);
    }

    /// A refused command is cancelled, not failed: nothing went wrong, somebody said no.
    #[test]
    fn a_command_a_person_refused_is_cancelled_rather_than_failed() {
        let result = completed(&item(json!({
            "type": "commandExecution", "id": "exec-1", "command": "rm -rf /",
            "status": "declined"
        })))
        .expect("expected a result");
        assert_eq!(result.status, ActivityStatus::Cancelled);
    }

    /// An item that completes while still claiming to run finished by a route nothing can name.
    /// Reporting success would be asserting an outcome there is no evidence for.
    #[test]
    fn an_item_that_completes_without_saying_how_is_not_reported_as_a_success() {
        for status in ["inProgress", "somethingElse"] {
            let result = completed(&item(json!({
                "type": "mcpToolCall", "id": "m-1", "server": "exa", "tool": "search",
                "status": status
            })))
            .expect("expected a result");
            assert_eq!(
                result.status,
                ActivityStatus::Failed,
                "expected {status:?} not to read as a success"
            );
        }
    }

    /// The answer and the reasoning are text, not activity. Both halves must agree, or a host
    /// receives a completion for a call id it was never told about.
    #[test]
    fn the_turns_own_text_is_never_an_activity_in_either_direction() {
        for raw in [
            json!({"type": "agentMessage", "id": "m-1", "text": "hello"}),
            json!({"type": "reasoning", "id": "r-1", "summary": ["thinking"], "content": []}),
            json!({"type": "somethingTheNextReleaseAdded", "id": "x-1"}),
        ] {
            let item = item(raw.clone());
            assert!(started(&item).is_none(), "expected no activity for {raw}");
            assert!(completed(&item).is_none(), "expected no result for {raw}");
        }
    }

    #[test]
    fn a_patch_names_one_file_or_counts_them() {
        let one = started(&item(json!({
            "type": "fileChange", "id": "p-1", "status": "completed",
            "changes": [{"path": "src/lib.rs", "kind": {"type": "add"}, "diff": ""}]
        })))
        .expect("expected an activity");
        assert_eq!(one.title, "src/lib.rs");

        let many = started(&item(json!({
            "type": "fileChange", "id": "p-2", "status": "completed",
            "changes": [
                {"path": "a.rs", "kind": {"type": "add"}, "diff": ""},
                {"path": "b.rs", "kind": {"type": "add"}, "diff": ""}
            ]
        })))
        .expect("expected an activity");
        assert_eq!(many.title, "2 files");
        assert_eq!(many.detail.as_deref(), Some("a.rs\nb.rs"));
    }

    /// The diff is structure a host can render as a diff, not only a path in a joined list.
    /// `kind` stays absent: at this pin the vendor states no per-file kind.
    #[test]
    fn a_file_change_activity_carries_its_diff_as_structured_content() {
        let diff = "@@ -1 +1 @@\n-old line\n+new line\n";
        let activity = started(&item(json!({
            "type": "fileChange", "id": "patch-1", "status": "completed",
            "changes": [{"path": "src/lib.rs", "diff": diff}]
        })))
        .expect("expected an activity");

        let Some(ActivityContent::Diff { files }) = &activity.content else {
            panic!("expected diff content, received {:?}", activity.content);
        };
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/lib.rs");
        assert_eq!(files[0].unified_diff.as_deref(), Some(diff));
        assert_eq!(
            files[0].kind, None,
            "expected no kind: the vendor states none at this pin"
        );
    }

    /// The completion carries the same structured diff as the start, not just its own `detail`.
    #[test]
    fn a_finished_file_changes_result_carries_its_diff_too() {
        let diff = "@@ -1,2 +1,2 @@\n-a\n+b\n";
        let result = completed(&item(json!({
            "type": "fileChange", "id": "patch-1", "status": "completed",
            "changes": [{"path": "src/lib.rs", "diff": diff}]
        })))
        .expect("expected a result");

        let Some(ActivityContent::Diff { files }) = &result.content else {
            panic!("expected diff content, received {:?}", result.content);
        };
        assert_eq!(files[0].unified_diff.as_deref(), Some(diff));
    }

    /// An empty diff string proves nothing was sent, so the file keeps its path but no diff body.
    #[test]
    fn a_file_change_with_no_diff_body_carries_no_unified_diff() {
        let activity = started(&item(json!({
            "type": "fileChange", "id": "patch-1", "status": "completed",
            "changes": [{"path": "src/lib.rs", "diff": ""}]
        })))
        .expect("expected an activity");

        let Some(ActivityContent::Diff { files }) = &activity.content else {
            panic!("expected diff content, received {:?}", activity.content);
        };
        assert_eq!(files[0].unified_diff, None);
    }

    #[test]
    fn an_mcp_call_names_the_server_and_the_tool_the_user_configured() {
        let activity = started(&item(json!({
            "type": "mcpToolCall", "id": "m-1", "server": "exa", "tool": "web_search",
            "status": "inProgress"
        })))
        .expect("expected an activity");
        assert_eq!(activity.kind, ActivityKind::Mcp);
        assert_eq!(activity.title, "exa: web_search");
    }

    #[test]
    fn entering_and_leaving_review_mode_both_render_as_review() {
        for kind in ["enteredReviewMode", "exitedReviewMode"] {
            let activity = started(&item(json!({
                "type": kind, "id": "r-1", "review": "current changes"
            })))
            .expect("expected an activity");
            assert_eq!(activity.kind, ActivityKind::Review);
            assert_eq!(activity.title, "current changes");
        }
    }

    /// A host reading an `Activity` on its own, without the event envelope beside it, still needs
    /// to know which vendor item it is.
    #[test]
    fn a_started_activity_carries_the_vendor_items_own_id() {
        let activity = started(&item(json!({
            "type": "commandExecution", "id": "exec-9", "command": "ls", "status": "inProgress"
        })))
        .expect("expected an activity");
        assert_eq!(activity.item_id.as_deref(), Some("exec-9"));
    }

    /// The plan is freeform `text` at this pin, not a step array — see `ThreadItem::Plan`. The
    /// whole thing stays in `detail`; nothing here invents steps by splitting it.
    #[test]
    fn a_plan_keeps_its_freeform_text_with_no_step_structure_invented() {
        let activity = started(&item(json!({
            "type": "plan", "id": "plan-1", "text": "1. read the reducer\n2. patch it"
        })))
        .expect("expected an activity");
        assert_eq!(
            activity.detail.as_deref(),
            Some("1. read the reducer\n2. patch it")
        );
        assert_eq!(
            activity.content, None,
            "expected no structure: the vendor sends freeform text at this pin"
        );
        assert_eq!(activity.item_id.as_deref(), Some("plan-1"));
    }
}
