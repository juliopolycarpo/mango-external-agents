//! `mea`: the unpublished smoke and capture CLI for mango-external-agents.
//!
//! `discover` and `doctor` probe installed CLIs without handling login. `turn` drives a
//! conversation with terminal approvals and optional NDJSON. `capture` records public contracts
//! and explicit archival transcripts. The binary is built from source and is not published.

mod capture;
mod doctor;
mod options;
mod redact;
mod terminal;
mod turn;

use std::process::ExitCode;
use std::sync::Arc;

use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::{
    ApprovalRouting, AuthState, EnvSource, ExecutablePath, HarnessKind, HarnessRegistry,
    HostContext, OpenSession, TurnRequest,
};

/// Every harness kind the binary links, in registry order.
fn harness_kinds() -> [&'static str; 3] {
    [
        mango_agent_claude::HARNESS_KIND,
        mango_agent_codex::HARNESS_KIND,
        mango_agent_acp::HARNESS_KIND,
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
        .broker(Arc::new(terminal::TerminalBroker))
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
        "help" | "--help" | "-h" => {
            print_banner();
            Ok(())
        }
        other => Err(format!(
            "expected `discover`, `doctor`, `turn` or `capture`, received {other:?}"
        )),
    }
}

fn print_banner() {
    println!(
        "mea {} (mango-external-agents {})",
        env!("CARGO_PKG_VERSION"),
        mango_external_agents::VERSION
    );
    for kind in harness_kinds() {
        println!("harness: {kind}");
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
    kind: HarnessKind,
    result: Result<mango_external_agents::Discovery, String>,
}

async fn discover_with(
    registry: &HarnessRegistry,
    host: &HostContext,
    selected: Option<&HarnessKind>,
) -> Result<Vec<DiscoveryReport>, String> {
    let harnesses = match selected {
        Some(kind) => vec![(
            kind.clone(),
            registry
                .require(kind)
                .map_err(|error| error.to_string())?
                .clone(),
        )],
        None => registry
            .kinds()
            .into_iter()
            .map(|kind| {
                Ok((
                    kind.clone(),
                    registry
                        .require(kind)
                        .map_err(|error| error.to_string())?
                        .clone(),
                ))
            })
            .collect::<Result<Vec<_>, String>>()?,
    };
    // Probed concurrently, in registry order. Each probe spawns a child and waits on it for up to
    // `Limits::request_timeout`, and the probes share nothing — run one after another, a sweep over
    // ten harnesses costs ten timeouts instead of one.
    let probes: Vec<(HarnessKind, tokio::task::JoinHandle<_>)> = harnesses
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
    let mut reports = discover_with(&registry, &host, options.kind.as_ref()).await?;
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
        options.kind.as_ref(),
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
    let kind = options.kind.clone().unwrap_or(HarnessKind::Claude);
    let harness = registry.require(&kind).map_err(|e| e.to_string())?;
    if let Some(transport) = options.transport {
        harness
            .descriptor()
            .require_transport(&transport)
            .map_err(|error| error.to_string())?;
    }

    let discovery = harness.discover(&host).await.map_err(|e| e.to_string())?;
    if let AuthState::LoggedOut { login_hint } = &discovery.auth {
        return Err(format!("not signed in; run `{login_hint}`"));
    }

    let mut request = OpenSession::new(format!("mea-{}", uuid_like()));
    if let Some(level) = options.level {
        request.configuration.level = Some(level);
        // The CLI only exposes the level axis. Its explicit form keeps the historic interactive
        // routing; omission of `--level` leaves both axes to the vendor.
        request.configuration.routing = Some(ApprovalRouting::User);
    }
    if let Some(executable) = &discovery.executable {
        request = request.with_executable(ExecutablePath::resolved(executable.clone()));
    }

    let session = harness
        .open_session(&host, request)
        .await
        .map_err(|e| e.to_string())?;
    eprintln!("session: {}", session.ids().native_session_id);

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
    let vendor = match kind {
        HarnessKind::Claude => "claude",
        HarnessKind::Codex => "codex",
        _ => "acp",
    };
    let out = options
        .out
        .unwrap_or_else(|| std::path::PathBuf::from(format!("fixtures/{vendor}")));
    let workspace = options
        .workspace
        .unwrap_or_else(|| std::env::temp_dir().join(format!("mea-capture-{}", uuid_like())));
    std::fs::create_dir_all(&workspace).map_err(|error| {
        format!("expected a writable capture workspace {workspace:?}, received {error}")
    })?;
    let result = match kind {
        HarnessKind::Codex if options.legacy.is_some() || options.transcripts => {
            capture::codex(&out, &workspace).await
        }
        HarnessKind::Codex => capture::codex_contract(&out, &workspace).await,
        HarnessKind::Claude => capture::claude(&out, &workspace).await,
        HarnessKind::Acp(profile) => capture::acp(&out, &workspace, &profile.to_string()).await,
        _ => {
            return Err(format!(
                "expected claude, codex or an ACP profile, received {kind}"
            ));
        }
    };
    result.map_err(|error| error.to_string())
}

/// A session id nobody has to be able to reproduce.
///
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

    use super::{Options, acp_profile_ids, describe, discover_with, harness_kinds, registry, run};
    use mango_external_agents::testing::FakeLauncher;
    use mango_external_agents::{
        AcpProfileId, Capabilities, ConfigurationVerdict, Discovery, EnvSource, Error, GateVerdict,
        Harness, HarnessDescriptor, HarnessKind, HarnessRegistry, HostContext, OpenSession,
        PermissionLevel, PermissionMatrix, Result, Session, TransportKind, VendorInfo,
    };

    /// A probe-only harness that records dispatch without running a vendor CLI.
    struct CountingDiscoveryHarness {
        descriptor: HarnessDescriptor,
        probes: Arc<AtomicUsize>,
    }

    impl CountingDiscoveryHarness {
        fn new(kind: HarnessKind, probes: Arc<AtomicUsize>) -> Self {
            Self {
                descriptor: HarnessDescriptor {
                    kind,
                    vendor: VendorInfo {
                        company: "Example",
                        terms_url: "https://example.com/terms",
                        privacy_url: "https://example.com/privacy",
                        skills_are_slash_commands: false,
                    },
                    capabilities: Capabilities::none(),
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

    #[test]
    fn harness_kinds_are_distinct() {
        let kinds = harness_kinds();
        let mut sorted = kinds.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            kinds.len(),
            "expected 3 distinct kinds, received {kinds:?}"
        );
    }

    #[test]
    fn the_registry_dispatches_to_each_implemented_harness() {
        assert!(registry().get(&HarnessKind::Claude).is_some());
        assert!(registry().get(&HarnessKind::Codex).is_some());
        for profile in acp_profile_ids() {
            assert!(
                registry()
                    .get(&HarnessKind::Acp(AcpProfileId::new(profile.clone())))
                    .is_some(),
                "expected the {profile:?} ACP profile to be registered"
            );
        }
    }

    #[tokio::test]
    async fn bare_discovery_dispatches_every_registered_harness() {
        let probes = Arc::new(AtomicUsize::new(0));
        let registry = HarnessRegistry::new(vec![
            Arc::new(CountingDiscoveryHarness::new(
                HarnessKind::Claude,
                probes.clone(),
            )),
            Arc::new(CountingDiscoveryHarness::new(
                HarnessKind::Codex,
                probes.clone(),
            )),
            Arc::new(CountingDiscoveryHarness::new(
                HarnessKind::Acp(AcpProfileId::new("test-agent")),
                probes.clone(),
            )),
        ])
        .expect("expected a fake registry");

        let reports = discover_with(&registry, &fake_host(), None)
            .await
            .expect("expected discovery to finish");

        assert_eq!(reports.len(), registry.len());
        assert_eq!(probes.load(Ordering::Relaxed), registry.len());

        let selected = discover_with(&registry, &fake_host(), Some(&HarnessKind::Codex))
            .await
            .expect("expected selected discovery to finish");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].kind, HarnessKind::Codex);
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
        assert_eq!(options.kind, Some(HarnessKind::Codex));
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
            Some(HarnessKind::Acp(AcpProfileId::new("cursor")))
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
}
