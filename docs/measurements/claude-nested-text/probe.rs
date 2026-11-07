use std::hint::black_box;

use mango_agent_claude::protocol::StreamRecord;
use mango_agent_claude::reducer::TurnReducer;
use serde_json::json;
use sha2::{Digest, Sha256};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const SCENARIOS: [&str; 6] = [
    "ordinary-ascii",
    "ordinary-unicode",
    "oversized-ascii",
    "oversized-unicode",
    "grow-then-oversized",
    "full-then-ignored",
];

fn records_for(scenario: &str) -> Vec<StreamRecord> {
    let blocks = match scenario {
        "ordinary-ascii" => vec!["a".repeat(256)],
        "ordinary-unicode" => vec![format!("{}{}", "a".repeat(64), "é😀漢".repeat(16)); 8],
        "oversized-ascii" => vec!["x".repeat(2 * 1024 * 1024)],
        "oversized-unicode" => vec!["😀".repeat(512 * 1024)],
        "grow-then-oversized" => vec!["seed".repeat(64), "x".repeat(2 * 1024 * 1024)],
        "full-then-ignored" => vec!["x".repeat(2 * 1024 * 1024), "y".repeat(2 * 1024 * 1024)],
        other => panic!("expected a registered scenario, received {other}"),
    };
    let opening = json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"task","name":"Task","input":{"prompt":"go"}}
    ]}});
    let mut records = vec![StreamRecord::parse(&opening.to_string()).expect("opening record")];
    records.extend(blocks.into_iter().map(|text| {
        let line = json!({"type":"assistant","parent_tool_use_id":"task", "message":{"content":[
            {"type":"text","text":text}
        ]}})
        .to_string();
        StreamRecord::parse(&line).expect("forwarded record")
    }));
    records
}

fn run(records: &[StreamRecord]) -> (TurnReducer, usize) {
    let mut reducer = TurnReducer::new();
    let mut events = 0;
    for record in records {
        let reduced = black_box(reducer.reduce(black_box(record)));
        assert!(reduced.init.is_none());
        events += reduced.events.len();
        black_box(&reduced);
    }
    (reducer, events)
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args[1] == "correctness" {
        for scenario in SCENARIOS {
            let records = records_for(scenario);
            let mut reducer = TurnReducer::new();
            let mut events = Vec::new();
            for record in &records {
                let reduced = reducer.reduce(record);
                assert!(reduced.init.is_none());
                events.extend(reduced.events);
            }
            events.extend(reducer.cancel());
            assert!(reducer.finished());
            let encoded = serde_json::to_vec(&events).expect("serialized events");
            println!(
                "{}",
                json!({"scenario":scenario,"events":events.len(),
                "serialized_bytes":encoded.len(),"sha256":format!("{:x}",Sha256::digest(&encoded))})
            );
        }
        return;
    }
    assert_eq!(args[1], "profile");
    let scenario = &args[2];
    // Inputs are parsed before profiling and borrowed by both the warm-up and measured run.
    let records = records_for(scenario);
    let (mut warm, warm_events) = run(&records);
    drop(black_box(warm.cancel()));
    drop(warm);
    let profiler = dhat::Profiler::builder().file_name(&args[3]).build();
    let (mut reducer, events) = run(&records);
    assert_eq!(events, warm_events);
    let active = dhat::HeapStats::get();
    let closed = reducer.cancel();
    let closing_events = closed.len();
    drop(closed);
    assert!(reducer.finished());
    let finished = dhat::HeapStats::get();
    drop(reducer);
    let released = dhat::HeapStats::get();
    drop(profiler);
    println!(
        "{}",
        json!({"scenario":scenario,"events":events,"closing_events":closing_events,
        "active_blocks":active.curr_blocks,"active_bytes":active.curr_bytes,
        "finished_blocks":finished.curr_blocks,"finished_bytes":finished.curr_bytes,
        "released_blocks":released.curr_blocks,"released_bytes":released.curr_bytes,
        "total_blocks":released.total_blocks,"total_bytes":released.total_bytes,
        "peak_blocks":released.max_blocks,"peak_bytes":released.max_bytes})
    );
}
