# Fixture capture rules

Fixture artifacts come from `mea capture`. Do not edit their contents by hand.

Public contract directories are regenerated at the exact CLI versions pinned by the drift workflow:

```sh
cargo run -p mea -- capture --harness claude --out fixtures/claude
cargo run -p mea -- capture --harness codex --out fixtures/codex
cargo run -p mea -- capture --harness acp --profile opencode --out fixtures/acp/opencode
```

These captures make no login request. Claude reads `--version` and `--help`. Codex reads its
version and runs `app-server generate-json-schema`. ACP reads a version and sends `initialize`
without `authenticate` or `session/new`. CI compares only public artifacts: the resulting
`contract/` directories and, for Codex, the separately regenerated schema inventory. It excludes
archival transcripts and historical help.

## Three classes of fixture

**Reproducible public contracts.** CI can install the CLI that produced them at the pin and record
them again, so a difference is a vendor change and the drift lane says so. Their manifests carry
`"reproducible": true`. The class is exactly the vendors `scripts/install-vendor-cli.sh` can
install — `claude`, `codex` and `opencode` — and nothing else: `mea` derives the member from that
list and a test compares the two, so a capture cannot claim a pin the installer does not have.

**Labelled maintainer captures.** Recorded on a maintainer's machine against an agent CI cannot
install at a version. Nothing re-records them, so their manifests carry `"reproducible": false`
together with the two facts a reader is owed instead: `capturedFrom`, the version line the CLI
printed, and `capturedAt`, the day. Routine drift checks leave them alone.

**Historical sets, deliberately not regenerated.** A present-day command cannot reproduce their
bytes, and that is the point of keeping them:

- `claude/historical/contract/` contains the old `auth status` shape.
- `claude/help/` covers versions before and after features the parser supports.
- `claude/transcripts/` and the Codex JSONL files record real vendor conversations.

A historical fixture remains captured and never hand-edited. Keep it when it continues to prove a
compatibility case. Replace it only with a new capture that proves the same case on a consciously
chosen version.

## Digests

Every `manifest.json` carries a `files` member holding one SHA-256 digest per file beside it, and
`mea`'s own test suite recomputes all of them — so inside a capture directory that carries a
manifest, "never hand-edited" is a failing test rather than a sentence. The test names the file,
the digest its manifest declares and the digest the file has, and it refuses a file in that
directory which the manifest does not declare. It runs under `scripts/check.sh` with the rest of
the suite; nothing has to be enabled for it.

What that covers is the four `contract/` directories. The archival transcripts
(`codex/*.jsonl`, `claude/transcripts/*.jsonl`) and the recorded help surfaces (`claude/help/`)
carry no manifest and therefore no digest: they are read by the replaying fakes, and an edit to one
shows up as a test that disagrees with the transcript rather than as an integrity failure. If such
a set ever needs the stronger claim, give it a `manifest.json` describing the capture and run
`mea digests`: the command fills in the digests of every manifest it finds, and the verification
test then covers that directory too.

`mea capture` writes the digests with the capture. A capture whose files changed for a reason
other than a vendor change — none is expected — is regenerated from the files as they stand:

```sh
cargo run -p mea -- digests            # rewrite every manifest's digests under fixtures/
cargo run -p mea -- digests --check    # the same verification, by hand
```

That command reads the committed bytes and writes what they hash to. It never runs a vendor CLI
and never touches a captured file, which is how the three contract manifests came to carry digests
without a re-capture; the historical Claude manifest already carried its own, and `mea digests`
leaves a manifest it agrees with untouched.

A manifest that says `"reproducible": false` is held to `capturedFrom` and `capturedAt` by the same
test: a capture nobody can re-record is worth keeping only if it says which build it came from.

The historical Claude manifest additionally carries an aggregate `checksum` from the session that
recorded it in 2026-09. Nothing in this repository can recompute it: it is not the digest of the
committed files in any order, of their digests, or of the manifest itself. It is left as written
and held only to its shape; the per-file digests are what the integrity check reads.
