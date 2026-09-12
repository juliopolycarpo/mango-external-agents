# mango-agent-acp

Agent Client Protocol harness for
[mango-external-agents](https://github.com/juliopolycarpo/mango-external-agents).

```toml
[dependencies]
mango-external-agents = "0.1"
mango-agent-acp = "0.1"
```

The harness drives only the vendor's official CLI through its documented programmatic surface.
It never handles login: it runs whatever the user already logged into with the vendor's own CLI
and reports that state, nothing more. See `docs/compliance.md` and `docs/harness-acp.md` in
the repository.
