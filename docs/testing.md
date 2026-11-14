# Running one crate's tests

```sh
cargo nextest run -p <crate>
```

Each library crate lists itself as a dev-dependency with its test features, so the bare command
runs the feature-gated targets (`testing`, `unix-socket`, `websocket`, `launcher-tokio`) and
matches `--all-features`. The counts are identical with and without the flag:

| Command                                       | core | claude | codex | acp |
| --------------------------------------------- | ---: | -----: | ----: | --: |
| `cargo nextest run -p <crate>`                | 1271 |    656 |   743 | 631 |
| `cargo nextest run -p <crate> --all-features` | 1271 |    656 |   743 | 631 |

## The entry

```toml
[dev-dependencies]
mango-agent-claude = { path = ".", features = ["testing"] }
```

The core takes `launcher-tokio`, `testing`, `unix-socket` and `websocket`. Claude and ACP take
`testing`. Codex takes `testing` and `unix-socket`. With each crate's defaults that is
`--all-features`.

- `cargo package` drops a path-only dev-dependency from the published manifest, so no published
  crate depends on itself.
- `scripts/check-dev-dependencies.py` and `scripts/check-workspace-dependencies.py` accept exactly
  that shape: the crate's own name in the root `[dev-dependencies]`, `path = "."`, and a non-empty
  `features` list without `default`. Any other entry named like the crate is refused.
  `scripts/test-release.sh` covers the accepted shape and each refused one.
- The tarball has no entry, so `scripts/test-packages.sh` runs the extracted packages with
  `--all-features`.

## Feature lanes

Once a test, example or bench target is selected, cargo unifies the entry's features into every
target of the crate, library included. `cargo clippy --all-targets --no-default-features -p
mango-external-agents` builds the library with `default`, `launcher-tokio`, `stdio`, `testing`,
`unix-socket` and `websocket`. A lane that selects those targets stays green while it builds no
reduced-feature library or test target.

The probe is `#[cfg(not(feature = "testing"))] compile_error!(...)` in the core's `lib.rs` or in an
integration test:

| Lane                                                                               | Self entry | Probe fires |
| ---------------------------------------------------------------------------------- | ---------- | ----------- |
| `cargo hack clippy --feature-powerset --depth 2 -p mango-external-agents`          | present    | yes         |
| the same with `--all-targets`                                                      | present    | no          |
| the same with `--all-targets` through `scripts/run-without-self-dev-dependency.sh` | removed    | yes         |

Without `--all-targets` cargo builds no dev target and resolves no dev-dependency, so a
library-only lane is unaffected by the entry. A lane with test targets is not: no cargo switch
ignores one dev-dependency, and `cargo hack --no-dev-deps` drops the others too.
`scripts/run-without-self-dev-dependency.sh` runs a command in a scratch copy of the workspace
whose manifests lack the entry and whose `Cargo.lock` lacks the self edges. It refuses a copy whose
lock differs by anything else, a self entry it cannot strip, and a workspace with no entry.

These lanes run through it:

- `scripts/check-features.sh powerset`: clippy over every feature pair, all targets.
- The bare core with all targets, the last line of `scripts/check-features.sh msrv`.
- `cargo clippy --workspace --all-targets --no-default-features` in `scripts/check.sh`, `ci.yml`
  and `fresh-dependencies.yml`.

The library-only runs (`cargo hack check --each-feature` in the msrv lane, `cargo check -p
mango-external-agents --all-features`) stay in the real tree. `scripts/test-check-powerset.sh` pins
which commands are wrapped. `scripts/test-run-without-self-dev-dependency.sh` reproduces the probe
in a throwaway workspace: the in-tree `--all-targets` check passes with the feature off, and the
same check through the runner fails.

## Script tests

Every `scripts/test-*.sh` and `scripts/test-*.py` runs in both `scripts/check.sh` and `ci.yml`, or is
declared in `scripts/check-script-tests.sh` as run through a parent test that does. That script fails
for a test that only one of the two runs, one that neither runs, and a stale reference.

Naming a test in `ci.yml` is not enough. `scripts/ci-script-steps.py` parses the workflow as YAML
(PyYAML, `python3-yaml`) and counts a test as run only when:

- it is a command of a `run:` step, not text in an `echo` or a comment;
- its job is listed in the `needs` of the `gate` job, and neither the job nor the step sets
  `continue-on-error`;
- its job has no `if:`, and its step has no `if:` or only one `matrix.os == '<os>'` test naming an
  OS the job's matrix lists;
- no earlier line of the same `run:` block is an `exit`, and its own line does not end in
  `|| true`, `|| :` or `|| exit 0`;
- the workflow triggers on `pull_request` and has a `gate` job.

It does not follow shell conditionals inside a `run:` block, `set +e`, or `matrix.include` entries.
`scripts/test-check-script-tests.sh` pins each rule against a scratch workflow.
