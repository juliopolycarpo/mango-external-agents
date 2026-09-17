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

use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use mango_external_agents::{
    CancelReason, CloseReason, Configuration, Error, ErrorCode, EventSink, ExecutablePath,
    HostContext, PermissionResponse, Result, SessionIds, SessionInfo, SessionLifecycle, StdioSpec,
    TurnRequest, TurnStream, VendorError, transports::stdio,
};
use serde_json::json;

use crate::argv::TurnArgv;
use crate::cli_surface::CliSurface;
use crate::mcp::ConfigFile;
use crate::models;
use crate::permissions::{self, ModeAvailability};
use crate::pinned::{SIGTERM_EXIT_CODE, STREAM_IDLE_TIMEOUT, VENDOR_ENVIRONMENT_KEYS};
use crate::probe::PROGRAM;
use crate::protocol::StreamRecord;
use crate::reducer::{RunInit, TurnReducer};

/// How a run ended, and who decided.
///
/// One transition, taken once. Everything that can stop a turn — a host's `cancel`, a `close`, the
/// host's own shutdown token — races to [`set`](OnceLock::set) the reason here and stops; the pump
/// reads it after the stream ends and writes the terminal pair. Nothing else emits, so "exactly one
/// terminal" is a property of the code's shape rather than of its timing.
type TurnEnd = OnceLock<CancelReason>;

/// The turn a session is running right now.
///
/// `control` is `None` from the moment a turn claims the slot until `stdio::open` returns it a
/// child: the reservation exists so a stop landing in that window has a place to record its
/// reason, even though there is nothing yet to kill.
struct ActiveTurn {
    end: Arc<TurnEnd>,
    control: Option<Arc<dyn mango_external_agents::ProcessControl>>,
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
    /// Held here rather than on [`Shared`] so closing releases the session's owner. An in-flight
    /// start holds a second owner until it installs or reaps its child, then the final owner removes
    /// the file. A session dropped without being closed releases this state too.
    mcp_config: Option<Arc<ConfigFile>>,
    /// The settings a turn without an override inherits.
    configuration: Configuration,
}

/// What a session needs for its whole life, shared with the task each turn runs on.
struct Shared {
    host: HostContext,
    executable: ExecutablePath,
    info: SessionInfo,
    availability: ModeAvailability,
    surface: Option<CliSurface>,
    lifecycle: SessionLifecycle,
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
    /// A resume reference is checked for shape and then taken on trust — see
    /// [`is_vendor_session_id`](crate::argv::is_vendor_session_id) for why the shape is checked at
    /// all. Verifying that the conversation still *exists* would cost a process launch per open,
    /// and a wrong guess is recoverable: a session Claude has forgotten fails at the first turn
    /// with the vendor's own message rather than at a probe nobody asked for. That is why
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
            mcp_config: mcp_config.map(Arc::new),
            configuration: info.effective_configuration.clone(),
        };
        Self {
            shared: Arc::new(Shared {
                host,
                executable,
                info,
                availability,
                surface,
                lifecycle: SessionLifecycle::default(),
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
        let control = take_turn(&mut self.shared.lock(), reason);
        end_turn(control, reason).await;
    }
}

/// Takes whatever turn is active and records why it stopped, under the same guard that takes it.
///
/// A turn's slot is reserved before its child is spawned, so a stop can land while `control` is
/// still `None` — nothing to kill yet, but the reason must not be lost. Recording it here, under
/// the lock that empties the slot, is what makes that reason visible to the spawn once it returns;
/// only the actual kill happens after the guard is released.
fn take_turn(
    state: &mut SessionState,
    reason: CancelReason,
) -> Option<Arc<dyn mango_external_agents::ProcessControl>> {
    let active = state.active.take()?;
    if active.end.set(reason).is_err() {
        return None;
    }
    active.control
}

/// Ends a child taken by [`take_turn`], if it had one yet. The pump writes the events.
async fn end_turn(
    control: Option<Arc<dyn mango_external_agents::ProcessControl>>,
    reason: CancelReason,
) {
    if let Some(control) = control {
        let _ = control.kill(reason).await;
    }
}

#[async_trait::async_trait]
impl mango_external_agents::Session for ClaudeSession {
    fn info(&self) -> &SessionInfo {
        &self.shared.info
    }

    /// The handle in force, which is not always the one opening minted.
    ///
    /// `--session-id` proposes a UUID and `system/init` normally echoes it back, but a run is free
    /// to report another, and from then on that is the only handle `--resume` accepts. Every turn
    /// after the first already follows it; this is what lets a host see it too, so the value it
    /// persists is the one its next `open_session` can actually resume. `info()` keeps the id
    /// opening answered with, which is what makes the change legible rather than silent.
    fn ids(&self) -> SessionIds {
        SessionIds {
            native_session_id: self.shared.lock().native_session_id.clone(),
            ..self.shared.info.ids.clone()
        }
    }

    async fn configuration(&self) -> Configuration {
        self.shared.lock().configuration.clone()
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        self.validate_turn_request(&request)?;
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

        let requested_configuration = request.configuration.clone();
        let configuration = {
            let current = self.shared.lock().configuration.clone();
            requested_configuration
                .as_ref()
                .map_or(current.clone(), |overrides| {
                    current.with_overrides(overrides)
                })
        };
        let mode = self.shared.resolve_mode(&configuration)?;
        // Configuration is caller-owned and becomes a value position after a Claude option.
        // Reject it before reserving a child: omitting an invalid explicit value would run the
        // turn under a setting the host did not select.
        models::validate_configuration(&configuration, self.shared.surface.as_ref())?;

        // A host that starts a second turn has decided the first is over. Taken before anything is
        // spawned so the two cannot both hold a child, and the slot this turn claims is reserved
        // in the same statement: a `cancel`, a `close`, or a third `start_turn` landing before
        // `stdio::open` returns below then has this reservation, rather than an empty slot, to
        // record its reason against.
        let end = Arc::new(TurnEnd::default());
        let (previous, mcp_lease) = {
            let Some(_lifecycle) = self.shared.lifecycle.begin_start() else {
                return Err(Error::Closed { subject: "session" });
            };
            let mut state = self.shared.lock();
            let previous = take_turn(&mut state, CancelReason::Requested);
            state.active = Some(ActiveTurn {
                end: Arc::clone(&end),
                control: None,
            });
            // The reservation and this clone share the same critical section. A close that wins
            // after it can release the session's reference, but this attempt still owns the file
            // until it has either installed or reaped the child it launches.
            (previous, state.mcp_config.as_ref().map(Arc::clone))
        };
        end_turn(previous, CancelReason::Requested).await;

        let (native_session_id, established) = {
            let state = self.shared.lock();
            (state.native_session_id.clone(), state.established)
        };
        let mcp_config = mcp_lease.as_ref().map(|file| file.argument().to_owned());
        let argv = match (TurnArgv {
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
        })
        .build()
        {
            Ok(argv) => argv,
            Err(error) => {
                clear_active(&self.shared, &end);
                // A close that won the race already released the session's own reference, which
                // makes this lease the last one and its drop the `remove_dir_all`. Off the worker,
                // for the same reason the write is.
                crate::mcp::release_off_worker(mcp_lease).await;
                return Err(error);
            }
        };

        let transport = match stdio::open(
            &self.shared.host,
            &StdioSpec::new(argv),
            &self.shared.executable,
            VENDOR_ENVIRONMENT_KEYS,
        )
        .await
        {
            Ok(transport) => transport,
            // No child ever launched, so there is nothing to reap here — only the reservation
            // this call made above, and only if a stop has not already taken it.
            Err(error) => {
                clear_active(&self.shared, &end);
                crate::mcp::release_off_worker(mcp_lease).await;
                return Err(error);
            }
        };

        // The lease is still held past `stdio::open`, deliberately: `close` may have taken the
        // session's `Arc` while the launcher awaited, but it cannot remove the file until this
        // call either installs the child or kills the stopped child below. Installing hands the
        // artifact back to the session, and this clone is then the cheap one to drop.
        let limits = *self.shared.host.limits();
        let (sink, events) = EventSink::new(
            self.shared.info.ids.session_id.clone(),
            request.turn_id.clone(),
            Arc::clone(self.shared.host.clock()),
            limits.turn_channel_capacity,
        );
        let control = Arc::clone(&transport.control);
        // Re-checked under the same guard that installs the control, because `stdio::open` is
        // awaited above and a stop can have landed while this reservation had no control to kill:
        // a `close` marks `closed` and takes it, a `cancel` or a racing `start_turn` takes it and
        // records the reason but kills nothing, because there was nothing here to kill yet. Either
        // way no stream has been handed out, so there is nothing for the stop to reach — this reaps
        // the child that just started and refuses instead of planting a live process nobody holds
        // a handle to.
        let stopped = {
            let lifecycle = self.shared.lifecycle.lock();
            let mut state = self.shared.lock();
            if !lifecycle.is_closed()
                && state
                    .active
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(&active.end, &end))
            {
                if requested_configuration.is_some() {
                    // `stdio::open` is the successful start boundary for the batch CLI: there is
                    // no app-server response to acknowledge later. A following unconfigured turn
                    // therefore repeats the flags the accepted process was launched with.
                    state.configuration = configuration.clone();
                }
                state.active = Some(ActiveTurn {
                    end: Arc::clone(&end),
                    control: Some(Arc::clone(&control)),
                });
                None
            } else {
                end.get().copied()
            }
        };
        if let Some(reason) = stopped {
            let _ = control.kill(reason).await;
            // After the kill, never before: the child read `--mcp-config` at startup. And off the
            // worker, because a close that won the race left this lease holding the last
            // reference, so this is where the `remove_dir_all` happens.
            crate::mcp::release_off_worker(mcp_lease).await;
            return Err(if self.shared.lifecycle.is_closed() {
                Error::Closed { subject: "session" }
            } else {
                Error::Cancelled { reason }
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
        let _ = response;
        self.require_capability(mango_external_agents::Capability::InteractiveApprovals)
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        self.stop_active_turn(reason).await;
        Ok(())
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        // Idempotent: a close racing a cancel, or two closes from different tasks, must not fail
        // the second caller. Both takes happen under the guard; nothing slow happens under it.
        let (control, mcp_config) = {
            let mut lifecycle = self.shared.lifecycle.lock();
            if !lifecycle.close() {
                return Ok(());
            }
            let mut state = self.shared.lock();
            let control = take_turn(&mut state, CancelReason::from(reason));
            (control, state.mcp_config.take())
        };
        end_turn(control, CancelReason::from(reason)).await;
        // The session releases its reference here. A start that is still awaiting a child retains
        // its own `Arc` through the post-spawn lifecycle check, then kills that child before the
        // final reference can remove the file. Off the lock, and off the async worker: removing
        // the directory is a synchronous filesystem call against the host's own scratch root.
        crate::mcp::release_off_worker(mcp_config).await;
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
    fn resolve_mode(&self, configuration: &Configuration) -> Result<Option<permissions::CliMode>> {
        permissions::configuration_mode(configuration, &self.availability)
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
    // The host's own patience for a child that should be exiting; it owns the process, so it owns
    // how long the turn waits on one that is not.
    let exit_grace = shared.host.limits().kill_grace;
    let (mut sender, mut receiver) = link.split();

    // One message, then end of input. A second message would run as its own turn with its own
    // result, which is a queued follow-up rather than steering — see `Capabilities::steering`.
    // Ending the input is also what the vendor documents as cancelling a pending prompt, so a run
    // that would have waited for an answer nobody can give stops waiting.
    let written = sender.send(prompt_line(&input)).await;
    let closed = sender.close().await;
    if let Err(error) = written.and(closed) {
        finish(&mut reducer, &sink, &end, &control, Some(error), exit_grace).await;
        clear_active(&shared, &end);
        return;
    }

    let cancel_token = shared.host.cancel().clone();
    let mut failure = None;
    loop {
        let line = tokio::select! {
            () = cancel_token.cancelled() => {
                let _ = end.set(CancelReason::Shutdown);
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
                    let _ = end.set(CancelReason::Requested);
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

    finish(&mut reducer, &sink, &end, &control, failure, exit_grace).await;
    clear_active(&shared, &end);
}

/// Writes the turn's terminal event, whichever way it ended, and reaps the child.
async fn finish(
    reducer: &mut TurnReducer,
    sink: &EventSink,
    end: &TurnEnd,
    control: &Arc<dyn mango_external_agents::ProcessControl>,
    failure: Option<Error>,
    exit_grace: Duration,
) {
    if reducer.finished() {
        // The vendor's own `result` already ended the turn. A cancel that arrived after it changes
        // nothing: the turn did finish.
    } else if let Some(&reason) = end.get() {
        for event in reducer.cancel() {
            let _ = sink.emit(event).await;
        }
        let _ = sink.cancel(reason).await;
    } else {
        // The exit status is worth a moment, because it is what names an exit the vendor documents
        // — but only a moment. A link that broke while the child worked on, or an idle timeout,
        // reaches here with a process that is alive and has no intention of exiting, and a turn
        // that waits for that exit is a turn that never ends. The status is the better message;
        // ending the turn is the one that has to happen. Killing first would be the other way
        // round: every broken link would then exit 143 and read as an outside interruption.
        let exit = tokio::time::timeout(exit_grace, control.wait())
            .await
            .ok()
            .and_then(std::result::Result::ok);
        for event in reducer.abort(no_result_error(failure, exit, control.stderr_tail())) {
            let _ = sink.emit(event).await;
        }
    }
    // What can still be running here is a background *Bash* task the run started — a dev server, a
    // watch build — which the vendor gives about five seconds after the result before terminating
    // it. So the turn does not wait on the exit, and nothing is allowed to outlive the turn that
    // started it either.
    //
    // Not a background subagent: those are waited for *before* the result, because their output is
    // part of the final answer, under the ceiling `VENDOR_ENVIRONMENT_KEYS` forwards. This kill
    // lands after the record that wait produced, so it pre-empts nothing.
    //
    // <https://code.claude.com/docs/en/headless.md>
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
    // The conversation exists either way — that is what the record proves, and it is what makes
    // the next turn a `--resume`.
    state.established = true;
    // Which handle it is followed under is a different question. This is the one value in this
    // file a vendor process chooses and a later argv carries, so it is vetted like the resume
    // reference a host supplies: a handle beginning with `-` would be read by the CLI's parser as
    // a flag rather than as `--resume`'s value. An unrecognisable echo leaves the minted id in
    // force, which is the id this run was asked to write and the better of the two guesses.
    if crate::argv::is_vendor_session_id(&session_id) {
        state.native_session_id = session_id;
    }
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
    let ended = match exit.map(|exit| (exit.code, exit.signal)) {
        Some((Some(code), _)) => format!("exit code {code}"),
        Some((None, Some(signal))) => format!("signal {signal}"),
        Some((None, None)) | None => String::from("no exit status"),
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
        let end = TurnEnd::new();
        assert!(end.set(CancelReason::Requested).is_ok());
        assert!(
            end.set(CancelReason::Shutdown).is_err(),
            "expected the second call to lose"
        );
        assert_eq!(end.get().copied(), Some(CancelReason::Requested));
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
