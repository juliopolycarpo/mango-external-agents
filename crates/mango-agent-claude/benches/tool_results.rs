//! The Claude reducer closing a call (a `tool_result` record in, the completion out) and starting
//! a file-change call (a `Write` or `Edit` `tool_use` in, the activity with its diff out), plus a
//! subagent's forwarded text blocks accumulating under the call that spawned it.
//!
//! The reducer keeps only the start of a result's text, so the cost of a call is what it takes to
//! find that start. A string payload is borrowed; an array payload is a list of text blocks that
//! has to be walked. The cases hold the payload constant and vary its shape and size: the
//! `array/*` cases are the ones a change to how an array is read can move, the `string/*` cases
//! are the control. Records are parsed in `setup`, so only `TurnReducer::reduce` is timed.
//!
//! ```sh
//! cargo bench -p mango-agent-claude --bench tool_results
//! ```

mod support;

use mango_agent_claude::protocol::StreamRecord;
use mango_agent_claude::reducer::TurnReducer;
use mango_external_agents::EventKind;
use serde_json::{Value, json};
use support::{Bench, Unit};

/// A line of the kind a `Read` or `Bash` result is made of, about 64 bytes.
const LINE: &str = "    let value = compute(input, &config).expect(\"expected a value\");\n";

/// `bytes` of source-like text, cut on a line boundary so no character is split.
fn text_of(bytes: usize) -> String {
    LINE.repeat(bytes.div_ceil(LINE.len()))
}

/// The payload shapes a `tool_result` arrives in.
enum Payload {
    /// One string, which the reducer borrows.
    Text(String),
    /// An array of `{"type": "text", "text": ...}` blocks, as an MCP tool or a subagent returns.
    Blocks(Vec<String>),
}

impl Payload {
    fn text(bytes: usize) -> Self {
        Self::Text(text_of(bytes))
    }

    /// `total` bytes of text split over `count` blocks.
    fn blocks(total: usize, count: usize) -> Self {
        Self::Blocks((0..count).map(|_| text_of(total / count)).collect())
    }

    fn json(&self) -> Value {
        match self {
            Self::Text(text) => json!(text),
            Self::Blocks(blocks) => Value::Array(
                blocks
                    .iter()
                    .map(|text| json!({"type": "text", "text": text}))
                    .collect(),
            ),
        }
    }
}

/// A reducer with `calls` Bash calls open, and the `tool_result` records that close them.
fn open_calls(calls: usize, payload: &Payload) -> (TurnReducer, Vec<StreamRecord>) {
    let mut reducer = TurnReducer::new();
    let content = payload.json();
    let mut results = Vec::with_capacity(calls);
    for call in 0..calls {
        let id = format!("toolu_{call}");
        let started = json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": "cat big"}}
        ]}})
        .to_string();
        let started = StreamRecord::parse(&started).expect("expected the bench record to parse");
        assert!(
            !reducer.reduce(&started).events.is_empty(),
            "expected tool_use {id} to open an activity"
        );
        let closing = json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": content}
        ]}})
        .to_string();
        results.push(StreamRecord::parse(&closing).expect("expected the bench record to parse"));
    }
    (reducer, results)
}

/// What a routine hands back: the reducer, the records it reduced and the event count. The runner
/// drops it after the timer stops, so freeing the parsed records is not charged to the reducer.
type Drained = (TurnReducer, Vec<StreamRecord>, usize);

fn close_all(mut reducer: TurnReducer, results: Vec<StreamRecord>) -> Drained {
    let mut output = 0;
    for record in &results {
        let events = reducer.reduce(record).events;
        assert!(
            matches!(
                events.as_slice(),
                [EventKind::ActivityCompleted { result, .. }] if result.detail.is_some()
            ),
            "expected exactly one completion carrying a detail, received {events:?}"
        );
        output += events.len();
    }
    (reducer, results, output)
}

/// The assistant record that starts `calls` file-change calls, and a fresh reducer to reduce it on.
///
/// `Write` carries one `content` string; `Edit` carries an `old_string` and a `new_string`. Each
/// string is `bytes` of source-like text.
fn file_change_records(calls: usize, tool: &str, bytes: usize) -> (TurnReducer, Vec<StreamRecord>) {
    let text = text_of(bytes);
    let input = match tool {
        "Write" => json!({"file_path": "/work/src/lib.rs", "content": text}),
        _ => json!({"file_path": "/work/src/lib.rs", "old_string": text, "new_string": text}),
    };
    let records = (0..calls)
        .map(|call| {
            let line = json!({"type": "assistant", "message": {"role": "assistant", "content": [
                {"type": "tool_use", "id": format!("toolu_{call}"), "name": tool, "input": input}
            ]}})
            .to_string();
            StreamRecord::parse(&line).expect("expected the bench record to parse")
        })
        .collect();
    (TurnReducer::new(), records)
}

fn start_all(mut reducer: TurnReducer, records: Vec<StreamRecord>) -> Drained {
    let mut output = 0;
    for record in &records {
        let events = reducer.reduce(record).events;
        assert!(
            matches!(
                events.as_slice(),
                [EventKind::ActivityStarted { activity, .. }] if activity.content.is_some()
            ),
            "expected exactly one started activity carrying a diff, received {events:?}"
        );
        output += events.len();
    }
    (reducer, records, output)
}

/// One open `Task` call and `blocks` text blocks of `bytes` each forwarded under it.
fn forwarded_blocks(blocks: usize, bytes: usize) -> (TurnReducer, Vec<StreamRecord>) {
    let mut reducer = TurnReducer::new();
    let started = json!({"type": "assistant", "message": {"role": "assistant", "content": [
        {"type": "tool_use", "id": "toolu_parent", "name": "Task", "input": {"prompt": "go"}}
    ]}})
    .to_string();
    let started = StreamRecord::parse(&started).expect("expected the bench record to parse");
    assert!(
        !reducer.reduce(&started).events.is_empty(),
        "expected the Task call to open an activity"
    );
    let text = text_of(bytes);
    let forwarded = json!({"type": "assistant", "parent_tool_use_id": "toolu_parent",
        "message": {"role": "assistant", "content": [{"type": "text", "text": text}]}})
    .to_string();
    let records = (0..blocks)
        .map(|_| StreamRecord::parse(&forwarded).expect("expected the bench record to parse"))
        .collect();
    (reducer, records)
}

fn forward_all(mut reducer: TurnReducer, records: Vec<StreamRecord>) -> Drained {
    let mut output = 0;
    for record in &records {
        let events = reducer.reduce(record).events;
        assert!(
            matches!(events.as_slice(), [EventKind::ActivityUpdated { .. }]),
            "expected one activity update per forwarded block, received {events:?}"
        );
        output += events.len();
    }
    (reducer, records, output)
}

fn main() {
    let bench = Bench::new("claude reducer (tool_result close, file-change start, forwarded text)");
    let cases: [(&str, usize, Payload); 8] = [
        ("claude/close/string/1MiB", 20, Payload::text(1 << 20)),
        ("claude/close/array-1x1MiB", 20, Payload::blocks(1 << 20, 1)),
        (
            "claude/close/array-100x10KiB",
            20,
            Payload::blocks(1 << 20, 100),
        ),
        (
            "claude/close/array-1x12KiB",
            300,
            Payload::blocks(12 << 10, 1),
        ),
        (
            "claude/close/array-1x32KiB",
            300,
            Payload::blocks(32 << 10, 1),
        ),
        (
            "claude/close/array-3x2KiB",
            1000,
            Payload::blocks(6 << 10, 3),
        ),
        ("claude/close/array-3x200B", 1000, Payload::blocks(600, 3)),
        ("claude/close/string/32KiB", 300, Payload::text(32 << 10)),
    ];
    for (name, calls, payload) in cases {
        bench.run(
            name,
            Unit::new(calls as u64, "call"),
            || open_calls(calls, &payload),
            |(reducer, results)| close_all(reducer, results),
        );
    }
    let file_changes: [(&str, usize, &str, usize); 4] = [
        ("claude/start/write-16KiB", 1000, "Write", 16 << 10),
        ("claude/start/write-200KiB", 100, "Write", 200 << 10),
        ("claude/start/edit-2x2KiB", 1000, "Edit", 2 << 10),
        ("claude/start/edit-2x100B", 1000, "Edit", 100),
    ];
    for (name, calls, tool, bytes) in file_changes {
        bench.run(
            name,
            Unit::new(calls as u64, "call"),
            || file_change_records(calls, tool, bytes),
            |(reducer, records)| start_all(reducer, records),
        );
    }
    bench.run(
        "claude/forward/1000x1KiB",
        Unit::new(1000, "block"),
        || forwarded_blocks(1000, 1 << 10),
        |(reducer, records)| forward_all(reducer, records),
    );
}
