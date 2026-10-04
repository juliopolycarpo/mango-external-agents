# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com), and this
project adheres to [Semantic Versioning](https://semver.org).

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


