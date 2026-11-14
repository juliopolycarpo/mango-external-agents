//! `mea`: the unpublished smoke and capture CLI for mango-external-agents.
//!
//! `discover` and `doctor` probe installed CLIs without handling login. `turn` drives a
//! conversation with terminal approvals and optional NDJSON. `capture` records public contracts
//! and explicit archival transcripts. The binary is built from source and is not published.

mod ask;
mod capture;
mod doctor;
mod failure;
#[cfg(test)]
mod json_schema_tests;
mod options;
mod redact;
mod terminal;
mod turn;
mod workspace;

use std::process::ExitCode;
use std::sync::Arc;

use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::{
    ApprovalRouting, AuthState, ConfigurationChange, EnvSource, ExecutablePath, Harness, HarnessId,
    HarnessRegistry, HostContext, OpenSession, TransportKind, TransportSelection, TurnRequest,
};
use options::HarnessChoice;

/// Every harness this binary links, as the line the banner prints for it.
///
/// The two native harnesses print the id they register under. The ACP crate publishes no single
/// id — one harness drives one agent, so every id names a profile too — so it prints its protocol
/// family, and the banner's own `ACP profiles:` line enumerates the rest.
fn harness_lines() -> [String; 3] {
    [
        mango_agent_claude::harness_id().to_string(),
        mango_agent_codex::harness_id().to_string(),
        format!(
            "{} (one id per profile)",
            mango_agent_acp::protocol_family()
        ),
    ]
}

/// The harnesses this binary can dispatch to.
///
/// Claude, Codex and every built-in ACP profile implement the shared `Harness` contract.
fn registry() -> HarnessRegistry {
    registry_attaching_to(None)
}

/// [`registry`], with Codex pointed at the Unix socket of an app-server the host already runs.
///
/// The path is handed to the harness as typed: its own checks (fits a socket address, served by
/// this user) answer for it. `mea` starts no server; running `codex app-server --listen
/// unix://PATH` is the operator's job.
fn registry_attaching_to(socket: Option<&std::path::Path>) -> HarnessRegistry {
    registry_with(socket, mango_agent_claude::harness())
}

/// [`registry_attaching_to`], with the Claude harness the caller built, such as one that carries a
/// `--json-schema`.
fn registry_with(socket: Option<&std::path::Path>, claude: Arc<dyn Harness>) -> HarnessRegistry {
    let codex = match socket {
        Some(path) => mango_agent_codex::CodexHarness::new().with_unix_socket(path),
        None => mango_agent_codex::CodexHarness::new(),
    };
    let mut harnesses = vec![claude, Arc::new(codex)];
    harnesses.extend(mango_agent_acp::AcpHarness::builtins());
    HarnessRegistry::new(harnesses).expect("expected one harness per kind")
}

/// A context with the default launcher, this process's environment and this directory.
///
/// `mea` is the host here, so it answers the three questions a host owes the library: how to spawn,
/// which directory is authorised, and who it says it is. The environment allowlist still applies —
/// reading this process's environment is not the same as passing it on.
fn host(cwd: Option<&std::path::Path>) -> Result<HostContext, String> {
    HostContext::builder()
        .launcher(Arc::new(TokioLauncher::new()))
        .cwd(match cwd {
            Some(path) => path.canonicalize().map_err(|error| {
                format!("expected an existing working directory {path:?}, received {error}")
            })?,
            None => {
                std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?
            }
        })
        // Unix validation accepts the platform temporary root only when its root ownership and
        // sticky bit protect session leaves. Product hosts choose their own authorised,
        // child-visible scratch root instead.
        .scratch(std::env::temp_dir())
        .environment(EnvSource::from_process())
        .client_info("mea", env!("CARGO_PKG_VERSION"))
        .build()
        .map_err(|error| error.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match run(&arguments).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mea: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(arguments: &[String]) -> Result<(), String> {
    let Some((command, rest)) = arguments.split_first() else {
        print_banner();
        return Ok(());
    };
    if rest
        .iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print_banner();
        return Ok(());
    }
    match command.as_str() {
        "discover" => discover(&Options::parse_for(command, rest)?).await,
        "doctor" => doctor(&Options::parse_for(command, rest)?).await,
        "turn" => turn(&Options::parse_for(command, rest)?).await,
        "capture" => capture(rest).await,
        "digests" => digests(rest),
        "help" | "--help" | "-h" => {
            print_banner();
            Ok(())
        }
        other => Err(format!(
            "expected `discover`, `doctor`, `turn`, `capture` or `digests`, received {other:?}"
        )),
    }
}

fn print_banner() {
    println!(
        "mea {} (mango-external-agents {})",
        env!("CARGO_PKG_VERSION"),
        mango_external_agents::VERSION
    );
    for line in harness_lines() {
        println!("harness: {line}");
    }
    println!(
        "usage: mea discover|doctor [--harness claude|codex|acp:<profile>] [--json] [--cwd DIR]"
    );
    println!(
        "       mea turn [--harness claude|codex|acp:<profile>] [--profile ID] [--transport stdio|websocket|unix-socket|acp] [--socket PATH] [--model ID] [--json-schema FILE] [--cwd DIR] [--json] [--level read-only|default|full-access] [--cancel-after SECONDS] <prompt>"
    );
    println!(
        "       mea capture --harness claude|codex|acp[:profile] [--profile ID] [--out DIR] [--workspace DIR] [--transcripts [--model ID]]"
    );
    println!(
        "       mea capture codex [--scenario NAME] [--model ID] [--out DIR] [--workspace DIR] (archival transcripts)"
    );
    println!(
        "       mea capture --harness claude --transcripts --scenario structured-output --json-schema FILE [--out DIR] [--workspace DIR] (archival transcript)"
    );
    println!(
        "       mea digests [--out DIR] [--check] (rewrite or verify capture manifest digests)"
    );
    println!("ACP profiles: {}", acp_profile_ids().join(", "));
}

fn acp_profile_ids() -> Vec<String> {
    mango_agent_acp::builtin_profiles()
        .into_iter()
        .map(|profile| profile.id.to_string())
        .collect()
}

/// One profile's state in the three words the smoke test asks for.
fn describe(discovery: &mango_external_agents::Discovery) -> String {
    use mango_external_agents::GateVerdict;
    match &discovery.gate {
        GateVerdict::NotInstalled => String::from("missing"),
        GateVerdict::VersionTooOld { found, minimum } => {
            format!("gated      {found} is older than {minimum}")
        }
        GateVerdict::MissingRequiredSurface { expected, received } => {
            format!("gated      expected {expected}; received {received}")
        }
        GateVerdict::Usable => match &discovery.version {
            Some(version) => format!("installed  {version}"),
            None => String::from("installed"),
        },
        // `Unknown`, and whatever the core adds next: a verdict this build cannot read is the same
        // honest answer as one that says the probe could not tell.
        _ => match &discovery.version {
            Some(version) => format!("unknown    started, reported {version}"),
            None => String::from("unknown    started, printed no version"),
        },
    }
}

use options::Options;

struct DiscoveryReport {
    kind: HarnessId,
    result: Result<mango_external_agents::Discovery, String>,
}

async fn discover_with(
    registry: &HarnessRegistry,
    host: &HostContext,
    selected: Option<HarnessId>,
) -> Result<Vec<DiscoveryReport>, String> {
    let harnesses = match selected {
        Some(id) => vec![(
            id.clone(),
            registry
                .require(&id)
                .map_err(|error| error.to_string())?
                .clone(),
        )],
        None => registry
            .ids()
            .into_iter()
            .map(|id| {
                Ok((
                    id.clone(),
                    registry
                        .require(id)
                        .map_err(|error| error.to_string())?
                        .clone(),
                ))
            })
            .collect::<Result<Vec<_>, String>>()?,
    };
    // Probed concurrently, in registry order. Each probe spawns a child and waits on it for up to
    // `Limits::request_timeout`, and the probes share nothing — run one after another, a sweep over
    // ten harnesses costs ten timeouts instead of one.
    let probes: Vec<(HarnessId, tokio::task::JoinHandle<_>)> = harnesses
        .into_iter()
        .map(|(kind, harness)| {
            let host = host.clone();
            (
                kind,
                tokio::spawn(async move {
                    harness
                        .discover(&host)
                        .await
                        .map_err(|e| command_failure(&e))
                }),
            )
        })
        .collect();
    let mut reports = Vec::with_capacity(probes.len());
    for (kind, probe) in probes {
        reports.push(DiscoveryReport {
            kind,
            // Folded into the row rather than into the sweep: one harness that panicked is one
            // line that says so, not nine probes nobody gets to read.
            result: probe.await.unwrap_or_else(|error| Err(error.to_string())),
        });
    }
    Ok(reports)
}

async fn discover(options: &Options) -> Result<(), String> {
    let host = host(options.cwd.as_deref())?;
    let registry = registry();
    let mut reports = discover_with(
        &registry,
        &host,
        options.kind.as_ref().map(HarnessChoice::id),
    )
    .await?;
    if options.json {
        println!("{}", doctor::json(&reports));
        return Ok(());
    }
    let Some(kind) = &options.kind else {
        for report in reports {
            let line = match report.result {
                Ok(discovery) => describe(&discovery),
                Err(error) => format!("error      {error}"),
            };
            println!("{:<24} {line}", report.kind.to_string());
        }
        return Ok(());
    };
    let discovery = reports
        .pop()
        .expect("expected one report for an explicit harness")
        .result?;

    println!("harness:      {kind}");
    println!("installed:    {:?}", discovery.version);
    println!("gate:         {:?}", discovery.gate);
    println!("auth:         {:?}", discovery.auth);
    println!("usable:       {}", discovery.is_usable());
    println!("capabilities: {:?}", discovery.capabilities);
    println!(
        "models:       {:?}",
        discovery
            .models
            .iter()
            .map(|model| model.id.as_str())
            .collect::<Vec<_>>()
    );
    println!("probed permission matrix:");
    for cell in discovery.permission_matrix.cells() {
        println!(
            "  {:?} / {:?}: {}{}",
            cell.level,
            cell.routing,
            if cell.supported { "yes" } else { "no" },
            cell.unsupported_reason
                .as_ref()
                .map(|reason| format!(" ({reason:?})"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

async fn doctor(options: &Options) -> Result<(), String> {
    let reports = discover_with(
        &registry(),
        &host(options.cwd.as_deref())?,
        options.kind.as_ref().map(HarnessChoice::id),
    )
    .await?;
    if options.json {
        println!("{}", doctor::json(&reports));
    } else {
        for report in &reports {
            println!("{}", doctor::text(report));
        }
    }
    Ok(())
}

async fn turn(options: &Options) -> Result<(), String> {
    if options.prompt.trim().is_empty() {
        return Err(String::from("expected a prompt, received none"));
    }
    turn_with(
        &turn_registry(options)?,
        &host(options.cwd.as_deref())?,
        options,
    )
    .await
}

/// The registry one `mea turn` dispatches through.
///
/// `--json-schema` is read here, before anything is discovered or spawned, and handed to
/// [`ClaudeHarness::with_json_schema`](mango_agent_claude::ClaudeHarness::with_json_schema), whose
/// refusal (not JSON, not an object, too large for a command line) is returned as it is. The flag
/// is only accepted with the Claude harness, which `Options::parse_for` has already checked.
fn turn_registry(options: &Options) -> Result<HarnessRegistry, String> {
    let claude: Arc<dyn Harness> = match &options.json_schema {
        Some(path) => Arc::new(
            mango_agent_claude::ClaudeHarness::new()
                .with_json_schema(&options::read_json_schema_file(path)?)
                .map_err(|error| error.to_string())?,
        ),
        None => mango_agent_claude::harness(),
    };
    Ok(registry_with(options.socket.as_deref(), claude))
}

/// One turn through the harness the options select, in the host and registry it is given.
async fn turn_with(
    registry: &HarnessRegistry,
    host: &HostContext,
    options: &Options,
) -> Result<(), String> {
    let kind = options.kind.clone().unwrap_or(HarnessChoice::Claude);
    let harness = registry.require(&kind.id()).map_err(|e| e.to_string())?;
    if let Some(transport) = options.transport {
        harness
            .descriptor()
            .require_transport(&transport)
            .map_err(|error| error.to_string())?;
    }

    // An attach spawns nothing, so the local `codex` has nothing to gate: the server's own
    // `initialize` answer carries its version, and its account read refuses a signed-out server.
    let discovery = if options.transport == Some(TransportKind::UnixSocket) {
        None
    } else {
        let discovery = harness
            .discover(host)
            .await
            .map_err(|e| command_failure(&e))?;
        if let AuthState::LoggedOut { login_hint } = &discovery.auth {
            return Err(format!("not signed in; run `{login_hint}`"));
        }
        Some(discovery)
    };

    let request = session_request(options, discovery.as_ref());

    // A gate refusal reaches an operator here rather than through `discovery.gate`: Codex names
    // its build in the app-server handshake, which happens inside `open_session`.
    let session = harness
        .open_session(host, request)
        .await
        .map_err(|e| command_failure(&e))?;
    eprintln!("session: {}", session.ids().native_session_id);
    eprintln!("{}", transport_report(session.snapshot().transport));

    turn::run_with_format(
        session.as_ref(),
        TurnRequest::new("mea-turn-1", options.prompt.clone()),
        turn::TurnOptions {
            json: options.json,
            cancel_after: options.cancel_after,
        },
    )
    .await
    .map_err(|error| command_failure(&error))
}

/// The session `mea turn` asks the harness to open for these options.
///
/// `--model` becomes the configuration's `model`, which the Codex harness sends as the `model`
/// member of `thread/start`. Nothing here writes the vendor's own configuration.
fn session_request(
    options: &Options,
    discovery: Option<&mango_external_agents::Discovery>,
) -> OpenSession {
    let mut request = OpenSession::new(format!("mea-{}", uuid_like()));
    if let Some(level) = options.level {
        request.configuration.level = ConfigurationChange::Set(level);
        // The CLI only exposes the level axis. Its explicit form keeps the historic interactive
        // routing; omission of `--level` leaves both axes to the vendor.
        request.configuration.routing = ConfigurationChange::Set(ApprovalRouting::User);
    }
    if let Some(model) = &options.model {
        request.configuration.model = ConfigurationChange::Set(model.clone());
    }
    if let Some(transport) = options.transport {
        request = request.over_transport(transport);
    }
    if let Some(executable) = discovery.and_then(|found| found.executable.as_ref()) {
        request = request.with_executable(ExecutablePath::resolved(executable.clone()));
    }
    request
}

async fn capture(arguments: &[String]) -> Result<(), String> {
    let options = options::CaptureOptions::parse(arguments)?;
    let kind = options.kind()?;
    let out = options
        .out
        .unwrap_or_else(|| options::capture_output(&kind));
    let workspace = workspace::CaptureWorkspace::new(options.workspace)?;
    let result = match &kind {
        HarnessChoice::Codex if options.legacy.is_some() || options.transcripts => {
            capture::codex(
                &out,
                &workspace.path,
                options.scenario.as_deref(),
                options.model.as_deref(),
            )
            .await
        }
        HarnessChoice::Codex => capture::codex_contract(&out, &workspace.path).await,
        HarnessChoice::Claude if options.transcripts => {
            // `CaptureOptions::kind` has already paired the scenario with its schema.
            let schema = options
                .json_schema
                .as_deref()
                .map(|path| options::read_json_schema_file_for("capture", path))
                .transpose()?;
            capture::claude_transcripts(
                &out,
                &workspace.path,
                options.scenario.as_deref().unwrap_or_default(),
                schema.as_deref(),
            )
            .await
        }
        HarnessChoice::Claude => capture::claude(&out, &workspace.path).await,
        HarnessChoice::Acp(profile) => capture::acp(&out, &workspace.path, profile.as_str()).await,
    };
    result.map_err(|error| command_failure(&error))
}

/// Rewrites — or with `--check`, verifies — the file digests of every capture manifest.
///
/// This is how digests reach a capture nobody can re-run: it reads the committed files and writes
/// what they hash to, so no vendor CLI is involved and no captured byte changes. `mea`'s own test
/// suite runs the same verification over the committed tree, which is what `scripts/check.sh`
/// executes; `--check` is that answer by hand, for one fixture root.
fn digests(arguments: &[String]) -> Result<(), String> {
    let options = options::DigestOptions::parse(arguments)?;
    let root = options.root();
    let manifests = capture::manifest::manifests(&root).map_err(|error| error.to_string())?;
    if manifests.is_empty() {
        return Err(format!(
            "expected at least one {} under {}, received none",
            capture::manifest::MANIFEST,
            root.display()
        ));
    }
    if options.check {
        let disagreements: Vec<String> = manifests
            .iter()
            .filter_map(|path| capture::manifest::verify(path).err())
            .map(|error| error.to_string())
            .collect();
        if !disagreements.is_empty() {
            return Err(disagreements.join("\n"));
        }
        println!("{} capture manifests match their files", manifests.len());
        return Ok(());
    }
    for path in manifests {
        let rewritten = capture::manifest::refresh(&path).map_err(|error| error.to_string())?;
        let state = if rewritten { "refreshed" } else { "current" };
        println!("{state} {}", path.display());
    }
    Ok(())
}

/// The carrier requested by the host and the one the open session actually uses.
/// Example: `transport_report(TransportSelection::new(None, TransportKind::Stdio))` names stdio.
fn transport_report(selection: TransportSelection) -> String {
    let requested = selection
        .requested
        .map_or_else(|| String::from("default"), |kind| kind.to_string());
    format!(
        "transport: requested {requested}; effective {}",
        selection.effective
    )
}

/// A session id nobody has to be able to reproduce.
///
/// The words for a refusal whose typed fields carry what `Display` deliberately bounds, or `None`
/// when the library's own sentence is enough.
///
/// The library keeps a vendor-reported version out of its own diagnostics: a build whose banner
/// nothing could parse falls back to the line the CLI printed, so `Display` reports that line's
/// size and leaves the text on the typed field. `mea` is an unpublished smoke tool running on the
/// operator's own machine rather than a diagnostic boundary — `docs/compliance.md` says so — and
/// "which version is installed" is usually the whole reason they ran it.
///
/// `mea turn --kind claude` against an old CLI prints
/// `expected version 2.1.211 or newer, received 2.1.9` instead of `… a vendor-reported version
/// (5 bytes)`.
fn refusal_words(cause: &mango_external_agents::Error) -> Option<String> {
    match cause {
        mango_external_agents::Error::VersionGate { found, minimum } => Some(format!(
            "expected version {minimum} or newer, received {found}"
        )),
        // The login command is typed on the error and left out of its `Display`. The spawn path
        // prints it after `discover`; an attach skips `discover`, so the refusal has to carry it.
        mango_external_agents::Error::AuthRequired { login_hint } => {
            Some(format!("not signed in; run `{login_hint}`"))
        }
        mango_external_agents::Error::Vendor(vendor) => Some(vendor_refusal(vendor)),
        _ => None,
    }
}

/// A failed command as an operator reads it: [`refusal_words`] for the error's cause, then the
/// cleanup and pid notes [`failure::describe`] adds.
///
/// [`failure::describe`] prints a vendor failure as its bare `<code>: vendor failure`, so the
/// prefix it writes for the cause is swapped for the words an operator needs. The cause is found
/// through the dispatch and cleanup annotations: the ACP handshake, for one, marks a vendor error
/// from a closed link `NotSubmitted` before `open_session` returns it, and a cleanup that did not
/// finish wraps the error that ended the session. Every failure printed by `discover`, `doctor`,
/// `turn` and `capture` goes through here.
fn command_failure(error: &mango_external_agents::Error) -> String {
    let described = failure::describe(error);
    let cause = error.cause();
    let Some(words) = refusal_words(cause) else {
        return described;
    };
    match described.strip_prefix(&cause.to_string()) {
        Some(notes) => format!("{words}{notes}"),
        None => described,
    }
}

/// A vendor's refusal with the vendor's own words, which the library's formatter leaves out.
///
/// An operator running a smoke turn needs the reason: a service that is degraded, a model that
/// does not exist and a child that died at launch all read `<code>: vendor failure` otherwise.
/// The message is read for credential names and the well-known token prefixes again here, and
/// bounded, whatever built the error. That read removes the C0 and C1 controls and the
/// bidirectional formatting characters before its rules run, so a credential split by an escape
/// sequence is still hidden. What survives it and a terminal may act on (line breaks, tabs,
/// U+2028 and U+2029) is written as a visible escape, so the summary stays on one line. A message
/// cut at its limit says so.
fn vendor_refusal(vendor: &mango_external_agents::VendorError) -> String {
    use mango_external_agents::normalize::TextLimit;

    let limit = TextLimit::ErrorMessage.max_code_points();
    let bounded = mango_external_agents::redact::bounded(&vendor.message, TextLimit::ErrorMessage);
    let message = turn::escape_terminal_text(bounded.text.trim());
    let mut line = vendor.to_string();
    if !message.is_empty() {
        line.push_str(": ");
        line.push_str(&message);
        // `truncated` is also set when a control character was merely removed, so the kept length
        // has to have reached the limit as well before the text counts as cut.
        if bounded.truncated && bounded.text.chars().count() >= limit {
            line.push_str(&format!(
                " ... [truncated: first {limit} characters of the message shown]"
            ));
        }
    }
    if let Some(code) = &vendor.vendor_code {
        line.push_str(&format!(" (vendor code {code})"));
    }
    if let Some(request) = &vendor.request_id {
        line.push_str(&format!(" (request {request})"));
    }
    line
}

/// `mea` is not a host that retries, so the only requirement is that two runs do not collide.
fn uuid_like() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{
        HarnessChoice, Options, acp_profile_ids, command_failure, describe, discover_with,
        harness_lines, registry, run, session_request, transport_report, turn_with,
    };
    use mango_external_agents::testing::FakeLauncher;
    use mango_external_agents::{
        CapabilityCeiling, ConfigurationVerdict, Discovery, EnvSource, Error, GateVerdict, Harness,
        HarnessDescriptor, HarnessId, HarnessIdentity, HarnessRegistry, HostContext, OpenSession,
        PermissionLevel, PermissionMatrix, ProfileId, Result, Session, TransportKind,
        TransportSelection, VendorInfo,
    };

    #[test]
    fn an_ordinary_command_failure_needs_no_process_cleanup_warning() {
        let error = Error::Closed { subject: "session" };
        assert_eq!(super::command_failure(&error), error.to_string());
    }

    #[test]
    fn the_turn_report_names_requested_and_effective_transport() {
        let selected = TransportSelection::new(Some(TransportKind::Acp), TransportKind::Acp);
        assert_eq!(
            transport_report(selected),
            "transport: requested acp; effective acp"
        );
        let defaulted = TransportSelection::new(None, TransportKind::Stdio);
        assert_eq!(
            transport_report(defaulted),
            "transport: requested default; effective stdio"
        );
    }

    /// The one field `mea` reads off the typed error rather than out of its diagnostic.
    ///
    /// A gate reaches `mea turn` through `open_session`, not through `discovery.gate`, and the
    /// library's own formatter reports the vendor-reported version as a byte count. An operator
    /// running a smoke turn against an old CLI needs the number.
    #[test]
    fn a_version_gate_names_the_version_the_cli_reported() {
        let gate = Error::VersionGate {
            found: String::from("2.1.9"),
            minimum: String::from("2.1.211"),
        };
        assert_eq!(
            command_failure(&gate),
            "expected version 2.1.211 or newer, received 2.1.9"
        );
    }

    /// An attach skips the local probe, so a signed-out server is refused inside `open_session`
    /// and this is the only place the operator is told which login command to run: the library's
    /// own sentence says to inspect the typed hint without printing it.
    #[test]
    fn a_signed_out_server_names_the_login_command() {
        let signed_out = Error::AuthRequired {
            login_hint: String::from("codex login"),
        };
        assert_eq!(
            command_failure(&signed_out),
            "not signed in; run `codex login`"
        );
    }

    /// Everything else keeps the library's own bounded sentence.
    #[test]
    fn any_other_refusal_is_flattened_as_the_library_wrote_it() {
        let protocol = Error::Protocol {
            expected: String::from("a loaded session"),
            received: String::from("payload-secret"),
        };
        assert_eq!(command_failure(&protocol), protocol.to_string());
        assert!(
            !command_failure(&protocol).contains("payload-secret"),
            "expected the vendor payload to stay out, received {}",
            command_failure(&protocol)
        );
    }

    /// A vendor's refusal at open carries the vendor's reason, which the library's own formatter
    /// leaves out. Without it an operator cannot tell a degraded service from a dead child.
    #[test]
    fn a_vendor_refusal_carries_the_vendors_reason_code_and_request() {
        let vendor = Error::Vendor(
            mango_external_agents::VendorError::new(
                mango_external_agents::ErrorCode::from_static("acp-request-failed"),
                "service degraded, try again later",
            )
            .with_vendor_code("-32603", true)
            .with_request_id("req-7"),
        );
        let shown = command_failure(&vendor);
        assert_eq!(
            shown,
            "acp-request-failed: vendor failure: service degraded, try again later (vendor code -32603) (request req-7)",
            "expected the vendor's reason, code and request | received: {shown}"
        );
    }

    /// The reason is read for credentials where it is printed, whatever built the error.
    #[test]
    fn a_vendor_refusal_hides_a_credential_in_the_vendors_reason() {
        let vendor = Error::Vendor(mango_external_agents::VendorError::new(
            mango_external_agents::ErrorCode::from_static("acp-link-closed"),
            "the agent's stderr: password=hunter2 retry with ghp_Canary4Abc9Def2Ghi7Jkl3Mno8Pqr5Stu1V",
        ));
        let shown = command_failure(&vendor);
        assert!(
            !shown.contains("hunter2") && !shown.contains("ghp_Canary"),
            "expected no credential in the printed reason | received: {shown}"
        );
        assert!(
            shown.contains("the agent's stderr"),
            "expected the reason's own words to stay | received: {shown}"
        );
    }

    fn vendor_failure(message: &str) -> Error {
        Error::Vendor(mango_external_agents::VendorError::new(
            mango_external_agents::ErrorCode::from_static("claude-structured-output-missing"),
            message,
        ))
    }

    /// The final summary of a failed command names the code the vendor reported and the reason it
    /// gave: a script that reads only the summary cannot otherwise tell why a turn failed.
    #[test]
    fn a_command_failure_summary_names_the_vendor_code_and_message() {
        let shown = super::command_failure(&vendor_failure("the schema was not satisfied"));
        assert_eq!(
            shown, "claude-structured-output-missing: vendor failure: the schema was not satisfied",
            "expected the vendor code and message | received: {shown}"
        );
    }

    /// A vendor error that an operation annotated (here with its dispatch) keeps its message.
    #[test]
    fn a_command_failure_summary_keeps_the_message_of_an_annotated_vendor_error() {
        let annotated = vendor_failure("the vendor took it and failed it")
            .with_dispatch(mango_external_agents::Dispatch::Accepted);
        let shown = super::command_failure(&annotated);
        assert_eq!(
            shown,
            "claude-structured-output-missing: vendor failure: the vendor took it and failed it",
            "expected the message under the dispatch annotation | received: {shown}"
        );
    }

    /// The message is vendor text printed at a terminal: nothing in it may act on the terminal,
    /// and a line break may not read as a second line of the tool's own output.
    #[test]
    fn a_command_failure_summary_escapes_control_characters_in_the_vendor_message() {
        let shown = super::command_failure(&vendor_failure(
            "line one\nline two\ttabbed \u{1b}[31mred\u{1b}[0m bell\u{7} bidi\u{202e}gnp.exe \u{2028}end",
        ));
        assert!(
            !shown.chars().any(super::turn::is_terminal_active),
            "expected no terminal-active character in the summary | received: {shown:?}"
        );
        // A line break, a tab and U+2028 survive the redaction read and become visible escapes.
        // The escape sequences' own bytes are removed by that read, and the words stay.
        for expected in [
            "line one\\nline two\\ttabbed",
            "red",
            "bell",
            "bidi",
            "gnp.exe",
            "\\u2028end",
        ] {
            assert!(
                shown.contains(expected),
                "expected the summary to hold {expected:?} | received: {shown:?}"
            );
        }
    }

    /// Longer than `TextLimit::ErrorMessage` allows, so the cut says so and says how much stayed.
    #[test]
    fn a_command_failure_summary_cuts_an_over_long_vendor_message_with_a_marker() {
        let limit = mango_external_agents::normalize::TextLimit::ErrorMessage.max_code_points();
        let shown = super::command_failure(&vendor_failure(&"x".repeat(limit + 500)));
        let expected = format!(
            "claude-structured-output-missing: vendor failure: {} ... [truncated: first {limit} characters of the message shown]",
            "x".repeat(limit)
        );
        assert_eq!(
            shown, expected,
            "expected the message cut at {limit} characters with a marker"
        );
    }

    /// A message that fits, even one that held a control character the redaction read removed, is
    /// not reported as cut.
    #[test]
    fn a_command_failure_summary_does_not_claim_a_cut_for_a_short_message() {
        let shown = super::command_failure(&vendor_failure("short\u{1b}[0m"));
        assert!(
            !shown.contains("truncated"),
            "expected no truncation marker on a short message | received: {shown}"
        );
    }

    /// An empty message leaves the code alone, with no dangling separator.
    #[test]
    fn a_command_failure_summary_for_an_empty_vendor_message_is_the_bare_code() {
        for message in ["", "  \n\t "] {
            let shown = super::command_failure(&vendor_failure(message));
            assert_eq!(
                shown, "claude-structured-output-missing: vendor failure",
                "expected the bare code for message {message:?} | received: {shown}"
            );
        }
    }

    /// A probe-only harness that records dispatch without running a vendor CLI.
    struct CountingDiscoveryHarness {
        descriptor: HarnessDescriptor,
        probes: Arc<AtomicUsize>,
        /// The vendor message a probe fails with; `None` probes as not installed.
        probe_failure: Option<&'static str>,
        /// The vendor message `open_session` fails with; `None` fails as a closed session.
        open_failure: Option<&'static str>,
        /// Whether both failures carry the dispatch annotation the real harnesses add.
        annotated: bool,
    }

    impl CountingDiscoveryHarness {
        fn failing_probe(mut self, message: &'static str) -> Self {
            self.probe_failure = Some(message);
            self
        }

        fn failing_open(mut self, message: &'static str) -> Self {
            self.open_failure = Some(message);
            self
        }

        /// Marks every failure `NotSubmitted`, as the ACP handshake does when its link closes.
        fn annotating_failures(mut self) -> Self {
            self.annotated = true;
            self
        }

        fn vendor_failure(&self, code: &'static str, message: &str) -> Error {
            let error = Error::Vendor(mango_external_agents::VendorError::new(
                mango_external_agents::ErrorCode::from_static(code),
                message,
            ));
            if self.annotated {
                error.with_dispatch(mango_external_agents::Dispatch::NotSubmitted)
            } else {
                error
            }
        }

        fn new(identity: HarnessIdentity, probes: Arc<AtomicUsize>) -> Self {
            Self {
                descriptor: HarnessDescriptor {
                    identity,
                    vendor: VendorInfo {
                        company: "Example",
                        terms_url: "https://example.com/terms",
                        privacy_url: "https://example.com/privacy",
                        skills_are_slash_commands: false,
                    },
                    capabilities: CapabilityCeiling::none(),
                    transports: &[TransportKind::Stdio],
                    vendor_environment_keys: &[],
                },
                probes,
                probe_failure: None,
                open_failure: None,
                annotated: false,
            }
        }
    }

    #[async_trait::async_trait]
    impl Harness for CountingDiscoveryHarness {
        fn descriptor(&self) -> &HarnessDescriptor {
            &self.descriptor
        }

        fn permission_matrix(&self) -> PermissionMatrix {
            PermissionMatrix::build(|_, _| ConfigurationVerdict::supported())
        }

        async fn probe(&self, _host: &HostContext) -> Result<Discovery> {
            self.probes.fetch_add(1, Ordering::Relaxed);
            if let Some(message) = self.probe_failure {
                return Err(self.vendor_failure("probe-failed", message));
            }
            Ok(Discovery::not_installed())
        }

        async fn open_session(
            &self,
            _host: &HostContext,
            _request: OpenSession,
        ) -> Result<Box<dyn Session>> {
            if let Some(message) = self.open_failure {
                return Err(self.vendor_failure("open-failed", message));
            }
            Err(Error::Closed {
                subject: "test session",
            })
        }
    }

    fn fake_host() -> HostContext {
        HostContext::builder()
            .launcher(Arc::new(FakeLauncher::new()))
            .cwd(std::env::temp_dir())
            .environment(EnvSource::from_pairs([("PATH", "/fake/bin")]))
            .client_info("mea-test", "0.0.0")
            .build()
            .expect("expected a fake host context")
    }

    /// The banner names each linked harness once, and the two that publish an id publish one the
    /// registry actually answers to — which a bare string constant could not promise.
    #[test]
    fn the_banner_names_each_linked_harness_once() {
        let lines = harness_lines();
        let mut sorted = lines.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            lines.len(),
            "expected 3 distinct lines, received {lines:?}"
        );

        let registry = registry();
        for id in [
            mango_agent_claude::harness_id(),
            mango_agent_codex::harness_id(),
        ] {
            assert!(
                registry.get(&id).is_some(),
                "expected the published id {id} to be one the registry answers to"
            );
        }
    }

    #[test]
    fn the_registry_dispatches_to_each_implemented_harness() {
        assert!(registry().get(&HarnessId::claude()).is_some());
        assert!(registry().get(&HarnessId::codex()).is_some());
        for profile in acp_profile_ids() {
            let profile_id = ProfileId::new(profile.clone()).expect("expected a valid profile");
            assert!(
                registry().get(&HarnessId::acp(&profile_id)).is_some(),
                "expected the {profile:?} ACP profile to be registered"
            );
        }
    }

    #[tokio::test]
    async fn bare_discovery_dispatches_every_registered_harness() {
        let probes = Arc::new(AtomicUsize::new(0));
        let registry = HarnessRegistry::new(vec![
            Arc::new(CountingDiscoveryHarness::new(
                HarnessIdentity::claude(),
                probes.clone(),
            )),
            Arc::new(CountingDiscoveryHarness::new(
                HarnessIdentity::codex(),
                probes.clone(),
            )),
            Arc::new(CountingDiscoveryHarness::new(
                HarnessIdentity::acp(
                    ProfileId::new("test-agent").expect("expected a valid profile"),
                ),
                probes.clone(),
            )),
        ])
        .expect("expected a fake registry");

        let reports = discover_with(&registry, &fake_host(), None)
            .await
            .expect("expected discovery to finish");

        assert_eq!(reports.len(), registry.len());
        assert_eq!(probes.load(Ordering::Relaxed), registry.len());

        let selected = discover_with(&registry, &fake_host(), Some(HarnessId::codex()))
            .await
            .expect("expected selected discovery to finish");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].kind, HarnessId::codex());
        assert_eq!(probes.load(Ordering::Relaxed), registry.len() + 1);
    }

    /// A sweep that cannot probe one harness reports why, not only the code.
    #[tokio::test]
    async fn a_failed_discovery_report_carries_the_vendors_message() {
        let registry = HarnessRegistry::new(vec![Arc::new(
            CountingDiscoveryHarness::new(HarnessIdentity::codex(), Arc::default())
                .failing_probe("the version probe timed out"),
        ) as Arc<dyn Harness>])
        .expect("expected a fake registry");

        let reports = discover_with(&registry, &fake_host(), None)
            .await
            .expect("expected discovery to finish");

        assert_eq!(
            reports[0].result.as_ref().err().map(String::as_str),
            Some("probe-failed: vendor failure: the version probe timed out"),
            "expected the probe's code and message in the report"
        );
    }

    /// A probe error the harness annotated (as the ACP handshake does) keeps its message in the
    /// sweep's report.
    #[tokio::test]
    async fn an_annotated_discovery_failure_carries_the_vendors_message() {
        let registry = HarnessRegistry::new(vec![Arc::new(
            CountingDiscoveryHarness::new(HarnessIdentity::codex(), Arc::default())
                .failing_probe("the version probe timed out")
                .annotating_failures(),
        ) as Arc<dyn Harness>])
        .expect("expected a fake registry");

        let reports = discover_with(&registry, &fake_host(), None)
            .await
            .expect("expected discovery to finish");

        assert_eq!(
            reports[0].result.as_ref().err().map(String::as_str),
            Some("probe-failed: vendor failure: the version probe timed out"),
            "expected the code and the message under the dispatch annotation"
        );
    }

    /// `mea turn` that is refused inside `open_session` with an annotated vendor error names the
    /// vendor's reason, not the code alone.
    #[tokio::test]
    async fn an_annotated_open_session_failure_carries_the_vendors_message() {
        let registry = HarnessRegistry::new(vec![Arc::new(
            CountingDiscoveryHarness::new(HarnessIdentity::claude(), Arc::default())
                .failing_open("a transport that closed under session/new")
                .annotating_failures(),
        ) as Arc<dyn Harness>])
        .expect("expected a fake registry");
        let options =
            Options::parse_for("turn", &["--harness", "claude", "hello"].map(String::from))
                .expect("expected valid turn arguments");

        let failure = turn_with(&registry, &fake_host(), &options)
            .await
            .expect_err("expected the scripted open failure");

        assert_eq!(
            failure, "open-failed: vendor failure: a transport that closed under session/new",
            "expected the code and the message under the dispatch annotation | received: {failure}"
        );
    }

    /// A vendor error whose process cleanup is still owed keeps its message in a refusal, and the
    /// operator is told the child may still be running.
    #[test]
    fn a_refusal_under_a_cleanup_control_keeps_the_message_and_the_pid_note() {
        let failure = Error::CleanupRequired {
            control: Arc::new(UnconfirmedChild),
            source: Box::new(vendor_failure("the agent closed the link")),
        };
        let shown = command_failure(&failure);
        assert_eq!(
            shown,
            "claude-structured-output-missing: vendor failure: the agent closed the link; process-tree cleanup is unconfirmed for launched pid 4242; inspect the process before retrying",
            "expected the message ahead of the cleanup note | received: {shown}"
        );
    }

    /// A named recovery identity without a native process.
    struct UnconfirmedChild;

    #[async_trait::async_trait]
    impl mango_external_agents::ProcessControl for UnconfirmedChild {
        fn pid(&self) -> Option<u32> {
            Some(4242)
        }
        fn stderr_tail(&self) -> String {
            String::new()
        }
        async fn wait(&self) -> Result<mango_external_agents::ExitStatus> {
            Ok(Default::default())
        }
        async fn kill(&self, _reason: mango_external_agents::CancelReason) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn help_aliases_dispatch_through_run() {
        for alias in ["help", "--help", "-h"] {
            assert!(
                run(&[String::from(alias)]).await.is_ok(),
                "expected {alias:?} to print help"
            );
        }
    }

    #[tokio::test]
    async fn an_unsupported_transport_is_rejected_before_discovery() {
        let args = [
            "turn",
            "--harness",
            "codex",
            "--transport",
            "websocket",
            "hello",
        ]
        .map(String::from);
        let error = run(&args)
            .await
            .expect_err("Codex does not support WebSocket");
        assert_eq!(
            error,
            "expected a transport codex supports, received websocket"
        );
    }

    #[tokio::test]
    async fn doctor_rejects_unknown_harnesses_before_discovery() {
        let args = ["doctor", "--harness", "no-such-harness"].map(String::from);
        let error = run(&args)
            .await
            .expect_err("unknown harness must be rejected");
        assert!(
            error.contains("received harness Some(\"no-such-harness\")"),
            "{error}"
        );
    }

    #[test]
    fn leaves_the_harness_unspecified_for_discover_all() {
        let options = Options::parse(&[String::from("say"), String::from("hello")])
            .expect("expected the arguments to parse");
        assert_eq!(options.kind, None);
        assert_eq!(options.level, None);
        assert_eq!(options.prompt, "say hello");
    }

    #[test]
    fn reads_the_level_a_caller_asked_for() {
        let options = Options::parse(&[
            String::from("--level"),
            String::from("full-access"),
            String::from("ship"),
        ])
        .expect("expected the arguments to parse");
        assert_eq!(options.level, Some(PermissionLevel::FullAccess));
        assert_eq!(options.prompt, "ship");
    }

    #[test]
    fn reads_the_codex_harness_a_caller_asked_for() {
        let options = Options::parse(&[
            String::from("--harness"),
            String::from("codex"),
            String::from("ship"),
        ])
        .expect("expected the arguments to parse");
        assert_eq!(options.kind, Some(HarnessChoice::Codex));
        assert_eq!(options.prompt, "ship");
    }

    #[test]
    fn reads_a_built_in_acp_harness() {
        let options = Options::parse(&[
            String::from("--harness"),
            String::from("acp:cursor"),
            String::from("ship"),
        ])
        .expect("expected the arguments to parse");
        assert_eq!(
            options.kind,
            Some(HarnessChoice::Acp(
                ProfileId::new("cursor").expect("expected a valid profile")
            ))
        );
        assert_eq!(options.prompt, "ship");
    }

    #[test]
    fn refuses_a_value_it_does_not_recognise_rather_than_guessing() {
        for arguments in [
            vec![String::from("--level"), String::from("yolo")],
            vec![String::from("--harness"), String::from("acp")],
            vec![String::from("--harness"), String::from("acp:unknown")],
            vec![String::from("--level")],
        ] {
            assert!(
                Options::parse(&arguments).is_err(),
                "expected {arguments:?} to be refused"
            );
        }
    }

    /// `--model` is the opened session's model, which the Codex harness sends as `thread/start`'s
    /// `model` member (`opening_with_an_explicit_model_sends_it_as_the_thread_start_model_member`
    /// in the Codex replay tests); without the flag the axis is left alone.
    #[test]
    fn a_turn_opens_its_session_on_the_model_the_flag_named() {
        let named = Options::parse_for(
            "turn",
            &["--harness", "codex", "--model", "gpt-5.2-codex", "hi"].map(String::from),
        )
        .expect("expected valid codex turn arguments");
        let request = session_request(&named, None);
        assert_eq!(
            request.configuration.model.set_value().map(String::as_str),
            Some("gpt-5.2-codex"),
            "expected the session to open on the named model"
        );

        let unnamed = Options::parse_for("turn", &["--harness", "codex", "hi"].map(String::from))
            .expect("expected valid codex turn arguments");
        let request = session_request(&unnamed, None);
        assert_eq!(
            request.configuration.model,
            mango_external_agents::ConfigurationChange::Keep,
            "expected the user's configured model to stay in force without the flag"
        );
    }

    #[tokio::test]
    async fn a_turn_refuses_a_model_for_another_harness_before_any_discovery() {
        let args = [
            "turn",
            "--harness",
            "claude",
            "--model",
            "gpt-5.2-codex",
            "hi",
        ]
        .map(String::from);
        let error = run(&args).await.expect_err("expected the model refused");
        assert_eq!(
            error,
            "(mea turn): expected --model with --harness codex | received --model \"gpt-5.2-codex\" with --harness claude"
        );
    }

    /// The three words the smoke test reads, plus the honest fourth: an agent that started and
    /// printed something unrecognisable is neither installed-and-fine nor gated.
    #[test]
    fn every_gate_verdict_reads_as_one_word_a_person_can_scan() {
        assert_eq!(describe(&Discovery::not_installed()), "missing");

        let usable = Discovery {
            gate: GateVerdict::Usable,
            version: Some(String::from("1.2.3")),
            ..Discovery::not_installed()
        };
        assert_eq!(describe(&usable), "installed  1.2.3");

        let gated = Discovery {
            gate: GateVerdict::VersionTooOld {
                found: String::from("0.9.0"),
                minimum: String::from("1.0.0"),
            },
            ..Discovery::not_installed()
        };
        assert!(
            describe(&gated).starts_with("gated"),
            "{}",
            describe(&gated)
        );

        let unknown = Discovery {
            gate: GateVerdict::Unknown,
            ..Discovery::not_installed()
        };
        assert!(describe(&unknown).starts_with("unknown"));
    }

    #[test]
    fn a_missing_required_surface_reports_a_known_gate() {
        let discovery = Discovery {
            gate: GateVerdict::MissingRequiredSurface {
                expected: "a required launch flag",
                received: "the flag was absent from help",
            },
            ..Discovery::not_installed()
        };
        assert_eq!(
            describe(&discovery),
            "gated      expected a required launch flag; received the flag was absent from help"
        );
    }
}

/// `mea turn` over a Unix socket, against a named stand-in for the server.
///
/// The stand-in refuses the WebSocket upgrade, so a run crosses a real socket and the harness's
/// real connect and upgrade, and ends in a refusal whose words the test can name. A full replayed
/// turn lives in `crates/mango-agent-codex/tests/unix_socket.rs`, which owns the recorded
/// conversation and the WebSocket server that speaks it.
#[cfg(all(test, unix))]
mod unix_socket_tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use mango_external_agents::testing::FakeLauncher;
    use mango_external_agents::{EnvSource, HostContext};

    use super::{Options, registry_attaching_to, run, turn_with};

    /// An app-server stand-in on a Unix socket that answers every upgrade with `403 Forbidden`.
    struct UpgradeRefusingSocketServer {
        /// Held so the socket's directory outlives the test.
        _directory: tempfile::TempDir,
        path: PathBuf,
        connections: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        accepting: Option<JoinHandle<()>>,
    }

    impl UpgradeRefusingSocketServer {
        fn start() -> Self {
            let directory = tempfile::tempdir().expect("expected a socket directory");
            let path = directory.path().join("app-server.sock");
            let listener = UnixListener::bind(&path).expect("expected the socket to bind");
            listener
                .set_nonblocking(true)
                .expect("expected a non-blocking listener");
            let connections = Arc::new(AtomicUsize::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let accepting = {
                let (connections, stop) = (Arc::clone(&connections), Arc::clone(&stop));
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let Ok((mut stream, _)) = listener.accept() else {
                            std::thread::sleep(Duration::from_millis(5));
                            continue;
                        };
                        connections.fetch_add(1, Ordering::AcqRel);
                        stream
                            .set_nonblocking(false)
                            .expect("expected a blocking connection");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .expect("expected a read timeout");
                        // The upgrade request ends at the blank line.
                        let mut request = Vec::new();
                        let mut byte = [0_u8; 1];
                        while !request.ends_with(b"\r\n\r\n")
                            && stream.read(&mut byte).is_ok_and(|read| read == 1)
                        {
                            request.push(byte[0]);
                        }
                        let _ = stream.write_all(
                            b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
                        );
                    }
                })
            };
            Self {
                _directory: directory,
                path,
                connections,
                stop,
                accepting: Some(accepting),
            }
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::Acquire)
        }
    }

    impl Drop for UpgradeRefusingSocketServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(accepting) = self.accepting.take() {
                let _ = accepting.join();
            }
        }
    }

    /// A host whose launcher holds no process, so a spawn of the local `codex` is visible.
    fn host_without_processes() -> (HostContext, Arc<FakeLauncher>) {
        let launcher = Arc::new(FakeLauncher::new());
        let host = HostContext::builder()
            .launcher(launcher.clone())
            .cwd(std::env::temp_dir())
            .environment(EnvSource::from_pairs([("PATH", "/fake/bin")]))
            .client_info("mea-test", "0.0.0")
            .build()
            .expect("expected a fake host context");
        (host, launcher)
    }

    fn options_for(command: &str, socket: &Path, rest: &[&str]) -> Options {
        let mut arguments: Vec<String> = [
            "--harness",
            "codex",
            "--transport",
            "unix-socket",
            "--socket",
        ]
        .map(String::from)
        .to_vec();
        arguments.push(socket.display().to_string());
        arguments.extend(rest.iter().map(|word| (*word).to_owned()));
        Options::parse_for(command, &arguments)
            .unwrap_or_else(|error| panic!("expected valid {command} arguments, received {error}"))
    }

    #[tokio::test]
    async fn a_turn_dials_the_named_socket_and_prints_the_harness_refusal_as_it_is() {
        let server = UpgradeRefusingSocketServer::start();
        let options = options_for("turn", &server.path, &["hello"]);
        let (host, launcher) = host_without_processes();

        let error = turn_with(
            &registry_attaching_to(options.socket.as_deref()),
            &host,
            &options,
        )
        .await
        .expect_err("expected the server's 403 to refuse the turn");

        assert!(
            error.contains("HTTP 403 Forbidden from the upgrade"),
            "expected the harness's own refusal (HTTP 403 Forbidden from the upgrade) | received: {error}"
        );
        assert_eq!(
            server.connections(),
            1,
            "expected one connection to the named socket"
        );
        assert_eq!(
            launcher.launches().len(),
            0,
            "expected an attach to spawn no local codex | received launches {:?}",
            launcher.launches()
        );
    }

    /// The path goes to the harness as typed: its refusal of a relative path is what comes out.
    #[tokio::test]
    async fn a_path_the_parser_never_saw_reaches_the_harness_unchanged() {
        let mut options = options_for("turn", Path::new("/run/codex.sock"), &["hello"]);
        options.socket = Some(PathBuf::from("relative/app-server.sock"));
        let (host, _launcher) = host_without_processes();

        let error = turn_with(
            &registry_attaching_to(options.socket.as_deref()),
            &host,
            &options,
        )
        .await
        .expect_err("expected the harness to refuse a relative path");

        assert!(
            error.contains("expected an absolute Unix socket path"),
            "expected the harness's own words about the relative path | received: {error}"
        );
    }

    #[tokio::test]
    async fn run_refuses_a_socket_without_its_transport_before_any_discovery() {
        let args = [
            "turn",
            "--harness",
            "codex",
            "--socket",
            "/run/codex.sock",
            "hi",
        ]
        .map(String::from);
        let error = run(&args).await.expect_err("expected the pairing refused");
        assert_eq!(
            error,
            "(mea turn): expected --socket with --transport unix-socket | received --socket without it"
        );
    }

    /// `doctor` stays read-only: it never reaches a socket, so it refuses both flags before any probe.
    #[tokio::test]
    async fn doctor_refuses_the_socket_before_any_probe() {
        let args = [
            "doctor",
            "--harness",
            "codex",
            "--transport",
            "unix-socket",
            "--socket",
            "/run/codex.sock",
        ]
        .map(String::from);
        let error = run(&args).await.expect_err("expected doctor to refuse");
        assert_eq!(
            error,
            "(mea doctor): expected no --socket and no unix-socket transport; doctor probes the local CLI and never opens a session | received --transport unix-socket --socket \"/run/codex.sock\""
        );
    }
}
