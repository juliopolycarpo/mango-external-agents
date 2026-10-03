# mango-external-agents

Drive local coding-agent CLIs (Claude Code, OpenAI Codex, any Agent Client Protocol agent) from
Rust behind one abstraction with two independent axes: **transport kind** and **harness kind**.

It is a library, not a daemon. It spawns the vendor's official CLI through a launcher the host
injects, speaks the vendor's documented programmatic surface, and hands the host a normalised event
stream plus typed control calls (respond to an approval, steer, cancel, close). It never handles a
login, never reads or forwards a credential, never ships a vendor binary, and never lets a vendor
tool call reach the host's tools. See [`docs/compliance.md`](docs/compliance.md).

## The matrix

| Harness      | Declared transport | Carrier                  |
| ------------ | ------------------ | ------------------------ |
| Claude       | `stdio`            | Per-turn child process   |
| Codex        | `stdio`            | App-server child process |
| ACP profiles | `acp`              | Child pipes              |

Core also provides a WebSocket transport for host integrations. The Codex harness does not enable
OpenAI's experimental WebSocket interface. ACP over HTTP is not shipped: the `agent-client-protocol-http`
crate pulls `aws-lc-rs` in under the ring-only TLS policy this workspace enforces.

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

## A host, step by step

The host implements `ProcessLauncher` (or takes `TokioLauncher` from the `launcher-tokio`
feature), authorises a working directory, and reads events. Two complete examples are compiled by
the test suite, so neither can drift from the API:

- [`crates/mango-external-agents/README.md`](crates/mango-external-agents/README.md) is a whole
  host against `testing::FakeLauncher` and `testing::FakeHarness`. It is a running doctest on the
  core crate with the `testing` feature enabled.
- [`crates/mango-agent-claude/README.md`](crates/mango-agent-claude/README.md) is the same shape
  against the real `ClaudeHarness`: build a `HostContext`, discover, open a session, stream a turn.
  It is compiled as a doctest and not run, because it needs the vendor CLI installed and signed in.

What separates the two is the launcher and the harness: a real host swaps `FakeLauncher` for its own
`ProcessLauncher` (or `TokioLauncher`) and `FakeHarness` for a vendor harness, and discovers the
CLI first, as the Claude example does.

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

The core and all three harness crates are implemented. Rust 1.97 or newer, edition 2024, MIT.
Release and publishing steps are in [docs/releasing.md](docs/releasing.md).
