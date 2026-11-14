# Running one crate's tests

Run a single crate with the feature flag:

```sh
cargo nextest run -p <crate> --all-features
```

Without `--all-features`, targets gated on a feature are skipped and the run still passes. Counts
at 0.8.0:

| Command                                       | core | claude | codex | acp |
| --------------------------------------------- | ---: | -----: | ----: | --: |
| `cargo nextest run -p <crate>`                | 1213 |    337 |   721 | 310 |
| `cargo nextest run -p <crate> --all-features` | 1271 |    651 |   743 | 631 |

The gated targets are the `testing` integration tests of Claude and ACP, the `unix-socket` tests of
Codex, and the `websocket`, `unix-socket`, `launcher-tokio` and `testing` tests of the core.

## Why the flag stays

A path-only dev-dependency of a crate on itself makes the bare command run everything:

```toml
[dev-dependencies]
mango-agent-claude = { path = ".", features = ["testing"] }
```

`cargo package` drops it from the published manifest, `scripts/check-dev-dependencies.sh` and
`scripts/check-versions.sh` can be taught to accept exactly that entry, and `cargo nextest run -p
<crate>` then matches `--all-features` in all four crates. It was built and measured, and it is
not adopted: cargo unifies the entry's features into every target of the crate, library included,
whenever dev targets are selected.

Probe: with `#[cfg(not(feature = "testing"))] compile_error!(...)` in the core's `lib.rs`,

- without the entry, `cargo hack clippy --feature-powerset --depth 2 --all-targets -p mango-external-agents`
  fails on the probe, as it should;
- with the entry, the same command and `cargo clippy --all-targets --no-default-features` both
  pass, and the library is built with `default`, `launcher-tokio`, `stdio`, `testing`,
  `unix-socket` and `websocket` under `--no-default-features`.

Every `--all-targets` feature lane in `scripts/check.sh` and CI would stay green while no longer
building a reduced-feature library or test target. That is the coverage the `required-features`
declarations and the powerset exist to give, so it costs more than the flag. Only
`scripts/check-msrv-features.sh`, which checks library targets alone, would be unaffected.

The entry becomes safe once those lanes build the library without `--all-targets` and keep reduced
feature test targets some other way. Until then, use `--all-features`.
