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

2. **An authorised working directory.** `HostContext::cwd` is a directory the host already
   authorised. The library never widens it and never chooses one.

3. **An environment source.** The library builds the positive allowlist — `BASE_ENVIRONMENT_KEYS`,
   every `LC_*`, and the harness's own `vendor_environment_keys` — from what the host passes.
   Nothing else reaches the child. There is deliberately no map of values a caller can supply, so
   no request can smuggle a host credential into a vendor process.

4. **Client identity.** `ClientInfo { name, version }`, sent to vendors that ask for it (Codex's
   `clientInfo`, ACP's `initialize`). It is the host's own name: a vendor reading its logs should
   see which product launched it.

5. **Optionally, a `PermissionBroker`.** By default every `ApprovalRequested` event reaches the
   host, which answers through `Session::respond`. A host with a policy implements the broker and
   returns `Allow`, `Deny { reason }` or `Ask`. A decision that cannot be applied to a particular
   question — the vendor offered no option matching it — becomes a question for a person rather
   than a failed turn.

6. **A cancellation token, a clock and the caps**, all with defaults: `CancelToken` for shutdown,
   `Clock` for the instant an event is stamped with, and `Limits` for the turn channel's capacity
   (1,024 events), the line and buffer caps, the stderr tail, the request timeout, the approval
   timeout and the kill grace. Request timeouts bound individual protocol calls; approval timeouts
   leave room for a person or host policy to decide. Harnesses read them back through
   `host.limits()`, and a host constructing `TokioLauncher` hands it the same ones with
   `TokioLauncher::with_limits`, so one setting governs a bound wherever it is enforced.

There is no credential field, and there never will be. The library reuses whatever the user
already logged into with the vendor's own CLI.

## What the host reads

- `Harness::discover` → `Discovery { executable, version, gate, auth, capabilities,
  permission_matrix, models }`, bounded by the trait before the host sees it. Capabilities and
  permission cells can narrow the harness declaration after a probe, but never widen it. The
  harness never caches; the host decides freshness. `AuthState` is `LoggedIn { mode }`, `LoggedOut
  { login_hint }` or `Unknown` — filled only from a surface that does not involve reading a
  credential, and `Unknown` when the only way to know would be to read one. The `executable` it
  found is what the host passes back on `OpenSession::with_executable`: a resolved path belongs to
  one harness, so it rides on the request rather than on the context every harness shares.
- `Harness::open_session` → a `Box<dyn Session>`. `SessionInfo` carries both ids, whether the
  vendor resumed, the configuration it actually accepted and what this build can do.
- `Session::configuration()` → the defaults a later turn inherits. `SessionInfo` is the opening
  snapshot; vendors such as Codex persist successful turn overrides. Read the shared accessor
  when displaying current settings or constructing the next request.
- `Session::start_turn` → a `TurnStream`: a bounded channel of `AgentEvent`. A host that stops
  reading slows the vendor instead of growing the library's memory.
- `AgentEvent { session_id, turn_id, at, kind }`. The `kind` is one of seventeen: session started,
  commands available, text and reasoning deltas with their block markers, the activity lifecycle,
  approval requested and resolved, usage, thread usage, account limits, cancelled, completed and
  error. `AgentEvent::is_terminal` answers "is this turn over" without a match.

Cancellation is a marker, not a terminal: it is emitted immediately before `Completed` and never
instead of it, so a host that does not recognise it still sees its turn end.

Start with `Configuration::default()` to leave permissions under the user's vendor profile.
Select a level explicitly with `level: Some(PermissionLevel::Default)` and select approval routing
with `routing: Some(ApprovalRouting::User)`. Successful explicit settings become session defaults;
later omitted fields retain them. Hosts need not resend settings on every turn. An absent value
from `Session::configuration()` means no reported selection, not read-only access.

For native review, pass a `ReviewRequest` to `Session::start_review`. `ReviewTarget` covers
uncommitted changes, a base branch, a commit, and custom instructions. The returned `ReviewStream`
contains an ordinary `TurnStream`, so the same event relay handles both. A harness that cannot
review returns `Error::NotSupported`; the host needs no vendor protocol code.

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
- `ScriptedLink` drives a protocol client with no process behind it.
- `RecordingBroker` and `FrozenClock` turn a policy decision and an event's timestamp into values
  a test can assert on.

```rust,ignore
let launcher = FakeLauncher::scripted(include_str!("../fixtures/claude/transcripts/hello.ndjson"));
// … build the host, open a session, assert on the events your mapping produced.
assert_eq!(launcher.last_launch().unwrap().env.get("CONNECTOR_SECRET"), None);
```

## Writing a harness

Implement `Harness` and `Session`, push every event through the `EventSink` a turn's stream comes
from — it normalises and bounds on the way through, so a reducer cannot emit an unbounded event by
accident — and leave the four optional methods to their defaults unless the vendor has them.

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
terminal, every event names its own session and turn, an approval can be answered, a cancelled turn
still completes, closing twice is not an error, every capability the descriptor did not declare
refuses as unsupported, and a probe never claims more than the descriptor's ceiling. A check that
cannot run on your fixture is reported as skipped rather than passed.

## Worked integration: mangostudio runtime

The runtime is the host. Its session supervisor owns a map from the application's session id to
`Box<dyn Session>`. Opening a session follows this sequence:

1. Resolve the user's workspace through the runtime's authorization policy. Build `HostContext`
   with that directory, the runtime's environment snapshot and its own client name and version.
2. Adapt the runtime's process supervisor to `ProcessLauncher`. Forward `LaunchSpec.argv`, `cwd`
   and the already filtered `env` unchanged. Return its stdin, stdout, exit and kill handles through
   `ManagedProcess`. Keep process groups and Windows job objects in this adapter.
3. Discover the selected harness. Show its capability and permission matrix in the session setup
   UI. Display `LoggedOut.login_hint` as text; an `Unknown` account state must not become a login
   form or trigger credential inspection.
4. Open the vendor session and retain its `SessionInfo` alongside the application session id.
   Create one task per active turn to drain its bounded event stream.

A runtime permission broker returns `Ask` when the user must decide. The event relay stores the
approval request id and its vendor option ids, then sends that question to the browser. When the
user chooses an option, the runtime calls `Session::respond` with that exact option id. Requests
that expire or belong to a closed session are removed from the pending map. The runtime must not
register the vendor's tools as application tools.

| Library event or call                                       | Runtime integration                                          |
| ----------------------------------------------------------- | ------------------------------------------------------------ |
| `TextDelta`                                                 | Append to the vendor session's assistant message             |
| `ActivityStarted` / `ActivityUpdated` / `ActivityCompleted` | Update that message's tool activity view                     |
| `ApprovalRequested` / `ApprovalResolved`                    | Add or remove the pending approval UI                        |
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
