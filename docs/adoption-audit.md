# Standalone adoption audit

Audited source: [`589394e3e95fdea1283c35e51893fa7910e90f90`](https://github.com/juliopolycarpo/mango-external-agents/tree/589394e3e95fdea1283c35e51893fa7910e90f90),
the published 0.3.1 implementation. This change preserves the four crate versions and public
behavior. Its runtime behavior correction is confined to the unpublished reference host.

## Dependencies and host assumptions

| Assumption examined                        | Exact evidence                                                                                                                                                                                                                                                                                                       | Disposition                                                                                                                    |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| Product or Mango Protocol dependency       | `Cargo.toml` declares only the four SDK crates in workspace dependencies. Each `crates/*/Cargo.toml` lists generic dependencies and its own core dependency. `cargo metadata --no-deps --format-version 1 --locked` identifies the four publishable packages and two unpublished examples.                           | No dependency to remove. The independent consumer's full resolved metadata also rejects product packages.                      |
| Hub RPC, schema and product identifiers    | `crates/mango-external-agents/src/identity.rs`, `session.rs` and `operation.rs` define SDK identities and traits. `examples/hub-host/src/hub.rs` defines its own `HubApi` using SDK `OperationRef`, `RequestFingerprint` and `TerminalStatus`. Its manifest depends only on core, async-trait, serde_json and Tokio. | Keep the local example port; it imports no product protocol or service.                                                        |
| Required durable orchestration or database | `RecoveryRecord` and `RequestFingerprint` live in core `recovery.rs`. `Harness::open_session` and `Session::start_turn` do not take them. Only the advanced example owns reservations and retry orchestration.                                                                                                       | Keep generic optional recovery facilities. The basic consumer opens and runs without them.                                     |
| Runtime home or persistence layout         | `HostContext` in core `host.rs` requires launcher, cwd and caller-supplied client identity. Environment, scratch, broker, clock, limits and cancellation are generic ports. Claude's `mcp.rs` enforces authorized child-visible scratch only when MCP configuration needs it.                                        | No MangoStudio runtime directory or database is required. Preserve authorization and scratch checks.                           |
| Authorization and consent storage          | Core `host.rs`, `permission.rs` and `process.rs` accept host ports and authorized launch values. No product consent store is a dependency.                                                                                                                                                                           | Host policy remains explicit. The small consumer denies approvals and never widens cwd or environment.                         |
| Product identity defaults                  | `HostContextBuilder::build` refuses absent client identity; the example strings in `host.rs` are caller data. Workspace homepage and crate keywords mention MangoStudio.                                                                                                                                             | Identity remains caller-supplied. Branding metadata is not dependency coupling.                                                |
| Mandatory launcher or framework            | Core manifest's `launcher-tokio` is optional. `ProcessLauncher`, `ByteSource`, `ByteSink` and `ProcessControl` in `process.rs` accept custom implementations. Each concrete harness documents its Tokio requirement.                                                                                                 | Keep injected process and byte-I/O ports. Test the independent consumer both without and with the bundled launcher.            |
| Vendor protocol and login                  | Each vendor crate owns its reducer/protocol. ACP and its transport dependencies stay in `mango-agent-acp`; Codex's vendor schema stays in its own crate. `docs/compliance.md` prohibits vendor-login handling and credential forwarding.                                                                             | No product relocation or new login facility. Real vendors still require installation, authentication and vendor qualification. |
| Delivery workflow                          | SDK manifests and public contracts do not depend on another product's release pipeline. `scripts/check-publish.sh` packages the four crates from this repository.                                                                                                                                                    | The adoption proof uses immutable published 0.3.1 packages. Docs/example changes need no crate bump.                           |

The consumer check prints all four registry source identities and requires `=0.3.1`, a single
consumer workspace member and no path dependency for those crates. It runs ACP, Claude and Codex
through discovery, opening, request dispatch, event consumption, response, live cancellation and
close. It checks authorized cwd, environment filtering and zero remaining fake children. This
proves independent compilation and deterministic harness integration; it does not qualify live
vendor installations, Windows process containment or native process cleanup.

## Dispatch inventory at the audited implementation

Production `NotSubmitted` annotations were re-read at the source above, excluding comments,
doctests and test modules. No vendor implementation changes in this PR, so this is the current
implementation inventory as well, not a fixed count future releases must satisfy.

| Harness file                        | Production annotations |
| ----------------------------------- | ---------------------: |
| `mango-agent-acp/src/harness.rs`    |                      7 |
| `mango-agent-acp/src/session.rs`    |                     14 |
| `mango-agent-claude/src/harness.rs` |                      4 |
| `mango-agent-claude/src/session.rs` |                     11 |
| `mango-agent-codex/src/harness.rs`  |                      6 |
| `mango-agent-codex/src/session.rs`  |                      8 |
| Total                               |                     50 |

Reproduce the candidate listing with
`rg -n 'Dispatch::NotSubmitted' crates/mango-agent-{acp,claude,codex}/src`, then inspect the owning
functions. The ACP configuration doctest is not a production annotation. Do not truncate a file at
its first test module: Codex has production session methods after an earlier test module.

The annotations cover several different facts: caller limits and configuration checks,
unsupported transports, busy ownership, closed/cancelled sessions, failed launches and writes that
never began. They prove acceptance certainty, not common retry policy or common session health.
The core error conformance test preserves distinct Busy, LimitExceeded, Link, Timeout, Vendor and
CleanupRequired advice under the same `NotSubmitted` annotation.

## Reference-host correction

The old start branch retried every safe-to-replay error. `RetryPolicy` intentionally has no attempt
ceiling. The named refusing session regression kept the loop running for 100 ms under a paused
clock: it observed four oversized-prompt attempts before clean shutdown. The corrected host settles
that refusal after exactly one start and one withdrawal, and remembers it on a repeat call.

Busy retains backoff and acceptance-unknown work retains reconciliation. Cleanup handles return
unchanged in every dispatch state. A second failing regression showed those handles being discarded
until external shutdown; the fixed host returns the handle before recovery. A third failing
regression showed acknowledged errors losing `Accepted` and allowing a contradictory absence answer
to unlock replay. The corrected record retains native acceptance and refuses that contradiction.
Unsubmitted nonretryable Link/Timeout/Vendor errors return to the host for recovery rather than
acquiring a caller-error label. Session usability is checked separately before an otherwise
retryable refusal is replayed. `examples/hub-host/tests/start_failures.rs` records these dispositions;
the existing recovery and stopping suites cover uncertainty and retained reservations.

All of this fits one adoption/examples PR. No second isolation change or public retry-query API is
needed. No performance claim is made; the measured attempt count demonstrates correctness only.
