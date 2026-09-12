# mango-agent-codex

OpenAI Codex harness for
[mango-external-agents](https://github.com/juliopolycarpo/mango-external-agents).

```toml
[dependencies]
mango-external-agents = "0.1"
mango-agent-codex = "0.1"
```

The harness drives only the vendor's official CLI through its documented programmatic surface.
It never handles login: it runs whatever the user already logged into with the vendor's own CLI
and reports that state, nothing more. See `docs/compliance.md` and `docs/harness-codex.md` in
the repository.
