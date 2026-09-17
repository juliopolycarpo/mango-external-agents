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

The following are historical captures. They are labelled because a present-day command cannot
reproduce their bytes, and routine drift checks must leave them alone:

- `claude/historical/contract/` contains the old `auth status` shape.
- `claude/help/` covers versions before and after features the parser supports.
- `claude/transcripts/` and the Codex JSONL files record real vendor conversations.

A historical fixture remains captured and never hand-edited. Keep it when it continues to prove a
compatibility case. Replace it only with a new capture that proves the same case on a consciously
chosen version.
