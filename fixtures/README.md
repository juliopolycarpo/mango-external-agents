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

Public command probes, ACP initialization and Codex transcript recording require confirmed
process-tree cleanup before their output qualifies. They own bounded cleanup from transport
acquisition, including when a future is dropped while the runtime remains alive. A stop failure
retains the injected recovery control and fails the capture even if the command or conversation
finished. When requested, a transcript diagnostic retains its caller prefix and gains
`# incomplete capture`; it is not a replay fixture. A completed recording that fails scenario checks
instead gains `# unqualified capture`. Qualified transcripts require those checks to pass.

## Claude transcript recorder

`mea capture --harness claude --transcripts --scenario structured-output --json-schema FILE` records
the stream-json lines a real, already signed-in `claude` emits for one schema turn into
`<out>/transcripts/structured-output.jsonl`. It never signs in or reads a token. The turn runs
through `ClaudeHarness`; a launcher wrapper copies the stdout of the turn launch only. Like the Codex
transcripts the file is archival and has no manifest, with `# claude/...` and `# capture {json}`
header lines that give the `claude --version` line and the capture time, and it is redacted at write
time (credential-shaped members such as `credential_expires_at`, account identity, the capture's
paths). The labels and cleanup rules above apply: `# incomplete capture` in
`structured-output.partial.jsonl` when the run fails or the child's stop is unconfirmed, `#
unqualified capture` in `structured-output.unqualified.jsonl` when the file lacks an assistant
`StructuredOutput` tool call or a final `result` with an object `structured_output`. The class is
the labelled maintainer capture: nothing in CI can re-record it, and it belongs to one account and one
CLI build. See [Transcript capture](../docs/harness-claude.md#transcript-capture) for the redaction
rules.

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

`fixtures/acp/cursor/` and `fixtures/acp/grok/` are this class. They exist because those two
profiles are the ones `mango-agent-acp` marks verified, and a claim a host is told to trust should
rest on a file rather than on a source comment. Refreshing one means having the agent installed:

```sh
cargo run -p mea -- capture --harness acp --profile cursor --out fixtures/acp/cursor
cargo run -p mea -- capture --harness acp --profile grok --out fixtures/acp/grok
```

Do that only when the agent is a version worth recording, and update the profile's
`VerificationEvidence` in the same change — the record names a build and a day, and a capture that
moved without it is a claim about the wrong one.

**Historical sets, deliberately not regenerated.** A present-day command cannot reproduce their
bytes, and that is the point of keeping them:

- `claude/historical/contract/` contains the old `auth status` shape.
- `claude/help/` covers versions before and after features the parser supports.
- `claude/transcripts/` and the Codex JSONL files record real vendor conversations. A Claude
  transcript recorded by `mea capture` carries the `#` header described above; the two older files
  there do not.
- `codex/subagent.jsonl` is a maintainer capture from an authenticated Codex 0.167.2 run on
  2026-11-14 with model `gpt-5.6-terra`, recorded by `mea capture --harness codex --transcripts
  --scenario subagent --model gpt-5.6-terra`. `mea` qualified it: it holds `subAgentActivity` items
  with a nonempty `agentThreadId`. Like the other transcripts it has no manifest; its `# capture`
  header and `# model:` line state the build and model, and `tests/replay/subagent.rs` in
  `mango-agent-codex` fails if the file loses them. It is one run on one `free` account.

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

What that covers is the six `contract/` directories. The archival transcripts
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
and never touches a captured file, which is how the three that predate the rule came to carry digests
without a re-capture; the historical Claude manifest already carried its own, and `mea digests`
leaves a manifest it agrees with untouched.

A manifest that says `"reproducible": false` is held to `capturedFrom` and `capturedAt` by the same
test: a capture nobody can re-record is worth keeping only if it says which build it came from.

The historical Claude manifest additionally carries an aggregate `checksum` from the session that
recorded it in 2026-09. Nothing in this repository can recompute it: it is not the digest of the
committed files in any order, of their digests, or of the manifest itself. It is left as written
and held only to its shape; the per-file digests are what the integrity check reads.

## Packaged test fixtures

The three harness crates carry derived copies of the captures their tests read. Claude and ACP
copy whole manifest-owned contract directories; Codex copies every archival JSONL transcript.
After capturing or changing a canonical fixture, regenerate the copies and verify them:

```sh
scripts/package-fixtures.py
scripts/package-fixtures.py --check
```

The check rejects missing, stale and extra copied files. `scripts/fix.sh` regenerates the copies,
and `scripts/check.sh` verifies them. Edit or capture only the canonical files under `fixtures/`.
