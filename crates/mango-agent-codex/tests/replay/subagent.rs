//! A real Codex subagent attribution frame, replayed through the harness.
//!
//! `fixtures/codex/subagent.jsonl` is an authenticated capture (`mea capture --harness codex
//! --transcripts --scenario subagent --model gpt-5.6-terra`), not a value this suite wrote.
//!
//! These tests are deliberately distinct from the schema-shaped ones from #210
//! (`activity::a_subagent_item_preserves_only_its_own_thread_identity` and
//! `turn_reducer::a_schema_shaped_subagent_frame_keeps_its_child_identity_on_the_activity`). Those
//! parse a frame built from the pinned schema; these feed the bytes the vendor sent. The first
//! group below pins what makes the fixture a real capture, so replacing it with a synthetic one
//! fails here before any attribution assertion can pass on invented data.

use super::*;
use mango_external_agents::{Activity, ActivityKind, ActivityStatus};
use serde_json::Value;

/// The Codex build the capture ran against, as its `# capture` header and the server's own
/// `thread/started` both state it.
const CAPTURED_CODEX_VERSION: &str = "0.167.2";
/// The model the account supported; the default model of the capturing host was refused.
const CAPTURED_MODEL: &str = "gpt-5.6-terra";
/// `capturedAtUnixSeconds` in the `# capture` header: 2026-11-14.
const CAPTURED_AT_UNIX_SECONDS: u64 = 1_794_636_868;
/// The id the vendor gave the first `subAgentActivity` item.
const SPAWN_ITEM_ID: &str = "call_0cb240bfb687457e8c427a1291c058c7";
/// The id of the item that reported the child finished.
const COMPLETION_ITEM_ID: &str = "subagent-completed-01a1d8b1-7cb7-7cd1-a262-a7f25c475bef";

/// The recorded file read raw, because [`Transcript::load`] drops the `#` header lines this test
/// needs.
struct SubagentFixture {
    header: Vec<String>,
    transcript: Transcript,
}

impl SubagentFixture {
    fn load() -> Self {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/codex/subagent.jsonl");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "(subagent replay): expected the captured fixture at {} | received {error}",
                path.display()
            )
        });
        Self {
            header: raw
                .lines()
                .filter_map(|line| line.strip_prefix("# "))
                .map(str::to_owned)
                .collect(),
            transcript: Transcript::load("subagent"),
        }
    }

    /// The JSON the recorder wrote after `# capture `.
    fn capture_header(&self) -> Value {
        let line = self
            .header
            .iter()
            .find_map(|line| line.strip_prefix("capture "))
            .unwrap_or_else(|| {
                panic!(
                    "(subagent replay): expected a `# capture {{...}}` header | received {:?}",
                    self.header
                )
            });
        serde_json::from_str(line).unwrap_or_else(|error| {
            panic!("(subagent replay): expected capture header JSON | received {line:?}: {error}")
        })
    }

    /// The `# model: ...` header.
    fn header_model(&self) -> Option<&str> {
        self.header
            .iter()
            .find_map(|line| line.strip_prefix("model: "))
    }

    /// The parent thread's id: the one `thread/start` answered with.
    fn parent_thread(&self) -> String {
        self.transcript.thread_id().unwrap_or_else(|| {
            panic!("(subagent replay): expected the recording to name its parent thread | received none")
        })
    }

    /// Every `subAgentActivity` item the vendor sent, with the method that carried it.
    fn subagent_items(&self) -> Vec<(&str, &Value)> {
        self.transcript
            .every_received()
            .into_iter()
            .filter_map(|frame| {
                let item = frame.pointer("/params/item")?;
                (item["type"] == "subAgentActivity")
                    .then(|| (frame["method"].as_str().unwrap_or_default(), item))
            })
            .collect()
    }

    /// The nonempty `agentThreadId` every such item carries, once they agree.
    fn child_thread(&self) -> String {
        let ids: std::collections::BTreeSet<&str> = self
            .subagent_items()
            .into_iter()
            .filter_map(|(_, item)| item["agentThreadId"].as_str())
            .filter(|id| !id.is_empty())
            .collect();
        match ids.iter().collect::<Vec<_>>().as_slice() {
            [only] => (**only).to_owned(),
            other => panic!(
                "(subagent replay): expected one nonempty agentThreadId across the subAgentActivity items | received {other:?}"
            ),
        }
    }
}

#[test]
fn the_fixture_is_a_real_codex_capture_with_a_child_frame() {
    let fixture = SubagentFixture::load();

    let header = fixture.capture_header();
    assert_eq!(
        header["codexVersion"], CAPTURED_CODEX_VERSION,
        "(subagent replay): expected codexVersion {CAPTURED_CODEX_VERSION} | received {header}"
    );
    assert_eq!(
        header["capturedAtUnixSeconds"], CAPTURED_AT_UNIX_SECONDS,
        "(subagent replay): expected capturedAtUnixSeconds {CAPTURED_AT_UNIX_SECONDS} | received {header}"
    );
    assert_eq!(
        fixture.header_model(),
        Some(CAPTURED_MODEL),
        "(subagent replay): expected a `# model: {CAPTURED_MODEL}` header | received {:?}",
        fixture.header
    );

    // The running server states its own build and model on the thread it opens. A hand-written
    // file would have to repeat them consistently with the header to pass.
    let thread_started = fixture
        .transcript
        .every_received()
        .into_iter()
        .find(|frame| frame["method"] == "thread/started")
        .unwrap_or_else(|| {
            panic!("(subagent replay): expected a thread/started notification | received none")
        });
    assert_eq!(
        thread_started.pointer("/params/thread/cliVersion"),
        Some(&Value::from(CAPTURED_CODEX_VERSION)),
        "(subagent replay): expected the server to report cliVersion {CAPTURED_CODEX_VERSION} | received {thread_started}"
    );
    assert_eq!(
        thread_started.pointer("/params/thread/model"),
        Some(&Value::from(CAPTURED_MODEL)),
        "(subagent replay): expected the server to report model {CAPTURED_MODEL} | received {thread_started}"
    );

    // The child frame: a started and a completed item for one nonempty child thread that is not
    // the parent's, carried with the vendor's own emission timestamp.
    let parent = fixture.parent_thread();
    let child = fixture.child_thread();
    assert_ne!(
        child, parent,
        "(subagent replay): expected a child thread distinct from the parent | received {child} twice"
    );
    for method in ["item/started", "item/completed"] {
        assert!(
            fixture
                .subagent_items()
                .iter()
                .any(|(carried_by, item)| *carried_by == method && item["id"] == SPAWN_ITEM_ID),
            "(subagent replay): expected a subAgentActivity {SPAWN_ITEM_ID} on {method} | received {:?}",
            fixture.subagent_items()
        );
    }
    assert!(
        fixture
            .subagent_items()
            .iter()
            .any(|(_, item)| item["id"] == COMPLETION_ITEM_ID),
        "(subagent replay): expected the {COMPLETION_ITEM_ID} activity | received {:?}",
        fixture.subagent_items()
    );
    assert!(
        fixture
            .transcript
            .every_received()
            .into_iter()
            .filter(|frame| frame.pointer("/params/item/type")
                == Some(&Value::from("subAgentActivity")))
            .all(|frame| frame["emittedAtMs"].is_u64()),
        "(subagent replay): expected every subAgentActivity frame to carry the server's emittedAtMs | received a frame without it"
    );
    assert!(
        fixture
            .transcript
            .every_received()
            .into_iter()
            .any(|frame| frame.pointer("/params/item/type")
                == Some(&Value::from("collabAgentToolCall"))),
        "(subagent replay): expected a collabAgentToolCall item | received none"
    );
}

/// The capture header parser names what it received when the recorder's line is absent.
#[test]
#[should_panic(expected = "(subagent replay): expected a `# capture {...}` header | received")]
fn a_fixture_without_a_capture_header_is_refused_by_name() {
    let fixture = SubagentFixture {
        header: vec![String::from("model: gpt-5.6-terra")],
        transcript: Transcript::default(),
    };
    let _ = fixture.capture_header();
}

/// Two different child ids are two children, so attribution to one of them would be a guess.
#[test]
#[should_panic(expected = "(subagent replay): expected one nonempty agentThreadId")]
fn two_child_threads_in_one_fixture_are_refused_by_name() {
    let item = |id: &str| {
        serde_json::json!({"method": "item/started", "params": {"item": {
            "type": "subAgentActivity", "id": "i", "agentThreadId": id}}})
    };
    let fixture = SubagentFixture {
        header: Vec::new(),
        transcript: Transcript {
            steps: Vec::new(),
            greeting: vec![item("child-a"), item("child-b")],
        },
    };
    let _ = fixture.child_thread();
}

/// Every event kind and activity the replayed turn produced for the subagent.
fn subagent_activities(events: &[EventKind]) -> Vec<(&str, &Activity)> {
    events
        .iter()
        .filter_map(|event| match event {
            EventKind::ActivityStarted { call_id, activity }
                if activity.kind == ActivityKind::Subagent =>
            {
                Some((call_id.as_str(), activity))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn subagent_activities_pick_out_only_the_subagent_kind() {
    let events = vec![
        EventKind::ActivityStarted {
            call_id: String::from("a"),
            activity: Activity::new("subagent", ActivityKind::Subagent, "started"),
        },
        EventKind::ActivityStarted {
            call_id: String::from("b"),
            activity: Activity::new("collab", ActivityKind::Other, "started"),
        },
        EventKind::Completed,
    ];
    let found = subagent_activities(&events);
    assert_eq!(
        found.iter().map(|(call, _)| *call).collect::<Vec<_>>(),
        ["a"],
        "(subagent replay): expected only the subagent-kind activity | received {found:?}"
    );
}

/// The parent turn replays through the harness: the child's thread id from the vendor's own
/// `agentThreadId` becomes the activity's subagent attribution, and nothing else changes owner.
#[tokio::test]
async fn a_real_subagent_frame_attributes_the_activity_to_the_child_thread_and_grants_nothing() {
    let fixture = SubagentFixture::load();
    let parent = fixture.parent_thread();
    let child = fixture.child_thread();

    let broker = FixedBroker::new(BrokerDecision::Deny {
        reason: String::from("the host grants nothing during a subagent replay"),
    });
    let (host, launcher) = host_with(&["subagent"], Some(broker.clone()));
    let session = CodexHarness::new()
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("(subagent replay): expected a session over the recorded handshake");
    let mut turn = session
        .start_turn(TurnRequest::new(
            "turn-1",
            "Use exactly one subagent to reply with the single word mango.",
        ))
        .await
        .expect("(subagent replay): expected the recorded turn/start to be accepted");
    let events = drain(&mut turn).await;

    // The parent turn is the host's and ends exactly once, on the parent's own terminal frame:
    // the child thread's `turn/completed` rode the same connection and did not end it.
    let terminals = events
        .iter()
        .filter(|kind| matches!(kind, EventKind::Completed | EventKind::Error { .. }))
        .count();
    assert!(
        terminals == 1 && matches!(events.last(), Some(EventKind::Completed)),
        "(subagent replay): expected one terminal Completed, last | received {terminals} terminals, last {:?}",
        events.last()
    );
    assert!(
        matches!(events.first(), Some(EventKind::TurnStarted { .. })),
        "(subagent replay): expected TurnStarted first | received {:?}",
        events.first()
    );

    // The spawn and the completion report, both from the real frames, carry the child's id.
    let subagents = subagent_activities(&events);
    assert_eq!(
        subagents.len(),
        2,
        "(subagent replay): expected the spawn and the completion as two subagent activities | received {subagents:?}"
    );
    for (call_id, activity) in &subagents {
        assert_eq!(
            activity.subagent_id.as_deref(),
            Some(child.as_str()),
            "(subagent replay): expected subagent_id {child} from agentThreadId | received {:?} on {call_id}",
            activity.subagent_id
        );
        assert_ne!(
            activity.subagent_id.as_deref(),
            Some(parent.as_str()),
            "(subagent replay): expected the child, not the host's parent thread {parent} | received the parent"
        );
        assert_eq!(
            activity.parent_id, None,
            "(subagent replay): expected no parent activity | received {:?} on {call_id}",
            activity.parent_id
        );
        assert_eq!(
            activity.name, "subagent",
            "(subagent replay): expected the subagent activity name | received {:?}",
            activity.name
        );
        // The redacted agentPath has no field of its own and is not carried, parsed or not.
        let carried = serde_json::to_string(activity).expect("activity serialises");
        assert!(
            !carried.contains("REDACTED"),
            "(subagent replay): expected agentPath not to be carried | received {carried}"
        );
    }
    let item_ids: Vec<_> = subagents
        .iter()
        .map(|(_, activity)| activity.item_id.as_deref())
        .collect();
    assert_eq!(
        item_ids,
        [Some(SPAWN_ITEM_ID), Some(COMPLETION_ITEM_ID)],
        "(subagent replay): expected the vendor's item ids in order | received {item_ids:?}"
    );

    // The collaboration tool call names its sender and receivers as `[REDACTED]` and an empty
    // list. It still parses, and it does not invent an attribution.
    let collaboration: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            EventKind::ActivityStarted { activity, .. } if activity.kind == ActivityKind::Other => {
                Some(activity)
            }
            _ => None,
        })
        .collect();
    assert!(
        collaboration.len() == 1
            && collaboration[0].subagent_id.is_none()
            && collaboration[0].parent_id.is_none(),
        "(subagent replay): expected one collabAgentToolCall activity without attribution | received {collaboration:?}"
    );
    let completed: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                EventKind::ActivityCompleted { result, .. } if result.status == ActivityStatus::Completed
            )
        })
        .collect();
    assert_eq!(
        completed.len(),
        3,
        "(subagent replay): expected three completed activities (spawn, collab call, completion) | received {completed:?}"
    );

    // Nothing granted host tool execution: no approval or question reached the host, the host's
    // broker was never asked, and the library answered no server request.
    assert!(
        !events.iter().any(|event| matches!(
            event,
            EventKind::ApprovalRequested { .. }
                | EventKind::ApprovalResolved { .. }
                | EventKind::QuestionAsked { .. }
        )),
        "(subagent replay): expected no approval or question event | received {events:?}"
    );
    assert_eq!(
        broker.seen().len(),
        0,
        "(subagent replay): expected the host broker never asked | received {:?}",
        broker.seen()
    );
    let written: Vec<Value> = launcher
        .written()
        .iter()
        .map(|line| serde_json::from_str(line).expect("the library wrote JSON"))
        .collect();
    let answers: Vec<_> = written
        .iter()
        .filter(|frame| frame.get("method").is_none())
        .collect();
    assert!(
        answers.is_empty(),
        "(subagent replay): expected no response frames to the server | received {answers:?}"
    );
    // Every call addressed a thread, if any, that is the host's parent thread: nothing was
    // started, steered or approved on the child.
    let addressed: Vec<_> = written
        .iter()
        .filter_map(|frame| frame.pointer("/params/threadId").and_then(Value::as_str))
        .collect();
    assert!(
        !addressed.is_empty() && addressed.iter().all(|thread| *thread == parent),
        "(subagent replay): expected every addressed thread to be the parent {parent} | received {addressed:?}"
    );
}
