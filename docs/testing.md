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

## One integration binary per crate

Each crate's integration tests are modules of one binary, `tests/it/main.rs`, with a module per
file (`tests/it/<name>.rs`, and `tests/it/<name>/` for a module's own submodules). A target of its
own costs a link of the whole dependency tree and about a hundred megabytes in `target/`, so this
replaces 33 test binaries with 7. Add a test file as a module of that crate's `main.rs`; do not add
a `tests/<name>.rs` beside it.

- A test is named `<module>::<path>` under the binary `it`, so a filter that named a file now names
  its module: `cargo nextest run -p mango-agent-claude -E 'test(cleanup_required::)'`.
- A module that needs a feature is gated in `main.rs` with `#[cfg(feature = "...")]`, so the
  reduced-feature lanes still compile the others. `mango-agent-acp` needs `testing` for all of its
  modules, so its target declares `required-features` instead.
- `mango-agent-acp/tests/smoke.rs` stays a binary of its own: a profile's verification evidence
  names that file, and it only runs on request.
- The tradeoff is that one compile error in a module blocks every module of that crate, and a
  change to one file recompiles the whole binary.
- `nextest` still runs each test in its own process, so tests do not share statics. `cargo test`
  runs a binary's tests as threads of one process; do not rely on that.

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

- `scripts/check-features.sh powerset`: clippy over every feature pair, all targets. CI runs it in
  the `powerset` job, a matrix of Linux, macOS and Windows beside the `rust` job, not as a step of it.
- The bare core with all targets, the last line of `scripts/check-features.sh msrv`.
- `cargo clippy --workspace --all-targets --no-default-features` in `scripts/check.sh`, `ci.yml`
  and `fresh-dependencies.yml`.

The library-only runs (`cargo hack check --each-feature` in the msrv lane, `cargo check -p
mango-external-agents --all-features`) stay in the real tree. `scripts/test-check-powerset.sh` pins
which commands are wrapped, and that `ci.yml` runs the powerset in the `powerset` job on all three
systems (a `gate` need, the `rust` cache restored without saving) and no longer in `rust`.
`scripts/test-run-without-self-dev-dependency.sh` reproduces the probe in a throwaway workspace: the
in-tree `--all-targets` check passes with the feature off, and the same check through the runner
fails.

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

## Shell lint

`scripts/check-shellcheck.sh` runs shellcheck over every `*.sh` under `scripts/`, the fakes in
`scripts/test-fixtures/` included. `.shellcheckrc` makes it follow `# shellcheck source=`
directives. The Policy job in `ci.yml` installs shellcheck through `taiki-e/install-action` at an
exact version (`shellcheck@0.11.0`, so a new release cannot add findings to an unrelated change),
runs the gate, and fails when the tool is missing. The gate prints the version it ran under.

`scripts/check.sh` runs the gate when shellcheck is on `PATH` and otherwise warns once and skips
it, as it does for cargo-hack. To get the CI version without a system-wide install, take the
`shellcheck-v0.11.0.linux.x86_64.tar.xz` release asset from koalaman/shellcheck into an empty
directory, check its SHA-256 against the digest GitHub lists for the asset, extract it, and put the
`shellcheck` binary on `PATH` for the run.

A finding that is intentional gets `# shellcheck disable=SCxxxx # reason` above the command, never
a blanket exclusion in the gate. A directive cannot sit between the lines of a pipeline: put it
before the whole command. `scripts/test-check-shellcheck.sh` covers the gate with a named fake
shellcheck and, when the real tool is installed, with a script that has a finding.

## CI caches

Only runs on `refs/heads/main` save a Rust cache: the `rust` job (`shared-key: rust`), the
`powerset` job (`shared-key: powerset`), the `msrv` and `msrv-windows` jobs and the
`Fresh dependencies` lane (`key: fresh`) set `save-if: ${{ github.ref == 'refs/heads/main' }}`. The
powerset keeps its own cache because it builds dependencies with feature sets the `rust` job never
selects. The vendor-drift and release jobs restore the `rust` cache and save nothing
(`save-if: false`). A cache on a `refs/pull/*` ref belongs to
that pull request and is left alone.

A Swatinem/rust-cache key is `v0-rust-<name>-<os>-<arch>-<environment hash>-<lockfile hash>`, with
`<name>` the `shared-key` or the `key` plus the job id. A toolchain, `.cargo/config.toml` or
`Cargo.lock` change saves a new generation under a new hash and nothing restores the old one again.
`scripts/prune-rust-caches.py` removes them. A family is the key without its two trailing hashes
(`v0-rust-rust-Windows_NT-x64`); on `refs/heads/main` the script keeps the newest entry of each
family and selects the older ones. It never selects an entry on another ref, a key outside the
grammar, or entries tied for the newest `created_at`.

```sh
scripts/prune-rust-caches.py --repo juliopolycarpo/mango-external-agents           # dry run
scripts/prune-rust-caches.py --repo juliopolycarpo/mango-external-agents --delete  # deletes
```

The default is a dry run that prints each family with the entries it keeps and deletes and their
sizes. `--delete` calls `gh api --method DELETE` for exactly the selected ids; it needs a token with
`actions: write` and exits 1 if a listing is malformed or any delete fails. The `Rust cache cleanup`
workflow runs it with `--delete` after each successful `CI` push run on `main`, and on manual
dispatch with a `dry-run` input that defaults to true.
`scripts/test-prune-rust-caches.sh` covers the policy with a named fake `gh`
(`scripts/test-fixtures/fake-gh-cache-cli.sh`).

## Workflow lint

The shell inside a workflow's `run:` blocks is not under `scripts/`, so shellcheck's gate never sees
it. `scripts/check-actionlint.sh` runs actionlint over `.github/workflows/*.yml`: it checks the
workflow syntax and `${{ }}` expressions and passes each `run:` block to shellcheck. actionlint
skips that pass without a message when shellcheck is not on `PATH`, so the gate fails when either
tool is missing rather than reporting a clean run with the blocks unchecked. It prints both versions.

`taiki-e/install-action` has no actionlint manifest, so the Policy job installs it with
`scripts/install-actionlint.sh <bin-dir>`. The helper downloads the exact release asset from
rhysd/actionlint, refuses to extract it unless its SHA-256 matches
`scripts/actionlint-pinned-sha256sums.txt`, and appends the directory to `GITHUB_PATH` in a
workflow step. The pinned version is `ACTIONLINT_VERSION` in the helper; a bump changes it and the
registry together. Locally, run the helper into a directory of your choice and put that on `PATH`.
The shellcheck the Policy job installs is the one actionlint calls.

`scripts/check.sh` runs the gate when both tools are on `PATH` and otherwise warns once and skips it.
The release workflow's verify job does not install actionlint, so there the gate is skipped with that
warning unless the runner image provides it.

A finding that is intentional gets `# shellcheck disable=SCxxxx # reason` on the first line of the
`run:` block, never an `-ignore` flag in the gate. `scripts/test-check-actionlint.sh` covers the
gate with a named fake actionlint and, when both real tools are installed, with a `run:` block that
has a finding and one that carries a reasoned directive. `scripts/test-install-actionlint.sh` covers
the installer with a named fake curl and uname.
