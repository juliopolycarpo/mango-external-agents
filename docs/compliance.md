# Compliance posture

What each vendor's public documents say about driving their CLI from another program, and what
this library does and does not do in response. Each harness section identifies the documented
interface, the relevant vendor statements and the limits of the integration.

Facts were read on 2026-09-12; re-verify against the vendor's current page before relying on them.

## Invariants for every harness

- **No login handling.** The library never authenticates a vendor, never opens a browser, never
  stores, reads, copies or forwards a vendor login token, and offers no "log in through mango-external-agents".
  A host may explicitly supply MCP server headers or environment values through the documented
  session configuration surface; those values are not vendor login credentials and never enter
  the vendor child's ambient environment or diagnostics.
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
  `Debug` and `Display` for process, transport, MCP, JSON-RPC, error, session-listing, stream-handle
  and event payload carriers report only safe metadata. The same policy covers discovery and receipts,
  configuration, interactions, identity containers and extension values; callers handle originals through
  typed fields rather than diagnostics. The exceptions are the refusals whose whole content is an
  instruction: a host configuration refusal, a launch failure, a link failure, a protocol refusal
  and a timeout name the payload-free summary the library crates themselves wrote — a structured
  `io::ErrorKind`, an HTTP status, a duration, a fixed protocol operation or a static phrase, never a path, a
  URL, a vendor message or a line a program printed — so the operator can see what to fix. The
  pinned minimum version is named because it is a library constant; a login hint remains a typed
  value for the host UI and is not diagnostic text. A launch failure still reports its executable through
  `redact::program_name`, and a link failure never names its peer at all, because a WebSocket
  transport puts the dialled URL there. The guarantee is about carriers — types that hold a value
  among others, where a derived `Debug` would print it as a side effect. `SessionId` and `TurnId`
  are not carriers but the ids themselves: they are the host's own, minted by the host and printed
  for it, and a host formatting a value it created is not a boundary this library stands on.
  `ErrorCode` is the other side of that line: also host- or vendor-filled, but
  written into sentences the library composes and a third party may read, so `Display` and `Debug`
  both write it only when it has a label's shape — at most 48 bytes of `a-z`, `0-9`, `-` and `_` — and write
  `vendor-code` when it does not. `Debug` is named because it is the formatter a
  derive would silently leave open, and because an assertion made through an `Error` cannot see
  it: `Error`'s own `Debug` forwards to its `Display`. `as_str` stays the unbounded protocol field.
  `HarnessId`, `ProtocolFamily` and `ProfileId` validate a bounded identifier grammar before
  construction; their explicit `Display` preserves the registered spelling and their `Debug`
  omits it.
  `examples/mea` is an unpublished smoke tool, not part of the guarantee either: its own refusals
  name the paths and OS messages its operator needs.
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

**Surface used:** `codex app-server`, the interface OpenAI documents for rich clients and ships its
own VS Code extension on. JSON-RPC over newline-delimited JSON. `clientInfo.name` is always the
host's name, passed through `HostContext`. Read on 2026-09-13 against `codex-cli 0.154.0`;
`docs/harness-codex.md` lists every method driven and the document each follows.
Host-configured MCP servers use the app-server's per-thread `config` override on
`thread/start` and `thread/resume`; no persistent Codex configuration is edited.

**Posture:** OpenAI has publicly welcomed third-party harnesses on subscriptions (press coverage,
2026-02); this is cited as reported, not as a licence term. What the vendor *does* document is the
interface: the app-server README describes the protocol as the way to build a rich client on Codex,
and asks that such clients identify themselves through `clientInfo`. This harness runs the user's
own installed `codex` under the user's own login and never touches its token.

**Quotes**, from
[`codex-rs/app-server/README.md`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server/README.md)
at `rust-v0.154.0`:

- "`codex app-server` is the interface Codex uses to power rich interfaces such as the Codex VS
  Code extension."
- "Applications building on top of `codex app-server` should identify themselves via the
  `clientInfo` parameter." — followed by: "`clientInfo.name` is used to identify the client for the
  OpenAI Compliance Logs Platform." The host's own name is therefore passed through unchanged; the
  library never substitutes its own.
- "Websocket transport is currently experimental and unsupported. Do not rely on it for production
  workloads." — which is why the harness declares `stdio` only.

**What this harness reads about an account:** `account/read`, and only the account *kind* plus, for
a ChatGPT sign-in, the plan name. The email that call also returns is not modelled; a test asserts
nothing of it survives into a value the harness holds. `~/.codex/auth.json` is never opened. The
server's `account/chatgptAuthTokens/refresh` request — which asks a client to hand over a refreshed
credential — is refused with a JSON-RPC error, unread.

**Protocol types:** hand-written against OpenAI's own published schema rather than vendored from
its source tree. `crates/mango-agent-codex/vendor/` holds the schema inventory
`codex app-server generate-json-schema` produced at the pinned tag, under the Apache-2.0 notice in
`vendor/NOTICE`; no OpenAI source code is copied into this repository.
`docs/harness-codex.md` records the measurement behind that decision.

**Branding:** nominative use only. "Codex" names the CLI being launched; no logos, no wordmarks,
and nothing implying an official or endorsed integration.

## Agent Client Protocol agents (`mango-agent-acp`)

**Surface used:** ACP v1 over the official `agent-client-protocol` crate, `unstable_protocol_v2` and
every other draft feature off. Every method driven is listed with its specification page in
[harness-acp.md](harness-acp.md#the-surface-driven). Profile facts were read on 2026-09-13.

**Posture:** the protocol exists for exactly this use — it is published by its authors as the way a
client drives an agent, and every profile here is an agent that ships an ACP mode of its own accord.
That is a statement about the protocol, not a licence from each vendor; the per-profile notes below say
what was and was not found.

**What this harness does not do:**

- **No login.** `authenticate` is never sent, no browser is opened, and no vendor login credential is
  read, stored or forwarded. `AuthState` is always `Unknown` for every ACP agent, because ACP's `initialize` reports
  which auth *methods* exist and has no field for whether anyone is signed in. A signed-out agent
  surfaces as `session/new` answering `-32000`, which becomes `Error::AuthRequired` carrying the
  agent's own login command as text for a person to run.
- **No downloaded binaries.** Neither npm shim is launched through `npx -y`, even though both READMEs
  document that form for a person to run: a library that invoked a package fetcher would be downloading
  a binary on its own initiative. Both profiles launch the installed binary, and a test asserts no
  built-in argv names a fetcher.
- **No host filesystem or terminal for the agent.** `clientCapabilities.fs` and `.terminal` are
  declined, so a vendor-initiated file or terminal request is answered with a JSON-RPC error and never
  executed. Every agent here uses its own tools instead.
- **Host-configured MCP servers only.** `session/new.mcpServers` and `session/load.mcpServers` carry
  only the servers the host explicitly supplied. Stdio command, arguments and server-only
  environment follow ACP v1 session setup; HTTP endpoints and headers are used only when the agent
  advertises `mcpCapabilities.http`. Malformed entries are refused before launch, and the library
  does not edit persistent agent configuration. See [ACP session setup](https://agentclientprotocol.com/protocol/v1/session-setup).

**Per profile.** Every agent below documents its own ACP mode, and for none of them was a statement
about third-party harnesses found either way — "not found" is the finding, not "permitted".

| Agent              | Owner                         | ACP mode documented at                             |
| ------------------ | ----------------------------- | -------------------------------------------------- |
| Cursor CLI         | Anysphere                     | [cursor.com/docs/cli/acp][cursor-acp]              |
| Grok Build         | SpaceXAI                      | [Grok Headless & Scripting][grok-acp]              |
| OpenCode           | Anomaly Innovations           | [opencode.ai/docs/acp][opencode-acp]               |
| Gemini CLI         | Google                        | [gemini-cli/docs/cli/acp-mode.md][gemini-acp]      |
| GitHub Copilot CLI | GitHub                        | [ACP server reference][copilot-acp]                |
| Goose              | Block                         | [block.github.io/goose ACP protocol][goose-acp]    |
| `codex-acp`        | OpenAI's CLI, via the adapter | [agentclientprotocol/codex-acp][codex-acp]         |
| `claude-agent-acp` | Claude Code, via the adapter  | [agentclientprotocol/claude-agent-acp][claude-acp] |

[cursor-acp]: https://cursor.com/docs/cli/acp
[grok-acp]: https://docs.x.ai/build/cli/headless-scripting
[opencode-acp]: https://opencode.ai/docs/acp/
[gemini-acp]: https://github.com/google-gemini/gemini-cli/blob/main/docs/cli/acp-mode.md
[copilot-acp]: https://docs.github.com/copilot/reference/copilot-cli-reference/acp-server
[goose-acp]: https://block.github.io/goose/docs/advanced/acp-protocol
[codex-acp]: https://github.com/agentclientprotocol/codex-acp
[claude-acp]: https://github.com/agentclientprotocol/claude-agent-acp

Some profiles need a note beyond the link:

- **Grok's example includes ACP `authenticate`.** This harness never sends it and never forwards
  `XAI_API_KEY`. It only supports the installed CLI when `initialize` and `session/new` can reuse
  the user's existing local login. The profile disables background updates with the documented
  `--no-auto-update` flag. A smoke turn passed against Grok 1.0.30 on 2026-09-13 without
  `authenticate`; that is an observation of this build, not a documented promise about other builds.

- **Goose has no corporate terms of service.** It is Apache-2.0 and brings the user's own model
  credentials, and Block publishes no page covering it — only product-specific terms for unrelated
  products. Its `terms_url` therefore points at the project's own
  [acceptable-usage document](https://github.com/block/goose/blob/main/ACCEPTABLE_USAGE.md), which is
  the document that does govern the tool, rather than at a page that does not.
- **Gemini's terms and privacy vary by the authentication method the user chose.** The profile links
  Google's general pages; a host whose disclosure has to be exact should read Gemini CLI's own
  terms-and-privacy index.
- **`codex-acp` proxies to Codex's own auth.** OpenAI has publicly welcomed third-party harnesses on
  subscriptions (press coverage, 2026-02); the posture cites that as reported, not as a licence term.
- **`claude-agent-acp` drives the user's own `claude`**, so the Claude Code section above applies
  unchanged. The package moved twice and the older `@zed-industries/claude-code-acp` is orphaned.

Every profile except `cursor` and `grok` is `verified: false`: the entry is documented but nobody has driven the
agent. A host can say so in its own interface.

**Quotes:** none of these vendors publishes an operative sentence about third-party harnesses for its
ACP mode, so there is nothing to quote.
