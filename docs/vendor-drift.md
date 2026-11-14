# Vendor contract drift

The [vendor contract workflow](../.github/workflows/vendor-drift.yml) separates a reproducible
public surface from captures that require a real account or a real conversation. It never logs a
vendor into CI and never supplies an API key, token, or browser session.

On pull requests, `pinned` runs on Ubuntu, macOS, and Windows. It downloads the exact native
release asset for each vendor, generates the public contracts into a scratch directory, and compares
them with the committed files:

| Vendor       | Pin       | Reproduced public artifact                                   |
| ------------ | --------- | ------------------------------------------------------------ |
| Claude Code  | `2.1.322` | `fixtures/claude/contract/`                                  |
| OpenAI Codex | `0.167.1` | `fixtures/codex/contract/` and the vendored schema inventory |
| OpenCode ACP | `1.19.5`  | `fixtures/acp/opencode/contract/`                            |

The reproduction pins select the builds whose public artifacts CI regenerates. Codex's supported
minimum is now 0.166.0 because its documented explicit gateway readiness handshake prevents
automatic browser authorization. The 2026-11-07 refresh qualifies public surfaces and readiness
with named fakes and a read-only official-binary probe, not authenticated model turns.
Historical recordings remain fixed.

Release tags and assets can change, so pinned assets are not treated as immutable. Before extracting
an archive, the installer verifies its SHA-256 against [the recorded per-platform digests](../scripts/vendor-cli-pinned-sha256sums.txt).
Those digests came from the vendor release assets' GitHub API metadata. The weekly latest lane instead
verifies each downloaded archive against that release's current API digest; it intentionally has no
in-repository checksum because finding a new release is its purpose. This is only a CI pinning
mechanism. People install the CLIs using the vendors' documented native installers: [Claude Code setup](https://code.claude.com/docs/en/setup), [Codex CLI setup](https://learn.chatgpt.com/docs/codex/cli), and [OpenCode installation](https://opencode.ai/docs/).
OpenCode documents `opencode acp` as its JSON-RPC-over-stdio ACP command, and Codex documents the
app-server surface used to generate the schema inventory.

`fixtures/**/transcripts/` and the versioned files in `fixtures/claude/help/` are **archival
captures**. They prove historical behavior, may include model output or account-dependent state, and
are deliberately excluded from the pull-request reproduction check. They remain captured by `mea`
and are never hand-edited. A historical capture is labelled as archival and is not re-captured just
because a newer CLI exists.

Every Monday, `latest-capture` installs the newest published release and recreates the same public
artifacts without issue-write permission. It uploads only a bounded drift result. A fresh
`latest-report` job checks out the scheduled commit, validates that result, and writes or updates one
open `drift(<vendor>)` issue per vendor when a diff exists. The issue body contains the diff. Codex
compares both its `fixtures/codex/contract/` public app-server capture and the generated schema
inventory; the version pin by itself is not a schema drift report. Pull-request jobs do not create,
edit, or close issues.

Each public contract records the CLI's own `--help` text next to its version, so a vendor surface
change reads as a diff. Claude records `claude --help` (`help.txt` plus the parsed `cli-surface.json`).
Codex records `codex --help` and `codex app-server --help` (`help.txt`, `app-server-help.txt`),
unparsed and verbatim except for one allowlisted row: `app`, the desktop-app launcher, is dropped from
`codex --help` because only the macOS and Windows builds list it (observed in the pinned job of
#368), and the contract must read the same on all three systems. The allowlist is
`PLATFORM_ONLY_CODEX_COMMANDS` in `examples/mea/src/capture/contracts.rs`. A new subcommand, a native ACP mode, or a changed `--listen` transport
list fails the Codex latest-contract job and opens the managed `drift(codex)` issue. A native ACP mode in either CLI is the
trigger for revisiting [the ACP convergence assessment](acp-convergence.md). The probes are
read-only, need no login and spawn only the CLI the host supplies.

The Claude scheduled lane also checks Anthropic's [headless CLI reference](https://code.claude.com/docs/en/headless)
for its documented statement that `--bare` is opt-in. A changed or missing statement opens the same
managed Claude drift issue, because a default flip would change which local configuration a scripted
turn loads.

The Claude and Codex scheduled lanes also run `scripts/check-vendor-acp-docs.sh`. It reads two public
Markdown pages per vendor, without a login, and fails when either names ACP (`acp` as a word, or
"Agent Client Protocol"):

| Vendor | Pages                                                                                                                                               |
| ------ | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| Claude | [documentation index](https://code.claude.com/docs/llms.txt), [CLI reference](https://code.claude.com/docs/en/cli-reference.md)                     |
| Codex  | [documentation index](https://learn.chatgpt.com/llms.txt), [command line options](https://learn.chatgpt.com/docs/developer-commands.md?surface=cli) |

Each page must still contain a phrase it carries today (`Claude Code Docs`, `--print`, `Codex CLI`,
`codex exec`). A page that moved, emptied or stopped answering exits 2, "could not verify", and does
not pass as clean. Either outcome opens the managed `drift(<vendor>)` issue. The contract
comparison still runs, so when a release ships the mode the issue carries the help diff too. The
issue body for a mention is the generic failure text; the workflow log names the page and the
matching line. The step runs on the schedule only, so a vendor page cannot block a pull request.
Its tests use the named fake fetcher `scripts/test-fixtures/fake-doc-curl.sh` and reach no network.

An accepted mention (say, an index line about a third-party adapter) keeps the watch failing every
Monday. Narrow `ACP_PATTERN` in the script in the same change that records the decision in
[the ACP convergence assessment](acp-convergence.md).

The watch sees a documented ACP mode before a release ships it, which the help diff cannot. The help
diff still catches a flag that ships undocumented.

Subcommand help texts stay out of the public contracts. Each one would be a committed contract that
the pinned job must reproduce byte for byte on Ubuntu, macOS and Windows (the `app` row of
`codex --help` shows how platforms differ), and every wording change in a command the library never
calls would open a drift issue. The top-level help and the documentation watch are the trigger. An
ACP flag in a subcommand's help before any watched page names it stays unwatched:
[#451](https://github.com/juliopolycarpo/mango-external-agents/issues/451).

To inspect a Codex schema without touching the checkout, use:

```bash
scripts/vendor-codex.sh 0.167.1 --out /tmp/codex-contract
scripts/check-vendor-contract.sh codex \
  crates/mango-agent-codex/vendor/schema.json /tmp/codex-contract/schema.json
```
