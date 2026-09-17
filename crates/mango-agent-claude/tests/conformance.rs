//! The core's harness contract, run against this harness.
//!
//! What the suite checks is what a host is entitled to assume of any harness — a turn ends exactly
//! once, every event names its turn, a cancelled turn still completes, closing twice is harmless,
//! an undeclared capability refuses rather than misbehaves — so passing it is the claim that a host
//! written against the trait can drive Claude Code without knowing it is Claude Code.

mod support;

use std::sync::Arc;
use std::time::Duration;

use mango_agent_claude::ClaudeHarness;
use mango_external_agents::testing::conformance::{self, Outcome};
use support::{FakeClaudeCli, READ_TURN, Run, host};

/// A build that replays the captured turn twice, then holds a third turn open to be cancelled.
///
/// Three, not two: the suite's own `check_session_state` starts a turn to prove a session update
/// reaches a subscriber, before `check_turn` starts the one it inspects and `check_cancelled_turn`
/// starts the one it cancels. A queue with only two entries would starve `check_turn` of a
/// completing transcript and hand it the stalling one meant for the cancellation check instead.
fn scripted() -> Arc<FakeClaudeCli> {
    Arc::new(
        FakeClaudeCli::new()
            .with_turn(Run::replaying(READ_TURN))
            .with_turn(Run::replaying(READ_TURN))
            .with_turn(Run::stalling(Vec::<String>::new())),
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
        vec!["an approval can be answered"],
        "expected exactly one skip, received {:#?}",
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
