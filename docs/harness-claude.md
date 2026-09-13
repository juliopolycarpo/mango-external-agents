# Claude Code harness (`mango-agent-claude`)

Drives the `claude` CLI a user already installed, through the headless surface Anthropic
documents, over one child process per turn. It never logs in, never reads a credential and never
downloads a binary.

Facts below were read on 2026-09-13 against `claude` **2.1.270** and the vendor's own pages.
Re-verify before relying on them; `mea capture` re-captures the fixtures under `fixtures/claude/`.

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

## The surface driven

Every turn:

```
claude --print
       --input-format stream-json --output-format stream-json
       --verbose --include-partial-messages
       --forward-subagent-text
       --permission-mode <mode>
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
- **`--permission-prompts none`** is passed only on a build that declares it (2.1.259+). It is
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
- **Resume is vetted for shape, never verified for existence.** Verifying that a conversation is
  still there would cost a process launch per open, and a wrong guess is recoverable: a session the
  vendor has forgotten fails at the first turn with the vendor's own message. `ResumeMode::Fallback`
  therefore behaves exactly like `Strict`, and `SessionInfo::fallback_reason` is always `None` —
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
  force. Same rule, same reason, as `models::safe_model` for `--model`.

  <https://code.claude.com/docs/en/cli-reference.md>
- **A second `start_turn` ends the first.** A host that starts one has decided the first is over.
- **Steering is not supported.** `--input-format stream-json` accepts a second message, but it runs
  as its own turn with its own `result` — a queued follow-up, not same-turn steering.

## Cancellation, and what the vendor actually does

`Session::cancel` records a reason, kills the child through the host's launcher, and the turn's
pump writes `Cancelled { reason }` followed by `Completed`. Exit 143 is read as a clean stop rather
than a failure: putting an error in the transcript for something the user asked for is worse than
saying nothing.

**The vendor's own turn is left unfinished.** This is a real asymmetry and it is the vendor's
documented behaviour, not this harness's choice:

> If you stop a `claude -p` run with SIGTERM … Claude Code exits with code 143. Claude Code leaves
> the turn that was in progress unfinished and records no result for it. … When you resume the
> session, Claude Code continues the turn that SIGTERM left unfinished.
> — [headless.md](https://code.claude.com/docs/en/headless.md)

So a cancelled turn's work may resume on the *next* turn of the same session. SIGINT is what the
vendor documents as ending a turn cleanly, and the library cannot ask for it: `ProcessControl::kill`
is the host's port, and which signal "end it" means is the launcher's decision. A host that wants
the vendor's clean-cancel semantics implements that in its own launcher. A core-owned way to ask
for an interrupt rather than a kill is the obvious follow-up.

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

**`auto` is resolved per account, not declared.** It needs a qualifying plan tier, and an
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
`Session::respond` returns a typed `claude-approvals-unsupported` refusal. A tool the permission
mode refuses arrives as `system/permission_denied` followed by a `tool_result` marked in error, and
is rendered as one failed activity carrying the vendor's own reason — the run continues and exits
zero, so a refused tool is not a failed turn.

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
empty one.

**Effort.** `--effort` is passed only for a level this build itself printed, and only when the host
chose one — which is what keeps a stored per-chat setting from breaking a downgrade.

## MCP passthrough

`OpenSession::mcp_servers` is written to a `mcpServers` JSON document and passed as
`--mcp-config <path>` ([mcp.md](https://code.claude.com/docs/en/mcp.md)). A file, never the
inline-string form the flag also accepts: inline would put `env` and `headers` on a command line
anybody can read. The file goes in a directory of its own with an unguessable name, both owner-only
where the platform has permissions, and is removed when the session is closed or dropped.

A build that does not declare `--mcp-config`, or a transport kind this harness does not map, is
refused. Accepting either would run every turn without the tools somebody configured and report
success.

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
- The **idle timeout** (10 minutes of silence) lives here rather than in a supervisor above.

## Known gaps

- `GateVerdict::VersionTooOld` carries the version and the floor but has nowhere to name *which*
  flag went missing, so a build refused for a missing flag reports an upgrade rather than the
  specific cause. The fixture-backed surface test is what names it for a maintainer.
- A real `ResumeMode::Fallback`, and a core-owned way to ask a launcher for an interrupt rather
  than a kill, are both recorded above as follow-ups.
- **The probed permission matrix has nowhere to go.** `Harness::permission_matrix` is the harness's
  own declaration, answered before any probe, so it cannot know this account or this build — it
  refuses `auto` unconditionally. `Discovery` carries capabilities and models but no matrix, so the
  narrowing discovery actually performs (an account that does qualify for `auto`, a build missing a
  mode) is invisible until `open_session` refuses. Every cell and its reason are computed; a host
  simply cannot read them in time to grey a row out. Giving `Discovery` a matrix field is the
  obvious fix and is a product call about the core's shape rather than a Claude one.
