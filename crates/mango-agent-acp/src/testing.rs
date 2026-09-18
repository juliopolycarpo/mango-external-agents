//! A fake ACP agent, for proving the harness without an agent installed.
//!
//! [`FakeAcpAgent`] answers the v1 wire from a script rather than from a model: it handles
//! `initialize`, `session/new`, `session/load`, `session/list`, `session/set_mode`, `session/cancel`
//! and `session/prompt`, and a prompt streams whatever updates it was built with. Everything it
//! writes is JSON it composes itself, so a rename in the schema crate shows up as a test that stops
//! matching rather than as a test that compiles against a wire nobody speaks.
//!
//! It is **not** a fixture. Captured contracts belong under `fixtures/` and are produced by
//! `mea capture` against a real agent; this is a named fake, which is what the repository's own rule
//! asks for in place of inline stubs.

use std::sync::{Arc, Mutex, PoisonError};

use mango_external_agents::testing::FakeProcess;

/// What the fake does when a turn asks for permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Approval {
    /// Never ask.
    Never,
    /// Ask once per turn, and wait for the client's answer before finishing.
    Once,
    /// Ask once per turn, offering only choices that allow — so a client with nothing to refuse with
    /// has to put the question to a person.
    OnlyAllows,
    /// Ask once per turn, offering a standing refusal and no one-time one.
    ///
    /// What an agent that only knows "reject always" looks like. A client that recognises only
    /// `reject_once` reads this option set as "nothing here refuses" and has to reach for something
    /// heavier than an answer.
    OnlyStandingRefusal,
    /// Ask once per turn and end the turn anyway, without waiting for the answer.
    ///
    /// What a misbehaving agent does, and what a well-behaved one looks like from the client's side
    /// when a cancel lands between the question and the answer. It is the only way to reach a state
    /// where a question outlives the turn it belongs to.
    WithoutWaiting,
}

/// A scripted ACP agent, as a [`FakeProcess`] the core's `FakeLauncher` can hand out.
///
/// # Example
///
/// ```
/// use mango_agent_acp::testing::FakeAcpAgent;
/// use mango_external_agents::testing::FakeLauncher;
///
/// let launcher = FakeLauncher::new();
/// launcher.push(FakeAcpAgent::new().process());
/// ```
#[derive(Clone, Debug)]
pub struct FakeAcpAgent {
    protocol_version: u16,
    load_session: bool,
    supports_listing: bool,
    supports_close: bool,
    modes: Vec<String>,
    approval: Approval,
    /// `session/new` answers with this error code instead of a session.
    new_session_error: Option<(i32, String)>,
    /// `session/load` answers with this error code instead of a loaded session, while the agent
    /// still advertises the capability.
    load_session_error: Option<(i32, String)>,
    /// Streams a turn's updates and never answers its `session/prompt`.
    never_finishes: bool,
    /// Raises a permission request when the client sends `session/close`.
    asks_when_closing: bool,
    /// Once the first `session/request_permission` is answered, raises a second one reusing the
    /// same JSON-RPC id instead of ending the turn.
    ///
    /// What a protocol-violating peer looks like: the JSON-RPC spec only asks that an id stay
    /// unique among a peer's *outstanding* requests, so a peer that reuses one the moment its first
    /// use is settled is within its rights. `agent-client-protocol`'s own
    /// `RequestCancellationRegistry` test documents exactly this peer as one it dispatches rather
    /// than refuses.
    reuse_request_id_for_second_ask: bool,
    /// The complete session configuration catalog returned by lifecycle and set-option calls.
    config_options: Option<Vec<serde_json::Value>>,
    updates: Vec<serde_json::Value>,
    stop_reason: String,
    version_output: String,
}

impl Default for FakeAcpAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeAcpAgent {
    /// An agent that opens a session and answers one turn with text, an activity and a completion.
    #[must_use]
    pub fn new() -> Self {
        Self {
            protocol_version: 1,
            load_session: true,
            supports_listing: false,
            supports_close: false,
            modes: Vec::new(),
            approval: Approval::Never,
            new_session_error: None,
            load_session_error: None,
            never_finishes: false,
            asks_when_closing: false,
            reuse_request_id_for_second_ask: false,
            config_options: None,
            updates: vec![
                serde_json::json!({
                    "sessionUpdate": "available_commands_update",
                    "availableCommands": [{ "name": "review", "description": "Review the diff" }]
                }),
                serde_json::json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": { "type": "text", "text": "thinking" }
                }),
                serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": { "type": "text", "text": "hello" }
                }),
                serde_json::json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": "call_1",
                    "title": "Run `cargo test`",
                    "kind": "execute",
                    "status": "completed"
                }),
                serde_json::json!({
                    "sessionUpdate": "usage_update",
                    "used": 1200,
                    "size": 200_000
                }),
            ],
            stop_reason: String::from("end_turn"),
            version_output: String::from("fake-acp 1.2.3"),
        }
    }

    /// Asks for one approval per turn.
    #[must_use]
    pub fn asking_for_approval(mut self, approval: Approval) -> Self {
        self.approval = approval;
        self
    }

    /// Answers `initialize` with this protocol version.
    #[must_use]
    pub fn with_protocol_version(mut self, version: u16) -> Self {
        self.protocol_version = version;
        self
    }

    /// Streams a turn's updates and never answers its `session/prompt`.
    ///
    /// The only way to hold a prompt in flight with nothing else outstanding, which is what a host
    /// dropping its `TurnStream` mid-turn has to be tested against: the turn slot belongs to the
    /// prompt, and a closed sink must not release it.
    #[must_use]
    pub fn never_finishing_turns(mut self) -> Self {
        self.never_finishes = true;
        self
    }

    /// Asks once per turn and ends the turn anyway, without waiting for the answer.
    #[must_use]
    pub fn asking_without_waiting(self) -> Self {
        self.asking_for_approval(Approval::WithoutWaiting)
    }

    /// Advertises `session/list`.
    #[must_use]
    pub fn listing_sessions(mut self) -> Self {
        self.supports_listing = true;
        self
    }

    /// Raises one `session/request_permission` when the client sends `session/close`.
    ///
    /// The window a `close` awaits in, and the only way to reach it: nothing may grant a permission
    /// while the session is being torn down.
    #[must_use]
    pub fn asking_when_closing(mut self) -> Self {
        self.asks_when_closing = true;
        self
    }

    /// Once the first `session/request_permission` of a turn is answered, raises a second one on
    /// the same JSON-RPC id rather than ending the turn.
    #[must_use]
    pub fn reusing_the_request_id_on_a_second_ask(mut self) -> Self {
        self.reuse_request_id_for_second_ask = true;
        self
    }

    /// Advertises `session/close`.
    #[must_use]
    pub fn closing_sessions(mut self) -> Self {
        self.supports_close = true;
        self
    }

    /// Advertises these mode ids on `session/new`.
    #[must_use]
    pub fn with_modes<I, S>(mut self, modes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.modes = modes.into_iter().map(Into::into).collect();
        self
    }

    /// Does not advertise `session/load`.
    #[must_use]
    pub fn without_load_session(mut self) -> Self {
        self.load_session = false;
        self
    }

    /// Answers `session/new` with this JSON-RPC error.
    #[must_use]
    pub fn refusing_new_session(mut self, code: i32, message: impl Into<String>) -> Self {
        self.new_session_error = Some((code, message.into()));
        self
    }

    /// Advertises `session/load` and then refuses the call with this JSON-RPC error.
    ///
    /// Distinct from [`without_load_session`](Self::without_load_session): an agent that never
    /// declared the capability is a static refusal, while one that declared it and then failed
    /// the call is the only way to reach the resume fallback a
    /// [`ResumeMode::Fallback`](mango_external_agents::ResumeMode) request asks for.
    #[must_use]
    pub fn refusing_load_session(mut self, code: i32, message: impl Into<String>) -> Self {
        self.load_session_error = Some((code, message.into()));
        self
    }

    /// Streams these `session/update` payloads for a turn instead of the default script.
    #[must_use]
    pub fn with_updates(mut self, updates: Vec<serde_json::Value>) -> Self {
        self.updates = updates;
        self
    }

    /// Returns these v1 session configuration options when a session opens.
    #[must_use]
    pub fn with_config_options(mut self, config_options: Vec<serde_json::Value>) -> Self {
        self.config_options = Some(config_options);
        self
    }

    /// Ends its turns with this stop reason.
    #[must_use]
    pub fn with_stop_reason(mut self, stop_reason: impl Into<String>) -> Self {
        self.stop_reason = stop_reason.into();
        self
    }

    /// Prints this for a version probe.
    #[must_use]
    pub fn printing_version(mut self, output: impl Into<String>) -> Self {
        self.version_output = output.into();
        self
    }

    /// A child that prints this agent's version output and exits, for a probe.
    #[must_use]
    pub fn version_process(&self) -> FakeProcess {
        FakeProcess::transcript([self.version_output.clone()])
    }

    /// A child that speaks the v1 wire for as long as it is driven.
    #[must_use]
    pub fn process(&self) -> FakeProcess {
        let agent = self.clone();
        let pending = Arc::new(Mutex::new(PendingTurn::default()));
        let config_options = Arc::new(Mutex::new(agent.config_options.clone()));
        FakeProcess::responding(move |line| agent.answer(line, &pending, &config_options))
    }

    fn answer(
        &self,
        line: &str,
        pending: &Arc<Mutex<PendingTurn>>,
        config_options: &Arc<Mutex<Option<Vec<serde_json::Value>>>>,
    ) -> Vec<String> {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            return Vec::new();
        };
        let method = message.get("method").and_then(serde_json::Value::as_str);
        let id = message.get("id").cloned();

        match (method, id) {
            (Some("initialize"), Some(id)) => vec![result(id, self.initialize_result())],
            (Some("session/new"), Some(id)) => vec![self.session_result(id, config_options)],
            (Some("session/load"), Some(id)) => match (self.load_session, &self.load_session_error)
            {
                (true, Some((code, message))) => vec![error(id, *code, message)],
                (true, None) => vec![self.load_session_result(id, config_options)],
                (false, _) => vec![error(id, -32601, "method not found")],
            },
            (Some("session/list"), Some(id)) => vec![result(
                id,
                serde_json::json!({
                    "sessions": [{ "sessionId": "sess_old", "cwd": "/repo", "title": "Yesterday" }]
                }),
            )],
            (Some("session/set_mode"), Some(id)) => vec![result(id, serde_json::json!({}))],
            (Some("session/set_config_option"), Some(id)) => {
                vec![result(
                    id,
                    self.set_config_option_result(&message, config_options),
                )]
            }
            (Some("session/close"), Some(id)) => {
                let mut lines = Vec::new();
                if self.asks_when_closing {
                    let request_id = pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .next_request_id();
                    lines.push(request(
                        request_id,
                        "session/request_permission",
                        self.permission_params(),
                    ));
                }
                lines.push(result(id, serde_json::json!({})));
                lines
            }
            (Some("session/prompt"), Some(id)) => self.prompt(id, pending),
            (Some("session/cancel"), None) => self.cancelled(pending),
            // An unanswered request would hang the client, and an unknown one is the agent's own
            // "method not found" rather than silence.
            (Some(_), Some(id)) => vec![error(id, -32601, "method not found")],
            // A response to our own `session/request_permission`.
            (None, Some(_)) => self.answered(&message, pending),
            _ => Vec::new(),
        }
    }

    fn initialize_result(&self) -> serde_json::Value {
        let mut session_capabilities = serde_json::Map::new();
        if self.supports_listing {
            session_capabilities.insert(String::from("list"), serde_json::json!({}));
        }
        if self.supports_close {
            session_capabilities.insert(String::from("close"), serde_json::json!({}));
        }
        serde_json::json!({
            "protocolVersion": self.protocol_version,
            "agentInfo": { "name": "fake-acp", "version": "1.2.3" },
            "agentCapabilities": {
                "loadSession": self.load_session,
                "promptCapabilities": { "image": true, "embeddedContext": true },
                "sessionCapabilities": session_capabilities,
            },
            "authMethods": [],
        })
    }

    fn session_result(
        &self,
        id: serde_json::Value,
        config_options: &Arc<Mutex<Option<Vec<serde_json::Value>>>>,
    ) -> String {
        match &self.new_session_error {
            Some((code, message)) => error(id, *code, message),
            None => {
                let mut response = serde_json::json!({
                    "sessionId": "sess_fake",
                    "modes": self.mode_state(),
                });
                if let Some(config_options) = config_options
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .as_ref()
                {
                    response["configOptions"] = serde_json::Value::Array(config_options.clone());
                }
                result(id, response)
            }
        }
    }

    fn set_config_option_result(
        &self,
        request: &serde_json::Value,
        config_options: &Arc<Mutex<Option<Vec<serde_json::Value>>>>,
    ) -> serde_json::Value {
        let mut state = config_options
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut updated = state.clone().unwrap_or_default();
        let params = request.get("params").unwrap_or(request);
        let Some(config_id) = params.get("configId").and_then(serde_json::Value::as_str) else {
            return serde_json::json!({ "configOptions": updated });
        };
        let value = match params.get("type").and_then(serde_json::Value::as_str) {
            Some("boolean") => params.get("value").cloned(),
            _ => params.get("value").cloned(),
        };
        let Some(value) = value else {
            return serde_json::json!({ "configOptions": updated });
        };
        for option in &mut updated {
            if option.get("id").and_then(serde_json::Value::as_str) == Some(config_id) {
                option["currentValue"] = value.clone();
            }
        }
        *state = Some(updated.clone());
        serde_json::json!({ "configOptions": updated })
    }

    fn load_session_result(
        &self,
        id: serde_json::Value,
        config_options: &Arc<Mutex<Option<Vec<serde_json::Value>>>>,
    ) -> String {
        let mut response = serde_json::json!({ "modes": self.mode_state() });
        if let Some(config_options) = config_options
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            response["configOptions"] = serde_json::Value::Array(config_options.clone());
        }
        result(id, response)
    }

    fn mode_state(&self) -> Option<serde_json::Value> {
        let first = self.modes.first()?;
        Some(serde_json::json!({
            "currentModeId": first,
            "availableModes": self
                .modes
                .iter()
                .map(|mode| serde_json::json!({ "id": mode, "name": mode }))
                .collect::<Vec<_>>(),
        }))
    }

    /// The updates for one turn, then either a permission request or the turn's end.
    fn prompt(&self, id: serde_json::Value, pending: &Arc<Mutex<PendingTurn>>) -> Vec<String> {
        let mut lines: Vec<String> = self
            .updates
            .iter()
            .map(|update| {
                notification(
                    "session/update",
                    serde_json::json!({ "sessionId": "sess_fake", "update": update }),
                )
            })
            .collect();

        if self.never_finishes {
            return lines;
        }

        if self.approval == Approval::Never {
            lines.push(result(
                id,
                serde_json::json!({ "stopReason": self.stop_reason }),
            ));
            return lines;
        }

        if self.approval == Approval::WithoutWaiting {
            let request_id = pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .next_request_id();
            lines.push(request(
                request_id,
                "session/request_permission",
                self.permission_params(),
            ));
            lines.push(result(
                id,
                serde_json::json!({ "stopReason": self.stop_reason }),
            ));
            return lines;
        }

        // The turn is held open: its response goes out when the client answers, which is what proves
        // the round trip rather than a request nobody replies to.
        let request_id = pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .open(id);
        lines.push(request(
            request_id,
            "session/request_permission",
            self.permission_params(),
        ));
        lines
    }

    fn permission_params(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionId": "sess_fake",
            "toolCall": { "toolCallId": "call_1", "kind": "execute", "title": "Run `rm -rf build`" },
            "options": self.options(),
        })
    }

    fn options(&self) -> serde_json::Value {
        match self.approval {
            Approval::OnlyAllows => serde_json::json!([
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "allow-all", "name": "Always", "kind": "allow_always" },
            ]),
            Approval::OnlyStandingRefusal => serde_json::json!([
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject-all", "name": "Never", "kind": "reject_always" },
            ]),
            _ => serde_json::json!([
                { "optionId": "allow", "name": "Allow", "kind": "allow_once" },
                { "optionId": "reject", "name": "Reject", "kind": "reject_once" },
            ]),
        }
    }

    /// The client answered this fake's `session/request_permission`, so its turn can finish.
    ///
    /// A `cancelled` outcome ends the turn as cancelled, which is what a real agent does: ACP requires
    /// a client sending `session/cancel` to withdraw every pending question that way, so treating a
    /// withdrawal as an ordinary answer would have this fake report `end_turn` for a turn somebody
    /// stopped — and a harness that got the reason wrong would look correct against it.
    fn answered(
        &self,
        message: &serde_json::Value,
        pending: &Arc<Mutex<PendingTurn>>,
    ) -> Vec<String> {
        // `/result/outcome/outcome`, not `/result/outcome`: `RequestPermissionResponse` carries the
        // outcome as a field and the enum is internally tagged `outcome`, so the tag sits one level in.
        let withdrawn = message
            .pointer("/result/outcome/outcome")
            .and_then(serde_json::Value::as_str)
            == Some("cancelled");

        if self.reuse_request_id_for_second_ask && !withdrawn {
            let mut guard = pending.lock().unwrap_or_else(PoisonError::into_inner);
            if guard.asks == 1 {
                let request_id = guard.reopen_with_the_same_id();
                drop(guard);
                return vec![request(
                    request_id,
                    "session/request_permission",
                    self.permission_params(),
                )];
            }
        }

        let turn = pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .close();
        let Some(turn) = turn else {
            return Vec::new();
        };
        let stop_reason = match withdrawn {
            true => "cancelled",
            false => self.stop_reason.as_str(),
        };
        vec![result(
            turn,
            serde_json::json!({ "stopReason": stop_reason }),
        )]
    }

    /// A cancelled turn answers `stop_reason: cancelled`, which is what ACP says it does.
    ///
    /// But only once nothing is outstanding. While a `session/request_permission` is unanswered this
    /// fake is, like a real agent, still inside the tool call that raised it — so the prompt response
    /// waits for the client to withdraw the question, which ACP requires it to do.
    fn cancelled(&self, pending: &Arc<Mutex<PendingTurn>>) -> Vec<String> {
        let turn = pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .close_unless_asking();
        let Some(turn) = turn else {
            return Vec::new();
        };
        vec![result(
            turn,
            serde_json::json!({ "stopReason": "cancelled" }),
        )]
    }
}

/// The one `session/prompt` this fake keeps open while it waits on a permission answer.
#[derive(Debug, Default)]
struct PendingTurn {
    prompt_id: Option<serde_json::Value>,
    /// Whether a `session/request_permission` is outstanding.
    ///
    /// Load-bearing for `session/cancel`. A real agent awaits the permission answer *inside* its tool
    /// call, so it cannot answer `session/prompt` until that await returns — which is exactly why ACP
    /// requires a cancelling client to withdraw the question. A fake that answered the prompt on
    /// `session/cancel` regardless would let a harness that never withdrew look correct.
    question_open: bool,
    next_request_id: i64,
    /// The id of the most recent `session/request_permission`, so a reused-id ask can repeat it.
    last_request_id: Option<i64>,
    /// How many `session/request_permission` this turn has raised.
    asks: u32,
}

impl PendingTurn {
    fn open(&mut self, prompt_id: serde_json::Value) -> i64 {
        self.prompt_id = Some(prompt_id);
        self.question_open = true;
        self.asks += 1;
        let id = self.next_request_id();
        self.last_request_id = Some(id);
        id
    }

    /// Raises a second question in the same turn, on the same JSON-RPC id the first one used.
    fn reopen_with_the_same_id(&mut self) -> i64 {
        self.question_open = true;
        self.asks += 1;
        self.last_request_id
            .expect("expected a first ask before a second reuses its id")
    }

    /// The next id for a request this fake sends.
    ///
    /// Numbered above anything the client sends, so the two id spaces never collide in a transcript
    /// somebody is reading.
    fn next_request_id(&mut self) -> i64 {
        self.next_request_id += 1;
        9_000 + self.next_request_id
    }

    /// Ends the turn, whatever else is outstanding.
    fn close(&mut self) -> Option<serde_json::Value> {
        self.question_open = false;
        self.prompt_id.take()
    }

    /// Ends the turn only if no question is outstanding, as a real agent's tool call would.
    fn close_unless_asking(&mut self) -> Option<serde_json::Value> {
        if self.question_open {
            return None;
        }
        self.prompt_id.take()
    }
}

fn result(id: serde_json::Value, result: serde_json::Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error(id: serde_json::Value, code: i32, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
    .to_string()
}

fn notification(method: &str, params: serde_json::Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()
}

fn request(id: i64, method: &str, params: serde_json::Value) -> String {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
        .to_string()
}
