# mango-agent-codex

OpenAI Codex harness for
[mango-external-agents](https://github.com/juliopolycarpo/mango-external-agents).

```toml
[dependencies]
mango-external-agents = "0.1"
mango-agent-codex = "0.1"
```

Drives the `codex` CLI the user already installed, through `codex app-server` — the interface
OpenAI documents for rich clients. One long-lived process per session, JSON-RPC over lines, and
the vendor's own approval questions brokered to the host rather than answered here.

```no_run
use mango_agent_codex::CodexHarness;
use mango_external_agents::{Harness, HostContext, OpenSession, TurnRequest};

# async fn example(host: HostContext) -> mango_external_agents::Result<()> {
let harness = CodexHarness::new();

// What is installed on this machine, without reading a credential.
let discovery = harness.discover(&host).await?;
if !discovery.is_usable() {
    return Ok(());
}

let session = harness.open_session(&host, OpenSession::new("chat-1")).await?;
let mut turn = session.start_turn(TurnRequest::new("turn-1", "what changed here?")).await?;
while let Some(event) = turn.recv().await {
    println!("{:?}", event.kind);
}
# Ok(())
# }
```

The harness drives only the vendor's official CLI through its documented programmatic surface.
It never handles login: it runs whatever the user already logged into with the vendor's own CLI
and reports that state, nothing more. Vendor tools never enter the host's tool registry — a
request to run one is refused with a protocol error — and nothing here grants a permission.

The protocol types are hand-written against OpenAI's own published schema rather than copied from
its source tree; `docs/harness-codex.md` records why, along with the surface driven, the permission
matrix, and the known gaps. Compliance posture is in `docs/compliance.md`.
