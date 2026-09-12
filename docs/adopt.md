# Adopting the library in a host

How a host (an IDE, a runtime, a CLI) embeds mango-external-agents. The types named here land with
the core crate in plan 002; this page fixes the shape so the crate can be written against it.

## What the host provides

1. **A `ProcessLauncher`.** The host spawns the vendor CLI: it decides the sandbox (Job Objects,
   process groups, bwrap, a container), the window flags, and the kill sequence. The library passes
   a `LaunchSpec { argv, cwd, env, stdin, hide_window }` and receives a `ManagedProcess` with stdin,
   a bounded line stream on stdout, a redacted stderr tail, `kill(reason)` and `wait()`. Hosts
   without a spawner of their own take `TokioLauncher` from the `launcher-tokio` feature.
2. **An authorised working directory.** `HostContext.cwd` is a directory the host already
   authorised; the library never widens it.
3. **An environment source.** The library builds the positive allowlist (base keys plus the
   harness's `vendor_environment_keys`) from what the host passes; nothing else reaches the child.
4. **Client identity.** `ClientInfo { name, version }`, sent to vendors that ask for it
   (Codex `clientInfo`, ACP `initialize`).
5. **Optionally, a `PermissionBroker`.** By default every `ApprovalRequested` event reaches the
   host, which answers through `Session::respond`. A host with a policy implements the broker and
   returns `Allow`, `Deny(reason)` or `Ask`.
6. **A cancellation token and a clock**, for tests and for shutdown.

## What the host reads

- `Harness::discover` → executable, version, gate verdict, auth state, models. The harness never
  caches; the host decides freshness.
- `Harness::open_session` → a `Session` handle. `start_turn` returns a bounded event stream
  (default 1,024 events); a slow host slows the vendor instead of growing memory.
- `AgentEvent` in fourteen kinds (session started, commands available, text and reasoning deltas,
  activity lifecycle, approval requested and resolved, usage, limits, cancelled, completed, error).
  The host maps them to its own wire; the library returns reason enums, never i18n keys.

## Mapping events to your own product

Keep the mapping in one module in the host. It is small and it is where product vocabulary
(disclosure text, presets, translated reasons) lives. mangostudio's runtime is the worked example
and will be linked here once plan 006 documents it.

## Testing a host

The `testing` feature ships `FakeLauncher` (scripted stdout, stderr, exit; records argv, env, cwd),
`FakeHarness`, `FakeSession`, `ScriptedLink` and `RecordingBroker`, plus a `conformance` module
that any `Harness` implementation must pass (open → turn → approval → respond → cancel → close).
A host test spawns nothing: it scripts a transcript from `fixtures/<vendor>/` and asserts on the
events it maps.
