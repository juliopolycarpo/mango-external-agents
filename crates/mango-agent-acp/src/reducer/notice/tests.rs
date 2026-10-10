use agent_client_protocol::schema::v1::SessionUpdate;
use serde_json::json;

use super::super::Reducer;
use super::*;

fn frame(value: serde_json::Value) -> SessionUpdate {
    serde_json::from_value(value).expect("expected a v1 session update")
}

#[test]
fn a_notice_frame_is_one_notice_event() {
    let wire: Notice = serde_json::from_value(json!({
        "severity": "warning",
        "title": "Rate limit close",
        "description": "80% used",
        "_meta": { "vendor": { "ignored": true } }
    }))
    .expect("expected a v1 notice");
    let received = notice(wire);
    assert_eq!(
        received,
        vec![EventKind::Notice {
            severity: NoticeSeverity::Warning,
            title: String::from("Rate limit close"),
            description: Some(String::from("80% used")),
        }],
        "expected one notice event | received: {received:?}"
    );
}

#[test]
fn a_notice_whose_title_has_nothing_to_show_produces_nothing() {
    for title in ["", "  ", "\u{1b}\u{202e}"] {
        let wire: Notice = serde_json::from_value(json!({ "severity": "error", "title": title }))
            .expect("expected a v1 notice");
        let received = notice(wire);
        assert!(
            received.is_empty(),
            "expected events for title {title:?}: none | received: {received:?}"
        );
    }
}

#[test]
fn every_wire_severity_has_a_neutral_one_and_an_unknown_one_keeps_its_spelling() {
    let cases = [
        ("info", NoticeSeverity::Info),
        ("warning", NoticeSeverity::Warning),
        ("error", NoticeSeverity::Error),
        ("_vendor", NoticeSeverity::Other(String::from("_vendor"))),
        ("critical", NoticeSeverity::Other(String::from("critical"))),
    ];
    for (wire, expected) in cases {
        let parsed: WireSeverity =
            serde_json::from_value(json!(wire)).expect("expected a severity");
        let received = severity(parsed);
        assert_eq!(
            received, expected,
            "expected severity for {wire:?}: {expected:?} | received: {received:?}"
        );
    }
}

#[test]
fn a_notice_between_two_thoughts_does_not_split_the_reasoning_block() {
    let mut reducer = Reducer::new();
    let thought = json!({
        "sessionUpdate": "agent_thought_chunk",
        "content": { "type": "text", "text": "thinking" }
    });
    let mut events = Vec::new();
    for value in [
        thought.clone(),
        json!({ "sessionUpdate": "notice", "severity": "info", "title": "heads up" }),
        thought,
    ] {
        events.extend(reducer.update(frame(value)).0);
    }
    assert!(
        matches!(
            events.as_slice(),
            [
                EventKind::ReasoningStarted,
                EventKind::ReasoningDelta { .. },
                EventKind::Notice { .. },
                EventKind::ReasoningDelta { .. }
            ]
        ),
        "expected one reasoning block with the notice inside it | received: {events:?}"
    );
}

#[test]
fn a_notice_is_not_a_session_fact() {
    let facts = Reducer::session_facts(frame(
        json!({ "sessionUpdate": "notice", "severity": "info", "title": "heads up" }),
    ));
    assert!(facts.is_empty(), "received: {facts:?}");
}
