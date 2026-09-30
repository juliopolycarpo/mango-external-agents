//! `Debug` for the Codex records decoded from the app-server's wire, and for the reducer that holds
//! them, reports metadata and never what the agent wrote or read.
//!
//! A `tool_result`-shaped body, such as command output that `cat`ed a `.env`, arrives in these
//! records. A derive would print it the moment a host debug-logs a notification, an item or a
//! reducer, so each is given a canary in every content field and formatted both ways a logger might.

use std::fmt::Debug;

use mango_agent_codex::protocol::items::{FileUpdateChange, ThreadItem};
use mango_agent_codex::protocol::notifications::Notification;
use mango_agent_codex::protocol::requests::TurnError;
use mango_agent_codex::turn_reducer::TurnReducer;
use serde_json::json;

/// Fails, naming the type and the field, when either debug form carries the canary.
fn assert_omits(type_name: &str, value: &dyn Debug, canaries: &[(&str, &str)]) {
    let compact = format!("{value:?}");
    let pretty = format!("{value:#?}");
    for (field, canary) in canaries {
        assert!(
            !compact.contains(canary),
            "expected {type_name} {{:?}} to omit the {field} canary {canary:?} | received: {compact}"
        );
        assert!(
            !pretty.contains(canary),
            "expected {type_name} {{:#?}} to omit the {field} canary {canary:?} | received: {pretty}"
        );
    }
}

/// The debug form still says what kind of thing it is, so a log line is worth reading.
fn assert_names(type_name: &str, value: &dyn Debug, expected: &str) {
    let compact = format!("{value:?}");
    assert!(
        compact.contains(expected),
        "expected {type_name} {{:?}} to name {expected:?} | received: {compact}"
    );
}

fn item(item: serde_json::Value) -> Notification {
    Notification::parse(
        "item/completed",
        json!({"threadId": "CANARY-thread", "turnId": "CANARY-turn", "item": item}),
    )
}

#[test]
fn a_completed_agent_message_does_not_print_its_text() {
    let notification =
        item(json!({"type": "agentMessage", "id": "CANARY-id", "text": "CANARY-text"}));
    let canaries = [
        ("thread_id", "CANARY-thread"),
        ("turn_id", "CANARY-turn"),
        ("item.id", "CANARY-id"),
        ("item.text", "CANARY-text"),
    ];
    assert_omits("Notification", &notification, &canaries);
    assert_names("Notification", &notification, "ItemCompleted");
    let Notification::ItemCompleted(inner) = notification else {
        panic!("expected an item notification");
    };
    assert_omits("ItemNotification", &inner, &canaries);
    assert_names("ThreadItem", &inner.item, "AgentMessage");
}

#[test]
fn every_item_family_hides_what_it_carries() {
    let items = [
        (
            "reasoning",
            json!({"type": "reasoning", "id": "CANARY-id",
                   "summary": ["CANARY-summary"], "content": ["CANARY-content"]}),
            vec![("summary", "CANARY-summary"), ("content", "CANARY-content")],
        ),
        (
            "commandExecution",
            json!({"type": "commandExecution", "id": "CANARY-id", "command": "CANARY-command",
                   "cwd": "CANARY-cwd", "aggregatedOutput": "CANARY-output", "status": "completed"}),
            vec![
                ("command", "CANARY-command"),
                ("cwd", "CANARY-cwd"),
                ("aggregated_output", "CANARY-output"),
            ],
        ),
        (
            "fileChange",
            json!({"type": "fileChange", "id": "CANARY-id",
                   "changes": [{"path": "CANARY-path", "diff": "CANARY-diff"}]}),
            vec![
                ("changes.path", "CANARY-path"),
                ("changes.diff", "CANARY-diff"),
            ],
        ),
        (
            "mcpToolCall",
            json!({"type": "mcpToolCall", "id": "CANARY-id", "server": "CANARY-server",
                   "tool": "CANARY-tool"}),
            vec![("server", "CANARY-server"), ("tool", "CANARY-tool")],
        ),
        (
            "webSearch",
            json!({"type": "webSearch", "id": "CANARY-id", "query": "CANARY-query"}),
            vec![("query", "CANARY-query")],
        ),
        (
            "plan",
            json!({"type": "plan", "id": "CANARY-id", "text": "CANARY-plan"}),
            vec![("text", "CANARY-plan")],
        ),
        (
            "subAgentActivity",
            json!({"type": "subAgentActivity", "id": "CANARY-id", "kind": "CANARY-kind"}),
            vec![("kind", "CANARY-kind")],
        ),
        (
            "enteredReviewMode",
            json!({"type": "enteredReviewMode", "id": "CANARY-id", "review": "CANARY-review"}),
            vec![("review", "CANARY-review")],
        ),
        (
            "exitedReviewMode",
            json!({"type": "exitedReviewMode", "id": "CANARY-id", "review": "CANARY-review"}),
            vec![("review", "CANARY-review")],
        ),
        (
            "other",
            json!({"type": "CANARY-family", "id": "CANARY-id"}),
            vec![("item_type", "CANARY-family")],
        ),
    ];
    for (family, raw, mut canaries) in items {
        canaries.push(("id", "CANARY-id"));
        let notification = item(raw);
        let Notification::ItemCompleted(inner) = &notification else {
            panic!(
                "expected {family} to decode as an item notification | received: {notification:?}"
            );
        };
        assert_omits(&format!("ThreadItem::{family}"), &inner.item, &canaries);
        assert_omits(&format!("Notification({family})"), &notification, &canaries);
    }
}

#[test]
fn a_thread_item_still_names_its_family() {
    let notification = item(
        json!({"type": "commandExecution", "id": "i", "command": "c",
                                   "status": "completed", "exitCode": 0}),
    );
    let Notification::ItemCompleted(inner) = notification else {
        panic!("expected an item notification");
    };
    assert_names("ThreadItem", &inner.item, "CommandExecution");
    assert_names("ThreadItem", &inner.item, "Completed");
    assert_names("ThreadItem", &inner.item, "exit_code");
}

#[test]
fn streamed_deltas_do_not_print_their_text() {
    let cases = [
        (
            "item/agentMessage/delta",
            json!({"threadId": "CANARY-thread", "turnId": "CANARY-turn",
                   "itemId": "CANARY-item", "delta": "CANARY-delta"}),
            "AgentMessageDelta",
        ),
        (
            "item/reasoning/textDelta",
            json!({"threadId": "CANARY-thread", "turnId": "CANARY-turn",
                   "itemId": "CANARY-item", "delta": "CANARY-delta"}),
            "ReasoningDelta",
        ),
        (
            "item/commandExecution/outputDelta",
            json!({"threadId": "CANARY-thread", "turnId": "CANARY-turn",
                   "itemId": "CANARY-item", "delta": "CANARY-delta"}),
            "CommandOutputDelta",
        ),
        (
            "item/mcpToolCall/progress",
            json!({"threadId": "CANARY-thread", "turnId": "CANARY-turn",
                   "itemId": "CANARY-item", "message": "CANARY-delta"}),
            "McpToolCallProgress",
        ),
        (
            "item/fileChange/patchUpdated",
            json!({"threadId": "CANARY-thread", "turnId": "CANARY-turn", "itemId": "CANARY-item",
                   "changes": [{"path": "CANARY-path", "diff": "CANARY-delta"}]}),
            "FileChangePatchUpdated",
        ),
    ];
    for (method, params, variant) in cases {
        let notification = Notification::parse(method, params);
        assert_omits(
            &format!("Notification({variant})"),
            &notification,
            &[
                ("thread_id", "CANARY-thread"),
                ("turn_id", "CANARY-turn"),
                ("item_id", "CANARY-item"),
                ("delta/message/diff", "CANARY-delta"),
                ("path", "CANARY-path"),
            ],
        );
        assert_names("Notification", &notification, variant);
    }
}

#[test]
fn a_file_update_does_not_print_its_path_or_diff() {
    let change = FileUpdateChange {
        path: String::from("CANARY-path"),
        diff: String::from("CANARY-diff"),
    };
    assert_omits(
        "FileUpdateChange",
        &change,
        &[("path", "CANARY-path"), ("diff", "CANARY-diff")],
    );
    let _ = ThreadItem::ContextCompaction {
        id: String::from("x"),
    };
}

#[test]
fn a_turn_error_does_not_print_what_the_vendor_wrote() {
    let error: TurnError = serde_json::from_value(json!({
        "message": "CANARY-message", "additionalDetails": "CANARY-details",
        "codexErrorInfo": {"CANARY-info": {"httpStatusCode": 502}},
    }))
    .expect("expected a turn error to decode");
    let canaries = [
        ("message", "CANARY-message"),
        ("additional_details", "CANARY-details"),
        ("codex_error_info", "CANARY-info"),
    ];
    assert_omits("TurnError", &error, &canaries);

    // The error reaches a host inside a completed turn and inside an error notification.
    let completed = Notification::parse(
        "turn/completed",
        json!({"threadId": "t", "turn": {"id": "u", "status": "failed", "error": {
            "message": "CANARY-message", "additionalDetails": "CANARY-details"}}}),
    );
    assert_omits("Notification(TurnCompleted)", &completed, &canaries);
    let errored = Notification::parse(
        "error",
        json!({"threadId": "t", "turnId": "u", "willRetry": false,
               "error": {"message": "CANARY-message", "additionalDetails": "CANARY-details"}}),
    );
    assert_omits("Notification(Error)", &errored, &canaries);
    assert_names("Notification", &errored, "Error");
}

/// The reducer holds streamed answer text and each open activity's latest output until the item
/// completes, so a debug-logged reducer is a debug-logged transcript.
#[test]
fn a_reducer_does_not_print_the_text_it_is_holding() {
    let mut turn = TurnReducer::new();
    let started = std::time::SystemTime::UNIX_EPOCH;
    let now = tokio::time::Instant::now();
    for (method, params) in [
        (
            "item/started",
            json!({"threadId": "t", "turnId": "u", "item": {
                "type": "commandExecution", "id": "CANARY-call", "command": "CANARY-command",
                "status": "inProgress"}}),
        ),
        (
            "item/commandExecution/outputDelta",
            json!({"threadId": "t", "turnId": "u", "itemId": "CANARY-call", "delta": "CANARY-output"}),
        ),
        (
            "item/agentMessage/delta",
            json!({"threadId": "t", "turnId": "u", "itemId": "CANARY-message", "delta": "CANARY-answer"}),
        ),
        (
            "error",
            json!({"threadId": "t", "turnId": "u", "willRetry": false, "error": {
                "message": "CANARY-failure", "codexErrorInfo": "CANARY-code"}}),
        ),
    ] {
        let notification = Notification::parse(method, params);
        let _ = turn.reduce(&notification, "t", Some("u"), started, now);
    }
    assert_omits(
        "TurnReducer",
        &turn,
        &[
            ("open activity call id", "CANARY-call"),
            ("activity tail", "CANARY-output"),
            ("streamed message id", "CANARY-message"),
            ("streamed message text", "CANARY-answer"),
            ("reported code", "CANARY-code"),
        ],
    );
    assert_names("TurnReducer", &turn, "TurnReducer");
}

/// A reducer's outcome carries events, whose own `Debug` is metadata-only in core. This guards that
/// the transitive form stays safe, so `Outcome` needs no impl of its own.
#[test]
fn an_outcome_carrying_the_agents_text_does_not_print_it() {
    let notification = item(json!({"type": "agentMessage", "id": "i", "text": "CANARY-answer"}));
    let outcome = mango_agent_codex::reducer::reduce(
        &notification,
        "CANARY-thread",
        std::time::SystemTime::UNIX_EPOCH,
    );
    assert_omits(
        "Outcome",
        &outcome,
        &[("agent message text", "CANARY-answer")],
    );
}

/// The harness holds an executable path and discovery holds what was probed; both go through core
/// types whose `Debug` is already metadata-only, so neither needs an impl here.
#[test]
fn a_harness_does_not_print_the_executable_it_was_pointed_at() {
    let harness = mango_agent_codex::CodexHarness::new().with_executable(
        mango_external_agents::ExecutablePath::resolved("/home/CANARY-user/bin/codex"),
    );
    assert_omits("CodexHarness", &harness, &[("executable", "CANARY-user")]);
}
