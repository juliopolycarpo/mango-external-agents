# The public contracts, and why each one is shaped the way it is

What a host is entitled to assume, which types are protected against future growth and which are
deliberately left open, and how the identifiers a host persists map onto the ones this library
speaks. Read it alongside [`adopt.md`](adopt.md), which is the walkthrough; this file is the
rationale and the compatibility surface.

## Identity: three facts, not one enum

| Fact              | Type             | Example                                                    | Who decides it                     |
| ----------------- | ---------------- | ---------------------------------------------------------- | ---------------------------------- |
| Registration name | `HarnessId`      | `claude`, `codex`, `acp:cursor`                            | the harness, persisted by the host |
| Wire dialect      | `ProtocolFamily` | `claude-code`, `codex-app-server`, `agent-client-protocol` | the harness                        |
| Execution profile | `ProfileId`      | `cursor`, `opencode`, `goose`, `custom`                    | the harness, per agent             |
| Carrier           | `TransportKind`  | `stdio`, `websocket`, `acp`                                | the **host**, per session          |

`HarnessIdentity` carries the first three. The carrier is the other axis and is chosen per session,
because the same dialect rides more than one of them.

All three identifiers are validated strings: 1–64 characters of ASCII lowercase, digits and
`-`, `_`, `.`, `:`, not beginning or ending with a separator and with no separator doubled. A
`ProfileId` is capped at 60 instead, so that `acp:` plus the profile still fits inside the same
ceiling — a profile that only fitted until it was prefixed would be a validated value producing an
unvalidated one.
They are map keys, log fields, and path components in a host's own storage, so a value that only
sometimes round-trips is worse than one that is refused. Construction and deserialization go
through the same rules, and a refusal names the offending value.

**Compatibility.** `HarnessId` serializes as the bare string, which is exactly what the previous
`HarnessKind` printed through `Display`:

| Previously                                      | Now                                               | Wire value     |
| ----------------------------------------------- | ------------------------------------------------- | -------------- |
| `HarnessKind::Claude`                           | `HarnessIdentity::claude()`                       | `"claude"`     |
| `HarnessKind::Codex`                            | `HarnessIdentity::codex()`                        | `"codex"`      |
| `HarnessKind::Acp(AcpProfileId::new("cursor"))` | `HarnessIdentity::acp(ProfileId::new("cursor")?)` | `"acp:cursor"` |

A host that persisted the old `Display` form reads it back unchanged. The JSON *shape* changed for
anyone who serialized the enum itself — `{"acp":"cursor"}` became `"acp:cursor"` — which is the
point: the new form is the one a host can store in a single column and hand back as a key.

A host with a native harness of its own builds `HarnessIdentity::custom(id, protocol, profile)` and
registers it in `HarnessRegistry` next to the built-in ones. Nothing in this crate needs an arm for
it, and there is no dynamic plugin loader, C ABI or bindings layer: a harness is a Rust type
implementing a Rust trait, compiled into the host.

## Capabilities: three tiers that only narrow

| Tier              | Type                     | Established by                                | May exceed               |
| ----------------- | ------------------------ | --------------------------------------------- | ------------------------ |
| Ceiling           | `CapabilityCeiling`      | the harness crate, before anything is spawned | —                        |
| Discovered        | `DiscoveredCapabilities` | one probe of one installed build              | never the ceiling        |
| Session-effective | `SessionCapabilities`    | one open session's handshake                  | never the discovered set |

They are three types rather than three values of one so the direction is a compile-time fact:
`DiscoveredCapabilities::clamped_to` takes a `CapabilityCeiling`, `SessionCapabilities::narrowed_to`
takes a `DiscoveredCapabilities`, and there is no way to pass them the other way round.

What a *probe* cannot see stays invisible until an open: a permission cell an account or an
administrator policy removes surfaces as a refusal from `open_session`, not as a narrower matrix.
`Discovery`'s own documentation says so, and `mea discover` labels its output accordingly.

## Session state versus turn transcript

Session state is read through `Session::snapshot` and watched through `Session::subscribe`. It
carries the two ids, the harness identity, the transport selection, the lifecycle status, the
session-effective capabilities, the configuration state, the configuration catalog, the slash
commands, and whether the vendor resumed.

Two rules follow from the split:

- **A session update is not a message.** The command catalog says what a person may type next; the
  last announcement wins and none of it is ever persisted as assistant content.
- **A turn event never carries session state.** `EventKind::TurnStarted` is the turn-scoped
  identity — the vendor's own handle for *this* attempt — and it replaced the two arms that used to
  smuggle session facts onto the stream.

### The subscription race, and why it cannot happen

Read-then-subscribe loses whatever lands in between. `SessionSubscription` is built the other way
round: subscribing *is* reading. `SessionSubscription::current()` answers with the snapshot the
subscription was opened at, and `changed()` wakes for anything after it — there is no window
because there are not two calls.

`current()` keeps that captured picture until `changed()` consumes another one. It does not peek
at a pending update and then deliver the same revision again through `changed()`.

Every snapshot carries a `SessionRevision` that only increases. Updates coalesce, which is the
right semantics for a picture of the present; the revision is what lets a consumer tell coalescing
from stillness, and a persisted revision is what lets it tell a stale read from a current one.

### Ordering and stale work

`AgentEvent` names the session, the logical turn **and** the attempt. A host compares
`OperationRef::is_superseded_by` before letting a late result apply, so an event from an attempt
that has already been replaced cannot mutate the attempt that replaced it.

## Configuration: snapshot, patch, catalog

| Type                   | Answers                                                            |
| ---------------------- | ------------------------------------------------------------------ |
| `ConfigurationPatch`   | what is being asked for — keep, set or reset, per axis             |
| `Configuration`        | a set of values, where absence means **unknown**                   |
| `ConfigurationState`   | the three readings: `requested`, `accepted`, `observed`            |
| `ConfigurationCatalog` | what a vendor says it can be set to, with its own ids and ordering |
| `ConfigurationOutcome` | what actually landed, and what became of the rest                  |

The three readings stay apart because they disagree in practice. `accepted` is a statement about
the request a harness encoded — the flag it really passed. `observed` is the only one that is
evidence, and a harness with no vendor surface reporting a setting leaves it unknown rather than
copying `accepted` across. An accepted command-line option is not a vendor-observed model.

Reset is a real operation and some vendors cannot do it. `refuse_unsupported_reset` is the shared
refusal; a vendor that cannot put an option back to its own default says so with
`SettingRejection::ResetNotSupported` rather than reporting a success it did not have.

Partial application is reported rather than flattened: `ConfigurationOutcome::is_partial` is true
when some axes landed and others were refused, and `Rollback` says whether the part that landed was
put back (`Restored`), left in place (`NotAttempted`, the honest answer for a vendor whose settings
cannot be un-set) or could not be put back (`Failed`). Nothing claims a transaction succeeded when
only part of it applied.

Catalogs are open. Models, reasoning efforts, modes and whatever a vendor invents next are rows,
not enum arms. A category this crate has no name for is carried as `ConfigurationCategory::Other`
and does not stop the rows beside it from being usable; a row whose id cannot survive bounding is
dropped on its own rather than taking the picker with it.

## Interactions: permission and question are different operations

|                                    | Permission             | Question          |
| ---------------------------------- | ---------------------- | ----------------- |
| Type                               | `PermissionRequest`    | `QuestionRequest` |
| Answered by                        | `Session::respond`     | `Session::answer` |
| Grants authority                   | yes                    | **no**            |
| A `PermissionBroker` may answer it | yes                    | never             |
| Capability                         | `InteractiveApprovals` | `Questions`       |

Both carry `Interaction`: the id to answer with, the owning session, the owning turn and attempt
when there is one, the deadline, and the resolution status. `InteractionKind::grants_authority` is
the field a host's audit trail and policy layer read, so a "which branch should I use?" prompt
cannot be recorded as, or policy-matched against, an executable grant.

**Secret collection and arbitrary forms are outside scope.** There is no `QuestionForm` arm for
either. A vendor asking for one is refused by name through `UnsupportedQuestion`, never reshaped
into ordinary free text — a password typed into a box labelled "answer" is a password in a host's
transcript.

### Permission options keep their reach

`PermissionOptionKind`'s `AllowAlways`/`RejectAlways` collapsed three separate facts into one word.
They are now separate:

- `PermissionEffect` — allow, reject, or something only a person can weigh.
- `PermissionScope` — `Once`, `Turn`, `Session`, `Persistent`, ordered narrowest first, and
  **absent** where the vendor does not actually expose a reach. An unstated reach is a reach nobody
  measured, so it is never read as the narrow one.
- `PermissionOption::policy_changing` — whether choosing it writes a rule the vendor applies on its
  own afterwards. Separate from scope because the two come apart: a vendor can offer a session-wide
  allow it forgets on exit, and a once-only allow it records in a settings file.
- `PermissionRisk` — what the vendor said, never what this library inferred.

`PermissionRequest::allow` and `deny` pick the narrowest reach on offer and prefer an option that
writes nothing. `ApprovalDecision` carries the reach of the option that won, because the request
that prompted it is gone by the time anyone audits the decision.

## Content: identity, relationships, and a bounded escape hatch

`Activity` carries `item_id`, `parent_id` and `subagent_id`, so a later update can address one
activity rather than replace a list, and a host can nest what a subagent did under the call that
started it. `ActivityContent` keeps a plan as steps with their own state, a diff as files with
their own counts, and output as its own thing — none of them flattened into prose a host would have
to re-parse.

`Extensions` is the long tail: a flat, scalar-only map, capped at 32 entries, with bounded keys and
values, run through the same credential redaction a stderr tail is. It is **observational**.
Nothing read from it is executed, dispatched or turned into an RPC, and there is deliberately no
nesting — nesting is what turns a metadata field into a payload channel. There is no raw vendor
frame anywhere in the public API.

## Operation identity and dispatch certainty

| Identity         | Minted by  | Stable across                          |
| ---------------- | ---------- | -------------------------------------- |
| `TurnId`         | the host   | every attempt at the same logical turn |
| `AttemptId`      | the host   | one dispatch of it                     |
| `native_turn_id` | the vendor | whatever the vendor decides            |

`TurnId` is **not an idempotency key**, and its documentation no longer says it is. Nothing in this
library, and nothing in any vendor it drives, deduplicates on it.

`AttemptId` is a **generation number**, not a name. Two of them have to be comparable —
`OperationRef::is_superseded_by` is what stops a late result from an abandoned attempt overwriting
the attempt that replaced it — and no ordering of opaque strings would be right: `attempt-10` sorts
before `attempt-2` lexicographically, which is the first shape a host naming its attempts would
reach for. A host that also wants an opaque handle per attempt keeps one beside this.

Before retrying, read `Error::dispatch()`. Harnesses attach it with `with_dispatch` where the
operation fails. An unannotated error returns `AcceptanceUnknown`; its variant alone cannot tell
whether work was submitted. `Error::cause()` exposes the original typed error for matching.

| Verdict             | Means                                       | Safe to replay       |
| ------------------- | ------------------------------------------- | -------------------- |
| `NotSubmitted`      | refused before anything reached the vendor  | yes                  |
| `Accepted`          | the vendor answered, so it read the request | no                   |
| `AcceptanceUnknown` | something broke around the request          | no — reconcile first |

`AcceptanceUnknown` is deliberately not "probably fine": that reading is the one that runs a turn
twice. A surface that cannot be reconciled must not be blindly re-executed.

## Discovery receipts are a seam, not a cache

`Harness::probe` may not memoise — how fresh an answer has to be is the host's decision. A host
that has just drawn a picker from a probe already has the answer, so `DiscoveryReceipt` lets it say
so: it travels on the `OpenSession` that uses it, is checked for harness identity, executable
identity and freshness, and is then forgotten. Nothing in this library stores one or looks one up,
and there is no process-global cache.

The executable and environment fingerprints are opaque and host-computed, and `verify_for`
deliberately does **not** check them: measuring what the executable looks like *now* is something
only the host can do. A host that wants the check measures again at open time and calls
`DiscoveryReceipt::describes`, which compares the two for equality and never interprets either — so
a host that hashes the binary, reads its mtime or records a package version all work.

A receipt for a resolved executable also refuses a request that names none. The launcher would
resolve the program name off `PATH`, which may answer with a different file, and a receipt that
vouches for one binary cannot vouch for whichever one that turns out to be.

## Listing and account readings without a conversation

`Harness::list_sessions` and `Harness::account_usage` are optional services for a host that needs
data before opening a conversation. Both default to `Error::NotSupported`, and the shipped
harnesses keep those defaults. The listing/account capability flags describe the corresponding
session methods, not these independent services: Codex currently requires an open `Session` for
`thread/list` and account usage. Hosts must handle a harness-level refusal even when its session
capability is advertised. Neither surface reads a credential.

## Protected types, justified individually

`#[non_exhaustive]` blocks struct-literal construction outside this crate, which is a real cost. It
is applied only where the expected growth justifies it, and every protected type has a constructor
or builder that keeps the cost at zero. `crates/mango-external-agents/tests/downstream_contract.rs`
is compiled as its own crate — the only place the attribute means anything — and proves every one
of them is still reachable.

| Protected                                                                                    | Why                                                                          |
| -------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------- |
| `OpenSession`, `TurnRequest`, `SessionQuery`                                                 | the host-facing request surface; it gained three fields in this change alone |
| `ConfigurationPatch`, `Configuration`, `ConfigurationState`, `ConfigurationOutcome`          | axes are added as vendors expose them                                        |
| `ConfigurationOption`, `ConfigurationOptionValue`, `RejectedSetting`                         | vendor metadata grows                                                        |
| `Interaction`, `QuestionRequest`, `Question`, `QuestionOption`, `QuestionResponse`, `Answer` | the interaction vocabulary is the youngest surface here                      |
| `PermissionRequest`, `PermissionOption`, `PermissionResponse`, `ApprovalDecision`            | scope and risk arrived in this change; more will                             |
| `Activity`, `AgentEvent`                                                                     | the event vocabulary grows with every vendor inventoried                     |
| `SessionSnapshot`, `TransportSelection`, `HarnessIdentity`                                   | session facts accumulate                                                     |
| `DiscoveryReceipt`                                                                           | identity and freshness metadata will grow                                    |
| `PlanStep`, `FileChange`                                                                     | structured content grows                                                     |

Left **open** on purpose:

| Open                                                                | Why                                                                                                                                                 |
| ------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- |
| `Discovery`                                                         | constructed by a harness, which recompiles against this crate anyway; closing it buys a compile error in place of a field nobody had to think about |
| `HarnessDescriptor`                                                 | same                                                                                                                                                |
| `SessionIds`, `Resume`, `McpServer`, `SessionPage`, `NativeSession` | small, stable, and constructed by implementors rather than hosts                                                                                    |
| `Capabilities`                                                      | exhaustively destructured in `Capabilities::beyond`, so a new flag is already a compile error where it matters                                      |

Enums are `#[non_exhaustive]` nearly everywhere, because a new arm is the ordinary way a vocabulary
grows and a host matching on one should be made to say what it does with the rest.

## What is deliberately not here

- No dynamic plugin loader, C ABI, language bindings or second product protocol. A harness is a
  Rust trait implementation.
- No raw vendor payload channel. `Extensions` is bounded scalars; there is nowhere to put a frame.
- No credential anywhere: no login method, no token field, no secret-collecting question.
- No promise of exactly-once vendor execution. `Dispatch` describes certainty; it does not create
  it.
- No implementation channel a host could take ownership of: `TurnStream`'s receiver is private
  behind `recv`/`try_recv`, so the turn's lifetime and its budget stay this library's to change.
