//! A fake `codex app-server` that replays what a real one said.
//!
//! The fixtures under `fixtures/codex/` were recorded by `mea capture` against a real binary, so
//! what these tests assert against is a conversation that actually happened rather than one whose
//! author already believed the harness was right.
//!
//! The one thing the replay cannot take literally is a JSON-RPC id: the harness numbers its own
//! calls, and a recorded answer carries the number the recorder used. So a response is re-addressed
//! to whatever id the call arrived under. Windows also retargets the `/workspace` fixture
//! placeholder to a drive-qualified test path; every other value stays byte for byte.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use mango_external_agents::testing::FakeProcess;
use serde_json::Value;

/// The prefix `mea capture` writes on a line the library sent.
const SENT: &str = ">>";
/// The prefix it writes on a line the vendor sent.
const RECEIVED: &str = "<<";

/// The host directory replayed Codex receives on this platform.
///
/// Captures use `/workspace`, which is a full path on Unix. Windows needs a drive-qualified path
/// for the same host contract, so the replay retargets only that fixture placeholder there.
pub fn workspace_path() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::temp_dir().join("mango-agent-codex-replay-workspace")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/workspace")
    }
}

#[cfg(windows)]
fn retarget_fixture_workspace(value: &mut Value) {
    match value {
        Value::String(text) if text == "/workspace" => {
            *text = workspace_path().to_string_lossy().into_owned();
        }
        Value::Array(values) => {
            for value in values {
                retarget_fixture_workspace(value);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                retarget_fixture_workspace(value);
            }
        }
        _ => {}
    }
}

/// One call, and everything the server wrote before the next call.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// The frame the library wrote.
    pub sent: Value,
    /// What the server wrote back, in order, before the next thing the library wrote.
    pub received: Vec<Value>,
}

impl Step {
    /// The JSON-RPC method this step's call used, when it had one.
    pub fn method(&self) -> Option<&str> {
        self.sent.get("method").and_then(Value::as_str)
    }
}

/// A recorded conversation, as steps.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Transcript {
    /// Every call and its answers, in the order they happened.
    pub steps: Vec<Step>,
    /// Anything the server wrote before the library said anything.
    pub greeting: Vec<Value>,
}

impl Transcript {
    /// Reads one `mea capture` fixture.
    ///
    /// # Panics
    ///
    /// When the fixture is missing or a line is not the JSONL the recorder writes. Both are a
    /// broken fixture rather than a failing assertion, and a test that limped on would be
    /// asserting against a transcript nobody recorded.
    pub fn load(name: &str) -> Self {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/codex")
            .join(format!("{name}.jsonl"));
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "expected the captured fixture at {}, received {error}; run `mea capture codex`",
                path.display()
            )
        });

        let mut transcript = Self::default();
        for line in raw.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (prefix, body) = line.split_at(2);
            let frame: Value = serde_json::from_str(body).unwrap_or_else(|error| {
                panic!(
                    "expected a JSON frame in {}, received {error}",
                    path.display()
                )
            });
            #[cfg(windows)]
            let frame = {
                let mut frame = frame;
                retarget_fixture_workspace(&mut frame);
                frame
            };
            match prefix {
                SENT => transcript.steps.push(Step {
                    sent: frame,
                    received: Vec::new(),
                }),
                RECEIVED => match transcript.steps.last_mut() {
                    Some(step) => step.received.push(frame),
                    None => transcript.greeting.push(frame),
                },
                other => panic!("expected {SENT} or {RECEIVED}, received {other:?}"),
            }
        }
        transcript
    }

    /// Everything the server wrote, in order, whichever step it belonged to.
    pub fn every_received(&self) -> Vec<&Value> {
        self.greeting
            .iter()
            .chain(self.steps.iter().flat_map(|step| step.received.iter()))
            .collect()
    }

    /// The thread the recorded conversation ran on.
    ///
    /// The harness adopts whatever `thread/start` answered with, so a test that wants to address
    /// this conversation has to read it out of the fixture rather than invent one.
    pub fn thread_id(&self) -> Option<String> {
        self.every_received()
            .into_iter()
            .find_map(|frame| frame.pointer("/result/thread/id").and_then(Value::as_str))
            .map(str::to_owned)
    }

    /// Replays this transcript over the real process's own stdin and stdout, then outlives EOF.
    ///
    /// For a re-exec'd copy of a test binary acting as a real `codex app-server` child: it answers
    /// the same way [`Self::as_process`] does, and after stdin ends it sleeps so only a kill can end
    /// it. The re-exec runs libtest with `--quiet`, whose only preamble is a complete
    /// `running 1 test` line that the JSON-RPC reader skips as a non-frame.
    ///
    /// ```ignore
    /// Transcript::load("turn").serve_stdio_until_killed();
    /// ```
    #[cfg(target_os = "linux")]
    pub fn serve_stdio_until_killed(&self) -> ! {
        use std::io::{BufRead as _, Write as _};
        let mut replay = Replay {
            remaining: self.steps.iter().cloned().collect(),
        };
        let mut stdout = std::io::stdout();
        for frame in &self.greeting {
            let _ = writeln!(stdout, "{frame}");
        }
        let _ = stdout.flush();
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            for answer in replay.answer(&line) {
                let _ = writeln!(stdout, "{answer}");
            }
            let _ = stdout.flush();
        }
        loop {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }

    /// The replay as a function: one frame the library wrote in, the lines the server answers with
    /// out.
    ///
    /// The same matching [`Self::as_process`] uses, for a peer that is not a child process, such
    /// as a fake app-server listening on a socket. The greeting is not part of it: the caller
    /// sends [`Self::greeting`] when its transport has a moment to.
    pub fn responder(&self) -> impl FnMut(&str) -> Vec<String> + Send + 'static {
        let mut replay = Replay {
            remaining: self.steps.iter().cloned().collect(),
        };
        move |line| replay.answer(line)
    }

    /// A fake child that replays this transcript.
    ///
    /// Matched by method rather than by position, so a harness that asks the same questions in a
    /// different order still gets the right answers — and one that asks a question the recording
    /// does not contain gets nothing, which shows up as a call that times out rather than as an
    /// answer to a different question.
    pub fn as_process(&self) -> FakeProcess {
        self.as_process_intercepting(|_| None)
    }

    /// The same replay, with some calls answered by `intercept` instead of by the recording.
    ///
    /// For the answers no recording holds: a turn the server refuses, a response missing a member
    /// the harness has to cope without. Writing those into a fixture would put words in the
    /// app-server's mouth, which is exactly what capturing fixtures exists to avoid — so they live
    /// in the test that needs them, next to the assertion that says why.
    ///
    /// `intercept` sees the frame the library wrote and returns the lines to answer it with, or
    /// `None` to leave the call to the recording.
    pub fn as_process_intercepting(
        &self,
        intercept: impl Fn(&Value) -> Option<Vec<String>> + Send + Sync + 'static,
    ) -> FakeProcess {
        let script = Arc::new(Mutex::new(self.responder()));
        FakeProcess::responding(move |line| {
            if let Ok(frame) = serde_json::from_str::<Value>(line)
                && let Some(answers) = intercept(&frame)
            {
                return answers;
            }
            (script.lock().unwrap_or_else(PoisonError::into_inner))(line)
        })
        .with_greeting(self.greeting.iter().map(Value::to_string))
    }
}

/// The replay's own position in the recorded conversation.
struct Replay {
    remaining: VecDeque<Step>,
}

impl Replay {
    fn answer(&mut self, line: &str) -> Vec<String> {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };

        // Synthetic current-protocol preflight for both fake and re-exec'd children,
        // never inserted into an archival recording. The caller's interceptor runs first.
        if frame["method"] == "account/gatewayOAuth/read" {
            return vec![
                serde_json::json!({"id": frame["id"], "result": {
                    "required": false, "status": null
                }})
                .to_string(),
            ];
        }

        // An answer to one of the server's own questions — an approval. The recording already
        // holds whatever followed it, under the call that was in flight at the time.
        let Some(method) = frame.get("method").and_then(Value::as_str) else {
            return self.drain_pending();
        };

        let at = self
            .remaining
            .iter()
            .position(|step| step.method() == Some(method));
        let Some(at) = at else {
            return Vec::new();
        };
        // Everything before the match is a call this harness did not make. Dropped rather than
        // replayed, so a recording of a richer conversation still serves a narrower one.
        let step = self.remaining.drain(..=at).next_back().unwrap_or(Step {
            sent: Value::Null,
            received: Vec::new(),
        });
        let id = frame.get("id").cloned();
        step.received
            .iter()
            .map(|answer| readdress(answer, id.as_ref()).to_string())
            .collect()
    }

    /// What the server wrote after the client answered one of its questions.
    ///
    /// The recorder wrote that as the next step, whose `sent` frame is the answer itself.
    fn drain_pending(&mut self) -> Vec<String> {
        let at = self
            .remaining
            .iter()
            .position(|step| step.sent.get("method").is_none());
        let Some(at) = at else {
            return Vec::new();
        };
        let step = self.remaining.drain(..=at).next_back().unwrap_or(Step {
            sent: Value::Null,
            received: Vec::new(),
        });
        step.received.iter().map(Value::to_string).collect()
    }
}

/// A recorded answer, re-addressed to the id the live call actually used.
///
/// Only a response is re-addressed: a notification has no id, and a question the *server* asks
/// carries an id of its own that the harness has to echo back untouched.
fn readdress(frame: &Value, id: Option<&Value>) -> Value {
    let (Some(id), Value::Object(members)) = (id, frame) else {
        return frame.clone();
    };
    if members.contains_key("method") || !members.contains_key("id") {
        return frame.clone();
    }
    let mut readdressed = members.clone();
    readdressed.insert(String::from("id"), id.clone());
    Value::Object(readdressed)
}

// All-features tests exercise the public export. Default-feature tests reuse the exact same
// implementation without turning an optional test API into a production default.
#[cfg(not(feature = "testing"))]
use mango_agent_codex::MINIMUM_CODEX_VERSION;
#[cfg(feature = "testing")]
pub use mango_agent_codex::testing::VersionAnsweringLauncher;
#[cfg(not(feature = "testing"))]
#[path = "../../src/testing_launcher.rs"]
#[allow(dead_code)]
mod version_answering_launcher;
#[cfg(not(feature = "testing"))]
pub use version_answering_launcher::VersionAnsweringLauncher;

#[cfg(test)]
mod tests {
    use super::{Step, Transcript, readdress, workspace_path};
    use serde_json::json;

    /// Another agent's version probe must keep its own fake process and launch authority.
    #[tokio::test]
    async fn a_non_codex_version_probe_reaches_the_inner_launcher() {
        use std::collections::BTreeMap;
        use std::sync::Arc;

        use mango_external_agents::process::{LaunchSpec, ProcessLauncher};
        use mango_external_agents::testing::{FakeLauncher, FakeProcess};

        for program in ["claude", "acp-agent"] {
            let inner = Arc::new(FakeLauncher::new());
            inner.push(FakeProcess::transcript(["native version banner"]));
            let spec = LaunchSpec {
                argv: vec![program.to_owned(), String::from("--version")],
                cwd: workspace_path(),
                env: BTreeMap::from([(String::from("TEST_SETTING"), String::from("kept"))]),
                stdin: false,
                hide_window: true,
            };
            let mut process = super::VersionAnsweringLauncher::over(inner.clone())
                .spawn(spec.clone())
                .await
                .expect("expected a fake version process");
            while process.stdout.next_chunk().await.expect("stdout").is_some() {}
            assert_eq!(
                inner.last_launch(),
                Some(spec),
                "expected another program's version probe to reach the inner launcher"
            );
        }
    }

    #[test]
    fn replay_workspace_is_a_native_absolute_path() {
        let workspace = workspace_path();
        assert!(
            workspace.is_absolute(),
            "expected {workspace:?} to be absolute"
        );
        assert!(
            workspace.to_str().is_some(),
            "expected a UTF-8 test workspace"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_replay_retargets_only_the_workspace_placeholder() {
        let mut frame = json!({
            "cwd": "/workspace",
            "unrelated": "/private",
        });
        super::retarget_fixture_workspace(&mut frame);
        assert_eq!(
            frame["cwd"],
            workspace_path().to_string_lossy().into_owned()
        );
        assert_eq!(frame["unrelated"], "/private");
    }

    #[test]
    fn a_response_is_readdressed_to_the_call_that_is_waiting() {
        let recorded = json!({"id": 0, "result": {"thread": {"id": "t"}}});
        assert_eq!(
            readdress(&recorded, Some(&json!("7"))),
            json!({"id": "7", "result": {"thread": {"id": "t"}}})
        );
    }

    /// A notification has nobody waiting on it, and a question the server asks carries an id the
    /// harness must echo back. Re-addressing either would break the conversation.
    #[test]
    fn a_notification_and_a_question_keep_the_ids_they_were_recorded_with() {
        let notification = json!({"method": "turn/completed", "params": {}});
        assert_eq!(readdress(&notification, Some(&json!("7"))), notification);

        let question = json!({"id": 0, "method": "item/commandExecution/requestApproval",
                              "params": {}});
        assert_eq!(readdress(&question, Some(&json!("7"))), question);
    }

    #[test]
    fn a_step_names_the_method_its_call_used() {
        let step = Step {
            sent: json!({"id": 1, "method": "thread/start", "params": {}}),
            received: Vec::new(),
        };
        assert_eq!(step.method(), Some("thread/start"));
    }

    /// The responder matches by method, answers under the id the call arrived with, and has
    /// nothing for a method the recording does not contain.
    #[test]
    fn the_responder_answers_by_method_under_the_callers_id() {
        let transcript = Transcript {
            steps: vec![Step {
                sent: json!({"id": 0, "method": "thread/start", "params": {}}),
                received: vec![json!({"id": 0, "result": {"thread": {"id": "t"}}})],
            }],
            greeting: Vec::new(),
        };
        let mut respond = transcript.responder();

        assert!(
            respond(r#"{"id":4,"method":"turn/start","params":{}}"#).is_empty(),
            "expected nothing for a method the recording does not hold"
        );
        let answers = respond(r#"{"id":9,"method":"thread/start","params":{}}"#);
        assert_eq!(
            answers,
            vec![json!({"id": 9, "result": {"thread": {"id": "t"}}}).to_string()]
        );
    }

    #[test]
    fn an_empty_transcript_names_no_thread() {
        assert_eq!(Transcript::default().thread_id(), None);
    }
}
