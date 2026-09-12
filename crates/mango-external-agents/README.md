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
vendor login and never reads, copies or forwards a credential.

Features: `stdio` (default), `websocket`, `launcher-tokio`, `testing`.
