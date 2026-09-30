//! `Notification::parse` decodes the params it was handed by reference, so a malformed frame can
//! still be routed from them. That is only safe while `T::deserialize(&Value)` accepts exactly what
//! `serde_json::from_value(Value)` accepts and reads the same thing out of it. These tests hold the
//! two together over every captured Codex notification and over hand-written frames that lean on
//! the awkward serde paths: defaults, nulls, an untagged fallback, wrong types, wide numbers.

use std::fmt::Debug;
use std::path::{Path, PathBuf};

use mango_agent_codex::protocol::notifications::{RateLimitsUpdated, method};
use mango_agent_codex::protocol::{
    AgentMessageDelta, CommandOutputDelta, ErrorNotification, FileChangePatchUpdated,
    ItemNotification, McpToolCallProgress, Notification, ReasoningDelta, ServerRequestResolved,
    ThreadStarted, TurnNotification, TurnTokenUsage,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// A named params value: where it came from, for the failure message.
struct Input {
    label: String,
    params: Value,
}

fn collect_transcripts(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_transcripts(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            found.push(path);
        }
    }
}

/// The params of every server notification in every captured Codex transcript.
fn captured() -> Vec<(String, String, Value)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codex");
    let mut paths = Vec::new();
    collect_transcripts(&root, &mut paths);
    paths.sort();
    let mut frames = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("expected {} to be readable: {error}", path.display()));
        for (index, line) in text
            .lines()
            .filter_map(|line| line.strip_prefix("<<"))
            .enumerate()
        {
            let Ok(Value::Object(frame)) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if frame.contains_key("id") {
                continue;
            }
            let (Some(Value::String(method)), Some(params)) =
                (frame.get("method"), frame.get("params"))
            else {
                continue;
            };
            let name = path
                .file_stem()
                .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
            frames.push((format!("{name}#{index}"), method.clone(), params.clone()));
        }
    }
    frames
}

/// Frames a real server does not send, aimed at the places a reference decode could differ.
fn adversarial() -> Vec<Input> {
    let big = "é\u{1F600}\\\"\n\t".repeat(64);
    let cases = [
        ("empty object", json!({})),
        ("null", Value::Null),
        ("array", json!([1, 2, 3])),
        ("string", json!("threadId")),
        ("number", json!(7)),
        (
            "delta with escapes and non-BMP text",
            json!({"threadId": "t", "turnId": "u", "itemId": "i", "delta": big}),
        ),
        (
            "delta with null default fields",
            json!({"threadId": "t", "turnId": "u", "itemId": null, "delta": null}),
        ),
        (
            "delta with a number for text",
            json!({"threadId": "t", "turnId": "u", "itemId": "i", "delta": 4}),
        ),
        (
            "ids as numbers",
            json!({"threadId": 1, "turnId": 2, "itemId": 3, "delta": "x"}),
        ),
        (
            "unknown extra fields, nested",
            json!({"threadId": "t", "turnId": "u", "itemId": "i", "delta": "x",
                   "extra": {"deep": [1, {"a": null}]}}),
        ),
        (
            "request id as an object",
            json!({"threadId": "t", "requestId": {"a": [1, 2.5, null]}}),
        ),
        (
            "request id as a float",
            json!({"threadId": "t", "requestId": 1.5e300}),
        ),
        (
            "request id past i64",
            json!({"threadId": "t", "requestId": 18446744073709551615_u64}),
        ),
        (
            "turn status as a number",
            json!({"threadId": "t", "turn": {"id": "u", "status": 7}}),
        ),
        (
            "turn with a known status",
            json!({"threadId": "t", "turn": {"id": "u", "status": "completed"}}),
        ),
        (
            "item of an unmodelled family",
            json!({"threadId": "t", "turnId": "u",
                   "item": {"type": "imageView", "id": "i", "status": "completed"}}),
        ),
        (
            "item of an unmodelled family without an id",
            json!({"threadId": "t", "turnId": "u", "item": {"type": "imageView"}}),
        ),
        (
            "item of a known family whose fields do not decode",
            json!({"threadId": "t", "turnId": "u",
                   "item": {"type": "commandExecution", "id": 9, "command": []}}),
        ),
        (
            "item with a numeric type",
            json!({"threadId": "t", "turnId": "u", "item": {"type": 7, "id": "i"}}),
        ),
        (
            "item without a type",
            json!({"threadId": "t", "turnId": "u", "item": {"id": "i"}}),
        ),
        (
            "item with an unknown status spelling",
            json!({"threadId": "t", "turnId": "u",
                   "item": {"type": "commandExecution", "id": "i", "command": "ls",
                            "status": "somethingNew", "exitCode": 1.0}}),
        ),
        (
            "file change with null diff and missing path",
            json!({"threadId": "t", "turnId": "u", "itemId": "i",
                   "changes": [{"diff": null}, {"path": "a"}, {"path": "b", "diff": "+x\n"}]}),
        ),
        (
            "file change with changes as an object",
            json!({"threadId": "t", "turnId": "u", "itemId": "i", "changes": {}}),
        ),
        (
            "token usage with floats for counts",
            json!({"threadId": "t", "turnId": "u",
                   "tokenUsage": {"last": {"inputTokens": 1.0, "outputTokens": 2},
                                  "total": {"inputTokens": -1}, "modelContextWindow": 1e3}}),
        ),
        (
            "token usage with an empty object",
            json!({"threadId": "t", "turnId": "u", "tokenUsage": {}}),
        ),
        (
            "rate limits with credits and a limit",
            json!({"rateLimits": {"primary": {"usedPercent": 12.5, "windowDurationMins": 300,
                                              "resetsAt": 1789283381},
                                  "planType": "plus",
                                  "credits": {"hasCredits": true, "unlimited": false,
                                              "balance": "4.20"},
                                  "spendControlReached": null}}),
        ),
        (
            "rate limits with the snake-case spelling",
            json!({"rate_limits": {}}),
        ),
        (
            "error with a retry flag",
            json!({"threadId": "t", "turnId": "u", "willRetry": true,
                   "error": {"message": "overloaded", "codexErrorInfo": "serverOverloaded"}}),
        ),
        (
            "error with a structured info object",
            json!({"threadId": "t", "error": {"message": "x",
                   "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 502}}}}),
        ),
        (
            "thread with unknown fields",
            json!({"thread": {"id": "t", "preview": "hi", "cliVersion": "0.0.0", "turns": []}}),
        ),
    ];
    cases
        .into_iter()
        .map(|(label, params)| Input {
            label: label.to_owned(),
            params,
        })
        .collect()
}

/// Decodes `input` as `T` both ways and requires the same verdict and the same value.
fn assert_decodes_alike<T>(input: &Input)
where
    T: DeserializeOwned + Debug + PartialEq,
{
    let by_value = serde_json::from_value::<T>(input.params.clone());
    let by_reference = T::deserialize(&input.params);
    let name = std::any::type_name::<T>();
    match (by_value, by_reference) {
        (Ok(value), Ok(reference)) => assert_eq!(
            value, reference,
            "expected {name} to read {} the same by reference as by value | by value: \
             {value:?} | by reference: {reference:?}",
            input.label
        ),
        (Err(_), Err(_)) => {}
        (by_value, by_reference) => panic!(
            "expected {name} to accept {} by reference exactly when it accepts it by value | \
             by value accepted: {} | by reference accepted: {}",
            input.label,
            by_value.is_ok(),
            by_reference.is_ok()
        ),
    }
}

fn assert_every_type_decodes_alike(input: &Input) {
    assert_decodes_alike::<ThreadStarted>(input);
    assert_decodes_alike::<TurnNotification>(input);
    assert_decodes_alike::<ItemNotification>(input);
    assert_decodes_alike::<AgentMessageDelta>(input);
    assert_decodes_alike::<ReasoningDelta>(input);
    assert_decodes_alike::<CommandOutputDelta>(input);
    assert_decodes_alike::<McpToolCallProgress>(input);
    assert_decodes_alike::<FileChangePatchUpdated>(input);
    assert_decodes_alike::<TurnTokenUsage>(input);
    assert_decodes_alike::<RateLimitsUpdated>(input);
    assert_decodes_alike::<ServerRequestResolved>(input);
    assert_decodes_alike::<ErrorNotification>(input);
}

/// Every params value a real server wrote decodes alike as every type, not only its own.
#[test]
fn captured_notifications_decode_alike_by_reference_and_by_value() {
    let frames = captured();
    assert!(
        frames.len() > 100,
        "expected the captured transcripts to hold notification frames | received: {} frames",
        frames.len()
    );
    for (label, _, params) in frames {
        assert_every_type_decodes_alike(&Input { label, params });
    }
}

#[test]
fn awkward_frames_decode_alike_by_reference_and_by_value() {
    for input in adversarial() {
        assert_every_type_decodes_alike(&input);
    }
}

/// The routing values of a frame that does not decode come from the params that were handed in.
#[test]
fn a_frame_that_does_not_decode_is_routed_from_its_own_params() {
    let notification = Notification::parse(
        method::AGENT_MESSAGE_DELTA,
        json!({"threadId": "thread-1", "turnId": "turn-1", "itemId": "i", "delta": 4}),
    );
    assert_eq!(
        notification,
        Notification::Malformed {
            method: String::from(method::AGENT_MESSAGE_DELTA),
            thread_id: Some(String::from("thread-1")),
            turn_id: Some(String::from("turn-1")),
        },
        "expected a delta with a numeric text to be malformed and keep its routing"
    );
}

/// The whole of `Notification::parse` still decides every captured frame the way its family's
/// type does, so no fixture frame flipped between typed and malformed.
#[test]
fn captured_notifications_are_not_malformed() {
    for (label, method, params) in captured() {
        let notification = Notification::parse(&method, params);
        assert!(
            !matches!(notification, Notification::Malformed { .. }),
            "expected {label} ({method}) to decode | received: {notification:?}"
        );
    }
}
