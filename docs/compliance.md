# Compliance posture

What each vendor's public documents say about driving their CLI from another program, and what
this library does and does not do in response. Quotes are filled in per harness as each lands
(plans 003–005); until then a section states the posture and the source it will quote.

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
`claude -p --output-format stream-json --input-format stream-json …`, one process per turn with
`--resume`.

**Posture:** Anthropic's legal page states that OAuth login is for subscription purchasers using
Claude Code and native Anthropic applications, that products built on Claude should use API keys,
and that third parties may not route requests through Free/Pro/Max credentials on behalf of their
users. The library runs the user's own installed `claude` under the user's own login and never
touches its token. Whether that counts as ordinary use of Claude Code is inferred from Anthropic's
enforcement pattern (token extraction was targeted, subprocess use was not), not from a written
exception. Discovery reports the auth state so a host can show its own disclosure.

**Quotes:** to be filled in plan 003 with the operative sentences and their URLs.

## OpenAI Codex (`mango-agent-codex`)

**Surface used:** `codex app-server`, the supported integration protocol (JSON-RPC over lines).
`clientInfo.name` is always the host's name, passed through `HostContext`.

**Posture:** OpenAI has publicly welcomed third-party harnesses on subscriptions (press coverage,
2026-02); the doc cites it as reported, not as a licence term.

**Quotes:** to be filled in plan 004 with the app-server documentation and the coverage cited.

## Agent Client Protocol agents (`mango-agent-acp`)

**Surface used:** ACP v1 over the official `agent-client-protocol` crate; each profile links the
agent's own documentation for its ACP mode.

**Posture:** the protocol exists for exactly this use. Per profile:

- **Cursor** (`agent acp`): documented; no third-party-harness statement found either way. The
  doc says "not found", not "permitted".
- **OpenCode**, **Gemini CLI**, **Copilot CLI**, **Goose**, the `codex-acp` and `claude-code-acp`
  shims: to be filled in plan 005, each with its documentation link.

**Quotes:** to be filled in plan 005.
