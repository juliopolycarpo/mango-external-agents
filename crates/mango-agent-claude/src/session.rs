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
    CancelReason, CloseReason, Configuration, ConfigurationState, Error, ErrorCode, EventKind,
    EventSink, ExecutablePath, HostContext, PermissionResponse, Result, SessionLifecycle,
    SessionState as CoreSessionState, SessionStatus, StdioSpec, TurnRequest, TurnStream,
    VendorError, event, transports::stdio,
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

/// Everything about a session that changes after it is opened and that
/// [`mango_external_agents::SessionState`] has no axis for — Claude's own process bookkeeping
/// rather than a fact a host reads off a snapshot.
#[derive(Default)]
struct Mutable {
    /// False until a run has actually created the conversation on disk.
    ///
    /// Distinct from [`SessionSnapshot::resumed`](mango_external_agents::SessionSnapshot::resumed):
    /// that records whether *opening* continued an existing conversation, and never changes again.
    /// This records whether the CLI should be told `--resume` or `--session-id` on the *next* turn,
    /// which flips from false to true the first time a run actually writes the conversation to
    /// disk — including a session that opened fresh.
    established: bool,
    /// The turn now running, when one is.
    active: Option<ActiveTurn>,
    /// The `--mcp-config` file every turn loads, when the host configured servers.
    ///
    /// Held here rather than on [`Shared`] so closing releases the session's owner. An in-flight
    /// start holds a second owner until it installs or reaps its child, then the final owner removes
    /// the file. A session dropped without being closed releases this state too.
    mcp_config: Option<Arc<ConfigFile>>,
}

impl Drop for Mutable {
    /// Hands the session's own artifact reference to the blocking pool, as every other path that
    /// can drop the last one does.
    ///
    /// `ConfigFile`'s `Drop` removes the directory synchronously, deliberately: a value dropped
    /// with no runtime has nothing to hand the call to. A session dropped rather than closed does
    /// usually have one — the host drops its handle inside a task, or a finished pump releases the
    /// last `Shared` on a worker — and that is where a `remove_dir_all` against a host's FUSE,
    /// container or network mount would stall every other task on that thread. `close` has already
    /// taken this by the time it runs, so the ordinary path costs nothing.
    fn drop(&mut self) {
        if let Some(config) = self.mcp_config.take() {
            crate::mcp::release_on_drop(config);
        }
    }
}

/// What a session needs for its whole life, shared with the task each turn runs on.
struct Shared {
    host: HostContext,
    executable: ExecutablePath,
    /// The vendor's own handle, the settings in force, the commands announced and what this
    /// session can do — everything [`mango_external_agents::Session::state`] answers with.
    core_state: CoreSessionState,
    availability: ModeAvailability,
    surface: Option<CliSurface>,
    lifecycle: SessionLifecycle,
    mutable: Mutex<Mutable>,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Mutable> {
        self.mutable.lock().unwrap_or_else(PoisonError::into_inner)
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
    /// [`SessionSnapshot::fallback_reason`](mango_external_agents::SessionSnapshot::fallback_reason)
    /// is always `None` — nothing was verified, so nothing fell back.
    ///
    /// `core_state` arrives already carrying the opening snapshot — see
    /// [`ClaudeHarness::open_session`](crate::ClaudeHarness) — so `established` here starts at
    /// whether that snapshot itself records a resumed conversation.
    pub(crate) fn new(
        host: HostContext,
        executable: ExecutablePath,
        core_state: CoreSessionState,
        availability: ModeAvailability,
        surface: Option<CliSurface>,
        mcp_config: Option<ConfigFile>,
    ) -> Self {
        let established = core_state.snapshot().resumed;
        let mutable = Mutable {
            established,
            active: None,
            mcp_config: mcp_config.map(Arc::new),
        };
        Self {
            shared: Arc::new(Shared {
                host,
                executable,
                core_state,
                availability,
                surface,
                lifecycle: SessionLifecycle::default(),
                mutable: Mutex::new(mutable),
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
    state: &mut Mutable,
    reason: CancelReason,
) -> Option<Arc<dyn mango_external_agents::ProcessControl>> {
    let active = state.active.take()?;
    if active.end.set(reason).is_err() {
        return None;
    }
    active.control
}

/// Kills the child a `start_turn` launched if that call never hands back its stream.
///
/// The awaits between installing a child and returning its stream are the problem this exists
/// for. A caller that times out or drops `start_turn` at one of them leaves a started child that
/// nothing else is watching: no pump has been spawned to reap it, this crate implements no `Drop`
/// for a session, and a [`ProcessControl`](mango_external_agents::ProcessControl) that is merely
/// dropped is not killed — the launcher sets no kill-on-drop. Without this the process would
/// outlive the host's interest in it until an explicit `close`, `cancel` or next `start_turn`, and
/// a host that simply drops the session issues none of the three.
///
/// Disarmed once the pump owns the child, which is the moment something else is responsible for it.
struct AbandonedStart {
    shared: Arc<Shared>,
    end: Arc<TurnEnd>,
    armed: bool,
}

impl AbandonedStart {
    /// Hands responsibility for the child to whoever comes next.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AbandonedStart {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The take decides the kill. `ProcessControl::kill` documents that the library asks once,
        // so a stop that already took this turn owns its teardown and must not be asked again;
        // taking it out under the lock is also what stops a later close or start from finding an
        // active turn pointing at a child being reaped here.
        let taken = {
            let mut state = self.shared.lock();
            if state
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(&active.end, &self.end))
            {
                take_turn(&mut state, CancelReason::Requested)
            } else {
                None
            }
        };
        let Some(control) = taken else {
            return;
        };
        // `kill` is async and a `Drop` is not, so the reap runs on a task of its own. Off a
        // runtime there is nothing to spawn onto and nothing left that could await a child.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = control.kill(CancelReason::Requested).await;
            });
        }
    }
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
    /// This session's live, observable state.
    ///
    /// The vendor's own handle is folded onto it by `apply_init` as `system/init` reports one —
    /// `--session-id` proposes a UUID and a run is free to report another, and from then on that
    /// is the only handle `--resume` accepts. [`SessionState::set_native_session_id`] is how that
    /// change reaches a host, in place of the turn-specific override [`Session::ids`] used to
    /// carry.
    ///
    /// [`SessionState::set_native_session_id`]: mango_external_agents::SessionState::set_native_session_id
    /// [`Session::ids`]: mango_external_agents::Session::ids
    fn state(&self) -> &CoreSessionState {
        &self.shared.core_state
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
        if let Some(patch) = &requested_configuration {
            // Same rule as opening: a fresh child is the only place Claude's mode and model land,
            // so there is no argv this harness could encode that un-sets one mid-session.
            mango_external_agents::configuration::refuse_unsupported_reset(patch)?;
        }
        let current = self
            .shared
            .core_state
            .snapshot()
            .configuration
            .accepted
            .clone();
        let configuration = requested_configuration
            .as_ref()
            .map_or_else(|| current.clone(), |patch| current.patched(patch));
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
        let (previous, mut mcp_lease) = {
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
            (
                previous,
                // In a cancellation-safe owner for the same reason `open_session`'s artifact is:
                // a caller that drops this future between here and the return reaches no explicit
                // release, and a close that won the race leaves this clone the last reference.
                crate::mcp::Prepared::new(state.mcp_config.as_ref().map(Arc::clone)),
            )
        };
        end_turn(previous, CancelReason::Requested).await;

        let native_session_id = self
            .shared
            .core_state
            .snapshot()
            .ids
            .native_session_id
            .clone();
        let established = {
            let state = self.shared.lock();
            state.established
        };
        let mcp_config = mcp_lease.get().map(|file| file.argument().to_owned());
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
                crate::mcp::release_off_worker(mcp_lease.take()).await;
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
                crate::mcp::release_off_worker(mcp_lease.take()).await;
                return Err(error);
            }
        };

        // The lease is still held past `stdio::open`, deliberately: `close` may have taken the
        // session's `Arc` while the launcher awaited, but it cannot remove the file until this
        // call either installs the child or kills the stopped child below. Installing hands the
        // artifact back to the session, and this clone is then the cheap one to drop.
        let limits = *self.shared.host.limits();
        let (sink, events) = EventSink::new(
            self.shared.core_state.snapshot().ids.session_id.clone(),
            request.turn_id.clone(),
            request.attempt.clone(),
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
                    //
                    // Requested and accepted move together: this harness encodes exactly what was
                    // asked once a mode resolves, so there is nothing for the two to disagree
                    // about. Observed stays unknown — no documented Claude surface reports its own
                    // settings back.
                    self.shared
                        .core_state
                        .set_configuration(ConfigurationState::new(
                            configuration.clone(),
                            configuration.clone(),
                            Configuration::unknown(),
                        ));
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
            crate::mcp::release_off_worker(mcp_lease.take()).await;
            return Err(if self.shared.lifecycle.is_closed() {
                Error::Closed { subject: "session" }
            } else {
                Error::Cancelled { reason }
            });
        }

        // Before the pump, and this ordering is the whole point: the release parks on the blocking
        // pool, so it is an await a caller that times out or drops `start_turn` can be cancelled
        // at. A pump spawned above it would already hold the prompt and the process, and would
        // send that prompt and let Claude run tools before noticing the receiver this future drops
        // with — work performed for a turn nobody will ever read. Releasing first leaves nothing
        // detached: from the spawn below to the return there is no await, so the window is gone.
        //
        // Nothing about the file's lifetime moves with it. The lease is an `Arc` clone the session
        // also holds, so this is usually a cheap decrement; it removes the artifact only when a
        // `close` already took the session's reference, and that `close` is killing the child
        // anyway.
        // Armed before the release below, which is the await this call can be abandoned at now
        // that the child is installed. Disarmed where the pump takes the child over.
        let mut abandoned = AbandonedStart {
            shared: Arc::clone(&self.shared),
            end: Arc::clone(&end),
            armed: true,
        };

        crate::mcp::release_off_worker(mcp_lease.take()).await;

        // Re-checked for the same reason the guard above re-checks after `stdio::open`: the
        // release is an await, so a `close`, a `cancel` or a second `start_turn` can have taken
        // this turn while it was parked there. Such a stop records its reason and kills the child,
        // and spawning the pump afterwards would write the prompt to a child somebody is killing
        // and hand the caller a successful stream for a turn that is already over. No stream has
        // been handed out yet, so refusing here is still the honest answer.
        let stopped = {
            let lifecycle = self.shared.lifecycle.lock();
            let mut state = self.shared.lock();
            if lifecycle.is_closed()
                || !state
                    .active
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(&active.end, &end))
            {
                // The stop that took the turn set the reason before taking it; the fallback covers
                // a take that lost the `set` race, which is the same reason `take_turn` bails on.
                Some(end.get().copied().unwrap_or(CancelReason::Requested))
            } else {
                // Committed here rather than where the child was installed. `stdio::open` is the
                // successful start boundary for the batch CLI — there is no app-server response to
                // acknowledge later — but a start can still be refused after it, and a refusal
                // that had already written these defaults would hand a cancelled turn's model and
                // permissions to the next turn that asked for nothing. This is the last point
                // where the start can still fail, and no await follows it.
                if requested_configuration.is_some() {
                    self.shared
                        .core_state
                        .set_configuration(ConfigurationState::new(
                            configuration.clone(),
                            configuration.clone(),
                            Configuration::unknown(),
                        ));
                }
                None
            }
        };
        if let Some(reason) = stopped {
            // No kill here, unlike the check before the child was installed. There the stop had
            // found `control: None` and killed nothing, so this call owed the teardown. Here the
            // stop took an active turn that already carried the control and has ended that child
            // itself — and `ProcessControl::kill` documents that the library asks once. The guard
            // is disarmed for the same reason.
            abandoned.disarm();
            return Err(if self.shared.lifecycle.is_closed() {
                Error::Closed { subject: "session" }
            } else {
                Error::Cancelled { reason }
            });
        }

        // The pump owns the child from here, after the turn-start event is enqueued.
        // `claude --print` names no turn, so the handle is the host's own id: a value the host can
        // reproduce, which is what a retry needs. Emitted here, before the pump task exists to
        // race it with anything the vendor says, so it is always the first thing on the stream —
        // including on a run whose own `system/init` never arrives.
        let native_turn_id = request.turn_id.as_str().to_owned();
        sink.emit(EventKind::TurnStarted {
            native_turn_id: native_turn_id.clone(),
        })
        .await?;
        abandoned.disarm();
        tokio::spawn(pump(
            Arc::clone(&self.shared),
            transport.link,
            control,
            sink,
            end,
            request.input,
        ));

        Ok(TurnStream::accepted(
            request.turn_id,
            request.attempt,
            native_turn_id,
            events,
        ))
    }

    /// There is nothing to answer.
    ///
    /// `Capabilities::interactive_approvals` is false for this harness, so no
    /// [`EventKind::ApprovalRequested`] is
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
        let (control, mut mcp_config) = {
            let mut lifecycle = self.shared.lifecycle.lock();
            if !lifecycle.close() {
                return Ok(());
            }
            let mut state = self.shared.lock();
            let control = take_turn(&mut state, CancelReason::from(reason));
            // In the same cancellation-safe owner the open and the turn use: the kill below is a
            // `ProcessControl` call that can take as long as the host's escalation grace, and a
            // caller that gives up on the close in that window would otherwise drop this on the
            // async worker. There is no second chance at it either — the lifecycle is already
            // closed, so a following close returns before reaching here.
            (control, crate::mcp::Prepared::new(state.mcp_config.take()))
        };
        self.shared.core_state.set_status(SessionStatus::Closed);
        end_turn(control, CancelReason::from(reason)).await;
        // The session releases its reference here. A start still awaiting a child holds its own
        // `Arc` until it releases it just before its post-release ownership check, and that check
        // sees this close and kills the child it launched. Either order is safe: whichever
        // reference goes last removes the file, and the child it was written for is being killed
        // by one of the two paths. Off the lock, and off the async worker: removing
        // the directory is a synchronous filesystem call against the host's own scratch root.
        // Reported rather than swallowed: this close promised the session's resources were
        // released, and the file holds the `env` and `headers` a host configured its servers with.
        crate::mcp::remove_on_close(mcp_config.take()).await
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
) {
    let mut reducer = TurnReducer::new();
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
/// Both facts here are session state, not turn events — see [`RunInit`] and
/// [`mango_external_agents::state`] for why they moved off the stream and onto
/// [`mango_external_agents::SessionState`].
///
/// The session id is the one thing discovery could not know, because it takes a live process to
/// learn it — and the record proves the conversation now exists on disk, which is what makes the
/// *next* turn a `--resume` rather than another attempt to mint an id the CLI has already taken.
/// The command catalog is read whether or not the session id arrived, because it is useful on its
/// own and a run that only failed to name itself should not lose it — see
/// [`commands::catalog`](crate::commands::catalog).
///
/// `init.permission_mode` is deliberately **not** folded back. Every run is launched with an
/// explicit `--permission-mode`, so the record echoes the mode this harness chose rather than the
/// account's own default; reading it as if it could establish one is actively unsafe, because a
/// single turn at auto-review would then resolve the plain default level to `auto` for the rest of
/// the session. A user who asked to be asked would stop being asked.
fn apply_init(shared: &Shared, init: RunInit) {
    if let Some(commands) = init.commands {
        shared
            .core_state
            .set_commands(event::normalized_catalog(commands));
    }

    let Some(session_id) = init.session_id else {
        return;
    };
    // The conversation exists either way — that is what the record proves, and it is what makes
    // the next turn a `--resume`.
    shared.lock().established = true;
    // Which handle it is followed under is a different question. This is the one value in this
    // file a vendor process chooses and a later argv carries, so it is vetted like the resume
    // reference a host supplies: a handle beginning with `-` would be read by the CLI's parser as
    // a flag rather than as `--resume`'s value. An unrecognisable echo leaves the minted id in
    // force, which is the id this run was asked to write and the better of the two guesses.
    if crate::argv::is_vendor_session_id(&session_id) {
        shared.core_state.set_native_session_id(session_id);
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
    use super::{SessionState, TurnEnd, no_result_error, prompt_line};
    use mango_external_agents::{CancelReason, ExitStatus};

    /// A session dropped rather than closed must hand its artifact to the blocking pool, not run
    /// `remove_dir_all` on the thread that dropped it — a host root can be a FUSE or network mount.
    ///
    /// The pool is given one thread and that thread is occupied, so a handed-off removal cannot
    /// have run yet when the assertion reads the directory. A removal done in place would already
    /// be finished there, which is exactly the difference under test.
    #[test]
    fn a_session_dropped_without_a_close_removes_its_artifact_off_the_worker() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_all()
            .build()
            .expect("expected a runtime whose blocking pool this test owns");

        let scratch = std::env::temp_dir().join(format!("mea-drop-state-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&scratch).expect("expected a scratch root");

        runtime.block_on(async {
            let file = crate::mcp::ConfigFile::write(&servers(), &scratch)
                .await
                .expect("expected a written artifact")
                .expect("expected servers to produce one");
            let directory = file
                .path()
                .parent()
                .expect("a file sits in a directory")
                .to_path_buf();

            let (release_pool, pool_held) = std::sync::mpsc::channel::<()>();
            let occupied = tokio::task::spawn_blocking(move || {
                let _ = pool_held.recv();
            });

            // Field by field: `SessionState` implements `Drop` now, so struct-update syntax
            // cannot move the rest out of a default.
            let mut state = SessionState::default();
            state.mcp_config = Some(std::sync::Arc::new(file));
            drop(state);

            assert!(
                directory.exists(),
                "expected the removal to be waiting on the occupied pool, received {directory:?} already gone"
            );

            drop(release_pool);
            let _ = occupied.await;
            for _ in 0..50 {
                if !directory.exists() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(
                !directory.exists(),
                "expected the handed-off removal to finish, received {directory:?}"
            );
        });

        let _ = std::fs::remove_dir_all(&scratch);
    }

    fn servers() -> Vec<mango_external_agents::McpServer> {
        vec![mango_external_agents::McpServer {
            name: String::from("docs"),
            transport: mango_external_agents::McpTransport::Stdio {
                command: String::from("docs-mcp"),
                args: Vec::new(),
                env: std::collections::BTreeMap::new(),
            },
        }]
    }

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
                subject: "bytes of one vendor output line",
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
