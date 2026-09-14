# mango-external-agents

Drive local coding-agent CLIs (Claude Code, OpenAI Codex, any Agent Client Protocol agent) from
Rust behind one abstraction with two independent axes: **transport kind** and **harness kind**.

It is a library, not a daemon. It spawns the vendor's official CLI through a launcher the host
injects, speaks the vendor's documented programmatic surface, and hands the host a normalised event
stream plus typed control calls (respond to an approval, steer, cancel, close). It never handles a
login, never reads or forwards a credential, never ships a vendor binary, and never lets a vendor
tool call reach the host's tools. See [`docs/compliance.md`](docs/compliance.md).

## The matrix

| Harness      | Declared transport | Carrier                                          |
| ------------ | ------------------ | ------------------------------------------------ |
| Claude       | `stdio`            | Per-turn child process                           |
| Codex        | `stdio`            | App-server child process                         |
| ACP profiles | `acp`              | Child pipes, or HTTP with the `acp-http` feature |

Core also provides a WebSocket transport for host integrations. The Codex harness does not enable
OpenAI's experimental WebSocket interface. ACP over HTTP still uses the `acp` transport kind.

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
feature), authorises a working directory, and reads events. Replacing `FakeLauncher` with a real
launcher and `ClaudeHarness` with the vendor's own is the only difference from a production host;
the same example against `testing::FakeHarness` is a running doctest on the core crate.

```rust,ignore
use mango_external_agents::testing::FakeLauncher;
use mango_external_agents::{
    CloseReason, EventKind, Harness, HostContext, OpenSession, TurnRequest,
};
use std::sync::Arc;

let launcher = FakeLauncher::scripted(include_str!("../fixtures/claude/transcripts/hello.ndjson"));
let host = HostContext::builder()
    .launcher(Arc::new(launcher))
    .cwd(std::env::current_dir()?)
    .client_info("my-host", env!("CARGO_PKG_VERSION"))
    .build()?;

let harness = mango_agent_claude::ClaudeHarness::default();
let session = harness.open_session(&host, OpenSession::new("chat-1")).await?;
let mut turn = session.start_turn(TurnRequest::new("turn-1", "say hello")).await?;
while let Some(event) = turn.recv().await {
    match event.kind {
        EventKind::TextDelta { text } => print!("{text}"),
        EventKind::ApprovalRequested { request } => session.respond(request.deny()?).await?,
        EventKind::Completed => break,
        _ => {}
    }
}
session.close(CloseReason::Requested).await?;
```

An event carries its session, its turn and the instant it was stamped alongside its `kind`, so a
host can log or route one without matching on what happened first.

## Documentation

- [`docs/adopt.md`](docs/adopt.md): how a host implements the ports and maps events
- [`docs/compliance.md`](docs/compliance.md): what each vendor permits and what the library does
- [`docs/harness-claude.md`](docs/harness-claude.md), [`docs/harness-codex.md`](docs/harness-codex.md), [`docs/harness-acp.md`](docs/harness-acp.md)
- [`docs/releasing.md`](docs/releasing.md)

## Smoke CLI

Build `mea` from this repository. It is an unpublished diagnostic tool; v0.1 ships library crates
only, with no downloadable `mea` binaries.

```sh
cargo run -p mea -- doctor --json
cargo run -p mea -- discover --harness claude
cargo run -p mea -- turn --harness codex --cwd /path/to/workspace --json "say hello"
cargo run -p mea -- turn --harness acp --profile cursor "say hello"
cargo run -p mea -- capture --harness claude --out /tmp/claude-contract
```

`doctor` reports installation, version, gate and auth state for every registered harness. A
logged-out vendor gets its own login command as a hint. Unknown auth stays unknown. `turn` asks
for explicit terminal approval; redirected stdin denies requests. `--json` emits a JSON report for
probes and one complete event per line for turns. See [fixture capture rules](fixtures/README.md).

## Status

The core and all three harness crates are implemented. Rust 1.96 or newer, edition 2024, MIT.
Release and publishing steps are in [docs/releasing.md](docs/releasing.md).
