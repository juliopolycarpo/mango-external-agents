//! `mea turn`: one real turn against the installed CLI, printed as it arrives.
//!
//! The smoke test the fixtures cannot be: it spawns the vendor's own binary, opens a session,
//! runs a turn and exercises the calls a host makes around one — account usage, session listing,
//! cancel. What it never does is grant a permission. An approval is refused and the refusal is
//! printed, because a smoke tool that let an agent out of its sandbox to prove the plumbing works
//! would be proving the wrong thing.

use std::path::PathBuf;

use mango_agent_codex::CodexHarness;
use mango_external_agents::event::EventKind;
use mango_external_agents::{
    CancelReason, CloseReason, Harness, HostContext, OpenSession, Result, Session, SessionQuery,
    TurnRequest, TurnStream,
};

/// How long one turn is given before it is cancelled.
const TURN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);

/// Runs one turn and prints everything it produced.
///
/// # Errors
///
/// Whatever discovery, the launcher or the vendor reported.
pub async fn run(host: &HostContext, prompt: &str) -> Result<()> {
    let harness = CodexHarness::new();

    let discovery = harness.discover(host).await?;
    println!("gate: {:?}, auth: {:?}", discovery.gate, discovery.auth);
    if !discovery.is_usable() {
        println!("not usable on this machine; nothing to run");
        return Ok(());
    }

    let session = harness
        .open_session(host, OpenSession::new("mea-turn"))
        .await?;
    println!(
        "thread: {} (resumed: {})",
        session.ids().native_session_id,
        session.info().resumed
    );

    let mut turn = session
        .start_turn(TurnRequest::new("mea-turn-1", prompt))
        .await?;
    let ended = tokio::time::timeout(TURN_DEADLINE, print_turn(session.as_ref(), &mut turn)).await;
    if ended.is_err() {
        println!("(deadline passed; cancelling)");
        session.cancel(CancelReason::Timeout).await?;
    }

    match session.refresh_account_usage().await {
        Ok(usage) => println!("account usage: {:?}", usage.limits),
        Err(error) => println!("account usage: {error}"),
    }
    match session.list_sessions(SessionQuery::default()).await {
        Ok(page) => println!("threads on this machine: {}", page.sessions.len()),
        Err(error) => println!("thread list: {error}"),
    }

    session.close(CloseReason::Requested).await
}

/// Prints one turn's events, refusing anything it is asked to approve.
async fn print_turn(session: &dyn Session, turn: &mut TurnStream) {
    while let Some(event) = turn.recv().await {
        let terminal = event.is_terminal();
        match &event.kind {
            EventKind::TextDelta { text } => print!("{text}"),
            EventKind::ApprovalRequested { request } => {
                println!("\n[approval] {} — refusing", request.title);
                match request.deny() {
                    // Refused, never granted: see this module's own note.
                    Ok(refusal) => {
                        if let Err(error) = session.respond(refusal).await {
                            println!("[approval] the refusal was not accepted: {error}");
                        }
                    }
                    Err(error) => println!("[approval] no way to refuse: {error}"),
                }
            }
            other => println!("\n[{other:?}]"),
        }
        if terminal {
            println!();
            break;
        }
    }
}

/// The directory a turn runs in, which is the one the caller is standing in.
#[must_use]
pub fn default_workspace() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}
