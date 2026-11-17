//! Every activity the reducer opens from a captured app-server stream is closed under the id it was
//! opened with.
//!
//! The terminal closes whatever the host still holds open, and the open-activity bound counts what
//! is left open, so both rest on a completion naming the id its start did. This replays the
//! recorded frames through the reducer for the item kinds the repository has captured. Kinds with
//! no capture (plan, web search, file change, MCP tool call, context compaction) are not covered
//! here: for those the pairing rests on `activity::started` and `activity::completed` both reading
//! the item's own `id`, which is code reading, not a recording.

use std::time::SystemTime;

use mango_agent_codex::protocol::Notification;
use mango_agent_codex::reducer::{Outcome, reduce};
use mango_external_agents::EventKind;
use serde_json::Value;

use crate::support::Transcript;

/// The ids the reducer opened and closed over one fixture, each in the order it saw them.
fn opened_and_closed(fixture: &str) -> (Vec<String>, Vec<String>) {
    let transcript = Transcript::load(fixture);
    let thread = transcript
        .thread_id()
        .unwrap_or_else(|| panic!("expected fixture {fixture} to name its thread"));
    let (mut opened, mut closed) = (Vec::new(), Vec::new());
    for frame in transcript.every_received() {
        let (Some(method), Some(params)) = (
            frame.get("method").and_then(Value::as_str),
            frame.get("params"),
        ) else {
            continue;
        };
        let notification = Notification::parse(method, params.clone());
        let Outcome::Emit(events) = reduce(&notification, &thread, SystemTime::UNIX_EPOCH) else {
            continue;
        };
        for event in events {
            match event {
                EventKind::ActivityStarted { call_id, .. } => opened.push(call_id),
                EventKind::ActivityCompleted { call_id, .. } => closed.push(call_id),
                _ => {}
            }
        }
    }
    (opened, closed)
}

/// Asserts `fixture` opened at least `minimum` activities and closed each under its own id.
fn assert_every_open_activity_closes(fixture: &str, minimum: usize) {
    let (opened, closed) = opened_and_closed(fixture);
    assert!(
        opened.len() >= minimum,
        "expected fixture {fixture} to open at least {minimum} activities | received {opened:?}"
    );
    let unclosed: Vec<&String> = opened.iter().filter(|id| !closed.contains(id)).collect();
    assert!(
        unclosed.is_empty(),
        "expected every activity opened in fixture {fixture} to close under its own id | received opened {opened:?}, closed {closed:?}, never closed {unclosed:?}"
    );
}

#[test]
fn a_captured_command_turn_closes_each_command_under_its_own_id() {
    assert_every_open_activity_closes("turn", 1);
}

#[test]
fn a_captured_approval_turn_closes_each_command_under_its_own_id() {
    assert_every_open_activity_closes("approval", 1);
}

#[test]
fn a_captured_subagent_turn_closes_each_activity_under_its_own_id() {
    assert_every_open_activity_closes("subagent", 1);
}

#[test]
fn a_captured_review_closes_each_activity_under_its_own_id() {
    assert_every_open_activity_closes("review", 1);
}
