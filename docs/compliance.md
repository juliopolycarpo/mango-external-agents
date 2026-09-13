# Compliance posture

What each vendor's public documents say about driving their CLI from another program, and what
this library does and does not do in response. Quotes are filled in per harness as each lands
(Claude, Codex, ACP); until then a section states the posture and the source it will quote.

Facts were read on 2026-09-12; re-verify against the vendor's current page before relying on them.

## Invariants for every harness

- **No login handling.** The library never authenticates a vendor, never opens a browser, never
  stores, reads, copies or forwards a token, and offers no "log in through mango-external-agents".
  It reuses whatever the user already logged into with the vendor's own CLI. Discovery may report
  that state (`LoggedIn { mode }`, `LoggedOut`, `Unknown`) only from a non-secret vendor surface;
  when the only way to know would be reading a credential file, the answer is `Unknown` and the
  host tells the user to run the vendor's login command. A turn against a logged-out CLI fails
  with `AuthRequired` carrying that command as text.
- **Official CLIs, documented surfaces.** Only the vendor's own executable, only the programmatic
  surface the vendor documents.
- **Vendor tools never enter the host's tool registry.** A vendor-initiated tool call is answered
  with a protocol error, never executed by the host.
- **Vendor assistant text is never replayed** into the host's own model context by the library.
- **Redaction.** stderr crossing a diagnostic boundary is redacted for credential-shaped text.
- **No telemetry, no listener, no downloaded binaries.**

## Claude Code (`mango-agent-claude`)

**Surface used:** the documented headless mode,
`claude --print --output-format stream-json --input-format stream-json …`, one process per turn
with `--resume`. Discovery adds three documented read-only probes: `claude --version`,
`claude --help` and `claude auth status`. Nothing else is read. See
[harness-claude.md](harness-claude.md) for every flag and the document behind it.

**Quotes** (read 2026-09-13, from
<https://code.claude.com/docs/en/legal-and-compliance>, "Authentication and credential use"):

> OAuth authentication is intended exclusively for purchasers of Claude Free, Pro, Max, Team, and
> Enterprise subscription plans and is designed to support ordinary use of Claude Code and other
> native Anthropic applications.

> Developers building products or services that interact with Claude's capabilities, including
> those using the Agent SDK, should use API key authentication through Claude Console or a
> supported cloud provider.

> Anthropic does not permit third-party developers to offer Claude.ai login or to route requests
> through Free, Pro, or Max plan credentials on behalf of their users.

> Anthropic reserves the right to take measures to enforce these restrictions and may do so
> without prior notice.

The contractual backstop is in the Consumer Terms themselves
(<https://www.anthropic.com/legal/consumer-terms>), among the prohibited uses:

> Except when you are accessing our Services via an Anthropic API Key or where we otherwise
> explicitly permit it, to access the Services through automated or non-human means, whether
> through a bot, script, or otherwise.

**Posture:** the library runs the user's own installed `claude`, under whatever the user already
logged into with the vendor's own CLI, and never reads, stores, copies or forwards its token. It
offers no Claude.ai login of its own and routes no request anywhere — the vendor's binary talks to
the vendor. Whether a host spawning that binary counts as "ordinary use of Claude Code" is
**inferred from Anthropic's enforcement pattern** (the targeted conduct has been extracting OAuth
tokens into another product, not running the CLI as a subprocess), not from a written exception,
and this page says so rather than claiming a permission nobody granted. The third quote is the one
a host should read closely: it bites on offering Claude.ai login and on routing requests through
subscription credentials, neither of which this library does.

Discovery reports `AuthState` — including whether the account is a subscription rather than an API
key — so a host can show its own disclosure and make its own call. `mango-external-agents` makes
none on a host's behalf.

**The route not taken.** Anthropic's own Agent SDK reaches a permission-callback channel by passing
`--permission-prompt-tool stdio`, a sentinel that appears in no `--help` and on no documentation
page. This harness does not use it: driving an undocumented surface is the thing this repository's
rules exist to prevent, and it would be a poor trade for a feature whose reliability history is
public and unresolved. `interactive_approvals` is reported false instead.

**Nominative use.** "Claude Code" and "Anthropic" name the tool being launched and the company
whose terms apply. No logos, no wordmarks, nothing implying an official or endorsed integration.

## OpenAI Codex (`mango-agent-codex`)

**Surface used:** `codex app-server`, the supported integration protocol (JSON-RPC over lines).
`clientInfo.name` is always the host's name, passed through `HostContext`.

**Posture:** OpenAI has publicly welcomed third-party harnesses on subscriptions (press coverage,
2026-02); the doc cites it as reported, not as a licence term.

**Quotes:** to be filled by the Codex harness with the app-server documentation and the coverage cited.

## Agent Client Protocol agents (`mango-agent-acp`)

**Surface used:** ACP v1 over the official `agent-client-protocol` crate; each profile links the
agent's own documentation for its ACP mode.

**Posture:** the protocol exists for exactly this use. Per profile:

- **Cursor** (`agent acp`): documented; no third-party-harness statement found either way. The
  doc says "not found", not "permitted".
- **OpenCode**, **Gemini CLI**, **Copilot CLI**, **Goose**, the `codex-acp` and `claude-code-acp`
  shims: to be filled by the ACP harness, each with its documentation link.

**Quotes:** to be filled by the ACP harness.
