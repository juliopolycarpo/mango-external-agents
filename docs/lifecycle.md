# Turn ownership and recovery

A session admits one native turn at a time. A concurrent start returns `Error::Busy` with
`Dispatch::NotSubmitted`; it never replaces, queues or steers the current turn. A stopping attempt
continues to own admission until native work can no longer conflict and its terminal is committed.
The host does not have to drain the old transcript before starting another turn.

Every callback belongs to an attempt. The harness compares that owner before publishing session
state, resolving interactions or clearing admission. Reusing a host turn ID does not give an older
callback authority over a newer attempt.

## Owners and cancellation

The host supervisor owns the session and `TurnStream`. Browser subscribers attach to that
supervisor; disconnecting a browser leaves the library operation owned and observable. Dropping a
start future, active stream or session abandons that ownership and triggers cleanup. Cleanup that
has already started survives cancellation of its caller while the host runtime remains alive.
The host must keep its runtime alive until shutdown completes and supply a launcher that contains
and reaps its process trees.

If bounded cleanup for a pre-session operation or a session close fails, the operation returns
`Error::CleanupRequired`. Its typed source describes the cleanup failure and
`Error::cleanup_control()` returns the host's `ProcessControl` handle. That handle may already
have received `kill`; the host owns reconciliation and any bounded retry, including reaping the
tree. The library does not retain a global cleanup registry or promise cleanup after the host
runtime ends.

These are different observations:

| Observation                       | What it proves                                                                                                    |
| --------------------------------- | ----------------------------------------------------------------------------------------------------------------- |
| `cancel` request                  | The owner requested native cancellation. Protocol acknowledgement may still be pending.                           |
| Native terminal                   | The vendor ended that turn. Persistent Codex and ACP processes can remain alive.                                  |
| `TurnStream::terminal_status()`   | One logical outcome is committed, including failures caused by overflow. It does not prove process reaping.       |
| Stream drained                    | The consumer read all queued events and the reserved terminal.                                                    |
| Successful `close`                | Session cleanup completed. New admission is permanently refused.                                                  |
| Successful `ProcessControl::wait` | The launched child exited. Its contained descendants can still serve work until they exit or the host stops them. |
| Successful process stop           | The launcher completed process-tree cleanup, reaped the leader, and observed its containment group empty.         |

`ProcessControl::interrupt` distinguishes a delivered graceful interrupt, no delivery to an already
exited or stopping child, and an unsupported operation. The Tokio launcher uses SIGINT on Unix and
reports unsupported on Windows. Hosts can
implement their platform's interruption mechanism. `stop_process_with_limits` waits for
`kill_grace`, then uses `shutdown_timeout` for each cleanup stage. Tree cleanup still runs after a
leader exits because descendants may hold the workspace. SIGTERM or forced termination does not
establish the same vendor continuation state as a clean interrupt.

On Windows, `TokioLauncher` creates each child suspended, places it in an outer [Job Object](https://learn.microsoft.com/windows/win32/procthread/job-objects), then resumes it only
after the nested terminating Job is ready. A Job contains children created by its members. Cleanup
terminates the nested Job and waits for the outer Job's direct and nested process list to become
empty, so a leader exit cannot make a surviving helper look stopped. If setup cannot establish
that containment, launch fails and the suspended child is terminated. `ProcessControl::wait`
reports the launched leader separately; a surviving helper remains available until it finishes or
the host calls `kill` or drops the process control.

Claude's native continuation after an interrupted or killed turn needs particular care. See
[the Claude contract](harness-claude.md). Cancellation must never silently switch the caller to a
different conversation.

## Stream and pending-work budgets

`EventSink::with_limits` uses the host's limits. The defaults per stream are 1,024 payload events
and 8 MiB of serialized queued bytes. Permission and question facts have a reserve of their own on
both axes. The count reserve is twice `max_pending_requests`, 128 events by default. The byte
reserve is that same count multiplied by the 8 KiB one interaction event is budgeted at, clamped
to half `turn_buffer_bytes` so a small budget still leaves payload room: 1 MiB of the 8 MiB
default, leaving payload 7 MiB.

The clamp is what a host tuning `turn_buffer_bytes` downward has to know about. Below twice the
reserve it binds, and payload gets **half** of whatever the host configured — so a budget that
comfortably held one event before the reserve existed can refuse it now, and the `LimitExceeded`
names the derived payload cap rather than the number the host set. A host that wants a small turn
budget should size it against the payload half, not the total.

Payload may occupy at most `turn_buffer_bytes` less that reserve; an interaction event may use the
payload area while it is free. The two together never exceed `turn_buffer_bytes`, which is the
number a host sized its memory against and the number `EventReceiver::queued_bytes` reports
against. Without the byte reserve a turn whose deltas had filled the budget could not queue the
approval the vendor had just raised, so the host was never asked and the turn waited on an answer
that could not arrive. The queue preserves ordering between payload and interaction events.

Codex question settlement stays pending until `QuestionResolved` is published. Accepting an answer
does not release that ownership: terminal cleanup publishes an accepted outcome before completing
the turn, and server withdrawal closes any announced prompt.

The terminal has separate storage for one bounded error or the pair `Cancelled`, `Completed`.
Committing it never waits for transcript consumption and cannot create a detached delivery task.
The first terminal wins; later events are refused. Vendor failure fields are normalized before
the terminal or its observable status retains them. Host session and turn IDs remain exact, so
hosts must bound those identifiers too.

Structured content is bounded before it is queued, and a diff's file contents share one budget of
their own (`DIFF_MAX_CONTENT_LENGTH`) — otherwise 256 per-file ceilings multiply out to megabytes in
a single event against the 7 MiB that budget leaves payload. A file past the budget keeps its row and loses its bodies,
and `truncated` says so. See [`contracts.md`](contracts.md).

Exceeding a count or byte budget returns `LimitExceeded` and commits a `stream-overflow` failure.
Already queued events remain readable, followed by that failure. The driver stops native work.
An overflow means the transcript is incomplete; it never means execution succeeded. Hosts should
drain streams in their supervisor even when no browser is connected.

JSON-RPC separately caps outbound requests and in-flight incoming requests at
`max_pending_requests`, 64 by default. Queued and in-flight incoming callback payloads share an
8 MiB budget by default. Callback queue pressure fails the connection explicitly; response
correlation continues on its own path until closure. Request deadlines include writes, and
dropping a request removes its pending correlation entry. Approval deadlines remain separate from
request and idle deadlines.

ACP also bounds its SDK frame boundary by `turn_channel_capacity` queued frames (1,024 by
default) and `turn_buffer_bytes` serialized bytes in each direction; notification bursts do not
count against `max_pending_requests`. Output byte accounting includes the active
physical write, and an oversized batch or queue fails the connection. Generic outgoing ACP
requests reserve admission before entering the SDK queue.

Framed transports also enforce `Limits::line`, and stderr has its own bounded tail. These are
per-stream and per-connection budgets, not a total host-memory limit. The host caps the number of
sessions, retained completed streams, attachments and source buffers it supplies through its
launcher. Encoded byte accounting excludes Rust container overhead, which is bounded separately
by event and request counts.

## Safe host retries

`RequestFingerprint::of` hashes a versioned encoding of the prompt, attachment metadata and
bytes, and configuration patch. Logical IDs and attempt numbers are excluded. The host reserves
logical IDs atomically in its own storage and rejects reuse with a different fingerprint. It also
retains the original session configuration and execution authority across recovery.

`RecoveryRecord` provides the in-memory validation and transition rules. The host owns durable
storage, network retries and backoff. Before a side-effecting submission, record
`AcceptanceUnknown`; change it to `Accepted` only after acknowledgement. A lost acknowledgement
leaves `Reconcile`, never permission for another submission. An identical retry returns to the
same logical operation. A stale attempt cannot finish a newer one.

Only native proof that an uncertain attempt never ran permits `reconcile_not_submitted`, followed
by a newer attempt. A vendor without a reconciliation query cannot be replayed blindly. A
committed terminal ends recovery. An explicit abort or revoked consent also stops retries.

For recoverable host transport failures, use bounded attempt deadlines and cancellation-aware
waits with capped backoff and jitter, honoring vendor retry hints. Keep retrying until success or
an explicit abort or terminal refusal; an arbitrary retry-count limit does not establish a
terminal outcome. The library contains no database, Hub client or automatic submission loop.
