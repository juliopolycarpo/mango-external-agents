//! A thread item: the unit the app-server streams a turn's work in.
//!
//! Nineteen variants upstream, of which a host renders a handful and the rest are either the
//! turn's own text (handled as deltas) or vendor bookkeeping. The variants here are the ones that
//! become an [`Activity`](mango_external_agents::Activity) or a piece of the answer; every other
//! item deserialises into [`ThreadItem::Other`] with its `type` kept, so the reducer can drop it
//! by name rather than by silence.

use serde::Deserialize;
use serde_json::Value;

/// How a piece of a turn's work ended, in the one spelling every item family shares.
///
/// Upstream declares a status enum per family — `CommandExecutionStatus`, `PatchApplyStatus`,
/// `McpToolCallStatus`, `DynamicToolCallStatus` — with the same four spellings between them.
/// One type reads all of them, and a spelling none of them has reads as
/// [`ItemStatus::Unknown`] rather than failing the item.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ItemStatus {
    /// Still running.
    InProgress,
    /// It finished.
    Completed,
    /// It failed.
    Failed,
    /// Somebody refused it.
    Declined,
    /// A spelling this harness does not know.
    #[serde(other)]
    #[default]
    Unknown,
}

/// Kept as the name the command-execution family uses, which is the one a reader looks for.
pub type CommandExecutionStatus = ItemStatus;

/// One file a patch touches.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileUpdateChange {
    /// The file.
    #[serde(default)]
    pub path: String,
    /// The unified diff, as the vendor wrote it.
    #[serde(default)]
    pub diff: String,
}

/// One unit of a turn's work, in the families this harness renders.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ThreadItem {
    /// A piece of the answer, as the whole message rather than a delta.
    AgentMessage {
        /// The item's own id.
        id: String,
        /// The text.
        #[serde(default)]
        text: String,
    },
    /// The agent's reasoning, as the whole block rather than a delta.
    Reasoning {
        /// The item's own id.
        id: String,
        /// The summary the vendor streamed.
        #[serde(default)]
        summary: Vec<String>,
        /// The raw reasoning, when the build sends it.
        #[serde(default)]
        content: Vec<String>,
    },
    /// A shell command.
    CommandExecution {
        /// The item's own id, echoed by the approval that gates it.
        id: String,
        /// The command line, as the vendor composed it.
        #[serde(default)]
        command: String,
        /// Where it runs.
        #[serde(default)]
        cwd: Option<String>,
        /// How it is going.
        #[serde(default)]
        status: ItemStatus,
        /// What it printed, stdout and stderr together.
        #[serde(default)]
        aggregated_output: Option<String>,
        /// What it exited with.
        #[serde(default)]
        exit_code: Option<i64>,
    },
    /// Files the agent wrote.
    FileChange {
        /// The item's own id.
        id: String,
        /// The files.
        #[serde(default)]
        changes: Vec<FileUpdateChange>,
        /// How it is going.
        #[serde(default)]
        status: ItemStatus,
    },
    /// A call to an MCP server the user configured with the vendor's own CLI.
    McpToolCall {
        /// The item's own id.
        id: String,
        /// The server.
        #[serde(default)]
        server: String,
        /// The tool on it.
        #[serde(default)]
        tool: String,
        /// How it is going.
        #[serde(default)]
        status: ItemStatus,
    },
    /// A web search.
    WebSearch {
        /// The item's own id.
        id: String,
        /// What was searched for.
        #[serde(default)]
        query: String,
    },
    /// A plan the agent wrote.
    Plan {
        /// The item's own id.
        id: String,
        /// The plan, as the vendor's own freeform prose.
        ///
        /// At the pinned schema version there is no step array here, only this one string — a
        /// reader expecting `PlanStep`-shaped structure will not find it on this item. Splitting
        /// `text` on newlines or numbering to invent steps would be this harness reading a
        /// structure into vendor prose that the app-server never sent.
        #[serde(default)]
        text: String,
    },
    /// A subagent the vendor started.
    ///
    /// The pinned schema's `ThreadItem` property union also lists `agentThreadId`,
    /// `senderThreadId` and `receiverThreadIds` — but that union is flattened across every item
    /// family (`collabAgentToolCall` among them), not broken out per variant, so it cannot say
    /// which family actually carries them. Nothing here claims one for `SubAgentActivity` without
    /// that confirmation; a thread id invented on the wrong family would be worse than the field
    /// staying absent.
    SubAgentActivity {
        /// The item's own id.
        id: String,
        /// What the subagent did.
        #[serde(default)]
        kind: Option<String>,
    },
    /// The vendor entered its own review mode.
    EnteredReviewMode {
        /// The item's own id.
        id: String,
        /// What is being reviewed, in the vendor's own words.
        #[serde(default)]
        review: String,
    },
    /// It left again.
    ExitedReviewMode {
        /// The item's own id.
        id: String,
        /// What was reviewed.
        #[serde(default)]
        review: String,
    },
    /// The vendor compacted its own context.
    ContextCompaction {
        /// The item's own id.
        id: String,
    },
    /// A family this harness does not render.
    #[serde(other)]
    Other,
}

impl ThreadItem {
    /// The vendor's own id for this item, when it has one.
    ///
    /// `None` for [`ThreadItem::Other`]: the variant keeps no fields, because an id belonging to
    /// an item nothing renders is an id nothing would echo back.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::AgentMessage { id, .. }
            | Self::Reasoning { id, .. }
            | Self::CommandExecution { id, .. }
            | Self::FileChange { id, .. }
            | Self::McpToolCall { id, .. }
            | Self::WebSearch { id, .. }
            | Self::Plan { id, .. }
            | Self::SubAgentActivity { id, .. }
            | Self::EnteredReviewMode { id, .. }
            | Self::ExitedReviewMode { id, .. }
            | Self::ContextCompaction { id } => Some(id.as_str()),
            Self::Other => None,
        }
    }

    /// Whether this item's own lifecycle is the turn's text rather than an activity.
    ///
    /// The answer and the reasoning arrive twice: as deltas while they are being written, and as a
    /// whole item when they finish. Rendering both would double every sentence, so the reducer
    /// takes the deltas and drops these.
    #[must_use]
    pub fn is_streamed_text(&self) -> bool {
        matches!(self, Self::AgentMessage { .. } | Self::Reasoning { .. })
    }
}

/// Whatever JSON an item arrived as, for a fixture test that wants the raw frame.
pub type RawItem = Value;

#[cfg(test)]
mod tests {
    use super::{ItemStatus, ThreadItem};
    use serde_json::json;

    /// The real frame from a captured turn, field for field.
    #[test]
    fn a_command_execution_reads_the_frame_the_app_server_wrote() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "commandExecution",
            "id": "exec-93d3b150",
            "pluginId": null,
            "scriptPath": null,
            "command": "/bin/bash -lc 'echo mango'",
            "cwd": "/workspace",
            "processId": "73682",
            "source": "unifiedExecStartup",
            "status": "completed",
            "commandActions": [{"type": "unknown", "command": "echo mango"}],
            "aggregatedOutput": "mango\n",
            "exitCode": 0,
            "durationMs": 0
        }))
        .expect("expected a command execution");

        let ThreadItem::CommandExecution {
            id,
            command,
            status,
            aggregated_output,
            exit_code,
            ..
        } = item
        else {
            panic!("expected a command execution, received something else");
        };
        assert_eq!(id, "exec-93d3b150");
        assert_eq!(command, "/bin/bash -lc 'echo mango'");
        assert_eq!(status, ItemStatus::Completed);
        assert_eq!(aggregated_output.as_deref(), Some("mango\n"));
        assert_eq!(exit_code, Some(0));
    }

    /// A build newer than the pin sends families this one has never heard of. An item is a unit of
    /// work to render, not a contract to enforce: an unknown one is dropped, not fatal.
    #[test]
    fn an_item_family_this_harness_does_not_render_is_kept_as_other() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "somethingTheNextReleaseAdded",
            "id": "x-1",
            "whatever": {"nested": true}
        }))
        .expect("expected an unknown item to survive");
        assert_eq!(item, ThreadItem::Other);
        assert_eq!(item.id(), None);
    }

    /// Four families spell their status the same way; a fifth spelling must not fail the item.
    #[test]
    fn a_status_spelling_this_harness_does_not_know_reads_as_unknown() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "mcpToolCall", "id": "m-1", "server": "exa", "tool": "search",
            "status": "somethingElse"
        }))
        .expect("expected the item to survive an unknown status");
        let ThreadItem::McpToolCall { status, .. } = item else {
            panic!("expected an mcp tool call");
        };
        assert_eq!(status, ItemStatus::Unknown);
    }

    /// The answer arrives as deltas and again as a finished item. Rendering both doubles it.
    #[test]
    fn the_answer_and_the_reasoning_are_streamed_rather_than_rendered_as_activity() {
        let message: ThreadItem =
            serde_json::from_value(json!({"type": "agentMessage", "id": "m", "text": "hi"}))
                .expect("expected a message");
        let reasoning: ThreadItem = serde_json::from_value(
            json!({"type": "reasoning", "id": "r", "summary": ["a"], "content": []}),
        )
        .expect("expected reasoning");
        let command: ThreadItem =
            serde_json::from_value(json!({"type": "commandExecution", "id": "c", "command": "ls"}))
                .expect("expected a command");

        assert!(message.is_streamed_text());
        assert!(reasoning.is_streamed_text());
        assert!(!command.is_streamed_text());
    }

    #[test]
    fn a_file_change_reads_every_file_it_touched() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "fileChange",
            "id": "patch-1",
            "status": "completed",
            "changes": [
                {"path": "src/lib.rs", "kind": {"type": "update", "move_path": null},
                 "diff": "@@ -1 +1 @@\n-a\n+b\n"},
                {"path": "README.md", "kind": {"type": "add"}, "diff": "+hello\n"}
            ]
        }))
        .expect("expected a file change");

        let ThreadItem::FileChange { changes, .. } = item else {
            panic!("expected a file change");
        };
        assert_eq!(changes.len(), 2);
        assert_eq!(changes[0].path, "src/lib.rs");
    }
}
