# Repository Guidelines

`AGENTS.md` is the canonical root instruction file for this repository. mango-external-agents is
a Rust library that drives coding-agent CLIs installed on the user's machine (Claude Code, OpenAI
Codex, any Agent Client Protocol agent) behind one abstraction with two independent axes:
**transport kind** (stdio, websocket, acp) and **harness kind** (claude, codex, acp with profiles).
It is a library, not a daemon: no listener, no service, no telemetry, no login handling.

## Command Guidelines

1. This repository is Rust only. Use `cargo`; there is no Bun, npm or Node toolchain here.
2. Lint and format: `cargo fmt`, `cargo clippy`, and `dprint` for markdown, TOML and YAML.
3. Run root scripts from the repository root: `scripts/check.sh`, `scripts/fix.sh`,
   `scripts/changelog.sh`. Read `scripts/` before inventing a command.
4. Tools the scripts expect on `PATH`: `cargo-nextest`, `cargo-deny`, `cargo-hack`, `dprint`,
   `git-cliff`. CI installs the same set through `taiki-e/install-action`.

## Working Loop

1. Read this file, then `docs/compliance.md` if the change touches a vendor surface.
2. Start from the closest entrypoint: a trait in core, a harness reducer, a fixture, a test.
3. Trace one layer outward at a time: core traits → harness crate → fixtures → `mea` → docs.
4. Run the smallest relevant validation first (one `cargo nextest run -p <crate>` filter), then
   `scripts/check.sh` before handoff.

## Layout

| Path                            | Owns                                                                                                                    |
| ------------------------------- | ----------------------------------------------------------------------------------------------------------------------- |
| `crates/mango-external-agents/` | Core: `Harness`/`Session` traits, events, normalisation, permission matrix, host ports, transports, testing             |
| `crates/mango-agent-claude/`    | Claude Code harness: stream-json dialect, reducer, auth probe, models, permissions                                      |
| `crates/mango-agent-codex/`     | Codex harness: app-server dialect, reducer, approvals, rate limits; vendored protocol types under `vendor/`             |
| `crates/mango-agent-acp/`       | Generic ACP harness over the official crate, per-agent profiles, `acp` transport                                        |
| `examples/mea/`                 | Unpublished CLI: discover, turn, capture, doctor (the smoke and drift tool)                                             |
| `fixtures/<vendor>/`            | Captured contracts and transcripts, replayed by the fakes                                                               |
| `docs/`                         | Compliance posture, host adoption guide, public contracts, the vendor field inventory, one guide per harness, releasing |
| `scripts/`                      | Shell: check, fix, changelog, lockstep versions, TLS rule, Codex vendoring                                              |

## Global Rules

- **No login, ever.** The library never authenticates a vendor, never opens a browser, never
  stores, reads, copies or forwards a token. It reports logged-in state only from a non-secret
  vendor surface and otherwise answers `Unknown`.
- **Official CLIs, documented surfaces only.** Every harness change cites the vendor document it
  follows, in the commit body and in `docs/harness-<vendor>.md`.
- **The host owns the process.** `ProcessLauncher`, the authorised working directory and the
  environment allowlist are injected; the library never spawns on its own initiative.
- **Approvals are brokered, never auto-answered.** Nothing in the library grants a permission.
- **Vendor tools never enter the host's tool registry**; vendor assistant text is never replayed
  into the host's own model context; stderr crossing a diagnostic boundary is redacted for
  credential-shaped text.
- **No dependency on `mango-protocol` or on mangostudio.** The runtime binary is where they meet.
- **TLS is `ring` everywhere.** `deny.toml` bans `aws-lc-rs` and `openssl`; `scripts/check-tls.sh`
  proves the tree is clean. `agent-client-protocol` and the smol family stay inside
  `mango-agent-acp`.
- **Vendor fixtures are captured by `mea capture`, never hand-edited.** Every `manifest.json`
  carries a SHA-256 digest per file beside it and `mea`'s test suite recomputes them, so an edited
  fixture fails `scripts/check.sh`; `mea digests` rewrites those digests from the committed files
  without running a vendor CLI. Public contracts are regenerated at the pinned vendor versions. A labelled historical capture stays fixed: it records
  behaviour that a current CLI cannot reproduce byte-for-byte, such as an authenticated transcript
  or a help surface predating a feature. See `fixtures/README.md`. The Codex wire contract is
  vendored as OpenAI's own schema, not as its Rust sources: `scripts/vendor-codex.sh` regenerates
  `crates/mango-agent-codex/vendor/` from `codex app-server generate-json-schema` at the pinned
  version, and the file is never patched in place. See `docs/harness-codex.md` for why the source
  tree is not vendored.
- **Toolchain policy.** `rust-toolchain.toml` is bumped within a week of a stable release;
  `rust-version` is stable − 2 and enforced by the MSRV lane.
- Every new function gets a test. A bug fix gets a regression test that fails first with the
  expected shape. Mock external I/O with named fake classes, not inline stubs.
- Error messages include the received value and the expected shape.
- `unsafe_code` is forbidden; `missing_docs` and `clippy::unwrap_used` are warnings that CI
  denies. Keep changes scoped; do not reformat unrelated files.

## Commits and pull requests

- Conventional Commits with a body: `type(scope): summary`. Scopes: `core`, `claude`, `codex`,
  `acp`, `mea`, `fixtures`, `docs`, `ci`, `build`, `deps`, `release`. One concern per commit.
- Signing and sign-off come from git config. Never write `Signed-off-by:` or `Co-authored-by:`
  trailers by hand.
- `CHANGELOG.md` is generated by git-cliff (`scripts/changelog.sh`); never edit it by hand.
- Versions are lockstep across the four crates: bump `[workspace.package]` and the
  `[workspace.dependencies]` entries together; `scripts/check-versions.sh` enforces it.
- PRs follow `.github/pull_request_template.md`, including the compliance-impact checkbox.

## Validation

After every change run `scripts/check.sh`. If it fails, run `scripts/fix.sh` and check again.
