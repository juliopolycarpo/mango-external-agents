# Releasing

One git tag releases the four crates to crates.io in dependency order (`mango-external-agents`,
`mango-agent-claude`, `mango-agent-codex`, `mango-agent-acp`) and creates a GitHub release. The tag
is the only trigger; no token is stored in the repository or in CI.

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
   git add -A && git commit -m "chore(release): v0.2.0"
   git tag -s v0.2.0 -m "v0.2.0"
   git push origin main v0.2.0
   ```

4. Watch the `Release` workflow. It verifies the manifests match the tag, runs `scripts/check.sh`,
   publishes each crate that is not on crates.io yet, and creates the GitHub release with
   git-cliff notes.

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
before asking for a token and skips every crate that is already there.

## When a release fails

The jobs run in order: verify, crates.io, GitHub release. A failure on the third crate leaves the
first two published; fix the cause and re-run the workflow from the failed job, which skips what is
already on the registry. Never delete a tag that published anything; cut the next patch.
