//! One Claude conversation, and the process each of its turns owns.
//!
//! The process policy here is the opposite of a persistent app-server's. `claude --print` is a
//! batch invocation that reads a prompt, runs a turn and exits, and continuity comes from a session
//! id on disk rather than from a pipe staying open. So a session owns no process at all: it owns a
//! **session id**, and each turn spawns, streams and reaps its own child.
//!
//! Three consequences worth stating where they are implemented.
//!
//! - **Opening starts nothing.** The id is minted up front — `--session-id <uuid>` is documented
//!   and echoed back — so a host holds a resumable handle before any tokens are spent.
//! - **The prompt travels on stdin.** `--input-format stream-json` is the documented programmatic
//!   input, and it keeps the prompt out of argv.
//! - **A turn ends exactly once.** Who ends it is one transition, recorded once, and
//!   only the pump task emits. A cancel records the reason and kills the child; the pump sees the
//!   stream end and writes the terminal pair. Two tasks racing to emit a terminal is the one defect
//!   a host cannot work around.

use std::sync::{Arc, Mutex, PoisonError};

use mango_external_agents::{
    CancelReason, CloseReason, Configuration, Error, ErrorCode, EventSink, ExecutablePath,
    HostContext, PermissionResponse, Result, SessionInfo, StdioSpec, TurnRequest, TurnStream,
    VendorError, transports::stdio,
};
use serde_json::json;

use crate::argv::TurnArgv;
use crate::cli_surface::CliSurface;
use crate::mcp::ConfigFile;
use crate::permissions::{self, ModeAvailability};
use crate::pinned::{SIGTERM_EXIT_CODE, STREAM_IDLE_TIMEOUT, VENDOR_ENVIRONMENT_KEYS};
use crate::probe::PROGRAM;
use crate::protocol::StreamRecord;
use crate::reducer::{RunInit, TurnReducer};

/// How a run ended, and who decided.
///
/// One transition, taken once. Everything that can stop a turn — a host's `cancel`, a `close`, the
/// host's own shutdown token — records its reason here and stops; the pump reads it after the
/// stream ends and writes the terminal pair. Nothing else emits, so "exactly one terminal" is a
/// property of the code's shape rather than of its timing.
#[derive(Debug, Default)]
struct TurnEnd {
    reason: Mutex<Option<CancelReason>>,
}

impl TurnEnd {
    /// Records why this turn is stopping, if nothing recorded one first.
    ///
    /// Answers whether this caller was the first, so a close racing a cancel does not kill the
    /// child twice or report two reasons.
    fn record(&self, reason: CancelReason) -> bool {
        let mut recorded = self.reason.lock().unwrap_or_else(PoisonError::into_inner);
        if recorded.is_some() {
            return false;
        }
        *recorded = Some(reason);
        true
    }

    fn reason(&self) -> Option<CancelReason> {
        *self.reason.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The turn a session is running right now.
struct ActiveTurn {
    end: Arc<TurnEnd>,
    control: Arc<dyn mango_external_agents::ProcessControl>,
}

/// Everything about a session that changes after it is opened.
#[derive(Default)]
struct SessionState {
    /// The handle the next turn resumes.
    ///
    /// Normally the id this session was opened with: `--session-id` is echoed back verbatim. A run
    /// that reported a different one is followed, because that is the conversation that now exists.
    native_session_id: String,
    /// False until a run has actually created the conversation on disk.
    established: bool,
    /// The turn now running, when one is.
    active: Option<ActiveTurn>,
    /// The `--mcp-config` file every turn loads, when the host configured servers.
    ///
    /// Held here rather than on [`Shared`] so that closing the session takes it out and drops it,
    /// which is what removes it from disk. A session that is dropped without being closed removes
    /// it too, when this state goes.
    mcp_config: Option<ConfigFile>,
    closed: bool,
}

/// What a session needs for its whole life, shared with the task each turn runs on.
struct Shared {
    host: HostContext,
    executable: ExecutablePath,
    info: SessionInfo,
    availability: ModeAvailability,
    surface: Option<CliSurface>,
    state: Mutex<SessionState>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, SessionState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One live Claude conversation.
pub struct ClaudeSession {
    shared: Arc<Shared>,
}

impl ClaudeSession {
    /// Adopts a session id without starting anything.
    ///
    /// A resume reference is taken at face value. Verifying one would cost a process launch per
    /// open, and a wrong guess is recoverable: a session Claude has forgotten fails at the first
    /// turn with the vendor's own message rather than at a probe nobody asked for. That is why
    /// [`ResumeMode`](mango_external_agents::ResumeMode) makes no difference here, and why
    /// [`SessionInfo::fallback_reason`] is always `None` — nothing was verified, so nothing fell
    /// back.
    pub(crate) fn new(
        host: HostContext,
        executable: ExecutablePath,
        info: SessionInfo,
        availability: ModeAvailability,
        surface: Option<CliSurface>,
        mcp_config: Option<ConfigFile>,
    ) -> Self {
        let state = SessionState {
            native_session_id: info.ids.native_session_id.clone(),
            established: info.resumed,
            active: None,
            mcp_config,
            closed: false,
        };
        Self {
            shared: Arc::new(Shared {
                host,
                executable,
                info,
                availability,
                surface,
                state: Mutex::new(state),
            }),
        }
    }

    /// Stops whatever turn is running, for one reason.
    ///
    /// The guard is taken and released in a statement of its own before anything is awaited: a
    /// `Session` method that held this lock across an await would deadlock the moment the turn
    /// channel filled, which is exactly when a host is least able to do anything about it.
    async fn stop_active_turn(&self, reason: CancelReason) {
        let active = self.shared.lock().active.take();
        end_turn(active, reason).await;
    }
}

/// Records why a turn stopped and ends its child. The pump writes the events.
async fn end_turn(active: Option<ActiveTurn>, reason: CancelReason) {
    let Some(active) = active else {
        return;
    };
    if !active.end.record(reason) {
        return;
    }
    let _ = active.control.kill(reason).await;
}

#[async_trait::async_trait]
impl mango_external_agents::Session for ClaudeSession {
    fn info(&self) -> &SessionInfo {
        &self.shared.info
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        if !request.attachments.is_empty() {
            // Claude Code's stream-json input takes content blocks, but nothing here encodes one
            // and `Capabilities::images` is false. Refusing is the honest answer: silently
            // dropping a file the user attached would run a turn about something that was never
            // sent.
            return Err(Error::HostConfiguration {
                expected: "a turn with no attachments, which this harness does not forward",
                received: format!("{} attachments", request.attachments.len()),
            });
        }

        let configuration = request
            .configuration
            .clone()
            .unwrap_or_else(|| self.shared.info.effective_configuration.clone());
        let mode = self.shared.resolve_mode(&configuration)?;

        // A host that starts a second turn has decided the first is over. Taken before anything is
        // spawned so the two cannot both hold a child.
        let previous = {
            let mut state = self.shared.lock();
            if state.closed {
                return Err(Error::Closed { subject: "session" });
            }
            state.active.take()
        };
        end_turn(previous, CancelReason::Requested).await;

        let (native_session_id, established, mcp_config) = {
            let state = self.shared.lock();
            (
                state.native_session_id.clone(),
                state.established,
                state
                    .mcp_config
                    .as_ref()
                    .map(|file| file.argument().to_owned()),
            )
        };
        let argv = TurnArgv {
            program: PROGRAM,
            mode,
            native_session_id: &native_session_id,
            established,
            model: configuration.model.as_deref(),
            effort: configuration.effort.as_deref(),
            accepted_efforts: self
                .shared
                .surface
                .as_ref()
                .and_then(CliSurface::effort_levels),
            declares_permission_prompts: self
                .shared
                .surface
                .as_ref()
                .is_some_and(CliSurface::declares_permission_prompts),
            mcp_config: mcp_config.as_deref(),
        }
        .build();

        let transport = stdio::open(
            &self.shared.host,
            &StdioSpec::new(argv),
            &self.shared.executable,
            VENDOR_ENVIRONMENT_KEYS,
        )
        .await?;

        let limits = *self.shared.host.limits();
        let (sink, events) = EventSink::new(
            self.shared.info.ids.session_id.clone(),
            request.turn_id.clone(),
            Arc::clone(self.shared.host.clock()),
            limits.turn_channel_capacity,
        );
        let end = Arc::new(TurnEnd::default());
        let control = Arc::clone(&transport.control);
        {
            let mut state = self.shared.lock();
            state.active = Some(ActiveTurn {
                end: Arc::clone(&end),
                control: Arc::clone(&control),
            });
        }

        tokio::spawn(pump(
            Arc::clone(&self.shared),
            transport.link,
            control,
            sink,
            end,
            request.input,
            established,
        ));

        Ok(TurnStream {
            // `claude --print` names no turn, so the handle is the host's own id: a value the host
            // can reproduce, which is what a retry needs.
            native_turn_id: request.turn_id.as_str().to_owned(),
            turn_id: request.turn_id,
            events,
        })
    }

    /// There is nothing to answer.
    ///
    /// `Capabilities::interactive_approvals` is false for this harness, so no
    /// [`EventKind::ApprovalRequested`](mango_external_agents::EventKind::ApprovalRequested) is
    /// ever emitted and nothing routes an answer here. Reaching this method means a caller invented
    /// an approval. See `docs/harness-claude.md` for what the vendor does offer and why this
    /// harness does not drive it.
    async fn respond(&self, response: PermissionResponse) -> Result<()> {
        Err(Error::Vendor(VendorError::new(
            ErrorCode::from_static("claude-approvals-unsupported"),
            format!(
                "expected an approval this harness raised, received an answer to {:?}; Claude Code delivers no answerable approval over its documented headless surface",
                response.request_id
            ),
        )))
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        self.stop_active_turn(reason).await;
        Ok(())
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        // Idempotent: a close racing a cancel, or two closes from different tasks, must not fail
        // the second caller. The take happens under the guard; the kill happens after it is gone.
        // The configuration file leaves with the session that wrote it: taken here, dropped at
        // the end of this statement, and removed from disk by that drop.
        let active = {
            let mut state = self.shared.lock();
            state.closed = true;
            drop(state.mcp_config.take());
            state.active.take()
        };
        end_turn(active, CancelReason::from(reason)).await;
        Ok(())
    }
}

impl Shared {
    /// The mode this configuration resolves to on this account and this build.
    ///
    /// Refused before a process starts. Discovery already reported the pair unsupported, but a
    /// stored configuration outlives a discovery: passing `--permission-mode auto` to a CLI whose
    /// managed settings reject it produces a startup failure indistinguishable from every other
    /// startup failure.
    fn resolve_mode(&self, configuration: &Configuration) -> Result<permissions::CliMode> {
        permissions::permission_mode(
            configuration.level,
            configuration.routing,
            &self.availability,
        )
        .filter(|mode| self.availability.accepts(*mode))
        .ok_or_else(|| Error::HostConfiguration {
            expected: "a permission level and routing this account and build can run",
            received: format!("{:?} with {:?}", configuration.level, configuration.routing),
        })
    }
}

/// One turn, from the prompt to the terminal event.
///
/// The only thing in this harness that emits.
async fn pump(
    shared: Arc<Shared>,
    link: mango_external_agents::Link,
    control: Arc<dyn mango_external_agents::ProcessControl>,
    sink: EventSink,
    end: Arc<TurnEnd>,
    input: String,
    resumed: bool,
) {
    let mut reducer = TurnReducer::new(resumed);
    let (mut sender, mut receiver) = link.split();

    // One message, then end of input. A second message would run as its own turn with its own
    // result, which is a queued follow-up rather than steering — see `Capabilities::steering`.
    // Ending the input is also what the vendor documents as cancelling a pending prompt, so a run
    // that would have waited for an answer nobody can give stops waiting.
    let written = sender.send(prompt_line(&input)).await;
    let closed = sender.close().await;
    if let Err(error) = written.and(closed) {
        finish(&mut reducer, &sink, &end, &control, Some(error)).await;
        clear_active(&shared, &end);
        return;
    }

    let cancel_token = shared.host.cancel().clone();
    let mut failure = None;
    loop {
        let line = tokio::select! {
            () = cancel_token.cancelled() => {
                end.record(CancelReason::Shutdown);
                let _ = control.kill(CancelReason::Shutdown).await;
                break;
            }
            received = tokio::time::timeout(STREAM_IDLE_TIMEOUT, receiver.recv()) => received,
        };
        let line = match line {
            Ok(Ok(Some(line))) => line,
            Ok(Ok(None)) => break,
            Ok(Err(error)) => {
                failure = Some(error);
                break;
            }
            Err(_elapsed) => {
                failure = Some(Error::Timeout {
                    operation: String::from("a stream-json record"),
                    after: STREAM_IDLE_TIMEOUT,
                });
                break;
            }
        };

        let Some(record) = StreamRecord::parse(&line) else {
            continue;
        };
        let reduction = reducer.reduce(&record);
        if let Some(init) = reduction.init {
            apply_init(&shared, init);
        }
        for event in reduction.events {
            match sink.emit(event).await {
                Ok(()) => {}
                // The host dropped the stream. There is nobody to tell, and a turn nobody is
                // reading is a turn to stop feeding.
                Err(Error::Closed { .. }) => {
                    end.record(CancelReason::Requested);
                    let _ = control.kill(CancelReason::Requested).await;
                    clear_active(&shared, &end);
                    return;
                }
                // A vendor value that could not be made safe to keep. One event is dropped; the
                // turn is not, because the rest of the run is still worth watching.
                Err(_) => {}
            }
        }
        if reducer.finished() {
            break;
        }
    }

    finish(&mut reducer, &sink, &end, &control, failure).await;
    clear_active(&shared, &end);
}

/// Writes the turn's terminal event, whichever way it ended, and reaps the child.
async fn finish(
    reducer: &mut TurnReducer,
    sink: &EventSink,
    end: &TurnEnd,
    control: &Arc<dyn mango_external_agents::ProcessControl>,
    failure: Option<Error>,
) {
    if reducer.finished() {
        // The vendor's own `result` already ended the turn. A cancel that arrived after it changes
        // nothing: the turn did finish.
    } else if let Some(reason) = end.reason() {
        for event in reducer.cancel() {
            let _ = sink.emit(event).await;
        }
        let _ = sink.cancel(reason).await;
    } else {
        let exit = control.wait().await.ok();
        for event in reducer.abort(no_result_error(failure, exit, control.stderr_tail())) {
            let _ = sink.emit(event).await;
        }
    }
    // Background subagents can hold the process open after the result, so the turn does not wait on
    // the exit — but nothing is allowed to outlive the turn that started it either.
    let _ = control.kill(CancelReason::Shutdown).await;
}

/// Forgets this turn, unless a newer one already replaced it.
fn clear_active(shared: &Shared, end: &Arc<TurnEnd>) {
    let mut state = shared.lock();
    if state
        .active
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(&active.end, end))
    {
        state.active = None;
    }
}

/// Folds a run's `system/init` back into the session.
///
/// The session id is the one thing discovery could not know, because it takes a live process to
/// learn it — and the record proves the conversation now exists on disk, which is what makes the
/// *next* turn a `--resume` rather than another attempt to mint an id the CLI has already taken.
///
/// `init.permission_mode` is deliberately **not** folded back. Every run is launched with an
/// explicit `--permission-mode`, so the record echoes the mode this harness chose rather than the
/// account's own default; reading it as if it could establish one is actively unsafe, because a
/// single turn at auto-review would then resolve the plain default level to `auto` for the rest of
/// the session. A user who asked to be asked would stop being asked.
fn apply_init(shared: &Shared, init: RunInit) {
    let Some(session_id) = init.session_id else {
        return;
    };
    let mut state = shared.lock();
    state.native_session_id = session_id;
    state.established = true;
}

/// One user message, as `--input-format stream-json` takes it.
fn prompt_line(input: &str) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": input }] }
    })
    .to_string()
}

/// What to say about a process that ended without a `result` record.
fn no_result_error(
    failure: Option<Error>,
    exit: Option<mango_external_agents::ExitStatus>,
    stderr_tail: String,
) -> VendorError {
    if let Some(failure) = failure {
        return VendorError::new(
            ErrorCode::from_static("claude-stream-broken"),
            failure.to_string(),
        );
    }
    // Exit 143 without a recorded cancellation: something outside this harness stopped the child.
    // Named rather than reported as an unexplained failure, because it is the one exit code the
    // vendor documents.
    if exit.is_some_and(|exit| exit.code == Some(SIGTERM_EXIT_CODE)) {
        return VendorError::new(
            ErrorCode::from_static("claude-terminated"),
            "Claude Code was stopped before the turn finished",
        );
    }
    let ended = match exit {
        Some(exit) => match (exit.code, exit.signal) {
            (Some(code), _) => format!("exit code {code}"),
            (None, Some(signal)) => format!("signal {signal}"),
            (None, None) => String::from("no exit status"),
        },
        None => String::from("no exit status"),
    };
    let detail = stderr_tail.trim().to_owned();
    let message = if detail.is_empty() {
        format!("Claude Code ended without a result ({ended})")
    } else {
        format!("Claude Code ended without a result ({ended}): {detail}")
    };
    VendorError::new(ErrorCode::from_static("claude-no-result"), message)
}

#[cfg(test)]
mod tests {
    use super::{TurnEnd, no_result_error, prompt_line};
    use mango_external_agents::{CancelReason, ExitStatus};

    #[test]
    fn only_the_first_caller_to_stop_a_turn_records_the_reason() {
        let end = TurnEnd::default();
        assert!(end.record(CancelReason::Requested));
        assert!(
            !end.record(CancelReason::Shutdown),
            "expected the second call to lose"
        );
        assert_eq!(end.reason(), Some(CancelReason::Requested));
    }

    #[test]
    fn the_prompt_travels_as_one_stream_json_user_message() {
        let line = prompt_line("read note.txt");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("expected valid JSON");
        assert_eq!(parsed["type"], "user");
        assert_eq!(parsed["message"]["role"], "user");
        assert_eq!(parsed["message"]["content"][0]["type"], "text");
        assert_eq!(parsed["message"]["content"][0]["text"], "read note.txt");
        assert!(!line.contains('\n'), "expected one line, received {line:?}");
    }

    #[test]
    fn a_prompt_that_contains_json_is_still_one_line() {
        let line = prompt_line("send {\"type\":\"result\"}\nand a newline");
        assert!(
            !line.contains('\n'),
            "expected the newline to be escaped, received {line:?}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("expected valid JSON");
        assert_eq!(
            parsed["message"]["content"][0]["text"],
            "send {\"type\":\"result\"}\nand a newline"
        );
    }

    #[test]
    fn names_the_one_exit_code_the_vendor_documents() {
        let error = no_result_error(
            None,
            Some(ExitStatus {
                code: Some(143),
                signal: None,
            }),
            String::new(),
        );
        assert_eq!(error.code.as_str(), "claude-terminated");
    }

    #[test]
    fn carries_the_stderr_tail_into_an_unexplained_exit() {
        let error = no_result_error(
            None,
            Some(ExitStatus {
                code: Some(1),
                signal: None,
            }),
            String::from("  error: unknown option '--forward-subagent-text'\n"),
        );
        assert_eq!(error.code.as_str(), "claude-no-result");
        assert!(
            error.message.contains("exit code 1"),
            "received {:?}",
            error.message
        );
        assert!(
            error.message.contains("--forward-subagent-text"),
            "received {:?}",
            error.message
        );
    }

    #[test]
    fn a_link_failure_is_reported_as_one_rather_than_as_a_missing_result() {
        let error = no_result_error(
            Some(mango_external_agents::Error::LimitExceeded {
                subject: "one vendor output line",
                limit: 1024,
                received: 2048,
            }),
            None,
            String::new(),
        );
        assert_eq!(error.code.as_str(), "claude-stream-broken");
        assert!(
            error.message.contains("2048"),
            "received {:?}",
            error.message
        );
    }
}
