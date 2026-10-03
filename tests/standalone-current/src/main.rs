use std::sync::Arc;

use mango_external_agents::{CancelToken, EnvSource, Harness, HostContext};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let choice = std::env::args().nth(1).unwrap_or_else(|| "codex".into());
    let harness: Box<dyn Harness> = match choice.as_str() {
        "claude" => Box::new(mango_agent_claude::ClaudeHarness::new()),
        "codex" => Box::new(mango_agent_codex::CodexHarness::new()),
        "cursor" => {
            Box::new(mango_agent_acp::AcpHarness::builtin("cursor").ok_or("missing profile")?)
        }
        _ => return Err(format!("received {choice:?}; expected claude, codex or cursor").into()),
    };
    // Running this CLI authorizes the caller's current directory. A service must authorize it itself.
    let host = HostContext::builder()
        .launcher(Arc::new(
            mango_external_agents::launcher::TokioLauncher::new(),
        ))
        .cwd(std::env::current_dir()?)
        .environment(EnvSource::from_process())
        .client_info("independent-agent-host", env!("CARGO_PKG_VERSION"))
        .build()?;
    let cancel = CancelToken::new();
    let signal = tokio::spawn({
        let cancel = cancel.clone();
        async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        }
    });
    let result = independent_agent_host::run(harness.as_ref(), &host, cancel, |event| {
        println!("{:?}", event.kind);
    })
    .await;
    signal.abort();
    result?;
    Ok(())
}
