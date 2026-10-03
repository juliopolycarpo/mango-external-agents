//! The event path a harness pays per emitted event, and what byte accounting adds to it.
//!
//! `EventSink::emit` normalizes the event, counts its serialized bytes against the turn budget and
//! queues it; `EventReceiver::try_recv` releases those bytes. The byte counter itself is private
//! to the crate, so the `emit+drain` cases are the authoritative number for it: a change to the
//! counter moves them. The `serialize/*` cases are a labelled stand-in, not the counter: the same
//! `serde_json::to_writer` into a counting writer that the counter uses today, run over the same
//! events, so the two can be compared. Run with:
//!
//! ```sh
//! cargo bench -p mango-external-agents --bench events
//! ```

mod support;

use std::sync::Arc;

use mango_external_agents::content::ActivityContent;
use mango_external_agents::host::SystemClock;
use mango_external_agents::normalize::{TextLimit, bound_text, sanitize_field};
use mango_external_agents::{
    Activity, ActivityKind, ActivityResult, ActivityStatus, AgentEvent, ApprovalDecision,
    AttemptId, DecisionSource, EventKind, EventReceiver, EventSink, Interaction, InteractionId,
    InteractionKind, Limits, PermissionEffect, PermissionOption, PermissionRequest,
    PermissionScope, SessionId, TurnId,
};
use support::{Bench, Unit};

/// Events emitted, drained or counted per sample, so a case is milliseconds rather than noise.
const EVENTS: usize = 1000;

/// A writer that only counts, the way the buffer's byte counter does.
#[derive(Default)]
struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Builds the events one case emits.
type Shape = fn() -> Vec<EventKind>;

fn sink() -> (EventSink, EventReceiver) {
    EventSink::with_limits(
        SessionId::new("bench-session"),
        TurnId::new("bench-turn"),
        AttemptId::FIRST,
        Arc::new(SystemClock),
        &Limits::default(),
    )
}

/// `EVENTS` text deltas of about 1 KiB, each `unit` repeated to fill.
fn text_deltas(unit: &str) -> Vec<EventKind> {
    let text = unit.repeat(1024 / unit.len().max(1) + 1);
    (0..EVENTS)
        .map(|_| EventKind::TextDelta { text: text.clone() })
        .collect()
}

/// `EVENTS` deltas of `text`, as short as a token or two: here the fixed fields (ids, attempt,
/// timestamp, tag) are most of what is serialized.
fn small_deltas(text: &str) -> Vec<EventKind> {
    let sanitized = sanitize_field(text).text;
    assert_eq!(
        sanitized, text,
        "expected the delta text to reach the buffer unchanged by normalization, received {sanitized:?}"
    );
    (0..EVENTS)
        .map(|_| EventKind::TextDelta {
            text: text.to_owned(),
        })
        .collect()
}

/// `EVENTS` approval requests with a title, a command detail and three options: the control class
/// the turn budget reserves room for.
fn approval_requests() -> Vec<EventKind> {
    (0..EVENTS)
        .map(|index| {
            let interaction = Interaction::new(
                InteractionId::new(format!("req-{index}")),
                InteractionKind::Permission,
                SessionId::new("bench-session"),
                std::time::SystemTime::UNIX_EPOCH,
            );
            let options = vec![
                PermissionOption::new("allow-once", PermissionEffect::Allow)
                    .with_scope(PermissionScope::Once),
                PermissionOption::new("allow-always", PermissionEffect::Allow)
                    .with_scope(PermissionScope::Session),
                PermissionOption::new("deny", PermissionEffect::Reject),
            ];
            let request = PermissionRequest::new(
                interaction,
                ActivityKind::Command,
                "Run `cargo test`",
                options,
            )
            .with_detail("cargo nextest run --workspace --all-features --locked\n".repeat(4));
            EventKind::ApprovalRequested { request }
        })
        .collect()
}

/// `EVENTS` answered approvals: a short payload that is all identifiers and tags.
fn approval_resolutions() -> Vec<EventKind> {
    (0..EVENTS)
        .map(|index| {
            let option = PermissionOption::new("allow-once", PermissionEffect::Allow)
                .with_scope(PermissionScope::Once);
            EventKind::ApprovalResolved {
                interaction_id: InteractionId::new(format!("req-{index}")),
                decision: ApprovalDecision::from_option(&option, DecisionSource::User),
            }
        })
        .collect()
}

/// `EVENTS` finished activities with a detail of a few hundred bytes.
fn completed_activities() -> Vec<EventKind> {
    (0..EVENTS)
        .map(|index| {
            let mut result = ActivityResult::new(ActivityStatus::Completed);
            result.detail = Some("test result: ok. 42 passed; 0 failed\n".repeat(8));
            EventKind::ActivityCompleted {
                call_id: format!("call-{index}"),
                result,
            }
        })
        .collect()
}

/// A started activity with a title, a detail and a few KiB of output content.
fn rich_activities() -> Vec<EventKind> {
    (0..EVENTS)
        .map(|index| {
            let mut activity = Activity::default();
            activity.name = String::from("Bash");
            activity.kind = ActivityKind::Command;
            activity.title = format!("cargo nextest run -p mango-external-agents --filter {index}");
            activity.detail = Some("running 42 tests\n".repeat(64));
            activity.item_id = Some(format!("item-{index}"));
            activity.content = Some(ActivityContent::Output {
                text: "test result: ok. 42 passed; 0 failed\n".repeat(64),
            });
            EventKind::ActivityStarted {
                call_id: format!("call-{index}"),
                activity,
            }
        })
        .collect()
}

/// Inputs for `normalize::bound_text` at the detail limit (4,096 code points), covering where the
/// first byte that is not plain ASCII sits: nowhere, near the start, late in the text, just before
/// the bound and after it.
fn bound_inputs() -> [(&'static str, String); 8] {
    let ansi = "\u{1b}[32mok\u{1b}[0m test result: 42 passed\n";
    [
        ("clean-ascii", "the quick brown fox ".repeat(210)),
        ("early-utf8", format!("é{}", "a".repeat(4_200))),
        (
            "late-utf8",
            format!("{}é{}", "a".repeat(4_000), "b".repeat(200)),
        ),
        (
            "late-esc",
            format!("{}\u{1b}[0m{}", "a".repeat(4_000), "b".repeat(200)),
        ),
        (
            "esc-at-4095",
            format!("{}\u{1b}[0m{}", "a".repeat(4_095), "b".repeat(200)),
        ),
        ("esc-after-bound", format!("{}\u{1b}[0m", "a".repeat(4_200))),
        (
            "short-title",
            String::from("cargo nextest run -p mango-external-agents"),
        ),
        ("ansi-command-output", ansi.repeat(4_200 / ansi.len() + 1)),
    ]
}

/// `EVENTS` started activities whose detail is `detail`, so the bound reaches `normalize`.
fn detail_activities(detail: &str) -> Vec<EventKind> {
    (0..EVENTS)
        .map(|index| {
            let mut activity = Activity::default();
            activity.name = String::from("Bash");
            activity.kind = ActivityKind::Command;
            activity.title = format!("cargo nextest run --filter {index}");
            activity.detail = Some(detail.to_owned());
            EventKind::ActivityStarted {
                call_id: format!("call-{index}"),
                activity,
            }
        })
        .collect()
}

/// Emits every event and reads each one back at once, the way a host that keeps up behaves.
fn emit_and_drain(rt: &tokio::runtime::Runtime, kinds: Vec<EventKind>) -> usize {
    rt.block_on(async {
        let (sink, mut events) = sink();
        let mut drained = 0;
        for kind in kinds {
            sink.emit(kind)
                .await
                .expect("expected the bench event to fit the default turn budget");
            events
                .try_recv()
                .expect("expected the emitted event to be queued");
            drained += 1;
        }
        drained
    })
}

/// The events a sink would queue for `kinds`, for the serialization stand-ins.
fn queued_events(rt: &tokio::runtime::Runtime, kinds: Vec<EventKind>) -> Vec<AgentEvent> {
    rt.block_on(async {
        let (sink, mut events) = sink();
        let mut queued = Vec::with_capacity(kinds.len());
        for kind in kinds {
            sink.emit(kind)
                .await
                .expect("expected the bench event to fit the default turn budget");
            queued.push(
                events
                    .try_recv()
                    .expect("expected the emitted event to be queued"),
            );
        }
        queued
    })
}

fn main() {
    let bench = Bench::new("events (emit + drain, serialization stand-ins, sanitising)");
    let rt = support::runtime();
    let per_event = Unit::new(EVENTS as u64, "event");

    // A dirty delta carries characters `sanitize_field` strips, so its output is a new string.
    let shapes: [(&str, Shape); 10] = [
        ("delta-16B-ascii", || small_deltas("hello, world....")),
        ("delta-64B-escapes", || small_deltas(&"a\"b\\".repeat(16))),
        ("approval-requested", approval_requests),
        ("approval-resolved", approval_resolutions),
        ("activity-completed", completed_activities),
        ("delta-1KiB-ascii", || text_deltas("the quick brown fox ")),
        ("delta-1KiB-escapes", || text_deltas("say \"hi\"\\n\t")),
        ("delta-1KiB-unicode", || text_deltas("héllo wörld 日本語 ")),
        ("delta-1KiB-dirty", || {
            text_deltas("clean\u{1b}[0m\u{0}text ")
        }),
        ("activity-rich", rich_activities),
    ];
    for (label, build) in shapes {
        bench.run(
            &format!("events/emit+drain/{label}"),
            per_event,
            build,
            |kinds| {
                let drained = emit_and_drain(&rt, kinds);
                assert_eq!(
                    drained, EVENTS,
                    "expected {EVENTS} events drained, received {drained}"
                );
            },
        );
    }

    // The same emit and drain with a detail whose first byte that is not plain ASCII sits late:
    // `bound_text` must not cost more here than it did before the clean-prefix copy.
    let details: Vec<(&str, String)> = bound_inputs()
        .into_iter()
        .filter(|(label, _)| matches!(*label, "late-utf8" | "late-esc" | "esc-at-4095"))
        .collect();
    for (label, detail) in &details {
        bench.run(
            &format!("events/emit+drain/activity-detail-{label}"),
            per_event,
            || detail_activities(detail),
            |kinds| {
                let drained = emit_and_drain(&rt, kinds);
                assert_eq!(
                    drained, EVENTS,
                    "expected {EVENTS} events drained, received {drained}"
                );
            },
        );
    }

    // The stand-in for the private `payload_bytes`: count the serialized size of events that are
    // already normalized and queued, and, for comparison, serialize them to a string as a host
    // would.
    for (label, build) in shapes {
        if !bench.selected(&format!("events/serialize-count/{label}"))
            && !bench.selected(&format!("events/serialize-string/{label}"))
        {
            continue;
        }
        let events = queued_events(&rt, build());
        bench.run(
            &format!("events/serialize-count/{label}"),
            per_event,
            || (),
            |()| {
                events
                    .iter()
                    .map(|event| {
                        let mut counter = CountingWriter::default();
                        serde_json::to_writer(&mut counter, event)
                            .expect("expected a normalized event to serialize");
                        counter.0
                    })
                    .sum::<usize>()
            },
        );
        bench.run(
            &format!("events/serialize-string/{label}"),
            per_event,
            || (),
            |()| {
                events
                    .iter()
                    .map(|event| {
                        serde_json::to_string(event)
                            .expect("expected a normalized event to serialize")
                            .len()
                    })
                    .sum::<usize>()
            },
        );
    }

    // `normalize::sanitize_field` on its own, where a clean string still gets a new allocation.
    let clean = "the quick brown fox ".repeat(52);
    let unicode = "héllo wörld 日本語 ".repeat(40);
    let dirty = "clean\u{1b}[0m\u{0}text ".repeat(64);
    for (label, input) in [
        ("ascii-clean", &clean),
        ("unicode-clean", &unicode),
        ("dirty", &dirty),
    ] {
        bench.run(
            &format!("normalize/sanitize_field-1KiB/{label}"),
            per_event,
            || (),
            |()| {
                (0..EVENTS)
                    .map(|_| sanitize_field(std::hint::black_box(input)).text.len())
                    .sum::<usize>()
            },
        );
    }

    // `normalize::bound_text` on its own at the detail limit, over every place the first byte that
    // is not plain ASCII can sit.
    for (label, input) in bound_inputs() {
        bench.run(
            &format!("normalize/bound_text-4KiB/{label}"),
            per_event,
            || (),
            |()| {
                (0..EVENTS)
                    .map(|_| {
                        bound_text(std::hint::black_box(&input), TextLimit::Detail)
                            .text
                            .len()
                    })
                    .sum::<usize>()
            },
        );
    }
}
