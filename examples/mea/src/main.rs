//! `mea`: the unpublished smoke and capture CLI for mango-external-agents.
//!
//! Three subcommands against a real installed vendor CLI:
//!
//! ```text
//! mea discover [--harness claude|codex]
//! mea turn     [--harness claude|codex] [--level read-only|default|full-access] <prompt>
//! mea capture codex [--out DIR] [--workspace DIR]
//! ```
//!
//! `doctor` lands with the vendor-drift workflow. What is here is what proves a
//! harness against the binary a user actually has, which no fixture can: fixtures prove the
//! dialect, and this proves the fixtures still describe the vendor.
//!
//! It is not published, and nothing in the library depends on it.

mod capture;
mod redact;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::{
    AuthState, EnvSource, ExecutablePath, HarnessKind, HarnessRegistry, HostContext, OpenSession,
    PermissionLevel, TurnRequest,
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
/// Claude and Codex currently implement the shared `Harness` contract.
fn registry() -> HarnessRegistry {
    HarnessRegistry::new(vec![
        mango_agent_claude::harness(),
        Arc::new(mango_agent_codex::CodexHarness::new()),
    ])
    .expect("expected one harness per kind")
}

/// A context with the default launcher, this process's environment and this directory.
///
/// `mea` is the host here, so it answers the three questions a host owes the library: how to spawn,
/// which directory is authorised, and who it says it is. The environment allowlist still applies —
/// reading this process's environment is not the same as passing it on.
fn host() -> Result<HostContext, String> {
    HostContext::builder()
        .launcher(Arc::new(TokioLauncher::new()))
        .cwd(std::env::current_dir().map_err(|error| format!("no working directory: {error}"))?)
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
    match command.as_str() {
        "discover" => discover(&Options::parse(rest)?).await,
        "turn" => turn(&Options::parse(rest)?).await,
        "capture" => capture(rest).await,
        other => Err(format!(
            "expected `discover`, `turn` or `capture`, received {other:?}"
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
    println!("usage: mea discover [--harness claude|codex]");
    println!(
        "       mea turn [--harness claude|codex] [--level read-only|default|full-access] <prompt>"
    );
    println!("       mea capture codex [--out DIR] [--workspace DIR]");
}

/// What a subcommand was asked for.
struct Options {
    kind: HarnessKind,
    level: PermissionLevel,
    prompt: String,
}

impl Options {
    fn parse(arguments: &[String]) -> Result<Self, String> {
        let mut kind = HarnessKind::Claude;
        let mut level = PermissionLevel::ReadOnly;
        let mut words: Vec<&str> = Vec::new();
        let mut rest = arguments.iter();
        while let Some(argument) = rest.next() {
            match argument.as_str() {
                "--harness" => {
                    let named = rest
                        .next()
                        .ok_or("expected a harness kind after --harness")?;
                    kind = match named.as_str() {
                        "claude" => HarnessKind::Claude,
                        "codex" => HarnessKind::Codex,
                        other => {
                            return Err(format!(
                                "expected `claude` or `codex`, received {other:?}"
                            ));
                        }
                    };
                }
                "--level" => {
                    let named = rest.next().ok_or("expected a level after --level")?;
                    level = match named.as_str() {
                        "read-only" => PermissionLevel::ReadOnly,
                        "default" => PermissionLevel::Default,
                        "full-access" => PermissionLevel::FullAccess,
                        other => {
                            return Err(format!(
                                "expected `read-only`, `default` or `full-access`, received {other:?}"
                            ));
                        }
                    };
                }
                word => words.push(word),
            }
        }
        Ok(Self {
            kind,
            level,
            prompt: words.join(" "),
        })
    }
}

async fn discover(options: &Options) -> Result<(), String> {
    let host = host()?;
    let registry = registry();
    let harness = registry.require(&options.kind).map_err(|e| e.to_string())?;
    let discovery = harness.discover(&host).await.map_err(|e| e.to_string())?;

    println!("harness:      {}", options.kind);
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
    // The harness's own declaration, not a probed one: `Discovery` has no field for the matrix
    // this account and this build actually allow, so what is printed here is the ceiling and a
    // narrowed cell only surfaces as an `open_session` refusal. Labelled rather than quietly
    // printed, because it sits two lines under a probed `auth:` and would read as one.
    println!("declared matrix (before any probe):");
    for cell in harness.permission_matrix().cells() {
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

async fn turn(options: &Options) -> Result<(), String> {
    if options.prompt.trim().is_empty() {
        return Err(String::from("expected a prompt, received none"));
    }
    let host = host()?;
    let registry = registry();
    let harness = registry.require(&options.kind).map_err(|e| e.to_string())?;

    let discovery = harness.discover(&host).await.map_err(|e| e.to_string())?;
    if let AuthState::LoggedOut { login_hint } = &discovery.auth {
        return Err(format!("not signed in; run `{login_hint}`"));
    }

    let mut request = OpenSession::new(format!("mea-{}", uuid_like()));
    request.configuration.level = options.level;
    if let Some(executable) = &discovery.executable {
        request = request.with_executable(ExecutablePath::resolved(executable.clone()));
    }

    let session = harness
        .open_session(&host, request)
        .await
        .map_err(|e| e.to_string())?;
    eprintln!("session: {}", session.ids().native_session_id);

    let mut stream = session
        .start_turn(TurnRequest::new("mea-turn-1", options.prompt.clone()))
        .await
        .map_err(|e| e.to_string())?;

    while let Some(event) = stream.recv().await {
        println!("{}", serde_json::json!(event.kind));
    }
    session
        .close(mango_external_agents::CloseReason::Requested)
        .await
        .map_err(|e| e.to_string())
}

async fn capture(arguments: &[String]) -> Result<(), String> {
    let harness = arguments.first().map(String::as_str).unwrap_or("codex");
    if harness != "codex" {
        return Err(format!(
            "expected a harness this command can capture (`codex`), received {harness:?}"
        ));
    }
    let out = flag(arguments, "--out").map_or_else(capture::default_out_dir, PathBuf::from);
    let workspace = flag(arguments, "--workspace")
        .map_or_else(|| std::env::temp_dir().join("mea-capture"), PathBuf::from);
    std::fs::create_dir_all(&workspace)
        .map_err(|error| format!("expected a writable capture workspace, received {error}"))?;
    capture::codex(&out, &workspace)
        .await
        .map_err(|error| error.to_string())
}

fn flag(arguments: &[String], name: &str) -> Option<String> {
    arguments
        .iter()
        .position(|argument| argument == name)
        .and_then(|at| arguments.get(at + 1))
        .cloned()
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
    use super::{Options, flag, harness_kinds, registry};
    use mango_external_agents::{HarnessKind, PermissionLevel};

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
    }

    #[test]
    fn defaults_to_claude_at_the_narrow_end_of_the_permission_axis() {
        let options = Options::parse(&[String::from("say"), String::from("hello")])
            .expect("expected the arguments to parse");
        assert_eq!(options.kind, HarnessKind::Claude);
        assert_eq!(options.level, PermissionLevel::ReadOnly);
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
        assert_eq!(options.level, PermissionLevel::FullAccess);
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
        assert_eq!(options.kind, HarnessKind::Codex);
        assert_eq!(options.prompt, "ship");
    }

    #[test]
    fn a_capture_flag_reads_the_argument_after_it() {
        let arguments: Vec<String> = ["codex", "--out", "fixtures/codex"]
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect();
        assert_eq!(flag(&arguments, "--out").as_deref(), Some("fixtures/codex"));
        assert_eq!(flag(&arguments, "--workspace"), None);
    }

    #[test]
    fn a_capture_flag_with_no_value_reads_as_absent() {
        let arguments = vec![String::from("--out")];
        assert_eq!(flag(&arguments, "--out"), None);
    }

    #[test]
    fn refuses_a_value_it_does_not_recognise_rather_than_guessing() {
        for arguments in [
            vec![String::from("--level"), String::from("yolo")],
            vec![String::from("--harness"), String::from("acp")],
            vec![String::from("--level")],
        ] {
            assert!(
                Options::parse(&arguments).is_err(),
                "expected {arguments:?} to be refused"
            );
        }
    }
}
