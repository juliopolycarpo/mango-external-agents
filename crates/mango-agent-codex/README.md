# mango-agent-codex

OpenAI Codex harness for
[mango-external-agents](https://github.com/juliopolycarpo/mango-external-agents).

```toml
[dependencies]
mango-external-agents = "0.8"
mango-agent-codex = "0.8"
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

## Attaching to a running app-server

With the `unix-socket` feature on Unix, `CodexHarness::with_unix_socket(path)` and
`OpenSession::over_transport(TransportKind::UnixSocket)` attach a session to an app-server the host
already runs with `codex app-server --listen unix://PATH`. The library never starts that server and
never presents a token; closing the session leaves the server running. `docs/harness-codex.md`
has the details and the vendor document.

## Host tests

The optional `testing` feature exports `testing::VersionAnsweringLauncher` and forwards the
core crate's `testing` feature. Enable it in a development dependency when the host injects a
`FakeLauncher` whose queued process belongs to the app-server rather than the preceding version
probe.

`VersionAnsweringLauncher::over(inner)` answers `codex --version` with the crate's
`MINIMUM_CODEX_VERSION` in the `codex-cli` banner, without consuming that queued process or adding
to its launch log. `new(inner).with_executable(path).with_version_line(line)` also answers the exact configured
executable and lets tests supply older or changed banners. Only a two-element version probe is
intercepted; other programs and arguments reach the inner launcher with their complete launch
specification, I/O, controls and typed errors unchanged.

This helper belongs to the unreleased 0.8 source. Published 0.7 consumers need a qualified
release before adopting the new feature.
