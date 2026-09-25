//! A thread item: the unit the app-server streams a turn's work in.
//!
//! Nineteen variants upstream, of which a host renders a handful and the rest are either the
//! turn's own text (handled as deltas) or vendor bookkeeping. The variants here are the ones that
//! become an [`Activity`](mango_external_agents::Activity) or a piece of the answer; every other
//! item deserialises into [`ThreadItem::Other`] with its `type` and `id` kept, so the work it
//! records can still be shown — or an echo of the client's own input dropped — by name rather
//! than by silence.

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
///
/// Non-exhaustive: a family that starts mattering becomes a variant here.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
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
    /// A family this harness does not model, with the two fields every family shares.
    ///
    /// Untagged, so serde reaches it only when `type` names none of the variants above. A known
    /// family whose own fields did not decode also lands here under its known name, which
    /// [`ThreadItem::is_unmodelled_family`] tells apart.
    #[serde(untagged)]
    Other {
        /// The vendor's own name for the family, such as `imageView`.
        #[serde(rename = "type")]
        item_type: String,
        /// The item's own id, when it carried a string one.
        #[serde(default)]
        id: Option<String>,
        /// How it ended, when the family states a status in the shared spelling.
        #[serde(default)]
        status: Option<ItemStatus>,
    },
}

/// The `type` of every family [`ThreadItem`] models as its own variant.
const MODELLED_FAMILIES: &[&str] = &[
    "agentMessage",
    "reasoning",
    "commandExecution",
    "fileChange",
    "mcpToolCall",
    "webSearch",
    "plan",
    "subAgentActivity",
    "enteredReviewMode",
    "exitedReviewMode",
    "contextCompaction",
];

/// Families that echo what the client itself sent, rather than record work the agent did.
///
/// `userMessage` is the input of this turn, `hookPrompt` the user's own configuration replayed,
/// and `functionCallOutput` tool output a client supplied through `turn/start.toolOutput`.
const CLIENT_ECHO_FAMILIES: &[&str] = &["userMessage", "hookPrompt", "functionCallOutput"];

impl ThreadItem {
    /// The vendor's own id for this item, when it has one.
    ///
    /// `None` only for a [`ThreadItem::Other`] that carried no string id.
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
            Self::Other { id, .. } => id.as_deref(),
        }
    }

    /// Whether this is a family this build does not model and that records the agent's work.
    ///
    /// False for every modelled family — including one whose own fields did not decode, which is
    /// a malformed item rather than a new kind of work — and for the echoes of the client's own
    /// input. For example, an `imageView` item is unmodelled; a `userMessage` is not.
    #[must_use]
    pub fn is_unmodelled_family(&self) -> bool {
        match self {
            Self::Other { item_type, .. } => {
                !MODELLED_FAMILIES.contains(&item_type.as_str())
                    && !CLIENT_ECHO_FAMILIES.contains(&item_type.as_str())
            }
            _ => false,
        }
    }

    /// Whether this item's own lifecycle is the turn's text rather than an activity.
    ///
    /// The answer and the reasoning arrive twice: as deltas while they are being written, and as a
    /// whole item when they finish. Rendering both would double every sentence, so neither becomes
    /// an activity; a completed answer contributes only the text its deltas did not deliver (see
    /// [`crate::turn_reducer`]).
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
    /// work to render, not a contract to enforce: an unknown one keeps its name and id, not fatal.
    #[test]
    fn an_item_family_this_harness_does_not_model_is_kept_as_other_with_its_name_and_id() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "somethingTheNextReleaseAdded",
            "id": "x-1",
            "whatever": {"nested": true}
        }))
        .expect("expected an unknown item to survive");
        assert_eq!(
            item,
            ThreadItem::Other {
                item_type: String::from("somethingTheNextReleaseAdded"),
                id: Some(String::from("x-1")),
                status: None,
            }
        );
        assert_eq!(item.id(), Some("x-1"));
        assert!(item.is_unmodelled_family());
    }

    /// A modelled family whose own fields did not decode is malformed, not a new kind of work.
    #[test]
    fn a_malformed_modelled_family_is_not_mistaken_for_an_unmodelled_one() {
        let item: ThreadItem = serde_json::from_value(json!({
            "type": "commandExecution", "id": "c-1", "command": 7
        }))
        .expect("expected the item to survive as data");
        assert!(
            !item.is_unmodelled_family(),
            "expected a malformed commandExecution to stay unrendered, received {item:?}"
        );
    }

    #[test]
    fn an_echo_of_the_clients_own_input_is_not_an_unmodelled_family() {
        for item_type in ["userMessage", "hookPrompt", "functionCallOutput"] {
            let item: ThreadItem =
                serde_json::from_value(json!({"type": item_type, "id": "e-1", "content": []}))
                    .expect("expected an echo to survive");
            assert!(
                !item.is_unmodelled_family(),
                "expected {item_type} to be an echo"
            );
        }
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
