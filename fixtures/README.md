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
paths). `--model ID` runs the turn on that model and adds a `# model: ID` line after the first
header line, as the Codex recorder does, with the id redacted by the model-id rule below; without it
there is no such line and the account's own default applies. The labels and cleanup rules above apply: `# incomplete capture` in
`structured-output.partial.jsonl` when the run fails or the child's stop is unconfirmed, `#
unqualified capture` in `structured-output.unqualified.jsonl` when the file lacks an assistant
`StructuredOutput` tool call or a final `result` with an object `structured_output`. The file is an
archival transcript, like `codex/subagent.jsonl` under the historical sets below: no manifest, no
`reproducible` flag, and nothing in CI can re-record it, since it belongs to one account and one CLI
build. The next subsection lists what the file keeps and drops.

### What a Claude transcript retains and redacts

The recorder redacts each line before it is held in memory. A string survives only under a short
list of structural members (`type`, `subtype`, `role`, `id`, `tool_use_id`, `parent_tool_use_id`,
`session_id`, `uuid`, `stop_reason`, `terminal_reason`, `permissionMode`, `claude_code_version`;
`model` is kept under its own rule below) and as the name of a non-`mcp__` tool call. Every other string becomes
`[REDACTED]`, so a field a later Claude Code adds is private until someone reviews it. `cwd` is
written as `/workspace`, and the capture's directory, home directory and login are rewritten in
every key and retained value. Lines that are not JSON objects are dropped.

Redacted whatever the value type:

- Members with a credential, account or plan shaped name: `*token` (not `*_tokens` counts),
  `credential*`, `account*`, `email`, `organization*`, `oauth`, `secret`, `password`, `apikey`,
  `authorization`, `cookie`, `userid`, and the plan-usage names `ratelimit*`, `resetsat`,
  `overage*`, `utilization`, `quota`.
- A `rate_limit_event`: it reports plan usage (utilisation, reset times, overage state, the names
  of the account's windows). Only `type`, `uuid` and `session_id` are kept; every other member is
  replaced whole, so an unknown member is denied by default. Nothing replays the record.
- A `system/init` record: it carries the user's configuration (`analytics_disabled`,
  `product_feedback_disabled`, `output_style`, `agents`, `tools`, `mcp_servers`, `memory_paths`,
  `messaging_socket_path`, the fast-mode state). Only `type`, `subtype`, `uuid`, `session_id`,
  `cwd`, `claude_code_version`, `model`, `permissionMode`, `capabilities`, `slash_commands`,
  `terminal_slash_commands`, `skills` and `plugins` are kept (the ones the reducer reads, plus the
  ids); every other member is replaced whole, so an unknown member is denied by default. Strings
  inside the kept lists still become `[REDACTED]`, so each list keeps its length and not its names.
- `subagent_stats.by_type`: its keys are subagent type names, which a user can define themselves.
- Strings inside `input`, `structured_output` and `wire_tool_inputs` (the same tool input again,
  keyed by tool-use id), even under a name such as `type` or `id`.
- A model id that is not a plain vendor name, in `model`, in the keys of `modelUsage` and
  `model_usage_since_last_result`, and on the `# model:` header line. On Bedrock or Vertex an id can
  be an ARN with an AWS account id, a project path, or an inference-profile name the user chose.
  The rule is an allow-list: an alias (`opus`, `sonnet[1m]`), a `claude-…` id (`claude-sonnet-5`,
  `claude-opus-4-1@20250805`) or a Bedrock id with its optional region prefix
  (`us.anthropic.claude-sonnet-4-5-20250929-v1:0`) is kept, and anything else becomes
  `[REDACTED]`. A family word or numeric segment the rule has not seen redacts the id until the
  rule is reviewed. Two redacted usage keys stay two entries (`[REDACTED]`, `[REDACTED]#2`).

Retained on purpose:

- The key names and numbers of the model's answer. They come from the operator's `--json-schema`
  and the scripted prompt, so the answer's shape stays for qualification. Redaction cannot tell a
  placeholder from a real figure, so an answer is kept as the model wrote it: a schema or prompt
  that elicits real data (a balance, an id, a usage count) commits that data. The built-in
  `structured-output` prompt asks for the word `mango` and the number `1` only; a schema of your
  own must elicit nothing more. `mea capture` prints a one-line notice to stderr when it records
  a schema turn; read the file before committing it.
- `toolu_…` ids, as values and as the keys of `wire_tool_inputs`. They are opaque and random per
  call, the same ids are already kept under `id` and `tool_use_id`, and a call has to pair with its
  result.
- Per-turn usage, cost and timing: token counts, `total_cost_usd`, `duration_ms`, `modelUsage`
  (keyed by a plain model id) and `subagent_stats` counters. They describe this turn, not the
  plan.
- `permissionMode`. The recorder passes `--permission-mode plan` itself, so the value is the
  capture's request and not the operator's default.
- `session_id`, `uuid` and `claude_code_version`, as before, and a plain vendor `model` id.

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
copy whole manifest-owned contract directories; Codex copies every archival JSONL transcript, and
Claude copies every `transcripts/*.jsonl` the same way, so a new recording is packaged without
editing the script. The Claude help surfaces stay named one by one. A recorder's
`*.partial.jsonl` (incomplete run) and `*.unqualified.jsonl` (completed run that failed
qualification) diagnostics are never packaged and never reported by `--check`, in any selection.
After capturing or changing a canonical fixture, regenerate the copies and verify them:

```sh
scripts/package-fixtures.py
scripts/package-fixtures.py --check
```

The check rejects missing, stale and extra copied files. `scripts/fix.sh` regenerates the copies,
and `scripts/check.sh` verifies them. Edit or capture only the canonical files under `fixtures/`.
