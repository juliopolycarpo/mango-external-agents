//! The core's harness contract, run against this harness.
//!
//! What the suite checks is what a host is entitled to assume of any harness — a turn ends exactly
//! once, every event names its turn, a cancelled turn still completes, closing twice is harmless,
//! an undeclared capability refuses rather than misbehaves — so passing it is the claim that a host
//! written against the trait can drive Claude Code without knowing it is Claude Code.

use crate::support;

use std::sync::Arc;
use std::time::Duration;

use mango_agent_claude::ClaudeHarness;
use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::testing::conformance::{self, Outcome};
use support::{FakeClaudeCli, READ_TURN, Run, host};

/// What a second turn has shown when the host cancels it: a running command and a reasoning block
/// that is still streaming, neither of which the vendor will get to finish.
///
/// The suite's "a cancelled turn closes what it opened" rule can only fail a harness that has
/// something to close, and a cancel fake that opened nothing let one that never closed either
/// pass it.
fn work_in_flight() -> Vec<String> {
    let stream_event = |event: serde_json::Value| {
        serde_json::json!({"type": "stream_event", "event": event}).to_string()
    };
    vec![
        serde_json::json!({"type": "assistant", "message": {"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_cancelled", "name": "Bash",
             "input": {"command": "sleep 600"}}]}})
        .to_string(),
        stream_event(
            serde_json::json!({"type": "content_block_start", "index": 1,
            "content_block": {"type": "thinking"}}),
        ),
        stream_event(
            serde_json::json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "thinking_delta", "thinking": "weighing it"}}),
        ),
    ]
}

/// A build that replays the captured turn, then holds a second turn open to be cancelled.
fn scripted() -> Arc<FakeClaudeCli> {
    Arc::new(
        FakeClaudeCli::new()
            .with_turn(Run::replaying(READ_TURN))
            .with_turn(Run::stalling(work_in_flight())),
    )
}

#[tokio::test]
async fn the_claude_harness_is_conformant() {
    let launcher = scripted();
    let report = conformance::run(
        &ClaudeHarness::new(),
        &host(Arc::clone(&launcher)),
        conformance::Options {
            session_id: String::from("conformance-session"),
            prompt: String::from("read note.txt"),
            turn_timeout: Duration::from_secs(10),
        },
    )
    .await;

    report.assert_passed();
    assert!(
        launcher.all_children_ended(),
        "expected every child the suite started to be reaped"
    );
}

/// What the suite makes of a launcher that cannot start the CLI.
///
/// An empty `FakeLauncher::new()` refuses every launch with `Error::Launch`, which discovery now
/// returns instead of reading as "not installed". The suite records that check as skipped, with the
/// launch failure as the reason, because nothing was verified: a host whose own conformance run
/// used to pass this check on an empty launcher sees a skip until it scripts `--version` or uses
/// `FakeLauncher::missing_program()`, which answers "not installed" and passes. Whether a
/// discovery error should fail the suite instead is a separate decision.
#[tokio::test]
async fn an_unscripted_launcher_skips_the_discovery_check_and_a_missing_program_passes_it() {
    const CHECK: &str = "discovery stays within the declared ceiling";
    let outcome_over = |launcher: FakeLauncher| async move {
        let report = conformance::run(
            &ClaudeHarness::new(),
            &support::host_under(Arc::new(launcher), mango_external_agents::Limits::default()),
            conformance::Options {
                turn_timeout: Duration::from_secs(1),
                ..conformance::Options::default()
            },
        )
        .await;
        report
            .checks
            .into_iter()
            .find(|check| check.name == CHECK)
            .unwrap_or_else(|| panic!("expected the suite to record the check named {CHECK:?}"))
            .outcome
    };

    let unscripted = outcome_over(FakeLauncher::new()).await;
    assert!(
        matches!(&unscripted, Outcome::Skipped(why) if why.contains("discovery failed on this machine")),
        "expected a skip naming the discovery failure for an unscripted launcher | received: {unscripted:?}"
    );
    let missing = outcome_over(FakeLauncher::missing_program()).await;
    assert_eq!(
        missing,
        Outcome::Passed,
        "expected a launcher that reports no such program to pass the discovery check"
    );
}

/// The one check this harness cannot pass, and why a skip is the honest outcome.
///
/// The suite refuses the first approval a harness raises and asserts the refusal was accepted.
/// Claude Code raises none over its documented headless surface, so there is nothing to refuse —
/// and a skip says that, where a green tick would claim a round trip nobody made. Asserted here so
/// that a future build which *does* deliver an answerable approval turns this into a failing test
/// rather than into silence.
#[tokio::test]
async fn the_approval_round_trip_is_skipped_rather_than_claimed() {
    let report = conformance::run(
        &ClaudeHarness::new(),
        &host(scripted()),
        conformance::Options::default(),
    )
    .await;

    let skipped: Vec<&'static str> = report
        .skipped()
        .into_iter()
        .map(|check| check.name)
        .collect();
    assert_eq!(
        skipped,
        vec![
            "an approval can be answered",
            // Claude declares no `Questions` capability, so the suite's positive question check
            // has nothing to run against. Named here rather than filtered out: a build that starts
            // declaring questions should turn this into a failing test rather than into silence,
            // exactly like the approval above it.
            "a question round-trips with the vendor's own ids",
        ],
        "expected exactly these skips, received {:#?}",
        report.skipped()
    );

    let approval = report
        .checks
        .iter()
        .find(|check| check.name == "an approval can be answered")
        .expect("expected the approval check to have run");
    assert!(
        matches!(&approval.outcome, Outcome::Skipped(why) if why.contains("asked for no approval")),
        "received {:?}",
        approval.outcome
    );
}
