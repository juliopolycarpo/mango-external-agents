//! Session notices and context compaction, from the wire to the host's turn stream.
//!
//! Both became stable in ACP v1 with schema 1.11 and are sent only to a client that advertised
//! them. Protocol: <https://agentclientprotocol.com/rfds/session-notices> and
//! <https://agentclientprotocol.com/rfds/session-compaction>.

use super::*;

use mango_external_agents::{ActivityContent, ActivityKind, ActivityStatus, NoticeSeverity};

fn text(text: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionUpdate": "agent_message_chunk",
        "content": { "type": "text", "text": text }
    })
}

fn notice(severity: &str, title: &str) -> serde_json::Value {
    serde_json::json!({ "sessionUpdate": "notice", "severity": severity, "title": title })
}

fn compaction(id: &str, status: &str) -> serde_json::Value {
    serde_json::json!({ "sessionUpdate": "compaction_update", "compactionId": id, "status": status })
}

fn chunk(id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionUpdate": "compaction_summary_chunk",
        "compactionId": id,
        "content": { "type": "text", "text": text }
    })
}

/// `frame` with `key` set to `value`.
fn with(mut frame: serde_json::Value, key: &str, value: serde_json::Value) -> serde_json::Value {
    frame[key] = value;
    frame
}

fn content(content: Option<&ActivityContent>) -> String {
    match content {
        None => String::from("-"),
        Some(ActivityContent::Empty) => String::from("empty"),
        Some(ActivityContent::Output { text }) => format!("output({text})"),
        Some(other) => format!("{other:?}"),
    }
}

/// The events these tests are about, one readable line each, in the order the host received them.
///
/// Turn bookkeeping (the turn's start, usage) is left out so an expected list is the wire's own
/// story. The terminal is kept: where it falls is part of what is asserted.
fn story(events: &[EventKind]) -> Vec<String> {
    events
        .iter()
        .filter_map(|kind| match kind {
            EventKind::TextDelta { text } => Some(format!("text({text})")),
            EventKind::Notice {
                severity,
                title,
                description,
            } => Some(format!(
                "notice {} {title:?} {}",
                serde_json::to_value(severity).expect("expected a writable severity"),
                description.as_deref().unwrap_or("-")
            )),
            EventKind::ActivityStarted { call_id, activity } => Some(format!(
                "started {call_id} name={} kind={:?} title={:?} item={}",
                activity.name,
                activity.kind,
                activity.title,
                activity.item_id.as_deref().unwrap_or("-")
            )),
            EventKind::ActivityUpdated { call_id, update } => Some(format!(
                "updated {call_id} detail={} content={}{}",
                update.detail.as_deref().unwrap_or("-"),
                content(update.content.as_ref()),
                if update.truncated { " truncated" } else { "" }
            )),
            EventKind::ActivityCompleted { call_id, result } => Some(format!(
                "completed {call_id} {:?} detail={} content={}{}",
                result.status,
                result.detail.as_deref().unwrap_or("-"),
                content(result.content.as_ref()),
                if result.truncated { " truncated" } else { "" }
            )),
            EventKind::Completed => Some(String::from("turn completed")),
            EventKind::Error { error } => Some(format!("turn failed {}", error.code.as_str())),
            _ => None,
        })
        .collect()
}

/// One turn against an agent that streams `updates`, read to its terminal.
async fn turn_story(updates: Vec<serde_json::Value>) -> Vec<String> {
    let (session, _launcher) = open(FakeAcpAgent::new().with_updates(updates), permissive()).await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    session.close(CloseReason::Requested).await.expect("close");
    story(&events)
}

#[track_caller]
fn assert_story(received: &[String], expected: &[&str]) {
    assert_eq!(
        received, expected,
        "expected host events: {expected:#?} | received: {received:#?}"
    );
}

const STARTED_C1: &str = "started acp:compaction:c1 name=compact kind=Compaction title=\"Compacting the conversation\" item=c1";

/// A client that wants notices and compaction updates has to say so: an agent may send either only
/// to a client that advertised it.
#[tokio::test]
async fn the_handshake_advertises_notices_and_compaction() {
    let (session, launcher) = open(FakeAcpAgent::new(), permissive()).await;
    let initialize = launcher
        .written()
        .into_iter()
        .find(|line| line.contains("\"initialize\""))
        .expect("expected an initialize request");
    let sent: serde_json::Value =
        serde_json::from_str(&initialize).expect("expected valid JSON-RPC");
    let advertised = &sent["params"]["clientCapabilities"]["session"];
    for capability in ["notices", "compaction"] {
        assert_eq!(
            advertised[capability],
            serde_json::json!({}),
            "expected clientCapabilities.session.{capability}: {{}} | received: {advertised}"
        );
    }
    session.close(CloseReason::Requested).await.expect("close");
}

#[tokio::test]
async fn a_notice_mid_turn_reaches_the_host_where_the_agent_sent_it() {
    let received = turn_story(vec![
        text("before"),
        with(
            notice("warning", "Rate limit close"),
            "description",
            serde_json::json!("80% of the hourly budget is used"),
        ),
        text("after"),
    ])
    .await;
    assert_story(
        &received,
        &[
            "text(before)",
            "notice \"warning\" \"Rate limit close\" 80% of the hourly budget is used",
            "text(after)",
            "turn completed",
        ],
    );
}

/// An unknown severity is a notice all the same, shown generically under the agent's own word.
#[tokio::test]
async fn every_severity_arrives_and_an_unknown_one_keeps_its_spelling() {
    let received = turn_story(vec![
        notice("info", "one"),
        notice("error", "two"),
        notice("_vendor_debug", "three"),
    ])
    .await;
    assert_story(
        &received,
        &[
            "notice \"info\" \"one\" -",
            "notice \"error\" \"two\" -",
            "notice {\"other\":\"_vendor_debug\"} \"three\" -",
            "turn completed",
        ],
    );
}

/// A notice with nothing to show is dropped on its own. Even one marked `error` ends nothing: only
/// the prompt's own answer ends a turn.
#[tokio::test]
async fn a_notice_with_no_usable_title_is_dropped_and_the_turn_goes_on() {
    let received = turn_story(vec![
        notice("error", ""),
        notice("error", " \u{1b}\u{202e} "),
        notice("error", "kept"),
        text("still going"),
    ])
    .await;
    assert_story(
        &received,
        &[
            "notice \"error\" \"kept\" -",
            "text(still going)",
            "turn completed",
        ],
    );
}

#[tokio::test]
async fn a_notices_text_is_sanitised_and_bounded() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().with_updates(vec![with(
            notice(
                &format!("\u{1b}[31m{}", "s".repeat(500)),
                &format!("\u{202e}{}", "t".repeat(500)),
            ),
            "description",
            serde_json::json!(format!("\u{7}{}", "d".repeat(5_000))),
        )]),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    let notice = events.iter().find_map(|kind| match kind {
        EventKind::Notice {
            severity: NoticeSeverity::Other(severity),
            title,
            description: Some(description),
        } => Some((severity, title, description)),
        _ => None,
    });
    let Some((severity, title, description)) = notice else {
        panic!(
            "expected one notice with an open severity and a description | received: {events:?}"
        );
    };
    let received = (
        severity.chars().count(),
        title.chars().count(),
        description.chars().count(),
    );
    assert_eq!(
        received,
        (128, 256, 4_096),
        "expected (severity, title, description) code points at their bounds | received: {received:?}"
    );
    for text in [severity, title, description] {
        assert!(
            !text.contains(['\u{1b}', '\u{202e}', '\u{7}']),
            "expected no control or bidi character to survive | received: {text:?}"
        );
    }
    session.close(CloseReason::Requested).await.expect("close");
}

/// The first frame fixes where the compaction sits among the text around it. The summary's first
/// chunk goes out at once, the rest is coalesced like a tool call's output and delivered ahead of
/// the completion.
#[tokio::test]
async fn a_compaction_is_started_updated_and_completed_in_the_order_the_agent_sent_it() {
    let received = turn_story(vec![
        text("before"),
        compaction("c1", "in_progress"),
        chunk("c1", "Sum"),
        text("during"),
        chunk("c1", "mary of "),
        chunk("c1", "the talk"),
        compaction("c1", "completed"),
        text("after"),
    ])
    .await;
    assert_story(
        &received,
        &[
            "text(before)",
            STARTED_C1,
            "updated acp:compaction:c1 detail=- content=output(Sum)",
            "text(during)",
            "updated acp:compaction:c1 detail=- content=output(Summary of the talk)",
            "completed acp:compaction:c1 Completed detail=- content=-",
            "text(after)",
            "turn completed",
        ],
    );
}

#[tokio::test]
async fn a_compaction_that_fails_carries_the_agents_error_as_its_detail() {
    let received = turn_story(vec![
        compaction("c1", "in_progress"),
        with(
            compaction("c1", "failed"),
            "error",
            serde_json::json!("the summariser ran out of context"),
        ),
    ])
    .await;
    assert_story(
        &received,
        &[
            STARTED_C1,
            "completed acp:compaction:c1 Failed detail=the summariser ran out of context content=-",
            "turn completed",
        ],
    );
}

#[tokio::test]
async fn a_cancelled_compaction_ends_as_cancelled() {
    let received = turn_story(vec![chunk("c1", "half"), compaction("c1", "cancelled")]).await;
    assert_story(
        &received,
        &[
            STARTED_C1,
            "updated acp:compaction:c1 detail=- content=output(half)",
            "completed acp:compaction:c1 Cancelled detail=- content=-",
            "turn completed",
        ],
    );
}

/// A `summary` on an update replaces what the chunks built, and later chunks append to the
/// replacement.
#[tokio::test]
async fn a_summary_on_an_update_replaces_the_chunks_before_it() {
    let received = turn_story(vec![
        chunk("c1", "draft"),
        with(
            compaction("c1", "in_progress"),
            "summary",
            serde_json::json!([
                { "type": "text", "text": "Final" },
                { "type": "image", "mimeType": "image/png", "data": "AAAA" },
                { "type": "text", "text": " summary" }
            ]),
        ),
        chunk("c1", ", extended"),
        compaction("c1", "completed"),
    ])
    .await;
    assert_story(
        &received,
        &[
            STARTED_C1,
            "updated acp:compaction:c1 detail=- content=output(draft)",
            "updated acp:compaction:c1 detail=- content=output(Final summary, extended)",
            "completed acp:compaction:c1 Completed detail=- content=-",
            "turn completed",
        ],
    );
}

#[tokio::test]
async fn a_null_or_empty_summary_clears_what_the_host_holds() {
    for cleared in [serde_json::Value::Null, serde_json::json!([])] {
        let received = turn_story(vec![
            chunk("c1", "draft"),
            with(compaction("c1", "completed"), "summary", cleared.clone()),
        ])
        .await;
        assert_story(
            &received,
            &[
                STARTED_C1,
                "updated acp:compaction:c1 detail=- content=output(draft)",
                "completed acp:compaction:c1 Completed detail=- content=empty",
                "turn completed",
            ],
        );
    }
}

/// A status this build does not know is not an ending and not a failure.
#[tokio::test]
async fn an_unknown_status_leaves_the_compaction_running_and_the_turn_alive() {
    let received = turn_story(vec![
        compaction("c1", "_vendor_paused"),
        text("still going"),
        compaction("c1", "completed"),
    ])
    .await;
    assert_story(
        &received,
        &[
            STARTED_C1,
            "text(still going)",
            "completed acp:compaction:c1 Completed detail=- content=-",
            "turn completed",
        ],
    );
}

/// ACP ends a turn with the prompt's answer, not a frame per activity. A compaction the agent
/// never ended is closed with the turn, its held summary first, like a tool call left running.
#[tokio::test]
async fn a_compaction_left_open_is_closed_when_the_turn_ends() {
    let received = turn_story(vec![chunk("c1", "half"), chunk("c1", " done")]).await;
    assert_story(
        &received,
        &[
            STARTED_C1,
            "updated acp:compaction:c1 detail=- content=output(half)",
            "updated acp:compaction:c1 detail=- content=output(half done)",
            "completed acp:compaction:c1 Completed detail=- content=-",
            "turn completed",
        ],
    );
}

/// A frame for a compaction that already ended has nothing to patch.
#[tokio::test]
async fn frames_after_a_compactions_end_are_dropped() {
    let received = turn_story(vec![
        compaction("c1", "completed"),
        chunk("c1", "late"),
        compaction("c1", "failed"),
    ])
    .await;
    assert_story(
        &received,
        &[
            STARTED_C1,
            "completed acp:compaction:c1 Completed detail=- content=-",
            "turn completed",
        ],
    );
}

/// A compaction id and a tool call id are separate namespaces on the wire, so one string may name
/// both. They stay two activities.
#[tokio::test]
async fn a_compaction_and_a_tool_call_with_the_same_id_stay_apart() {
    let received = turn_story(vec![
        serde_json::json!({
            "sessionUpdate": "tool_call", "toolCallId": "c1", "title": "Run", "kind": "execute",
            "status": "in_progress"
        }),
        compaction("c1", "completed"),
        serde_json::json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "c1", "status": "failed"
        }),
    ])
    .await;
    let ended: Vec<&str> = received
        .iter()
        .filter(|line| line.starts_with("completed "))
        .map(|line| line.split(" detail=").next().unwrap_or(line))
        .collect();
    assert_eq!(
        ended,
        [
            "completed acp:compaction:c1 Completed",
            "completed c1 Failed"
        ],
        "expected two activities, each ended by its own frame | received: {received:#?}"
    );
}

#[tokio::test]
async fn a_compactions_summary_and_error_are_sanitised_and_bounded() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().with_updates(vec![
            chunk("c1", &format!("\u{1b}[2J{}", "s".repeat(5_000))),
            with(
                compaction("c1", "failed"),
                "error",
                serde_json::json!(format!("\u{202e}{}", "e".repeat(5_000))),
            ),
        ]),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    let summary = events.iter().find_map(|kind| match kind {
        EventKind::ActivityUpdated { update, .. } => match &update.content {
            Some(ActivityContent::Output { text }) => Some((text.as_str(), update.truncated)),
            _ => None,
        },
        _ => None,
    });
    let error = events.iter().find_map(|kind| match kind {
        EventKind::ActivityCompleted { result, .. } if result.status == ActivityStatus::Failed => {
            result
                .detail
                .as_deref()
                .map(|detail| (detail, result.truncated))
        }
        _ => None,
    });
    for (name, field) in [("summary", summary), ("error", error)] {
        let Some((text, truncated)) = field else {
            panic!("expected a compaction {name} to reach the host | received: {events:?}");
        };
        let received = (text.chars().count(), truncated);
        assert_eq!(
            received,
            (4_096, true),
            "expected {name} (code points, truncated): (4096, true) | received: {received:?}"
        );
        assert!(
            !text.contains(['\u{1b}', '\u{202e}']),
            "expected no control or bidi character in the {name} | received: {:?}",
            text.chars().take(8).collect::<String>()
        );
    }
    session.close(CloseReason::Requested).await.expect("close");
}

/// A compaction id the core cannot publish is refused, not shortened, and the host is told: the
/// turn fails as it does for a tool call id of the same kind.
#[tokio::test]
async fn a_compaction_id_the_core_refuses_fails_the_turn() {
    for id in ["   ".to_owned(), "x".repeat(200), "x".repeat(120)] {
        let received = turn_story(vec![
            text("before"),
            compaction(&id, "in_progress"),
            text("never shown"),
        ])
        .await;
        assert_story(
            &received,
            &["text(before)", "turn failed acp-refused-event"],
        );
    }
}

/// The kind is the neutral one a host already has an icon for, whichever vendor compacted.
#[tokio::test]
async fn a_compaction_is_the_neutral_compaction_activity() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().with_updates(vec![compaction("c1", "in_progress")]),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    let started = events.iter().find_map(|kind| match kind {
        EventKind::ActivityStarted { activity, .. } => Some(activity),
        _ => None,
    });
    assert_eq!(
        started.map(|activity| activity.kind),
        Some(ActivityKind::Compaction)
    );
    session.close(CloseReason::Requested).await.expect("close");
}

/// The JSON a host reads for a notice and for a whole compaction, as `docs/harness-acp.md` shows it.
#[tokio::test]
async fn the_json_a_host_reads_for_a_notice_and_a_compaction() {
    let (session, _launcher) = open(
        FakeAcpAgent::new().with_updates(vec![
            with(
                notice("warning", "Rate limit close"),
                "description",
                serde_json::json!("80% of the hourly budget is used"),
            ),
            compaction("c1", "in_progress"),
            chunk("c1", "Summary so far"),
            with(
                compaction("c1", "failed"),
                "error",
                serde_json::json!("out of context"),
            ),
        ]),
        permissive(),
    )
    .await;
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    let received: Vec<serde_json::Value> = events
        .iter()
        .filter(|kind| {
            matches!(
                kind,
                EventKind::Notice { .. }
                    | EventKind::ActivityStarted { .. }
                    | EventKind::ActivityUpdated { .. }
                    | EventKind::ActivityCompleted { .. }
            )
        })
        .map(|kind| serde_json::json!(kind))
        .collect();
    let expected = serde_json::json!([
        {
            "type": "notice",
            "severity": "warning",
            "title": "Rate limit close",
            "description": "80% of the hourly budget is used"
        },
        {
            "type": "activity_started",
            "call_id": "acp:compaction:c1",
            "activity": {
                "name": "compact",
                "kind": "compaction",
                "title": "Compacting the conversation",
                "itemId": "c1",
                "truncated": false
            }
        },
        {
            "type": "activity_updated",
            "call_id": "acp:compaction:c1",
            "update": {
                "content": { "type": "output", "text": "Summary so far" },
                "truncated": false
            }
        },
        {
            "type": "activity_completed",
            "call_id": "acp:compaction:c1",
            "result": { "status": "failed", "detail": "out of context", "truncated": false }
        }
    ]);
    assert_eq!(
        serde_json::Value::Array(received.clone()),
        expected,
        "expected host JSON: {expected:#} | received: {received:#?}"
    );
    session.close(CloseReason::Requested).await.expect("close");
}

/// A notice or a compaction frame with no turn in flight has no stream to travel on. `session/load`
/// replays history before any turn exists, so what it replays is dropped; a compaction still
/// running when a turn starts opens there on its next frame, without the summary sent before it.
#[tokio::test]
async fn frames_replayed_before_any_turn_are_dropped_and_a_running_compaction_reopens() {
    let launcher = FakeLauncher::new();
    launcher.push(
        FakeAcpAgent::new()
            .replaying_on_load(vec![
                notice("warning", "replayed"),
                compaction("c1", "in_progress"),
                chunk("c1", "replayed summary"),
            ])
            .with_updates(vec![chunk("c1", "live summary")])
            .process(),
    );
    let session = AcpHarness::new(profile())
        .open_session(
            &host(&launcher),
            OpenSession::new("chat-1")
                .with_configuration(permissive())
                .resuming("sess_fake", ResumeMode::Strict),
        )
        .await
        .expect("expected the session to load");
    let mut turn = session
        .start_turn(TurnRequest::new("turn-1", "go"))
        .await
        .expect("expected a turn");
    let events = drain(&mut turn).await;
    session.close(CloseReason::Requested).await.expect("close");
    assert_story(
        &story(&events),
        &[
            STARTED_C1,
            "updated acp:compaction:c1 detail=- content=output(live summary)",
            "completed acp:compaction:c1 Completed detail=- content=-",
            "turn completed",
        ],
    );
}
