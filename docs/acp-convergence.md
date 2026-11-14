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

| Capability                 | Claude dedicated                                                        | Codex dedicated                                    | ACP profile (either adapter)                                                      |
| -------------------------- | ----------------------------------------------------------------------- | -------------------------------------------------- | --------------------------------------------------------------------------------- |
| Structured output          | yes: `--json-schema`, `structured_output`                               | not driven                                         | no: no driven ACP method carries a schema                                         |
| Interactive approvals      | no: modes and allow-lists only                                          | yes, brokered                                      | yes, when the agent sends `session/request_permission`                            |
| Typed questions            | no                                                                      | yes (experimental upstream)                        | no: `elicitation/create` is declined                                              |
| Permission levels          | `ReadOnly`, `Default`, `FullAccess`, and `auto` for qualifying accounts | full matrix over `approvalPolicy` and sandbox      | `Default`, `ReadOnly`; `FullAccess` only if the profile knows a mode id (none do) |
| Resume                     | `--resume`; nonresumable after a forced kill                            | `thread/resume`                                    | `session/load`; both adapters advertise `loadSession`                             |
| Cancellation               | SIGINT or kill, with a recorded resumability verdict                    | `turn/interrupt`                                   | `session/cancel`, then the kill grace                                             |
| Steering                   | no                                                                      | yes                                                | no: ACP v1 has no surface; adapter extensions are not consumed                    |
| Native review              | no                                                                      | yes                                                | no: `native_review` stays false                                                   |
| Account quota, rate limits | no                                                                      | yes: `account/rateLimits/*` into `AccountLimits`   | no: `usage_update` reports the context window, not plan quota                     |
| Usage reporting            | yes: the `result` record's counts                                       | yes                                                | context-window use only                                                           |
| Model catalog flag         | yes, per build: aliases from `--model` help                             | yes: `model/list`                                  | `model_catalog` stays false; a `model` config option: unmeasured (`session/new`)  |
| Session configuration      | no: argv on a fresh child                                               | per turn                                           | yes: `session/set_config_option`                                                  |
| Session listing            | no                                                                      | yes                                                | `session/list`; both adapters advertise `sessionCapabilities.list`                |
| Images                     | no                                                                      | yes                                                | yes: both adapters advertise `promptCapabilities.image`                           |
| MCP passthrough            | `--mcp-config`, per build                                               | per-thread `config.mcp_servers`                    | `mcpServers`; HTTP only when advertised                                           |
| Login state                | `auth status` probe                                                     | `account/read`                                     | always `Unknown`                                                                  |
| Public contract under CI   | `fixtures/claude/contract/`                                             | `fixtures/codex/contract/` and the vendored schema | labelled maintainer captures, not pinned (see below)                              |

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
nothing else: no `authenticate`, no `session/new`.

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

The last row is not in the capture. `mea` records the adapter's own `--version` and nothing beneath
it, so these two versions were read with each binary's own `--version` from the install tree the
capture ran against. Neither equals the pin above (Claude Code `2.1.322`, Codex `0.167.1`).
Codex `0.166.0` is the supported minimum.

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
- `configOptions`, and with it the model catalog cell, stays unmeasured. An agent returns it from
  `session/new`, and a capture never sends that: it opens no session.

Criterion 2 is not met, and this is a decision rather than an oversight. `scripts/install-vendor-cli.sh`
downloads an exact vendor release asset and checks its digest. Neither adapter is an asset it knows,
and an exact `npm install` of the adapter pins only the top level: `codex-acp` declares
`@openai/codex` as `^0.166.0`, so the binary beneath it is whatever the range resolves to on the
day. Both manifests therefore say `reproducible: false` and carry `capturedFrom` and `capturedAt`.
The install and capture recipe is in [fixtures/README.md](../fixtures/README.md).

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
mode surfaces there, or in the documentation watch below.

- **Trigger:** a `drift(claude)` or `drift(codex)` issue that shows one of:
  - a diff that adds an `acp` command, an ACP-named flag, or an ACP entry in
    `codex app-server --listen`;
  - a failed documentation watch (`scripts/check-vendor-acp-docs.sh`) where the vendor's
    documentation index or CLI reference now mentions ACP. The workflow log names the page and the
    line ([vendor-drift.md](vendor-drift.md)).
- **Not a trigger:** the same watch failing because a page could not be read or no longer has its
  anchor phrase, or because a reviewed allowlist entry matches no line any more. Those are a broken
  watch to repair or an entry to delete, not a vendor signal.
- **Action:** whoever triages it reopens this assessment: cite the vendor document that describes
  the mode, re-read the capability table against a fresh `initialize` capture, decide criterion by
  criterion, and record the outcome here with a new read date. A documentation page without a
  shipped command does not meet criterion 1.

The lane does not watch two things:

- An ACP flag under another subcommand's help before any watched page names it. Subcommand help is
  kept out of the public contracts ([vendor-drift.md](vendor-drift.md)):
  [#451](https://github.com/juliopolycarpo/mango-external-agents/issues/451).
- An adapter's own `initialize` answer. It is captured once and no lane records it again, because
  neither adapter is pinned (see above).
