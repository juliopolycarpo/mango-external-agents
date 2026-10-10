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
| `permissionProfile/list`        | `Discovery::permission_matrix`         |
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
second timestamps pass through when supplied. Every listing states its filters rather than
inheriting the server's defaults, which are creation order and interactive sources only
([`thread/list`][app-server]): `sortKey: "recency_at"`, `sortDirection: "desc"`,
`sourceKinds: ["cli", "exec", "appServer"]` and `archived: false`. `vscode` is left out because an
editor-owned thread has a live owner the host cannot see, and the subagent kinds are Codex's own
machinery. A row's timestamp is its `recencyAt`, falling back to `updatedAt` for a build that
leaves it null. Rows with missing or foreign workspace paths are
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
`item/commandExecution/outputDelta`, `item/mcpToolCall/progress`, `item/fileChange/patchUpdated`,
`thread/tokenUsage/updated`, `account/rateLimits/updated`, `serverRequest/resolved`, `error`.
Everything else is dropped by name.

**A failed turn keeps the vendor's classification.** The app-server documents failures as an
`error` notification with `{ error: { message, codexErrorInfo?, additionalDetails? } }` followed by
`turn/completed` with status `failed` ([app-server][app-server]). The `codexErrorInfo` label — a
string such as `usageLimitExceeded`, or the one key of an object such as `httpConnectionFailed` —
becomes `VendorError::vendor_code` on the turn's failure, taken from the completion's own error or,
when that names none, from the last report the server did not mean to retry. The harness code stays
`codex-turn-failed`.

**`turn/completed` is the only terminal.** The `error` notification reads like an ending and is
not one — it carries `willRetry`, and the turn's own completion still follows. Ending a turn there
would end the host's turn twice.

**Every terminal closes what it leaves open.** The app-server documents `item/started` and
`item/completed` as the lifecycle of a thread item and `turn/interrupt` as ending the turn with
status `interrupted` ([app-server][app-server]); it does not promise a completion for an item that
was still running when the turn was interrupted, failed or torn down. A host that only saw the
opening would keep an activity spinning and a reasoning block open, so the session tracks what it
has published and, in the same step that claims the terminal, emits one `ActivityCompleted` per
open call, in start order, and one `ReasoningEnded` per open phase, and then the terminal. The
status is `cancelled` for an interrupt, a host shutdown, and a turn the vendor reported complete
with items unreported, and `failed` for a failed turn, a malformed terminal, a lost connection
and a poisoned session. The claim is taken once, so a late second terminal closes nothing. A turn
with nothing open emits nothing extra.

The closes go through `EventSink::emit_close`. They are payload, so a turn whose payload budget
is spent has no room for them; the sink refuses such a close without committing anything, the
first refusal stops the closes, and the terminal the turn was ending with is committed as usual. A
full queue therefore costs the host the closes it had no room for and never the cancellation,
completion or failure. A stream that already committed a terminal, an overflow for instance,
refuses every close, so nothing follows it. Nothing waits for room.

**An event the core refuses fails the turn.** Every vendor id is untrusted: an activity id that is
blank, longer than the bound, or containing characters the core refuses (control and bidirectional
formatting characters) is refused rather than shortened or repaired, because a changed id would
name a different object. Dropping that event in silence would leave a host rendering a turn that is
missing an activity, so the turn closes what it holds open as failed and ends with the code
`codex-refused-event`, and the native turn is interrupted like any other abandoned one. What the turn was still asking is settled before the failure, as before an ordinary terminal, and a steer is rejected as already completed from the moment the failure is on the stream. Ids the
app-server sends today are UUIDs, so this is a guard against a malformed or changed vendor stream,
not something seen in the captured transcripts.

**Conversation events and approvals retain thread and native-turn ownership.** Delayed events
cannot finish a replacement turn. Until the `turn/start` answer names the native turn, only a frame on this session's own thread can
claim the pending id: a subagent's traffic on the shared connection is ignored there, so it cannot
be announced as the turn, cannot drop the session's own early output, and cannot become the target
of a cancel. Reviews explicitly account for the early `turn/started` id
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
own refusal (`-32600 "no active turn to steer"`) maps to `SteerRejection::TurnAlreadyCompleted`,
and its structured `activeTurnNotSteerable` refusal (`-32600 "cannot steer a review turn"` or
`"cannot steer a compact turn"`) maps to `SteerRejection::TurnNotSteerable`. The JSON-RPC client
does not retain the error's `data`, so that refusal is recognised by code and message.

The app-server answers a steer with "the accepted `turnId`" ([app-server][app-server]). When that
differs from the id the steer expected, Codex continues the turn under it, and the session adopts
it: later frames are matched against it, the interrupt names it, and the next steer's
`expectedTurnId` carries it. A host keeps steering with the native id it was given at
`TurnStarted`; the session accepts that id for the whole turn, and the last few superseded ones,
for the same attempt. The steer's answer and the continuation's first frames can arrive back to
back on separate workers, so notifications arriving while a steer is in flight are held in order
and replayed once its answer is handled; none is routed against the id the steer replaced. The
replay runs in its own task, so a host that drops its steer future mid-way does not strand the
queue. Held frames are bounded by count and by `Limits::turn_buffer_bytes`, and exceeding either
fails the session; a held frame for this conversation still restarts the idle deadline.
Steers are serialized per session, so a second steer issued while the first is in flight reads the
id the first one left behind. The JSON-RPC client keeps reading while an approval is pending, so a
steer sent then is answered without waiting for the approval.

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
sends `turn/interrupt`, waits for the turn to settle, and closes and reaps the app-server if it
will not. A browser disconnect that should leave work running must therefore retain the stream in
a host supervisor.

Teardown has one owner per session. The host-shutdown and connection-loss watcher, `close`, an
abandoned turn that will not settle, and a dropped session all request the same detached worker,
which closes the JSON-RPC client once and stops the child once. A connection that ends, like a
poisoned session, fails and releases the turn whose terminal it claimed before it wakes that
worker: closing the client stops the task that reports the connection's end, so waking the worker
first could leave the turn claimed with no terminal for the host. The wake itself is not lost when
that task is stopped or unwinds, as it does if the host's process control panics under it. For the
same reason the worker, whoever requested it, gives a terminal that is being committed up to
`shutdown_timeout` to land before it closes the client. A committer that is gone cannot land it,
so when that wait expires the worker fails the turn itself, with the vendor code
`terminal-abandoned` and not retryable: the turn's real outcome is unknown. A connection's end
that finds a poisoned session already ending the turn leaves both the turn and the wake to it. A
teardown acts for the reason a host's `close` gave, even when a stop worker asked for it first. A session dropped on a thread
without a Tokio runtime hands that worker to the runtime it was opened on. Every `close` waits for
the worker's result, so none reports success before the child is reaped. If the worker cannot reap
the child, or panics inside the host's process control, each caller receives the same
`Error::CleanupRequired` control for host reconciliation. `Closed` is published only after the
reap succeeds.

Stopping a turn has four deadlines, and none of them borrows another's meaning. The
[app-server protocol](https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md)
answers `turn/interrupt` with an empty result and later ends the turn with `turn/completed` status
`interrupted`; neither step has a vendor deadline, so each is bounded by the host's `Limits`:

| Stage                        | Bound                                                            | On expiry                                                                        |
| ---------------------------- | ---------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| Pending start                | `request_timeout`, from the start request or from the stop       | Start treated as unanswered; the settle stage begins                             |
| Interrupt ack                | The `turn/interrupt` request's own `request_timeout`             | Process shutdown, as for a refused interrupt                                     |
| Interrupt error during steer | `request_timeout`, from the interrupt's failure                  | The error is judged as it stands: process shutdown if the turn is still admitted |
| Turn settle after ack        | `cancel_settle_timeout` (60 s default), from the acknowledgement | Process shutdown                                                                 |
| Shutdown escalation          | `kill_grace`, then `shutdown_timeout` per teardown stage         | `Error::CleanupRequired` carrying the host's process control                     |

The pending-start bound has two clocks. The start request's own deadline returns
`Dispatch::AcceptanceUnknown` to its caller. The stop worker also bounds its own wait by
`request_timeout`, counted from when the stop began. That second clock matters when a host keeps
the `start_turn` future alive but no longer polls it: the request's own deadline can then never
fire.

A start future dropped before any byte of its `turn/start` reached the link releases the slot at
once, with no settle wait and no kill: Codex never saw the request, so no native turn can exist.

The stages add up. With `rt` for `request_timeout`, `st` for `shutdown_timeout` and `kg` for
`kill_grace`, the worst cases are:

| Path                       | Worst case                                       | At the defaults |
| -------------------------- | ------------------------------------------------ | --------------- |
| Teardown `T`               | `kg` + `st` + `st` + (`kg` + `kg` + `st` + `st`) | 26 s            |
| Cancel, interrupt accepted | `rt` + `rt` + `cancel_settle_timeout` + `T`      | about 5.5 min   |
| Cancel, interrupt failed   | `rt` + `rt` + `rt` + `st` + `T`                  | about 6.5 min   |

The teardown's terms are, in order: the discarded protocol interrupt, the wait for a terminal
another task claimed (only when there is one), the client's close, and the process stop (a
graceful interrupt and its wait, then the kill and its wait). A cancel's first `rt` is the pending
start and its second the interrupt. In the failed case the third `rt` is the steer hold, paid only
while a `turn/steer` is in flight, and the `st` is the wait for a closing connection to fail the
turn, paid only when the connection is ending; a refused interrupt on a healthy connection with no
steer goes straight to `T`. A start whose answer was lost but that a notification later names pays one
more interrupt and settle round; if that late interrupt fails, its steer and connection waits come
on top, outside `cancel_settle_timeout`. For that whole time the session stays busy. A `cancel` call waits
only when the turn is already named: then it covers the interrupt, the settle deadline and any
shutdown. A cancel that arrives before the start answer returns once the stop is recorded. `close`
does not wait for this: it goes straight to process shutdown, bounded by `kill_grace` and
`shutdown_timeout` alone.

Each owner has one stop worker, and it sends at most one `turn/interrupt`: repeated cancels, a
dropped stream, an idle expiry and a racing `close` all share it. Until the turn ends the slot stays
occupied and `start_turn` answers `Busy`, because Codex reads a `turn/start` on a live turn as a
steer. An expired deadline is not treated as proof the turn stopped. The harness stops waiting and
reaps the process, and the session is `Closed` only once that reap succeeds. A cancel that
arrives before `turn/start` answers returns once the stop is recorded, and the worker sends the
interrupt when the answer names the turn. The turn's terminal arrives on the start caller's
stream. If escalation cannot reap the child, `close` returns the resulting `CleanupRequired` and
its control, so nothing is lost by not waiting. A cancel on a named turn waits for the worker and
returns its error whole, including a `CleanupRequired`.

The stop worker reads the `turn/interrupt` answer in order. The JSON-RPC client settles a response
as soon as it reads it and runs notifications on a separate task (`docs/lifecycle.md`), so an
answer can reach its caller before a notification the app-server wrote ahead of it. When a turn
ends on its own while the interrupt is in flight, an error answer written after that turn's
`turn/completed` used to find the turn still admitted: the cancel returned the error and the stop
escalated to process shutdown, for a turn that had completed. The worker now sends the interrupt
with `RequestOptions::after_earlier_notifications`, so the answer is judged only once the handler
has returned from everything read before it. A turn that completed first makes the cancel succeed,
and a refusal for a turn that is still running is reported as before. This is client-side timing
only: the method, its parameters and the bytes on the wire are unchanged.

It is the only ordered request in this harness, and three things follow from it:

- The interrupt's `request_timeout` keeps running while its answer waits behind the handler. The
  handler never waits for the host: publication is nonblocking, and an overflow poisons the
  session. A host that stopped reading its stream therefore cannot delay the answer. An answer
  that does wait out the deadline is the "Interrupt ack" expiry above: the cancel returns the
  timeout after the process shutdown.
- When the app-server exits or the link fails, the unanswered interrupt fails after the frames
  already read have been handled, within `shutdown_timeout`. A `turn/completed` read before the
  exit therefore still makes the cancel succeed. With no completion read, or when the dead
  process's pipe refused the interrupt outright, the stop waits up to one more `shutdown_timeout`
  for the connection's end to fail the turn before it judges the interrupt. The stream then ends
  with the connection failure, and the cancel reports the teardown's result, not the interrupt's
  error: it returns once the child is reaped, or with that teardown's `CleanupRequired`. Before,
  the host saw either that or a `Cancelled` terminal with the interrupt's error, depending on
  task order. A session poisoned while a cancel waits is treated the same way: the stop joins
  the teardown the poison leads to, so the cancel returns once the child is reaped.
- Two interrupts stay unordered, because their answer is discarded and the caller is already
  ending the turn itself: a poisoned session's, which can be sent from inside the notification
  handler, where the client refuses an ordered request before writing it, and the teardown
  worker's, which must not wait on a handler it is about to stop.

A steer needs one more wait. While a `turn/steer` is in flight, and until the frames it held have
been replayed, notifications are held (see above) and routed later on the replay's own task, so
the handler returns from a held `turn/completed` without having routed it. A stop whose interrupt
fails, whether refused, unacknowledged or cut off, therefore waits for the hold to empty before it
judges the error. The wait has its own bound, `request_timeout`, because the hold has none this
side can rely on: a host can keep a steer future alive without polling it. A failed interrupt can
thus wait behind a slow steer answer, for that long and no longer. The wait also ends as soon as
the turn is gone, since a connection's end or a poisoned session ends the turn without going
through the hold. An accepted interrupt does not wait.

Malformed terminal frames fail their addressed turn; an unrouteable terminal closes the session.
Connection loss and host shutdown terminate active streams, release approvals and reap the process.
Native activity restarts the host's `Limits::idle_timeout`, including the streamed progress of a
running item: between a command's `item/started` and `item/completed` the app-server writes only
`item/commandExecution/outputDelta` for it, so a build that prints for longer than the deadline is
still a working turn. That activity must be this turn's own: the connection
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

`model/list` and `permissionProfile/list` are cursor-paginated with a server-chosen page size
([app-server][app-server]). The probe follows `nextCursor` for up to eight pages each, and the
model catalog also stops at the core's 256-model cap, counting only models the picker shows
(hidden ones are dropped as they are read). A page that fails mid-walk keeps the models already
read. A profile listing that still has a cursor after eight pages is incomplete, and is treated
like one that failed: the declared matrix stays.

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

Discovery narrows the matrix to what the machine allows. The app-server documents
`permissionProfile/list` "with the project cwd to discover available profiles and whether managed
requirements allow each one" ([app-server][app-server]); the probe asks it for the host's working
directory and reads the built-in profile each level selects — `:read-only`, `:workspace` and
`:danger-full-access`. A level whose profile is reported `allowed: false`, or is not listed at all,
is unsupported under both routings with `UnsupportedReason::Other(PROFILE_DISALLOWED)`, the
constant `mango_agent_codex::permissions::PROFILE_DISALLOWED`, so a host can say a policy refused
it. A build that does not answer the call keeps the declared matrix: not being able to ask is not a
refusal. A cell narrowed only at open time still fails at `thread/start`, as the core's discovery
contract describes.

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

| Vendor decision                 | Effect  | Scope     | Risk        | Writes a rule | Note                                                     |
| ------------------------------- | ------- | --------- | ----------- | ------------- | -------------------------------------------------------- |
| `accept`                        | Allow   | `Once`    | unspecified | no            |                                                          |
| `acceptForSession`              | Allow   | `Session` | unspecified | no            | Codex forgets it when the thread ends                    |
| `decline`                       | Reject  | `Once`    | unspecified | no            | The turn goes on                                         |
| `cancel`                        | `Other` | —         | destructive | no            | Stops the turn, which a reject must not mean             |
| `acceptWithExecpolicyAmendment` | `Other` | —         | destructive | **yes**       | Only when the request proposed one and it displays whole |
| `applyNetworkPolicyAmendment`   | `Other` | —         | destructive | **yes**       | Only when the request proposed one and it displays whole |

Four facts rather than one word. The two amendments are the reason: each one writes a policy Codex
applies to later requests on its own, and the old vocabulary had no way to say that — `Other` said
"only a person can weigh this" and nothing about what agreeing would leave behind. Their scope is
left **unstated**, because Codex does not say how far an amendment reaches, and an unstated reach is
never read as the narrow one.

### Standing rules an amendment writes

An amendment option writes a rule Codex applies on its own from then on, so the option's label
carries the rule itself and never a generic description of it. The pinned digest in
`vendor/schema.json` lists property names only, with no field types. The types come from
`codex app-server generate-json-schema` run at the pinned 0.154.0:
`CommandExecutionRequestApprovalParams.proposedExecpolicyAmendment` is `string[]` (a command
prefix), `proposedNetworkPolicyAmendments` is `NetworkPolicyAmendment[]` where
`NetworkPolicyAmendment` is `{host: string, action: "allow" | "deny"}`, and
`networkApprovalContext` is `{host: string, protocol: "http" | "https" | "socks5Tcp" |
"socks5Udp"}`. The reply shapes are the `acceptWithExecpolicyAmendment` and
`applyNetworkPolicyAmendment` arms of `CommandExecutionApprovalDecision`.

| Proposal                             | Label                                                             |
| ------------------------------------ | ----------------------------------------------------------------- |
| `["git","status"]`                   | `Allow, and always allow commands starting with ["git","status"]` |
| `{host: "a.example", action: allow}` | `Apply standing network rule: allow "a.example"`                  |
| `{host: "a.example", action: deny}`  | `Apply standing network rule: deny "a.example"`                   |

- **The prefix is compact JSON**, because a space-joined prefix reads the same for `["rm -rf","x"]`
  and `["rm","-rf","x"]`.
- **An option is offered only when its label survives the core's option-label bound**
  (`TextLimit::ApprovalOptionLabel`, 128 code points, with control and bidirectional characters
  stripped) unchanged, checked with `normalize::bound_text` on the final string. Cutting a label
  would show a person less than the rule they are writing, which is the concealment the label
  exists to prevent, so the option is dropped instead. The four plain options remain, and the
  dropped rule is spelled out in the detail (up to 256 code points; a longer one is described as
  too long to show) as a standing rule "proposed but not offered".
- **A proposal outside the declared shape is not offered**: an execpolicy amendment that is not a
  nonempty array of strings, or a network amendment without a nonempty `host` and an `action` of
  exactly `allow` or `deny`, or that carries any other member. A rule that cannot be labelled exactly cannot be consented to.
- **The action is what the wire proposes.** A `deny` proposal is labelled `deny`, never "Allow".
  What Codex does with the current request when a `deny` amendment is applied is **unverified**:
  the pinned description says only "User chose a persistent network policy rule (allow/deny) for
  this host", so the label says nothing about the request in front of the person. The reply
  carries exactly the `{action, host}` that was labelled.
- **`networkApprovalContext` leads the detail**, ahead of the agent's own `reason`, as
  `Network access requested: <protocol> to <host>`. A context of a shape the pin does not declare
  (an extra member, or a protocol outside the enum) is shown whole as it arrived.
- **The refusal is unchanged**: `decline`. Options stay `Other`, destructive and
  `policy_changing`, so a broker's `allow()` never selects one, and nothing is auto-selected.

**Only the first network proposal is a candidate.** The choice was made deliberately:

- Every network amendment answers with the same option id, `applyNetworkPolicyAmendment`, because
  `ApprovalDecisionValue::option_id` is a public `&'static str`. Offering several would need
  indexed ids, and the broker path resolves the chosen option by reading that id back off the
  decision, so a broker choosing the second would be audited as an unresolved option, losing its
  `policy_changing` and destructive flags.
- The core refuses a request of more than `APPROVAL_MAX_OPTIONS` (16) options, and the pin
  declares no bound on the number of proposals, so offering all of them lets a long proposal list
  stop a prompt from reaching the host at all.
- Nothing is lost silently: every later proposal is listed in the detail (four at most, then a
  count). A person who wants one of those rules can decline and write it in their own Codex
  configuration.

Offering every proposal remains possible once options can carry ids other than the vendor
decision's own.

**How often a prefix survives.** The label wording leaves 81 code points for the compact JSON of a
prefix, and 91 for a network host. A host name is short enough that a network option is dropped
only for a host longer than that. A prefix keeps its option when the JSON of the whole argv fits:
`["git","status"]` (16), `["cargo","test","--workspace"]` (32) and
`["curl","-s","https://api.example.com/v1/status"]` (48) all do. It loses it for a long argv, such
as a shell wrapper around a whole script or a `curl` with an inline payload, whose JSON exceeds 81.
There is no captured corpus of real proposals to count against: the archival capture in
`fixtures/codex/approval.jsonl` records one three-word prefix with its text redacted, and the
sample of commands available locally is too small to state a rate. What is asserted is the
boundary itself, by
`approvals::tests::realistic_prefixes_mostly_keep_their_option`. A prefix that loses its option is
not silently gone: the detail carries the rule and the four plain options still work.

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

### Debug output

Every interaction carrier prints metadata only, following the `Debug` policy in
`docs/compliance.md`: `PendingApproval`, `ServerRequest`, the approval, permissions, question and
elicitation params, and every answer type (`ServerAnswer`, `ApprovalDecisionValue`,
`ApprovalResponse`, `PermissionsRequestApprovalResponse` and the question-round answers). A
debug-logged command approval reads, abridged,
`CommandExecution(CommandExecutionApprovalParams { command: Some(<14 bytes redacted>), .. })`:
which members are present and how large they are, never the command, reason, working directory,
amendment prefix, host, permission profile or answer text. Vendor-minted ids are reported by
length, as core does for an `InteractionId`; the option ids this harness offers (`accept`,
`grant:turn`) and the method names it recognises print as themselves.

The records decoded from the app-server's notifications follow the same rule, because command
output, diffs and answer text arrive in them: `ThreadItem`, `FileUpdateChange`, `TurnError`, the
item, message-delta, reasoning-delta, command-output, MCP-progress and patch-update notifications,
and `TurnReducer`, which buffers streamed text and each open activity's output until the item
completes. They print the item family, status, counts and byte sizes. `Notification` itself holds
only those records and ids, so it keeps its derive. Token usage, rate limits, statuses and the
handshake hold numbers and labels and keep theirs, and so do `CodexHarness`, `CodexDiscovery` and
`VendorConfiguration`, which reach their paths and account only through core types whose `Debug`
is already metadata-only. The boundary is stated in `docs/compliance.md`.

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
`grant:turn` is offered exactly this profile. Following the [app-server permissions contract][readme],
the harness offers only `deny` if the profile and its `Grants` prefix cannot survive detail
normalization unchanged. This covers the 4,096-code-point limit and stripped display-control
characters. A long reason or working directory may still be truncated after the complete profile.
`strictAutoReview` is never set — it asks the vendor to change
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

A `serverRequest/resolved` withdrawal closes an announced question with `QuestionOutcome::Cancelled`
without sending another answer to the server. An answer already accepted by `Session::answer`
retains its outcome during shutdown and terminal cleanup. The pending round remains owned until
its resolution is published, so `Completed` cannot leave an accepted answer's prompt open.

#### When a question can actually arrive

The library declares `questions: true` and drives the whole round trip, but that says what it can
answer, not how often the vendor asks. What is known, read on 2026-09-30 against the pinned schema
(`0.154.0`) and a live `codex-cli 0.159.2`:

- `item/tool/requestUserInput` is in the **stable** schema of both builds, the one generated
  without `--experimental`, described as "EXPERIMENTAL". The label is a description, not a gate:
  the vendor's [app-server documentation][app-server] says some methods and fields "are
  intentionally gated behind `experimentalApi` capability" and lists `tool/requestUserInput`
  "(experimental)", yet this request stays in the stable schema. This harness sends no
  `capabilities` in `initialize`, so it never opts in to `experimentalApi`.
- The vendor's own feature list (`codex features list`, `0.159.2`, an empty `CODEX_HOME`) reports
  `default_mode_request_user_input` as "under development" and `false`. By its name it is the
  switch for questions in Default mode; this note claims nothing beyond its stage and state.
- `TurnStartParams.collaborationMode` is absent from the pinned inventory and, at `0.159.2`,
  appears only in the schema generated with `--experimental`, so the library has no supported way
  to select a collaboration mode such as Plan. This note cites no vendor sentence tying Plan mode
  to questions, so it does not claim that it would make one arrive.
- The one candidate setting found is that feature flag, and it is the user's to set. With
  `-c features.default_mode_request_user_input=true`, `codex features list` reports it `true`, and
  `codex features enable` writes it to the user's `config.toml`. That shows the flag can be turned
  on, not that turning it on makes the vendor raise `requestUserInput`; no vendor source or capture
  confirms that yet, which is what the capture task below is for. The library cannot set it. It
  starts `codex app-server` with no `-c` argument, edits no persistent Codex configuration, and its
  per-thread `config` override on `thread/start` and `thread/resume` carries only the host's MCP
  servers and the reasoning effort.

So with default settings the vendor probably never asks a question on a library turn. That has
**not** been observed end to end: no authenticated turn was run for this note, and the conformance
suite exercises the question path with a pushed synthetic request, not a captured one.

`questions` stays `true` on purpose. Reporting `false` would make conformance require `answer` to
refuse as unsupported, and a user who enabled the feature would then receive rounds the host cannot
answer.

**Capture task, open:** once a question is reachable (the feature flag enabled in the user's own
Codex configuration on a build that has it, or a supported way to select a collaboration mode),
record one round with `mea capture` and replay it as a fixture, so the question path is proven against
the vendor's own frames rather than a synthetic one. Re-check the facts above at each pin
refresh; if the vendor removes the label or turns the flag on by default, this note is out of date.

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

`account/read` answers with the kind of account and, for a ChatGPT sign-in, a plan name and an
email. `~/.codex/auth.json` is never opened, the email is never modelled, and there is no login
method anywhere.

**The account fingerprint.** A host that keeps a Codex continuation across restarts has to notice
when the signed-in account changed, and the email is the only non-secret account identity the
app-server reports ([app-server][app-server]: "`email` is null when the ChatGPT account doesn't have
an email address"). `CodexHarness::discover_with_account(host, key)` runs the ordinary probe and also
returns `CodexAccount { plan_type, fingerprint }`, where the fingerprint is
`hex(HMAC-SHA256(key, "codex:" + email))[..32]` — the value the TypeScript adapter stored, so a
migrating host keeps matching its continuations. The address is borrowed from the raw `account/read`
answer for that one digest and returned nowhere. A plain hash is refused on purpose: anyone holding it
could test a guessed address offline, so the key is required (`AccountFingerprintKey` refuses an
empty one) and never leaves the host. `Harness::discover`, which has no key, computes nothing. A
ChatGPT account without an email, an API-key account and a Bedrock account have no fingerprint.

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

## Account quota

`account/rateLimits/read` returns the account's full quota, and `account/rateLimits/updated` is
"emitted whenever a user's ChatGPT rate limits change" ([app-server][app-server]) and may carry only
what changed. The session keeps the last full reading as a baseline: `Session::refresh_account_usage`
sets it, and each update is merged onto it before the result reaches the running turn as
`AccountLimits`. A window or plan the update omits or sends as `null` keeps the baseline's value; a
present one overwrites it. An update that arrives before any baseline is not shown: the session asks
for one `account/rateLimits/read` in the background and reports its full answer instead. Every full
read — that one or a host's `refresh_account_usage` — has any update that arrived while it was in
flight merged over its answer, and a refresh returns that merged reading. Updates held for a read
that failed are dropped with it rather than laid over a later one. Reads are numbered as they are
sent, and a read that answers after a later one was adopted is older than the baseline and is
dropped rather than rewinding it.

Beyond the windows and the plan, a reading carries what the documented `account/rateLimits/read`
reports about the rest of the account's quota ([app-server][app-server]):

| Vendor field                                        | `AccountLimits`                                                                      |
| --------------------------------------------------- | ------------------------------------------------------------------------------------ |
| `rateLimits.credits`                                | `credits: Credits { has_credits, unlimited, balance }`                               |
| `rateLimits.individualLimit`, `spendControlReached` | `spend_control: SpendControl { limit, used, remaining_percent, resets_at, reached }` |
| `rateLimitResetCredits`                             | `reset_credits: ResetCredits { available_count, credits }`                           |

Absence stays absent: a `null` spend-control state is unavailable rather than recovered, and
`reset_credits.credits` is `None` when only the count is known. Credits and spend control merge
like the windows — a `null` credit balance or reached flag keeps the baseline's. Reset credits are
reported only by a full read, so a sparse update leaves the last read's in place.

## Transports

`stdio` only. The app-server also offers `--listen ws://IP:PORT` and a unix socket. OpenAI's
[app-server documentation][app-server] (read 2026-09-30) lists the first as "websocket (--listen
ws://IP:PORT, experimental and unsupported)" and says: "The app-server command and WebSocket
transport are experimental and aren’t supported for production workloads." Declaring it would
invite hosts to build on a surface the vendor does not support for production. The unix socket is
the later opportunity; an undeclared transport is refused as `Error::UnsupportedTransport` before
anything is spawned.

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

## Answer text

The answer streams as `item/agentMessage/delta` and arrives again, whole, on the `agentMessage`
item's `item/completed`. Deltas are emitted as they come; the completion adds only the text its
deltas did not deliver, so a message that was never streamed — as a resumed conversation can
deliver one — still reaches the host exactly once. When the completed text does not start with what
was streamed, the vendor rewrote the message and the whole text is emitted. The streamed text is
kept per message only up to what one stdio line can carry once decoded (three times
`Limits::line.max_line_bytes`, since repairing invalid UTF-8 can triple a line, and never more than
`max_buffered_bytes`): a completion cannot begin with more than that, so past
it the text is dropped and the completion is emitted whole, never refused. The documented item
shape is `agentMessage - {id, text, phase?} containing the accumulated agent reply`
([app-server][app-server]).

## Structured content

What an item reports reaches a host as structure rather than as one more line of `detail`:

| Item                                | `ActivityContent`                                                                  |
| ----------------------------------- | ---------------------------------------------------------------------------------- |
| `fileChange.changes[]`              | `Diff { files }`, one `FileChange` per change, `diff` as the unified diff          |
| `commandExecution.aggregatedOutput` | `Output { text }`, beside the detail that also carries the exit code               |
| `plan.text`                         | nothing — freeform at this pin, and splitting it into steps would invent structure |

While an item runs, its streamed progress updates the same activity through `ActivityUpdated`,
following the [item notifications][app-server] the app-server documents:

| Notification                        | `ActivityUpdate`                                             |
| ----------------------------------- | ------------------------------------------------------------ |
| `item/commandExecution/outputDelta` | `detail`: the most recent 2,000 characters of output         |
| `item/mcpToolCall/progress`         | `detail`: the most recent 2,000 characters of progress lines |
| `item/fileChange/patchUpdated`      | `detail`: the paths; `content`: `Diff` of the patch as it is |

An activity reports at most one update every five seconds — the first after a quiet window is
emitted at once, and what was held back rides the next one — and `truncated` says when the tail
dropped older output. The completed item still carries the whole `aggregatedOutput`. Progress for
an item this turn never announced as an activity, or already completed, is dropped rather than
addressed to a call id the host was never told about.

`FileChange::kind` stays **absent**: the pinned schema states no per-file kind, and reading one off
the diff text would be the re-parsing the type exists to prevent. Every activity carries the item's
own id in `item_id`. A subagent's thread id is not carried either — the vendored inventory is a
flattened property union across all nineteen item families, so it cannot say which family owns
`agentThreadId`, and no captured frame carries it.

An item family this build does not model — `imageView`, `dynamicToolCall`, `sleep`, or one a
newer Codex adds — is still work the agent did, so it renders as an `ActivityKind::Other` activity
named by the vendor's own `type`, bracketed by its `id`. Echoes of the client's own input
(`userMessage`, `hookPrompt`, `functionCallOutput`) render nothing, and an unknown item without an
id renders nothing because no completion could address it. The item families are listed in the
[app-server documentation][app-server].

## Known gaps

- No websocket or unix-socket transport (see above).
- A subagent item's thread id is unmappable: see above.
- Claude-style plan structure has no counterpart; `plan` is freeform text at this pin.
- `PermissionLevel` maps to the three plain `AskForApproval` values; the vendor's `granular`
  variant is neither sent nor modelled.
- `thread/fork`, thread archival, the queue and the realtime families are not driven.
- Vendor steering refusals other than "no active turn" and "cannot steer a review/compact turn"
  retain the vendor error; native reviews are rejected locally as `TurnNotSteerable`.

Compliance posture: see [compliance.md](compliance.md).

[readme]: https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server/README.md
[resume-error]: https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server/src/request_processors/thread_processor.rs
[thread-protocol]: https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server-protocol/src/protocol/v2/thread.rs
[config-reference]: https://developers.openai.com/codex/config-reference
[app-server]: https://learn.chatgpt.com/docs/app-server
