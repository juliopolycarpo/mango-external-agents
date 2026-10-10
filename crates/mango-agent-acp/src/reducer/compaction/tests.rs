use std::time::Duration;

use agent_client_protocol::schema::v1::SessionUpdate;
use serde_json::json;

use super::*;

const ID: &str = "acp:compaction:c1";

fn frame(value: serde_json::Value) -> SessionUpdate {
    serde_json::from_value(value).expect("expected a v1 session update")
}

fn update(status: &str) -> serde_json::Value {
    json!({ "sessionUpdate": "compaction_update", "compactionId": "c1", "status": status })
}

fn patch(status: &str, key: &str, value: serde_json::Value) -> serde_json::Value {
    let mut frame = update(status);
    frame[key] = value;
    frame
}

fn chunk(text: &str) -> serde_json::Value {
    json!({
        "sessionUpdate": "compaction_summary_chunk",
        "compactionId": "c1",
        "content": { "type": "text", "text": text }
    })
}

/// Every frame through a reducer that forwards each update as it arrives.
fn reduce(frames: Vec<serde_json::Value>) -> Vec<EventKind> {
    let mut reducer = Reducer::new().with_update_interval(Duration::ZERO);
    frames
        .into_iter()
        .flat_map(|value| reducer.update(frame(value)).0)
        .collect()
}

fn content(content: Option<&ActivityContent>) -> String {
    match content {
        None => String::from("-"),
        Some(ActivityContent::Empty) => String::from("empty"),
        Some(ActivityContent::Output { text }) => format!("output({text})"),
        Some(other) => format!("{other:?}"),
    }
}

/// One line per event: enough to read an expected sequence at a glance.
fn story(events: &[EventKind]) -> Vec<String> {
    events
        .iter()
        .map(|kind| match kind {
            EventKind::ActivityStarted { call_id, .. } => format!("started {call_id}"),
            EventKind::ActivityUpdated { call_id, update } => format!(
                "updated {call_id} detail={:?} content={}{}",
                update.detail,
                content(update.content.as_ref()),
                if update.truncated { " truncated" } else { "" }
            ),
            EventKind::ActivityCompleted { call_id, result } => format!(
                "completed {call_id} {:?} detail={:?} content={}{}",
                result.status,
                result.detail,
                content(result.content.as_ref()),
                if result.truncated { " truncated" } else { "" }
            ),
            other => format!("{other:?}"),
        })
        .collect()
}

#[track_caller]
fn assert_story(events: &[EventKind], expected: &[&str]) {
    let received = story(events);
    assert_eq!(
        received, expected,
        "expected events: {expected:#?} | received: {received:#?}"
    );
}

#[test]
fn a_compaction_call_id_is_namespaced_and_an_unusable_id_is_left_for_the_core_to_refuse() {
    let cases = [
        ("c1", "acp:compaction:c1"),
        ("acp:plan", "acp:compaction:acp:plan"),
        ("", ""),
        ("   ", "   "),
        ("bad\u{1b}id", "bad\u{1b}id"),
    ];
    for (raw, expected) in cases {
        let received = compaction_call_id(&CompactionId::new(raw));
        assert_eq!(
            received, expected,
            "expected call id for {raw:?}: {expected:?} | received: {received:?}"
        );
    }
    let long = "x".repeat(200);
    assert_eq!(compaction_call_id(&CompactionId::new(long.as_str())), long);
}

#[test]
fn only_the_three_terminal_statuses_end_a_compaction() {
    let cases = [
        (CompactionStatus::InProgress, None),
        (CompactionStatus::Completed, Some(ActivityStatus::Completed)),
        (CompactionStatus::Failed, Some(ActivityStatus::Failed)),
        (CompactionStatus::Cancelled, Some(ActivityStatus::Cancelled)),
        (CompactionStatus::Other(String::from("_paused")), None),
    ];
    for (status, expected) in cases {
        let received = finished(&status);
        assert_eq!(
            received, expected,
            "expected outcome of {status:?}: {expected:?} | received: {received:?}"
        );
    }
}

#[test]
fn the_opening_event_is_the_activity_the_codex_harness_uses_for_its_own_compaction() {
    let EventKind::ActivityStarted { call_id, activity } = started(ID, &CompactionId::new("c1"))
    else {
        panic!("expected an ActivityStarted");
    };
    let received = (
        call_id.as_str(),
        activity.name.as_str(),
        activity.kind,
        activity.title.as_str(),
        activity.item_id.as_deref(),
    );
    assert_eq!(
        received,
        (
            ID,
            "compact",
            ActivityKind::Compaction,
            "Compacting the conversation",
            Some("c1")
        ),
        "expected (call id, name, kind, title, item id) | received: {received:?}"
    );
}

#[test]
fn a_summary_appends_in_order_and_reports_whether_anything_changed() {
    let mut summary = Summary::default();
    assert!(summary.append("Sum"));
    assert!(summary.append("mary"));
    assert!(
        !summary.append(""),
        "expected an empty chunk to change nothing"
    );
    assert_eq!(
        summary.content(),
        ActivityContent::Output {
            text: String::from("Summary")
        }
    );
    assert!(!summary.cut);
}

#[test]
fn a_summary_stops_at_the_published_bound_and_says_so_once() {
    let bound = TextLimit::Detail.max_code_points();
    let mut summary = Summary::default();
    assert!(summary.append(&"é".repeat(bound - 1)));
    assert!(
        summary.append("abc"),
        "expected the chunk that crosses the bound to change the summary"
    );
    let received = (
        summary.text.chars().count(),
        summary.code_points,
        summary.cut,
    );
    assert_eq!(
        received,
        (bound, bound, true),
        "expected (code points, counted, cut) at the bound | received: {received:?}"
    );
    assert!(summary.text.ends_with('a'));
    assert!(
        !summary.append("more"),
        "expected a chunk past the bound to change nothing a host has not been told"
    );
    assert_eq!(summary.text.chars().count(), bound);
}

#[test]
fn a_summary_strips_what_the_core_strips_and_counts_it_as_a_cut() {
    let mut summary = Summary::default();
    assert!(summary.append("a\u{1b}b\u{202e}c"));
    assert_eq!(summary.text, "abc");
    assert_eq!(summary.code_points, 3);
    assert!(summary.cut);
}

#[test]
fn replacing_a_summary_keeps_only_the_text_blocks_and_forgets_an_earlier_cut() {
    let mut summary = Summary::default();
    summary.append("old\u{1b}");
    let blocks: Vec<ContentBlock> = serde_json::from_value(json!([
        { "type": "text", "text": "new" },
        { "type": "image", "mimeType": "image/png", "data": "AAAA" },
        { "type": "text", "text": " text" }
    ]))
    .expect("expected content blocks");
    summary.replace(blocks);
    assert_eq!(summary.text, "new text");
    assert!(!summary.cut);
}

#[test]
fn clearing_a_summary_reports_whether_there_was_anything_to_clear() {
    let mut summary = Summary::default();
    assert!(!summary.clear());
    summary.append("text");
    assert!(summary.clear());
    assert_eq!(summary.content(), ActivityContent::Empty);
    assert_eq!(summary.code_points, 0);
}

#[test]
fn forgetting_every_compaction_leaves_none() {
    let mut compactions = Compactions::default();
    compactions
        .open
        .insert(String::from(ID), Compaction::default());
    compactions.clear();
    assert!(compactions.open.is_empty());
}

#[test]
fn the_first_update_opens_the_bracket_and_a_terminal_status_closes_it() {
    assert_story(
        &reduce(vec![update("in_progress"), update("completed")]),
        &[
            "started acp:compaction:c1",
            "completed acp:compaction:c1 Completed detail=None content=-",
        ],
    );
}

#[test]
fn a_compaction_first_seen_already_over_is_started_and_completed_at_once() {
    assert_story(
        &reduce(vec![patch(
            "failed",
            "error",
            json!("the summariser ran out of context"),
        )]),
        &[
            "started acp:compaction:c1",
            "completed acp:compaction:c1 Failed detail=Some(\"the summariser ran out of context\") content=-",
        ],
    );
}

#[test]
fn a_chunk_for_an_unseen_id_opens_an_in_progress_compaction() {
    assert_story(
        &reduce(vec![chunk("first")]),
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(first)",
        ],
    );
}

#[test]
fn each_chunk_carries_the_whole_summary_so_far() {
    assert_story(
        &reduce(vec![chunk("a"), chunk("b"), chunk("c")]),
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(a)",
            "updated acp:compaction:c1 detail=None content=output(ab)",
            "updated acp:compaction:c1 detail=None content=output(abc)",
        ],
    );
}

#[test]
fn a_non_text_chunk_opens_the_compaction_and_adds_nothing_to_its_summary() {
    let image = json!({
        "sessionUpdate": "compaction_summary_chunk",
        "compactionId": "c1",
        "content": { "type": "image", "mimeType": "image/png", "data": "AAAA" }
    });
    assert_story(
        &reduce(vec![image.clone(), chunk("text"), image]),
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(text)",
        ],
    );
}

#[test]
fn a_summary_patch_keeps_replaces_or_clears() {
    let events = reduce(vec![
        chunk("draft"),
        // Omitted: nothing about the summary changes, so nothing is sent.
        update("in_progress"),
        patch(
            "in_progress",
            "summary",
            json!([{ "type": "text", "text": "final" }]),
        ),
        chunk(" words"),
        patch("in_progress", "summary", json!(null)),
        chunk("again"),
        patch("in_progress", "summary", json!([])),
        // Already empty: clearing it again is not a change.
        patch("in_progress", "summary", json!(null)),
    ]);
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(draft)",
            "updated acp:compaction:c1 detail=None content=output(final)",
            "updated acp:compaction:c1 detail=None content=output(final words)",
            "updated acp:compaction:c1 detail=None content=empty",
            "updated acp:compaction:c1 detail=None content=output(again)",
            "updated acp:compaction:c1 detail=None content=empty",
        ],
    );
}

#[test]
fn a_replacement_with_no_text_block_clears_the_summary() {
    let events = reduce(vec![
        chunk("draft"),
        patch(
            "in_progress",
            "summary",
            json!([{ "type": "image", "mimeType": "image/png", "data": "AAAA" }]),
        ),
    ]);
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(draft)",
            "updated acp:compaction:c1 detail=None content=empty",
        ],
    );
}

#[test]
fn a_summary_cleared_on_first_sight_has_nothing_to_clear() {
    assert_story(
        &reduce(vec![patch("in_progress", "summary", json!(null))]),
        &["started acp:compaction:c1"],
    );
}

#[test]
fn an_error_patch_is_the_activitys_detail_and_survives_to_the_completion() {
    let events = reduce(vec![
        patch("in_progress", "error", json!("retrying")),
        update("in_progress"),
        update("failed"),
    ]);
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=Some(\"retrying\") content=-",
            "completed acp:compaction:c1 Failed detail=Some(\"retrying\") content=-",
        ],
    );
}

#[test]
fn a_cleared_error_empties_the_detail_and_is_gone_at_the_completion() {
    let events = reduce(vec![
        patch("in_progress", "error", json!("retrying")),
        patch("in_progress", "error", json!(null)),
        // Nothing left to clear.
        patch("in_progress", "error", json!(null)),
        update("completed"),
    ]);
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=Some(\"retrying\") content=-",
            "updated acp:compaction:c1 detail=Some(\"\") content=-",
            "completed acp:compaction:c1 Completed detail=None content=-",
        ],
    );
}

#[test]
fn a_completion_that_patches_the_summary_carries_it_as_the_result() {
    let events = reduce(vec![
        chunk("draft"),
        patch(
            "completed",
            "summary",
            json!([{ "type": "text", "text": "final" }]),
        ),
    ]);
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(draft)",
            "completed acp:compaction:c1 Completed detail=None content=output(final)",
        ],
    );
}

#[test]
fn an_unknown_status_is_still_running() {
    let events = reduce(vec![
        update("_vendor_paused"),
        chunk("text"),
        update("some_future_status"),
    ]);
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(text)",
        ],
    );
}

#[test]
fn frames_for_a_compaction_that_ended_are_dropped() {
    let events = reduce(vec![
        update("cancelled"),
        chunk("late"),
        update("in_progress"),
        update("completed"),
    ]);
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "completed acp:compaction:c1 Cancelled detail=None content=-",
        ],
    );
}

#[test]
fn chunks_inside_the_interval_are_held_and_delivered_ahead_of_the_completion() {
    let mut reducer = Reducer::new();
    let start = Instant::now();
    let mut events = Vec::new();
    for (offset, value) in [
        (0, chunk("a")),
        (1, chunk("b")),
        (2, chunk("c")),
        (3, update("completed")),
    ] {
        let at = start + Duration::from_secs(offset);
        events.extend(reducer.update_at(frame(value), at).0);
    }
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(a)",
            "updated acp:compaction:c1 detail=None content=output(abc)",
            "completed acp:compaction:c1 Completed detail=None content=-",
        ],
    );
}

#[test]
fn a_chunk_after_the_interval_carries_what_was_held() {
    let mut reducer = Reducer::new();
    let start = Instant::now();
    let mut events = Vec::new();
    for (offset, value) in [(0, chunk("a")), (1, chunk("b")), (6, chunk("c"))] {
        let at = start + Duration::from_secs(offset);
        events.extend(reducer.update_at(frame(value), at).0);
    }
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(a)",
            "updated acp:compaction:c1 detail=None content=output(abc)",
        ],
    );
}

#[test]
fn a_completion_that_replaces_the_summary_supersedes_the_held_one() {
    let mut reducer = Reducer::new();
    let now = Instant::now();
    let mut events = Vec::new();
    for value in [
        chunk("a"),
        chunk("b"),
        patch(
            "completed",
            "summary",
            json!([{ "type": "text", "text": "final" }]),
        ),
    ] {
        events.extend(reducer.update_at(frame(value), now).0);
    }
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(a)",
            "completed acp:compaction:c1 Completed detail=None content=output(final)",
        ],
    );
}

#[test]
fn a_compaction_left_running_ends_with_the_turn_and_takes_its_status() {
    let mut reducer = Reducer::new();
    let now = Instant::now();
    let mut events = Vec::new();
    for value in [chunk("a"), chunk("b")] {
        events.extend(reducer.update_at(frame(value), now).0);
    }
    events.extend(reducer.finish_with(ActivityStatus::Cancelled));
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=None content=output(a)",
            "updated acp:compaction:c1 detail=None content=output(ab)",
            "completed acp:compaction:c1 Cancelled detail=None content=-",
        ],
    );
    assert!(
        reducer.compactions.open.is_empty() && reducer.open_calls_len() == 0,
        "expected the turn's end to leave no compaction behind"
    );
    assert!(
        reducer.update(frame(chunk("late"))).0.is_empty(),
        "expected a frame after the close to be dropped"
    );
}

#[test]
fn a_long_summary_is_held_at_the_published_size_and_reported_truncated() {
    let bound = TextLimit::Detail.max_code_points();
    let mut reducer = Reducer::new().with_update_interval(Duration::ZERO);
    let mut last = Vec::new();
    for _ in 0..40 {
        let events = reducer.update(frame(chunk(&"s".repeat(1_000)))).0;
        if !events.is_empty() {
            last = events;
        }
    }
    let held: usize = reducer
        .compactions
        .open
        .values()
        .map(|compaction| compaction.summary.text.len())
        .sum();
    assert_eq!(
        held, bound,
        "expected bytes of summary held after 40 KB of chunks: {bound} | received: {held}"
    );
    let Some(EventKind::ActivityUpdated { update, .. }) = last.last() else {
        panic!("expected the last change to be an update | received: {last:?}");
    };
    let normalized = update.clone().normalized();
    assert!(
        matches!(&normalized.content, Some(ActivityContent::Output { text }) if text.len() == bound),
        "expected the published summary at the bound"
    );
    assert!(
        normalized.truncated,
        "expected the cut to survive the core's own bounding"
    );
}

#[test]
fn an_over_long_error_is_bounded_and_reported_truncated() {
    let events = reduce(vec![patch("failed", "error", json!("e".repeat(5_000)))]);
    let Some(EventKind::ActivityCompleted { result, .. }) = events.last() else {
        panic!("expected a completion | received: {events:?}");
    };
    let received = (result.detail.as_ref().map(String::len), result.truncated);
    assert_eq!(
        received,
        (Some(TextLimit::Detail.max_code_points()), true),
        "expected (detail bytes, truncated) | received: {received:?}"
    );
}

#[test]
fn an_id_the_core_refuses_is_announced_once_and_then_remembered_as_ended() {
    for id in ["   ".to_owned(), "x".repeat(200), "x".repeat(120)] {
        let mut reducer = Reducer::new();
        let first = json!({
            "sessionUpdate": "compaction_update", "compactionId": id, "status": "in_progress"
        });
        let events = reducer.update(frame(first.clone())).0;
        let [started @ EventKind::ActivityStarted { .. }] = events.as_slice() else {
            panic!("expected one ActivityStarted for the core to refuse | received: {events:?}");
        };
        assert!(
            started.clone().normalized().is_err(),
            "expected the core to refuse the event for a {}-byte id",
            id.len()
        );
        assert_eq!(reducer.open_calls_len(), 0);
        assert!(reducer.compactions.open.is_empty());
        assert!(
            reducer.update(frame(first)).0.is_empty(),
            "expected a later frame for the refused id to be dropped"
        );
    }
}

#[test]
fn a_compaction_and_a_tool_call_sharing_an_id_are_two_activities() {
    let events = reduce(vec![
        json!({
            "sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Run",
            "status": "in_progress"
        }),
        update("in_progress"),
        update("completed"),
        json!({ "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "failed" }),
    ]);
    let received: Vec<String> = story(&events)
        .into_iter()
        .map(|line| line.split(" detail=").next().unwrap_or(&line).to_owned())
        .collect();
    assert_eq!(
        received,
        [
            "started c1",
            "started acp:compaction:c1",
            "completed acp:compaction:c1 Completed",
            "completed c1 Failed",
        ],
        "expected two brackets under two ids | received: {received:#?}"
    );
}

#[test]
fn a_compaction_closes_an_open_reasoning_block_like_any_activity() {
    let events = reduce(vec![
        json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "thinking" }
        }),
        update("in_progress"),
    ]);
    assert!(
        matches!(
            events.as_slice(),
            [
                EventKind::ReasoningStarted,
                EventKind::ReasoningDelta { .. },
                EventKind::ReasoningEnded,
                EventKind::ActivityStarted { .. }
            ]
        ),
        "expected the reasoning block closed ahead of the compaction | received: {events:?}"
    );
}

#[test]
fn a_change_with_nothing_in_it_is_not_sent() {
    let mut reducer = Reducer::new();
    let _ = reducer.update(frame(update("in_progress")));
    let events = reducer.compaction_changed(String::from(ID), Held::default(), Instant::now());
    assert!(events.is_empty(), "received: {events:?}");
}

#[test]
fn closing_a_compaction_delivers_what_is_held_and_forgets_it() {
    let mut reducer = Reducer::new();
    let now = Instant::now();
    for value in [chunk("a"), chunk("b")] {
        let _ = reducer.update_at(frame(value), now);
    }
    let events = reducer.close_compaction(
        String::from(ID),
        ActivityResult::new(ActivityStatus::Failed),
    );
    assert_story(
        &events,
        &[
            "updated acp:compaction:c1 detail=None content=output(ab)",
            "completed acp:compaction:c1 Failed detail=None content=-",
        ],
    );
    assert!(reducer.is_finished(ID));
    assert!(reducer.compactions.open.is_empty());
}

#[test]
fn opening_a_compaction_announces_it_once() {
    let mut reducer = Reducer::new();
    let id = CompactionId::new("c1");
    let first = reducer.open_compaction(ID, &id);
    assert!(
        matches!(first.as_deref(), Some([EventKind::ActivityStarted { .. }])),
        "expected the first sighting to be announced | received: {first:?}"
    );
    let second = reducer.open_compaction(ID, &id);
    assert_eq!(
        second,
        Some(Vec::new()),
        "expected a running compaction to need no announcement"
    );
    reducer.open_calls.remove(ID);
    reducer.mark_finished(ID);
    assert_eq!(
        reducer.open_compaction(ID, &id),
        None,
        "expected an ended compaction to take no more frames"
    );
}

/// The completion is the last word on the detail: an error cleared in the terminal frame must not
/// leave an earlier one on a host's screen, and a held one must not be delivered ahead of it.
#[test]
fn an_error_cleared_by_the_terminal_frame_empties_the_detail() {
    let mut reducer = Reducer::new();
    let now = Instant::now();
    let mut events = Vec::new();
    for value in [
        patch("in_progress", "error", json!("x")),
        patch("in_progress", "error", json!("y")),
        patch("completed", "error", json!(null)),
    ] {
        events.extend(reducer.update_at(frame(value), now).0);
    }
    assert_story(
        &events,
        &[
            "started acp:compaction:c1",
            "updated acp:compaction:c1 detail=Some(\"x\") content=-",
            "completed acp:compaction:c1 Completed detail=Some(\"\") content=-",
        ],
    );
}

#[test]
fn the_reducers_debug_output_names_no_summary_or_error_text() {
    let mut reducer = Reducer::new();
    let _ = reducer.update(frame(chunk("summary-secret")));
    let _ = reducer.update(frame(patch("in_progress", "error", json!("error-secret"))));
    let rendered = format!("{reducer:?}");
    for secret in ["summary-secret", "error-secret"] {
        assert!(
            !rendered.contains(secret),
            "expected no compaction text in debug output | received: {rendered}"
        );
    }
}
