# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com), and this
project adheres to [Semantic Versioning](https://semver.org).

## [0.8.0] - 2026-11-15

### 🚀 Features

- **(mea)** Record a named vendor CLI version in an ACP capture (#481)
- **(mea)** Report retained Claude members that no review has seen (#421)
- **(mea)** Report the Claude members a capture replaced or had not seen (#420)
- **(mea)** Choose the model for a Claude transcript capture (#409)
- **(mea)** Record a Claude schema-turn transcript through capture (#392)
- **(mea)** Pass a JSON schema to a Claude turn and print the answer (#387)
- **(claude)** Surface the --json-schema answer as a structured output event (#381)
- **(core)** Add an event for a turn's validated structured answer (#380)
- **(mea)** Pick the Codex model for turn and capture (#375)
- **(mea)** Pass a Unix socket path to turn (#373)
- **(codex)** Attach to a running app-server over its Unix socket (#370)
- [**breaking**] **(claude)** Pass an opt-in JSON schema for structured output (#369)
- **(mea)** Record the Codex CLI help surface in the public contract (#368)
- **(codex)** Export a scoped version-answering test launcher (#270)
- [**breaking**] **(codex)** Preserve variant evidence and child attribution (#210)

### 🐛 Bug Fixes

- **(claude)** Singular count nouns in refused-launch shapes (#483)
- **(mea)** Declare tool_use_result a user-chosen map in the drift report (#482)
- **(mea)** Replace numbers and booleans in recorded tool data (#472)
- **(claude)** Count a known flag name that is another flag's value in a refused launch (#468)
- **(mea)** Replace structural-named strings inside a recorded tool_input (#463)
- **(claude)** Name only the flags the library emits in a refused fake launch (#455)
- **(mea)** Declare who chose the keys under a reviewed Claude member (#448)
- **(mea)** Keep the plain Vertex publisher model id in a Claude transcript (#444)
- **(claude)** Name a refused fake launch by flag shape, never by argv (#439)
- **(mea)** Tell the operator a Claude capture keeps the answer verbatim (#418)
- **(mea)** Keep only structural members of the Claude init record (#417)
- **(mea)** Describe credential_expires_at as the string Claude Code emits (#413)
- **(mea)** Redact account-shaped model ids in a Claude transcript (#412)
- **(mea)** Use one failure formatter so a vendor message is not dropped (#411)
- **(claude)** Tighten the fake Claude CLI and extend the feature powerset (#410)
- **(fixtures)** Keep recorder diagnostics out of the packaged crates (#408)
- **(mea)** Redact account-shaped members in a recorded Claude transcript (#401)
- **(mea)** Print the vendor message in the remaining failure reports (#405)
- **(fixtures)** Package and replay a recorded Claude transcript (#402)
- **(mea)** Carry the vendor message into a turn's failure summary (#391)
- **(claude)** End a schema turn without an answer as an error (#386)
- **(core)** End the process group when runtime shutdown drops its cleanup (#360)
- **(core)** Release oversized stderr tail allocations (#359)
- **(core)** Reclaim severely stripped text buffers (#358)
- [**breaking**] **(codex)** Decode observed reviewers independently (#353)
- **(claude)** Preserve probe presence and host refusals (#352)
- **(ci)** Check resolved ACP schema identity (#350)
- **(claude)** Refuse malformed HTTP MCP servers (#351)
- [**breaking**] **(codex)** Decode observed approval policies independently (#349)
- **(release)** Validate prerelease package versions (#341)
- **(fixtures)** Ship captures needed by packaged crate tests (#335)
- **(acp)** Withdraw permissions after stream overflow (#334)
- **(acp)** Route unreadable updates to their admitted turn (#332)
- **(acp)** Bind opening updates to their native session (#328)
- **(acp)** Bind permission requests to the turn that received them (#323)
- **(acp)** Preserve turn outcomes when activity cleanup fills the stream (#318)
- **(acp)** Keep permission settlement owned through terminal draining (#287)
- **(codex)** Commit interaction announcement bookkeeping atomically (#285)
- **(codex)** Retain question outcomes through publication (#283)
- **(codex)** Retain approval settlements through publication (#281)
- **(core)** Omit unknown question labels from diagnostics (#279)
- **(core)** Reject ambiguous approval choices before brokering (#278)
- **(mea)** Retain bounded cleanup ownership across captures (#274)
- **(mea)** Retain cleanup failures when the turn also fails (#225)
- **(claude)** Fail turns and taint refused transcripts (#224)
- **(codex)** Preserve version probe outcomes and host refusals (#223)
- **(codex)** Release abandoned quota read registrations (#222)
- **(claude)** Bound and release nested transcript storage (#221)
- **(core)** Elect one final stream sender before terminal publication (#220)
- **(core)** Preserve fake interaction ownership when answering (#219)
- **(core)** Publish fake announcements atomically with attachment (#218)
- **(examples)** Preserve retry growth and handle jitter extremes (#216)
- **(ci)** Fail TLS policy checks on graph inspection errors (#215)
- **(core)** Preserve session page truncation across normalization (#214)
- [**breaking**] **(codex)** Require explicit gateway readiness and qualify vendor pins (#212)
- [**breaking**] **(codex)** Preserve GCP sign-in and tolerate future account kinds (#239)
- **(acp)** Retain typed cleanup failures across shared waiters (#211)
- **(core)** Distinguish process teardown failures from launch failures (#209)
- **(deps)** Require Tokio 1.53.3 for I/O safety (#206)
- **(acp)** Repair Goose metadata and Codex source citations (#202)
- **(deps)** Reject distrusted certificate intermediates (#203)

### ⚡ Performance

- **(core)** Strip dirty text without String::retain (#504)
- **(codex)** Encode images into the final data URL buffer (#217)
- **(core)** Normalize discovery permissions during intersection (#241)
- **(codex)** Avoid unused activity construction on completion (#234)

### ♻️ Refactor

- **(ci)** Serve the MSRV feature check and the powerset from one driver (#440)
- **(claude)** Share the fake Claude CLI behind a testing feature (#393)
- [**breaking**] **(core)** Validate HTTP MCP endpoints with one core grammar (#291)
- [**breaking**] **(core)** Compare vendor versions with one dotted-number module (#302)
- **(claude)** Generate UUIDs with ring and drop uuid (#303)

### 📚 Documentation

- **(ci)** Record why the ACP adapter captures are not a scheduled lane (#479)
- **(claude)** Record the first live --model run and pin its shape (#447)
- **(acp)** State which vendor binary the claude-agent-acp and codex-acp profiles run (#445)
- Record why the per-crate test command keeps --all-features (#441)
- Assess ACP convergence for the Claude and Codex harnesses (#435)
- Run the working-loop test command with --all-features (#403)
- **(codex)** State when the app-server refuses a socket's parent directory (#378)
- **(core)** Retain ring for request fingerprints (#346)
- **(acp)** Clarify session facts and HTTP transport policy (#226)
- **(docs)** Refresh guide navigation and bug template (#208)
- **(release)** Maintain release and adoption procedures (#207)
- **(docs)** Correct harness facts and evidence dates (#205)
- **(core)** Align public contracts and lifecycle guidance with source (#204)

### 🧪 Testing

- **(fixtures)** Capture the claude-agent-acp and codex-acp handshakes (#459)
- **(fixtures)** Pin the Claude recorder's output names to the packaging script (#446)
- **(mea)** Cover the call sites of the Claude recorder's stderr notices (#437)
- **(claude)** Pin emit_run_end and resume after a missing structured answer (#390)
- **(codex)** Replay a real subagent attribution frame (#376)
- **(codex)** Correlate native approval responses (#355)
- **(acp)** Synchronize permission holds with response owners (#327)
- **(claude)** Create scratch roots that pass under any umask (#298)
- **(core)** Gate writes in the JSON-RPC close-race test (#305)
- **(acp)** Replace expiry scheduler spins with causal waits (#244)
- **(acp)** Qualify expiry answer refusal with a live sibling (#213)
- **(mea)** Add named Codex attribution capture (#236)
- **(consumer)** Pin the registry consumer at 0.7.1 (#199)

### ⬆️ Dependencies

- **(deps)** Hold new crate versions for three days before locking them (#505)
- **(deps)** Fail cargo-deny on a new duplicate or an unused licence (#301)
- **(deps)** Refresh the lockfile and require win32job 2.1.0 (#290)

### 👷 CI

- Run the feature powerset in its own parallel job (#510)
- Prune superseded Rust caches on main (#508)
- Install the pinned toolchain before the release cache restore (#507)
- Restore main's Rust cache in the release jobs and save none (#506)
- Save rust-cache from main only and share it with vendor-drift (#502)
- Silence SC2016 on the literal backticks the vendor docs scripts print (#496)
- Run shellcheck over the repository scripts (#486)
- Carry the matching page and line in the ACP docs watch issue body (#484)
- Prove the script tests named in ci.yml can run (#474)
- Let the ACP docs watch accept a reviewed mention (#469)
- Run the feature-lane script tests in CI (#464)
- Watch vendor docs for an ACP page or command (#458)
- **(release)** Install cargo-hack before release verification (#348)
- **(deps)** Bump the actions group with 5 updates (#345)
- **(deps)** Schedule reviewed GitHub Actions updates (#292)

### 🏗️ Build

- **(build)** Keep only line tables in dev and test debug info (#509)
- **(build)** Pin Rust 1.100.0 (#501)
- [**breaking**] **(build)** Raise the minimum Rust version to 1.98 (#503)
- Run gated tests from the bare per-crate command (#450)
- **(claude)** Pin Claude Code at 2.1.322 (#356)
- **(codex)** Pin Codex at 0.167.1 (#357)
- **(codex)** Project consumed schema semantics (#354)
- **(acp)** Pin agent-client-protocol-schema at 1.17.0 (#299)
- Declare third-party dependencies once in the workspace (#300)
- **(acp)** Declare async-trait once and refuse a repeated dev-dependency (#297)

## [0.7.1] - 2026-10-16

### 🚀 Features

- **(claude)** Report resumability and tag a refused resume (#196)

### 🐛 Bug Fixes

- **(deps)** Require rustls 0.23.46 for its four TLS advisories (#197)
- **(core)** Read well-known bare tokens in a stderr tail (#193)
- **(core)** Forward the XDG base directories to the agents that read them (#192)
- **(codex)** Refuse an outdated app-server before it is started (#194)
- **(acp)** Name the host's process control when it panics (#195)
- **(codex)** Name the host's process control when it panics (#191)
- **(mea)** Print the vendor's reason when a smoke turn cannot open (#190)

### 🧪 Testing

- **(core)** Pin the registry consumer to the published 0.7.0 (#189)

## [0.7.0] - 2026-10-12

### 🐛 Bug Fixes

- **(core)** Redact well-known bare tokens, escaped JSON and values on the next line (#186)
- **(claude)** Keep a closing session Closing until its artifact removal ends (#187)
- [**breaking**] **(claude)** Return CleanupRequired when a stop cannot prove the child gone (#184)
- **(codex)** Finish a terminal raised inside a request when the connection is cut (#185)
- **(core)** Bound a close behind a handler that blocks its thread (#182)

### 📚 Documentation

- **(core)** Say which task a close looks at again (#183)

### 🧪 Testing

- **(core)** Pin the registry consumer to the published 0.6.1 (#181)

## [0.6.1] - 2026-10-12

### 🚀 Features

- **(core)** Give a host each harness's worst-case deadlines (#176)

### 🐛 Bug Fixes

- **(core)** Redact quoted, glued and unusual credential shapes in vendor text (#177)
- **(core)** Redact credential-shaped text in a vendor's error message (#175)
- **(core)** Bound a second close behind one that is held (#173)
- **(acp)** Give a host the login hint when an agent answers a prompt with auth-required (#170)

### 🧪 Testing

- **(core)** Measure the built-in launcher's stop bound (#172)
- **(codex)** Prove a refused hold claims nothing through the peer seam (#174)
- **(core)** Pin the registry consumer to the published 0.6.0 (#171)

## [0.6.0] - 2026-10-11

### 🚀 Features

- **(core)** Hold a hand-off's cancellation off and settle the last close race (#168)
- **(core)** Name the outbound bound an error or an end is about, by type (#165)
- **(core)** Say when a peer's error member could not be read (#164)
- **(core)** Answer JSON that is not a request with an invalid-request error (#160)
- [**breaking**] **(acp)** Run on the in-house JSON-RPC peer and drop the SDK (#159)
- **(core)** Hand the peer's questions over in order with a responder (#158)
- **(core)** Read batches, answer parse errors and cancel on abandon (#157)
- **(core)** Let a notification call hold its cancellation off (#153)
- **(core)** Let one request wait without a deadline, outside the pending budget (#151)
- [**breaking**] **(core)** Queue outgoing frames in call order under an outbound budget (#149)
- **(core)** Let a request ask for its answer after earlier notifications (#145)
- **(acp)** Surface session notices and compaction (#147)
- **(acp)** Name a tool call by its own name when the agent sends one (#144)
- **(mea)** Cancel a turn after a delay (#143)

### 🐛 Bug Fixes

- **(acp)** End the session when the agent's error cannot be read (#166)
- **(claude)** Bound every wait of a close and state the deadlines (#162)
- **(acp)** Answer JSON that is not a request as an invalid request (#163)
- **(acp)** Keep the session after an agent's error on a prompt (#161)
- **(codex)** Finish a turn's terminal when the connection is cut under it (#156)
- **(core)** Begin nothing queued once an end has been decided (#155)
- **(codex)** Settle a cancel that races a steer or a poisoned session (#152)
- **(codex)** Read a turn's completion before answering a late interrupt (#150)
- **(acp)** Report an agent that died mid-prompt as a closed link (#148)

### ♻️ Refactor

- **(acp)** Read the peer's outbound bounds by type (#167)
- **(acp)** Import the wire types from the schema crate (#154)

### 🧪 Testing

- **(acp)** Pin connection behaviour through the public surface (#146)
- **(core)** Wait for the handler in the notification order tests (#142)
- **(core)** Pin the registry consumer to the published 0.5.0 (#141)

## [0.5.0] - 2026-10-10

### 🐛 Bug Fixes

- **(ci)** Run the feature powerset in check.sh (#137)

### 🧪 Testing

- **(acp)** Pin that a burst's last update precedes its terminal (#138)
- **(core)** Pin the registry consumer to the published 0.4.3 (#136)

### 🏗️ Build

- [**breaking**] **(acp)** Bump agent-client-protocol to 3.3.0 (#139)

## [0.4.3] - 2026-10-05

### 🐛 Bug Fixes

- **(core)** Redact a credential whose name a removed byte interrupts (#134)
- **(core)** Redact the stderr tail outside its lock (#133)

### 🧪 Testing

- **(core)** Pin the registry consumer to the published 0.4.2 (#131)

### 👷 CI

- Check isolated minimum-Rust features before a release publishes (#132)

## [0.4.2] - 2026-10-04

### 🐛 Bug Fixes

- **(core)** Redact long credential names and URL schemes in linear time (#128)

### ⚡ Performance

- **(core)** Count event bytes and repair malformed lines faster (#129)

### 👷 CI

- Check isolated features on the minimum Rust and keep fresh locks (#127)

## [0.4.1] - 2026-10-03

### ⚡ Performance

- **(core)** Skip clean text when redacting stderr (#125)

## [0.4.0] - 2026-10-03

### 🐛 Bug Fixes

- **(ci)** Remove publication scratch before caching (#115)

### ⚡ Performance

- **(core)** Find line breaks a block at a time (#123)
- **(core)** Keep the ASCII prefix scan branch-free (#122)
- **(core)** Keep the clean-text scan vectorized on Rust 1.99 (#120)

### 🧪 Testing

- **(core)** Time the send, not buffer release, in the stdio benches (#121)

### 🏗️ Build

- [**breaking**] **(build)** Raise the minimum Rust version to 1.97 (#118)
- **(build)** Pin Rust 1.99.0 (#117)

## [0.3.2] - 2026-10-02

### 🚀 Features

- **(acp)** Allow an independent outbound byte budget (#113)

### 🐛 Bug Fixes

- **(core)** Close links after completed transport write failures (#112)
- **(examples)** Settle refusals in independent SDK hosts (#111)

## [0.3.1] - 2026-09-30

### 🚀 Features

- **(core)** Let a harness close open structures without replacing its terminal (#71)

### 🐛 Bug Fixes

- **(acp)** Keep a stale turn's frame out of the next turn's reducer (#82)
- **(acp)** Keep agent tool call ids out of the plan's id namespace (#94)
- **(core)** End the connection when any write times out mid-frame (#99)
- **(codex)** Keep raw protocol records out of Debug output (#105)
- **(examples)** Report an unconfirmed stop when cancel fails (#78)
- **(core)** Stop a malformed escape sequence from hiding a credential name (#95)
- **(claude)** Keep raw stream records out of Debug output (#106)
- **(acp)** Report events the core refuses to publish (#81)
- **(examples)** Track stream acceptance certainty separately (#101)
- **(core)** Clamp the JSON-RPC notification queue to what a channel can hold (#90)
- **(codex)** Fail the turn when the core refuses to publish an event (#88)
- **(acp)** Stop a child launched without stdin before failing the open (#91)
- **(acp)** Clamp the pending-request limit to what a semaphore can hold (#89)
- **(codex)** Keep approval payloads out of Debug output (#98)
- **(acp)** Refuse configuration once the watcher has ended a session (#100)
- **(acp)** Shut down an active session on drop (#92)
- **(examples)** Keep the Hub's settlement separate from the local terminal (#80)
- **(codex)** Ignore other threads when claiming a pending turn (#85)
- **(claude)** End reasoning when a run stops mid-thinking (#63)
- **(core)** Terminate the link when a reply cannot be written (#87)
- **(ci)** Match vendor-drift issues by label and marker (#65)
- **(acp)** Stop approval callbacks when their turn ends (#86)
- **(acp)** Accept MCP server arguments that start with a dash (#96)
- **(codex)** Show the rule an approval amendment grants (#77)
- **(codex)** Close open activities and reasoning before the terminal (#68)
- **(acp)** Stop a running session when the host cancels (#73)
- **(core)** Discard an overflowed stderr line until its end (#60)
- **(acp)** Settle an expired approval that has no refusal option (#57)
- **(mea)** Report a capture whose child was not reaped (#58)
- **(examples)** Keep the reservation history on the logical turn (#59)
- Stop buffering streamed answer text no completion can repeat (#47)
- **(acp)** Store fixed-size keys for finished tool calls (#45)
- **(claude)** Refuse partial discovery output instead of misreporting it (#41)
- **(acp)** Refuse prompts that exceed the outbound frame before submission (#42)
- **(acp)** Bound the version probe's output (#40)
- **(core)** Count repaired UTF-8 against the unread-output budget (#38)
- **(examples)** Report lagged hub-host subscribers instead of skipping silently (#39)

### ⚡ Performance

- **(acp)** Bound oversized held tool-call updates when they are held (#84)
- **(core)** Copy clean prefixes in bound_text (#66)
- **(codex)** Keep only the tail of a command-output chunk that alone exceeds it (#93)
- **(claude)** Skip forwarded subagent text once the buffer is full (#97)
- **(acp)** Skip turn reduction for replayed history (#62)
- **(core)** Use a VecDeque in the scripted test link (#69)
- **(claude)** Check an Edit's strings without copying them (#56)
- **(claude)** Cut long text at an ASCII boundary without decoding it (#55)
- **(claude)** Bound tool-result text before joining it (#52)
- **(codex)** Avoid discarded copies in the notification path (#54)
- **(examples)** Bound hub-host broadcast retention by bytes (#46)
- **(core)** Record fake stdin lines in one scan (#53)
- **(acp)** Read tool-call details without cloning content (#51)
- **(core)** Stop copying owned JSON-RPC fields, text deltas and stdio messages (#50)
- **(core)** Avoid rescanning and copying lines in LineStream (#48)
- **(examples)** Build the hub-host retry request once (#49)

### 📚 Documentation

- **(release)** Update the README install snippets when bumping the version (#108)
- Fix the adopt guide's example and the Codex WebSocket quote (#102)
- **(examples)** Describe what a lagged subscriber can recover (#76)
- **(release)** State that packaged crates do not carry test fixtures (#74)
- **(codex)** Note the reachability of requestUserInput questions (#67)
- Point the README example at the tested fake-driven doctest (#64)
- **(acp)** Document the standing refusal an expired approval selects (#83)
- Cite current Anthropic, OpenAI and SpaceXAI terms in the compliance page (#61)
- **(core)** State that byte budgets are wire limits, not memory limits (#44)

### 🧪 Testing

- **(core)** Cancel with an open activity and reasoning; failing stdin fake (#104)
- **(examples)** Run the hub-host retry bench through scripts/bench.sh (#79)
- **(core)** Add non-gating performance benchmarks (#43)

### 👷 CI

- **(release)** Check the README install requirement against the release rule (#109)
- **(release)** Skip an existing GitHub release on rerun (#72)
- **(ci)** Test against freshly resolved dependencies (#70)
- **(release)** Verify the release tag signature (#75)

### 🏗️ Build

- **(acp)** Keep the testing dev-dependency in the published manifest (#103)

## [0.3.0] - 2026-09-25

### 🚀 Features

- [**breaking**] **(codex)** Report Codex credits, reset credits and spend control (#33)

### 🐛 Bug Fixes

- [**breaking**] **(acp)** Bring the ACP harness to parity with the TypeScript adapter (#29)
- [**breaking**] **(codex)** Bring the Codex harness to parity with the TypeScript adapter (#31)

## [0.2.0] - 2026-09-24

### 🐛 Bug Fixes

- **(acp)** Report transport budget overflow as a turn error (#27)
- [**breaking**] **(codex)** Separate turn-cancel settling from shutdown deadlines (#26)
- Finish Codex and ACP teardown before settling close (#25)
- **(acp)** Budget notification bursts by frames, not pending requests (#24)
- **(release)** Make the changelog describe what 0.1.0 shipped (#19)

## [0.1.0] - 2026-09-19

### 🚀 Features

- [**breaking**] Structured activity content and Codex interaction answers (#17)
- [**breaking**] **(core)** Add verified discovery and session services (#16)
- [**breaking**] **(core)** Settle session, configuration, identity and interaction contracts (#13)
- **(release)** Prepare 0.1.0 with smoke tooling and vendor drift checks (#6)
- **(acp)** The Agent Client Protocol harness, with per-agent profiles (#5)
- [**breaking**] **(codex)** Codex app-server harness (#4)
- **(claude)** The Claude Code harness (#3)
- **(core)** Traits, events, host ports, transports and testing fakes (#2)

### 🐛 Bug Fixes

- [**breaking**] **(core)** Make turn ownership and shutdown cancellation-safe (#15)
- [**breaking**] Harden diagnostics, Claude MCP scratch, and turn lifecycle (#12)

### 📚 Documentation

- Point at crates, not at plan files (#1)
- Repo rules, compliance skeleton, adopt guide

### 👷 CI

- Fmt, clippy, tests, deny, msrv on three OSes

### 🏗️ Build

- Release pipeline with trusted publishing

### 🧹 Miscellaneous

- Bootstrap the workspace

### Other

- Release gate: host adoption, Hub-owned retry, and publication readiness (#18)


