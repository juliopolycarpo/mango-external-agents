# ACP convergence assessment

Question: should `mango-agent-claude` and `mango-agent-codex` shrink to profiles of
[`mango-agent-acp`](harness-acp.md)? This page records where each CLI stands against the Agent
Client Protocol, what the dedicated harnesses give that a profile does not, the criteria for
converging, and the signal that reopens the question.

**Verdict at this revision: do not converge.** Neither vendor documents an ACP mode in its own CLI,
and the ACP surface this library drives carries less than either dedicated harness. Both agents stay
reachable as opt-in profiles (`claude-agent-acp`, `codex-acp`). Each has a committed `initialize`
capture, and neither has had a session opened against it.

Evidence read on 2026-11-14. The pinned CLIs are Claude Code `2.1.322` and Codex `0.167.1`
([vendor-drift.md](vendor-drift.md)). Reading a vendor page later does not requalify a pin.

## Does the CLI expose an ACP mode?

**Claude Code: no.**

- `fixtures/claude/contract/help.txt` lists no `acp` command (the commands are `agents`, `auth`,
  `mcp`, `plugin`, `gateway` and similar), and `cli-surface.json` lists no ACP flag.
- The [CLI reference](https://code.claude.com/docs/en/cli-reference), the
  [headless page](https://code.claude.com/docs/en/headless) and the
  [docs index](https://code.claude.com/docs/llms.txt) name no ACP command, flag or page.
  Programmatic use is documented as `claude -p` and the Agent SDK.
- Requests for one,
  [anthropics/claude-code#6686](https://github.com/anthropics/claude-code/issues/6686) and
  [#48506](https://github.com/anthropics/claude-code/issues/48506), are closed (not planned, then
  inactive) and locked. A closed request is not a statement about the roadmap.

**Codex: no.**

- `fixtures/codex/contract/help.txt` and `app-server-help.txt` list no `acp` command.
  `app-server --listen` accepts `stdio://`, `unix://`, `unix://PATH`, `ws://IP:PORT` and `off`.
- The [CLI reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli) lists no ACP
  command. The [app-server page](https://learn.chatgpt.com/docs/app-server) presents app-server as
  the interface for rich clients and, for automation, points to the Codex SDK. Neither page
  mentions ACP.
- [openai/codex#30052](https://github.com/openai/codex/issues/30052) asks for built-in ACP support.
  It is open with no maintainer reply.

The ACP project's own [agent list](https://agentclientprotocol.com/get-started/agents) agrees:
"Claude Agent" is "via Zed's SDK adapter" and "Codex CLI" is "via ACP's adapter". The Zed link now
redirects to
[`agentclientprotocol/claude-agent-acp`](https://github.com/agentclientprotocol/claude-agent-acp).
Both are adapters, not CLI modes:

- `claude-agent-acp`: "This tool implements an ACP agent by using the official Claude Agent SDK".
  Each standalone archive contains two files, "`claude-agent-acp`, and the Claude CLI (`claude`) it
  runs"; `CLAUDE_CODE_EXECUTABLE` selects an installed CLI instead. It also serves the draft ACP v2 to
  clients that ask for it.
- [`codex-acp`](https://github.com/agentclientprotocol/codex-acp): "It starts the Codex App Server,
  translates ACP requests into Codex operations, and maps Codex events back into the client." Its
  npm package "includes a compatible `@openai/codex` dependency"; `CODEX_PATH` selects another.
  Its subagent, steering and async-question features need client negotiation or extensions, and
  "other clients keep the earlier fields".

A Codex ACP profile is therefore the app-server dialect the dedicated harness already speaks, behind
a second process and narrowed to ACP's vocabulary. Both adapters may also run a vendor binary the
host did not name; see
[Which vendor binary an adapter runs](harness-acp.md#which-vendor-binary-an-adapter-runs).

## What the dedicated harnesses give that a profile does not

"Profile" means the generic harness as it stands: `ceiling()` in
`crates/mango-agent-acp/src/harness.rs`, narrowed when a session opens by what the agent's
`initialize` answer advertises. Both adapters' handshakes are committed under
`fixtures/acp/claude-agent-acp/` and `fixtures/acp/codex-acp/`
([#434](https://github.com/juliopolycarpo/mango-external-agents/issues/434)); a cell they cannot
answer reads "unmeasured" instead of a guess. The two adapters advertise the same values for the
cells below, so the column holds for either.

| Capability                 | Claude dedicated                                                        | Codex dedicated                                    | ACP profile (either adapter)                                                                                                           |
| -------------------------- | ----------------------------------------------------------------------- | -------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------- |
| Structured output          | yes: `--json-schema`, `structured_output`                               | not driven                                         | no: no driven ACP method carries a schema                                                                                              |
| Interactive approvals      | no: modes and allow-lists only                                          | yes, brokered                                      | yes, when the agent sends `session/request_permission`                                                                                 |
| Typed questions            | no                                                                      | yes (experimental upstream)                        | no: `elicitation/create` is declined                                                                                                   |
| Permission levels          | `ReadOnly`, `Default`, `FullAccess`, and `auto` for qualifying accounts | full matrix over `approvalPolicy` and sandbox      | `Default`, `ReadOnly`; `FullAccess` only if the profile knows a mode id (none do)                                                      |
| Resume                     | `--resume`; nonresumable after a forced kill                            | `thread/resume`                                    | `session/load`; both adapters advertise `loadSession`                                                                                  |
| Cancellation               | SIGINT or kill, with a recorded resumability verdict                    | `turn/interrupt`                                   | `session/cancel`, then the kill grace                                                                                                  |
| Steering                   | no                                                                      | yes                                                | no: ACP v1 has no surface; adapter extensions are not consumed                                                                         |
| Native review              | no                                                                      | yes                                                | no: `native_review` stays false                                                                                                        |
| Account quota, rate limits | no                                                                      | yes: `account/rateLimits/*` into `AccountLimits`   | no: `usage_update` reports the context window, not plan quota                                                                          |
| Usage reporting            | yes: the `result` record's counts                                       | yes                                                | context-window use only                                                                                                                |
| Model catalog flag         | yes, per build: aliases from `--model` help                             | yes: `model/list`                                  | `model_catalog` stays false; a `model` config option: `claude-agent-acp` offers one, `codex-acp` refuses `session/new` without a login |
| Session configuration      | no: argv on a fresh child                                               | per turn                                           | yes: `session/set_config_option`                                                                                                       |
| Session listing            | no                                                                      | yes                                                | `session/list`; both adapters advertise `sessionCapabilities.list`                                                                     |
| Images                     | no                                                                      | yes                                                | yes: both adapters advertise `promptCapabilities.image`                                                                                |
| MCP passthrough            | `--mcp-config`, per build                                               | per-thread `config.mcp_servers`                    | `mcpServers`; HTTP only when advertised                                                                                                |
| Login state                | `auth status` probe                                                     | `account/read`                                     | always `Unknown`                                                                                                                       |
| Public contract under CI   | `fixtures/claude/contract/`                                             | `fixtures/codex/contract/` and the vendored schema | labelled maintainer captures, not pinned (see below)                                                                                   |

Reading the table:

- Converging Codex would lose rate limits, steering, native review, typed questions and the account
  probe. Nothing on the ACP surface this library drives replaces them.
- Converging Claude would lose structured output and the interrupt-verdict logic.
- Claude would gain one thing: a brokered approval path. `claude-agent-acp` reaches it through the
  Agent SDK's `canUseTool` callback (its `src/v1/acp-agent.ts` uses it), which the Agent SDK
  documents. The dedicated harness declines the route because the SDK drives it over a
  `--permission-prompt-tool stdio` sentinel that no CLI help or docs page lists (see "Approvals:
  none, and why" in [harness-claude.md](harness-claude.md)). A decision to converge Claude has to
  settle whether that route now counts as documented.
- No rule of the library weakens. Cancelling an ACP turn withdraws every pending question, and no
  profile reaches `FullAccess` by answering on an agent's behalf.

## What the adapter handshakes measured

Captured on 2026-11-14 with `mea capture --harness acp --profile <id>`. Each sent `initialize` and
nothing else: no `authenticate`, no `session/new`. The two adapters were captured again on
2026-11-18 with `--open-session`, which also sends `session/new` once `initialize` answered
([below](#what-sessionnew-measured)); the `initialize` files came out byte for byte the same.

| `initialize` member                     | `claude-agent-acp` 0.107.0                                                                    | `codex-acp` 2.6.0                     |
| --------------------------------------- | --------------------------------------------------------------------------------------------- | ------------------------------------- |
| `protocolVersion`                       | `1`                                                                                           | `1`                                   |
| `loadSession`                           | `true`                                                                                        | `true`                                |
| `promptCapabilities`                    | `image`, `embeddedContext`                                                                    | `image`, `embeddedContext`            |
| `sessionCapabilities`, both             | `list`, `resume`, `close`, `fork`, `delete`, `title`, `additionalDirectories`, `systemPrompt` | same                                  |
| `sessionCapabilities`, one adapter only | `options`, `status`                                                                           | `subagents`                           |
| `mcpCapabilities`                       | `http`, `sse`                                                                                 | `http`; `sse` and `acp` are `false`   |
| `authMethods`                           | none listed                                                                                   | `api-key`, `chat-gpt`                 |
| Vendor CLI under the npm install tree   | `claude` 2.1.323 (SDK platform package 0.3.323)                                               | `codex-cli` 0.166.0 (`@openai/codex`) |

The last row is in `version.json` under `vendorCli`, recorded with `mea capture --vendor-cli` on the
binary each npm install tree ships. `mea` takes the name from the operator and never searches for
it, so the member says the version was named, not observed beneath the running adapter. Neither
equals the pin above (Claude Code `2.1.322`, Codex `0.167.1`). Codex `0.166.0` is the supported
minimum.

What it settles for the table above:

- Both adapters advertise `loadSession`, `sessionCapabilities.list` and `promptCapabilities.image`,
  so `capabilities_from` in `crates/mango-agent-acp/src/harness.rs` reports `resume`, `session_listing`
  and `images` as true for either. The advertisement is not a driven session: no `session/load`,
  `session/list` or image prompt has run against an adapter.
- Against the dedicated harnesses, the cells that move are Claude's: its dedicated harness has no
  session listing and no images, and `claude-agent-acp` advertises both. Codex's dedicated harness
  already has them.
- The rows that decide convergence do not move. The handshake carries no rate limits, steering,
  native review or typed questions for Codex, and no structured output or interrupt verdict for
  Claude. `codex-acp`'s `subagents` session capability is an adapter extension that this harness
  does not consume.
- `configOptions`, and with it the model catalog cell, comes from `session/new`, which `initialize`
  never carries. The next section reads it.

### What `session/new` measured

[#453](https://github.com/juliopolycarpo/mango-external-agents/issues/453) asked whether a capture
may send `session/new`. It may, as an opt-in: `mea capture --harness acp --profile <id> --open-session`
sends it after `initialize` and writes `session-new.json` beside the other files. The decision:

- The request is `{"cwd": <the capture workspace>, "mcpServers": []}`. The capture never calls
  `authenticate` and reads no credential. It is unauthenticated by construction: `mea` creates its
  own empty `HOME`, `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `XDG_*` directories, gives the adapter only
  those whatever the shell has set, and removes them afterwards, error or not. A logged-in machine
  therefore opens no session as its owner and writes nothing under `~/.claude` or `~/.codex`. The
  recipe is in [fixtures/README.md](../fixtures/README.md).
- An adapter that wants a login answers with a JSON-RPC error. The capture records the code and the
  redacted message in `session-new.json` and the manifest says `sessionOpened: false`. That is a
  recorded answer, not a failed capture.
- The record keeps ids, names, categories, types and current values, and drops the session id (it
  changes every run), the descriptions (they carry prices and wording) and `_meta`. The manifest's
  `probe` reads `initialize, session/new`, and `sessionOpened` is true only when a session opened.
- It is refused for a profile the drift lane records again (`opencode`): what `session/new` answers
  follows the machine's agent state, and a reproducible contract must not.
- Opening a session makes the Claude adapter write its own state (`.claude.json`, a backup, empty
  `projects`, `sessions` and `plugins` directories) under the scratch `CLAUDE_CONFIG_DIR`, where its
  `initialize` capture wrote nothing. `codex-acp` writes its SQLite state under `CODEX_HOME` for
  either probe. Nothing else is sent, and the child is stopped after the answer.

Measured on 2026-11-18, with no login on either adapter:

| `session/new` member   | `claude-agent-acp` 0.107.0                                       | `codex-acp` 2.6.0                         |
| ---------------------- | ---------------------------------------------------------------- | ----------------------------------------- |
| Answer                 | a session                                                        | error `-32000`, "Authentication required" |
| `modes`, current       | `auto`                                                           | none: refused                             |
| `modes`, available ids | `default`, `acceptEdits`, `plan`, `auto`, `bypassPermissions`    | none: refused                             |
| `configOptions` ids    | `mode`, `model`, `effort`, `fast`, `ultracode`                   | none: refused                             |
| `model` option values  | `default`, `opus`, `fable`, `sonnet`, `haiku`; current `default` | none: refused                             |

What it settles:

- Claude's model catalog cell is measured: the adapter offers a `model` config option, with
  aliases, not full model ids, and without a login. A signed-in account may be offered a different
  list; this one is what the adapter returns to an empty configuration. `model_catalog` stays false
  because the harness reads the live catalog at session time and invents none.
- Claude's mode ids are now on file. `SessionModeIds` in `crates/mango-agent-acp/src/profile.rs`
  still holds none for either profile: which of these ids means `ReadOnly`, `Default` or
  `FullAccess` is a policy decision for a separate change, and the `Permission levels` cell above
  stays as it is until then.
- Codex's catalog and mode ids stay unmeasured. `codex-acp` refuses `session/new` without a login,
  and the refusal is the recorded shape. Measuring them needs the maintainer's own signed-in
  `CODEX_HOME`, and `--open-session` deliberately never reads one; that is a separate decision.

Criterion 2 is not met, and this is a decision rather than an oversight
([#454](https://github.com/juliopolycarpo/mango-external-agents/issues/454)). Both adapters install
into an empty directory with `npm install --ignore-scripts` and recapture byte for byte without a
login, so the criterion could be met. `scripts/install-vendor-cli.sh` does not know either adapter,
though. `claude-agent-acp` declares exact dependencies, so its whole tree follows the adapter version;
`codex-acp` declares `@openai/codex` as `^0.166.0`, so the binary beneath it floats within 0.166.x
unless a lockfile pins it. Building the lane waits for criterion 1, because a reproducible contract
cannot move the verdict while the vendors ship no ACP mode, and a recapture of the latest adapter
reports additions that nothing in the library reads. The evidence is in
[vendor-drift.md](vendor-drift.md#adapter-captures-not-a-scheduled-lane). Both manifests say
`reproducible: false` and carry `capturedFrom` and `capturedAt`. The install and capture recipe,
and when to run it, is in [fixtures/README.md](../fixtures/README.md).

## Does any surface report the vendor build an adapter runs?

No documented, non-secret surface does, so the `vendorCli` member stays what the operator named
([#480](https://github.com/juliopolycarpo/mango-external-agents/issues/480)). Read on 2026-11-17
from the npm install trees of `claude-agent-acp` 0.107.0 and `codex-acp` 2.6.0, with
`--ignore-scripts`, empty `CLAUDE_CONFIG_DIR`, `CODEX_HOME` and `HOME`, and no login:

- **`initialize` `agentInfo`** names the adapter itself. The raw reply carries
  `{"name": "@agentclientprotocol/claude-agent-acp", "title": "Claude Agent", "version": "0.107.0"}`
  and `{"name": "@agentclientprotocol/codex-acp", "title": "Codex", "version": "2.6.0"}`. In
  `claude-agent-acp` the source fills the three members from its own `package.json`. Neither reply
  has a member for the CLI beneath it, in `agentInfo` or in `_meta`.
- **Adapter flags.** `codex-acp --version` prints `@agentclientprotocol/codex-acp 2.6.0`, again the
  adapter. The Claude adapter documents no version flag, and its `--help` starts the server. Both
  packages carry an undocumented passthrough to the vendor CLI (`--cli` or `cli`); a documented
  contract would be needed before `mea` runs it, because the adapter's own resolution is not one
  ([harness-acp.md](harness-acp.md#which-vendor-binary-an-adapter-runs)).
- **The adapters' own documentation** (the READMEs and the pages the project reads, copied on the
  same date) names `CLAUDE_CODE_EXECUTABLE` and `CODEX_PATH` as the overrides and says nothing about
  reporting the result.
- **`session/new`** returns modes and `configOptions`, not a build. It also needs a session, which a
  capture opens only with `--open-session`, under a scratch home, and which records no build either
  ([what it measured](#what-sessionnew-measured)).

What the record can claim is unchanged: `version.json` holds the adapter's version from `agentInfo`
and, under `vendorCli`, the `--version` of the binary the operator named. When a capture names a
binary other than the one the adapter resolves, nothing in the handshake shows it. Whoever reads
`vendorCli` as "the build that ran" must check the adapter's environment themselves
(`CLAUDE_CODE_EXECUTABLE` and `CODEX_PATH` unset means the package's bundled copy). Reopen this
when an adapter documents a field or a flag that reports the build it spawns; a `mea` change would
then record it beside the operator's claim and flag a mismatch.

## Decision criteria

Converge a dedicated harness onto an ACP profile only when all of these hold. Each names its
evidence.

1. **The vendor ships the mode.** The vendor's own CLI documents an ACP command or flag, cited from
   the vendor's docs and visible in the pinned `help.txt`. The repository follows official CLIs and
   documented surfaces ([compliance.md](compliance.md)); an adapter from the ACP project does not
   meet that bar, because it would put a third-party process on the path to the vendor.
2. **The contract is reproducible.** `mea capture --harness acp --profile <id>` records the
   `initialize` answer and `vendor-drift.yml` pins and regenerates it, as it does for OpenCode. A
   capture marked `reproducible: false` cannot carry a pin the lane keeps.
3. **Parity on the rows above.** For each capability the dedicated harness has and the profile
   lacks, either the ACP surface carries it (shown by a capture or transcript) or the loss is
   accepted in writing with the hosts that use it. The deciding rows are rate limits, steering,
   review and typed questions for Codex, and structured output and the interrupt verdict for Claude.
4. **Process ownership holds.** The host's `ProcessLauncher` owns one process tree and names the
   vendor binary. A cancel, close and forced kill reach the CLI under any intermediate process, the
   environment allowlist reaches it, and the version gate and pinned contract describe the binary
   that runs. A test against a fake shows it.
5. **Approval and auth rules are unchanged.** Brokered approvals only, no `authenticate`, no
   credential read, and `AuthState` honest about what is `Unknown`. If the profile makes the login
   probe less informative, the loss is stated.
6. **A migration path exists.** `HarnessIdentity`, the capability set and the published crate names
   are public API on a 0.x line. Removal needs a deprecation release and a documented mapping.
7. **The fixtures move with it.** Transcripts and `fixtures/<vendor>/contract/` get ACP equivalents
   captured by `mea capture`, never edited by hand.

Criterion 1 is the gate: until it holds, weighing the other six is wasted work.

## The drift-lane trigger

The weekly `latest-capture` job in [vendor-drift.yml](../.github/workflows/vendor-drift.yml)
installs the newest Claude Code and Codex, regenerates the public contracts and opens the managed
`drift(claude)` or `drift(codex)` issue when the diff is not empty
([vendor-drift.md](vendor-drift.md)). The contracts record each CLI's own help text, so a native ACP
mode surfaces there, in the documentation watch or in the subcommand help watch below.

- **Trigger:** a `drift(claude)` or `drift(codex)` issue that shows one of:
  - a diff that adds an `acp` command, an ACP-named flag, or an ACP entry in
    `codex app-server --listen`;
  - a failed documentation watch (`scripts/check-vendor-acp-docs.sh`) where the vendor's
    documentation index or CLI reference now mentions ACP. The issue body names the page and the
    line ([vendor-drift.md](vendor-drift.md));
  - a failed subcommand help watch (`scripts/check-vendor-subcommand-help.sh`) where a
    subcommand's own `--help` now names ACP. The issue body names the command and the line.
- **Not a trigger:** the same watch failing because a page could not be read or no longer has its
  anchor phrase, or because a reviewed allowlist entry matches no line any more, or the subcommand
  help watch exiting 2 because a command's help could not be read. Those are a broken watch to
  repair or an entry to delete, not a vendor signal.
- **Action:** whoever triages it reopens this assessment: cite the vendor document that describes
  the mode, re-read the capability table against a fresh `initialize` capture, decide criterion by
  criterion, and record the outcome here with a new read date. A documentation page without a
  shipped command does not meet criterion 1.

The lane does not watch one thing:

- An adapter's own `initialize` answer. It is captured once and no lane records it again. That is a
  decision, not a limit: it moves between adapter releases, and nothing the library reads has moved
  across the versions compared (see above).

## Accepted documentation mentions

A vendor page line that names ACP and that the project decided to keep, for example an index line
about a third-party adapter, is accepted in
[`scripts/vendor-acp-docs-allowlist.txt`](../scripts/vendor-acp-docs-allowlist.txt) and recorded
here. Record each entry as its exact line text, on a line of its own inside a `text` code fence,
followed by the vendor and why it does not meet criterion 1. `scripts/test-vendor-drift.sh` fails
when an allowlist entry is not a whole line of this section, and names the entry
([vendor-drift.md](vendor-drift.md)).

No entry is accepted at this revision.
