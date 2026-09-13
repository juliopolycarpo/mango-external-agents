# OpenAI Codex harness (`mango-agent-codex`)

Drives the `codex` CLI a user already installed, through `codex app-server` — the interface OpenAI
documents for rich clients and uses for its own VS Code extension.

Facts on this page were read on 2026-09-13 against `codex-cli 0.153.4`. Re-verify against the
vendor's current documentation before relying on them.

## The executable and the version gate

|                   |                                                                              |
| ----------------- | ---------------------------------------------------------------------------- |
| Executable        | `codex`, resolved by the host (`OpenSession::with_executable`) or by name    |
| Arguments         | `app-server`, and nothing else                                               |
| Version read from | `codex --version` for a probe; the handshake's own `userAgent` for a session |
| Minimum version   | `0.153.4` (`MINIMUM_CODEX_VERSION`), which is also `vendor/PIN`              |

The floor is the pinned build rather than something older. The app-server's item families and its
`thread/`–`turn/` method names changed shape inside the 0.15x series, so a lower floor would be a
claim about builds nothing here was checked against. An older `codex` on `PATH` reports
`GateVerdict::VersionTooOld` with both numbers and claims no capabilities; it never crashes, and no
app-server is spawned for it.

A `--version` line nobody can parse is `GateVerdict::Unknown`, not a refusal: a CLI that changed
the shape of its version output has not stopped working, and the host may still choose to try.

## The documented surface this harness drives

Every method below is from [`codex-rs/app-server/README.md`][readme] at `rust-v0.153.4`. JSON-RPC
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
| `thread/list`                   | `Session::list_sessions`               |
| `turn/start`                    | `Session::start_turn`                  |
| `turn/steer`                    | `Session::steer`                       |
| `turn/interrupt`                | `Session::cancel`                      |
| `review/start`                  | `Session::start_review`                |

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

**Every conversation-scoped notification is filtered by thread id.** A subagent's thread rides the
same connection, and replaying its events under this session's turn would attribute another
conversation's work to this one.

## Sessions, turns and steering

One long-lived `codex app-server` per session. `thread/start` opens a conversation;
`thread/resume` continues one, with `excludeTurns: true` — the vendor keeps the transcript it
wrote, and this library never replays one into anybody's context. `ResumeMode::Fallback` starts a
new thread and records why in `SessionInfo::fallback_reason`.

`turn/start` on a live turn is taken by the app-server as a **steer** — its own documentation says
`turnTrigger` is "ignored when this request steers an already-active turn". A host that meant a new
turn would hold a stream that never gets a `turn/completed` of its own, so a second turn is refused
before the call is made, as a retryable `codex-turn-already-running`.

`turn/steer` carries `expectedTurnId` as a precondition. A steer naming a turn that is not the one
running is refused here rather than landing on whatever turn happens to be live; the app-server's
own refusal (`-32600 "no active turn to steer"`) maps to `SteerRejection::TurnAlreadyCompleted`.

`review/start` is sent without `delivery`, which the server reads as inline: a detached review runs
on a thread this session is not subscribed to, and its events would arrive under an id the reducer
drops. `ReviewStream::review_thread_id` reports whatever the server named.

Attachments: image attachments travel as `UserInput::image` with a `data:` URL, verified against a
real `turn/start`. Other attachment kinds are dropped — `UserInput` has no general-purpose file arm,
and smuggling one in as text would send something different from what the host attached.

## The permission matrix

All six (level, routing) pairs are supported. A level is two vendor settings that move together,
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

## Approvals

Two of the server's questions are approvals a person can answer:
`item/commandExecution/requestApproval` and `item/fileChange/requestApproval`. Options come from
the declared `CommandExecutionApprovalDecision` / `FileChangeApprovalDecision` enums:

| Vendor decision                 | Neutral kind         | Note                                             |
| ------------------------------- | -------------------- | ------------------------------------------------ |
| `accept`                        | `AllowOnce`          |                                                  |
| `acceptForSession`              | `AllowAlways`        |                                                  |
| `decline`                       | `RejectOnce`         | The turn goes on                                 |
| `cancel`                        | `Other`, destructive | Stops the turn, which `RejectOnce` must not mean |
| `acceptWithExecpolicyAmendment` | `Other`, destructive | Only when the request proposed one               |
| `applyNetworkPolicyAmendment`   | `Other`, destructive | Only when the request proposed one               |

The running server also writes an `availableDecisions` member that **its own generated schema does
not declare**. It is deliberately not read: building the option set a person chooses from out of an
undeclared field would make every prompt depend on a member that can change without a schema
change. A test asserts the member is still undeclared, so the day it is documented is the day this
harness can start reading it on purpose.

Nothing is auto-answered. Without a `PermissionBroker` every question reaches the host as an
`ApprovalRequested` event; with one, the event is still emitted and the broker's answer is
attributed to `DecisionSource::AutoReview`.

A question has a 30-minute deadline (`approvals::APPROVAL_TIMEOUT`), because the app-server sets
none of its own and blocks until the client replies. On expiry, on a cancel and on a close, every
waiting question is answered `decline` — never `cancel`, which would stop a turn a deadline has no
business stopping. `serverRequest/resolved` releases a question the server stopped waiting on, so
the task composing a reply does not outlive the question.

Every other server-initiated request is refused with a JSON-RPC error rather than left hanging:

| Request                                                                                                                                                                        | Refusal                                                 |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ------------------------------------------------------- |
| `item/tool/call`                                                                                                                                                               | Vendor tools never enter the host's tool registry       |
| `account/chatgptAuthTokens/refresh`                                                                                                                                            | This library reads, stores and forwards no vendor token |
| `item/tool/requestUserInput`, `mcpServer/elicitation/request`, `item/permissions/requestApproval`, `attestation/generate`, the v1 `execCommandApproval` / `applyPatchApproval` | No answer in the neutral approval contract              |

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

`Capabilities::mcp_passthrough` is `false`. MCP servers are configured in the user's own
`~/.codex/config.toml` or with `codex mcp`; there is no app-server call that takes a server
definition from a client. Servers the user configured still run, and their calls are rendered as
`ActivityKind::Mcp` — what a host cannot do is pass one through.

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
it against the pinned build and reduces the 622-definition, 583 KB bundle to the field names and
enum values of the types this harness speaks — 12 KB in `vendor/schema.json`. The tests in
`src/protocol/schema.rs` assert every name this crate writes or reads against it, so a field
renamed upstream fails a test here rather than a turn on a user's machine.

Re-pinning is `scripts/vendor-codex.sh <version>`, which refuses to run against a `codex` whose
version is not the one being pinned. It needs the vendor's CLI, so it is a maintainer's command
rather than a CI lane; the pin test (`PIN == MINIMUM_CODEX_VERSION`) runs everywhere.

What the inventory deliberately does **not** catch is a field *added* upstream. This harness reads
a subset on purpose, and every addition would otherwise be a red build.

## Fixtures

`fixtures/codex/` holds four conversations recorded by `mea capture codex` against a real
`codex app-server`: `handshake`, `turn`, `approval` and `interrupt`. They are never hand-edited —
the redaction happens in the capture (`examples/mea/src/redact.rs`), which replaces the members
that identify a person or a machine and rewrites the capture's own two directories.

The approval fixture records a **refusal**. A fixture that captured a grant would be a recording of
this tool letting an agent out of its sandbox, checked into the repository.

## Known gaps

- No websocket or unix-socket transport (see above).
- No MCP passthrough (see above).
- `PermissionLevel` maps to the three plain `AskForApproval` values; the vendor's `granular`
  variant is neither sent nor modelled.
- `thread/fork`, thread archival, the queue and the realtime families are not driven.
- `SteerRejection::TurnNotSteerable` is never produced. The app-server's refusal for a turn that
  refuses steering — a review, a compaction — has not been observed, so a steer it declines for any
  reason other than "no active turn" surfaces as the vendor error rather than as that reason.
- A session closed or cancelled while its host has stopped reading ends that turn with a bare
  `Completed` under a 200 ms grace, rather than with the cancellation marker a turn that ends
  normally carries. One event is what fits: a bounded sink offers no way to put two events on or
  neither, and a grace that elapsed between a marker and its terminal would leave the host the one
  shape the core's contract rules out. The reason is not lost with the marker — it is the argument
  the host passed to `close` or `cancel` in the first place.

Compliance posture: see [compliance.md](compliance.md).

[readme]: https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/app-server/README.md
