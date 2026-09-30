//! The Claude reducer closing a call: a `tool_result` record in, the completion out.
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

fn close_all(mut reducer: TurnReducer, results: Vec<StreamRecord>) -> (TurnReducer, usize) {
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
    (reducer, output)
}

fn main() {
    let bench = Bench::new("claude reducer (tool_result close)");
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
}
