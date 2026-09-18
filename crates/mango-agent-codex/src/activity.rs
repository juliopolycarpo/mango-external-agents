//! A thread item, as something to render.
//!
//! The neutral vocabulary is observational on purpose: an [`Activity`] picks an icon and a line of
//! text, and nothing here can be handed to a tool executor because nothing here names a host tool.
//! What the vendor called its own tool travels verbatim in [`Activity::name`].

use mango_external_agents::event::{Activity, ActivityKind, ActivityResult, ActivityStatus};

use crate::protocol::items::{ItemStatus, ThreadItem};

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
    let (name, kind, title, detail) = match item {
        ThreadItem::CommandExecution { command, cwd, .. } => (
            "shell",
            ActivityKind::Command,
            command.clone(),
            cwd.clone().map(|cwd| format!("in {cwd}")),
        ),
        ThreadItem::FileChange { changes, .. } => (
            "apply_patch",
            ActivityKind::FileChange,
            match changes.len() {
                1 => changes[0].path.clone(),
                count => format!("{count} files"),
            },
            file_change_detail(changes),
        ),
        ThreadItem::McpToolCall { server, tool, .. } => {
            ("mcp", ActivityKind::Mcp, format!("{server}: {tool}"), None)
        }
        ThreadItem::WebSearch { query, .. } => {
            ("web_search", ActivityKind::WebSearch, query.clone(), None)
        }
        ThreadItem::Plan { text, .. } => (
            "update_plan",
            ActivityKind::Plan,
            String::from("Plan"),
            Some(text.clone()),
        ),
        ThreadItem::SubAgentActivity { kind, .. } => (
            "subagent",
            ActivityKind::Subagent,
            kind.clone().unwrap_or_else(|| String::from("Subagent")),
            None,
        ),
        ThreadItem::EnteredReviewMode { review, .. }
        | ThreadItem::ExitedReviewMode { review, .. } => {
            ("review", ActivityKind::Review, review.clone(), None)
        }
        ThreadItem::ContextCompaction { .. } => (
            "compact",
            ActivityKind::Compaction,
            String::from("Compacting the conversation"),
            None,
        ),
        ThreadItem::AgentMessage { .. } | ThreadItem::Reasoning { .. } | ThreadItem::Other => {
            return None;
        }
    };
    let activity = Activity::new(name, kind, title);
    Some(match detail {
        Some(detail) => activity.with_detail(detail),
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
    let (status, detail) = match item {
        ThreadItem::CommandExecution {
            status,
            aggregated_output,
            exit_code,
            ..
        } => (
            *status,
            command_detail(aggregated_output.as_deref(), *exit_code),
        ),
        ThreadItem::FileChange {
            status, changes, ..
        } => (*status, file_change_detail(changes)),
        ThreadItem::McpToolCall { status, .. } => (*status, None),
        // Families with no status of their own: reaching a completion notification is the whole
        // report, so they end as completed rather than as an unknown this harness invented.
        _ => (ItemStatus::Completed, None),
    };
    Some(
        ActivityResult::new(match status {
            ItemStatus::Completed => ActivityStatus::Completed,
            ItemStatus::Failed => ActivityStatus::Failed,
            ItemStatus::Declined => ActivityStatus::Cancelled,
            // An item that reached a completion notification while still claiming to be running,
            // or carrying a spelling this build does not know, has finished by some route nobody
            // here can name. `Completed` would assert success it has no evidence of.
            ItemStatus::InProgress | ItemStatus::Unknown => ActivityStatus::Failed,
        })
        .with_optional_detail(detail),
    )
}

fn command_detail(output: Option<&str>, exit_code: Option<i64>) -> Option<String> {
    match (output, exit_code) {
        (Some(output), Some(code)) => Some(format!("exit {code}\n{output}")),
        (Some(output), None) => Some(output.to_owned()),
        (None, Some(code)) => Some(format!("exit {code}")),
        (None, None) => None,
    }
}

fn file_change_detail(changes: &[crate::protocol::items::FileUpdateChange]) -> Option<String> {
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

#[cfg(test)]
mod tests {
    use super::{completed, started};
    use crate::protocol::items::ThreadItem;
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
}
