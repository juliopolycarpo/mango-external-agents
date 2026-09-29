//! The Codex notification pipeline, stage by stage and end to end.
//!
//! One frame takes this path: the raw line is parsed into a JSON value, `Notification::parse`
//! decodes it, `TurnReducer::reduce` turns it into events, `EventSink::emit` normalizes and queues
//! them, `EventReceiver::try_recv` drains them and the host serializes what it read. The `stage/*`
//! cases time one step on input prepared in `setup`; the `pipeline/*` cases time all of them, so a
//! change to one step shows both where it landed and what it did to the whole. Run with:
//!
//! ```sh
//! cargo bench -p mango-agent-codex --bench pipeline
//! ```

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use mango_agent_codex::protocol::notifications::Notification;
use mango_agent_codex::reducer::Outcome;
use mango_agent_codex::turn_reducer::{ACTIVITY_UPDATE_INTERVAL, TurnReducer};
use mango_external_agents::host::SystemClock;
use mango_external_agents::{AttemptId, EventReceiver, EventSink, Limits, SessionId, TurnId};
use serde_json::{Value, json};
use support::{Bench, Unit};
use tokio::time::Instant;

const THREAD: &str = "bench-thread";
const TURN: &str = "bench-turn";

/// Frames per sample for small frames, so a case is milliseconds rather than noise.
const SMALL_FRAMES: usize = 1000;

/// Frames per sample for large patch frames.
const PATCH_FRAMES: usize = 20;

/// How many times the captured transcripts are replayed per sample: one pass is about a
/// millisecond, which scheduler noise would swamp.
const FIXTURE_REPLAYS: usize = 10;

/// Bytes of diff in one large patch frame: five files of 100 KB.
const PATCH_FILES: usize = 5;
const PATCH_FILE_BYTES: usize = 100_000;

/// One raw wire frame: a method and its params, serialized the way the app-server writes it.
struct Frame {
    line: String,
}

impl Frame {
    fn new(method: &str, params: &Value) -> Self {
        Self {
            line: json!({ "method": method, "params": params }).to_string(),
        }
    }

    /// What the JSON-RPC client does with a line before the harness sees it.
    fn split(&self) -> (String, Value) {
        let Value::Object(mut frame) =
            serde_json::from_str::<Value>(&self.line).expect("expected the bench frame to be JSON")
        else {
            panic!("expected the bench frame to be a JSON object");
        };
        let method = frame
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .expect("expected the bench frame to name a method");
        let params = frame.remove("params").unwrap_or(Value::Null);
        (method, params)
    }

    fn notification(&self) -> Notification {
        let (method, params) = self.split();
        Notification::parse(&method, params)
    }
}

fn sink() -> (EventSink, EventReceiver) {
    EventSink::with_limits(
        SessionId::new("bench-session"),
        TurnId::new(TURN),
        AttemptId::FIRST,
        Arc::new(SystemClock),
        &Limits::default(),
    )
}

fn reduce(reducer: &mut TurnReducer, notification: &Notification, instant: Instant) -> Outcome {
    reducer.reduce(
        notification,
        THREAD,
        Some(TURN),
        SystemTime::UNIX_EPOCH,
        instant,
    )
}

/// A reducer that has been told about one running item, the way `item/started` does.
fn reducer_with_item(item: &Value, start: Instant) -> TurnReducer {
    let mut reducer = TurnReducer::new();
    let started = Frame::new(
        "item/started",
        &json!({"threadId": THREAD, "turnId": TURN, "item": item}),
    );
    let outcome = reduce(&mut reducer, &started.notification(), start);
    assert!(
        matches!(outcome, Outcome::Emit(_)),
        "expected the item to open an activity, received {outcome:?}"
    );
    reducer
}

/// Emits what `outcome` says and reads it back, returning the serialized bytes a host would see.
async fn emit_and_serialize(
    sink: &EventSink,
    events: &mut EventReceiver,
    outcome: Outcome,
) -> usize {
    let Outcome::Emit(kinds) = outcome else {
        return 0;
    };
    let mut bytes = 0;
    for kind in kinds {
        sink.emit(kind)
            .await
            .expect("expected the bench event to fit the default turn budget");
        let event = events
            .try_recv()
            .expect("expected the emitted event to be queued");
        bytes += serde_json::to_string(&event)
            .expect("expected a normalized event to serialize")
            .len();
    }
    bytes
}

/// `count` agent-message deltas of about 1 KiB, as the raw lines the app-server writes.
fn text_delta_frames(count: usize) -> Vec<Frame> {
    let text = "the quick brown fox jumps over the lazy dog. ".repeat(23);
    (0..count)
        .map(|_| {
            Frame::new(
                "item/agentMessage/delta",
                &json!({"threadId": THREAD, "turnId": TURN, "itemId": "msg-1", "delta": text}),
            )
        })
        .collect()
}

/// A patch of `PATCH_FILES` files of `PATCH_FILE_BYTES` bytes of diff each.
fn patch_changes() -> Value {
    let diff = "+added line of source code\n".repeat(PATCH_FILE_BYTES / 27);
    Value::Array(
        (0..PATCH_FILES)
            .map(|index| {
                json!({"path": format!("src/file{index}.rs"), "kind": {"type": "update"}, "diff": diff})
            })
            .collect(),
    )
}

fn patch_frames(count: usize) -> Vec<Frame> {
    (0..count)
        .map(|_| {
            Frame::new(
                "item/fileChange/patchUpdated",
                &json!({"threadId": THREAD, "turnId": TURN, "itemId": "patch-1", "changes": patch_changes()}),
            )
        })
        .collect()
}

fn main() {
    let bench = Bench::new("codex pipeline (parse, reduce, emit, drain, serialize)");
    let rt = support::runtime();
    let per_frame = Unit::new(SMALL_FRAMES as u64, "frame");
    let per_patch = Unit::new(PATCH_FRAMES as u64, "frame");
    let start = Instant::now();

    // The text pipeline: 1000 deltas of about 1 KiB, one message.
    bench.run(
        "codex/stage/wire-parse/text-delta-1KiB",
        per_frame,
        || text_delta_frames(SMALL_FRAMES),
        |frames| {
            for frame in &frames {
                // Dropped inside the timing: freeing the decoded tree is part of what a copy costs.
                std::hint::black_box(frame.notification());
            }
        },
    );
    bench.run(
        "codex/stage/reduce/text-delta-1KiB",
        per_frame,
        || {
            let notifications: Vec<Notification> = text_delta_frames(SMALL_FRAMES)
                .iter()
                .map(Frame::notification)
                .collect();
            (TurnReducer::new(), notifications)
        },
        |(mut reducer, notifications)| {
            for notification in &notifications {
                let outcome = reduce(&mut reducer, notification, start);
                assert!(
                    matches!(outcome, Outcome::Emit(_)),
                    "expected a text delta to emit, received {outcome:?}"
                );
            }
            reducer
        },
    );
    bench.run(
        "codex/pipeline/text-delta-1KiB",
        per_frame,
        || (TurnReducer::new(), text_delta_frames(SMALL_FRAMES)),
        |(mut reducer, frames)| {
            rt.block_on(async {
                let (sink, mut events) = sink();
                let mut bytes = 0;
                for frame in &frames {
                    let outcome = reduce(&mut reducer, &frame.notification(), start);
                    assert!(
                        matches!(outcome, Outcome::Emit(_)),
                        "expected a text delta to emit, received {outcome:?}"
                    );
                    bytes += emit_and_serialize(&sink, &mut events, outcome).await;
                }
                (reducer, bytes)
            })
        },
    );

    // Large patch updates: the reducer clones the diff before it decides to throttle it, and the
    // notification decoder clones params before it decodes them.
    let patch_item =
        json!({"type": "fileChange", "id": "patch-1", "changes": [], "status": "inProgress"});
    bench.run(
        "codex/stage/wire-parse/patch-update-500KB",
        per_patch,
        || patch_frames(PATCH_FRAMES),
        |frames| {
            for frame in &frames {
                // Dropped inside the timing: freeing the decoded tree is part of what a copy costs.
                std::hint::black_box(frame.notification());
            }
        },
    );
    bench.run(
        "codex/stage/reduce/patch-update-500KB/throttled",
        per_patch,
        || {
            let notifications: Vec<Notification> = patch_frames(PATCH_FRAMES)
                .iter()
                .map(Frame::notification)
                .collect();
            (reducer_with_item(&patch_item, start), notifications)
        },
        |(mut reducer, notifications)| {
            // The first update opens the window; the rest land inside it and are suppressed.
            let mut emitted = 0;
            for notification in &notifications {
                if matches!(reduce(&mut reducer, notification, start), Outcome::Emit(_)) {
                    emitted += 1;
                }
            }
            assert_eq!(
                emitted, 1,
                "expected 1 emitted patch update inside one window, received {emitted}"
            );
            reducer
        },
    );
    bench.run(
        "codex/stage/reduce/patch-update-500KB/emitting",
        per_patch,
        || {
            let notifications: Vec<Notification> = patch_frames(PATCH_FRAMES)
                .iter()
                .map(Frame::notification)
                .collect();
            (reducer_with_item(&patch_item, start), notifications)
        },
        |(mut reducer, notifications)| {
            // Each update lands a full window after the last, so every one is emitted.
            let step = ACTIVITY_UPDATE_INTERVAL + Duration::from_millis(1);
            let mut at = start;
            for notification in &notifications {
                at += step;
                let outcome = reduce(&mut reducer, notification, at);
                assert!(
                    matches!(outcome, Outcome::Emit(_)),
                    "expected a patch update a window later to emit, received {outcome:?}"
                );
            }
            reducer
        },
    );

    // A patch item announced with its content and completed: the activity is rendered on both.
    let changes = patch_changes();
    let started = Frame::new(
        "item/started",
        &json!({"threadId": THREAD, "turnId": TURN, "item": {
            "type": "fileChange", "id": "patch-2", "changes": changes, "status": "inProgress"}}),
    );
    let completed = Frame::new(
        "item/completed",
        &json!({"threadId": THREAD, "turnId": TURN, "item": {
            "type": "fileChange", "id": "patch-2", "changes": changes, "status": "completed"}}),
    );
    bench.run(
        "codex/stage/reduce/file-change-started+completed-500KB",
        per_patch,
        || {
            let pairs: Vec<(Notification, Notification)> = (0..PATCH_FRAMES)
                .map(|_| (started.notification(), completed.notification()))
                .collect();
            (TurnReducer::new(), pairs)
        },
        |(mut reducer, pairs)| {
            for (started, completed) in &pairs {
                let opened = reduce(&mut reducer, started, start);
                assert!(
                    matches!(opened, Outcome::Emit(_)),
                    "expected a file change to open an activity, received {opened:?}"
                );
                let closed = reduce(&mut reducer, completed, start);
                assert!(
                    matches!(closed, Outcome::Emit(_)),
                    "expected a file change to complete its activity, received {closed:?}"
                );
            }
            reducer
        },
    );

    // Command output: every delta beyond the first lands inside the throttle window.
    let command_item = json!({"type": "commandExecution", "id": "cmd-1",
                              "command": "cargo build", "status": "inProgress"});
    let output_text = "   Compiling mango-external-agents v0.3.0 (/work/crates)\n".repeat(19);
    let output_frames = |count: usize| -> Vec<Frame> {
        (0..count)
            .map(|_| {
                Frame::new(
                    "item/commandExecution/outputDelta",
                    &json!({"threadId": THREAD, "turnId": TURN, "itemId": "cmd-1", "delta": output_text}),
                )
            })
            .collect()
    };
    bench.run(
        "codex/stage/reduce/command-output-1KiB/throttled",
        per_frame,
        || {
            let notifications: Vec<Notification> = output_frames(SMALL_FRAMES)
                .iter()
                .map(Frame::notification)
                .collect();
            (reducer_with_item(&command_item, start), notifications)
        },
        |(mut reducer, notifications)| {
            for notification in &notifications {
                std::hint::black_box(reduce(&mut reducer, notification, start));
            }
            reducer
        },
    );

    replay_fixtures(&bench, &rt);
}

/// The notification frames of one captured transcript, and the thread and turn they name.
struct Transcript {
    name: String,
    frames: Vec<Frame>,
    thread: String,
}

fn collect_transcripts(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_transcripts(&path, found);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            found.push(path);
        }
    }
}

/// Every Codex transcript's server notifications (`<<` lines with a method and no id).
fn transcripts() -> Vec<Transcript> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/codex");
    let mut paths = Vec::new();
    collect_transcripts(&root, &mut paths);
    paths.sort();
    let mut found = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let mut frames = Vec::new();
        let mut thread = None;
        for line in text.lines().filter_map(|line| line.strip_prefix("<<")) {
            let Ok(Value::Object(frame)) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if frame.contains_key("id") || !frame.contains_key("method") {
                continue;
            }
            if thread.is_none() {
                thread = frame
                    .get("params")
                    .and_then(|params| params.get("threadId"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            frames.push(Frame {
                line: line.to_owned(),
            });
        }
        let name = path
            .file_stem()
            .map_or_else(String::new, |stem| stem.to_string_lossy().into_owned());
        if let (false, Some(thread)) = (frames.is_empty(), thread) {
            found.push(Transcript {
                name,
                frames,
                thread,
            });
        }
    }
    found
}

/// Pushes every captured notification through the pipeline, as a host would see the session.
fn replay_fixtures(bench: &Bench, rt: &tokio::runtime::Runtime) {
    let transcripts = transcripts();
    if transcripts.is_empty() {
        println!("# skipped codex/pipeline/fixtures: no fixtures/codex directory beside the crate");
        return;
    }
    let total: usize = transcripts
        .iter()
        .map(|transcript| transcript.frames.len())
        .sum();
    let names: Vec<&str> = transcripts.iter().map(|t| t.name.as_str()).collect();
    println!(
        "# fixture corpus: {} transcripts ({}), {total} notification frames ({FIXTURE_REPLAYS} replays)",
        transcripts.len(),
        names.join(", ")
    );
    bench.run(
        "codex/pipeline/fixtures",
        Unit::new(total as u64, "frame"),
        || (),
        |()| {
            rt.block_on(async {
                let mut emitted = 0;
                for _ in 0..FIXTURE_REPLAYS {
                    for transcript in &transcripts {
                        emitted += replay(transcript).await;
                    }
                }
                assert!(
                    emitted > 0,
                    "expected the captured transcripts to emit events, received none"
                );
                emitted
            })
        },
    );
}

/// One transcript through parse, reduce, emit, drain and serialize; returns events emitted.
///
/// A turn that ends starts a fresh reducer and sink, because a sink refuses events after its
/// terminal. The terminal itself is not emitted.
async fn replay(transcript: &Transcript) -> usize {
    let start = Instant::now();
    let mut reducer = TurnReducer::new();
    let (mut sink, mut events) = sink();
    let mut turn: Option<String> = None;
    let mut emitted = 0;
    for frame in &transcript.frames {
        let (method, params) = frame.split();
        if method == "turn/started" {
            turn = params
                .get("turn")
                .and_then(|turn| turn.get("id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        let notification = Notification::parse(&method, params);
        let outcome = reducer.reduce(
            &notification,
            &transcript.thread,
            turn.as_deref(),
            SystemTime::UNIX_EPOCH,
            start,
        );
        match outcome {
            Outcome::Emit(kinds) => {
                for kind in kinds {
                    if sink.emit(kind).await.is_err() {
                        continue;
                    }
                    if let Ok(event) = events.try_recv() {
                        emitted += 1;
                        std::hint::black_box(serde_json::to_string(&event).ok());
                    }
                }
            }
            Outcome::Finish { .. } | Outcome::Poison { .. } => {
                reducer = TurnReducer::new();
                (sink, events) = self::sink();
                turn = None;
            }
            Outcome::Ignore => {}
        }
    }
    emitted
}
