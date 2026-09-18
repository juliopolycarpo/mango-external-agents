# Agent Client Protocol harness (`mango-agent-acp`)

One harness for every agent that speaks the [Agent Client Protocol][acp] as the agent side, over the
official [`agent-client-protocol`][sdk] crate. What differs between agents is not the protocol — it is
the argv, the version string, the login command and the documents a host's disclosure links, and those
live in a **profile**. Adding an agent is a table entry, not a new harness.

Facts here were read on 2026-09-17 against the pages linked inline. Re-verify before relying on them.

[acp]: https://agentclientprotocol.com/protocol/overview
[sdk]: https://crates.io/crates/agent-client-protocol

## The surface driven

ACP **v1 only**. The SDK's `unstable_protocol_v2` feature and every other draft feature stay off, so
the wire this crate speaks is the one the specification documents as stable. An agent whose
`initialize` answers a different `protocolVersion` is refused before a session exists — negotiating
down would mean sending v1 messages to an agent that answered something else.

| What      | Method                                                                  | Reference                                                                    |
| --------- | ----------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| Handshake | `initialize` (`protocolVersion: 1`, `clientCapabilities`, `clientInfo`) | [initialization](https://agentclientprotocol.com/protocol/v1/initialization) |
| Open      | `session/new` with the host's authorised working directory              | [session setup](https://agentclientprotocol.com/protocol/v1/session-setup)   |
| Resume    | `session/load`, when `agentCapabilities.loadSession`                    | [session setup](https://agentclientprotocol.com/protocol/v1/session-setup)   |
| Turn      | `session/prompt`, whose response is the turn's end                      | [prompt turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)       |
| Stream    | `session/update` notifications                                          | [prompt turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)       |
| Approvals | `session/request_permission`                                            | [tool calls](https://agentclientprotocol.com/protocol/v1/tool-calls)         |
| Cancel    | `session/cancel`                                                        | [prompt turn](https://agentclientprotocol.com/protocol/v1/prompt-turn)       |
| Level     | `session/set_mode`, when the profile knows a mode id                    | [session modes](https://agentclientprotocol.com/protocol/v1/session-modes)   |
| Listing   | `session/list`, when `sessionCapabilities.list`                         | [session setup](https://agentclientprotocol.com/protocol/v1/session-setup)   |
| Close     | `session/close`, when `sessionCapabilities.close`                       | [session setup](https://agentclientprotocol.com/protocol/v1/session-setup)   |

`authenticate` is **never sent**. See [Auth](#auth).

## Transport

`TransportKind::Acp` only, through `AcpSpec::ChildPipes`. The host's `ProcessLauncher` spawns the
agent and the library receives the pipes; the SDK's own `AcpAgent` and `Stdio` carriers are unusable
here because the first spawns the agent itself and the second takes over this process's stdio.

The core's `LineStream` and `ByteSink` frame the pipes under `Limits::line`. `BoundedTransport`
passes frames through the SDK's [`Channel` interface](https://docs.rs/agent-client-protocol/2.1.0/agent_client_protocol/struct.Channel.html),
leaving JSON-RPC parsing and routing to the official SDK. It caps queued frame counts at
`max_pending_requests` and serialized bytes at `turn_buffer_bytes` in each direction. Output bytes
remain charged through the physical write. Batch sizes have the same pending-message cap. Queue
pressure fails the connection explicitly and triggers native cleanup.

The SDK's `Lines` carrier uses unbounded internal queues; its `ByteStreams` carrier also frames input
without the host's line cap. Neither provides the budgets this harness requires.

`AcpSpec::Http` exists in the core and is **not implemented**: `agent-client-protocol-http` 2.1.0 pulls
`aws-lc-rs` through `reqwest` 0.13's `rustls` feature and through `async-tungstenite`'s
`tokio-rustls-webpki-roots`, and this workspace's `deny.toml` bans it — TLS is `ring` everywhere.
Asking for it is `Error::UnsupportedTransport`.

## The driving model

The SDK's connection is a scope rather than an object: `connect_with` runs the dispatch loop for
exactly as long as its closure, and the closure is the only place a `ConnectionTo` exists. A `Session`
outlives any one call, so the closure hands a clone out through a channel and parks on a shutdown
signal, with the whole thing on one spawned task per session.

This works because 2.1.0's connection is `Send`: `ConnectTo` is `Send + 'static`, its future is `Send`,
and `ConnectionTo<Agent>` is `Clone + Send + Sync`. No `LocalSet` and no thread per session.

Handlers leave the dispatch loop promptly:

- A `session/update` handler reserves bounded transcript capacity and returns. If the transcript
  budget is exhausted, the stream commits its terminal error and the prompt owner cancels native work;
  dispatch never waits for a host to read.
- A `session/request_permission` handler must not wait for an answer, because the answer arrives
  through `Session::respond` on another task. It parks the agent's responder and returns; broker
  deliberation is a separately bounded callback, capped by `Limits::max_pending_requests`.

The typed handlers are installed before connecting. A final SDK handler consumes unsupported
notifications and answers unsupported requests with `Method not found`; it does not retain them
for a future dynamic session handler. Responses continue through the SDK's correlation router.

Every request except `session/prompt` is bounded by `Limits::request_timeout` and fails with
`Error::Timeout` naming the method. The prompt is a turn and may take as long as the agent needs.

## Turns

One `session/prompt` in flight per session. The response *is* the turn's end, so two prompts would race
for one stream of updates with nothing on the wire to tell them apart; a second is refused with
`Error::Busy` rather than queued. The slot belongs to the prompt. Dropping its `TurnStream`, an
overflow, or an explicit cancel first sends ACP's `session/cancel`, waits for `Limits::kill_grace`,
then escalates through the host's process control if the prompt remains live. The owner remains
installed until that native work can no longer report, and each turn carries a generation so a late
prompt can only ever end its own turn.

ACP v1 does not acknowledge a submitted prompt or assign it a turn handle. The SDK accepting the
request into its outgoing queue is therefore `Dispatch::AcceptanceUnknown`: after a broken pipe, a
host cannot safely replay the same prompt without knowing whether the agent received it.

`start_turn` and `close` share the core lifecycle gate while they synchronously claim the prompt slot
or close the session. The gate is released before every await, so a close either sees a claimed prompt
to end or prevents the start from submitting one after teardown begins.

`TurnStream::native_turn_id` is `acp-turn-<n>`, a per-session sequence this harness mints — ACP
names no turn handle of its own. It is minted synchronously, before the prompt reaches the wire, so
`EventKind::TurnStarted` is genuinely the first event a turn produces rather than something that
arrives only if the agent answers. The cost is that this id appears in no captured transcript: it is
the harness's own counter, not something a vendor said.

Steering is `Error::NotSupported` on every profile: ACP v1 has no surface for adding to a turn that is
already running.

### What a `session/update` becomes

| ACP                                                                  | Event                                                                                 |
| -------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `agent_message_chunk` (text)                                         | `TextDelta`                                                                           |
| `agent_thought_chunk`                                                | `ReasoningStarted` / `ReasoningDelta` / `ReasoningEnded`                              |
| `user_message_chunk`                                                 | dropped — it is the host's own prompt echoed back                                     |
| `tool_call`                                                          | `ActivityStarted`, plus `ActivityCompleted` when it already carries a terminal status |
| `tool_call_update`                                                   | `ActivityUpdated`, or `ActivityCompleted` on `completed`/`failed`                     |
| `plan`                                                               | `ActivityStarted`/`ActivityUpdated` under one synthetic call id                       |
| `available_commands_update`                                          | session state, not a turn event — the snapshot's `commands`, names bare               |
| `usage_update`                                                       | `ThreadUsage`, with `size` as the context window                                      |
| `current_mode_update`, `config_option_update`, `session_info_update` | dropped — session state, not transcript                                               |

ACP calls them tool calls; they arrive as *activity* because nothing in this library may reach a host's
tool registry. The reasoning pair is synthesised: ACP streams thought chunks with no start or end
marker, and a host relies on the pair to tell "still running" from "the agent withheld it".

Two deliberate limits, each with a test pinning it:

- A non-text block in an *agent message* — an image, audio, an embedded resource — produces no event.
  Rendering one into words would attribute prose to the agent that it did not write.
- `usage_update` reports context **in use** against the window, not cumulative spend. It reaches a host
  as `ThreadUsage.total` with `context_window_tokens` set, which is the only denominator that makes a
  percentage honest; there is no per-turn figure, because `PromptResponse.usage` is behind the SDK's
  `unstable_end_turn_token_usage` feature.

A `stop_reason` of `cancelled` ends the turn as a cancellation carrying the reason the *host* gave —
ACP supplies none of its own, and flattening it would report a shutdown as "you stopped this turn".
Every other reason, `refusal` included, completes the turn: a refusal is the agent ending its own turn,
and reporting it as an error would tell a host to retry a decision.

## Permissions

Two axes, six cells, answered per profile by `profile::matrix`. Routing never varies — who answers an
approval is the host's own arrangement (a `PermissionBroker`, or the `ApprovalRequested` event) and the
agent cannot tell the difference. Level does, and the three cases are **not** symmetric:

- **`Default` — supported.** What plain ACP already is: the agent asks, and the host answers — through
  its `PermissionBroker` if it installed one, otherwise through the `ApprovalRequested` event.
- **`ReadOnly` — supported.** Refusing every request the agent raises grants nothing, and it is the
  host's own standing instruction rather than a decision the library made. The question is still
  emitted, so a host sees what was asked, then resolved with `DecisionSource::AutoReview`. An agent that
  offered no way to refuse leaves nothing to pick, so that question reaches a person instead — and the
  broker is **not** consulted in that case, nor in any other under this level. A policy answering
  `Allow` would become an allowing option id on the wire, which is the one outcome this level exists to
  make impossible.
- **`FullAccess` — `NotOfferedByVendor`**, unless the profile knows the agent's own mode id for it.
  Reaching it by answering would mean the library *allowing* on the agent's behalf, which is the one
  thing nothing here may do.

A level a profile cannot reach is refused at `open_session` and again at `start_turn`, never
downgraded: silently running a read-only request under "ask every time" would grant more freedom than
anybody chose.

A patch axis left at `keep` leaves the vendor's configuration untouched. After a host makes an
explicit selection, later turns inherit it until another accepted override replaces it.
`Session::snapshot().configuration.accepted` reports that current selection. A rejected turn cannot
change it.

Native option `Set` and `Reset` patches are refused before launch or prompt submission: this
adapter has no native configuration catalog or mapping. `Keep` remains a no-op. A turn's local
settings become inherited only when `session/prompt` enters the SDK, under the same gate that
excludes close; reserving a turn handle alone does not accept its patch. The
[v1 prompt lifecycle](https://agentclientprotocol.com/protocol/v1/prompt-turn) defines the prompt
response as completion, so the adapter does not wait for that response to publish local acceptance.

A per-turn level is compared to the session's **by mode**, and any pair whose mode differs from the one
the session was opened under is refused in both directions. Narrowing looks harmless and is not: a turn
asking for `ReadOnly` on a session the agent runs in its own full-access mode would register a
read-only turn while the agent, still in that mode, raises no permission request at all for the
standing refusal to answer.

Cancelling withdraws every question the agent is waiting on, with ACP's own `Cancelled` outcome — the
specification requires it, and an agent whose permission await is not itself cancellation-aware never
returns from its tool call otherwise, so the turn would end with no terminal at all.
Permission requests arriving after cancellation receive that same outcome, even when no permission
level was selected. They cannot become new pending questions or reach the broker.
Cancellation and completion cleanup remain attached to their turn. A close that wins before the
prompt is queued prevents that prompt from being sent.

ACP's four option kinds map onto the core's effect and reach, so a host policy can answer without
reading a label in a language it does not know. The option set itself is passed through with the
agent's own ids, order and words.

| ACP kind        | Effect | Scope  | Writes a rule |
| --------------- | ------ | ------ | ------------- |
| `allow_once`    | Allow  | `Once` | no            |
| `allow_always`  | Allow  | —      | **yes**       |
| `reject_once`   | Reject | `Once` | no            |
| `reject_always` | Reject | —      | **yes**       |

The two `always` kinds carry no scope on purpose. ACP says the agent should remember the choice; it
does not say for how long, and a session-wide reading would be a promise the protocol never made.
`policy_changing` says exactly what ACP does state, and an unstated reach is never rendered as the
narrow one. The request id is one the harness mints itself, never reused for the life of
the session — not the tool call id, which repeats when an agent asks about the same call twice, and
not the JSON-RPC id, which is the agent's own to choose and to reuse once a request is no longer
outstanding.

`PermissionRequest::expires_at` comes from `Limits::approval_timeout`. The core's
`ApprovalDeadline` starts when the question arrives and covers broker deliberation and host response
time. Answers at or after the deadline cannot allow work, even before the timer task runs.
Expiry selects the agent's `reject_once` option and records `DecisionSource::Expired`. If the agent
offers no one-time refusal, the harness cancels the turn with `CancelReason::Timeout` and
withdraws the question with ACP's `Cancelled` outcome;
there is no `ApprovalResolved` selection event because no vendor option was selected. It never
chooses `reject_always` for a timeout. These outcomes follow the
[ACP v1 permission specification](https://agentclientprotocol.com/protocol/v1/tool-calls#requesting-permission).
The timer answers the agent even if the bounded event channel is full. Approval events remain
ordered before the turn terminal and arrive when the host resumes reading.

## Auth

**No login handling, ever.** `authenticate` is never sent, no browser is opened and no credential is
read, stored or forwarded.

`AuthState` is **always `Unknown`** for every ACP agent. That is not a gap in the probe: ACP's
`initialize` reports which authentication *methods* an agent offers and has no field anywhere for
whether somebody is signed in, so the only way to find out is to try `session/new` and see. When it
answers `-32000` (`AuthRequired`), `open_session` fails with `Error::AuthRequired` carrying the
profile's own login command as **text for a person to run**. A profile with no login command of its own
— a CLI that signs in inside an interactive session — carries its documentation link instead, because
printing a plausible command would send a person to a prompt that does not exist.

## Discovery

The probe runs the profile's version argv through the host's launcher and reads what the agent printed.
That argv is usually `<executable> --version`, but a profile may carry its own: Grok's version argv is
`grok --no-auto-update --version`, so a probe does not trigger the background self-update the session
argv already declines (see [Profiles](#profiles)). It does **not** search `PATH` (the library never does; a host that resolved a path passes it on
`AcpHarness::with_executable`) and it does **not** run `initialize`, because a handshake is a session
and discovering an agent should not open one.

The harness uses that path for both discovery and sessions and returns it in `Discovery::executable`.
`OpenSession::with_executable` can override it for one session. This lets a host support custom
install locations without editing the profile or changing a user's `PATH`.

- A launcher that could not start the program is `GateVerdict::NotInstalled`.
- Output with no dotted number is `GateVerdict::Unknown`. An agent that changed the shape of
  `--version` is not an agent that stopped working.
- Versions compare as dotted numbers of any length rather than as semver, because Cursor versions by
  date; a semver parser would call `2026.08.25-3e8eec8` unparseable and gate a build that works.
- `Discovery::capabilities` is as wide as the harness **ceiling**, because what a build supports is
  only knowable from `initialize`. What the agent actually advertised arrives as the session-effective
  tier on `Session::snapshot().capabilities` after `open_session` — which is the case the three
  capability tiers exist for.

## Client capabilities

Everything declined: `fs.readTextFile`, `fs.writeTextFile` and `terminal` are all `false`. The host owns
files and terminals, so an agent asking this client to write one would be asking the library to act on
the host's filesystem on a third party's instruction. This is not a gap — every agent here has its own
file and shell tools and uses them, which is what the activity events describe.

## Profiles

| Profile                        | ACP argv                             | Login                 | Verified            |
| ------------------------------ | ------------------------------------ | --------------------- | ------------------- |
| [`cursor`][p-cursor]           | `cursor-agent acp`                   | `cursor-agent login`  | **yes**, 2026-09-13 |
| [`grok`][p-grok]               | `grok --no-auto-update agent stdio`  | `grok login`          | **yes**, 2026-09-13 |
| [`opencode`][p-opencode]       | `opencode acp`                       | `opencode auth login` | no                  |
| [`gemini`][p-gemini]           | `gemini --acp`                       | none; inside `gemini` | no                  |
| [`copilot`][p-copilot]         | `copilot --acp`                      | `copilot login`       | no                  |
| [`goose`][p-goose]             | `goose acp --with-builtin developer` | `goose configure`     | no                  |
| [`codex-acp`][p-codex]         | `codex-acp`                          | `codex login`         | no                  |
| [`claude-agent-acp`][p-claude] | `claude-agent-acp`                   | `claude auth login`   | no                  |
| `custom`                       | the host's own argv                  | the host's own        | no                  |

[p-cursor]: https://cursor.com/docs/cli/acp
[p-cursor-install]: https://cursor.com/docs/cli/installation
[p-grok]: https://docs.x.ai/build/cli/headless-scripting
[p-opencode]: https://opencode.ai/docs/acp/
[p-gemini]: https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md
[p-copilot]: https://docs.github.com/copilot/reference/copilot-cli-reference/acp-server
[p-goose]: https://block.github.io/goose/docs/advanced/acp-protocol
[p-codex]: https://github.com/agentclientprotocol/codex-acp
[p-claude]: https://github.com/agentclientprotocol/claude-agent-acp
[p-powershell-file]: https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_powershell_exe?view=powershell-5.1

`tests/smoke.rs` passed on 2026-09-13 against `cursor-agent` 2026.09.10-fd3934a and Grok 1.0.30.
Each opened a session, returned `pong` and closed. Neither run sent ACP `authenticate`.

`verified` says a profile was driven against the agent itself by `tests/smoke.rs`. Everything else is a
documented entry that nobody has run; a host can say so in its own interface, and nothing here pretends
otherwise.

### Executable names and installation order

Cursor's [installer](https://cursor.com/install) creates both `agent` and `cursor-agent` as aliases
of the same executable. Grok also installs an `agent` command, so the Cursor profile uses
`cursor-agent` for discovery, sessions and its login hint. It never falls back to `agent`: a version
number from that command cannot establish which vendor owns it.

On native Windows, Cursor documents a PowerShell installation and `agent --version` verification in
its [installation guide][p-cursor-install]. When its native entrypoint resolves to
`cursor-agent.ps1`, the launcher uses `powershell.exe -NoProfile -NonInteractive -File <script>
<args>`. PowerShell documents that `-File` passes the following values as script arguments, while
`-NoProfile` skips user profiles and `-NonInteractive` turns a prompt into a failure rather than a
hang in [about_PowerShell_exe][p-powershell-file]. The launcher regression tests cover that
invocation. A native Windows smoke turn completed against Cursor `2026.08.04-aaa8809`,
with the expected text and a terminal completion event.

The Grok command is `grok --no-auto-update agent stdio`, using the ACP invocation and update control
documented in [Headless & Scripting][p-grok]. Its version argv carries the same flag —
`grok --no-auto-update --version` — because a probe is the one call a host makes before it has decided
to run this agent at all, and it should not be the call that updates it. Verified against Grok 1.0.30,
which prints `grok 1.0.30 (04b7ffed98c6) [stable]`. Using the distinct executable names allows
both installations to coexist regardless of which installer last claimed `agent`. Hosts with a
custom installation can supply their own resolved executable path.

Grok's documented example calls `authenticate` before `session/new`. This library never sends that
request. The profile can only use a build that accepts an existing local login without it; an
authentication refusal stays `AuthRequired` with `grok login` as text for the host to display.
Reuse of an existing local login was verified against Grok 1.0.30. Its documentation does not
promise that the explicit authentication step can always be omitted.

Two other spellings worth recording:

- Gemini's flag is **`--acp`**; `--experimental-acp` is its deprecated predecessor.
- The Claude Code adapter moved twice and is now `@agentclientprotocol/claude-agent-acp` with the
  binary **`claude-agent-acp`**. The older `claude-code-acp` spelling launches an orphaned package.

### `npx` and the two npm shims

Neither adapter is launched through `npx -y`. Their READMEs document that form for a person to run, and
the library does not invoke a package fetcher on its own initiative — "no downloaded binaries" is an
invariant, not a default. A profile test asserts no built-in argv names one. A host that wants the
`npx` form supplies it through `AcpProfile::custom`.

### No version floors

No profile pins a `minimum_version`. A floor nobody has checked would refuse a build that works, and
`GateVerdict::Unknown` is the honest answer for a version this harness cannot read.

### Environment keys

Built-in profiles do not forward API keys or tokens from the host's environment. Sign-in remains
the user's arrangement with the installed CLI. Beyond the core's base allowlist, only `codex-acp`
adds `NO_BROWSER`, the [adapter's documented switch][p-codex] for suppressing a sign-in page.

## Sessions

| Method                  | Behaviour                                                                                                                        |
| ----------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| `start_turn`            | `session/prompt`; one in flight                                                                                                  |
| `respond`               | answers one parked `session/request_permission`; an answer to an already-settled question is accepted and sends nothing further  |
| `cancel`                | withdraws every pending question, then `session/cancel`, recording the host's reason first                                       |
| `close`                 | idempotent: withdraws every pending question, sends `session/close` when advertised, ends the child, then settles the owned turn |
| `steer`                 | `Error::NotSupported`                                                                                                            |
| `list_sessions`         | `session/list` when `sessionCapabilities.list`, else `Error::NotSupported`                                                       |
| `start_review`          | `Error::NotSupported`                                                                                                            |
| `refresh_account_usage` | `Error::NotSupported` — v1 reports a session's context window, never an account's plan quota                                     |

A resume against an agent that does not advertise `loadSession` is `Error::Protocol` under
`ResumeMode::Strict`, and a fresh conversation with `SessionSnapshot::fallback_reason` set otherwise.

`NativeSession::updated_at` is left absent even when `session/list` returns one: it is an RFC 3339
string on the wire and a `SystemTime` in the core, and a date crate for one optional picker field is not
worth the dependency.

## Attachments

`Attachment` bytes come from the host, which owns the filesystem — the library never reads a file. Text
goes as a text resource (an agent should not have to base64-decode a source file to read one line),
images as `ImageContent`, everything else as a blob. The URI scheme is `attachment:`, not `file:`: the
host has not said where the bytes came from, and a `file:` URI would name a path on the agent's own
machine that the agent might then try to read.

An attachment whose kind the agent never advertised in `promptCapabilities` is refused **before** the
turn starts. Rejected mid-turn it reads to a user as the agent breaking rather than as a file that was
never going to work.

## Known caveats

- **No MCP passthrough.** `OpenSession::mcp_servers` is refused when nonempty, before launching the
  agent. This harness sends an empty `session/new.mcpServers` and reports `mcp_passthrough: false`.
- **No model selection.** ACP v1 has no surface for one, so a `Configuration` naming a model or a
  reasoning effort is refused rather than silently ignored, and `model_catalog` is never reported.
- **Public OpenCode contract capture.** `mea capture --harness acp --profile opencode` records the
  installed CLI's version and its v1 `initialize` answer under `fixtures/acp/opencode/contract/`.
  It sends no `authenticate` or `session/new` request. The capture keeps each auth method's `type`,
  `id` and `name` and the shape of `agentCapabilities`, drops `_meta` extension objects, and
  replaces every string *value* inside `agentCapabilities` with `[REDACTED]` rather than guessing
  which of them is a machine path. It ends the child immediately after the answer.
  `testing::FakeAcpAgent` still covers the full v1 lifecycle; `tests/smoke.rs` drives a real agent
  on demand.

## Smoke test

```bash
cargo run -p mea -- discover                      # all linked harnesses and ACP profiles
cargo run -p mea -- discover --harness acp:grok   # one profile in detail
MEA_ACP_PROFILE=cursor cargo test -p mango-agent-acp --all-features \
    --test smoke -- --ignored --nocapture         # one real turn
```

Compliance posture: see [compliance.md](compliance.md).
