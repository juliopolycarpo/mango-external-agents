//! `Debug` for the raw `stream-json` records reports metadata and never what the agent wrote or read.
//!
//! A `tool_result` body arrives in these records: a `Read` of a `.env` is the line
//! `{"type":"tool_result","content":"API_KEY=..."}`. `StreamRecord` keeps the whole line's JSON and
//! its borrowed views point into it, so a derived `Debug` prints every byte of it. Each is given a
//! canary in every content field and formatted both ways a logger might.

use std::fmt::Debug;

use mango_agent_claude::protocol::StreamRecord;

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

fn record(line: &str) -> StreamRecord {
    StreamRecord::parse(line).expect("expected a parseable record")
}

#[test]
fn a_tool_result_record_does_not_print_the_body_it_carries() {
    let record = record(
        r#"{"type":"user","message":{"content":[{"type":"tool_result",
            "tool_use_id":"CANARY-toolu","content":"CANARY-body=hunter2"}]},
            "session_id":"CANARY-session"}"#,
    );
    let canaries = [
        ("tool_result content", "CANARY-body"),
        ("tool_use_id", "CANARY-toolu"),
        ("session_id", "CANARY-session"),
    ];
    assert_omits("StreamRecord", &record, &canaries);
    assert_names("StreamRecord", &record, "StreamRecord");
    assert_names("StreamRecord", &record, "user");
    let blocks = record.content_blocks();
    assert_omits("ContentBlock", &blocks[0], &canaries);
    assert_names("ContentBlock", &blocks[0], "tool_result");
}

#[test]
fn an_assistant_record_does_not_print_its_text_thinking_or_tool_input() {
    let record = record(
        r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"CANARY-text"},
            {"type":"thinking","thinking":"CANARY-thinking"},
            {"type":"tool_use","id":"CANARY-id","name":"CANARY-tool",
             "input":{"file_path":"CANARY-path"}}]}}"#,
    );
    let canaries = [
        ("text", "CANARY-text"),
        ("thinking", "CANARY-thinking"),
        ("tool_use id", "CANARY-id"),
        ("tool name", "CANARY-tool"),
        ("tool input", "CANARY-path"),
    ];
    assert_omits("StreamRecord", &record, &canaries);
    for (index, block) in record.content_blocks().iter().enumerate() {
        assert_omits(&format!("ContentBlock[{index}]"), block, &canaries);
    }
}

#[test]
fn a_stream_event_and_its_delta_do_not_print_the_streamed_text() {
    let record = record(
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,
            "delta":{"type":"text_delta","text":"CANARY-delta","thinking":"CANARY-think"}}}"#,
    );
    let canaries = [
        ("delta text", "CANARY-delta"),
        ("delta thinking", "CANARY-think"),
    ];
    assert_omits("StreamRecord", &record, &canaries);
    let event = record.stream_event().expect("expected a stream event");
    assert_omits("StreamEvent", &event, &canaries);
    assert_names("StreamEvent", &event, "content_block_delta");
    let delta = event.delta().expect("expected a delta");
    assert_omits("Delta", &delta, &canaries);
    assert_names("Delta", &delta, "text_delta");
}

#[test]
fn a_result_record_does_not_print_the_final_answer_or_errors() {
    let record = record(
        r#"{"type":"result","subtype":"success","is_error":true,"result":"CANARY-result",
            "errors":["CANARY-error"],"terminal_reason":"CANARY-reason",
            "usage":{"input_tokens":4}}"#,
    );
    let canaries = [
        ("result", "CANARY-result"),
        ("errors", "CANARY-error"),
        ("terminal_reason", "CANARY-reason"),
    ];
    assert_omits("StreamRecord", &record, &canaries);
    assert_omits("ResultRecord", &record.result(), &canaries);
    assert_names("StreamRecord", &record, "result");
}

#[test]
fn the_init_record_and_a_permission_denial_do_not_print_their_members() {
    let init = record(
        r#"{"type":"system","subtype":"init","session_id":"CANARY-session","model":"CANARY-model",
            "slash_commands":["CANARY-command"],"skills":["CANARY-skill"],
            "plugins":[{"name":"CANARY-plugin"}],"permissionMode":"CANARY-mode"}"#,
    );
    let canaries = [
        ("session_id", "CANARY-session"),
        ("model", "CANARY-model"),
        ("slash_commands", "CANARY-command"),
        ("skills", "CANARY-skill"),
        ("plugins", "CANARY-plugin"),
        ("permissionMode", "CANARY-mode"),
    ];
    assert_omits("StreamRecord", &init, &canaries);
    assert_omits("InitRecord", &init.init(), &canaries);

    let denied =
        record(r#"{"type":"user","tool_use_id":"CANARY-toolu","message":"CANARY-denied"}"#);
    assert_omits(
        "PermissionDenied",
        &denied.permission_denied(),
        &[
            ("tool_use_id", "CANARY-toolu"),
            ("message", "CANARY-denied"),
        ],
    );
}

#[test]
fn an_unknown_discriminator_is_reported_by_size_not_by_text() {
    let record = record(r#"{"type":"CANARY type with spaces and secrets","subtype":"success"}"#);
    assert_omits("StreamRecord", &record, &[("type", "CANARY")]);
    assert_names("StreamRecord", &record, "success");
}

/// A credential or an id can have a label's shape, so a value that is not a discriminator this
/// harness knows is sized even when it looks like one.
#[test]
fn a_credential_shaped_discriminator_is_not_printed() {
    let record = record(
        r#"{"type":"AKIAIOSFODNN7EXAMPLE","subtype":"3f2b8c1e-0d4a-4c7e-9a55-1b2c3d4e5f60"}"#,
    );
    assert_omits(
        "StreamRecord",
        &record,
        &[
            ("type", "AKIAIOSFODNN7EXAMPLE"),
            ("subtype", "3f2b8c1e-0d4a-4c7e-9a55-1b2c3d4e5f60"),
        ],
    );
}

/// The reducer holds the streamed text of each open block until its `assistant` record lands, the
/// text a subagent forwarded, the message of each permission denial and the call ids that key them,
/// so a debug-logged reducer is a debug-logged transcript.
#[test]
fn a_reducer_does_not_print_what_it_is_holding() {
    let mut turn = mango_agent_claude::reducer::TurnReducer::new();
    for line in [
        r#"{"type":"system","subtype":"permission_denied","tool_use_id":"CANARY-denied-id",
            "message":"CANARY-denied-message"}"#,
        r#"{"type":"assistant","message":{"content":[
            {"type":"tool_use","id":"CANARY-call","name":"Read","input":{"file_path":"CANARY-path"}}]}}"#,
        r#"{"type":"assistant","parent_tool_use_id":"CANARY-call","message":{"content":[
            {"type":"text","text":"CANARY-nested"}]}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","index":3,
            "delta":{"type":"text_delta","text":"CANARY-delta"}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_start","index":4,
            "content_block":{"type":"thinking"}}}"#,
    ] {
        let _ = turn.reduce(&record(line));
    }
    assert_omits(
        "TurnReducer",
        &turn,
        &[
            ("open activity call id", "CANARY-call"),
            ("denied activity id", "CANARY-denied-id"),
            ("denied activity message", "CANARY-denied-message"),
            ("nested subagent text", "CANARY-nested"),
            ("delivered block text", "CANARY-delta"),
        ],
    );
    assert_names("TurnReducer", &turn, "TurnReducer");
}
