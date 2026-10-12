# Compliance posture

What each vendor's public documents say about driving their CLI from another program, and what
this library does and does not do in response. Each harness section identifies the documented
interface, the relevant vendor statements and the limits of the integration.

Facts were first read on 2026-09-12. The vendor terms quoted in the Claude Code, OpenAI Codex and
Grok sections were re-read from the vendor's own pages on 2026-09-30, and each quote carries its URL
and read date; anything else keeps the date stated where it appears. Vendor terms move, so re-verify
against the vendor's current page before relying on them. This page discloses what vendors publish
and what the library does in response; it is not a legal determination, and a host answers for its
own deployment.

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
  The same holds for the other text a vendor generates: a compaction summary (an activity's
  content) and a notice (`EventKind::Notice`) are for display only, and a host must not feed either
  to its own model as instructions.
- **Redaction.** A vendor's own diagnostic text is redacted for credential-shaped text before it is
  bounded, wherever it enters, and a vendor's transcript content is not. What is redacted where:

  | Text                                                                                        | Redacted | By                                                              |
  | ------------------------------------------------------------------------------------------- | -------- | --------------------------------------------------------------- |
  | stderr crossing a diagnostic boundary                                                       | yes      | `redact::stderr_text` (no unnamed tokens, see below)            |
  | an `EventKind::Error` message and its vendor code                                           | yes      | the core, whichever harness produced it                         |
  | the title, description and unknown severity of an `EventKind::Notice`                       | yes      | `EventKind::notice`                                             |
  | a JSON-RPC peer's error message, when it is an error object                                 | yes      | `JsonRpcError::into_vendor_error`                               |
  | a setup `Error::Vendor` message                                                             | yes      | the core, then bounded                                          |
  | the version a probe printed, a `VersionTooOld` `found`, an unsupported option's explanation | yes      | `redact::bounded`, in `Discovery::normalized` and the matrix    |
  | a session's title and preview (the conversation's own words), a login hint, an account plan | no       | content or host text, bounded only                              |
  | an ACP agent's error message and its `data`, a JSON value read as structure                 | yes      | `redact::structured`, then the text rules and the token pass    |
  | an ACP compaction error                                                                     | yes      | `redact::bounded`                                               |
  | a malformed JSON-RPC `error` member                                                         | n/a      | a fixed description; the member is never quoted                 |
  | `CallFailureCause::Peer`, the peer's message exactly as sent                                | **no**   | exact by design, so a harness may compare it; never show it raw |
  | assistant text, reasoning, tool output and tool errors (`is_error`, denial reasons)         | no       | the answer, not a diagnostic                                    |

  `CallFailureCause::Peer` holds the peer's error exactly as sent, because the caller of a request
  may need its words and its data. A harness must not show it to a host raw: it converts it with
  `JsonRpcError::into_vendor_error`, which redacts, or runs the message through
  `redact::stderr_text`. Codex recognises two refusals (a missing rollout and an unloaded thread) by
  their text, so it compares the redacted form of the answer with `stderr_text` of the expected
  text, which keeps a credential-shaped thread id from hiding the refusal.

  Beyond `name=value` and `Authorization: Bearer`, the rules read quoted pairs (`{"api_key":"sk-x"}`),
  names with a keyword at the end or after a capital (`accessToken`, `PGPASSWORD`), any authorization
  scheme (also with an equals sign), cookies, a `password:` phrase, a query-string credential, a
  token used as a URL's user, and the header names `x-functions-key` and
  `Ocp-Apim-Subscription-Key`. Text glued to the closing quote of a quoted value goes with the value
  (`password="pa"ss"word"`). A `&` ends a value only in a query string, one with a `?` before it in
  the same run of non-blank text, at any distance. After an equals sign, `authorization` takes one
  token unless its scheme is a known one; a colon makes it a header line. A quoted member named for
  a cookie is a member; a `Cookie:` or `Set-Cookie:` line is read to its end. Words that begin with a keyword and are not credentials
  (`tokenizer`, `secretary`, `passwordless`) are left alone.

  A status after a name (`OPENAI_API_KEY: not set`), a number of at most eleven digits under a name
  that counts tokens (`max_tokens: 100000`) and the rest of a query string are kept; a value that
  could be a credential is redacted, and a second credential word in the name
  (`session_token_cookie: expired`) vetoes a keep. `testing::redaction` holds the corpus of both kinds.

  What was added after the first release of these rules, and what each costs:

  - **Escaped JSON.** A quoted name and value are read at any level of escaping up to five
    strings deep (`{\\\"api_key\\\":\\\"sk-x\\\"}`): a quote opened with `n` backslashes closes at a
    quote with exactly `n`. `redact::structured` also walks a string that is itself a JSON object or
    array (up to 256 KiB, four strings deep) as structure and writes it again, compactly, when it
    found something (keys sorted, whitespace dropped, a number written as `serde_json` writes it,
    so `1E5` is `100000.0`); any other string comes back as it was.
  - **Containers and sequences.** A value that opens with `[` or `{` is read across line breaks, up
    to 64 lines and 16 KiB, so a pretty-printed array under `"tokens":` goes whole; a container
    that does not close within that, or after the pass has read 8 KiB of them beyond a byte for each
    byte of text, ends with its first line as it did. A YAML `-` after a separator marks a
    sequence of bare items: its first item and every item below it in the same column, up to 64, are
    the value. A first item that is quoted, a status or a count is read as a single value, and a
    hyphen at the end of a line is a value, not a marker.
    `ends_awaiting_value` also reports a container a credential's name opened and nothing has closed,
    so a stderr cut that drops its first line goes on dropping lines until the container closes (the
    discard window, 512 bytes, still bounds that).
  - **Counts.** Twelve or more digits (not counting `,` and `_`) under a name that counts tokens are
    a secret, in the text rules and in `redact::structured`; ten digits is the largest 32-bit
    counter, and the corpus holds none above eleven. A comma still ends a value under every name,
    so `tokens_used: 1,234,567,890,123` is redacted up to the first comma. A millisecond epoch under a
    count name (`input_tokens=1700000000000`) has twelve digits and is redacted.
  - **Well-known tokens with no name beside them**, by `redact::bounded` and `redact::structured`
    (the paths that carry a vendor's messages, errors, notices, `data` and discovery text) and not
    by `stderr_text`: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_` and `github_pat_` GitHub tokens
    ([token formats][github-tokens]); GitLab `glpat-` ([token overview][gitlab-tokens]); Slack
    `xox[bpasr]-` ([token types][slack-tokens]); Stripe `sk_`, `rk_` and `pk_` with `live_` or `test_`
    ([API keys][stripe-keys]); AWS `AKIA` and `ASIA` access key ids, exactly 20 characters
    ([identifiers][aws-identifiers]); Google `AIza` API keys and `ya29.` OAuth tokens; Anthropic and
    OpenAI `sk-ant-`, `sk-proj-`, `sk-live-` and the older 40-character `sk-` as their consoles and
    error messages print them; signed web tokens, two `eyJ` parts and a signature
    ([RFC 7519][rfc-7519]); private-key blocks from `-----BEGIN ... PRIVATE KEY-----` to the end
    marker or the end of the text ([RFC 7468][rfc-7468]); and npm, Hugging Face, DigitalOcean and
    Shopify tokens. A token starts where a run of letters, digits, `_` and `-` starts and is
    redacted with the run it is in, so `sk-learn`, `task-ant-lines` and `npm_config_cache` are
    words. Each prefix has a minimum length and an alphabet (`redact/token_shapes.rs` is the
    specification), and a body under 40 characters has a digit, so `sk-live-reload-development-server`
    is a name. The private-key blocks are read before the name rules, so a name cannot take the
    first line of a block; every other prefix is read after them, so a name that opens like a prefix
    (`sk-proj-prod-api-key: x`) still reaches the rules whole. The AWS documentation key is redacted like any other; an allowlist for it would
    be a way past the rule.
  - **A URL whose user is a credential name** (`x://token:;pw@host`) reaches its final shape in one
    call, so a host that redacts a tail twice gets the same string.

  The token pass reads the whole text a byte at a time, which costs plain stderr lines 120% more
  instructions (10.39 M to 22.5 M per 320 KiB), and a byte-class gate does not help because plain
  tails are full of `_` and `-`. Stderr tails are read line after line on the hot path, so
  `stderr_text` does not run it (its cost against the previous release is within 1% on every
  plain case, and 3.7% on text that is a credential on every line); `redact::bounded` and `redact::structured` do, at 2.2 times the instructions on
  plain text of 16 KiB. The measurements are in `docs/benchmarks.md`.

  What the rules do not read, and a host should not rely on them for:

  - a bare token in the text of a stderr tail: `stderr_text` reads names and values, not prefixes,
    so `token: expired (ghp_...)` passes through it when it is a line of stderr. The same holds for
    the other text that crosses a boundary through `stderr_text` alone: the version banners the
    harnesses print, an `ExtensionValue`'s text and the WebSocket URL text;
  - a bare token outside the closed list, and any credential with no prefix: an AWS secret access
    key (40 characters of base64) or an Azure key has nothing to recognise it by;
  - a token glued to other name characters (`_ghp_..._`, `ghp_...-`), behind an encoding (`%20ghp_...`,
    `\u003cghp_...`), or with a prefix the list does not hold; a pagination cursor made of two base64
    JSON parts and a word is a web token to the rule;
  - a private-key header quoted in prose (`expected -----BEGIN ... PRIVATE KEY----- header`) takes
    the rest of the message, since the block has no end;
  - escaping deeper than five strings, a JSON text inside a string deeper than four, or one over
    256 KiB: the walk leaves those to the text rules, which read a quoted pair at any level up to
    five;
  - a container that spans more than 64 lines or 16 KiB, one that never closes, one cut by a stderr
    tail, and a YAML sequence cut by a tail (`ends_awaiting_value` does not know its column): the rest of the container after the cut is read by the tail's own rules, which see
    no name;
  - a quoted value with an embedded quote (`'it''s'`), and, inside escaped JSON, a string that ends
    in an escaped backslash, which keeps the value open to the end of its line; a credential in percent-encoded or base64
    form, and a line break written as `%0A`;
  - a credential that is itself a kept word or a short count: `password=404`, `secret=required`,
    `tokens: 12345`. A status word after a credential's name is kept on purpose (a CLI prints
    `OPENAI_API_KEY: not set` constantly), and the number of digits that separates a count from a
    secret is a threshold: a secret of eleven digits or fewer under a count name passes;
  - a text that carried a removed byte is read again until it stops changing; a text that did not
    is read once.

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
- **Raw records and reducers.** The `Debug` boundary reaches the records decoded from a vendor's
  wire and the reducers that hold them, not only the interaction and identity carriers above. A
  record that can carry what the agent wrote or read — message and reasoning text, command text
  and output, a diff or a path, an error message, a tool's name — has a hand-written `Debug` that
  reports its kind, which members are present, counts and byte sizes, and never the text, because
  a command's output can be the contents of a `.env`. A reducer that buffers such text reports
  how much it holds, not what. A record that holds only numbers, statuses and enums keeps its
  derive, as does one that reaches text only through a field that is itself metadata-only, so
  a token-usage or rate-limit snapshot prints in full. The Codex records covered are
  `ThreadItem`, `FileUpdateChange`, `TurnError`, the item, delta, progress and patch
  notifications, and `TurnReducer`. Requests a host builds from its own input, such as a turn's
  prompt, are not carriers: a host formatting text it wrote is not a boundary this library stands
  on, the same as a `SessionId` or a `TurnId`. The exception this leaves is a host that
  hand-parses a frame with `serde_json` and prints the `Value`; a library type cannot cover that.
- **No telemetry, no listener, no downloaded binaries.**

## Claude Code (`mango-agent-claude`)

**Surface used:** the documented headless mode,
`claude --print --output-format stream-json --input-format stream-json …`, one process per turn
with `--resume`. Discovery adds three documented read-only probes: `claude --version`,
`claude --help` and `claude auth status`. Nothing else is read. See
[harness-claude.md](harness-claude.md) for every flag and the document behind it.

**Quotes** (read 2026-09-30, from
<https://code.claude.com/docs/en/legal-and-compliance>). Under "Can customers offer Claude Code in
their products?":

> Unless we’ve mutually agreed otherwise, preinstalling or running Claude Code in your products or
> services (e.g. in hosted sandboxes or other agent infrastructure) requires agreeing to our
> Commercial Terms of Service and complying with the conditions below:

> The Claude Code binary must not be modified. Claude Code must be installed and run as published by
> Anthropic, and customers may not remove, disable, or restrict any authentication method built into
> it (including methods that permit signing in with a Claude account or the user’s own API key).

> Customers may not pay for, resell, or intermediate Claude usage on their end users’ behalf. Each
> end user must authenticate with their own Anthropic API key, Claude subscription plan credentials,
> or 3P inference provider credential (Amazon Bedrock, Google Cloud’s Agent Platform, Microsoft
> Foundry).

Under "Authentication and credential use":

> OAuth authentication is intended exclusively for purchasers of Claude Free, Pro, Max, Team, and
> Enterprise subscription plans and is designed to support ordinary use of Claude Code and other
> native Anthropic applications.

> Developers building products or services that interact with Claude’s capabilities, including those
> using the Agent SDK, should use API key authentication through Claude Console or a supported cloud
> provider. Anthropic does not permit third-party developers to offer Claude.ai login into their own
> applications, or to route requests through Free, Pro, or Max plan credentials on behalf of their
> users. Moreover, developers may not collect, store, or intermediate Claude.ai credentials or
> session tokens — sign-in to a Claude account must complete through Anthropic’s own flow.

> This does not restrict how customers provision and manage their own API keys or third-party
> inference provider credentials — for example, configuring an API key in a development
> environment, secrets manager, or machine image for use by the customer’s own authorized users —
> provided the resulting usage is billed to the key owner under their agreement with Anthropic (or
> the applicable provider) and is not resold or intermediated as described above.

> Nor does it prevent an end user from signing in to the unmodified Claude Code binary with their own
> Claude subscription, including where a platform hosts Claude Code as described under Can customers
> offer Claude Code in their products? above.

> Anthropic reserves the right to take measures to enforce these restrictions and may do so without
> prior notice.

The contractual backstop is in the Consumer Terms themselves
(<https://www.anthropic.com/legal/consumer-terms>, read 2026-09-13 and not re-read for this update),
among the prohibited uses:

> Except when you are accessing our Services via an Anthropic API Key or where we otherwise
> explicitly permit it, to access the Services through automated or non-human means, whether
> through a bot, script, or otherwise.

**Posture:** the library runs the user's own installed `claude`, under whatever the user already
logged into with the vendor's own CLI, and never reads, stores, copies or forwards its token. It
offers no Claude.ai login of its own and routes no request anywhere — the vendor's binary talks to
the vendor. Anthropic now publishes written conditions for running Claude Code inside a product or
service, quoted above; this page reads them against what the library does and leaves the verdict on
a host's deployment to the host. The page speaks of customers who preinstall or run Claude Code in
their products or services. It does not name a library that spawns the user's own binary, so this
page claims no permission the vendor did not write down.

- **Deployment scope.** A host that preinstalls or runs Claude Code in its own product or service,
  for example in a hosted sandbox, is the party the conditions address: it needs the Commercial
  Terms and must meet the conditions. A host that only drives the `claude` a user installed and
  signed into on their own machine should still read them.
- **The library does not modify the binary.** It launches the executable that the host's
  `ExecutablePath`, or the bare `claude` name its launcher resolves, points at, and patches, wraps
  or replaces nothing in it. It does not check that the executable is Anthropic's own build:
  supplying and verifying the official, unmodified binary is the host's job.
- **No built-in authentication method is removed, disabled or restricted by a flag.** The argv adds
  no option that selects or drops a sign-in method. `--bare`, which would skip OAuth and the keychain
  and authenticate only from `ANTHROPIC_API_KEY` or an `apiKeyHelper`, is deliberately not passed
  (`docs/harness-claude.md`).
- **The child environment is the library's own allowlist, and it excludes API-key variables.** A
  vendor child receives a fixed set of location and locale variables plus the ones its harness
  names, which for Claude are `CLAUDE_CONFIG_DIR` and `CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS`. A host
  cannot add a key. `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN` and `ANTHROPIC_AUTH_TOKEN` are
  deliberately not on it, because the library forwards no credential. So an API key supplied
  through the environment does not reach the child under this library, whatever the host's own
  environment holds; a user who wants API-key mode configures it in `claude` itself. That is a limit
  on how a credential can reach the binary, and it bears on the quoted sentence that customers "may
  not remove, disable, or restrict any authentication method built into it (including methods that
  permit signing in with a Claude account or the user’s own API key)". This page draws no
  conclusion: the sentence is addressed to customers running Claude Code in their products, and
  whether an environment allowlist counts is Anthropic's to say. The written allowance for
  customer-managed API keys, quoted above, concerns how customers provision keys for their own
  authorized users; a host that relies on it configures the key in `claude` itself, because the
  library will not carry it.
- **No credential or session token is collected, stored or intermediated.** Sign-in completes
  through Anthropic's own flow: a signed-out CLI fails a turn with `AuthRequired`, which carries
  `claude auth login` as text for a person to run.
- **No resale or intermediation by the library.** It routes no request and pays for none. It does
  not establish whose account the binary authenticates with: discovery reports the account mode
  only, and a host can point `CLAUDE_CONFIG_DIR` at shared configuration. Ensuring that every end
  user authenticates with their own account is the host's job.

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

**Debug output.** The raw `stream-json` records and the reducer that holds them are carriers under
the `Debug` policy above, because a `tool_result` body arrives in them and a `Read` of a `.env` is
that body. `StreamRecord`, its borrowed views (`InitRecord`, `PermissionDenied`, `ContentBlock`,
`StreamEvent`, `Delta`, `ResultRecord`) and `TurnReducer` report a record's kind, its member counts
and byte sizes, never a value, a call id or a denial's message.

## OpenAI Codex (`mango-agent-codex`)

**Surface used:** `codex app-server`, the interface OpenAI documents for rich clients and ships its
own VS Code extension on. JSON-RPC over newline-delimited JSON. `clientInfo.name` is always the
host's name, passed through `HostContext`. Read on 2026-09-13 against `codex-cli 0.154.0`;
`docs/harness-codex.md` lists every method driven and the document each follows.
Host-configured MCP servers use the app-server's per-thread `config` override on
`thread/start` and `thread/resume`; no persistent Codex configuration is edited.

**Posture:** OpenAI's documentation states a deployment limit on app-server authentication (quoted
below): it is for local or open-source applications and "has never been permitted for commercial or
hosted services". What the vendor documents beyond that is the interface: the app-server README
describes the protocol as the way to build a rich client on Codex, and asks that such clients
identify themselves through `clientInfo`. This harness runs the user's own installed `codex` under
the user's own login and never touches its token.

**Deployment scope.** A host that is a commercial or hosted service must not rely on app-server
authentication, which is what a user's local `codex` login reached through `codex app-server`
amounts to. OpenAI names Sign in with ChatGPT as its route for those cases; this library implements
neither and never authenticates. It cannot tell what kind of service its host is, so the disclosure
is here and the decision is the host's. OpenAI has also publicly welcomed third-party harnesses on
subscriptions (press coverage, 2026-02); that is cited as reported, not as a licence term, and it
does not displace the published sentence.

**Auth policy quotes** (read 2026-09-30, from <https://learn.chatgpt.com/docs/app-server>,
section "Auth endpoints"):

> If you’ve built a local or open-source application using Codex app-server authentication, you can
> continue using it, though we recommend migrating to Sign in with ChatGPT so users have greater
> control over and visibility into their usage. [...] App-server authentication has never been
> permitted for commercial or hosted services.

From the same page's transport notes:

> The app-server command and WebSocket transport are experimental and aren’t supported for
> production workloads.

**Restricting the vendor's own authentication.** The harness starts `codex app-server` with no
authentication option and sends no login request; the one server request that asks a client for
credentials, `account/chatgptAuthTokens/refresh`, is refused. As with Claude, the child environment
is the library's own allowlist, and the only vendor variable on it is `CODEX_HOME`, so a credential
in a host's environment is not forwarded. The sentence quoted above does not address that, and this
page draws no conclusion.

**Interface quotes**, read against `rust-v0.154.0`, from
[`codex-rs/app-server/README.md`](https://github.com/openai/codex/blob/rust-v0.154.0/codex-rs/app-server/README.md)
at that tag:

- "`codex app-server` is the interface Codex uses to power rich interfaces such as the Codex VS
  Code extension."
- "Applications building on top of `codex app-server` should identify themselves via the
  `clientInfo` parameter." — followed by: "`clientInfo.name` is used to identify the client for the
  OpenAI Compliance Logs Platform." The host's own name is therefore passed through unchanged; the
  library never substitutes its own.
- "Websocket transport is currently experimental and unsupported. Do not rely on it for production
  workloads." — which is why the harness declares `stdio` only.

**What this harness reads about an account:** `account/read`, and only the account *kind* plus, for
a ChatGPT sign-in, the plan name and — only when the host calls
`CodexHarness::discover_with_account` with a key of its own — the email as the input to a keyed
digest. The email is personal data, not a credential: it is read from the same documented,
non-secret `account/read` answer, used for exactly one HMAC-SHA256 under the host's key, and then
dropped. It is never modelled in a protocol type, stored in a returned value, formatted into a
diagnostic, or forwarded; what the host receives is the digest, which is meaningless without the key
that stays on the host's machine. A test asserts nothing of the address survives. No token is read,
stored, copied or forwarded, and `~/.codex/auth.json` is never opened. The
server's `account/chatgptAuthTokens/refresh` request — which asks a client to hand over a refreshed
credential — is refused with a JSON-RPC error, unread.

**What this harness answers rather than errors.** Three of the server's other requests now get a
real answer, because a method-not-found tells a server the client is broken rather than that it
declined. An MCP elicitation is answered `decline` — the library renders no arbitrary form, and its
`message`, `requestedSchema` and `content` are never deserialised at all, because a form's fields
can name a credential. An `item/permissions/requestApproval` is brokered and answered with the
requested profile echoed back verbatim under the scope the vendor declares, or with a profile that
grants nothing; nothing is synthesised or widened. An `item/tool/requestUserInput` becomes a typed
question that grants no authority, and a round carrying a question the vendor marks `isSecret` is
refused whole and never shown to a host. `item/tool/call` remains refused: vendor tools never enter
the host's tool registry.

**Protocol types:** hand-written against OpenAI's own published schema rather than vendored from
its source tree. `crates/mango-agent-codex/vendor/` holds the schema inventory
`codex app-server generate-json-schema` produced at the pinned tag, under the Apache-2.0 notice in
`vendor/NOTICE`; no OpenAI source code is copied into this repository.
`docs/harness-codex.md` records the measurement behind that decision.

**Branding:** nominative use only. "Codex" names the CLI being launched; no logos, no wordmarks,
and nothing implying an official or endorsed integration.

## Agent Client Protocol agents (`mango-agent-acp`)

**Surface used:** ACP v1, with the wire types of the official `agent-client-protocol-schema` crate
(every `unstable_*` feature off) carried by this library's own JSON-RPC 2.0 peer. Every method driven is listed with its specification page in
[harness-acp.md](harness-acp.md#the-surface-driven). Profile facts were read on 2026-09-13.

The client advertises three session capabilities, all of which only let an agent report more:
`configOptions.boolean`, `notices` and `compaction`. The last two follow the
[session notices](https://agentclientprotocol.com/rfds/session-notices) and
[session compaction](https://agentclientprotocol.com/rfds/session-compaction) RFDs, stable in v1 from
schema 1.11 and read on 2026-10-10. What they carry is vendor-generated text, surfaced for display
only under the rule above.

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
[github-tokens]: https://docs.github.com/en/authentication/keeping-your-account-and-data-secure/about-authentication-to-github#githubs-token-formats
[gitlab-tokens]: https://docs.gitlab.com/security/tokens/
[slack-tokens]: https://api.slack.com/authentication/token-types
[stripe-keys]: https://docs.stripe.com/keys
[aws-identifiers]: https://docs.aws.amazon.com/IAM/latest/UserGuide/reference_identifiers.html
[rfc-7519]: https://www.rfc-editor.org/rfc/rfc7519
[rfc-7468]: https://www.rfc-editor.org/rfc/rfc7468

Some profiles need a note beyond the link:

- **Grok's legal entity is SpaceXAI LLC.** "SpaceXAI" is the brand and the profile's `company`
  value. The party to SpaceXAI's terms is the entity, named in
  <https://x.ai/legal/terms-of-service> (Terms of Service - Consumer, last updated September 11,
  2026, read 2026-09-30): "These Terms form an agreement between you and SpaceXAI LLC (“SpaceXAI,”
  “we,” “our,” or “us”) when you accept these Terms or when you otherwise access, interact with,
  and/or use the platform. SpaceXAI LLC is a Nevada company". The business terms at
  <https://x.ai/legal/terms-of-service-enterprise> (last updated August 14, 2026) are "entered into
  between SpaceXAI LLC (“SpaceXAI”) and the business customer". That re-read looked for the entity
  only; it is not a finding about third-party harnesses, so the "not found" above stands.
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
- **`codex-acp` proxies to Codex's own auth.** The app-server authentication scope quoted in the
  OpenAI Codex section applies to what that adapter reaches: local or open-source use, not a
  commercial or hosted service. OpenAI has also publicly welcomed third-party harnesses on
  subscriptions (press coverage, 2026-02); the posture cites that as reported, not as a licence
  term. The profile lets a host's `NO_BROWSER` through by name, the adapter's own documented switch
  against opening a sign-in page; the library sets nothing else about authentication.
- **`claude-agent-acp` drives the user's own `claude`**, so the Claude Code section above applies
  unchanged. The package moved twice and the older `@zed-industries/claude-code-acp` is orphaned.

Only `cursor` and `grok` answer `AcpProfile::is_verified()`, and each carries two records: the live
`tests/smoke.rs` run that earned it, and the `initialize` handshake captured beside it under
`fixtures/acp/`. `opencode` carries the committed-capture record alone — its handshake was captured,
no session was opened, and it is not verified. Every other entry is documented and nobody has driven
the agent.

Neither `cursor-agent` nor `grok` has a pinned installer in `scripts/install-vendor-cli.sh`, so their
captures declare `reproducible: false` and name the build and the day instead. CI cannot reproduce
them; they are a maintainer's record of one machine, held to the tree by their digests like every
other capture. A host can say all of this in its own interface; see `docs/harness-acp.md`.

**Quotes:** none of these vendors publishes an operative sentence about third-party harnesses for its
ACP mode, so there is nothing to quote.
