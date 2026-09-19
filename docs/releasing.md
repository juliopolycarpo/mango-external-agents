# Releasing

One git tag releases the four crates to crates.io in dependency order (`mango-external-agents`,
`mango-agent-claude`, `mango-agent-codex`, `mango-agent-acp`) and creates a GitHub release. Both the tag trigger and manual retries require a matching `refs/tags/v<version>` ref. No token is
stored in the repository or in CI.

## Cut a release

1. Make sure `main` is green and the changelog preview reads well:

   ```sh
   scripts/changelog.sh v0.2.0
   git diff CHANGELOG.md
   ```

2. Move the workspace to the new version: `version` under `[workspace.package]` and the four
   `version = "…"` entries under `[workspace.dependencies]` in `Cargo.toml`, then refresh the
   lockfile and prove the lockstep:

   ```sh
   cargo update --workspace
   scripts/check-versions.sh 0.2.0
   ```

3. Review the diff, commit, tag and push. The tag must be signed.

   ```sh
   git add Cargo.toml Cargo.lock CHANGELOG.md
   git commit -m "chore(release): v0.2.0" -m "Release the four crates at the same version."
   git tag -s v0.2.0 -m "v0.2.0"
   git push origin main v0.2.0
   ```

4. Watch the `Release` workflow. It verifies the manifests match the tag, runs `scripts/check.sh`,
   publishes each crate that is not on crates.io yet, and creates the GitHub release with
   git-cliff notes.

## First release checklist

Before publishing 0.1.0, run `scripts/check.sh` and `scripts/check-publish.sh`, and verify each
crate's docs with `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
--locked`.

`scripts/check-publish.sh` is the same gate the release workflow runs on the tag, brought forward
to any head. It wraps `cargo publish --workspace --dry-run --locked` — the workspace dry run stages
local crate dependencies, so it verifies all four packages before any of their names exist on
crates.io — and adds the two questions a dry run does not answer:

- **Is the publishable set still exactly four?** A new workspace member that forgets
  `publish = false` would otherwise join the lockstep release the first time somebody tags.
- **Does each tarball carry what it owes and nothing else?** A crate is immutable once it lands, so
  a missing README or licence, or a scratch directory that joined the package, costs a version
  number rather than an edit. Pass `--list` to read each crate's packaged files in full.

Run it on a clean tree: `cargo package --list` refuses to describe a package whose sources have
uncommitted changes, and the script surfaces that refusal rather than reporting an empty package.

Run `mea doctor` and a harmless `mea turn` against Claude, Codex and Cursor on the maintainer's
Linux and Windows installations. Pinned CI covers Linux, macOS and Windows public contracts;
authenticated turns require the maintainer's existing vendor login and are not a PR CI job.

After the first manual publish, check all four crate pages and docs.rs builds. Configure trusted
publishing before relying on an automatic later release. Publishing and tagging happen only after
the release PR is reviewed and merged; a passing PR does not prove a registry upload.

**The tag must point at the commit the tarballs name.** Every published `.crate` carries a
`.cargo_vcs_info.json` recording the commit it was packaged from. That value is immutable once the
version lands, so tagging a later head — even one whose `crates/` tree is byte-identical — leaves
the artifact pointing at a commit that is not the release. Read it back before tagging:

```sh
tar xzfO ~/.cargo/registry/cache/*/mango-external-agents-<version>.crate \
  mango-external-agents-<version>/.cargo_vcs_info.json
```

`mea` remains source-only and unpublished. No binaries are attached to v0.1 releases.

### 0.1.0, as executed

Recorded because the order differed from the numbered procedure above and a later reader should not
have to infer it. On 2026-09-19 the four crates were published by hand from `3ddd25e` — the merged
release-gate head — and `v0.1.0` was tagged on that same commit afterwards, once the artifacts were
verified: identical sources, README and licence in every tarball, all four resolving into a fresh
downstream build, docs.rs green, `ring`-only TLS in the resolved graph. Trusted publishing was
configured after the crates existed, which is the only order crates.io allows.

Two checklist items above were not satisfied before that publish: no authenticated Claude or Codex
turn, and no Windows lane. Both were run afterwards rather than before.

## Pre-releases

A pre-release tag (`v0.2.0-rc.1`) publishes to crates.io as a pre-release version, which Cargo
does not resolve from a caret requirement, and marks the GitHub release as a pre-release. Canary
tags (`v*-canary*`) are ignored by the workflow.

## What the workflow needs

The repository owner sets these up once.

- A GitHub environment named `release` whose deployment branch rule admits only tags matching
  `v*`. The publish job runs in it, so a push to a branch can never publish.
- crates.io trusted publishing for each of the four crates, configured in the crate settings on
  crates.io with this repository and the workflow file `release.yml`. The publish step uses
  `rust-lang/crates-io-auth-action` to exchange the OIDC token for a short-lived one; one exchange
  covers every crate that trusts this repository.

Trusted publishing can only be configured for a crate that already exists. The first version of
each crate is therefore published from a maintainer's machine, in dependency order:

```sh
cargo login
cargo publish -p mango-external-agents --locked
cargo publish -p mango-agent-claude --locked
cargo publish -p mango-agent-codex --locked
cargo publish -p mango-agent-acp --locked
```

After that, configure trusted publishing on all four crates and let the workflow handle every
later tag. The tag for that first version can still be pushed: the publish job checks crates.io
before asking for a token and skips every crate that is already there — which also means such a tag
exercises nothing of the trusted-publishing path. The first tag that has something left to publish
is the first one that proves it.

## When a release fails

The jobs run in order: verify, crates.io, GitHub release. A failure on the third crate leaves the
first two published; fix the cause and re-run the workflow from the failed job, which skips what is
already on the registry. Never delete a tag that published anything; cut the next patch.
