//! `mea`: the unpublished smoke and capture CLI for mango-external-agents.
//!
//! `discover` and `doctor` probe installed CLIs without handling login. `turn` drives a
//! conversation with terminal approvals and optional NDJSON. `capture` records public contracts
//! and explicit archival transcripts. The binary is built from source and is not published.

mod ask;
mod capture;
mod doctor;
mod options;
mod redact;
mod terminal;
mod turn;
mod workspace;

use std::process::ExitCode;
use std::sync::Arc;

use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::{
    ApprovalRouting, AuthState, ConfigurationChange, EnvSource, ExecutablePath, HarnessId,
    HarnessRegistry, HostContext, OpenSession, TransportSelection, TurnRequest,
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
    let mut harnesses = vec![
        mango_agent_claude::harness(),
        Arc::new(mango_agent_codex::CodexHarness::new()),
    ];
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
        "discover" => discover(&Options::parse(rest)?).await,
        "doctor" => doctor(&Options::parse(rest)?).await,
        "turn" => turn(&Options::parse(rest)?).await,
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
        "       mea turn [--harness claude|codex|acp:<profile>] [--profile ID] [--transport stdio|websocket|acp] [--cwd DIR] [--json] [--level read-only|default|full-access] <prompt>"
    );
    println!(
        "       mea capture --harness claude|codex|acp[:profile] [--profile ID] [--out DIR] [--workspace DIR] [--transcripts]"
    );
    println!("       mea capture codex [--out DIR] [--workspace DIR] (archival transcripts)");
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
                tokio::spawn(
                    async move { harness.discover(&host).await.map_err(|e| e.to_string()) },
                ),
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
    let host = host(options.cwd.as_deref())?;
    let registry = registry();
    let kind = options.kind.clone().unwrap_or(HarnessChoice::Claude);
    let harness = registry.require(&kind.id()).map_err(|e| e.to_string())?;
    if let Some(transport) = options.transport {
        harness
            .descriptor()
            .require_transport(&transport)
            .map_err(|error| error.to_string())?;
    }

    let discovery = harness.discover(&host).await.map_err(|e| refusal(&e))?;
    if let AuthState::LoggedOut { login_hint } = &discovery.auth {
        return Err(format!("not signed in; run `{login_hint}`"));
    }

    let mut request = OpenSession::new(format!("mea-{}", uuid_like()));
    if let Some(level) = options.level {
        request.configuration.level = ConfigurationChange::Set(level);
        // The CLI only exposes the level axis. Its explicit form keeps the historic interactive
        // routing; omission of `--level` leaves both axes to the vendor.
        request.configuration.routing = ConfigurationChange::Set(ApprovalRouting::User);
    }
    if let Some(transport) = options.transport {
        request = request.over_transport(transport);
    }
    if let Some(executable) = &discovery.executable {
        request = request.with_executable(ExecutablePath::resolved(executable.clone()));
    }

    // A gate refusal reaches an operator here rather than through `discovery.gate`: Codex names
    // its build in the app-server handshake, which happens inside `open_session`.
    let session = harness
        .open_session(&host, request)
        .await
        .map_err(|e| refusal(&e))?;
    eprintln!("session: {}", session.ids().native_session_id);
    eprintln!("{}", transport_report(session.snapshot().transport));

    turn::run_with_format(
        session.as_ref(),
        TurnRequest::new("mea-turn-1", options.prompt.clone()),
        options.json,
    )
    .await
    .map_err(|e| e.to_string())
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
            capture::codex(&out, &workspace.path).await
        }
        HarnessChoice::Codex => capture::codex_contract(&out, &workspace.path).await,
        HarnessChoice::Claude => capture::claude(&out, &workspace.path).await,
        HarnessChoice::Acp(profile) => capture::acp(&out, &workspace.path, profile.as_str()).await,
    };
    result.map_err(|error| error.to_string())
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
/// Flattens a library refusal for an operator, naming what `Display` deliberately bounds.
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
fn refusal(error: &mango_external_agents::Error) -> String {
    match error {
        mango_external_agents::Error::VersionGate { found, minimum } => {
            format!("expected version {minimum} or newer, received {found}")
        }
        other => other.to_string(),
    }
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
        HarnessChoice, Options, acp_profile_ids, describe, discover_with, harness_lines, refusal,
        registry, run, transport_report,
    };
    use mango_external_agents::testing::FakeLauncher;
    use mango_external_agents::{
        CapabilityCeiling, ConfigurationVerdict, Discovery, EnvSource, Error, GateVerdict, Harness,
        HarnessDescriptor, HarnessId, HarnessIdentity, HarnessRegistry, HostContext, OpenSession,
        PermissionLevel, PermissionMatrix, ProfileId, Result, Session, TransportKind,
        TransportSelection, VendorInfo,
    };

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
            refusal(&gate),
            "expected version 2.1.211 or newer, received 2.1.9"
        );
    }

    /// Everything else keeps the library's own bounded sentence.
    #[test]
    fn any_other_refusal_is_flattened_as_the_library_wrote_it() {
        let protocol = Error::Protocol {
            expected: String::from("a loaded session"),
            received: String::from("payload-secret"),
        };
        assert_eq!(refusal(&protocol), protocol.to_string());
        assert!(
            !refusal(&protocol).contains("payload-secret"),
            "expected the vendor payload to stay out, received {}",
            refusal(&protocol)
        );
    }

    /// A probe-only harness that records dispatch without running a vendor CLI.
    struct CountingDiscoveryHarness {
        descriptor: HarnessDescriptor,
        probes: Arc<AtomicUsize>,
    }

    impl CountingDiscoveryHarness {
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
            Ok(Discovery::not_installed())
        }

        async fn open_session(
            &self,
            _host: &HostContext,
            _request: OpenSession,
        ) -> Result<Box<dyn Session>> {
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
