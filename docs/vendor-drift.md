# Vendor contract drift

The [vendor contract workflow](../.github/workflows/vendor-drift.yml) separates a reproducible
public surface from captures that require a real account or a real conversation. It never logs a
vendor into CI and never supplies an API key, token, or browser session.

On pull requests, `pinned` runs on Ubuntu, macOS, and Windows. It downloads the exact native
release asset for each vendor, generates the public contracts into a scratch directory, and compares
them with the committed files:

| Vendor       | Pin       | Reproduced public artifact                                   |
| ------------ | --------- | ------------------------------------------------------------ |
| Claude Code  | `2.1.325` | `fixtures/claude/contract/`                                  |
| OpenAI Codex | `0.169.0` | `fixtures/codex/contract/` and the vendored schema inventory |
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
"Agent Client Protocol") on a line the reviewed allowlist (below) does not accept:

| Vendor | Pages                                                                                                                                               |
| ------ | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| Claude | [documentation index](https://code.claude.com/docs/llms.txt), [CLI reference](https://code.claude.com/docs/en/cli-reference.md)                     |
| Codex  | [documentation index](https://learn.chatgpt.com/llms.txt), [command line options](https://learn.chatgpt.com/docs/developer-commands.md?surface=cli) |

Each page must still contain a phrase it carries today (`Claude Code Docs`, `--print`, `Codex CLI`,
`codex exec`). A page that moved, emptied or stopped answering exits 2, "could not verify", and does
not pass as clean. Either outcome opens the managed `drift(<vendor>)` issue. The contract
comparison still runs, so when a release ships the mode the issue carries the help diff too. The
issue body carries a "Documentation watch" section: each page URL that needs attention and the
matching line text with its line number (at most three lines per page, each cut at 1000 bytes,
which holds a whole `llms.txt` index line, with the count shown), a note for a page that could
not be downloaded or lost its phrase, and any stale allowlist entry. The contract diff is cut at
48000 bytes so the whole report stays under the 65536 bytes the "Bound the drift result" step allows. The section holds public page
text and public URLs only; the workflow passes a report path as the script's second argument, and
the file exists only when the watch fails. The step runs on the schedule only, so a vendor page
cannot block a pull request. Its tests use the named fake fetcher
`scripts/test-fixtures/fake-doc-curl.sh` and reach no network. The workflow step that joins the
section to the issue body is exercised locally, not in CI.

An accepted mention (say, an index line about a third-party adapter) goes in the reviewed
allowlist, [`scripts/vendor-acp-docs-allowlist.txt`](../scripts/vendor-acp-docs-allowlist.txt), not
in `ACP_PATTERN`. One entry per line, three fields separated by a TAB: the vendor (`claude` or
`codex`), the exact text of the page line, and the reason the project keeps it. A Markdown table row
carries `|` itself, which is why the separator is a TAB. The watch subtracts an entry only when it
equals a whole line of one of that vendor's watched pages, so a reworded line, a longer line that
contains it, or the same text under the other vendor fails again and gets a new review. Add the
entry in the same change that records the decision in [the ACP convergence assessment](acp-convergence.md).
The record is the entry's exact line text as a whole line of that page's "Accepted documentation
mentions" section, inside a `text` code fence (a fence keeps dprint from reflowing a table row). The
check is offline: `scripts/test-vendor-drift.sh` runs `unrecorded_allowlist_entries` over the
committed allowlist and assessment, in `scripts/check.sh` and CI, and fails naming each entry that
has no such line. A line elsewhere on the page, or inside the section but reworded, does not count.

An entry that no watched page carries any more is stale. The watch fails on it, exit 1, and names it
in the issue body and the workflow log, so the weekly issue asks for the entry to be deleted; a dead entry would
otherwise excuse the line if it came back. The stale check is skipped when a page could not be read
(exit 2), because that page may still carry the line. A malformed entry, an unknown vendor or an
empty reason exits 2 before any page is read.

The watch sees a documented ACP mode before a release ships it, which the help diff cannot. The help
diff still catches a flag that ships undocumented.

Subcommand help texts stay out of the public contracts. Each one would be a committed contract that
the pinned job must reproduce byte for byte on Ubuntu, macOS and Windows (the `app` row of
`codex --help` shows how platforms differ), and every wording change in a command the library never
calls would open a drift issue. The top-level help and the documentation watch are the trigger, and
a third watch covers the gap between them.

## Subcommand help watch

An ACP flag a release adds under `claude mcp`, `codex exec` or another subcommand shows in neither
the top-level help nor a vendor page until a watched page names it. The Claude and Codex scheduled
lanes therefore run `scripts/check-vendor-subcommand-help.sh` after the capture. It reads the command
names from the `Commands:` section of the top-level help the capture just wrote (without `help` and
without the alias after a `|`), runs `<cli> <command> --help` once for each, searches the text with
the same `ACP_PATTERN` as the documentation watch, and discards it. Nothing is committed and no
subcommand help is compared, so a rewording opens no issue.

The probe needs no login and starts only the CLI the lane installed, with `--help` as the only
argument, `stdin` closed and `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and the XDG directories set to a
scratch directory it removes. It does not read or write anyone's own configuration.

| Exit | Meaning                                                                                                                                                                                                |
| ---- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| 0    | No subcommand help names ACP.                                                                                                                                                                          |
| 1    | At least one does. The log and the issue body name each command and the matching lines with their numbers.                                                                                             |
| 2    | The watch could not look: no command in the top-level help, a command name that is not a plain word, or a subcommand whose `--help` exited non-zero or did not answer in time. It is not a clean pass. |

Each `--help` runs under `timeout` with a 30 second limit (`HELP_TIMEOUT_SECONDS`, TERM and then
KILL five seconds later), so a CLI that waits on a prompt or the network is reported by name as
"did not answer" and cannot hold the lane until the step's own limit.

A flagged or unreadable run opens the same managed `drift(<vendor>)` issue as the other watches and
adds a "Subcommand help watch" section to its body: the command line, the matching lines (at most
three per command, each cut at 240 bytes, with the count shown) and at most six commands, with the
rest named. The section holds public help text only. The step is `continue-on-error` and bounded at
five minutes, runs only when the capture succeeded, and runs on the schedule only. Its tests use
the named fake `scripts/test-fixtures/fake-vendor-cli.sh` and run no vendor.

The check ran against the pinned `claude` 2.1.325 (18 commands) and `codex` 0.169.0 (26 commands)
on 2026-11-17 with empty configuration directories and found no ACP mention in any of them. Treat a
hit as the same trigger as a hit in the top-level help ([acp-convergence.md](acp-convergence.md)).

## Adapter captures: not a scheduled lane

[#454](https://github.com/juliopolycarpo/mango-external-agents/issues/454) asked whether the
`claude-agent-acp` and `codex-acp` `initialize` captures can be pinned and recaptured in CI. They
can. They stay labelled maintainer captures anyway: no lane records them again, the installer and
`PINNED_VENDORS` are unchanged, and the refresh recipe in [fixtures/README.md](../fixtures/README.md)
is the only path.

Evidence, read on 2026-11-14 on one Linux x86-64 machine with Node 26.13.0 and npm 12.2.2. Nothing
ran on a CI runner. Each adapter was installed with `npm install --prefix DIR --ignore-scripts` into
a fresh empty directory at an exact version and captured with `mea capture --harness acp` from another
directory, with empty `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `HOME`:

| Question                                  | `claude-agent-acp` 0.107.0                                                                                        | `codex-acp` 2.6.0                                                                                                                                    |
| ----------------------------------------- | ----------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------- |
| Install                                   | 108 packages, none with an install script                                                                         | 25 packages, none with an install script                                                                                                             |
| What the adapter's manifest pins          | every dependency exact: SDK 0.3.323, so `claude` 2.1.323, and the platform packages too                           | ranges: `@openai/codex ^0.166.0`, `@agentclientprotocol/sdk ~2.4.0`, `zod ^4.0.0`                                                                    |
| Resolved today                            | the same tree on both installs                                                                                    | `@openai/codex` 0.166.0, the only 0.166.x on npm; `@agentclientprotocol/sdk` 2.4.0; `zod` 4.6.7                                                      |
| Release assets                            | 8 archives of 195 to 228 MB, each with a GitHub API digest                                                        | none, npm only                                                                                                                                       |
| Two installs, then a capture each         | both lockfiles equal, apart from the directory name; both captures equal each other and the committed `contract/` | same                                                                                                                                                 |
| Credential, `authenticate`, `session/new` | none sent; nothing written under the empty home                                                                   | none sent; no credential file written (the state files in the next row only)                                                                         |
| What a capture started (strace, one run)  | the SDK's bundled `claude` binary, argv not recorded; no network connection                                       | `codex app-server`, which wrote `installation_id` and SQLite files under `CODEX_HOME` and ran `git-remote-https` for `github.com/openai/plugins.git` |

The caret on a `0.x` line stops below 0.167.0, so only a later 0.166.x could move the Codex binary
beneath `codex-acp`. Two installs minutes apart do not test that float, and a recapture on the day of
the committed one does not test drift over time.

Whether a recapture adds signal beyond the Claude and Codex CLI lanes depends on which recapture:

- **Pinned** (a committed lockfile and `npm ci --ignore-scripts`): deterministic, and it replays the
  committed bytes. It changes only when a maintainer edits the lockfile, which the manifest digest
  test already covers. It adds no signal.
- **Latest** (the newest adapter each Monday): it sees what the CLI lanes cannot. `initialize` moves
  with the adapter release and not with the CLI. Against the committed 0.107.0 and 2.6.0 captures:

  | Adapter version             | Members the committed capture has and this version lacks                          |
  | --------------------------- | --------------------------------------------------------------------------------- |
  | `claude-agent-acp` 0.103.0  | `agentCapabilities.auth.status`; `sessionCapabilities.options`, `status`, `title` |
  | `claude-agent-acp` 0.105.0  | `sessionCapabilities.options`                                                     |
  | `codex-acp` 2.5.0 and 2.3.0 | `sessionCapabilities.title`                                                       |

  Nothing reads those members. `capabilities_from` in `crates/mango-agent-acp/src/harness.rs` takes
  `loadSession`, `promptCapabilities.image` and `sessionCapabilities.list`, and all three are equal
  across every version tried. A drift issue would ask for no decision. The cadence makes it frequent:
  `claude-agent-acp` published six releases from 2026-11-10 to 2026-11-13 and `codex-acp` six from
  2026-10-10 to 2026-11-08.

The cost side: the lane would need a Node toolchain beside the Rust and `gh` ones, run third-party
adapter code and a vendor binary in CI, give the Codex adapter a path to clone from GitHub, and, for
Claude's archives, download 195 to 228 MB per run. The adapters are also not a vendor-documented
surface, which is criterion 1 of [the convergence assessment](acp-convergence.md#decision-criteria);
criteria 1 and 3 fail before a reproducible contract (criterion 2) could change that verdict. The
issue asked for a lane only if the verdict moved toward a profile, and it did not.

A lane becomes worth building when that verdict moves. Then, per adapter: a committed
`package.json` and lockfile with `npm ci --ignore-scripts` into an empty directory for `codex-acp`,
where the manifest leaves ranges; the release archive for `claude-agent-acp`, which fits
`scripts/install-vendor-cli.sh`; and `PINNED_VENDORS` and the installer extended together, since a
`mea` test compares them.

To inspect a Codex schema without touching the checkout, use:

```bash
scripts/vendor-codex.sh 0.169.0 --out /tmp/codex-contract
scripts/check-vendor-contract.sh codex \
  crates/mango-agent-codex/vendor/schema.json /tmp/codex-contract/schema.json
```
