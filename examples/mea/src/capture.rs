//! `mea capture`: records a real vendor conversation as a fixture the fakes replay.
//!
//! Every line the library writes and every line the vendor answers with, in order, redacted and
//! written as JSONL. The scenarios are scripted here rather than typed at a prompt, so re-running
//! the command against a newer build produces a diff against the same conversation rather than a
//! different one.
//!
//! This is the only way a fixture is made. Hand-writing one produces a transcript that agrees with
//! whatever the author believed, which is exactly the belief a fixture exists to check.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::HostContext;
use mango_external_agents::error::{Error, Result};
use mango_external_agents::launcher::TokioLauncher;
use mango_external_agents::link::{LinkReceiver, LinkSender};
use mango_external_agents::transport::{ExecutablePath, StdioSpec};
use mango_external_agents::transports::stdio;
use serde_json::{Value, json};

use crate::redact;

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
];

/// Records every Codex scenario into `out_dir`.
///
/// # Errors
///
/// Whatever the launcher or the vendor reported. A scenario that fails leaves the fixtures written
/// so far in place: a partial capture is a diff to read, not a reason to lose the rest.
pub async fn codex(out_dir: &Path, workspace: &Path) -> Result<()> {
    std::fs::create_dir_all(out_dir).map_err(|error| Error::HostConfiguration {
        expected: "a writable fixture directory",
        received: error.to_string(),
    })?;

    for scenario in SCENARIOS {
        println!("capturing codex/{}: {}", scenario.name, scenario.purpose);
        let recorded = record(scenario, workspace).await?;
        let path = out_dir.join(format!("{}.jsonl", scenario.name));
        std::fs::write(&path, recorded).map_err(|error| Error::HostConfiguration {
            expected: "a writable fixture file",
            received: error.to_string(),
        })?;
        println!("  wrote {}", path.display());
    }
    Ok(())
}

/// One scenario, as the lines both sides wrote.
async fn record(scenario: &Scenario, workspace: &Path) -> Result<String> {
    let host = HostContext::builder()
        .launcher(Arc::new(TokioLauncher::new()))
        .cwd(workspace.to_path_buf())
        .client_info("mea", env!("CARGO_PKG_VERSION"))
        .environment(mango_external_agents::EnvSource::from_process())
        .build()?;

    let transport = stdio::open(
        &host,
        &StdioSpec::new(["codex", "app-server"]),
        &ExecutablePath::default(),
        &["CODEX_HOME"],
    )
    .await?;
    let (sender, receiver) = transport.link.split();
    let mut recorder = Recorder {
        sender,
        receiver,
        cwd: workspace.to_string_lossy().into_owned(),
        home: home_directory(),
        lines: vec![format!("# codex/{}: {}", scenario.name, scenario.purpose)],
    };

    let outcome = recorder.run(scenario).await;
    let _ = transport
        .control
        .kill(mango_external_agents::CancelReason::Shutdown)
        .await;
    outcome?;
    Ok(recorder.lines.join("\n") + "\n")
}

/// A conversation being written down as it happens.
struct Recorder {
    sender: Box<dyn LinkSender>,
    receiver: Box<dyn LinkReceiver>,
    cwd: String,
    home: String,
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

    async fn start_thread(&mut self, sandbox: &str, approval_policy: &str) -> Result<String> {
        self.send(json!({"id": 5, "method": "thread/start", "params": {
            "cwd": self.cwd, "sandbox": sandbox, "approvalPolicy": approval_policy
        }}))
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
        self.lines
            .push(format!("{SENT}{}", redact::frame(frame, self.paths())));
        self.sender.send(line).await
    }

    fn paths(&self) -> redact::Paths<'_> {
        redact::Paths {
            cwd: &self.cwd,
            home: &self.home,
        }
    }

    /// Reads and records until a frame matches, or the step's deadline passes.
    async fn read_until(&mut self, matches: impl Fn(&Value) -> bool) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
        loop {
            let next = tokio::time::timeout_at(deadline, self.receiver.recv())
                .await
                .map_err(|_| Error::Timeout {
                    operation: String::from("a captured step"),
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
                redact::frame(frame.clone(), self.paths())
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

fn is_turn_completed(frame: &Value) -> bool {
    frame.get("method") == Some(&json!("turn/completed"))
}

/// Where fixtures live, relative to the repository root.
#[must_use]
pub fn default_out_dir() -> PathBuf {
    PathBuf::from("fixtures/codex")
}
