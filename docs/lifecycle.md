# Turn ownership and recovery

A session admits one native turn at a time. A concurrent start returns `Error::Busy` with
`Dispatch::NotSubmitted`; it never replaces, queues or steers the current turn. A stopping attempt
continues to own admission until native work can no longer conflict and its terminal is committed.
The host does not have to drain the old transcript before starting another turn. In the ACP
harness the slot is released a moment after the terminal is published, so a start made the
instant the terminal is read can still be answered `Error::Busy`, and is simply made again.

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

One rule reconciles a leftover child on all three harnesses: branch on `cleanup_control()`. A
`close` whose stop did not prove the child gone returns `Error::CleanupRequired` on Claude, Codex
and ACP, and so does a Claude or Codex `cancel` that has to stop the process; an ACP `cancel` only
queues `session/cancel` and never stops one. A stage that ran out of time, an error from the host's
control and a panic in its code all count. `cleanup_control()` is the control the host's launcher
returned. The cause differs: Claude and Codex keep the typed failure of the stage, readable
through `cause()`, where ACP carries it as `Error::Vendor` text. The first outcome of a teardown is
kept: a later `close` returns a copy of it, with the same control, and does not ask the child to
stop again. A `close` that gave up on a launch still inside the launcher has no child and so no
control; the `start_turn` that owns the launch gets the `CleanupRequired` if its stop then fails,
and so does a later Claude `close`.

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
`kill_grace`, then uses `shutdown_timeout` for each cleanup stage. In the Codex harness those two
limits bound process teardown only. There, a turn being cancelled on a live session is bounded by
`cancel_settle_timeout` once its protocol stop is acknowledged, and expiry escalates to that
teardown. See `docs/harness-codex.md` for the stages. The Claude and ACP harnesses still bound
their cancellation with `kill_grace` and `shutdown_timeout`. Tree cleanup still runs after a
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
number of encoded (wire) bytes a host sizes its turn queue against and the number
`EventReceiver::queued_bytes` reports against; parsed heap is a separate matter, covered at the end
of this section. Without the byte reserve a turn whose deltas had filled the budget could not queue the
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

A harness that ends a turn with an activity or a reasoning phase still open closes it with
`EventSink::emit_close`, before it commits the terminal. Unlike `emit`, a close the budget has no
room for is refused with `LimitExceeded` and commits nothing, so the terminal that follows keeps
its own cause: a cancellation stays `Cancelled` and a completion stays `Completed`. The refusal is
sticky: after it no later close is queued and later payload is refused as overflow, so the host
never sees new events behind a close that is missing, and a stream whose last sender is dropped
after a refusal, with no terminal, ends with `stream-overflow` rather than silently. Under a full
queue the closes are best-effort and the terminal wins. A host therefore reads any terminal
(`Completed`, `Cancelled` or `Error`) as ending every activity and reasoning phase still open,
whether or not a close event arrived.

JSON-RPC separately caps outbound requests and in-flight incoming requests at
`max_pending_requests`, 64 by default. Queued and in-flight incoming callback payloads share an
8 MiB budget by default. Callback queue pressure fails the connection explicitly; response
correlation continues on its own path until closure. Arrival order therefore holds among
notifications and peer requests, not between them and responses: a response can reach its caller
before the handler has seen a notification that arrived ahead of it. A request made with
`RequestOptions::after_earlier_notifications` opts out of that for its own answer: the response
waits in the callback queue and reaches its caller once the handler has returned from every
notification read before it (and, on a client that hands questions over, from the hand-off of
every question read before it; without that option a question only has to have been started),
and when the peer exits or the link fails it is still delivered (or
the call failed) only after that queued work has drained, within `shutdown_timeout` (twice that
when the drain runs out inside a stretch that holds its cancellation off, below). Up to
`max_pending_requests` such answers have places reserved in the queue apart from the callback
budgets, so callback pressure cannot refuse one. `Client::close` and callback queue pressure fail
such a call at once. Request deadlines include
writes, and dropping a request removes its pending correlation entry. Approval deadlines remain
separate from request and idle deadlines.

`Client::request` and `Client::notify` wait for their own write. On a connection that only ever
uses those, callers share the link's lock and nothing else, as before. `Client::submit_request`
and `Client::submit_notification` queue the frame and return at once with the write and the answer
to wait for, which lets a caller under its own lock fix the order of two frames. From the first
frame queued on a connection, every outgoing frame takes its place in one queue, behind the
callers already waiting on the lock, so frames reach the wire in the order their callers sent
them; a caller that waits for its own write then takes the free link at once when nothing is
queued, and a turn in the queue otherwise. A queueing caller is slowed by nothing but
`WireOptions`, which `Client::connect_with` takes and which bounds nothing by default. One frame
larger than the per-frame bound is refused alone, with `Dispatch::NotSubmitted` and nothing
written, whoever sends it; an answer to the peer refused that way is replaced by a `-32603` reply,
and when even that cannot fit, because the peer's own id passes the bound, the connection ends as
a failed link. A frame queued with `submit_*` is refused alone in the same way when it is larger
than the queue could hold even when empty, which is every such frame under a queue bound of zero.
Passing the bounds on queued frames or queued bytes with what the queue already holds, which
counts the frame being written, means the peer stopped reading: the connection ends with `PeerTermination::OutboundBackpressure` and the call
receives the same `LimitExceeded`. When a close is already on record the close owns the end
instead: the call alone receives the `LimitExceeded`, the link is not made unusable, the handler
is told nothing, and what was queued before the close is still written before the link goes. Which of the three bounds was passed is read by type, not from
the error's text: `OutboundBound::exceeded_by(&error)` for the refusal a caller receives and
`PeerTermination::outbound_bound()` for the end the handler is told of, each `None` for anything
else. Which wait of a `Client::close` ran out is read by type too, from the `Timeout` it returns:
`is_link_shutdown_timeout(&error)` is `true` for a close giving up on the link, which may be still
open, `is_reader_shutdown_timeout(&error)` for a close that gave up on the reader, and
`is_handler_shutdown_timeout(&error)` for one that gave up on the handler's task, with the link
already closed in both. The `jsonrpc` module documentation says what each means; the paragraphs on
a second close and on a handler that blocks its thread, below, say when the last two come.
A queued request's `Reply` resolves to a `CallFailure` that
tells the peer's own error from an ended connection (`Client::ended` gives the same
`ConnectionEnd`), a deadline, an unwritten frame and a stopped handler by type, and
`CallFailure::peer_error_was_unreadable` tells an `error` member that could not be read as an
error object, which fails the call with a stand-in `-32603`, from a peer's own `-32603`. `Client::close`
writes the notifications queued before it and refuses the requests still queued: a host that needs a last
request answered waits for its reply first. A close that times out or is dropped before it
reaches the link leaves the link unusable, so nothing queued is written afterwards. A `Reply` that
is kept keeps the link's sender alive after the `Client` is dropped; drop replies before relying
on a child seeing end-of-input.

One request can differ from the connection's defaults through `RequestOptions`.
`without_deadline` waits for the answer for as long as the connection lasts, for a request such as
an ACP `session/prompt` that no fixed time bounds; the write of its frame keeps `request_timeout`,
so a peer that stopped reading still fails it. `outside_pending_budget` neither counts the request
against `max_pending_requests` nor refuses it there, for a request the host bounds some other way.
`labelled` gives it a host-written name that `CallFailure::label` returns and no error text
carries. `Client::close` takes at most twice `shutdown_timeout`, once for the answers this side
still owes the peer and once for the queue and the link, and may be dropped at any point for a
tighter bound. From the moment a close begins, or the connection ends, the queue takes nothing
more: a `submit_*` that comes after is refused, with `Error::Closed` or, on a link an earlier write
left unusable, `Error::Link`, and nothing of it is queued or written, and one that got in ahead is a frame queued before the close. `Client::request` and
`Client::notify` do not pass through the queue and are as they were: on a connection that has
ended, `request` returns `Error::Closed` and `notify` returns `Ok(())` having written nothing, and
one that read the connection as open an instant before it ended reaches the link, which answers it
(a closed link refuses the write, and the call returns that refusal).

Two ends of a connection stop the handler's task without draining its queue: the peer passing an
inbound budget, and `Client::close`. A call to `PeerHandler::on_notification`, or to
`PeerHandler::on_request_handoff`, that is in progress then is cancelled at the await it had reached. A call that must finish a stretch it has begun asks
for `Client::hold_cancellation()` before the stretch and drops the hold after it. While a hold is
out, such an end waits for it, for up to `shutdown_timeout`, and cancels only then; a hold asked
for after the cancellation was decided is never granted, so the stretch runs whole or not at all.
A close overlaps that wait with the one for the answers it owes the peer, so its bound of twice
`shutdown_timeout` stands, and after an inbound overflow `on_terminated` comes within one
`shutdown_timeout`. Calls that hold nothing are cancelled as promptly as before. The other ends
(the peer's exit, a failed link, this side's outbound budget) drain the queue within
`shutdown_timeout`, as before; a stretch still held when that runs out is waited for in the same
way, for up to one `shutdown_timeout` more. A hold belongs to its call: one kept, moved or
forgotten stops counting when the call is over.

`on_terminated` is made once, after the handler's task has stopped, and not at all for an end a
close put on record first: when the peer's exit, a failed link or a budget passed on either side
is found with a close already on record, `Client::ended` reports `Closed` and the handler is told
nothing. A close given up after it recorded its end (its future dropped, a timeout of the caller's
own, or made from inside the handler's own call, which it cuts short) still owns the end: the
handler is told nothing and the reader fails no pending call, and calling `close` again, or
dropping the client, finishes the teardown. What the cut leaves of the link depends on where it
came. Before the link was closed (the close was failing the pending calls, stopping the handler's
task, waiting for the answers it owes the peer, or waiting for the queue and the link) the link is
still open; after it, while the close waited for the reader, the link is closed. Either way a
caller can write nothing afterwards: `submit_*` is refused, `request` returns `Error::Closed` and
`notify` returns `Ok(())` having written nothing. The link is made unusable by a cut that came
before it was closed (unless another close is still in progress), which is what keeps a frame
already queued behind a write that did not finish from being written. A `Client::close` that
arrives while a connection is ending under this side waits for that call to return, within the
bound of a close, where it used to take the reader down under it. Dropping the `Client` cannot
wait: the call in progress is cancelled whatever it holds, and `on_terminated` may not be made at
all.

A second `close` made while the first is still held, kept and never polled again, ends within its
own bound all the same. If the first is parked inside the closing of the link it holds the sender,
and the second returns the link's `Timeout` after `shutdown_timeout` without having closed it;
dropping the first lets a later close finish. If it is parked in its wait for `on_terminated` it
holds the reader, and the second, which has closed the link by then, returns a `Timeout` that names
the reader (`is_reader_shutdown_timeout`, not `is_link_shutdown_timeout`) when its own bound of
twice `shutdown_timeout` runs out. A first close that finishes within its bound is no hindrance: the second then finds nothing
left to wait for, and both are `Ok`. The timeout is not limited to a first close that is parked and
forgotten. A later close whose own bound runs out in the window after the first one's expiry, about
a scheduler turn, in which the first still holds the reader while it takes it down, sees it too,
where it used to see `Ok`. The first close, polled again after the second gave up, still completes
with `Ok`; dropping it takes the reader down mid-call, so `on_terminated` does not return.

A close future that is kept holds a borrow of the client it was called on, or a share of it when it
owns one, so a client is not dropped under it. While the future is parked in its wait for the
reader, the reader lives no longer than the future: dropping it takes the reader down. A host that
wants a kept close gone drops the future, and then the client. A future that is leaked instead (`std::mem::forget`) holds the lock on the
reader's handle for good, and dropping the client stops the reader without that lock.

A handler that blocks its thread, with `std::thread::sleep` or a blocking lock, cannot be stopped
by the abort that a close gives up on, since an abort lands at an await. A close does not wait for
it without a bound, whichever call it blocks in, and ends within twice `shutdown_timeout` plus at
most 100 ms, 50 ms for each task whose abort it has to wait to land once its time is spent. The
link is closed, the task stays on its blocked thread until the call returns and then ends without
a further call, and the client stays closed: `Client::ended` keeps the end it recorded, no end is
told to the handler through a further call (`on_terminated`, where it is the call that blocks, was
made once), and a later close or a drop does not wait for the call again, so a later close that
returns `Ok` says nothing of whether the call has returned.

- `on_notification` and `on_request_handoff` run on the handler's task. On a connection that has not
  ended the close aborts it and waits for it as long as the first grace has left, as for a slow
  call, or 50 ms if that is spent, closes the link, and returns the handler's `Timeout`
  (`is_handler_shutdown_timeout`) if the task is still running as it ends, or the link's own
  error if closing the link failed. A task that ended in the meantime, however late, is not a
  failure: the close looks at it again after the link is closed.
- `on_terminated`, and those two on a connection that ended under this side once the reader has
  taken the handler's task to drain it, are waited for by the reader, which the close waits for
  within its bound. A close that gets to the task before the reader does, while the reader has
  recorded the end but not yet reached the task, is in the case above. The close then takes the
  reader down, allows the abort 50 ms to land when its bound is spent, and returns the reader's
  `Timeout` (`is_reader_shutdown_timeout`) when the reader is inside a call that blocks, or the link's own
  error. The reader is the last thing a close waits for, so it is not looked at again: a reader that
  is not blocked but takes longer than those 50 ms to stop, after `on_terminated` used the whole
  bound, gives the same `Timeout`. A reader parked at an await, waiting for a task whose call blocks, is taken down, and the
  close returns `Ok`.
- `on_request` runs on tasks the close lets write their refusals for up to `shutdown_timeout`,
  then aborts without waiting for them. The close returns `Ok`.

A call to `on_notification` or `on_request_handoff` that blocks its thread for longer than the
first grace and then returns is treated as one that never does: the close returns the handler's
`Timeout`, with the link closed, where it used to wait the call out. A handler must not block its
thread; this only keeps the close from waiting for it. The bound also relies on another worker
thread of the runtime being free to run its timers, and on the link's own
`send` and `close` not blocking their thread.

A caller that wraps `close` in a timeout shorter than twice `shutdown_timeout` on a connection that
ended under this side can cut `on_terminated`, and should wrap `close` in nothing shorter than that
bound if it relies on the call finishing.

Things a client does on the wire only when its `WireOptions` ask for them, each off by
default. `with_batch_decoding` reads a line that is a JSON array as a batch: its members are
routed in order, each notification and question counted against `max_pending_notifications` on its
own and the whole batch counted before any of it is queued, the line's bytes taken from
`max_pending_bytes` once and shared out among those members; the answers to a batch's questions
go out as frames of their own. `with_parse_error_replies(max_queued)` answers a line that is not
JSON with `-32700` and a null id, at most `max_queued` of them waiting to be written at a time,
held beside the bounds of the outbound queue. A reply is a write: a peer that sends such lines and
does not read what it is sent leaves one waiting, and when that write's `request_timeout` runs out
the connection ends as after any write that could not finish, where a client that answers nothing
would have gone on reading. What is answered is a line the transport handed over that is not JSON,
which includes a stdio line whose bytes were not UTF-8, since the line reader replaces those bytes
and hands the line on. What the transport cannot hand over is not answered, because the read fails
and the connection ends: a line or websocket message past `max_line_bytes`, and a websocket text
frame that is not UTF-8. `with_invalid_request_replies(max_queued)` answers JSON that is not a
request with `-32600`, under a bound of its own and beside the queue's bounds in the same way,
each reply a frame of its own. Answered under a null id: a line that is a number, a string,
`true`, `false` or `null`; where batches are read, an empty batch and each member of a batch that
is not an object; where they are not, a line that is an array, once. Answered under its own id
when that is a string or a number of at most 128 bytes as this side writes it, and under null
otherwise: an object with neither `result` nor
`error` whose `method` is missing or not a string, or whose `id` is something an id cannot be.
Never answered: an object with `result` or `error`, which is a response, this side's own replies
among them, so two peers that answer invalid requests cannot loop; an object with no `method` and an id that
could name a call, which is read as that call's response as before; and a request or notification
with a `method` that is a string, whatever its `params` or `jsonrpc` say. One batch is answered
for at most `max_queued` of its members, and a batch refused whole is answered for none. A bound of nothing answers nothing and reroutes nothing. Without
the option every object is routed exactly as before: `{"id":4,"method":5}` is the response to
call `4`, and `{"method":"x","id":{}}` is put to the handler.
`with_cancel_on_abandon(method)` queues a notification
`method` with `{"requestId": id}` when a request that was written is given up, by its future or
`Reply` being dropped or its deadline running out; not for a request that never began to be
written or whose write has ended with the link refusing its frame whole, not once a response to
it has been taken for its caller, and not after the connection has ended. A response read at the
moment its caller lets go ends one way only: taken for the caller with nothing sent, or the cancel
sent and the response dropped as a late one. One request the peer never got is announced all the
same: one given up while its frame is being written, when the link then refuses that frame whole
and stays usable. Some of the frame may have reached the peer at the moment the call is let go,
and a decision put off until the write ended would let the caller's next write out ahead of the
cancel. That a refusal which is not a link failure took no byte is the contract of
`LinkSender::send`.

A client made with `WireOptions::with_request_handoff` hands each of the peer's questions to
`PeerHandler::on_request_handoff` on the task that runs the notifications, in the order the peer
sent it, with a `Responder`. The call is awaited like a notification, so a response asked for
after earlier peer work waits for the questions read before it to be handed over too. The handler
answers there or keeps the responder and returns; a responder is answered from any task, and
`Responder::answer` queues the answer without waiting, in the outbound queue and against its
bounds, behind every frame queued before it. A question past `max_in_flight_requests` is not
handed over: `PeerHandler::on_request_past_cap` answers it on the spot, with a result if the
protocol wants one, and this client's `-32000` refusal otherwise. An answer too large for a frame
comes back with its responder, the connection untouched. A responder dropped unanswered tells the
peer with `-32603` and gives back what the question held. An answer is a frame that has to be
written: one that was queued and then could not be, because the link refused it or its deadline
ran out in the queue, ends the connection whether or not anyone awaits its write, and so does a
question nothing can be said for: one no reply to fits a frame, or one let go on a queue bounded
at nothing. Answers are queued without waiting, so the outbound bounds are
what limits them when a peer asks faster than it reads: the option does not set them, so set them beside it. An end that
finds a close already on record is the close's, whatever its cause (a write or a read that fails,
the peer's exit, a budget passed on either side), and the handler is told of no termination for
it. A question still queued when the
connection ends is not handed over. Once the connection has ended a
responder still out answers nothing and says nothing: a handler that parks questions settles them
before it closes. The hand-off call is cancelled at an end like a notification call, and
`hold_cancellation` holds there as it does in one. A request with `after_earlier_notifications` made from
inside it is refused before it is written, as it is inside a notification. Without the option none of this applies: a question is
answered by `on_request` on a task of its own, and its answer is written when that task has the
link. An end raises the shutdown flag, and from then on that call is dropped at the await it is
parked at, never run to its end, and the peer is refused with `-32000` over a link that still
takes the write: awaited for up to `shutdown_timeout`, before the link is closed, and before
`on_terminated` for an end that is not a close. A close and an inbound overflow raise the flag at
once; the peer's exit, a failed link and an outbound overflow first give the notifications
already read up to `shutdown_timeout` to reach the handler, and a call that returns in that time
answers the peer itself. A link whose write failed, that this side's outbound budget made
unusable, or that the peer's exit has closed to writes, takes no refusal. Dropping the `Client` aborts the call without waiting for it; a refusal
another thread had begun to write may still go out.

ACP sets those bounds from `Limits`: queued JSON-RPC messages by the larger of
`turn_channel_capacity` and `max_pending_requests` (1,024 by default, batch members that are
notifications or questions counted individually), and incoming bytes by `turn_buffer_bytes`. One
outgoing frame may be `host.outbound_buffer_bytes()` bytes, which defaults to `turn_buffer_bytes`,
and the outgoing queue twice that; the optional builder override changes outgoing bytes without
raising incoming or event caps. Output byte accounting includes the active physical write. An
oversized frame is refused alone; an oversized batch or queue ends the connection. Generic
outgoing ACP requests reserve admission before they are queued. See
[the ACP transport guide](harness-acp.md#transport) for retained-buffer accounting and exclusions.

Framed transports also enforce `Limits::line`, and stderr has its own bounded tail. These are
per-stream and per-connection budgets, not a total host-memory limit. The host caps the number of
sessions, retained completed streams, attachments and source buffers it supplies through its
launcher.

Every byte budget in this library, including `turn_buffer_bytes`, `outbound_buffer_bytes`,
`max_pending_bytes` (the JSON-RPC
callback byte cap on `ClientOptions`, not a `Limits` field; `ClientOptions::with_limits` sets it from
`turn_buffer_bytes`) and the line caps, counts encoded (wire) bytes. None of them measures parsed memory, and event and request
counts do not bound it either: one large message is a single event however much it expands once
parsed. How much a message expands depends on its shape and on the build. As a lower bound, a
`serde_json::Value` is 32 bytes, or 72 bytes when any crate in the build enables
`serde_json/preserve_order`; `mango-agent-acp` does, through the `agent-client-protocol-schema`
crate, whose manifest enables it for the whole build.
Object members, array capacity and allocator rounding come on top. In one measurement of a 1 MB
message on x86_64 (release build, `serde_json` 1.0.151), a single string parsed to about 1 MB,
while a 1 MB array of `0`, `[]` or `{}` elements held about 17 MB (32-byte values) or 38 MB
(72-byte values), and a 1 MB array of `{"a":1}` objects held about 83 MB or 55 MB. Those are
worst-case shapes, not typical ones, and ordinary text stays close to its wire size. Treat the
figures as an illustration, not a multiplier. A host that needs a hard memory cap must enforce it
itself, for example with an operating-system memory limit on the process that embeds the library.

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
