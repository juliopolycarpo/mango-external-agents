# mango-agent-claude

Claude Code harness for
[mango-external-agents](https://github.com/juliopolycarpo/mango-external-agents).

```toml
[dependencies]
mango-external-agents = "0.1"
mango-agent-claude = "0.1"
```

The harness drives only the vendor's official CLI through its documented programmatic surface —
`claude --print --output-format stream-json --input-format stream-json`, one child process per
turn. It never handles login: it runs whatever the user already logged into with the vendor's own
CLI and reports that state, nothing more.

```rust,no_run
use std::sync::Arc;
use mango_external_agents::{HostContext, EnvSource, OpenSession, TurnRequest, Harness};

# async fn example(launcher: Arc<dyn mango_external_agents::ProcessLauncher>) -> mango_external_agents::Result<()> {
let host = HostContext::builder()
    .launcher(launcher)              // the host owns the process
    .cwd(std::env::current_dir().expect("a working directory")) // the directory the host authorised
    .environment(EnvSource::from_process())
    .client_info("my-product", "1.0.0")
    .build()?;

let harness = mango_agent_claude::ClaudeHarness::new();
let discovery = harness.discover(&host).await?;
if !discovery.is_usable() {
    return Ok(()); // not installed, too old, or nobody is signed in
}

let session = harness.open_session(&host, OpenSession::new("chat-1")).await?;
let mut turn = session.start_turn(TurnRequest::new("turn-1", "summarise README.md")).await?;
while let Some(event) = turn.recv().await {
    println!("{:?}", event.kind);
}
# Ok(())
# }
```

Claude Code raises no answerable approval over its documented headless surface, so
`interactive_approvals` is false and a refused tool arrives as a failed activity carrying the
vendor's own reason. See `docs/harness-claude.md` in the repository for the measurement behind
that and for every other capability verdict, and `docs/compliance.md` for the posture.
