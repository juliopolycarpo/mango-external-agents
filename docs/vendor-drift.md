# Vendor contract drift

The [vendor contract workflow](../.github/workflows/vendor-drift.yml) separates a reproducible
public surface from captures that require a real account or a real conversation. It never logs a
vendor into CI and never supplies an API key, token, or browser session.

On pull requests, `pinned` runs on Ubuntu, macOS, and Windows. It downloads the exact native
release asset for each vendor, generates the public contracts into a scratch directory, and compares
them with the committed files:

| Vendor       | Pin       | Reproduced public artifact                                   |
| ------------ | --------- | ------------------------------------------------------------ |
| Claude Code  | `2.1.270` | `fixtures/claude/contract/`                                  |
| OpenAI Codex | `0.154.0` | `fixtures/codex/contract/` and the vendored schema inventory |
| OpenCode ACP | `1.18.30` | `fixtures/acp/opencode/contract/`                            |

The installer uses the vendor's immutable GitHub release rather than a mutable `latest` installer.
That is only a CI pinning mechanism. People install the CLIs using the vendors' documented native
installers: [Claude Code setup](https://code.claude.com/docs/en/setup), [Codex CLI setup](https://learn.chatgpt.com/docs/codex/cli), and [OpenCode installation](https://opencode.ai/docs/).
OpenCode documents `opencode acp` as its JSON-RPC-over-stdio ACP command, and Codex documents the
app-server surface used to generate the schema inventory.

`fixtures/**/transcripts/` and the versioned files in `fixtures/claude/help/` are **archival
captures**. They prove historical behavior, may include model output or account-dependent state, and
are deliberately excluded from the pull-request reproduction check. They remain captured by `mea`
and are never hand-edited. A historical capture is labelled as archival and is not re-captured just
because a newer CLI exists.

Every Monday, `latest` installs the newest published release, recreates the same public artifacts,
and writes or updates one open `drift(<vendor>)` issue per vendor when a diff exists. The issue body
contains the diff. Codex compares both its `fixtures/codex/contract/` public app-server capture and
the generated schema inventory; the version pin by itself is not a schema drift report. The scheduled
job has issue write permission; pull-request jobs do not create, edit, or close issues.

The Claude scheduled lane also checks Anthropic's [headless CLI reference](https://code.claude.com/docs/en/headless)
for its documented statement that `--bare` is opt-in. A changed or missing statement opens the same
managed Claude drift issue, because a default flip would change which local configuration a scripted
turn loads.

To inspect a Codex schema without touching the checkout, use:

```bash
scripts/vendor-codex.sh 0.154.0 --out /tmp/codex-contract
scripts/check-vendor-contract.sh codex \
  crates/mango-agent-codex/vendor/schema.json /tmp/codex-contract/schema.json
```
