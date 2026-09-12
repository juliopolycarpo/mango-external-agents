# mango-external-agents

Drive local coding-agent CLIs (Claude Code, OpenAI Codex, any Agent Client Protocol agent) from
Rust behind one abstraction with two independent axes: **transport kind** and **harness kind**.

It is a library, not a daemon. It spawns the vendor's official CLI through a launcher the host
injects, speaks the vendor's documented programmatic surface, and hands the host a normalised event
stream plus typed control calls (respond to an approval, steer, cancel, close). It never handles a
login, never reads or forwards a credential, never ships a vendor binary, and never lets a vendor
tool call reach the host's tools. See [`docs/compliance.md`](docs/compliance.md).

## The matrix

```
                     Transport kind
                     stdio            websocket             acp (official crate)
Harness kind  ┌──────────────────┬──────────────────────┬────────────────────────┐
claude        │ ✓ per-turn child │ ✗                    │ ✗ (shim = acp profile) │
codex         │ ✓ app-server     │ experimental, feature │ ✗ (shim = acp profile) │
acp (generic) │ ✓ child pipes    │ ✓ acp-http, feature  │ ✓ default              │
              └──────────────────┴──────────────────────┴────────────────────────┘
```

A harness declares which transport kinds it supports; the library refuses an unsupported pair with
a typed error before anything is spawned.

## Crates

| Crate                                                   | Role                                                                                   |
| ------------------------------------------------------- | -------------------------------------------------------------------------------------- |
| [`mango-external-agents`](crates/mango-external-agents) | Core: traits, events, permission matrix, host ports, stdio and websocket, `testing`    |
| [`mango-agent-claude`](crates/mango-agent-claude)       | Claude Code through `claude -p --output-format stream-json --input-format stream-json` |
| [`mango-agent-codex`](crates/mango-agent-codex)         | OpenAI Codex through `codex app-server` (JSON-RPC over lines)                          |
| [`mango-agent-acp`](crates/mango-agent-acp)             | Any ACP agent through the official `agent-client-protocol` crate, with profiles        |
| `examples/mea`                                          | Unpublished CLI for smoke runs and fixture capture                                     |

Versions are lockstep: one tag releases the four crates.

## A host in twenty lines

The host implements `ProcessLauncher` (or takes `TokioLauncher` from the `launcher-tokio`
feature), authorises a working directory, and reads events. Against the `testing` fakes it looks
like this; the types land with the core crate.

```rust,ignore
use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{AgentEvent, HostContext, OpenSession, TurnRequest};
use std::sync::Arc;

let launcher = FakeLauncher::scripted(include_str!("../fixtures/claude/transcripts/hello.ndjson"));
let host = HostContext::builder()
    .launcher(Arc::new(launcher))
    .cwd(std::env::current_dir()?)
    .client_info("my-host", env!("CARGO_PKG_VERSION"))
    .build();

let harness = mango_agent_claude::ClaudeHarness::default();
let session = harness.open_session(&host, OpenSession::default()).await?;
let mut turn = session.start_turn(TurnRequest::prompt("say hello")).await?;
while let Some(event) = turn.events.recv().await {
    match event {
        AgentEvent::TextDelta { text, .. } => print!("{text}"),
        AgentEvent::ApprovalRequested { request, .. } => session.respond(request.deny()).await?,
        AgentEvent::Completed { .. } => break,
        _ => {}
    }
}
session.close(CloseReason::Requested).await?;
```

## Documentation

- [`docs/adopt.md`](docs/adopt.md): how a host implements the ports and maps events
- [`docs/compliance.md`](docs/compliance.md): what each vendor permits and what the library does
- [`docs/harness-claude.md`](docs/harness-claude.md), [`docs/harness-codex.md`](docs/harness-codex.md), [`docs/harness-acp.md`](docs/harness-acp.md)
- [`docs/releasing.md`](docs/releasing.md)

## Status

Bootstrapped; the crates compile and declare their harness kinds. Behaviour lands crate by crate;
`CHANGELOG.md` tracks it. Rust 1.96 or newer, edition 2024, MIT.
