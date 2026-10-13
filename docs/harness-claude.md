# Claude Code harness (`mango-agent-claude`)

Drives the `claude` CLI a user already installed, through the headless surface Anthropic
documents, over one child process per turn. It never logs in, never reads a credential and never
downloads a binary.

Facts below were re-checked on 2026-09-17 against `claude` **2.1.270** and the vendor's own pages.
Re-verify before relying on them; `mea capture --harness claude` regenerates the public contract
under `fixtures/claude/contract/`.

Compliance posture: see [compliance.md](compliance.md).

## The executable and the version gate

|                   |                                                                                           |
| ----------------- | ----------------------------------------------------------------------------------------- |
| Executable        | `claude`, resolved by the host's launcher or pinned with `ClaudeHarness::with_executable` |
| Version read from | `claude --version`, e.g. `2.1.270 (Claude Code)`                                          |
| Minimum driven    | **2.1.211**                                                                               |
| Transport kinds   | `stdio` only; anything else is `Error::UnsupportedTransport` before a spawn               |

2.1.211 is where `--forward-subagent-text` arrived ("Added `--forward-subagent-text` flag and
`CLAUDE_CODE_FORWARD_SUBAGENT_TEXT` environment variable to include subagent text and thinking in
stream-json output" —
[changelog](https://code.claude.com/docs/en/changelog)). Every turn passes that flag, so a build
without it fails at startup.

**The flag surface is the gate; the version is the fallback.** `claude --help` is parsed for the
flags and vocabularies this harness depends on. A repackaged or backported build that has every
flag a turn passes stays usable, even below the pin. Discovery reports a parsed surface missing one
of those flags as `GateVerdict::MissingRequiredSurface`; opening returns `Error::Protocol` with a
static expected shape and no raw help text. A `--help` that yielded neither the permission modes
nor any required flag is unreadable, never "a binary with no options": a parsed old version then
reports `GateVerdict::VersionTooOld`, while a current or unreadable version reports
`GateVerdict::Unknown` and opening returns a distinct safe `Error::Protocol` refusal. A multi-line
wrapper banner can include its own semver, so discovery reads the semver from the line that
identifies Claude Code rather than accepting the first semver in all stdout. A single bare version
remains accepted for the documented compact form.

Probe output that may be incomplete is never read as a verdict. A read error before the CLI closed
stdout, a line over the host's line cap, more than 1 MiB of stdout (the largest captured help is
21,401 bytes, about 2% of that cap) or the 15 s timeout cut a probe short, and what each probe
concludes from the lines that did arrive differs:

- `--help` is strict: a cut-off listing is unestablished, so discovery reports
  `GateVerdict::Unknown` rather than `MissingRequiredSurface`. Missing lines cannot show that a flag
  is absent.
- `--version` uses the complete lines it read, because an incomplete read proves the binary ran. A
  banner line that names Claude Code and arrived whole still supplies the version, as if the read
  had finished. A cut-off read is read strictly: a lone line that lacks the "Claude Code" banner,
  including the bare compact form `2.1.270`, is not trusted, because the banner line behind a
  wrapper's own preamble may be the one that was lost. Otherwise, and when no line arrived, the CLI
  is installed with an unreadable version and the gate comes from `--help`, as for an unparseable
  banner. Discovery then reports no `version` at all rather than the lines that arrived, so a
  receipt never hands a wrapper's number back to opening as if it were Claude Code's. Only a spawn that failed or a child that printed nothing
  reports the CLI as not installed, and opening is never refused as "a CLI that reported no
  version" for a read that failed part way.
- `auth status` gives the lines that arrived to the same parser as a complete read, which trusts
  only a whole JSON object. A status document that arrived complete keeps its answer, so a
  signed-in account keeps `auto` even if later output failed. A document cut off inside the object
  is not parsed and the answer is unknown authentication, which leaves `auto` unavailable, as for
  any unknown account.

Anthropic's [CLI reference](https://code.claude.com/docs/en/cli-reference.md#cli-flags) cautions
that `claude --help` does not list every supported flag. This harness deliberately applies the
runtime gate only to the fixed argv it drives, each measured in the pinned 2.1.270 capture and
documented on the linked CLI and headless references. If a later build omits one of those required
flags from `--help`, opening refuses it even when the published reference still lists the flag.
The conservative limitation avoids treating a changed help grammar as proof that an untested child
will accept every argument. It can be relaxed only with a documented feature-detection surface or
a new pinned capture that demonstrates the exact argv; an arbitrary version-based fallback would
send an unverified flag after a user submitted a turn.

Two later builds change behaviour without changing what this harness may pass, so they are
recorded rather than gated on:

- **2.1.219** — nested subagent forwarding. Below it, `--forward-subagent-text` emits one level, so
  a depth-2 subagent's output is attributed to the wrong parent or dropped.
- **2.1.223** — `--resume <id>` stops being scoped to the project directory the session was created
  in. Documented only in the headless guide's own version note; the changelog entry for that
  release does not mention it, and
  [sessions.md](https://code.claude.com/docs/en/sessions.md) still describes the old behaviour
  unqualified. This harness passes the same working directory on resume either way, so the
  inconsistency cannot bite — but it is a live docs contradiction worth re-checking.

## Environment

Two variables survive the allowlist beyond the base list: `CLAUDE_CONFIG_DIR` and
`CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` (see `pinned::VENDOR_ENVIRONMENT_KEYS`). The document for the
first is [Claude Code environment variables](https://code.claude.com/docs/en/env-vars), read
2026-10-13: "Override the configuration directory (default: `~/.claude`). All settings, session
history, and plugins are stored under this path." `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN` and
`CLAUDE_CODE_OAUTH_TOKEN` are deliberately absent.

The base allowlist reaches this CLI too: `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`,
`XDG_CACHE_HOME`, `APPDATA` and `LOCALAPPDATA`, beside `HOME`. Claude Code's documentation names
none of them. An observation on the installed `2.1.296` binary
(`strings -a <claude> | grep -o -E '.{100}XDG_CONFIG_HOME.{140}'`) shows it reading
`XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `XDG_CACHE_HOME` and `XDG_STATE_HOME` in a per-application
directory resolver, `XDG_CONFIG_HOME` for git's global ignore file (`git/ignore`), and naming them in
lists of environment variables whose purpose was not determined. Which account a run uses is
decided by `CLAUDE_CONFIG_DIR` or `~/.claude`, per the document above; whether the resolver feeds
anything the harness depends on was not determined.

## Fixture capture

`mea capture --harness claude` runs only `claude --version` and `claude --help`. It writes the raw
help text, a parsed CLI surface, and a version record under `fixtures/claude/contract/`. The command
does not read `claude auth status`, open a session, or send a prompt, so the pinned drift job can
reproduce the directory without a login.

`fixtures/claude/historical/contract/auth-status.json`, the versioned files in
`fixtures/claude/help/`, and `fixtures/claude/transcripts/` are historical captures. They cover an
older CLI surface and authenticated turns that cannot be reproduced byte-for-byte. Keep them fixed;
do not regenerate them during a routine drift check. [Fixture rules](../fixtures/README.md) record
the distinction.

## The surface driven

Every turn:

```
claude --print
       --input-format stream-json --output-format stream-json
       --verbose --include-partial-messages
       --forward-subagent-text
       [--permission-mode <mode>]
       [--permission-prompts none]
       (--session-id <uuid> | --resume <uuid>)
       [--model <id>] [--effort <level>] [--mcp-config <path>]
```

- **`--print` with both stream-json formats** is the documented headless interface
  ([headless.md](https://code.claude.com/docs/en/headless.md)).
- **`--verbose --include-partial-messages`** are both needed for token-level deltas. Without them
  the stream arrives in whole messages and nothing renders until each block is complete.
- **The prompt is never in argv.** It is written as one `{"type":"user",…}` message on stdin, which
  is then closed. argv is world-readable in `ps` on every platform this runs on.
- **Prompt input is bounded.** The write and stdin close share `Limits::request_timeout`. A dropped
  stream, host shutdown, `Session::cancel` or `Session::close` stops the child through the turn's
  one teardown owner instead of waiting for a blocked host sink. This follows Claude's documented
  stream-json stdin input and EOF-driven prompt boundary ([headless.md](https://code.claude.com/docs/en/headless.md)).
- **Omitted permissions preserve the user's CLI profile.** No permission mode or prompt override
  is passed until the host selects permissions. Claude's single mode flag requires both axes on
  the first selection; subsequent partial updates inherit the other axis. Accepted settings persist
  in `Session::snapshot().configuration.accepted` and are repeated on later batch invocations.
  Permission settings stay out of `observed`: a flag this harness put on a command line is a flag
  it encoded, not a setting the vendor reported it is running under. `system/init` does report a
  model for a live run, so that one value is published in
  `Session::snapshot().configuration.observed.model` ([headless.md](https://code.claude.com/docs/en/headless.md)).
- **`--permission-prompts none`** accompanies an explicit permission mode only on a build that
  declares it (2.1.259+). It is
  pinning, not a fix: the vendor's current default is `host`, and this harness is not an answering
  host, so a build whose default later *waits* would park every approval-needing turn until the
  idle timeout. `host` is never passed.
- **`--dangerously-skip-permissions`** and `--allow-dangerously-skip-permissions` are never passed.
  Full access uses `--permission-mode bypassPermissions`, the documented flag value.
- **`--bare`** is not passed. It skips hooks, MCP, CLAUDE.md and, decisively, never reads OAuth or
  the keychain — it authenticates only from `ANTHROPIC_API_KEY` or an `apiKeyHelper`. For the
  subscription sign-in this harness hosts, determinism and authentication are mutually exclusive,
  and this takes authentication. **Drift watch:** the vendor states "`--bare` is the recommended
  mode for scripted and SDK calls, and will become the default for `-p` in a future release"
  ([headless.md](https://code.claude.com/docs/en/headless.md)). When that lands, this harness must
  pin the non-bare behaviour explicitly.

## Session model

`claude --print` is a batch invocation: it reads a prompt, runs a turn and exits. Continuity is a
session id on disk, not a pipe staying open — so a session owns **no process**. It owns a session
id, and each turn spawns, streams and reaps its own child.

- **Opening starts nothing.** The id is minted as a UUID (`--session-id` "must be a valid UUID") so
  a host holds a resumable handle before any tokens are spent. The first turn passes
  `--session-id`; every later one passes `--resume`, using whatever handle the run's own
  `system/init` reported.
- **`Session::ids()` and `Session::snapshot().ids` are the handle in force.** A run may report a
  `session_id` other than the one `--session-id` proposed, and from then on that is the only handle
  `--resume` accepts — so it is the one a host persists. The opening snapshot remains available to
  the host that retained it when the session was opened.
- **Strict resume is lazy and confirmed.** Opening does not launch a process, so the requested UUID
  remains an unverified candidate in `Session::ids()` while `SessionSnapshot::resumed` is false.
  The first turn passes it to `--resume`; only `system/init` echoing that exact UUID confirms the
  resumed snapshot. A missing, invalid or different id ends that turn with a failure instead
  of presenting another conversation as the requested history. It is `claude-stream-broken`
  carrying a protocol message; an errored `result` the CLI prints before any `system/init` adds the
  result's subtype as the failure's `vendor_code` (see
  [A strict resume the CLI refuses](#a-strict-resume-the-cli-refuses)). Conversation content or a result
  before that confirmation is refused without emitting it. A later turn is held to the same handle.
  When its `system/init` supplies an id, the [documented `--resume <uuid>` form](https://code.claude.com/docs/en/headless.md)
  requires it to be UUID-shaped and this harness requires it to match the persisted handle. A
  missing id remains tolerated because it makes no competing identity assertion. A malformed or
  different id fails that turn rather than replacing the handle a host persisted — see
  [A later turn may not answer from another conversation](#the-event-stream). Only the *first*
  turn's echo may replace the minted id, because that turn proposes one rather than naming an
  existing conversation.

  A failed strict verification, including a stream that ends before `system/init` without a host
  cancellation, leaves the session nonresumable even when native cleanup finishes through the
  host's graceful-interrupt path. Graceful cleanup establishes that the child exited; it cannot
  establish that the child had loaded the requested history. The next turn is refused before a new
  Claude process starts.

  **Fallback is explicitly unsupported.** The documented headless surface supplies no separate
  resume operation or conclusive cannot-resume result. Retrying any failed first turn under a new
  UUID could turn an authentication, transport or acceptance-unknown failure into a silent fresh
  chat. `ResumeMode::Fallback` therefore returns `Error::HostConfiguration` before probes, scratch
  files or a child process are created. It can be implemented only when the vendor supplies a
  recognized signal that safely authorizes a new conversation.

  The *shape* is checked, and that is a different question. A resume reference goes on the command
  line as `--resume <value>`, and an argv array stops shell injection but not **argument**
  injection: a stored handle beginning with `-` is read by the CLI's parser as a flag of its own,
  which is how a forgotten reference could put `--dangerously-skip-permissions` on a turn. The
  vendor documents the handle as a UUID (`--session-id` "must be a valid UUID") and echoes it back
  verbatim, so there is a published shape to check rather than a guess to accommodate. A reference
  that is not one is refused at `open_session` with `Error::HostConfiguration`, and an `init`
  record that echoes back a handle of some other shape is not followed — the minted id stays in
  force. The same argument-position rule applies to an explicit `--model` value.

  <https://code.claude.com/docs/en/cli-reference.md>
- **A second `start_turn` is refused while one is active.** Admission claims the session's sole
  active-turn slot before a child starts. A concurrent caller receives `Error::Busy` before any
  second Claude invocation is launched; it must explicitly cancel the active turn and wait for
  its terminal state before retrying.
- **`start_turn` and `close` share the core lifecycle gate.** It covers the synchronous child
  reservation and teardown claim, then releases before every await. Close therefore either takes the
  reservation or makes the starting call reap the child it launched, rather than leaving a process
  with no session authority.
- **Steering is not supported.** `--input-format stream-json` accepts a second message, but it runs
  as its own turn with its own `result` — a queued follow-up, not same-turn steering.
- **The turn's own kill pre-empts no background subagent.** A run waits for background subagents and
  workflows *before* it prints `result`, because their output is part of the final answer, capped by
  `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS` (ten minutes by default since 2.1.182) — which is why that
  variable is on the environment allowlist and why it is an operator's to lower. The kill lands
  after the terminal event, so what it ends is a background *Bash* task the run left running, which
  the vendor would otherwise terminate about five seconds later itself.

  <https://code.claude.com/docs/en/headless.md>

**A later turn may not answer from another conversation.** Every turn after the first is spawned
with `--resume <handle>`. When `system/init` supplies a handle, it must be a UUID and equal that
handle. A missing field leaves the persisted handle unchanged, but a malformed or different handle
means the CLI may have answered from history this session has never seen. That turn fails with
`Error::Protocol` and the handle the host persisted stays in force; adopting a supplied replacement
would switch to a conversation nobody read, silently and with no event to notice. The first turn
stays lenient on purpose: it *proposes* a handle with `--session-id`, and a CLI that started a
different conversation anyway has made that one the real one, so following its id is the better
guess. A strict resume is stricter still — see [Session model](#session-model).

## Cancellation, and what the vendor actually does

`Session::cancel` records a reason and asks the host launcher to interrupt the child first. The
host supplies the OS-specific interrupt and containment policy. If the child does not exit during
the host-configured grace period, the launcher escalates to process-tree termination and reaps it.
The turn pump writes `Cancelled { reason }` followed by `Completed`; an unread transcript cannot
block that control-plane cleanup. A cancel, close, host shutdown, dropped stream and native
completion all join the same per-turn teardown. Exit 143 is read as a clean stop rather than a
failure: putting an error in the transcript for something the user asked for is worse than saying
nothing.

Close waits for a pending launch and for native cleanup before reporting success, each under a
bound: see [Deadlines](#deadlines). If native cleanup fails, it returns the failure as
`Error::CleanupRequired` carrying the host's `ProcessControl`, keeps the
session `Closing`, and preserves the MCP artifact in the host's scratch directory because the child
may still read it. The host reclaims that artifact after independently establishing that no child
remains. If native cleanup succeeds but artifact removal fails or does not finish in time, the
session is `Closed` and close reports the removal error.

**The vendor's own turn is left unfinished.** This is a real asymmetry and it is the vendor's
documented behaviour, not this harness's choice:

> If you stop a `claude -p` run with SIGTERM, for example with `kill` or from a process
> supervisor, Claude Code exits with code 143. Claude Code leaves the turn that was in progress
> unfinished and records no result for it. To end the turn instead, send SIGINT … When you resume
> the session, Claude Code leaves the interrupted turn as it is, and your next prompt drives the
> conversation. To have Claude Code continue the interrupted turn on resume instead, set
> `CLAUDE_CODE_RESUME_INTERRUPTED_TURN=1`.
> — [headless.md](https://code.claude.com/docs/en/headless.md)

An earlier revision of that page said the opposite default ("Claude Code continues the turn that
SIGTERM left unfinished"), and this harness was written against it. The default has since changed,
and the opt-in variable can still be turned on in the vendor's own configuration, which this
library neither reads nor controls. The harness therefore keeps the conservative reading: it cannot
prove that a killed conversation will not resume work the user cancelled.

After forced termination the harness marks the session nonresumable and refuses another turn with
the recorded cancellation reason. It never silently mints a replacement native conversation for a
strict session, and it never resumes the killed prompt. SIGINT is what the vendor documents as
ending a turn cleanly. A launcher reports that graceful interruption separately from forced
termination, which preserves the existing native continuation.

### Resumability after a stop

`Session::cancel` returns `Ok(())` for a clean interrupt and for a kill alike, and the
`Cancelled` marker carries only the host's reason. How the conversation stands is on the snapshot
instead: `SessionSnapshot::resumability`, a non-exhaustive `Resumability`. A `SessionSubscription`
wakes for every change.

| Value            | Meaning for Claude                                                                                                                             | Strict resume          |
| ---------------- | ---------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------- |
| `Unknown`        | No `system/init` yet, a strict resume no turn has confirmed, or a stop is pending. No evidence either way.                                     | decide on other facts  |
| `Resumable`      | `system/init` proved the conversation exists, and every turn since finished with the vendor's `result` or was stopped by a graceful interrupt. | safe                   |
| `UnfinishedTurn` | A turn ended with no `result` and no clean interrupt: the child was force-terminated, died, went silent, or a stop did not complete.           | do not; open a new one |
| `Unverified`     | A strict resume was not confirmed: another handle, or the CLI ended before `system/init`.                                                      | do not; open a new one |

Only a turn the vendor finished leaves the conversation `Resumable`, whatever the host's control
later answers about the child. A tainted value is permanent for the session, and the first cause
wins. It is wider than the refusal `start_turn` gives, which is unchanged: a kill or an unconfirmed
resume refuses later turns with `Error::Cancelled`, while a child that died or a silent stream
publishes `UnfinishedTurn` and leaves the session usable.

How each ending reads. "Terminal" is the snapshot read on the turn's terminal event; "stop
returned" is after `cancel` (or `close`) returns; "closed" is after `close`. The terminal of a
cancelled turn is written at once, before the stop finishes (see [Deadlines](#deadlines)), so
while a stop is pending the value is `Unknown`, never a `Resumable` the stop may withdraw.

| Ending                                                                | Terminal                      | Stop returned    | Closed           |
| --------------------------------------------------------------------- | ----------------------------- | ---------------- | ---------------- |
| Turn finished with `result` (success or failure)                      | `Resumable`                   | n/a              | `Resumable`      |
| Child died mid-turn after `system/init` (exit 143, 1, 0)              | `UnfinishedTurn`              | n/a              | `UnfinishedTurn` |
| Idle timeout or unreadable stream after `system/init`                 | `UnfinishedTurn`              | n/a              | `UnfinishedTurn` |
| Child died before any `system/init` (control with graceful interrupt) | `Unknown`                     | n/a              | `Unknown`        |
| `cancel` that interrupted gracefully                                  | `Unknown` or `Resumable`      | `Resumable`      | `Resumable`      |
| `cancel` that had to kill                                             | `Unknown` or `UnfinishedTurn` | `UnfinishedTurn` | `UnfinishedTurn` |
| `close`, host `CancelToken`, dropped stream, dropped session          | as `cancel`                   | as `cancel`      | as `cancel`      |
| Stop that failed (`CleanupRequired`)                                  | `Unknown` or `UnfinishedTurn` | `UnfinishedTurn` | `UnfinishedTurn` |
| Launch that failed over a child it could not end                      | n/a                           | `UnfinishedTurn` | `UnfinishedTurn` |
| Strict resume the vendor did not confirm                              | `Unverified`                  | n/a              | `Unverified`     |

A host stores the value beside the vendor handle whenever it changes, and strict-resumes a stored
conversation only when it reads `Resumable`. `Resumability::allows_strict_resume` is that test.

**`Resumable` describes a conversation at rest.** It turns true when `system/init` arrives, while
the first turn is still running. A host that loses the session mid-turn (a crash, a dropped
connection) never sees a stop recorded, and the child may have died without one. If the host's own
`RecoveryRecord` shows a turn with no terminal when the session was lost, treat the conversation as
`UnfinishedTurn` whatever value was stored.

```rust,ignore
session.cancel(CancelReason::Requested).await?;
let stored = session.snapshot().resumability;
database.save(session.ids().native_session_id, stored); // serde: "unfinished-turn", ...
```

**Codex and ACP sessions report `Unknown` always.** For a host, `Unknown` there is "this library
has no verdict", not "unsafe": Codex resumes a thread by id however a turn ended, and an ACP agent
decides per profile whether it can load a session. Attempt the resume with the agent's own
operation and handle its refusal; see [the Codex guide](harness-codex.md#sessions-turns-and-steering)
and [the ACP guide](harness-acp.md#sessions).

### A strict resume the CLI refuses

A strict resume is lazy, so a conversation the vendor no longer has surfaces only on the first
turn. What `claude` 2.1.296 does for `claude -p --resume <unknown uuid> --input-format stream-json
--output-format stream-json --verbose`, run with an empty `CLAUDE_CONFIG_DIR` so nothing is signed
in and no model is reached:

- it fails **before authentication**: the same invocation against a conversation that exists
  prints `system/init` and fails later at the API, so the missing-conversation check comes first;
- stderr is `No conversation found with session ID: <uuid>`, the exit code is 1, and stdout is one
  record: `{"type":"result","subtype":"error_during_execution","is_error":true,"num_turns":0,…,
  "errors":["No conversation found with session ID: <uuid>"]}` with zero usage and **no
  `system/init`**.

The vendor documents the message and the exit code
([errors.md](https://code.claude.com/docs/en/errors.md), "No conversation found with the session
ID"), and that `system/init` is the first event and an in-run failure is printed as the `result`
([headless.md](https://code.claude.com/docs/en/headless.md)). It documents **no field** that
says the conversation is missing: `error_during_execution` is the generic subtype, exit 1 is the
generic failure code, and the only specific part is prose. The same page documents other resume
failures (`Failed to resume the conversation`, an unreadable transcript) with the same shape. So the
harness never matches the wording and does not claim "not found".

What it reads is the shape, and the shape proves little. An errored `result` while a strict resume
is unconfirmed means the run ended with a failure before it announced the session. It does **not**
prove the conversation is gone: hooks and plugin events can precede `system/init`, and an in-run
failure such as missing authentication prints as a `result` the same way. The harness does not
rename the failure. The turn ends as before, `claude-stream-broken` with the same protocol message,
so a host matching that code for this case keeps matching. Two facts are added:

- `SessionSnapshot::resumability` becomes `Unverified`, and the next `start_turn` is refused as it
  always was;
- the failure's `vendor_code` is the result's subtype (`error_during_execution`) when it is one the
  harness names, and absent for a result that is not an error or for content before `system/init`.
  The vendor's own `errors` text is not carried.

What a host does:

1. After a failed first strict-resume turn, read `resumability` and the failure's `vendor_code`.
   `Unverified` with a `vendor_code` means the CLI ended the run before announcing the session. It
   is not a verdict on the stored conversation: do not delete it on this signal alone, and do not
   record it as unresumable without knowing why. Show the failure to a person.
2. Opening a new session and resending is a product decision, not one the library authorises.
3. **Replay safety is not proven.** The prompt was already written to the child's stdin before the
   CLI answered, and the terminal carries no `Dispatch`. The CLI's zero usage and `num_turns: 0`
   are the vendor's own report. The host's `RecoveryRecord` for the turn still decides replay.

Not captured with `mea capture`: that command records `--version` and `--help` for Claude and runs
no turn, so there is no fixture for this record. The tests replay the observed line through the
fake CLI.

Closing stdin is also what the vendor documents as cancelling a pending prompt, and this harness
closes it immediately after the prompt — so a run that would otherwise wait for an answer nobody
can give does not wait.

**A turn that fails does not wait on the process to agree.** When the stream ends without a
`result`, the exit status is worth having: 143 is the one code the vendor documents, and naming it
beats reporting an unexplained failure. But two of the three ways to reach that point — a link that
broke mid-run, and the idle timeout — leave a child that is alive and has no reason to exit, so the
wait is bounded by the host's own `Limits::kill_grace` and the turn ends either way. The child is
killed after, not before: killing first would make every broken link exit 143 and read as an
outside interruption.

**A child that exits ends its turn even if its stdout stays open.** A pipe closes when its last
writer does. A helper that inherited the descriptor, such as a background Bash task the run left
behind, holds the read open after the Claude process is gone. The pump watches
`ProcessControl::wait` beside the read. When the exit arrives it keeps reading for one
`Limits::kill_grace`, counted from the exit, because what the child wrote before it left is still
in the pipe and can be the `result`. Then the turn ends as it does at end of stream, with the exit
status in its error, and the stop that follows ends the helper. A line the read already has is always taken. When the grace runs out the pump yields to the
runtime once and asks the read one more time, because a pipe's read is ready only after the
runtime's I/O driver has reported the bytes. That is what a `kill_grace` of zero comes to: no wait,
and one more pass. It covers bytes the driver reports in that pass, which Tokio's driver does for
bytes already in the kernel but does not promise, so a host that must not lose the tail of a run
leaves the grace above zero. A host control that fails to report an exit leaves the read to the
idle timer.

**The process the control waits on has to be the one that writes.** Since the exit starts the
grace, a launcher whose leader exits while the real CLI keeps running would cut every turn one
`kill_grace` after that exit. No launch path of the core `TokioLauncher` has such a leader:

- On Unix it runs the program directly. The native install is the `claude` binary. The npm
  install is a script with a `node` shebang, which the kernel runs in the same process. A shell
  wrapper that calls `claude` as a foreground command, with or without `exec`, stays until it
  returns.
- On Windows it runs `claude.exe` when the native install put one on `PATH`. When there is none,
  as with the npm install's `claude.ps1`, it runs `powershell.exe -File` on that script, and
  PowerShell stays until the `node` it started exits. A host that pins a `.cmd` shim gets
  `cmd.exe`, which waits the same way.

Those are read from the launcher and from the shape of the shims, and no test starts a real CLI. A
host launcher, or a wrapper that starts the CLI in the background and returns, has to make
`ProcessControl::wait` answer for the process that owns stdout.

### Deadlines

The [headless documentation](https://code.claude.com/docs/en/headless.md) names SIGINT, SIGTERM and
exit code 143, and gives no deadline for any of them. Every stage is bounded by the host's
`Limits`:

| Stage                                     | Bound                                   | On expiry                                                                                                 |
| ----------------------------------------- | --------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| Prompt write and stdin close              | `request_timeout`, for the two together | The turn fails as `claude-stream-broken`; the exit wait and a stop follow                                 |
| Silent stream                             | `idle_timeout`, floored at ten minutes  | The same                                                                                                  |
| Reading on after the child exited         | `kill_grace`, from the exit             | The read ends as if the stream had                                                                        |
| Exit wait before a failure's terminal     | `kill_grace`                            | The terminal is written with no exit status                                                               |
| Stop: the interrupt call                  | `kill_grace`                            | Taken as not delivered; the kill is next                                                                  |
| Stop: exit after a delivered interrupt    | `kill_grace`                            | The kill is next                                                                                          |
| Stop: the kill                            | `shutdown_timeout`                      | `Error::CleanupRequired`, cause `Error::Timeout` naming `process-tree termination`; no reap is waited for |
| Stop: the reap                            | `shutdown_timeout`                      | `Error::CleanupRequired`, cause `Error::Timeout` naming `process reaping`                                 |
| Close: a launch still inside the launcher | `shutdown_timeout`, from the close      | `Error::Timeout` naming `a pending Claude launch`; the session is `Closing`                               |
| Close: MCP artifact removal               | `shutdown_timeout`, after the stop      | `Error::Timeout` naming `MCP configuration removal`; the session is `Closed`                              |

The four stop stages are core's `stop_process_with_limits`, which every Claude stop goes through.
A launcher that reports no graceful interrupt skips both `kill_grace` stages. The interrupt and
the kill are the host's `ProcessControl`, so a stage can also end early with that control's own
error.

The stages add up. With `st` for `shutdown_timeout` and `kg` for `kill_grace`, the worst cases are
below. The first value column holds for any launcher on any platform. The other two are what the
core `TokioLauncher` comes to when it was built `with_limits` from the same `Limits`, and they are
smaller for two reasons. Its interrupt call returns at once. Its kill answers only when the whole
tree is gone, so the reap that follows has nothing left to wait for. On Unix that leaves `kg` for
the SIGINT, then a kill that sends SIGTERM, waits `kg`, sends SIGKILL and waits `kg` again. That
last wait is a ceiling: SIGKILL cannot be declined, so the tree is gone at once and a child that
ignores both signals takes about 2 `kg`, 4 s at the defaults. On
Windows it reports no graceful interrupt, and its kill gives the Job 2 `kg` to empty. Either kill
is cut at `st` if that is the smaller.

| Path                                         | Worst case                 | At the defaults | `TokioLauncher`, Unix | `TokioLauncher`, Windows | `Deadlines` method           |
| -------------------------------------------- | -------------------------- | --------------- | --------------------- | ------------------------ | ---------------------------- |
| Stop `S`                                     | `kg` + `kg` + `st` + `st`  | 14 s            | 6 s                   | 4 s                      | `process_stop`, `teardown`   |
| `cancel`, to the turn's terminal             | none                       | at once         | at once               | at once                  | `cancel_to_terminal`         |
| `cancel`, to its return                      | `S`                        | 14 s            | 6 s                   | 4 s                      | `cancel_to_return`           |
| `cancel` while the launch is pending         | none                       | at once         | at once               | at once                  |                              |
| `close`, no turn                             | `st`                       | 5 s             | 5 s                   | 5 s                      | `close_no_turn`              |
| `close` with a turn active                   | `S` + `st`                 | 19 s            | 11 s                  | 9 s                      | `close_with_turn`            |
| `close` while the launch is pending          | `st`, or `st` + `S` + `st` | 5 s, or 24 s    | 5 s, or 16 s          | 5 s, or 14 s             | `close_pending_launch`       |
| Any `close`, the largest of the three above  | `st` + `S` + `st`          | 24 s            | 16 s                  | 14 s                     | `close_worst_case`, `close`  |
| A later `close` over a late child's stop     | `S`                        | 14 s            | 6 s                   | 4 s                      | `close_after_pending_launch` |
| Child exit or end of stream, to the terminal | `kg`                       | 2 s             | 2 s                   | 2 s                      | `exit_to_terminal`           |
| Child exit or end of stream, to reaped       | `kg` + `S`                 | 16 s            | 8 s                   | 6 s                      | `exit_to_reaped`             |
| Silent stream, to the terminal               | idle floor + `kg`          | 602 s           | 602 s                 | 602 s                    | `silent_stream_to_terminal`  |
| Host shutdown token, to the terminal         | `S`                        | 14 s            | 6 s                   | 4 s                      | `host_shutdown_to_terminal`  |
| Session dropped mid-turn, to reaped          | `S`                        | 14 s            | 6 s                   | 4 s                      | `dropped_session_to_reaped`  |

The method names are on `mango_agent_claude::deadlines::Deadlines`, built with
`Deadlines::from_limits(&limits)`. They give the first value column, the one that holds for any
launcher, as `Duration`s from the same `Limits`; the `TokioLauncher` columns are that launcher's
own and are not in the API. Every method counts from a stated moment, and the table's rows carry it
in their names: the call to `cancel` or `close`, the child's exit or the end of its stream, the
last record read from a silent child, the host's token being cancelled, the last session handle
being dropped. A host that puts a timeout around `close` uses `Deadlines::close_worst_case()`,
counted from the call to `close`; it is the largest of the three close rows. The silent-stream
figure reads the idle floor through the same function the pump does,
`pinned::stream_idle_timeout`, and the close waits and the exit grace go through
`deadlines::launch_wait`, `removal_bound` and `exit_grace`, so the figure and the wait cannot drift
apart. The stop is the core's `stop_process_with_limits`, which this crate cannot edit, so
`process_stop` repeats its formula and a test runs the core against a control that spends every
stage to keep the two equal.

Tests on a paused clock against a fake launcher drive these rows to their worst case, with a
child that answers one millisecond inside each bound, and assert the elapsed time falls between the
method's value less those milliseconds and the value itself: the stop `S`, both against the core
directly and through `cancel`, which also returns its terminal at once; `close` with a turn active,
which is the stop followed by a removal that stalls for `st`, 19 s at the defaults; `close` with
no turn; `close` while the launch is pending, to the end of the stop it runs for a child that
arrives one millisecond before the wait ends; a later `close` over that child's stop, which fails
after spending every stage; the host shutdown token; a dropped session; the `kg`
after an exit; the end of a stream to reaped; and the silent stream, at the defaults and at an
`idle_timeout` above the floor. They run on CI's Linux, macOS and Windows lanes. Three things are
not driven to the figure in the table:

- The last `st` of `close` while the launch is pending. The start that owns the launch holds its
  own lease on the artifact until its stop returns, so a close that learns of the reap first finds
  the artifact still held and removes nothing. In the tests the close returns after the wait and
  the stop alone, `st` + `S`, and 24 s stays as the ceiling.
- A child that exited, to reaped. The fake child answers the stop's waits at once, so the stop
  spends fewer stages. The end of a stream under a live child spends all of them and drives the
  same row.
- `cancel` while the launch is pending, which returns at once and has no bound to measure.

The two `TokioLauncher` columns are pinned by
real-process tests on the real clock, at a `kill_grace` of 500 ms on Unix and 1 s or 4 s on
Windows, in `crates/mango-external-agents/src/launcher/tokio_launcher/tests/stop_bound.rs`. Where a
stage must have been waited out the test asserts a lower bound for it, which a loaded runner cannot
break. The upper bounds carry slack and only catch a gross regression: the launcher's own error,
returned when its last wait expires, enforces the ceiling. On Unix, a child and a helper in its
group that ignore SIGINT and SIGTERM take at least 2 `kg`, end by SIGKILL and leave no process
group; a child that ignores only SIGINT takes at least 1 `kg` and ends by SIGTERM; a child that
exits on SIGINT reports `Interrupted` inside one `kg`. On Linux those measured 1.02 s, 523 ms and
1.1 ms. On Windows, a stop spends no `kg` stage, and a Job holding a leader and a helper is gone,
both processes with it, within 2 `kg`; that test runs on CI's Windows lane only.

What each path does, and what a host sees when a bound runs out:

- **`cancel`.** The pump writes `Cancelled { reason }` and the terminal as soon as the stop is
  recorded, without waiting for the process. `cancel` itself returns when the stop does. While the
  launch is still pending there is no child, so `cancel` returns `Ok` at once, and so does a second
  `cancel`, or a `cancel` after a `close` that gave up on the launch: none of them waits for the
  launcher. The `start_turn` that owns the launch stops the child when the launcher returns it and
  answers `Error::Cancelled`.
- **`close` with a turn active.** The stop, then the artifact removal. A stop that fails returns
  `Error::CleanupRequired` and leaves the session `Closing` and busy, and the artifact on disk. A removal that fails or runs out of
  time leaves the session `Closed`. A blocking removal cannot be cancelled, so it carries on
  on the runtime's blocking pool and the artifact goes when the host's mount answers. The wait
  has moved, not gone: dropping a Tokio `Runtime` waits for every blocking task it started, so a
  removal that never answers now holds up the host's shutdown where it used to hold up `close`. A
  host that sees this error ends its runtime with `Runtime::shutdown_timeout` or
  `Runtime::shutdown_background`.
- **`close` while the launch is pending.** The close waits one `shutdown_timeout` for the launcher.
  If the child arrives in that time and the `start_turn` that owns the launch has begun its stop,
  the close waits for that stop and then for the artifact removal, `close_pending_launch`, 24 s at
  the defaults. The start usually still holds its lease on the artifact when the stop returns, so
  the close has nothing to remove and the figure is a ceiling. If the child does not arrive, or
  arrives and its start is not run, the close fails after one `shutdown_timeout` and the session
  stays `Closing`. A later `close` looks at what became of the launch. If the late child's stop
  failed it returns that `Error::CleanupRequired`, waiting for the stop if it is still running,
  `close_after_pending_launch`, at most one stop. If the stop succeeded, or the launch failed and
  no child ever existed, nothing is left to stop: the session becomes `Closed` and that close, and
  every later one, returns `Ok`. If the launch is still in the launcher it repeats the timeout. The
  stop runs on a task of its own, but it is begun by the `start_turn` future when that
  is polled after the launcher returned, or by its drop. A `start_turn` future that is kept alive
  and never polled again begins no stop and leaves the child running, so the close takes the
  `shutdown_timeout` branch and the child stays until the future is polled or dropped. The close
  never waits longer than `close_pending_launch` for that reason. The start that owns the launch
  removes the artifact after a reap and leaves it on disk after a stop that failed, whether the
  `start_turn` caller is still waiting or dropped its call.
- **Child exit or end of stream.** The terminal is an `Error` event with code `claude-no-result`,
  `claude-terminated` for exit 143, or `claude-stream-broken` when the link itself failed. The
  stop runs after it. Nobody is waiting on that stop, so
  if it fails the next `start_turn` gets `Error::Busy`, and a later `cancel` or `close` returns the
  stop's failure as `Error::CleanupRequired`.
- **Host shutdown token.** The pump stops the child first and writes `Cancelled { Shutdown }` once
  the stop returns. A stop that failed is left for the next `cancel` or `close`, as
  `Error::CleanupRequired`.
- **Session dropped mid-turn.** `Drop` cannot wait, so the stop runs detached on the runtime that
  opened the session and its result, `Error::CleanupRequired` if the stop failed, goes nowhere a
  host can ask: the session is gone. If that runtime has shut down, no stop is sent.
  The session still releases the host's `ProcessControl`, and the child's fate is that control's
  own `Drop`. The core `TokioLauncher`'s control sends SIGKILL to the process group there on Unix.
  On Windows the launcher's reaper task owns the Job, and the Job closes, ending its members, when
  that task goes with its runtime. A host control with no such `Drop` leaves the child running.

A stop that did not prove the child gone returns `Error::CleanupRequired`, as the Codex and ACP
harnesses do for a `close`, so a host reconciles a leftover child with one rule whatever the
harness. That is a stage that ran out of time (`Error::Timeout` naming `process-tree termination`
or `process reaping`), an error from the host's `ProcessControl`, and a control that panicked (an
`Error::Link` whose peer is `the host's process control`). The error carries the host's own
`ProcessControl`, the one its launcher returned for that child, through `Error::cleanup_control()`,
and the failure of the stage as its cause, through `Error::cause()`. If the host's control answers
with a `CleanupRequired` of its own, Codex wraps it the same way, and ACP flattens it to text: the error still carries
the launcher's control, the host's answer is its `source`, and `cause()` reads through it to the
innermost error. The control that answer named is reachable only by walking
`std::error::Error::source`. On 0.6 that answer reached the caller unchanged, and
`cleanup_control()` returned the control it named. The error holds for `cancel`, `close`, the
`start_turn` that owns a late child, and every later call that reads the outcome of a stop nobody
waited on. A `start_turn` reports it under the `Accepted` dispatch annotation, so branch on
`cleanup_control()` and `cause()`, which read through that layer, rather than on the variant.

No stop failure comes back as a bare `Error::Timeout`. Two close failures have no control to
carry, and neither is a failed stop. A `close` that gave up on a launch still inside the launcher
(`a pending Claude launch`) has no child yet. The `start_turn` that owns that launch gets the
`CleanupRequired` if its stop then fails, and so does a later `close`, as described above. An
artifact removal that fails or runs out of time (`MCP configuration removal`) comes after a stop
that succeeded, on a session that is already `Closed`.

The first outcome of a teardown is kept, as it is in the Codex and ACP harnesses. Every later
`cancel` and `close` gets a copy of it, with the same control, and nothing retries: the library asks
a child to stop once, and a host that holds the control is the one that can ask again or reap the
tree itself. The turn's slot stays taken, so `start_turn` answers `Error::Busy` (`Error::Closed`
once a `close` has run), and the session stays `Closing` after a `close`. Ending the child through
the control does not reopen the session, because the library gets no signal that the host reaped
it, as in the other two harnesses: a host in that state reconciles the child through the control
the error carried and opens a new session.

Two differences from Codex and ACP follow from how a Claude session is built, which runs one child
per turn where they run one per session. After a failed shutdown, Codex releases the turn before
it stops the child, so a second `cancel` returns `Ok` there; here the turn's slot is still taken,
so a second `cancel` returns the kept failure. And a Codex session that began a shutdown refuses
`start_turn` with `Error::Closed`, where a Claude session answers `Error::Busy` until a `close`
has run. The cause stays typed, as it does in Codex, where ACP carries it as text. An ACP `cancel`
only queues `session/cancel`, so it has no stop to fail.

A stop runs on the runtime that opened the session. A `cancel` or `close` made from another
runtime, after that one shut down, cannot get the stop run: with a child installed it returns
`Error::CleanupRequired` with the host's control and an `Error::Link` naming the runtime as its
cause, and the child is still there for the host to reconcile. A `close` over a session with no
active turn has no child to stop: it returns that `Error::Link` alone and the session is `Closed`. One
over a launch that is still pending returns the same error and the session stays `Closing`, since
a child may yet arrive. A worker lost after the stop
already proved the child gone, inside the artifact removal for one, owes no cleanup: the waiter
gets the bare `Error::Link`, with no control, and a `close` leaves the session `Closed`.

A host that moved from 0.6 matched `Error::Timeout` on these calls. Read `cleanup_control()` first
and finish the stop through the control it returns, and read `cause()` for the stage that failed:
`Error::Timeout` is what `cause()` returns for a stage that ran out.

A launch future the library never saw complete, because the runtime it ran on went away, is the
launcher's own to clean up, as for a host that drops `ProcessLauncher::launch`: the session reads
it as a launch that produced no child, and a later `close` can reach `Closed`.

A launch that fails over a child its launcher could not end (a `CleanupRequired` out of the spawn)
is kept the same way: the `start_turn` returns it, the reservation stays taken, and every later
`cancel` and `close` repeats it, with the MCP artifact left on disk.

Not bounded by anything here:

- **The launcher's own `spawn`.** `start_turn` waits for it as long as the host's launcher takes.
  Only a close's wait for that launch has a bound.
- **A turn that keeps talking.** The idle timer measures silence. A run that prints a record at
  least every ten minutes can run as long as the vendor lets it.
- **The artifact removal on a path other than close.** A `start_turn` that a close raced can hold
  the last reference to the MCP artifact, and waits for its removal on the blocking pool before it
  returns.

`cancel_settle_timeout` and `approval_timeout` are not read by this harness. It has no protocol
stop to settle and no approval to wait for. `request_timeout` bounds only the prompt write.

## Permissions

Claude is the vendor where the product's two axes are **not** independent: one
`--permission-mode` flag mixes "what may run" with "who answers". Unrepresentable pairs come back
unsupported with a reason rather than being rounded to the nearest mode.

| Level       | Routing     | Mode                | Notes                                                |
| ----------- | ----------- | ------------------- | ---------------------------------------------------- |
| read-only   | user        | `plan`              | vendor id reported as `plan`                         |
| default     | user        | `manual`            | vendor id reported as `default`, the config spelling |
| full-access | user        | `bypassPermissions` |                                                      |
| default     | auto-review | `auto`              | only when the account qualifies — see below          |
| read-only   | auto-review | —                   | `plan` changes nothing, so nothing reviews           |
| full-access | auto-review | —                   | nothing left to review                               |

`dontAsk` is never selected. It points the opposite way from `auto` — pre-approved tools only, for
locked-down CI — and substituting it would silently narrow what a user asked to widen.

The static declaration includes `auto` as a possible mode. The probed matrix narrows it per
account: it needs a qualifying plan tier, and an
administrator can remove it with `disableAutoMode` in the platform's managed-settings document,
which makes the CLI reject `--permission-mode auto` *at startup* — indistinguishable from any other
startup failure. So discovery reads that document (policy, not a secret; only the literal
`"disable"` counts) and asks `auth status` which kind of account is in play. An account the probe
could not establish fails closed: an unsupported cell with a reason is recoverable by the person
reading it, a turn that dies at startup is not.

A mode this build's own `--permission-mode` does not list narrows that cell to
`RequiresNewerVersion` rather than refusing the harness.

## Approvals: none, and why

`Capabilities::interactive_approvals` is **false**. No `ApprovalRequested` is ever emitted, and
`Session::respond` returns `Error::NotSupported { capability: Capability::InteractiveApprovals }`.
`Session::answer` and `Session::configure` refuse the same way, under `Capability::Questions` and
`Capability::SessionConfiguration`: Claude Code's headless surface asks no typed questions, and its
model and permission-mode flags are argv on a fresh child rather than settings an open session can
be re-pointed at. `Discovery::configuration_catalog` is empty for the same reason — the CLI
publishes no settings surface to enumerate, which is a different statement from a catalog whose rows
are all unsupported.
A tool the permission mode refuses arrives as `system/permission_denied` followed by a
`tool_result` marked in error, and is rendered as one failed activity carrying the vendor's own
reason — the run continues and exits zero, so a refused tool is not a failed turn.

This is a measured verdict, and the measurement is worth recording because it is not obvious:

- `--sdk-url` is hard-gated. Against 2.1.270: `--sdk-url rejected: host "localhost" is not an
  approved Anthropic endpoint. This flag is reserved for Remote Control worker processes connecting
  to Anthropic's backend.` It is in no `--help` and on no docs page.
- `--permission-prompt-tool` is documented as taking **an MCP tool**
  ([cli-reference.md](https://code.claude.com/docs/en/cli-reference.md)). Serving one would make
  this library part of the authorisation path for an agent it does not own, needing request ids,
  replay protection, expiry and a threat model of its own.
- There **is** a third route, and this harness deliberately does not take it. Anthropic's own
  `@anthropic-ai/claude-agent-sdk` passes the literal sentinel `--permission-prompt-tool stdio`
  whenever a caller supplies a `canUseTool` callback, which switches `can_use_tool` onto
  `control_request` / `control_response` frames on the same NDJSON channel. That value appears in
  no `--help` output and on no documentation page — only in the SDK's shipped source — and it has a
  multi-version history of the CLI silently not emitting the request
  ([claude-code#34046](https://github.com/anthropics/claude-code/issues/34046), reported across
  2.1.6–2.1.123 and closed as stale rather than as fixed). This repository drives documented
  surfaces only, so the route is recorded here rather than taken. It becomes the obvious upgrade
  the day the vendor documents it.

## What discovery reports

Three probes, all documented, read-only and non-secret: `claude --version`, `claude --help` and
`claude auth status` ("Show authentication status as JSON. Use `--text` for human-readable output.
Exits with code 0 if logged in, 1 if not").

An accepted `DiscoveryReceipt` reuses its version and authentication answers when opening a
session, but re-runs `--help`. The receipt records normalized discovery facts, not the exact help
grammar needed to decide current safe argv such as permission modes, effort levels and MCP support;
inventing that grammar from a capability summary would risk passing an undeclared flag.
A receipt that says the CLI was not installed retains the no-CLI launch refusal without probing it.

What the receipt is worth, measured rather than claimed:

| Sequence                                       | Vendor boots            |
| ---------------------------------------------- | ----------------------- |
| `discover`                                     | 3                       |
| `open_session` with an accepted receipt        | 1                       |
| `open_session` with no receipt                 | 3                       |
| `open_session` with a stale or changed receipt | 0, plus a typed refusal |

So a host that discovers and then opens without vouching for the result pays **six** boots before
its first token, and one that passes a receipt pays four. `open_session` deliberately does not
cache: the harness cannot know the executable, the environment or the account did not move between
the two calls, and guessing is exactly what the receipt replaces. The numbers are asserted as exact
counts in `tests/harness_over_a_fake_cli.rs`, so probe creep fails a test rather than quietly
slowing a first token.

`auth status` returns more personal data than any other vendor's status call — `email`, `orgId`,
`orgName`, `projectsDirectory`, `subscriptionType`. **None of it leaves the parser.** Two facts do:
whether somebody is signed in, and whether the account is a subscription, an API key or a cloud
provider's credentials — the second because `auto` depends on it. `subscriptionType` is dropped
because `AuthMode` says *how* an account authenticates, and a plan tier is a different question
with no home in the core's vocabulary.

An unreadable answer is `Unknown`, never signed-out: Claude may keep credentials in the system
keychain, so only an explicit `loggedIn: false` is a signed-out verdict. A turn against one fails
with `Error::AuthRequired { login_hint: "claude auth login" }` — text for a person to run, which
this library never runs.

## Capabilities

| Capability                       | This harness | Why                                                                                                                                      |
| -------------------------------- | ------------ | ---------------------------------------------------------------------------------------------------------------------------------------- |
| `structured_streaming`           | yes          |                                                                                                                                          |
| `reasoning_stream`               | yes          | `thinking` blocks and their deltas                                                                                                       |
| `resume`                         | yes          | `--resume`                                                                                                                               |
| `cancellation`                   | yes          | with the caveat above                                                                                                                    |
| `usage_reporting`                | yes          | the `result` record's own counts                                                                                                         |
| `model_catalog`                  | per build    | the aliases `--model`'s description advertises                                                                                           |
| `mcp_passthrough`                | per build    | `--mcp-config`                                                                                                                           |
| `interactive_approvals`          | no           | see above                                                                                                                                |
| `steering`                       | no           | a second message is its own turn                                                                                                         |
| `session_listing`                | no           | the vendor's transcripts live under a path documented as subject to change; parsing it would be reading another company's private format |
| `images`                         | no           | a turn carrying attachments is refused rather than silently stripped                                                                     |
| `native_review`, `account_usage` | no           | no surface observed                                                                                                                      |

**Models.** There is no `models list` command and no handshake, so the aliases in `--model`'s own
description are the whole catalog. Nothing is marked default, deliberately: the help declares no
default, and naming one would put `--model` on every argv and override whatever default the account
is on. A build that advertises no aliases reports no catalog at all, which is not the same as an
empty one. An explicit model is an opaque vendor value: it is checked only for the documented argv
value shape, never rejected merely because it is absent from a partial alias catalog. An invalid
value is refused before a child starts rather than omitted or allowed to alter the CLI defaults.

**Effort.** `--effort` is passed only for the exact level this build itself printed, and only when
the host chose one. An explicit level that is unavailable after a downgrade is refused before a
child starts, so the turn never silently falls back to another effort or replaces accepted defaults.

## MCP passthrough

`OpenSession::mcp_servers` is written to a `mcpServers` JSON document and passed as
`--mcp-config <path>` ([mcp.md](https://code.claude.com/docs/en/mcp.md)). A file, never the
inline-string form the flag also accepts: inline would put `env` and `headers` on a command line
anybody can read. The host supplies an absolute scratch root that is visible at the same path to
the launched child. On Unix, every directory on the root's path must be owned by the
effective user or root, and a directory writable by group or other must be sticky, so no account
can replace the unique session leaf — or any name above it — before Claude reads it. The check
covers the ancestors because Claude opens `--mcp-config` by pathname: the child resolves every
name again, so a handle this process holds cannot protect that walk. Both the path as the host
wrote it and its canonical form are checked, because a symlink makes them two different chains: a
shared directory holding a symlink into a private tree is on the walk the child performs even
though canonicalisation resolves it away. A symlink component is judged by its owner, not by its own
`lrwxrwxrwx` mode; the directory that holds it is checked as its own ancestor, so a symlink a
third party owns is refused however sticky that directory is. The host remains responsible for ACLs and child mount mapping.
The harness creates one unique leaf below it with owner-only Unix permissions, or the host root's
inherited Windows ACL, and removes that leaf when the session is closed or dropped. It never
creates, changes or falls back outside the host root. A missing or unusable root refuses opening
before a Claude probe starts — including a root whose finished leaf path could not occupy a
`--mcp-config` value, such as one holding a control character that every Unix filesystem accepts
and the CLI's own parser does not. That check runs before the leaf is created, so a session never
opens over a path whose every turn would fail.

A build that does not declare `--mcp-config` returns
`Error::NotSupported { capability: Capability::McpPassthrough }`; a transport kind this harness
does not map is refused as host configuration. Accepting either would run every turn without the
tools somebody configured and report success.

## The event stream

The reducer holds three properties, each with a fixture case behind it:

- **Text is delivered once.** `--include-partial-messages` carries the same output twice — as
  deltas and again as a completed block — so deltas are the source and a completed block
  contributes only the remainder nothing streamed for it. Buffers are matched by delivery channel,
  because Claude restates in text the plan it just reasoned through, and letting the wrong buffer
  claim the other's delivery silently truncates the reply. What a block delivered is kept only up
  to what one stdio line can carry once decoded (three times `Limits::line.max_line_bytes`, since
  repairing invalid UTF-8 can triple a line, and never more than `max_buffered_bytes`): a completed block cannot begin with more than that, so past it the text is
  dropped and the completed block is emitted whole, never refused.
- **Subagents stay the vendor's.** `--forward-subagent-text` tags a subagent's messages with
  `parent_tool_use_id`; those nest under the `Task` activity as accumulating detail, never
  promoted into the transcript, which would tell a user the host made a hand-off it did not make.
- **Unknown records are ignored, not fatal.** `system/status`, `system/thinking_tokens`,
  `system/api_retry` and `rate_limit_event` all appear on one live run, and the vocabulary keeps
  growing. Only a `result` ends the turn.
- **The end of a run closes what it opened.** A `thinking` block is opened by `content_block_start`
  and closed by `content_block_stop`
  ([headless streaming](https://code.claude.com/docs/en/headless)), and every recorded run sends
  the stop. A cancel, a process that dies, or a `result` that lands mid-thought never does, so the
  run's end emits the missing `ReasoningEnded` (and cancels open activities) ahead of the terminal;
  a stop that arrives afterwards produces nothing. Those closes go through `EventSink::emit_close`:
  when the turn's queue has no room for one it is dropped, and a close never replaces the turn's
  terminal: any terminal ends what is still open. The refusal is sticky, so the usage a `result`
  reports is dropped with it. A usage that is sent but does not fit (the closes fit, the usage
  does not) is ordinary payload overflow, and the turn ends as `stream-overflow`, exactly as it
  would for a delta.

**Structured content** is carried, not flattened, where a fixture evidences the shape. Every
activity the reducer starts sets `item_id` from the `tool_use` block's own `id` — Claude's dialect
draws no difference between a call and the transcript item it produced, so the two coincide. A
`Write` call's `file_path`/`content` become a single-file `ActivityContent::Diff`, with `kind` left
absent because `Write` also overwrites a file that already exists and nothing in the stream says
which of the two happened; that mapping and the item id are both pinned by
`fixtures/claude/transcripts/denied-write-turn.jsonl`. An `Edit` call becomes a single-file `Diff` too, and
there `kind` **is** stated — `Modified` — because a stated before is what distinguishes a
modification from a creation without guessing. Its two strings are deliberately **not** carried:
`old_text`/`new_text` are documented as the file's contents on either side of the change, which is
what an ACP diff block sends, and `old_string`/`new_string` are a *region* of a file. Misfiling one
as the other would make the same host code report that a two-thousand line file had been replaced
by one line for Claude while being right about ACP. The neutral contract has no field for a region,
so the region is omitted rather than misfiled. The key names come from a live headless run rather
than from a fixture, which is weaker evidence and is why a reducer test pins them: a rename upstream
fails there rather than as a row that quietly stops appearing. A closing `tool_result`'s own body becomes
`ActivityContent::Output` on the `ActivityResult`, independent of a held `system/permission_denied`
reason, which still wins the one-line `detail`.

`MultiEdit`, `NotebookEdit`, `TodoWrite` and `ExitPlanMode` are **not** mapped: no captured
transcript in this repo exercises them, and the pinned build's own `system/init.tools` list does not
even enumerate the last three. `TodoWrite` is the expensive one — it is where a plan's steps live,
so until it is captured a Claude plan reaches a host as an activity title rather than as
`ActivityContent::Plan`, which is the one place this harness is behind the other two. A `mea
capture` of a turn that drives these tools is what unblocks it; guessing their input keys from a
published tool description would produce a mapping no test in this repository could hold to
account.

**Slash commands** are published by provenance. A build that states `terminal_slash_commands` is
authoritative; one that does not publishes only the names whose origin the same record states — a
skill, a plugin's `plugin:command`, an MCP server's `mcp__*`. Announcing nothing is not announcing
an empty catalog: the last announcement wins wherever it lands, so an empty one would erase a real
catalog an earlier run published.

### Debug output

The raw `stream-json` records are carriers under the `Debug` policy in `docs/compliance.md`: a
`tool_result` body arrives in them, and a `Read` of a `.env` is that body. `StreamRecord` keeps the
whole line's JSON and `InitRecord`, `PermissionDenied`, `ContentBlock`, `StreamEvent`, `Delta` and
`ResultRecord` borrow from it, so each has a hand-written `Debug` that reports the record's `type`
and `subtype` (when they are a discriminator this harness knows by name, otherwise their size), how many members it has and
how many bytes they take, never a value. A debug-logged tool result reads
`ContentBlock { kind: Some("tool_result"), is_error: false, members: 3, size: <96 bytes redacted> }`.

`TurnReducer` follows the same rule. It holds the streamed text of each open block, the text a
subagent forwarded, each permission denial's message and the call ids that key them, so it reports
how many of each it holds and how many bytes, never the text or an id.

## Deliberate differences from the mangostudio TypeScript adapter

- The turn stream is a **bounded** channel; a host that stops reading applies backpressure to the
  vendor and, if it drops the stream, ends the child.
- Account **fingerprinting is gone**. The TypeScript adapter derived a keyed HMAC of the account
  email so a host could notice the account had changed. Nothing in the core carries it, and
  reconstructing it would mean reading the email this harness deliberately drops.
- **`--permission-prompts none`** is passed where the build declares it; the TypeScript adapter
  added this late and the reasoning is carried over intact.
- The **idle timeout** is host-configured and lives here rather than in a supervisor above, but the
  harness floors it at `pinned::STREAM_IDLE_TIMEOUT` (ten minutes). A Claude tool call legitimately
  runs for minutes with the vendor emitting nothing, so a shorter cap does not describe a stalled
  child — it cuts a working turn. A host that wants a longer leash sets `Limits::idle_timeout`
  above the floor and gets it.

Discovery exposes the account and build restrictions in `Discovery.permission_matrix`; the static
`Harness::permission_matrix` is its upper bound. Hosts can use the probed matrix to disable
unavailable choices before opening a session.
