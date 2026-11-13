# Releasing

One git tag releases the four crates to crates.io in dependency order (`mango-external-agents`,
`mango-agent-claude`, `mango-agent-codex`, `mango-agent-acp`) and creates a GitHub release. Both the tag trigger and manual retries require a matching `refs/tags/v<version>` ref. No token is
stored in the repository or in CI.

## Cut a release

Use `RELEASE_VERSION` for the intended new version throughout the procedure. For example,
`RELEASE_VERSION=0.8.0` is illustrative, not a statement of the latest release. For a prerelease,
set the full suffix too. These commands belong to the reviewed release process, not to an
ordinary documentation PR.

1. Make sure `main` is green and the changelog preview reads well:

   ```sh
   scripts/changelog.sh "v$RELEASE_VERSION"
   git diff CHANGELOG.md
   ```

2. Move the workspace to the new version: `version` under `[workspace.package]` and the four
   `version = "…"` entries under `[workspace.dependencies]` in `Cargo.toml`. Set the install
   snippet in each of the four `crates/*/README.md` files to the new `major.minor` (`"0.8"` for
   the illustrative 0.8.x release): a published README is immutable, and 0.3.0 shipped `"0.1"`, which a caret requirement
   never resolves to 0.3. For a pre-release, write the full version with its suffix instead
   (`"0.8.0-rc.1"`), because no caret requirement resolves to one. Then refresh the lockfile and
   prove the lockstep; the same script refuses a README snippet that breaks this rule:

   ```sh
   cargo update --workspace
   scripts/check-versions.sh "$RELEASE_VERSION"
   ```

3. Review and merge the release PR after the pre-publication requirements below pass.
   On the resulting clean release commit, tag and push. The tag must be an annotated, signed tag: the `Release`
   workflow refuses one that GitHub does not report as verified, so add your signing key to your
   GitHub account as a signing key first. This checks that GitHub verified the signature, not which
   maintainer made it.

   ```sh
   git tag -s "v$RELEASE_VERSION" -m "v$RELEASE_VERSION"
   git push origin main "v$RELEASE_VERSION"
   ```

4. Watch the `Release` workflow. It verifies the tag signature and that the manifests match the tag, runs `scripts/check.sh`,
   runs the isolated minimum-Rust feature check (`scripts/check-msrv-features.sh --locked`, on the
   `rust-version` the script reads from `Cargo.toml`, one package at a time), publishes each crate
   that is not on crates.io yet, and creates the GitHub release with git-cliff notes. The checks
   run in the verify job, which the publish job needs, so a tag that fails the minimum-Rust check
   publishes nothing. A full rerun is safe: a crate already on crates.io and a GitHub release that
   already exists are skipped, and the existing release keeps its notes.

5. Bump the current registry consumer once the release is on crates.io. The consumer can only name
   a version the registry already has, so the release pull request cannot do this. In a follow-up
   pull request, change the four `=` pins in `tests/standalone-current/Cargo.toml` to the new
   version, set its `rust-version` (and "Rust … or newer" in its README) to the minimum the
   published release declares, update only those four lock entries with that compiler, and test
   it. Update the literal published pin in `docs/adopt.md` in that same follow-up.
   `scripts/check-standalone.sh` fails if the consumer's `rust-version` differs from the one the
   pinned crates declare:

   ```sh
   export RUSTUP_TOOLCHAIN=$(scripts/check-msrv-features.sh --print-toolchain)
   cargo update --manifest-path tests/standalone-current/Cargo.toml \
     -p mango-external-agents -p mango-agent-acp -p mango-agent-claude -p mango-agent-codex
   scripts/check-standalone.sh --test-only current
   ```

   `scripts/check-versions.sh` delegates the pin policy to `scripts/check-consumer-pin.sh`.
   That script accepts these states (numeric comparisons refer to `major.minor.patch`):

   - The exact workspace version, including its suffix if present.
   - A stable pin at the previous patch in the same major/minor when the workspace patch is
     greater than zero.
   - When the workspace patch is zero and its minor is greater than zero, any stable patch in
     the previous minor of the same major.
   - When the workspace minor and patch are zero and its major is greater than zero, any stable
     release in the previous major.
   - A prerelease pin with the same numeric major/minor/patch as the workspace. The script does
     not order prerelease suffixes; registry availability is checked separately by the consumer.

   Older or unrelated pins fail this policy. Update the current consumer after each publication;
   the allowance accommodates the release PR before the registry upload, not a reason to leave
   the consumer behind. `tests/standalone-historical` stays at `=0.3.1`; never bump it.

## Pre-publication requirements

For every release, run `scripts/check.sh`, `scripts/check-publish.sh --list` and
`scripts/check-msrv-features.sh --locked` on the intended release sources. Verify crate docs with
`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`.
The isolated feature check needs the declared minimum Rust toolchain installed. `scripts/check.sh`
also runs the extracted core package unit tests with an isolated prerelease version, using the
checkout’s selected Rust toolchain.

`scripts/check-publish.sh` wraps `cargo publish --workspace --dry-run --locked`. The workspace dry
run stages local crate dependencies, so it verifies the four packages together. It also verifies
that the publishable set is exactly the four library crates and that each package carries its
README and licence without build, scratch or environment files. Run it on a clean tree:
`cargo package --list` refuses sources with uncommitted changes. Inspect the packaged files before
publishing; a registry artifact is immutable.

The three harness crates ship local copies for their fixture-based tests and Codex's pipeline
benchmark. Captures produced by `mea capture` remain canonical under the repository's `fixtures/`
directory, with their existing
manifest digests. Run `scripts/package-fixtures.py` after a capture changes and commit the derived
crate-local copies with that change. `scripts/package-fixtures.py --check` rejects missing, stale
or extra copies; `scripts/check.sh` and CI run that check.

The publication gate runs `scripts/test-packages.sh`: it extracts all four archives outside the
repository and runs their tests without a root-fixture fallback. Before the new core version is
published, the helper patches each extracted archive's core dependency only to the extracted core
archive. The selected Rust toolchain follows the tests outside the checkout.

Core's captured-transcript framing cases still use the checkout's fixture corpus and skip when
it is absent. Its generated-input framing cases remain usable from an archive. This
[benchmark scope decision](https://github.com/juliopolycarpo/mango-external-agents/issues/340)
keeps the non-gating cross-vendor corpus out of the core package; all four archives' shipped tests
are covered by the standalone test gate.

For pre-commit archive testing, `scripts/test-packages.sh --allow-dirty` explicitly tests working-tree
bytes. Release qualification uses the clean-tree publication gate above. The earlier missing-fixture
investigation remains in
[the packaged-test documentation change](https://github.com/juliopolycarpo/mango-external-agents/pull/74);
its failure counts describe the historical layout.

Run `mea doctor` and a harmless `mea turn` against Claude, Codex and Cursor on the maintainer's
Linux and Windows installations. Pinned CI covers Linux, macOS and Windows public contracts;
authenticated turns require the maintainer's existing vendor login and are not a PR CI job.
Documentation or fixture-provenance corrections do not establish a fresh authenticated turn or
native Windows qualification. Record which versions and platforms were actually tested.

After publication, check all four crate pages, registry artifacts and docs.rs builds. A passing
PR or a skipped already-published crate does not prove a new upload or an OIDC exchange.

**The tag must point at the commit the tarballs name.** Every published `.crate` carries a
`.cargo_vcs_info.json` recording its packaging commit. Compare each registry artifact with the
release tag's commit, not just a byte-identical `crates/` tree. For the manual first-publication
bootstrap described below, verify the uploaded artifacts before tagging. Read registry artifacts
rather than `target/package/`, which only describes a local candidate:

```sh
crate=mango-external-agents
curl -sSL "https://static.crates.io/crates/$crate/$crate-$RELEASE_VERSION.crate" |
  tar xzO "$crate-$RELEASE_VERSION/.cargo_vcs_info.json"
```

`mea` remains source-only and unpublished. GitHub releases distribute the library release notes,
with no downloadable `mea` binaries.

The [first-release correction](https://github.com/juliopolycarpo/mango-external-agents/pull/19)
and [its dated procedure](https://github.com/juliopolycarpo/mango-external-agents/blob/9cb630869c98de48c3589a142bfaf06133632a42/docs/releasing.md#010-as-executed)
record the manual 0.1.0 publication on 2026-09-19, its artifact/tag identity and the authenticated
turns and Windows checks performed afterwards. That bootstrap receipt does not qualify a later
release.

## Pre-releases

A pre-release tag (`v$RELEASE_VERSION` with a suffix such as `-rc.1`) publishes to crates.io as a pre-release version, which Cargo
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
- The squash-merge title set to the pull request title, not GitHub's default. Under
  `COMMIT_OR_PR_TITLE` a pull request holding a single commit is squashed under that commit's
  subject, so `scripts/check-pr-title.sh` would be checking text the changelog never sees.

  ```sh
  gh api -X PATCH repos/juliopolycarpo/mango-external-agents -f squash_merge_commit_title=PR_TITLE
  ```

- The branch ruleset requiring the `Title reads as a commit subject` context. It lives in its own
  workflow, so it is not covered by ci.yml's `Gate` context and does not block a merge until the
  ruleset names it.

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
