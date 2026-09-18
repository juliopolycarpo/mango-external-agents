# Adopting the library in a host

How a host (an IDE, a runtime, a CLI) embeds mango-external-agents. Every type named here exists
in the core crate; the crate's own README carries the same walkthrough as a doctest, so the shape
below is compiled rather than described.

## What the host provides

All of it through one `HostContext`, built once and shared by every session:

```rust,ignore
let host = HostContext::builder()
    .launcher(Arc::new(TokioLauncher::new().with_limits(&limits)))  // or the host's own
    .cwd(authorised_directory)                  // already authorised, never widened
    .scratch(child_visible_scratch_directory)   // optional, required for artifacts such as Claude MCP config
    .environment(EnvSource::from_process())     // the source; the allowlist does the filtering
    .client_info("my-host", env!("CARGO_PKG_VERSION"))
    .broker(Arc::new(MyPolicy))                 // optional
    .limits(Limits::default())                  // optional
    .build()?;
```

1. **A `ProcessLauncher`.** The host spawns the vendor CLI: it decides the sandbox (job objects,
   process groups, bwrap, a container), the window flags and the kill sequence. The library passes
   a `LaunchSpec { argv, cwd, env, stdin, hide_window }` — the cwd and the environment are already
   decided, so a launcher that overwrote either would be widening an authorisation it was given —
   and receives a `ManagedProcess` in three separately owned halves: a `ByteSource` for stdout, a
   `ByteSink` for stdin, and an `Arc<dyn ProcessControl>` for waiting, killing and the redacted
   stderr tail. Byte chunks rather than an `AsyncRead`, so a host on tokio, on smol, on blocking
   threads or on a recorded fixture can all answer the port. Framing is the library's: `LineStream`
   assembles lines under a cap and refuses an over-long one rather than allocating it.

   Hosts without a spawner of their own take `TokioLauncher` from the `launcher-tokio` feature.

   **Implement `ProcessControl::interrupt` if you want cancellation to be recoverable.** It has a
   default body returning `InterruptOutcome::Unsupported`, and `TokioLauncher` returns the same off
   Unix, so a launcher that does not override it makes every `Session::cancel` a forced
   termination. The Claude harness records a forced termination as nonresumable — the vendor
   documents that resuming would continue the turn the kill left unfinished, so the harness refuses
   instead — and from then on `start_turn` returns `Error::Cancelled` for the life of that session.
   With a graceful interrupt the same cancel reports `StopOutcome::Interrupted` and the session
   keeps its native continuation. On Unix this is `SIGINT` to the child's process group; on Windows
   it is a console-specific port the host owns, which is why the library does not guess one.

2. **An authorised working directory.** `HostContext::cwd` is a directory the host already
   authorised. The library never widens it and never chooses one.

3. **Scratch storage when a harness needs an artifact the child reads.** `HostContext::scratch`
   is an absolute, host-created directory. The host owns its ACL and any container or sandbox
   mount that makes the same path visible to the child. On Unix, Claude accepts a root when every
   directory on its path — the root and each ancestor, both as written and after resolving
   symlinks — is owned by the effective user or root, and is either not group- or other-writable or has the sticky bit (`/tmp` normally
   does). A private root beneath a world-writable non-sticky parent is refused: the child resolves
   the path by name, so a parent another account can rename names somebody else's file by then.
   Note that WSL DrvFs mounts such as `/mnt/d` report mode `0777` without the sticky bit, so a
   scratch root beneath one is refused; put it under the user's home instead. On Windows, its ACL
   must authorize only the intended child identities.
   The library creates and removes only a unique leaf beneath it, and the whole leaf path must be
   one the vendor's own command line can carry. It never falls back to a process-global temporary
   directory. Claude requires this for `--mcp-config`; a host that does not use MCP can leave it
   unset.

4. **An environment source.** The library builds the positive allowlist — `BASE_ENVIRONMENT_KEYS`,
   every `LC_*`, and the harness's own `vendor_environment_keys` — from what the host passes.
   Nothing else reaches the child. There is deliberately no map of values a caller can supply, so
   no request can smuggle a host credential into a vendor process.
   On Windows, duplicate environment names that differ only by case are canonicalized once with
   the first lexical spelling winning. Hosts should avoid conflicting `PATH` and `Path` values.

5. **Client identity.** `ClientInfo { name, version }`, sent to vendors that ask for it (Codex's
   `clientInfo`, ACP's `initialize`). It is the host's own name: a vendor reading its logs should
   see which product launched it.

6. **Optionally, a `PermissionBroker`.** By default every `ApprovalRequested` event reaches the
   host, which answers through `Session::respond`. A host with a policy implements the broker and
   returns `Allow`, `Deny { reason }` or `Ask`. A decision that cannot be applied to a particular
   question — the vendor offered no option matching it — becomes a question for a person rather
   than a failed turn.

7. **A cancellation token, a clock and the caps**, all with defaults: `CancelToken` for shutdown,
   `Clock` for the instant an event is stamped with, and `Limits` for the turn channel's capacity
   (1,024 payload events and 8 MiB), pending requests, line and buffer caps, stderr tail, request,
   approval and idle timeouts, graceful interruption and shutdown deadlines. Request timeouts bound
   individual protocol calls; approval timeouts leave room for a person or host policy to decide.
   Harnesses read them back through
   `host.limits()`, and a host constructing `TokioLauncher` hands it the same ones with
   `TokioLauncher::with_limits`, so one setting governs a bound wherever it is enforced.

There is no credential field, and there never will be. The library reuses whatever the user
already logged into with the vendor's own CLI.

## What the host reads

- `Harness::discover` → `Discovery { executable, version, gate, auth, capabilities,
  permission_matrix, models, configuration_catalog }`, bounded by the trait before the host sees
  it. Capabilities and permission cells can narrow the harness declaration after a probe, but
  never widen it. The harness never caches; the host decides freshness. `AuthState` is `LoggedIn
  { mode }`, `LoggedOut { login_hint }` or `Unknown` — filled only from a surface that does not
  involve reading a credential, and `Unknown` when the only way to know would be to read one. The
  `executable` it found is what the host passes back on `OpenSession::with_executable`: a resolved
  path belongs to one harness, so it rides on the request rather than on the context every harness
  shares.
- `Harness::list_sessions` and `Harness::account_usage` are optional services for reading without
  an open conversation. Both default to `Error::NotSupported`; session capability flags do not
  guarantee these separate services. The shipped harnesses retain those defaults. In Codex,
  listing and account usage are available through an open `Session`, not through a pre-session picker.
- `Harness::open_session` → a `Box<dyn Session>`. A host that has just probed can hand the answer
  back on `OpenSession::with_discovery(DiscoveryReceipt)` rather than paying for the probe twice;
  the receipt is checked for harness identity, executable identity and freshness, and then
  forgotten. It is not a cache and nothing in the library stores one.
- `Session::snapshot()` → a `SessionSnapshot`: the two ids, the harness identity, the transport
  selection, the lifecycle status, this session's capabilities, the configuration state, the
  configuration catalog, the slash commands, and whether the vendor resumed. It is a value, so two
  fields read off one snapshot were read at one instant.
- `Session::subscribe()` → a `SessionSubscription` that cannot miss a change made after it was
  opened: subscribing *is* reading, so there is no window between the read and the subscription.
  Every snapshot carries a `SessionRevision` that only increases.
- `Session::configure(ConfigurationPatch)` → a `ConfigurationOutcome`, for a vendor that can be
  reconfigured on an open session. Most vendors cannot set several options atomically, so the
  outcome says which axes landed, which were refused and why, and what became of the rest.
- `Session::start_turn` → a `TurnStream`: a bounded channel of `AgentEvent` read through `recv()`.
  Overflow commits an explicit failure and stops native work. Terminal status remains observable
  without draining the stream. Keep this owner in the supervisor across browser disconnects;
  dropping it requests cancellation. See [turn ownership and recovery](lifecycle.md).
- `AgentEvent { session_id, turn_id, attempt, at, kind }`. The `kind` is turn-scoped, always:
  turn started, text and reasoning deltas with their block markers, the activity lifecycle,
  approval requested and resolved, question asked and resolved, usage, thread usage, account
  limits, cancelled, completed and error. `AgentEvent::is_terminal` answers "is this turn over"
  without a match, and `AgentEvent::operation()` answers "whose work was this".

Session facts are **not** turn events. The vendor's own session handle and the slash-command
catalog are read from the snapshot, because they change between turns and before the first one —
and because carrying them on a turn stream meant inventing a turn id for something no turn
produced.

Cancellation is a marker, not a terminal: it is emitted immediately before `Completed` and never
instead of it, so a host that does not recognise it still sees its turn end.

Settings are a **patch**, not a set of values. `ConfigurationPatch::new()` changes nothing and
leaves every axis under the user's own vendor profile; `.level(ConfigurationChange::Set(
PermissionLevel::Default))` selects one, and `.model(ConfigurationChange::Reset)` removes an
override the host had set. A vendor that cannot reset refuses explicitly rather than reporting a
success it did not have.

Read the result through `Session::snapshot().configuration`, which keeps three readings apart:
`requested` is what the host asked for, `accepted` is what the harness confirmed it encoded, and
`observed` is what the vendor reported about itself. Only the third is evidence — an accepted
command-line flag is not a vendor-observed model — and an absent value on any of them means
unknown, never read-only.

A vendor that stops to ask a **question** rather than for an approval sends
`EventKind::QuestionAsked`, answered through `Session::answer`. It grants nothing: no
`PermissionBroker` is consulted about one, and `InteractionKind::grants_authority` is the field a
host's audit trail reads to keep the two apart. Secret collection and arbitrary forms are outside
scope and are refused by name rather than reshaped into free text.

For native review, pass a `ReviewRequest` to `Session::start_review`. `ReviewTarget` covers
uncommitted changes, a base branch, a commit, and custom instructions. The returned `ReviewStream`
contains an ordinary `TurnStream`, so the same event relay handles both. A harness that cannot
review returns `Error::NotSupported`; the host needs no vendor protocol code.

See [`contracts.md`](contracts.md) for the rationale behind each of these shapes, the identifier
mapping a host persists, and which public types are protected against future growth.

## Mapping events to your own product

Keep the mapping in one module. It is small, and it is where product vocabulary lives —
disclosure text, presets, translated reasons. The library returns reason enums and never an i18n
key, precisely so that module is the only place a string is chosen.

## Testing a host

The `testing` feature ships fakes that spawn nothing:

- `FakeLauncher` replays a transcript or answers each line written to it, and records the argv, the
  cwd and the environment every launch received — which is how a host proves its own secret never
  reached a vendor child.
- `FakeHarness` emits the shape a real harness emits, including an approval that waits for an
  answer, so a host's event mapping can be written before any vendor CLI exists.
- `Announcer` makes a `FakeProcess` speak without being written to first, which is what a peer that
  announces on its own initiative does — and the only way to reach what a session does about traffic
  that arrives while it is waiting.
- `ScriptedLink` drives a protocol client with no process behind it.
- `RecordingBroker` and `FrozenClock` turn a policy decision and an event's timestamp into values
  a test can assert on.

```rust,ignore
let launcher = FakeLauncher::scripted(include_str!("../fixtures/claude/transcripts/hello.ndjson"));
// … build the host, open a session, assert on the events your mapping produced.
assert_eq!(launcher.last_launch().unwrap().env.get("CONNECTOR_SECRET"), None);
```

## Writing a harness

Implement `Harness` and `Session`, push every turn event through the `EventSink` a turn's stream
comes from — it normalises and bounds on the way through, so a reducer cannot emit an unbounded
event by accident — publish every *session* fact through the `SessionState` the session holds, and
leave the optional methods to their defaults unless the vendor has them. A capability the harness
does not implement stays `false`: unfinished behaviour is unadvertised, not silently accepted.

`Session` requires exactly one method for state — `fn state(&self) -> &SessionState` — and
`snapshot`, `subscribe`, `ids`, `capabilities` and `require_capability` are provided from it.

A harness holds a `HostContext`, not a bag of durations, so take the bounds from it rather than
from a constant: `ClientOptions::new("Codex app-server").with_limits(host.limits())`. A harness
that threads its own timeout is a harness that ignores the host on the day the host asked for ten
seconds.

Then run the conformance suite:

```rust,ignore
let report = mango_external_agents::testing::conformance::run(
    &MyHarness::new(),
    &host,
    conformance::Options::default(),
)
.await;
report.assert_passed();
```

It checks what a host is entitled to assume: a turn ends exactly once and nothing follows its
terminal, every event names its own session, turn and attempt, an approval can be answered, a
cancelled turn still completes, closing twice is not an error, session state is readable before any
turn has run, a session update reaches a subscriber, every capability the descriptor did not
declare refuses as unsupported, and a probe never claims more than the descriptor's ceiling. A
check that cannot run on your fixture is reported as skipped rather than passed.

## Worked integration: mangostudio runtime

The runtime is the host. Its session supervisor owns a map from the application's session id to
`Box<dyn Session>`. Opening a session follows this sequence:

1. Resolve the user's workspace and child-visible scratch directory through the runtime's
   authorization policy. Build `HostContext` with those paths, the runtime's environment snapshot
   and its own client name and version.
2. Adapt the runtime's process supervisor to `ProcessLauncher`. Forward `LaunchSpec.argv`, `cwd`
   and the already filtered `env` unchanged. Return its stdin, stdout, exit and kill handles through
   `ManagedProcess`. Keep process groups and Windows job objects in this adapter.
3. Discover the selected harness. Show its capability and permission matrix in the session setup
   UI. Display `LoggedOut.login_hint` as text; an `Unknown` account state must not become a login
   form or trigger credential inspection.
4. Open the vendor session and retain its handle alongside the application session id. Subscribe
   to `Session::subscribe()` once, so a renamed vendor handle, a re-announced command catalog or a
   settings change reaches the browser without a turn having to be running. Create one task per
   active turn to drain its bounded event stream.

A runtime permission broker returns `Ask` when the user must decide. The event relay stores the
interaction id and its vendor option ids, then sends that question to the browser. When the user
chooses an option, the runtime calls `Session::respond` with that exact option id. Render the
option's `scope` and `policy_changing` flag: "allow for this session" and "allow from now on" are
different decisions, and a host that shows one as the other widens an authorisation nobody gave.
Requests that expire or belong to a closed session are removed from the pending map. The runtime
must not register the vendor's tools as application tools.

| Library event or call                                       | Runtime integration                                          |
| ----------------------------------------------------------- | ------------------------------------------------------------ |
| `TextDelta`                                                 | Append to the vendor session's assistant message             |
| `ActivityStarted` / `ActivityUpdated` / `ActivityCompleted` | Update that message's tool activity view                     |
| `TurnStarted`                                               | Record the vendor's handle for this attempt                  |
| `ApprovalRequested` / `ApprovalResolved`                    | Add or remove the pending approval UI, with its reach shown  |
| `QuestionAsked` / `QuestionResolved`                        | Add or remove a question prompt — never an approval prompt   |
| `Usage` / `ThreadUsage` / `AccountLimits`                   | Update usage displays without inferring prices               |
| `Error`                                                     | Show the bounded error and retain the failed turn's identity |
| `Completed`                                                 | Stop the turn relay and release its pending UI state         |
| `cancel(ConsentRevoked)`                                    | Stop work when the user revokes the session's consent        |
| `close(Shutdown)`                                           | Release the vendor session when the runtime shuts down       |

Keep this mapping in the runtime adapter. The library does not depend on mangostudio's protocol
or database types. Persist native session ids only for vendor resume; never feed the vendor's
assistant output back into the host model's context as instructions. The `mea` host provides an
executable example of `TokioLauncher`, terminal approval decisions and event consumption without
the runtime's browser or database dependencies.

On Windows, `TokioLauncher` also resolves installed `.ps1` entrypoints when native executable
resolution fails. It searches only the supplied `PATH`, runs Windows PowerShell from the supplied
`SystemRoot` with `-File`, and preserves arguments as data. This covers Cursor's official Windows
launcher without requiring a host to create a wrapper or change execution policy.

Before a Windows child runs, `TokioLauncher` starts it suspended and attaches it to an outer
[Job Object](https://learn.microsoft.com/windows/win32/procthread/job-objects). It then creates
the nested Job that terminates the tree. Windows reports the outer Job's direct and nested members
through its process list, so cancellation waits until that list is empty, including when the
original child already exited. `ProcessControl::wait` reports the original child independently, so
contained helpers continue until they finish or the host calls `kill` or drops the control.
Dropping a handle requests cleanup while the host runtime remains live; the Job's close policy
terminates remaining members during runtime shutdown. A failed attachment terminates the suspended
child and reports launch failure rather than returning an uncontained process.
