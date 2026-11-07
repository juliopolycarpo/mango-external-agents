//! `mea capture`: records a real vendor conversation as a fixture the fakes replay.
//!
//! Every line the library writes and every line the vendor answers with, in order, redacted and
//! written as JSONL. The scenarios are scripted here rather than typed at a prompt, so re-running
//! the command against a newer build produces a diff against the same conversation rather than a
//! different one.
//!
//! This is the only way a fixture is made. Hand-writing one produces a transcript that agrees with
//! whatever the author believed, which is exactly the belief a fixture exists to check.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::HostContext;
use mango_external_agents::error::{Error, Result};
use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::link::{LinkReceiver, LinkSender};
use mango_external_agents::transport::{ExecutablePath, StdioSpec};
use mango_external_agents::transports::stdio;
use serde_json::{Map, Value, json};

use crate::redact;

mod contracts;

pub use contracts::{DEFAULT_ACP_CAPTURE_PROFILE, acp, claude, codex_contract, manifest};

/// Which direction a recorded line went.
///
/// Written into the fixture so a replaying fake knows what to expect and what to answer with.
pub const SENT: &str = ">>";
/// A line the vendor wrote.
pub const RECEIVED: &str = "<<";

/// How long one scripted step waits for the vendor before moving on.
const STEP_TIMEOUT: Duration = Duration::from_secs(90);

/// One scripted conversation.
struct Scenario {
    /// The file it is written to, under `fixtures/codex/`.
    name: &'static str,
    /// What it proves, for the header line.
    purpose: &'static str,
}

/// The scenarios `mea capture codex` records.
const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "handshake",
        purpose: "the handshake, a thread, and every read-only call a session makes",
    },
    Scenario {
        name: "turn",
        purpose: "two turns on one thread, one of which runs a command",
    },
    Scenario {
        name: "approval",
        purpose: "a turn that asks for a command approval and is refused",
    },
    Scenario {
        name: "interrupt",
        purpose: "a turn stopped by turn/interrupt",
    },
    Scenario {
        name: "review",
        purpose: "the vendor's own review, run inline on the session's thread",
    },
    Scenario {
        name: "review-base-branch",
        purpose: "an inline review against the current branch",
    },
    Scenario {
        name: "review-commit",
        purpose: "an inline review of HEAD",
    },
    Scenario {
        name: "review-custom",
        purpose: "an inline review with host-provided instructions",
    },
    Scenario {
        name: "permission-transitions",
        purpose: "explicit policies followed by a turn that inherits full access",
    },
    Scenario {
        name: "user-defaults",
        purpose: "a thread and turn that leave the user's Codex permission defaults untouched",
    },
    Scenario {
        name: "subagent",
        purpose: "one read-only subagent with multi_agent_v2 enabled for the capture process",
    },
];

/// The named Codex transcript scenarios, for the CLI's `--scenario` parser.
pub fn scenario_names() -> Vec<&'static str> {
    SCENARIOS.iter().map(|scenario| scenario.name).collect()
}

/// Records every Codex scenario, or only `selected`, into `out_dir`.
///
/// # Errors
///
/// Whatever the launcher or the vendor reported. A scenario that fails leaves the fixtures written
/// so far in place: a partial capture is a diff to read, not a reason to lose the rest.
pub async fn codex(out_dir: &Path, workspace: &Path, selected: Option<&str>) -> Result<()> {
    let host = HostContext::builder()
        .launcher(Arc::new(TokioLauncher::new()))
        .cwd(workspace.to_path_buf())
        .client_info("mea", env!("CARGO_PKG_VERSION"))
        .environment(mango_external_agents::EnvSource::from_process())
        .build()?;
    codex_with(&host, out_dir, workspace, selected).await
}

/// Refuses a `codex` older than the library's floor, before any app-server is started.
///
/// An older app-server writes sqlite state, an installation id and system skills under
/// `CODEX_HOME` merely by starting, and a capture would record a conversation the library cannot
/// drive. A `--version` line nobody can parse is not a refusal, as in the library.
///
/// # Errors
///
/// [`Error::VersionGate`] for a build below the floor, and whatever the version command reported
/// when it could not run.
async fn require_supported_codex(host: &HostContext) -> Result<Option<String>> {
    let output = contracts::command_output(
        host,
        vec![String::from("codex"), String::from("--version")],
        &["CODEX_HOME"],
    )
    .await?;
    let Some(version) = mango_agent_codex::discovery::parse_version(&output) else {
        return Ok(None);
    };
    let minimum = mango_agent_codex::MINIMUM_CODEX_VERSION;
    if mango_agent_codex::discovery::meets_minimum(&version, minimum) == Some(false) {
        return Err(Error::VersionGate {
            found: version,
            minimum: String::from(minimum),
        });
    }
    Ok(Some(version))
}

/// [`codex`] on a host the caller built, so a test can inject its own launcher.
async fn codex_with(
    host: &HostContext,
    out_dir: &Path,
    workspace: &Path,
    selected: Option<&str>,
) -> Result<()> {
    let scenarios: Vec<_> = SCENARIOS
        .iter()
        .filter(|scenario| match selected {
            Some(name) => scenario.name == name,
            None => scenario.name != "subagent",
        })
        .collect();
    if scenarios.is_empty() {
        return Err(Error::HostConfiguration {
            expected: "a named Codex transcript scenario",
            received: selected.unwrap_or_default().to_owned(),
        });
    }
    let version = require_supported_codex(host).await?;
    std::fs::create_dir_all(out_dir).map_err(|error| Error::HostConfiguration {
        expected: "a writable fixture directory",
        received: error.to_string(),
    })?;

    for scenario in scenarios {
        println!("capturing codex/{}: {}", scenario.name, scenario.purpose);
        let prefix = if scenario.name == "subagent" {
            let metadata = json!({
                "codexVersion": version,
                "captureVersion": env!("CARGO_PKG_VERSION"),
                "capturedAtUnixSeconds": host.now().duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs()).ok(),
                "features": ["multi_agent_v2"],
            });
            format!("# capture {metadata}\n")
        } else {
            String::new()
        };
        let partial = out_dir.join(format!("{}.partial.jsonl", scenario.name));
        let diagnostic =
            (scenario.name == "subagent").then_some((partial.as_path(), prefix.as_str()));
        let conversation = record(host, scenario, workspace, diagnostic).await?;
        let mut recorded = prefix + &conversation;
        let qualified = scenario.name != "subagent" || has_captured_subagent(&recorded);
        let unqualified = out_dir.join("subagent.unqualified.jsonl");
        let path = if qualified {
            out_dir.join(format!("{}.jsonl", scenario.name))
        } else {
            recorded.insert_str(0, "# unqualified capture\n");
            unqualified.clone()
        };
        std::fs::write(&path, &recorded).map_err(|error| Error::HostConfiguration {
            expected: "a writable fixture file",
            received: error.to_string(),
        })?;
        if scenario.name == "subagent" {
            for path in std::iter::once(&partial).chain(qualified.then_some(&unqualified)) {
                if let Err(error) = std::fs::remove_file(path)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(Error::HostConfiguration {
                        expected: "a removable stale capture diagnostic",
                        received: error.to_string(),
                    });
                }
            }
        }
        if !qualified {
            let kinds: std::collections::BTreeSet<_> = recorded
                .lines()
                .filter_map(|line| line.strip_prefix(RECEIVED))
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .filter(|frame| {
                    matches!(
                        frame["method"].as_str(),
                        Some("item/started" | "item/completed")
                    )
                })
                .filter_map(|frame| frame["params"]["item"]["type"].as_str().map(str::to_owned))
                .collect();
            let kinds = if kinds.is_empty() {
                String::from("none")
            } else {
                kinds.into_iter().collect::<Vec<_>>().join(", ")
            };
            let received = mango_external_agents::redact::bounded(
                &format!("item types: {kinds}; retained unqualified capture"),
                mango_external_agents::normalize::TextLimit::ErrorMessage,
            )
            .text;
            println!("  retained unqualified capture {}", path.display());
            return Err(Error::Protocol {
                expected: String::from(
                    "a captured subAgentActivity item with a nonempty agentThreadId",
                ),
                received,
            });
        }
        println!("  wrote {}", path.display());
    }
    Ok(())
}

/// A completed turn alone is not evidence of an attributed child item.
fn has_captured_subagent(recorded: &str) -> bool {
    use mango_agent_codex::protocol::{Notification, ThreadItem};

    recorded
        .lines()
        .filter_map(|line| line.strip_prefix(RECEIVED))
        .any(|line| {
            let Ok(frame) = serde_json::from_str::<Value>(line) else {
                return false;
            };
            let Some(method) = frame["method"].as_str() else {
                return false;
            };
            let notification = Notification::parse(method, frame["params"].clone());
            let (Notification::ItemStarted(item) | Notification::ItemCompleted(item)) =
                notification
            else {
                return false;
            };
            matches!(item.item, ThreadItem::SubAgentActivity { agent_thread_id: Some(id), .. }
                if !id.is_empty() && id != redact::PLACEHOLDER)
        })
}

/// One scenario, as the lines both sides wrote.
///
/// A child that cannot be reaped fails the capture even when the conversation succeeded: the
/// fixture is not affected, but a `codex app-server` left running on the maintainer's machine is
/// not something to report as a clean run. When the conversation failed too, that error is the one
/// returned and the cleanup failure is printed beside it.
async fn record(
    host: &HostContext,
    scenario: &Scenario,
    workspace: &Path,
    diagnostic: Option<(&Path, &str)>,
) -> Result<String> {
    let spec = if scenario.name == "subagent" {
        StdioSpec::new(["codex", "--enable", "multi_agent_v2", "app-server"])
    } else {
        StdioSpec::new(["codex", "app-server"])
    };
    let transport = stdio::open(host, &spec, &ExecutablePath::default(), &["CODEX_HOME"]).await?;
    let (sender, receiver) = transport.link.split();
    let mut recorder = Recorder {
        sender,
        receiver,
        cwd: workspace.to_string_lossy().into_owned(),
        home: home_directory(),
        user: account_name(),
        lines: vec![format!("# codex/{}: {}", scenario.name, scenario.purpose)],
    };

    let outcome = recorder.run(scenario).await;
    let stopped = transport
        .control
        .kill(mango_external_agents::CancelReason::Shutdown)
        .await;
    match outcome {
        Ok(()) => {
            stopped?;
            Ok(recorder.lines.join("\n") + "\n")
        }
        Err(error) => {
            if let Some((path, prefix)) = diagnostic {
                let partial = format!(
                    "{prefix}# incomplete capture\n{}\n",
                    recorder.lines.join("\n")
                );
                if let Err(write_error) = std::fs::write(path, partial) {
                    eprintln!("could not retain the redacted partial capture: {write_error}");
                }
            }
            if let Err(cleanup) = stopped {
                eprintln!(
                    "codex/{}: the app-server was not reaped: {cleanup}",
                    scenario.name
                );
            }
            Err(error)
        }
    }
}

/// A conversation being written down as it happens.
struct Recorder {
    sender: Box<dyn LinkSender>,
    receiver: Box<dyn LinkReceiver>,
    cwd: String,
    home: String,
    user: String,
    lines: Vec<String>,
}

impl Recorder {
    async fn run(&mut self, scenario: &Scenario) -> Result<()> {
        self.send(json!({"id": 0, "method": "initialize", "params": {
            "clientInfo": {"name": "mea", "title": "mea capture", "version": "0.1.0"}
        }}))
        .await?;
        self.read_until(|frame| frame.get("id") == Some(&json!(0)))
            .await?;
        self.send(json!({"method": "initialized"})).await?;

        // Every scenario reads the account, because every session the harness opens does: it is
        // how a signed-out CLI is refused before a turn is started. A fixture without it would be
        // a recording of a conversation the library never has.
        self.send(json!({"id": 1, "method": "account/read", "params": {}}))
            .await?;
        self.read_until(|frame| frame.get("id") == Some(&json!(1)))
            .await?;

        match scenario.name {
            "handshake" => self.handshake().await,
            "turn" => self.turn().await,
            "approval" => self.approval().await,
            "interrupt" => self.interrupt().await,
            "review" | "review-base-branch" | "review-commit" | "review-custom" => {
                self.review(scenario.name).await
            }
            "permission-transitions" => self.permission_transitions().await,
            "user-defaults" => self.user_defaults().await,
            "subagent" => self.subagent().await,
            other => Err(Error::HostConfiguration {
                expected: "a scenario this command knows",
                received: other.to_owned(),
            }),
        }
    }

    async fn handshake(&mut self) -> Result<()> {
        // A thread first, so this transcript can also stand in for a session that makes only the
        // read-only calls below. Without it the fixture describes a connection nobody opened.
        self.start_thread("read-only", "never").await?;
        self.send(json!({"id": 2, "method": "model/list", "params": {"limit": 4}}))
            .await?;
        self.read_until(|frame| frame.get("id") == Some(&json!(2)))
            .await?;
        self.send(json!({"id": 3, "method": "account/rateLimits/read"}))
            .await?;
        self.read_until(|frame| frame.get("id") == Some(&json!(3)))
            .await?;
        self.send(json!({"id": 4, "method": "thread/list", "params": {"limit": 3}}))
            .await?;
        self.read_until(|frame| frame.get("id") == Some(&json!(4)))
            .await
            .map(|_| ())
    }

    async fn turn(&mut self) -> Result<()> {
        let thread = self.start_thread("read-only", "never").await?;
        self.send(json!({"id": 10, "method": "turn/start", "params": {
            "threadId": thread,
            "input": [{"type": "text", "text":
                "Run the shell command `echo mango` and tell me its output. Nothing else.",
                "text_elements": []}]
        }}))
        .await?;
        self.read_until(is_turn_completed).await?;

        // A second turn on the same thread, because the first one is the only one that announces
        // the vendor session — and a fixture with one turn cannot show the second one not doing
        // it.
        self.send(json!({"id": 11, "method": "turn/start", "params": {
            "threadId": thread,
            "input": [{"type": "text", "text": "Reply with the single word done.",
                       "text_elements": []}]
        }}))
        .await?;
        self.read_until(is_turn_completed).await.map(|_| ())
    }

    async fn subagent(&mut self) -> Result<()> {
        let thread = self.start_thread("read-only", "never").await?;
        self.send(json!({"id": 10, "method": "turn/start", "params": {
            "threadId": thread,
            "input": [{"type": "text", "text":
                "Use exactly one subagent to reply with the single word mango. Wait for it, then reply done. \
                 Do not run commands, read or modify files, or start additional agents.",
                "text_elements": []}]
        }})).await?;
        let terminal = self
            .read_until(|frame| {
                (is_turn_completed(frame) && frame["params"]["threadId"] == thread)
                    || (frame.get("id") == Some(&json!(10)) && frame.get("error").is_some())
            })
            .await?;
        if terminal.get("error").is_some() {
            return Err(Error::Protocol {
                expected: String::from("an accepted turn/start for a subagent capture"),
                received: redact::incoming_frame(terminal, self.paths()).to_string(),
            });
        }
        Ok(())
    }

    async fn approval(&mut self) -> Result<()> {
        let thread = self.start_thread("read-only", "on-request").await?;
        self.send(json!({"id": 10, "method": "turn/start", "params": {
            "threadId": thread,
            "input": [{"type": "text", "text":
                "Create a file called mango.txt containing the word mango, with a shell command. \
                 Ask for approval to leave the sandbox if you need it.",
                "text_elements": []}]
        }}))
        .await?;

        let asked = self
            .read_until(|frame| {
                frame
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(|method| method.ends_with("requestApproval"))
            })
            .await?;
        let id = asked.get("id").cloned().unwrap_or(json!(0));
        // Refused, never granted. A fixture that captured a grant would be a recording of this
        // tool letting an agent out of its sandbox, checked into the repository.
        self.send(json!({"id": id, "result": {"decision": "decline"}}))
            .await?;
        self.read_until(is_turn_completed).await.map(|_| ())
    }

    async fn interrupt(&mut self) -> Result<()> {
        let thread = self.start_thread("read-only", "never").await?;
        self.send(json!({"id": 10, "method": "turn/start", "params": {
            "threadId": thread,
            "input": [{"type": "text", "text": "Count slowly from 1 to 200, one per line.",
                       "text_elements": []}]
        }}))
        .await?;
        let started = self
            .read_until(|frame| frame.get("id") == Some(&json!(10)))
            .await?;
        let turn = started["result"]["turn"]["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();

        // Long enough that the turn is genuinely running, so the transcript shows an interrupt
        // rather than a race against a turn that had not started.
        tokio::time::sleep(Duration::from_secs(4)).await;
        self.send(json!({"id": 11, "method": "turn/interrupt",
                         "params": {"threadId": thread, "turnId": turn}}))
            .await?;
        self.read_until(is_turn_completed).await.map(|_| ())
    }

    async fn review(&mut self, scenario: &str) -> Result<()> {
        let thread = self.start_thread("read-only", "never").await?;
        // Without `delivery`, which the server reads as inline: a detached review runs on a thread
        // the session is not subscribed to, and this transcript would describe events the harness
        // is right to drop.
        self.send(json!({"id": 10, "method": "review/start", "params": {
            "threadId": thread, "target": review_target(scenario)
        }}))
        .await?;
        self.read_until(is_turn_completed).await.map(|_| ())
    }

    async fn permission_transitions(&mut self) -> Result<()> {
        let thread = self.start_thread("read-only", "never").await?;
        for (index, level) in [
            mango_external_agents::PermissionLevel::ReadOnly,
            mango_external_agents::PermissionLevel::Default,
            mango_external_agents::PermissionLevel::FullAccess,
        ]
        .into_iter()
        .enumerate()
        {
            let configuration = mango_agent_codex::permissions::VendorConfiguration::for_pair(
                level,
                mango_external_agents::ApprovalRouting::User,
            );
            self.send(json!({"id": 10 + index, "method": "turn/start", "params": {
                "threadId": thread,
                "input": [{"type": "text", "text": "Reply with the single word mango. Do not use tools.", "text_elements": []}],
                "approvalPolicy": configuration.approval_policy,
                "sandboxPolicy": configuration.sandbox_policy(&self.cwd),
                "approvalsReviewer": configuration.approvals_reviewer
            }})).await?;
            self.read_until(is_turn_completed).await?;
        }

        // Codex keeps a thread's explicit selection. Omitting every permission member on the next
        // turn records that inheritance rather than accidentally forcing a host default again.
        self.send(json!({"id": 13, "method": "turn/start", "params":
            unconfigured_turn_params(&thread)
        }))
        .await?;
        self.read_until(is_turn_completed).await?;
        Ok(())
    }

    async fn user_defaults(&mut self) -> Result<()> {
        // An omitted initial policy asks Codex to use the user's own configured defaults. This is
        // deliberately distinct from a read-only policy, which would overwrite that choice.
        let thread = self.start_thread_with(None, None).await?;
        self.send(json!({"id": 10, "method": "turn/start", "params":
            unconfigured_turn_params(&thread)
        }))
        .await?;
        self.read_until(is_turn_completed).await.map(|_| ())
    }

    async fn start_thread(&mut self, sandbox: &str, approval_policy: &str) -> Result<String> {
        self.start_thread_with(Some(sandbox), Some(approval_policy))
            .await
    }

    async fn start_thread_with(
        &mut self,
        sandbox: Option<&str>,
        approval_policy: Option<&str>,
    ) -> Result<String> {
        self.send(json!({"id": 5, "method": "thread/start", "params":
            thread_start_params(&self.cwd, sandbox, approval_policy)
        }))
        .await?;
        let started = self
            .read_until(|frame| frame.get("id") == Some(&json!(5)))
            .await?;
        started["result"]["thread"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::Protocol {
                expected: String::from("a thread/start answer naming a thread"),
                received: started.to_string(),
            })
    }

    async fn send(&mut self, frame: Value) -> Result<()> {
        let line = frame.to_string();
        self.lines.push(format!(
            "{SENT}{}",
            redact::outgoing_frame(frame, self.paths())
        ));
        self.sender.send(line).await
    }

    fn paths(&self) -> redact::Paths<'_> {
        redact::Paths {
            cwd: &self.cwd,
            home: &self.home,
            user: &self.user,
        }
    }

    /// Reads and records until a frame matches, or the step's deadline passes.
    async fn read_until(&mut self, matches: impl Fn(&Value) -> bool) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        let request = self
            .lines
            .iter()
            .rev()
            .filter_map(|line| line.strip_prefix(SENT))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|frame| frame["method"].as_str().map(str::to_owned))
            .unwrap_or_else(|| String::from("vendor"));
        loop {
            let next = tokio::time::timeout_at(deadline, self.receiver.recv())
                .await
                .map_err(|_| Error::Timeout {
                    operation: format!("a captured {request} step"),
                    after: STEP_TIMEOUT,
                })?;
            let Some(line) = next? else {
                return Err(Error::Link {
                    peer: String::from("Codex app-server"),
                    message: String::from("the app-server exited mid-capture"),
                });
            };
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            self.lines.push(format!(
                "{RECEIVED}{}",
                redact::incoming_frame(frame.clone(), self.paths())
            ));
            if matches(&frame) {
                return Ok(frame);
            }
        }
    }
}

/// The home directory this capture is running under, for the redactor to rewrite.
///
/// Read from the environment rather than derived: the capture is a developer tool, and a home
/// directory it could not find is one nothing gets rewritten for — which the fixture review would
/// catch, rather than a wrong guess that quietly rewrote the wrong prefix.
fn home_directory() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default()
}

/// The account this capture runs as, for the same reason and from the same place.
///
/// A turn runs commands and the vendor reports what they printed, so the operator's login reaches
/// a fixture through `ls -la` rather than through any member a denylist could name.
fn account_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default()
}

fn is_turn_completed(frame: &Value) -> bool {
    frame.get("method") == Some(&json!("turn/completed"))
}

/// Builds `thread/start` members without imposing a permission choice when one is absent.
fn thread_start_params(cwd: &str, sandbox: Option<&str>, approval_policy: Option<&str>) -> Value {
    let mut params = Map::from_iter([(String::from("cwd"), json!(cwd))]);
    if let Some(sandbox) = sandbox {
        params.insert(String::from("sandbox"), json!(sandbox));
    }
    if let Some(approval_policy) = approval_policy {
        params.insert(String::from("approvalPolicy"), json!(approval_policy));
    }
    Value::Object(params)
}

/// Builds a no-tools turn that deliberately leaves Codex permission members absent.
fn unconfigured_turn_params(thread: &str) -> Value {
    json!({
        "threadId": thread,
        "input": [{
            "type": "text",
            "text": "Reply with the single word mango. Do not use tools.",
            "text_elements": []
        }]
    })
}

fn review_target(scenario: &str) -> Value {
    match scenario {
        "review-base-branch" => json!({"type": "baseBranch", "branch": "HEAD"}),
        "review-commit" => json!({"type": "commit", "sha": "HEAD", "title": "Current commit"}),
        "review-custom" => {
            json!({"type": "custom", "instructions": "Review the current changes for correctness. Do not modify files."})
        }
        _ => json!({"type": "uncommittedChanges"}),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::{
        SCENARIOS, Scenario, codex_with, record, review_target, thread_start_params,
        unconfigured_turn_params,
    };
    use mango_external_agents::process::{
        LaunchSpec, ManagedProcess, ProcessControl, ProcessLauncher,
    };
    use mango_external_agents::testing::{FakeLauncher, FakeProcess};
    use mango_external_agents::{CancelReason, EnvSource, Error, ExitStatus, HostContext, Result};
    use serde_json::{Value, json};

    /// A Codex app-server that answers every request, and names a thread for `thread/start`.
    fn app_server() -> FakeProcess {
        FakeProcess::responding(|line| {
            let Ok(frame) = serde_json::from_str::<Value>(line) else {
                return Vec::new();
            };
            let (Some(_), Some(id)) = (frame.get("method"), frame.get("id")) else {
                return Vec::new();
            };
            vec![json!({"id": id, "result": {"thread": {"id": "thread-1"}}}).to_string()]
        })
    }

    /// Mechanical replies for scenario-selection tests, not a vendor transcript.
    fn archival_app_server() -> FakeProcess {
        FakeProcess::responding(|line| {
            let frame: Value = serde_json::from_str(line).expect("scripted capture request");
            let terminal = json!({"method": "turn/completed", "params": {
                "threadId": "parent-thread", "turn": {"id": "turn", "status": "completed"}
            }});
            if frame["result"]["decision"] == "decline" {
                return vec![terminal.to_string()];
            }
            let (Some(method), Some(id)) = (frame["method"].as_str(), frame.get("id")) else {
                return Vec::new();
            };
            let response = json!({"id": id, "result": {
                "thread": {"id": "parent-thread"}, "turn": {"id": "turn"}
            }});
            match method {
                "turn/start" if frame["params"]["input"][0]["text"].as_str().is_some_and(|text| text.contains("mango.txt")) => vec![
                    response.to_string(), json!({"id": 90, "method": "item/commandExecution/requestApproval", "params": {}}).to_string()
                ],
                "turn/start" if frame["params"]["input"][0]["text"].as_str().is_some_and(|text| text.contains("Count slowly")) => vec![response.to_string()],
                "turn/start" | "turn/interrupt" | "review/start" => vec![response.to_string(), terminal.to_string()],
                _ => vec![response.to_string()],
            }
        })
    }

    /// Scripted protocol traffic, not a claimed vendor capture.
    fn subagent_app_server(with_child_item: bool, refused: bool) -> FakeProcess {
        FakeProcess::responding(move |line| {
            let Ok(frame) = serde_json::from_str::<Value>(line) else {
                return Vec::new();
            };
            let (Some(method), Some(id)) = (frame["method"].as_str(), frame.get("id")) else {
                return Vec::new();
            };
            if method != "turn/start" {
                return vec![
                    json!({"id": id, "result": {"thread": {"id": "parent-thread"}}}).to_string(),
                ];
            }
            if refused {
                return vec![
                    json!({"id": id, "error": {"code": -32000, "message": "capture refused"}})
                        .to_string(),
                ];
            }
            let item = if with_child_item {
                json!({"type": "subAgentActivity", "id": "child-item", "kind": "started",
                    "agentPath": "root/child", "agentThreadId": "child-thread"})
            } else {
                json!({"type": "collabAgentToolCall", "id": "collab", "agentThreadId": "wrong-family"})
            };
            [
                json!({"id": id, "result": {"turn": {"id": "parent-turn", "status": "inProgress"}}}),
                json!({"method": "item/started", "params": {"threadId": "parent-thread", "turnId": "parent-turn", "item": item}}),
                json!({"method": "turn/completed", "params": {"threadId": "child-thread", "turn": {"id": "child-turn", "status": "completed"}}}),
                json!({"method": "turn/completed", "params": {"threadId": "parent-thread", "turn": {"id": "parent-turn", "status": "completed"}}}),
            ].into_iter().map(|frame| frame.to_string()).collect()
        })
    }

    /// A launcher whose children end on a kill but report that they could not be reaped.
    struct UnreapableLauncher {
        inner: Arc<FakeLauncher>,
    }

    struct UnreapableControl {
        inner: Arc<dyn ProcessControl>,
    }

    #[async_trait::async_trait]
    impl ProcessLauncher for UnreapableLauncher {
        async fn spawn(&self, spec: LaunchSpec) -> Result<ManagedProcess> {
            let is_version_probe = spec.argv.get(1).map(String::as_str) == Some("--version");
            let process = self.inner.spawn(spec).await?;
            if is_version_probe {
                return Ok(process);
            }
            Ok(ManagedProcess {
                stdout: process.stdout,
                stdin: process.stdin,
                control: Arc::new(UnreapableControl {
                    inner: process.control,
                }),
            })
        }
    }

    #[async_trait::async_trait]
    impl ProcessControl for UnreapableControl {
        fn pid(&self) -> Option<u32> {
            self.inner.pid()
        }

        fn stderr_tail(&self) -> String {
            self.inner.stderr_tail()
        }

        async fn wait(&self) -> Result<ExitStatus> {
            self.inner.wait().await
        }

        async fn kill(&self, reason: CancelReason) -> Result<()> {
            self.inner.kill(reason).await?;
            Err(Error::Timeout {
                operation: String::from("reaping the fake app-server"),
                after: Duration::from_secs(2),
            })
        }
    }

    fn host(launcher: Arc<dyn ProcessLauncher>) -> HostContext {
        HostContext::builder()
            .launcher(launcher)
            .cwd(std::env::temp_dir())
            .environment(EnvSource::from_pairs([("PATH", "/fake/bin")]))
            .client_info("mea-test", "0.0.0")
            .build()
            .expect("expected a fake capture host")
    }

    fn handshake() -> &'static Scenario {
        SCENARIOS
            .iter()
            .find(|scenario| scenario.name == "handshake")
            .expect("expected a handshake scenario")
    }

    fn unreapable(launcher: &Arc<FakeLauncher>) -> Arc<UnreapableLauncher> {
        Arc::new(UnreapableLauncher {
            inner: Arc::clone(launcher),
        })
    }

    #[tokio::test]
    async fn record_returns_the_transcript_when_the_child_is_reaped() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(app_server());

        let recorded = record(
            &host(launcher.clone()),
            handshake(),
            &std::env::temp_dir(),
            None,
        )
        .await
        .expect("expected a reaped capture to succeed");

        assert!(
            recorded.contains("account/read"),
            "expected a transcript that reads the account | received {recorded}"
        );
        assert_eq!(launcher.live_children(), 0);
    }

    #[tokio::test]
    async fn a_named_subagent_capture_records_only_that_scenario_and_waits_for_its_parent() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(version_line(mango_agent_codex::MINIMUM_CODEX_VERSION));
        launcher.push(subagent_app_server(true, false));
        let out =
            std::env::temp_dir().join(format!("mea-selected-subagent-{}", std::process::id()));
        std::fs::create_dir_all(&out).expect("test capture directory");
        std::fs::write(
            out.join("subagent.partial.jsonl"),
            "# incomplete older attempt\n",
        )
        .expect("old partial");
        std::fs::write(
            out.join("subagent.unqualified.jsonl"),
            "# unqualified older attempt\n",
        )
        .expect("old unqualified record");
        let result = codex_with(
            &host(launcher.clone()),
            &out,
            &std::env::temp_dir(),
            Some("subagent"),
        )
        .await;
        let recorded = std::fs::read_to_string(out.join("subagent.jsonl")).unwrap_or_default();
        let files = std::fs::read_dir(&out)
            .map(|files| files.count())
            .unwrap_or_default();
        if out.exists() {
            std::fs::remove_dir_all(&out).expect("remove generated test capture");
        }

        assert!(
            result.is_ok(),
            "expected a qualified child capture, received {result:?}"
        );
        assert_eq!(files, 1);
        assert_eq!(
            launched(&launcher),
            [
                "codex --version",
                "codex --enable multi_agent_v2 app-server"
            ]
        );
        assert!(super::has_captured_subagent(&recorded));
        let metadata: Value = serde_json::from_str(
            recorded
                .lines()
                .next()
                .expect("metadata")
                .trim_start_matches("# capture "),
        )
        .expect("capture metadata");
        assert_eq!(
            metadata["codexVersion"],
            mango_agent_codex::MINIMUM_CODEX_VERSION
        );
        assert!(metadata["capturedAtUnixSeconds"].is_u64());
        let last: Value = serde_json::from_str(
            recorded
                .lines()
                .last()
                .expect("parent terminal")
                .trim_start_matches(super::RECEIVED),
        )
        .expect("terminal frame");
        assert_eq!(last["params"]["threadId"], "parent-thread");
        assert_eq!(launcher.live_children(), 0);
    }

    #[tokio::test]
    async fn a_completed_capture_without_an_attributed_child_fails_and_retains_its_record() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(version_line(mango_agent_codex::MINIMUM_CODEX_VERSION));
        launcher.push(subagent_app_server(false, false));
        let out = std::env::temp_dir().join(format!("mea-absent-subagent-{}", std::process::id()));
        std::fs::create_dir_all(&out).expect("test capture directory");
        std::fs::write(
            out.join("subagent.partial.jsonl"),
            "# incomplete older attempt\n",
        )
        .expect("old partial");
        let result = codex_with(
            &host(launcher.clone()),
            &out,
            &std::env::temp_dir(),
            Some("subagent"),
        )
        .await;
        let recorded =
            std::fs::read_to_string(out.join("subagent.unqualified.jsonl")).unwrap_or_default();
        let credited = out.join("subagent.jsonl").exists();
        let partial = out.join("subagent.partial.jsonl").exists();
        if out.exists() {
            std::fs::remove_dir_all(&out).expect("remove unqualified test capture");
        }

        assert!(
            !credited,
            "expected an unqualified capture not to use the success filename"
        );
        assert!(
            !partial,
            "expected the completed diagnostic to replace the stale partial"
        );
        assert!(matches!(result, Err(Error::Protocol { expected, received })
            if expected.contains("agentThreadId") && received.contains("collabAgentToolCall")));
        assert!(recorded.starts_with("# unqualified capture\n"));
        assert!(recorded.contains("collabAgentToolCall"));
        assert!(!super::has_captured_subagent(&recorded));
        assert_eq!(launcher.live_children(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_default_archival_capture_never_enables_or_runs_the_subagent_scenario() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(version_line(mango_agent_codex::MINIMUM_CODEX_VERSION));
        for _ in SCENARIOS {
            launcher.push(archival_app_server());
        }
        let out = std::env::temp_dir().join(format!("mea-default-archival-{}", std::process::id()));
        let result = codex_with(&host(launcher.clone()), &out, &std::env::temp_dir(), None).await;
        let files = std::fs::read_dir(&out).expect("capture directory").count();
        std::fs::remove_dir_all(&out).expect("remove test captures");
        assert!(
            result.is_ok(),
            "expected the archival set alone to succeed: {result:?}"
        );
        assert_eq!(files, 10);
        let launches = launched(&launcher);
        assert_eq!(launches[1], "codex app-server");
        assert_eq!(launches.len(), 11, "version plus ten archival scenarios");
        assert!(
            launches
                .iter()
                .skip(1)
                .all(|argv| argv == "codex app-server")
        );
        assert!(!launches.iter().any(|argv| argv.contains("--enable")));
        assert_eq!(launcher.live_children(), 0);
    }

    #[tokio::test]
    async fn an_unqualified_capture_reports_when_no_item_type_was_received() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(version_line(mango_agent_codex::MINIMUM_CODEX_VERSION));
        launcher.push(archival_app_server());
        let out =
            std::env::temp_dir().join(format!("mea-no-subagent-items-{}", std::process::id()));
        let result = codex_with(
            &host(launcher.clone()),
            &out,
            &std::env::temp_dir(),
            Some("subagent"),
        )
        .await;
        std::fs::remove_dir_all(&out).expect("remove test diagnostic");
        assert!(
            matches!(result, Err(Error::Protocol { received, .. }) if received.contains("item types: none"))
        );
        assert_eq!(launcher.live_children(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_capture_names_its_step_and_retains_only_redacted_partial_traffic() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(version_line(mango_agent_codex::MINIMUM_CODEX_VERSION));
        launcher.push(FakeProcess::responding(|line| {
            let frame: Value = serde_json::from_str(line).expect("scripted request");
            let Some(id) = frame.get("id") else { return Vec::new(); };
            let result = match frame["method"].as_str() {
                Some("initialize") => json!({}),
                Some("account/read") => json!({"account": {"type": "chatgpt", "email": "capture-canary@example.invalid"}}),
                _ => return Vec::new(),
            };
            vec![json!({"id": id, "result": result}).to_string()]
        }));
        let out = std::env::temp_dir().join(format!("mea-stalled-subagent-{}", std::process::id()));
        let workspace = out.join("private-workspace-canary");
        let result = codex_with(&host(launcher.clone()), &out, &workspace, Some("subagent")).await;
        let partial = std::fs::read_to_string(out.join("subagent.partial.jsonl"))
            .expect("retained partial traffic");
        assert!(!out.join("subagent.jsonl").exists());
        std::fs::remove_dir_all(&out).expect("remove partial test capture");
        assert!(
            matches!(&result, Err(Error::Timeout { operation, after })
            if operation.contains("thread/start") && *after == super::STEP_TIMEOUT),
            "expected the stalled step and its deadline, received {result:?}"
        );
        assert!(partial.contains("# incomplete capture"));
        assert!(partial.contains("account/read"));
        assert!(partial.contains("thread/start"));
        let started: Value = partial
            .lines()
            .filter_map(|line| line.strip_prefix(super::SENT))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|frame| frame["method"] == "thread/start")
            .expect("recorded thread/start");
        assert_eq!(started["params"]["cwd"], crate::redact::FIXTURE_CWD);
        assert!(!partial.contains("capture-canary@example.invalid"));
        assert!(!partial.contains(&*workspace.to_string_lossy()));
        assert_eq!(launcher.live_children(), 0);
    }

    #[tokio::test]
    async fn a_refused_subagent_turn_reports_the_refusal_and_reaps() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(subagent_app_server(false, true));
        let scenario = SCENARIOS
            .iter()
            .find(|scenario| scenario.name == "subagent")
            .expect("subagent scenario");
        let result = record(
            &host(launcher.clone()),
            scenario,
            &std::env::temp_dir(),
            None,
        )
        .await;
        let Err(Error::Protocol { expected, received }) = &result else {
            panic!("expected a refused turn/start, received {result:?}");
        };
        assert!(expected.contains("turn/start"));
        let error: Value = serde_json::from_str(received).expect("redacted refusal frame");
        assert_eq!(error["error"]["code"], -32000);
        assert_eq!(launcher.live_children(), 0);
    }

    #[tokio::test]
    async fn an_unknown_scenario_is_refused_before_any_probe_or_child() {
        let launcher = Arc::new(FakeLauncher::new());
        let result = codex_with(
            &host(launcher.clone()),
            &std::env::temp_dir(),
            &std::env::temp_dir(),
            Some("not-a-scenario"),
        )
        .await;
        assert!(
            matches!(result, Err(Error::HostConfiguration { received, .. }) if received == "not-a-scenario")
        );
        assert!(launcher.launches().is_empty());
    }

    #[test]
    fn capture_qualification_requires_a_received_child_item_and_nonempty_identity() {
        for id in [
            json!(""),
            json!(crate::redact::PLACEHOLDER),
            Value::Null,
            json!(7),
        ] {
            let frame = json!({"method": "item/started", "params": {"threadId": "parent-thread", "turnId": "parent-turn", "item": {
                "type": "subAgentActivity", "id": "child", "agentThreadId": id
            }}});
            assert!(!super::has_captured_subagent(&format!(
                "{}{}",
                super::RECEIVED,
                frame
            )));
        }
        let frame = json!({"method": "item/completed", "params": {"threadId": "parent-thread", "turnId": "parent-turn", "item": {
            "type": "subAgentActivity", "id": "child", "agentThreadId": "child-thread"
        }}});
        assert!(!super::has_captured_subagent(&format!(
            "{}{}",
            super::SENT,
            frame
        )));
        assert!(super::has_captured_subagent(&format!(
            "# metadata\n{}{}",
            super::RECEIVED,
            frame
        )));
        assert!(!super::has_captured_subagent(
            "<<not JSON\n# agentThreadId child-thread"
        ));
        for malformed in [
            json!({"method": "item/started", "params": {"threadId": "parent-thread", "turnId": "parent-turn", "item": {
                "type": "subAgentActivity", "agentThreadId": "child-thread"
            }}}),
            json!({"method": "item/started", "params": {"item": {
                "type": "subAgentActivity", "id": "child-item", "agentThreadId": "child-thread"
            }}}),
        ] {
            assert!(
                !super::has_captured_subagent(&format!("{}{}", super::RECEIVED, malformed)),
                "expected a malformed item/context not to qualify: {malformed}"
            );
        }
    }

    #[tokio::test]
    async fn record_fails_when_the_child_cannot_be_reaped() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(app_server());

        let result = record(
            &host(unreapable(&launcher)),
            handshake(),
            &std::env::temp_dir(),
            None,
        )
        .await;

        assert!(
            matches!(&result, Err(Error::Timeout { operation, .. }) if operation.contains("reaping")),
            "expected a capture error naming the reap failure | received {result:?}"
        );
        assert_eq!(
            launcher.live_children(),
            0,
            "expected the child to be ended before the reap failure is reported"
        );
    }

    #[tokio::test]
    async fn a_failed_conversation_keeps_its_own_error_when_the_reap_fails_too() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(app_server());
        let scenario = Scenario {
            name: "unknown",
            purpose: "a scenario this command does not know",
        };

        let result = record(
            &host(unreapable(&launcher)),
            &scenario,
            &std::env::temp_dir(),
            None,
        )
        .await;

        assert!(
            matches!(&result, Err(Error::HostConfiguration { received, .. }) if received == "unknown"),
            "expected the conversation's own error | received {result:?}"
        );
    }

    fn succeeded() -> ExitStatus {
        ExitStatus {
            code: Some(0),
            signal: None,
        }
    }

    /// What `codex --version` prints for `version`.
    fn version_line(version: &str) -> FakeProcess {
        FakeProcess::transcript([format!("codex-cli {version}")]).with_exit(succeeded())
    }

    fn launched(launcher: &FakeLauncher) -> Vec<String> {
        launcher
            .launches()
            .iter()
            .map(|launch| launch.argv.join(" "))
            .collect()
    }

    #[tokio::test]
    async fn a_capture_refuses_an_outdated_codex_without_starting_an_app_server() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(version_line("0.150.0"));
        launcher.push(app_server());
        let out = std::env::temp_dir().join(format!("mea-capture-outdated-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);

        let result = codex_with(&host(launcher.clone()), &out, &std::env::temp_dir(), None).await;

        let launches = launched(&launcher);
        let wrote_a_directory = out.exists();
        let _ = std::fs::remove_dir_all(&out);
        assert_eq!(
            launches,
            ["codex --version"],
            "expected only the version command to launch | received {launches:?}"
        );
        assert!(
            matches!(&result, Err(Error::VersionGate { found, minimum })
                if found == "0.150.0" && minimum == mango_agent_codex::MINIMUM_CODEX_VERSION),
            "expected VersionGate for 0.150.0 | received {result:?}"
        );
        assert!(
            !wrote_a_directory,
            "expected nothing written for a refused capture"
        );
    }

    #[tokio::test]
    async fn a_capture_of_a_current_codex_goes_on_to_start_the_app_server() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(version_line(mango_agent_codex::MINIMUM_CODEX_VERSION));
        launcher.push(app_server());
        let out = std::env::temp_dir().join(format!("mea-capture-current-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);

        let result = codex_with(&host(launcher.clone()), &out, &std::env::temp_dir(), None).await;

        let launches = launched(&launcher);
        let _ = std::fs::remove_dir_all(&out);
        assert_eq!(
            launches,
            ["codex --version", "codex app-server"],
            "expected the version probe, then one app-server | received {launches:?}"
        );
        assert!(
            !matches!(&result, Err(Error::VersionGate { .. })),
            "expected the gate to pass | received {result:?}"
        );
    }

    #[tokio::test]
    async fn an_unparseable_codex_version_does_not_stop_a_capture() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(FakeProcess::transcript(["codex banana"]).with_exit(succeeded()));
        launcher.push(app_server());
        let out = std::env::temp_dir().join(format!("mea-capture-unknown-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);

        let result = codex_with(&host(launcher.clone()), &out, &std::env::temp_dir(), None).await;

        let launches = launched(&launcher);
        let _ = std::fs::remove_dir_all(&out);
        assert_eq!(
            launches,
            ["codex --version", "codex app-server"],
            "expected an unreadable version to proceed | received {launches:?}"
        );
        assert!(
            !matches!(&result, Err(Error::VersionGate { .. })),
            "expected no refusal for an unreadable version | received {result:?}"
        );
    }

    #[tokio::test]
    async fn codex_capture_writes_no_fixture_when_the_child_cannot_be_reaped() {
        let launcher = Arc::new(FakeLauncher::new());
        launcher.push(version_line(mango_agent_codex::MINIMUM_CODEX_VERSION));
        launcher.push(app_server());
        let out = std::env::temp_dir().join(format!("mea-capture-unreaped-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);

        let result = codex_with(
            &host(unreapable(&launcher)),
            &out,
            &std::env::temp_dir(),
            None,
        )
        .await;

        let written: Vec<_> = std::fs::read_dir(&out)
            .expect("expected the fixture directory to exist")
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .collect();
        std::fs::remove_dir_all(&out).expect("expected temporary capture cleanup");
        assert!(
            result.is_err(),
            "expected the capture to fail | received {result:?}"
        );
        assert!(
            written.is_empty(),
            "expected no fixture after an unreaped child | received {written:?}"
        );
    }

    #[test]
    fn review_scenarios_use_the_documented_targets() {
        assert_eq!(
            review_target("review"),
            json!({"type": "uncommittedChanges"})
        );
        assert_eq!(
            review_target("review-base-branch"),
            json!({"type": "baseBranch", "branch": "HEAD"})
        );
        assert_eq!(
            review_target("review-commit"),
            json!({"type": "commit", "sha": "HEAD", "title": "Current commit"})
        );
        assert_eq!(review_target("review-custom")["type"], "custom");
        assert!(
            review_target("review-custom")["instructions"]
                .as_str()
                .is_some_and(|text| !text.is_empty())
        );
    }

    #[test]
    fn user_default_thread_start_omits_permission_members() {
        let params = thread_start_params("/workspace", None, None);

        assert_eq!(params["cwd"], "/workspace");
        assert!(params.get("sandbox").is_none());
        assert!(params.get("approvalPolicy").is_none());
    }

    #[test]
    fn unconfigured_turn_omits_every_permission_member() {
        let params = unconfigured_turn_params("thread-1");

        assert_eq!(params["threadId"], "thread-1");
        for member in [
            "approvalPolicy",
            "sandbox",
            "sandboxPolicy",
            "approvalsReviewer",
        ] {
            assert!(params.get(member).is_none(), "{member} must be absent");
        }
    }
}
