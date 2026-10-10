//! What a host receives for a tool call that carries its own `name`, through the public session.
//!
//! The reducer's unit tests pin the mapping; these pin what is published, after the core has
//! bounded the event. ACP v1 reference: <https://agentclientprotocol.com/protocol/tool-calls>

use super::*;

/// The name and title of every activity the turn started, in order, with whether each was cut.
fn started_activities(events: &[EventKind]) -> Vec<(&str, &str, bool)> {
    events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::ActivityStarted { activity, .. } => Some((
                activity.name.as_str(),
                activity.title.as_str(),
                activity.truncated,
            )),
            _ => None,
        })
        .collect()
}

/// The title of every activity update the turn delivered, in order.
fn updated_titles(events: &[EventKind]) -> Vec<Option<&str>> {
    events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::ActivityUpdated { update, .. } => Some(update.title.as_deref()),
            _ => None,
        })
        .collect()
}

async fn events_of(updates: Vec<serde_json::Value>) -> Vec<EventKind> {
    let (session, _launcher) = open(FakeAcpAgent::new().with_updates(updates), permissive()).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    drain(&mut turn).await
}

/// One call that names its tool, one that does not, and one first seen through an update: the name
/// is the activity's name where the agent sent one, the title is the title in every case, and a
/// name that arrives for a call already running renames nothing.
#[tokio::test]
async fn a_host_sees_a_tool_calls_own_name_beside_its_title() {
    let events = events_of(vec![
        serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_named",
            "title": "Read src/lib.rs",
            "name": "read_file",
            "kind": "read",
            "status": "pending"
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "call_unnamed",
            "title": "Run `cargo test`",
            "name": null,
            "kind": "execute",
            "status": "pending"
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_late",
            "title": "Search for `fn main`",
            "name": "grep",
            "status": "in_progress"
        }),
        serde_json::json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "call_unnamed",
            "name": "shell"
        }),
    ])
    .await;

    let started = started_activities(&events);
    assert_eq!(
        started,
        [
            ("read_file", "Read src/lib.rs", false),
            ("Run `cargo test`", "Run `cargo test`", false),
            ("grep", "Search for `fn main`", false),
        ],
        "expected started (name, title, truncated): read_file, the unnamed call's title, grep | \
         received: {started:?}"
    );
    let updates = updated_titles(&events);
    assert!(
        updates.is_empty(),
        "expected no activity update for a name sent after the start | received titles: {updates:?}"
    );
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the turn to complete | received: {:?}",
        events.last()
    );
}

/// A name is agent-controlled text. What is published is stripped of terminal controls and
/// overrides and cut to the core's activity-name bound, with the cut reported, and the turn still
/// completes: text is bounded, never refused.
#[tokio::test]
async fn a_host_never_sees_a_hostile_tool_name_unbounded() {
    let hostile = format!(
        "\u{1b}[2J\u{1b}]0;owned\u{7}rm\u{0} -rf\u{202e}{}",
        "x".repeat(50_000)
    );
    let events = events_of(vec![serde_json::json!({
        "sessionUpdate": "tool_call",
        "toolCallId": "call_hostile",
        "title": "Delete",
        "name": hostile,
        "kind": "delete",
        "status": "pending"
    })])
    .await;

    let started = started_activities(&events);
    let [(name, title, truncated)] = started.as_slice() else {
        panic!("expected one started activity | received: {started:?}");
    };
    let expected = format!("[2J]0;ownedrm -rf{}", "x".repeat(128 - 17));
    assert_eq!(
        *name, expected,
        "expected published name: 128 code points with every control character stripped | \
         received: {name:?}"
    );
    assert_eq!(
        (*title, *truncated),
        ("Delete", true),
        "expected title \"Delete\" and truncated: true | received: title {title:?}, truncated \
         {truncated}"
    );
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the turn to complete | received: {:?}",
        events.last()
    );
}
