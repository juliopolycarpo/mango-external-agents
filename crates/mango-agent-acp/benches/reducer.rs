//! The ACP reducer on tool-call frames whose content is large.
//!
//! A tool call's content blocks are what the reducer reads for the one-line `detail` and again for
//! the structured `content`. The cases here feed it diffs and text bodies of a realistic size (a
//! few KiB) and of a stress size (100 KiB per diff, a 1 MiB image beside the text), so a change to
//! how content is read shows where it lands. Run with:
//!
//! ```sh
//! cargo bench -p mango-agent-acp --bench reducer
//! ```

mod support;

use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::SessionUpdate;
use mango_agent_acp::reducer::Reducer;
use mango_external_agents::ActivityContent;
use mango_external_agents::event::EventKind;
use serde_json::{Value, json};
use support::{Bench, Unit};

/// Frames per sample for the large-content cases.
const LARGE_FRAMES: usize = 20;

/// Frames per sample for the small-content cases, so a case is milliseconds rather than noise.
const SMALL_FRAMES: usize = 2000;

/// Diff blocks in one frame of the large cases, and the bytes of new text in each.
const LARGE_DIFFS: usize = 10;
const LARGE_DIFF_BYTES: usize = 100_000;

/// A typical edit: two files of a couple of KiB.
const SMALL_DIFFS: usize = 2;
const SMALL_DIFF_BYTES: usize = 2_000;

/// Bytes of the image and of the text body in the 1 MiB cases.
const MIB: usize = 1 << 20;

/// Farther apart than the reducer's update coalescing window, so no update is held back.
const FRAME_SPACING: Duration = Duration::from_secs(10);

fn diff_block(index: usize, bytes: usize) -> Value {
    let body = "+added line of source code\n".repeat(bytes / 27);
    json!({
        "type": "diff",
        "path": format!("/repo/src/file{index}.rs"),
        "oldText": "-removed line of source code\n",
        "newText": body,
    })
}

fn text_block(text: &str) -> Value {
    json!({ "type": "content", "content": { "type": "text", "text": text } })
}

fn image_block(bytes: usize) -> Value {
    json!({
        "type": "content",
        "content": { "type": "image", "mimeType": "image/png", "data": "A".repeat(bytes) },
    })
}

fn update(value: Value) -> SessionUpdate {
    serde_json::from_value(value).expect("expected the bench frame to be a v1 session update")
}

fn tool_call(index: usize, content: &[Value]) -> SessionUpdate {
    update(json!({
        "sessionUpdate": "tool_call",
        "toolCallId": format!("call-{index}"),
        "title": "Edit",
        "kind": "edit",
        "status": "in_progress",
        "content": content,
    }))
}

fn tool_call_update(index: usize, status: &str, content: &[Value]) -> SessionUpdate {
    update(json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": format!("call-{index}"),
        "status": status,
        "content": content,
    }))
}

/// `count` new tool calls that each carry `content`.
fn started(count: usize, content: &[Value]) -> Vec<SessionUpdate> {
    (0..count).map(|index| tool_call(index, content)).collect()
}

/// Whether any of `events` carries structured content a host would render.
fn carries_content(events: &[EventKind]) -> bool {
    events.iter().any(|event| match event {
        EventKind::ActivityStarted { activity, .. } => activity.content.is_some(),
        EventKind::ActivityUpdated { update, .. } => update.content.is_some(),
        EventKind::ActivityCompleted { result, .. } => result
            .content
            .as_ref()
            .is_some_and(|content| *content != ActivityContent::Empty),
        _ => false,
    })
}

/// Reduces every frame, spacing their instants out, and asserts each one produced content.
fn reduce(frames: Vec<SessionUpdate>, mut reducer: Reducer) -> usize {
    let start = Instant::now();
    let mut carried = 0;
    for (index, frame) in frames.into_iter().enumerate() {
        let spacing = FRAME_SPACING * u32::try_from(index).unwrap_or(u32::MAX);
        let (events, _) = reducer.update_at(frame, start + spacing);
        assert!(
            carries_content(&events),
            "expected frame {index} to carry content | received: {events:?}"
        );
        carried += 1;
        std::hint::black_box(events);
    }
    carried
}

/// A reducer already tracking `count` running calls, for the update cases.
fn reducer_with_open_calls(count: usize) -> Reducer {
    let mut reducer = Reducer::new();
    let start = Instant::now();
    for index in 0..count {
        let (events, _) = reducer.update_at(tool_call(index, &[]), start);
        assert_eq!(
            events.len(),
            1,
            "expected one ActivityStarted per opened call | received: {events:?}"
        );
    }
    reducer
}

fn main() {
    let bench = Bench::new("acp reducer (tool-call content)");
    let large = Unit::new(LARGE_FRAMES as u64, "frame");
    let small = Unit::new(SMALL_FRAMES as u64, "frame");

    let large_diffs: Vec<Value> = (0..LARGE_DIFFS)
        .map(|index| diff_block(index, LARGE_DIFF_BYTES))
        .collect();
    let small_diffs: Vec<Value> = (0..SMALL_DIFFS)
        .map(|index| diff_block(index, SMALL_DIFF_BYTES))
        .collect();
    let commentary = "rewrote the entry point and the tests that pinned the old behaviour";
    let big_output = "output line\n".repeat(MIB / 12);

    // A new tool call with ten 100 KiB diffs and no text: the detail is the first path.
    bench.run(
        "acp/reduce/tool-call/diffs-10x100KiB",
        large,
        || started(LARGE_FRAMES, &large_diffs),
        |frames| reduce(frames, Reducer::new()),
    );
    // The same diffs arriving as an update to a running call.
    bench.run(
        "acp/reduce/tool-call-update/diffs-10x100KiB",
        large,
        || {
            let frames = (0..LARGE_FRAMES)
                .map(|index| tool_call_update(index, "in_progress", &large_diffs))
                .collect();
            (frames, reducer_with_open_calls(LARGE_FRAMES))
        },
        |(frames, reducer)| reduce(frames, reducer),
    );
    // And completing the call with them.
    bench.run(
        "acp/reduce/tool-call-update/completed-diffs-10x100KiB",
        large,
        || {
            let frames = (0..LARGE_FRAMES)
                .map(|index| tool_call_update(index, "completed", &large_diffs))
                .collect();
            (frames, reducer_with_open_calls(LARGE_FRAMES))
        },
        |(frames, reducer)| reduce(frames, reducer),
    );
    // A diff-free call with a 1 MiB image ahead of a text block: the image is neither detail nor
    // content, and the text is both.
    bench.run(
        "acp/reduce/tool-call/image-1MiB-then-text",
        large,
        || started(LARGE_FRAMES, &[image_block(MIB), text_block(commentary)]),
        |frames| reduce(frames, Reducer::new()),
    );
    // A 1 MiB text body: it is both the detail and the `Output` content.
    bench.run(
        "acp/reduce/tool-call/text-1MiB",
        large,
        || started(LARGE_FRAMES, &[text_block(&big_output)]),
        |frames| reduce(frames, Reducer::new()),
    );
    // What most frames look like: two small diffs, or a short text.
    bench.run(
        "acp/reduce/tool-call/diffs-2x2KiB",
        small,
        || started(SMALL_FRAMES, &small_diffs),
        |frames| reduce(frames, Reducer::new()),
    );
    bench.run(
        "acp/reduce/tool-call/text-short",
        small,
        || started(SMALL_FRAMES, &[text_block(commentary)]),
        |frames| reduce(frames, Reducer::new()),
    );
}
