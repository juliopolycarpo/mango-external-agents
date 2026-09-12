# mango-external-agents

Core of [mango-external-agents](https://github.com/juliopolycarpo/mango-external-agents): the
`Harness` and `Session` traits, the normalised event model, the permission matrix, the host ports
(`ProcessLauncher`, `PermissionBroker`, `Clock`), the stdio and WebSocket transports, a JSON-RPC
client over a line link, and the `testing` fakes every harness crate is proven against.

```toml
[dependencies]
mango-external-agents = "0.1"
```

Harness crates plug in on top: `mango-agent-claude`, `mango-agent-codex`, `mango-agent-acp`.

The crate is a library, not a daemon. It spawns nothing on its own: the host injects the process
launcher, the working directory it authorised and the environment allowlist. It never handles a
vendor login and never reads, copies or forwards a credential — there is no login method on either
trait and no credential field on `HostContext`, so a host cannot pass one in even if it wanted to.

## A host, against the fakes

The same shape against a real harness; only the two `testing` types change. This example is
compiled and run as part of the test suite, so it cannot drift from the API.

```rust
# use std::sync::Arc;
use mango_external_agents::testing::{FakeHarness, FakeLauncher};
use mango_external_agents::{
    CloseReason, EventKind, Harness, HostContext, OpenSession, TurnRequest,
};

# #[tokio::main(flavor = "current_thread")]
# async fn main() -> Result<(), Box<dyn std::error::Error>> {
let host = HostContext::builder()
    .launcher(Arc::new(FakeLauncher::new()))
    .cwd(std::env::temp_dir())
    .client_info("my-host", "1.0.0")
    .build()?;

let harness = FakeHarness::new();
let session = harness.open_session(&host, OpenSession::new("chat-1")).await?;
let mut turn = session
    .start_turn(TurnRequest::new("turn-1", "say hello"))
    .await?;

while let Some(event) = turn.recv().await {
    match event.kind {
        EventKind::TextDelta { text } => print!("{text}"),
        // Nothing in the library answers this: the host decides, or its broker does.
        EventKind::ApprovalRequested { request } => session.respond(request.deny()?).await?,
        EventKind::Completed => break,
        _ => {}
    }
}
session.close(CloseReason::Requested).await?;
# Ok(())
# }
```

See [`docs/adopt.md`](https://github.com/juliopolycarpo/mango-external-agents/blob/main/docs/adopt.md)
for the ports a host implements and how a harness is proven against `testing::conformance`.

## Features

| Feature          | What it adds                                                                  |
| ---------------- | ----------------------------------------------------------------------------- |
| `stdio`          | Default. Child processes through the host's launcher, framed by lines         |
| `websocket`      | A dialled `ws://`/`wss://` endpoint (tokio-tungstenite over `ring`)           |
| `launcher-tokio` | `TokioLauncher`, for hosts with no spawner of their own                       |
| `testing`        | `FakeLauncher`, `FakeHarness`, `ScriptedLink`, `RecordingBroker`, conformance |

`--no-default-features` builds: a host that brings its own transport pays for nothing else.
