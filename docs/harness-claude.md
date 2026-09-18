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
flags and vocabularies this harness depends on, and the pin only decides when that parse produced
nothing usable. A repackaged or backported build that has everything a turn passes stays usable;
an at-pin build that lost a flag is refused before a session opens. A `--help` that yielded
neither the permission modes nor any required flag reads as "the probe failed", never as "the
binary has no options".

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
- **Resume is vetted for shape, never verified for existence.** Verifying that a conversation is
  still there would cost a process launch per open, and a wrong guess is recoverable: a session the
  vendor has forgotten fails at the first turn with the vendor's own message. `ResumeMode::Fallback`
  therefore behaves exactly like `Strict`, and `SessionSnapshot::fallback_reason` is always `None` —
  nothing was verified, so nothing fell back. Implementing a real fallback means retrying a failed
  first turn under a fresh id, and that waits for a stable signal to key off (today the only one is
  vendor prose).

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

## Cancellation, and what the vendor actually does

`Session::cancel` records a reason and asks the host launcher to interrupt the child first. The
host supplies the OS-specific interrupt and containment policy. If the child does not exit during
the host-configured grace period, the launcher escalates to process-tree termination and reaps it.
The turn pump writes `Cancelled { reason }` followed by `Completed`; an unread transcript cannot
block that control-plane cleanup. A cancel, close, host shutdown, dropped stream and native
completion all join the same per-turn teardown. Exit 143 is read as a clean stop rather than a
failure: putting an error in the transcript for something the user asked for is worse than saying
nothing.

Close waits for a pending launch and for native cleanup before reporting success. If native cleanup
fails, it returns the error, keeps the session `Closing`, and preserves the MCP artifact in the
host's scratch directory because the child may still read it. The host reclaims that artifact after
independently establishing that no child remains. If native cleanup succeeds but artifact removal
fails, the session is `Closed` and close reports the removal error.

**The vendor's own turn is left unfinished.** This is a real asymmetry and it is the vendor's
documented behaviour, not this harness's choice:

> If you stop a `claude -p` run with SIGTERM … Claude Code exits with code 143. Claude Code leaves
> the turn that was in progress unfinished and records no result for it. … When you resume the
> session, Claude Code continues the turn that SIGTERM left unfinished.
> — [headless.md](https://code.claude.com/docs/en/headless.md)

After forced termination the harness marks the session nonresumable and refuses another turn with
the recorded cancellation reason. It never silently mints a replacement native conversation for a
strict session, and it never resumes the killed prompt. SIGINT is what the vendor documents as
ending a turn cleanly. A launcher reports that graceful interruption separately from forced
termination, which preserves the existing native continuation.

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
empty one. An explicit model must fit Claude's documented identifier shape and an argv value
position; an invalid value is refused before a child starts rather than omitted.

**Effort.** `--effort` is passed only for the exact level this build itself printed, and only when
the host chose one. An explicit level that is unavailable after a downgrade is refused before a
child starts, so the turn never silently falls back to another effort.

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
  claim the other's delivery silently truncates the reply.
- **Subagents stay the vendor's.** `--forward-subagent-text` tags a subagent's messages with
  `parent_tool_use_id`; those nest under the `Task` activity as accumulating detail, never
  promoted into the transcript, which would tell a user the host made a hand-off it did not make.
- **Unknown records are ignored, not fatal.** `system/status`, `system/thinking_tokens`,
  `system/api_retry` and `rate_limit_event` all appear on one live run, and the vocabulary keeps
  growing. Only a `result` ends the turn.

**Slash commands** are published by provenance. A build that states `terminal_slash_commands` is
authoritative; one that does not publishes only the names whose origin the same record states — a
skill, a plugin's `plugin:command`, an MCP server's `mcp__*`. Announcing nothing is not announcing
an empty catalog: the last announcement wins wherever it lands, so an empty one would erase a real
catalog an earlier run published.

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

## Known gaps

- `GateVerdict::VersionTooOld` carries the version and the floor but has nowhere to name *which*
  flag went missing, so a build refused for a missing flag reports an upgrade rather than the
  specific cause. The fixture-backed surface test is what names it for a maintainer.
- A real `ResumeMode::Fallback` is recorded above as a follow-up.

Discovery exposes the account and build restrictions in `Discovery.permission_matrix`; the static
`Harness::permission_matrix` is its upper bound. Hosts can use the probed matrix to disable
unavailable choices before opening a session.
