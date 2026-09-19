# OpenAI Codex harness (`mango-agent-codex`)

Drives the `codex` CLI a user already installed, through `codex app-server` — the interface OpenAI
documents for rich clients and uses for its own VS Code extension.

Facts on this page were read on 2026-09-17 against `codex-cli 0.154.0`. Re-verify against the
vendor's current documentation before relying on them.

## The executable and the version gate

|                   |                                                                                                     |
| ----------------- | --------------------------------------------------------------------------------------------------- |
| Executable        | `codex`, resolved by the host (`OpenSession::with_executable`) or by name                           |
| Arguments         | `app-server`, and nothing else                                                                      |
| Version read from | `codex --version` for discovery; the handshake's own `userAgent` for a session or picker connection |
| Minimum version   | `0.154.0` (`MINIMUM_CODEX_VERSION`), which is also `vendor/PIN`                                     |

The floor is the pinned build rather than something older. The app-server's item families and its
`thread/`–`turn/` method names changed shape inside the 0.15x series, so a lower floor would be a
claim about builds nothing here was checked against. An older `codex` on `PATH` reports
`GateVerdict::VersionTooOld` with both numbers and claims no capabilities; it never crashes, and no
app-server is spawned for it.

A `--version` line nobody can parse is `GateVerdict::Unknown`, not a refusal: a CLI that changed
the shape of its version output has not stopped working, and the host may still choose to try.
The session and short-lived picker connection also apply that same floor to a parseable handshake
`userAgent` before they send a thread request, then clean up the child on a refusal.

## The documented surface this harness drives

Every method below follows the [official app-server documentation][app-server] and the
[`codex-rs/app-server/README.md`][readme] at `rust-v0.154.0`. JSON-RPC
2.0 over newline-delimited JSON, **without** the `"jsonrpc"` member — the README's own words:
"bidirectional communication using JSON-RPC 2.0 messages (with the `"jsonrpc":"2.0"` header omitted
on the wire)".

| Call                            | Used for                               |
| ------------------------------- | -------------------------------------- |
| `initialize`, `initialized`     | The handshake, carrying `clientInfo`   |
| `account/read`                  | Whether somebody is signed in, and how |
| `account/rateLimits/read`       | `Session::refresh_account_usage`       |
| `model/list`                    | `Discovery::models`                    |
| `thread/start`, `thread/resume` | `Harness::open_session`                |
| `thread/read`                   | Metadata-only resume workspace check   |
| `thread/list`                   | Session and harness-level listing      |
| `turn/start`                    | `Session::start_turn`                  |
| `turn/steer`                    | `Session::steer`                       |
| `turn/interrupt`                | `Session::cancel`                      |
| `review/start`                  | `Session::start_review`                |

`Harness::list_sessions` opens a short-lived app-server connection, initializes it, asks for a
page and closes it without starting a thread. Canceling the picker request also kills that
connection's child through the injected process control. A live `Session::list_sessions` reuses its own
connection. Both paths require an absolute, lexically normalized UTF-8 host working directory as
the `cwd` filter and refuse a query for another directory. Codex refuses that host configuration
before it launches `codex --version` or an app-server, without canonicalizing the path or reading
the filesystem. The vendor's cursor, native id, title, preview and Unix
second timestamps pass through when supplied. Rows with missing or foreign workspace paths are
discarded even if the server returns them under the `cwd` filter. Before `thread/resume`, the
harness calls `thread/read` with `includeTurns: false` and requires its native id and original
working directory to match the request and the host's authorised directory. It checks the resume
response again before exposing the handle. A `thread/read` absence alone never authorises a fresh
fallback: the pinned `thread/resume` missing-rollout result must still confirm that outcome.
Account usage remains session-scoped; the separate
`Harness::account_usage` service returns `Error::NotSupported`.

`clientInfo.name` is always the host's own name, from `HostContext::client_info`. The README says
this identifies the client to OpenAI's compliance logging platform, so writing anything else would
be a misattribution rather than a nicety.

Notifications acted on: `turn/started`, `turn/completed`, `item/started`, `item/completed`,
`item/agentMessage/delta`, `item/reasoning/textDelta`, `item/reasoning/summaryTextDelta`,
`thread/tokenUsage/updated`, `account/rateLimits/updated`, `serverRequest/resolved`, `error`.
Everything else is dropped by name.

**`turn/completed` is the only terminal.** The `error` notification reads like an ending and is
not one — it carries `willRetry`, and the turn's own completion still follows. Ending a turn there
would end the host's turn twice.

**Conversation events and approvals retain thread and native-turn ownership.** Delayed events
cannot finish a replacement turn. Reviews explicitly account for the early `turn/started` id
differing from the review response. Child-thread events remain excluded until the shared API
can represent parent-child activity and approval ownership. A terminal or an explicitly refused
`turn/start` clears every owner-bound marker before it releases admission, so a cancellation reaper
or a delayed request id cannot affect a replacement turn.

## Sessions, turns and steering

One long-lived `codex app-server` per session. `thread/start` opens a conversation;
`thread/resume` continues one, with `excludeTurns: true` — the vendor keeps the transcript it
wrote, and this library never replays one into anybody's context. `ResumeMode::Fallback` starts a
new thread only when the pinned app-server returns `-32600` with the exact
`no rollout found for thread id <requested id>` result. It records that reason in
`SessionSnapshot::fallback_reason` and exposes the new native id. The same error code can also
mean configuration failure, so other refusals, timeouts and broken connections remain errors.
This distinction follows the pinned [thread resume error mapping][resume-error] and is covered by
fake app-server tests for both outcomes.

`turn/start` on a live turn is taken by the app-server as a **steer** — its own documentation says
`turnTrigger` is "ignored when this request steers an already-active turn". A host that meant a new
turn would hold a stream that never gets a `turn/completed` of its own, so admission is one locked
transition and a genuinely live attempt receives the retryable typed `Error::Busy` refusal before
anything reaches the vendor.

`turn/steer` carries `expectedTurnId` as a precondition. A steer naming a turn that is not the one
running is refused here rather than landing on whatever turn happens to be live; the app-server's
own refusal (`-32600 "no active turn to steer"`) maps to `SteerRejection::TurnAlreadyCompleted`.

`review/start` is sent without `delivery`, which the server reads as inline: a detached review runs
on a thread this session is not subscribed to, and its events would arrive under an id the reducer
drops. `ReviewStream::review_thread_id` reports whatever the server named.

Hosts use the shared `ReviewTarget` enum for uncommitted changes, a base branch, a commit with an
optional title, or custom instructions. Each target uses the same bounded stream, cancellation,
and completion handling as an ordinary turn. Hosts do not construct Codex requests themselves.

Attachments: image attachments travel as `UserInput::image` with a `data:` URL, verified against a
real `turn/start`. PNG, JPEG, GIF and WebP are accepted, up to four attachments and 2 MiB each.
Unsupported kinds, media types and oversized inputs are rejected before encoding or writing a
request; `UserInput` has no general-purpose file arm.

Cancelling before `turn/start` answers records the request and keeps the vendor's turn slot
occupied. The returned handle is interrupted as soon as it arrives; another start is refused
until the vendor completes or the start fails. A transport failure after submission returns a
`TurnStream` marked `Dispatch::AcceptanceUnknown`, rather than claiming the turn never started;
the host retains that handle to reconcile or cancel it. Late start responses only update their own
start attempt, even if a host reuses a `TurnId`. This follows the vendor's requirement to name the
active turn in `turn/interrupt` and `turn/steer`.

The slot is released when Codex commits its native terminal, before the host drains buffered
events. A dropped library `TurnStream` is owner abandonment: the harness refuses pending approvals,
sends `turn/interrupt`, waits through the host's graceful-turn bound, and closes and reaps the
app-server if the turn will not settle. A browser disconnect that should leave work running must
therefore retain the stream in a host supervisor. Dropping the owning session follows the same
bounded cleanup path. Closing twice waits for the first reaper; if it cannot reap the child, each
caller receives the same `Error::CleanupRequired` control for host reconciliation.

Malformed terminal frames fail their addressed turn; an unrouteable terminal closes the session.
Connection loss and host shutdown terminate active streams, release approvals and reap the process.
Native activity restarts the host's `Limits::idle_timeout`, but only this turn's own: the connection
also carries a subagent's thread, a detached review's, later frames for a turn already over, and
account-level `rateLimits` updates that name no conversation at all, and none of those extend the
deadline. An outstanding approval pauses that clock because its `approval_timeout` is the deadline
that governs Codex's blocked wait. Idle expiry
uses the same `turn/interrupt` and bounded process-reaping path as a dropped stream.
The transcript has host-configured event, byte, and pending-request limits. Overflow commits a
reserved `stream-overflow` terminal and drives shutdown instead of growing memory, silently losing
an approval, or blocking cancellation on an unread consumer.

Native reviews reject steering with `TurnNotSteerable`. Cancellation and close retain the host's
reason when they win the terminal race; when Codex completed first, its completed outcome remains
the one terminal fact.

## The permission matrix

All six explicit (level, routing) pairs are supported. Omitted permission fields leave the user's
Codex profile in control: the harness sends no sandbox, approval policy or reviewer override for
an axis the host has never selected. An empty `ConfigurationPatch` does not impose read-only mode.
A selected level is two vendor settings that move together,
because setting one without the other produces a configuration nobody chose:

| `PermissionLevel` | `sandbox`            | `approvalPolicy` |
| ----------------- | -------------------- | ---------------- |
| `ReadOnly`        | `read-only`          | `never`          |
| `Default`         | `workspace-write`    | `on-request`     |
| `FullAccess`      | `danger-full-access` | `never`          |

`ReadOnly` pairs with `never` rather than `on-request` deliberately: nothing at that level may
change the machine, so an escalation prompt's only honest answer is no.

| `ApprovalRouting` | `approvalsReviewer` |
| ----------------- | ------------------- |
| `User`            | `user`              |
| `AutoReview`      | `auto_review`       |

`auto_review` was accepted by a real app-server on a consumer subscription, so refusing the cell
here would be this harness narrowing what the vendor offers rather than reporting it.

Per-turn permission changes send `approvalPolicy` and the structured `sandboxPolicy` together.
Read-only disables network access; workspace-write authorises only the host's workspace and
excludes temporary directories. Full access is sent only from a host-selected configuration.
These fields use the pinned CLI's schema, which does not yet declare the newer `ReadOnlyAccess`
fields shown in the current online documentation.

Codex persists successful turn overrides. `Session::snapshot().configuration` reports the settings
a later turn inherits, split three ways: `requested` is what the host asked for, `accepted` is what
this harness encoded onto `thread/start` and `turn/start`, and `observed` contains only fields the
app-server returned while opening the thread. A successful turn override clears an observed value
only for the axis it actually superseded, because `turn/start` does not report the setting it used.
An older delayed success cannot clear an observation a newer generation already owns. A patch axis
left at `keep` retains the last host selection, or the user's Codex defaults if there has been no
selection.
Native reviews inherit the same current settings. Hosts use the shared `Session` trait and need
no Codex-specific permission state machine.

An explicit opening effort uses `config.model_reasoning_effort` on `thread/start` or
`thread/resume`, alongside any host MCP entries. The [pinned protocol][thread-protocol] declares
the request-scoped `config` field and the [official config reference][config-reference] names this
key. Opening and per-turn model/effort IDs must be nonempty, bounded and free of control
characters. Opaque model IDs remain valid without membership in a static catalog. The harness
reports an effort as accepted only after the app-server accepts the thread; any effort the
response reports stays separately observed.
An isolated 0.154.0 app-server probe returned `reasoningEffort: high` for a
`config.model_reasoning_effort: high` thread start and wrote no `config.toml`.

## Approvals

Two of the server's questions are approvals a person can answer:
`item/commandExecution/requestApproval` and `item/fileChange/requestApproval`. Options come from
the declared `CommandExecutionApprovalDecision` / `FileChangeApprovalDecision` enums:

| Vendor decision                 | Effect  | Scope     | Risk        | Writes a rule | Note                                         |
| ------------------------------- | ------- | --------- | ----------- | ------------- | -------------------------------------------- |
| `accept`                        | Allow   | `Once`    | unspecified | no            |                                              |
| `acceptForSession`              | Allow   | `Session` | unspecified | no            | Codex forgets it when the thread ends        |
| `decline`                       | Reject  | `Once`    | unspecified | no            | The turn goes on                             |
| `cancel`                        | `Other` | —         | destructive | no            | Stops the turn, which a reject must not mean |
| `acceptWithExecpolicyAmendment` | `Other` | —         | destructive | **yes**       | Only when the request proposed one           |
| `applyNetworkPolicyAmendment`   | `Other` | —         | destructive | **yes**       | Only when the request proposed one           |

Four facts rather than one word. The two amendments are the reason: each one writes a policy Codex
applies to later requests on its own, and the old vocabulary had no way to say that — `Other` said
"only a person can weigh this" and nothing about what agreeing would leave behind. Their scope is
left **unstated**, because Codex does not say how far an amendment reaches, and an unstated reach is
never read as the narrow one.

The running server also writes an `availableDecisions` member that **its own generated schema does
not declare**. It is deliberately not read: building the option set a person chooses from out of an
undeclared field would make every prompt depend on a member that can change without a schema
change. A test asserts the member is still undeclared, so the day it is documented is the day this
harness can start reading it on purpose.

Nothing is auto-answered. Without a `PermissionBroker` every question reaches the host as an
`ApprovalRequested` event; with one, the event is still emitted and the broker's answer is
attributed to `DecisionSource::AutoReview`.

A question has the host's `Limits::approval_timeout` deadline, because the app-server sets none
of its own and blocks until the client replies. Its `expires_at` is translated once into the core
`ApprovalDeadline`, so event backpressure cannot restart the timer and broker deliberation and a
host response share the same deadline; a late host choice is refused even if the expiry waiter has
not run yet. Publication is bounded and nonblocking, so an approval audit cannot delay the
app-server reply or restart its deadline. On expiry, on a cancel and on a close, every waiting
question is answered `decline` — never `cancel`, which would stop a turn a deadline has no business
stopping. `serverRequest/resolved` releases a question the server stopped waiting on, so the task
composing a reply does not outlive the question. The same notification after this client replied
confirms that answer and consumes its bounded acknowledgement; it cannot become an early marker
that spends capacity for a later approval. This is client-side timing only: it adds no app-server
method or wire field beyond the existing [documented surface][readme].

### Permissions

`item/permissions/requestApproval` is a third approval family: a permission profile the agent asks
to be granted, not a command or a file change. It drives through the same
`PermissionRequest`/`PermissionBroker` path as the two families above, with three options mirroring
the scopes the vendor's own `PermissionGrantScope` declares:

| Option id       | Effect | Scope     | Answers with                                    |
| --------------- | ------ | --------- | ----------------------------------------------- |
| `grant:turn`    | Allow  | `Turn`    | `{"permissions": <echoed>, "scope": "turn"}`    |
| `grant:session` | Allow  | `Session` | `{"permissions": <echoed>, "scope": "session"}` |
| `deny`          | Reject | `Once`    | `{"permissions": {}}`                           |

`permissions` always travels back exactly as the request carried it: this harness never
synthesises, widens or narrows a permission profile, and the only alternative to granting exactly
what was asked is granting nothing. It is also what the request's detail leads with, compactly
serialised and unchanged, ahead of the agent's own `reason` and `cwd`: a host or a broker offered
`grant:turn` is offered exactly this profile, and a detail is cut from the end, so a verbose reason
must not be able to push the authority being granted out of what is rendered. `strictAutoReview` is never set — it asks the vendor to change
how it reviews later requests on its own, a standing instruction this library has no basis to give.

### Questions

`item/tool/requestUserInput` is not an approval: answering it tells the agent something and
authorises nothing, so it drives through `QuestionRequest`/`Session::answer` and a
`PermissionBroker` is never consulted about one. The pinned build's own schema marks this surface
**EXPERIMENTAL**.

| Wire field                      | Core shape                                                  |
| ------------------------------- | ----------------------------------------------------------- |
| `id`                            | `QuestionId`                                                |
| `question`                      | prompt                                                      |
| `header`                        | detail                                                      |
| `isBlocking` (round-level)      | `required`, on every question in the round                  |
| `options` non-empty             | `QuestionForm::Choice`, each option's own `label` as its id |
| `options` absent, null or empty | `QuestionForm::FreeText`                                    |

`isOther` is not offered: the neutral question contract has no arm for "one of these, or write your
own", so a round that sets it is presented as its declared choices only. A round where any question
sets `isSecret` is refused whole and natively — the wire answer is `{"answers": {}}`, the event is
`QuestionResolved` with `UnsupportedQuestion::SecretCollection` — and no part of it, blocking or
not, ever reaches the host: a password typed into a box labelled "answer" is a password in a
host's transcript. `autoResolutionMs` is declared but documented as deprecated and not read; the
deadline is the same `Limits::approval_timeout` every approval shares. An unanswered round
resolves exactly once, at that deadline, at a cancelled turn or session, or at a validated answer
from `Session::answer` — whichever is first.

### MCP elicitations

`mcpServer/elicitation/request` asks the client to fill in an arbitrary JSON-schema form on an MCP
server's behalf. It is answered ahead of the turn-correlation gates every other server request
passes through: at this pin the `url` branch carries no `turnId` at all, and a server may write the
member as `null`, so correlating first would answer the one family whose whole point is not
receiving a JSON-RPC error with exactly that error. An elicitation that names no turn, names
another turn, or arrives outside one is still declined on the wire; only one that belongs to the
active turn is recorded as a `QuestionResolved` on it. This library renders no form — see `UnsupportedQuestion::ArbitraryForm` and the
scope note in `docs/contracts.md` — so it answers `{"action": "decline"}` immediately and natively,
never `cancel`: the vendor's own documentation is that `decline` lets the turn continue while
`cancel` ends it, and refusing to render a form this library does not own is not a reason to end
somebody's turn. None of the request's own fields (`message`, `requestedSchema`, `content`,
`serverName`, `url`) are ever deserialised, so none of them can reach a host-visible event or a
log. A `QuestionResolved` event carrying `UnsupportedQuestion::ArbitraryForm` records that the
vendor asked and this library said no; no `QuestionAsked` is ever emitted for one, because a form
is never put to a host.

Every other server-initiated request is refused with a JSON-RPC error rather than left hanging:

| Request                                                                     | Refusal                                                 |
| --------------------------------------------------------------------------- | ------------------------------------------------------- |
| `item/tool/call`                                                            | Vendor tools never enter the host's tool registry       |
| `account/chatgptAuthTokens/refresh`                                         | This library reads, stores and forwards no vendor token |
| `attestation/generate`, the v1 `execCommandApproval` / `applyPatchApproval` | No answer in the neutral approval contract              |

## Auth, without reading a credential

`account/read` answers with the kind of account and, for a ChatGPT sign-in, a plan name. That is
all this harness reads — `~/.codex/auth.json` is never opened, the email the same call returns is
not modelled, and there is no login method anywhere.

| `account/read`                      | `AuthState`                               |
| ----------------------------------- | ----------------------------------------- |
| `{"type":"chatgpt"}`                | `LoggedIn { Subscription }`               |
| `{"type":"apiKey"}`                 | `LoggedIn { ApiKey }`                     |
| `{"type":"amazonBedrock"}`          | `LoggedIn { Other("amazon-bedrock") }`    |
| `null`, `requiresOpenaiAuth: true`  | `LoggedOut { login_hint: "codex login" }` |
| `null`, `requiresOpenaiAuth: false` | `Unknown`                                 |

The last row matters: a build pointed at a provider of its own is not signed out of anything that
would stop a turn, and telling the user to run `codex login` would send them to fix something that
is not broken. Opening a session against a signed-out CLI fails with `Error::AuthRequired` carrying
the vendor's own command as text. The library never runs it.

## Transports

`stdio` only. The app-server also offers `--listen ws://IP:PORT` and a unix socket, and its README
says of the first: "Websocket transport is currently experimental and unsupported. Do not rely on
it for production workloads." Declaring it would invite hosts to build on a surface the vendor has
already withdrawn once. The unix socket is the later opportunity; an undeclared transport is
refused as `Error::UnsupportedTransport` before anything is spawned.

## MCP

`Capabilities::mcp_passthrough` is `true`. Host entries travel in the `config.mcp_servers` map
on `thread/start` or `thread/resume`, the app-server's documented per-thread override. Stdio
entries preserve command, arguments and server-only environment. Streamable HTTP entries preserve
URL and literal headers. The harness validates header names and values with the `http` crate before
launching Codex. Invalid, duplicate or unsupported entries fail before launching Codex;
the harness never edits the user's `config.toml` or adds server credentials to the Codex child's
environment. Servers the user configured with `codex mcp` remain available to the vendor.

The `config` field is declared on both requests by the [pinned protocol][thread-protocol], and
the field names follow the [official config reference][config-reference]. A 0.154.0 app-server
probe with an isolated `CODEX_HOME` accepted a stdio `config.mcp_servers` override at
`thread/start` and wrote no persistent `config.toml`. Fake app-server tests cover both start and
resume mapping, header and environment separation, and pre-spawn refusals. MCP tool calls still
render as `ActivityKind::Mcp`.

## Environment

One variable survives the allowlist: `CODEX_HOME`, the directory the user's own CLI keeps its
configuration in. A child that could not find it would run under a configuration the user never
chose. `OPENAI_API_KEY` is deliberately absent — this library does no API-key plumbing of any kind.

## Why the protocol types are hand-written

The plan for this crate was to vendor `codex-rs/app-server-protocol` and `codex-rs/protocol`,
because crates.io refuses git dependencies and the crates.io packages of those names are a
third-party mirror. Measuring the closure settled it the other way:

|                               |         |
| ----------------------------- | ------- |
| In-repo crates in the closure | 32      |
| Rust files                    | 421     |
| Lines of Rust                 | 177,655 |
| Third-party crates named      | 107     |

Among those 107: **`native-tls`**, which `deny.toml` bans and `scripts/check-tls.sh` exists to keep
out; **`opentelemetry`**, in a library that promises no telemetry; plus `sqlx`, `libsqlite3-sys`,
`starlark`, `tree-sitter`, `keyring`, `landlock`, `seccompiler`, `portable-pty`, `reqwest` and
`gix`. Copying that tree in to obtain a few dozen wire structs would import a TLS stack this
workspace refuses and a dependency surface no MSRV lane could hold.

What is vendored instead is OpenAI's own description of the wire.
`codex app-server generate-json-schema` is a documented subcommand; `scripts/vendor-codex.sh` runs
it against the pinned build and reduces the bundle to field names, enum values and JSON-RPC
method discriminators in `vendor/schema.json`. The tests in
`src/protocol/schema.rs` assert every name this crate writes or reads against it, so a field
renamed upstream fails a test here rather than a turn on a user's machine.

Re-pinning is `scripts/vendor-codex.sh <version>`, which refuses to run against a `codex` whose
version is not the one being pinned. It needs the vendor's CLI, so it is a maintainer's command
rather than a CI lane; the pin test (`PIN == MINIMUM_CODEX_VERSION`) runs everywhere.

What the inventory deliberately does **not** catch is a field *added* upstream. This harness reads
a subset on purpose, and every addition would otherwise be a red build.

## Fixtures

Bare `mea discover` probes every linked harness. Select Codex explicitly for its detailed report;
`mea turn` defaults to Claude when `--harness` is absent:

```sh
cargo run -p mea -- discover --harness codex
cargo run -p mea -- turn --harness codex "summarise this repository"
cargo run -p mea -- capture --harness codex       # public schema contract
cargo run -p mea -- capture codex                 # archival authenticated transcripts
```

`mea capture --harness codex` records the public CLI version plus the generated initialization
schemas under `fixtures/codex/contract/`. It does not start an app-server session. The complete
schema inventory remains under `crates/mango-agent-codex/vendor/`, generated by
`scripts/vendor-codex.sh`.

`fixtures/codex/` also holds archival conversations recorded by `mea capture codex` against a real
`codex app-server`: `handshake`, `turn`, `approval`, `interrupt`, `review`, three additional review
targets, `permission-transitions` and `user-defaults`. The redaction happens in the capture
(`examples/mea/src/redact.rs`). Incoming frames retain the wire shape, enum values and opaque
identifiers that replay needs, while every other vendor string becomes `[REDACTED]`. Fixtures are
protocol evidence, not a record of a model answer, command, tool result, review or reasoning.

The approval fixture records a **refusal**. A fixture that captured a grant would be a recording of
this tool letting an agent out of its sandbox, checked into the repository.

## Structured content

What an item reports reaches a host as structure rather than as one more line of `detail`:

| Item                                | `ActivityContent`                                                                  |
| ----------------------------------- | ---------------------------------------------------------------------------------- |
| `fileChange.changes[]`              | `Diff { files }`, one `FileChange` per change, `diff` as the unified diff          |
| `commandExecution.aggregatedOutput` | `Output { text }`, beside the detail that also carries the exit code               |
| `plan.text`                         | nothing — freeform at this pin, and splitting it into steps would invent structure |

`FileChange::kind` stays **absent**: the pinned schema states no per-file kind, and reading one off
the diff text would be the re-parsing the type exists to prevent. Every activity carries the item's
own id in `item_id`. A subagent's thread id is not carried either — the vendored inventory is a
flattened property union across all nineteen item families, so it cannot say which family owns
`agentThreadId`, and no captured frame carries it.

## Known gaps

- No websocket or unix-socket transport (see above).
- A subagent item's thread id is unmappable: see above.
- Claude-style plan structure has no counterpart; `plan` is freeform text at this pin.
- `PermissionLevel` maps to the three plain `AskForApproval` values; the vendor's `granular`
  variant is neither sent nor modelled.
- `thread/fork`, thread archival, the queue and the realtime families are not driven.
- Vendor steering refusals other than the observed "no active turn" response retain the vendor
  error; native reviews are rejected locally as `TurnNotSteerable`.

Compliance posture: see [compliance.md](compliance.md).

[readme]: https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server/README.md
[resume-error]: https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server/src/request_processors/thread_processor.rs
[thread-protocol]: https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server-protocol/src/protocol/v2/thread.rs
[config-reference]: https://developers.openai.com/codex/config-reference
[app-server]: https://learn.chatgpt.com/docs/app-server
