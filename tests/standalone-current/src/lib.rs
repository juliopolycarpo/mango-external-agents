//! A small host using registry crates, with no product service or durable orchestration.

use mango_external_agents::{
    AgentEvent, CancelReason, CancelToken, CloseReason, Error, EventKind, ExecutablePath, Harness,
    HostContext, OpenSession, Result, TurnRequest,
};

/// Discovers a harness, runs one prompt, refuses approval requests, and closes the session.
///
/// The caller authorizes the cwd and chooses the launcher. Cancellation stops the active turn;
/// event consumption continues until its terminal. This example's policy always denies tools.
///
/// ```no_run
/// # async fn example(host: &mango_external_agents::HostContext) -> mango_external_agents::Result<()> {
/// independent_agent_host::run(
///     &mango_agent_codex::CodexHarness::new(), host,
///     mango_external_agents::CancelToken::new(), |event| println!("{:?}", event.kind),
/// ).await
/// # }
/// ```
pub async fn run(
    harness: &dyn Harness,
    host: &HostContext,
    cancel: CancelToken,
    mut event_received: impl FnMut(&AgentEvent),
) -> Result<()> {
    let discovery = harness.discover(host).await?;
    if !discovery.is_usable() {
        return Err(Error::HostConfiguration {
            expected: "an installed, usable vendor CLI with its own authentication",
            received: format!("discovery gate {:?}", discovery.gate),
        });
    }
    let mut request = OpenSession::new("chat-1");
    if let Some(executable) = discovery.executable {
        request = request.with_executable(ExecutablePath::resolved(executable));
    }
    let session = harness.open_session(host, request).await?;
    let outcome = async {
        let mut turn = session
            .start_turn(TurnRequest::new("turn-1", "Say hello"))
            .await?;
        let mut stopping = false;
        loop {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled(), if !stopping => {
                    stopping = true;
                    session.cancel(CancelReason::Requested).await?;
                    continue;
                }
                event = turn.recv() => event,
            };
            let Some(event) = event else {
                return Err(Error::Closed {
                    subject: "turn before its terminal event",
                });
            };
            if !stopping && let EventKind::ApprovalRequested { request } = &event.kind {
                session.respond(request.deny()?).await?;
            }
            event_received(&event);
            if event.is_terminal() {
                return Ok(());
            }
        }
    }
    .await;
    // Close even when the event loop failed. A cleanup failure takes precedence and retains its handle.
    session.close(CloseReason::Requested).await?;
    outcome
}
