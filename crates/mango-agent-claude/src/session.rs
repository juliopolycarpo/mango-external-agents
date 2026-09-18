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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use mango_external_agents::process::stop_process_with_limits;
use mango_external_agents::{
    CancelReason, CloseReason, Configuration, ConfigurationState, Dispatch, Error, ErrorCode,
    EventKind, EventSink, ExecutablePath, HostContext, PermissionResponse, Result,
    SessionLifecycle, SessionState as CoreSessionState, SessionStatus, StdioSpec, StopOutcome,
    TurnRequest, TurnStream, VendorError, event, transports::stdio,
};
use serde_json::json;

use crate::argv::TurnArgv;
use crate::cli_surface::CliSurface;
use crate::mcp::ConfigFile;
use crate::models;
use crate::permissions::{self, ModeAvailability};
use crate::pinned::{SIGTERM_EXIT_CODE, VENDOR_ENVIRONMENT_KEYS};
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
    /// Set only after native work has been reaped. A stopping owner stays admitted until the
    /// pump commits its terminal outcome (or its receiver is abandoned).
    stopped: bool,
    /// The terminal result has committed, or its receiver was abandoned. A reaped process alone
    /// cannot admit a replacement while its old stream still owns the terminal transition.
    settled: bool,
    teardown: Option<Arc<Teardown>>,
}

struct Teardown {
    result: Mutex<Option<std::result::Result<(), Arc<Error>>>>,
    done: tokio::sync::Notify,
    worker_started: AtomicBool,
}

impl Teardown {
    fn new() -> Self {
        Self {
            result: Mutex::new(None),
            done: tokio::sync::Notify::new(),
            worker_started: AtomicBool::new(false),
        }
    }

    async fn wait(&self) -> Result<()> {
        loop {
            let notified = self.done.notified();
            if let Some(result) = self
                .result
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_ref()
            {
                return match result {
                    Ok(()) => Ok(()),
                    Err(error) => Err(copy_error(error)),
                };
            }
            notified.await;
        }
    }

    fn starts_worker(&self) -> bool {
        !self.worker_started.swap(true, Ordering::AcqRel)
    }

    fn finish(&self, result: Result<()>) {
        *self.result.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(result.map_err(Arc::new));
        self.done.notify_waiters();
    }
}

/// Reconstructs the result a second close or cancel observes after the first caller has consumed
/// it. `Error` deliberately has no `Clone` because it may wrap another error, so teardown owns the
/// first value and makes a typed copy for each waiter.
fn copy_error(error: &Error) -> Error {
    match error {
        Error::Busy => Error::Busy,
        Error::Operation { dispatch, source } => copy_error(source).with_dispatch(*dispatch),
        Error::Vendor(error) => Error::Vendor(error.clone()),
        Error::NotSupported { capability } => Error::NotSupported {
            capability: *capability,
        },
        Error::UnsupportedTransport { harness, transport } => Error::UnsupportedTransport {
            harness: harness.clone(),
            transport: *transport,
        },
        Error::VersionGate { found, minimum } => Error::VersionGate {
            found: found.clone(),
            minimum: minimum.clone(),
        },
        Error::AuthRequired { login_hint } => Error::AuthRequired {
            login_hint: login_hint.clone(),
        },
        Error::Launch { program, message } => Error::Launch {
            program: program.clone(),
            message: message.clone(),
        },
        Error::Link { peer, message } => Error::Link {
            peer: peer.clone(),
            message: message.clone(),
        },
        Error::LimitExceeded {
            subject,
            limit,
            received,
        } => Error::LimitExceeded {
            subject,
            limit: *limit,
            received: *received,
        },
        Error::InvalidVendorValue { field, received } => Error::InvalidVendorValue {
            field,
            received: received.clone(),
        },
        Error::Protocol { expected, received } => Error::Protocol {
            expected: expected.clone(),
            received: received.clone(),
        },
        Error::Timeout { operation, after } => Error::Timeout {
            operation: operation.clone(),
            after: *after,
        },
        Error::Cancelled { reason } => Error::Cancelled { reason: *reason },
        Error::Closed { subject } => Error::Closed { subject },
        Error::HostConfiguration { expected, received } => Error::HostConfiguration {
            expected,
            received: received.clone(),
        },
        _ => Error::HostConfiguration {
            expected: "a replayable Claude teardown failure",
            received: String::from("an error variant this harness does not retain"),
        },
    }
}

/// An active attempt synchronously claimed for teardown.
///
/// The attempt has already recorded the caller's reason before this value is returned. `control`
/// is absent only while a start is still awaiting the launcher; that start observes the recorded
/// reason once its child exists and becomes responsible for stopping the child it created.
struct TakenTurn {
    end: Arc<TurnEnd>,
    control: Option<Arc<dyn mango_external_agents::ProcessControl>>,
}

/// The synchronous part of a close transition, before its durable worker starts.
enum CloseClaim {
    Existing(Option<Arc<Teardown>>),
    New {
        close: Arc<Teardown>,
        active: Option<Arc<Teardown>>,
        taken: Option<TakenTurn>,
        mcp_config: crate::mcp::Prepared<Arc<ConfigFile>>,
    },
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
    /// A forced termination can leave Claude's native conversation positioned at an unfinished
    /// prompt. The harness never silently selects a different native id for a strict session;
    /// callers receive an explicit refusal until they open a fresh session themselves.
    nonresumable: Option<CancelReason>,
    /// The one close worker that owns the session artifact and publishes its final state.
    close_teardown: Option<Arc<Teardown>>,
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
    /// Runtime that opened the session, retained for `Drop` paths which can run on another
    /// thread after the caller has left that runtime's context.
    runtime: Option<tokio::runtime::Handle>,
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
            nonresumable: None,
            close_teardown: None,
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
                runtime: tokio::runtime::Handle::try_current().ok(),
            }),
        }
    }

    /// Stops whatever turn is running, for one reason.
    ///
    /// The guard is taken and released in a statement of its own before anything is awaited: a
    /// `Session` method that held this lock across an await would deadlock the moment the turn
    /// channel filled, which is exactly when a host is least able to do anything about it.
    async fn stop_active_turn(&self, reason: CancelReason) -> Result<()> {
        let taken = request_stop(&mut self.shared.lock(), reason);
        let Some(taken) = taken else {
            return wait_existing_teardown(&self.shared).await;
        };
        // A reservation with no control has no native process yet. Its start guard owns the child
        // if one arrives; claiming it stopped here would free or taint the wrong lifecycle.
        if taken.control.is_none() {
            return Ok(());
        }
        await_durable_stop(Arc::clone(&self.shared), taken, reason, false).await
    }
}

/// Starts teardown before exposing an await. Dropping the caller detaches this task, leaving the
/// process control owned until it reaps or reports its bounded failure.
async fn await_durable_stop(
    shared: Arc<Shared>,
    taken: TakenTurn,
    reason: CancelReason,
    settled: bool,
) -> Result<()> {
    let teardown = install_teardown(&shared, &taken.end);
    start_teardown(&shared, &teardown.state, taken, reason, settled);
    teardown.state.wait().await
}

struct InstalledTeardown {
    state: Arc<Teardown>,
}
fn install_teardown(shared: &Shared, end: &TurnEnd) -> InstalledTeardown {
    let mut state = shared.lock();
    let active = state
        .active
        .as_mut()
        .filter(|a| std::ptr::eq(a.end.as_ref(), end))
        .expect("active teardown owner");
    if let Some(existing) = &active.teardown {
        return InstalledTeardown {
            state: Arc::clone(existing),
        };
    }
    let created = Arc::new(Teardown::new());
    active.teardown = Some(Arc::clone(&created));
    InstalledTeardown { state: created }
}
async fn wait_existing_teardown(shared: &Shared) -> Result<()> {
    let teardown = {
        let state = shared.lock();
        state
            .active
            .as_ref()
            .and_then(|a| a.teardown.as_ref())
            .cloned()
    };
    match teardown {
        Some(t) => t.wait().await,
        None => Ok(()),
    }
}

/// Detaches a stop before a caller reaches its first await. One owner can start the native work;
/// every other caller waits on its retained outcome.
fn start_teardown(
    shared: &Arc<Shared>,
    teardown: &Arc<Teardown>,
    taken: TakenTurn,
    reason: CancelReason,
    settled: bool,
) {
    if !teardown.starts_worker() {
        return;
    }
    let shared_for_worker = Arc::clone(shared);
    let teardown_for_worker = Arc::clone(teardown);
    spawn_owned(shared, async move {
        run_teardown(
            &shared_for_worker,
            &teardown_for_worker,
            taken,
            reason,
            settled,
        )
        .await;
    });
}

/// Performs exactly one bounded native stop for a claimed child and retains its typed outcome.
async fn run_teardown(
    shared: &Shared,
    teardown: &Teardown,
    taken: TakenTurn,
    reason: CancelReason,
    settled: bool,
) {
    let outcome = end_turn(taken.control, reason, shared.host.limits()).await;
    if let Ok(outcome) = outcome {
        record_stop(
            shared,
            &taken.end,
            !matches!(outcome, Some(StopOutcome::Interrupted)),
            reason,
            settled,
        );
    }
    teardown.finish(outcome.map(drop));
}

/// Starts an already-installed close teardown when a start that was pending at close finally
/// receives its child. The worker survives the start caller that happens to observe it.
fn start_late_teardown(
    shared: &Arc<Shared>,
    end: &Arc<TurnEnd>,
    control: Arc<dyn mango_external_agents::ProcessControl>,
    reason: CancelReason,
    settled: bool,
) -> Option<Arc<Teardown>> {
    let teardown = {
        let state = shared.lock();
        state
            .active
            .as_ref()
            .filter(|active| Arc::ptr_eq(&active.end, end))
            .and_then(|active| active.teardown.as_ref())
            .cloned()
    }?;
    start_teardown(
        shared,
        &teardown,
        TakenTurn {
            end: Arc::clone(end),
            control: Some(control),
        },
        reason,
        settled,
    );
    Some(teardown)
}

/// Releases a close waiting on a launcher that failed before it returned a child. No native work
/// existed, so this must not set `stopped` or taint the resumable Claude conversation.
fn finish_unstarted_teardown(shared: &Shared, end: &Arc<TurnEnd>) {
    let teardown = {
        let state = shared.lock();
        state
            .active
            .as_ref()
            .filter(|active| Arc::ptr_eq(&active.end, end))
            .and_then(|active| active.teardown.as_ref())
            .cloned()
    };
    if let Some(teardown) = teardown
        && teardown.starts_worker()
    {
        teardown.finish(Ok(()));
    }
}

/// Completes one close after its active child and MCP artifact are both gone.
async fn run_close_teardown(
    shared: &Shared,
    close: &Teardown,
    active: Option<Arc<Teardown>>,
    mut mcp_config: crate::mcp::Prepared<Arc<ConfigFile>>,
) {
    let native = match active {
        Some(teardown) => teardown.wait().await,
        None => Ok(()),
    };
    let result = match native {
        Ok(()) => {
            let result = crate::mcp::remove_on_close(mcp_config.take()).await;
            // Close is terminal after native cleanup, even when the host's scratch mount refuses
            // the final unlink. The typed error still tells the caller which resource remained.
            shared.core_state.set_status(SessionStatus::Closed);
            result
        }
        Err(error) => {
            crate::mcp::preserve_after_failed_native_cleanup(mcp_config);
            Err(error)
        }
    };
    close.finish(result);
}

impl Drop for ClaudeSession {
    /// Stops an active child when the last owning session handle disappears.
    ///
    /// The stream pump holds `Shared`, not `ClaudeSession`, so waiting for `Shared::drop` would
    /// leave a running child alive precisely when no host session can cancel it. The synchronous
    /// claim ensures a pump that is already completing cannot be stopped twice.
    fn drop(&mut self) {
        let taken = {
            let mut state = self.shared.lock();
            request_stop(&mut state, CancelReason::Shutdown).or_else(|| active_turn(&state))
        };
        let Some(taken) = taken else {
            return;
        };
        spawn_teardown(&self.shared, taken, CancelReason::Shutdown);
    }
}

/// Takes whatever turn is active and records why it stopped, under the same guard that takes it.
///
/// A turn's slot is reserved before its child is spawned, so a stop can land while `control` is
/// still `None` — nothing to kill yet, but the reason must not be lost. Recording it under the
/// owner lock makes that reason visible to the spawn once it returns; only the actual kill happens
/// after the guard is released.
fn request_stop(state: &mut Mutable, reason: CancelReason) -> Option<TakenTurn> {
    let active = state.active.as_mut()?;
    if active.end.set(reason).is_err() {
        return None;
    }
    Some(TakenTurn {
        end: Arc::clone(&active.end),
        control: active.control.as_ref().map(Arc::clone),
    })
}

/// Clones the stopping owner so a close or final drop can finish cleanup begun by a caller that
/// abandoned its cancellation future. `stop_process_with_limits` is idempotent at the process
/// boundary; this never clears ownership itself.
fn active_turn(state: &Mutable) -> Option<TakenTurn> {
    let active = state.active.as_ref()?;
    Some(TakenTurn {
        end: Arc::clone(&active.end),
        control: active.control.as_ref().map(Arc::clone),
    })
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
    spawn: Option<tokio::task::JoinHandle<Result<stdio::StdioTransport>>>,
    /// Installed before any cleanup await so an abort transfers this child to `Drop`.
    control: Option<Arc<dyn mango_external_agents::ProcessControl>>,
    /// Kept until the late child is reaped when a dropped start races a session close.
    mcp_lease: crate::mcp::Prepared<Arc<ConfigFile>>,
    armed: bool,
}

impl AbandonedStart {
    /// Hands responsibility for the child to whoever comes next.
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Waits for the owned launcher task, so dropping this future cannot cancel a spawn that may
    /// still hand a live process back later.
    async fn spawn(&mut self) -> Result<stdio::StdioTransport> {
        let result = self
            .spawn
            .as_mut()
            .expect("expected one owned Claude spawn task")
            .await;
        self.spawn = None;
        result.map_err(|_| Error::Launch {
            program: String::from(PROGRAM),
            message: String::from("the owned Claude spawn task was cancelled"),
        })?
    }
}

impl Drop for AbandonedStart {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // `ProcessControl::kill` documents one stop request. Once a child has been installed, a
        // prior stop owns that process; a pending launcher remains this guard's responsibility.
        let spawn = self.spawn.take();
        let owned_control = self.control.take();
        let taken = {
            let state = self.shared.lock();
            let active = state
                .active
                .as_ref()
                .filter(|active| Arc::ptr_eq(&active.end, &self.end));
            active.and_then(|active| {
                let reason = active.end.get().copied().unwrap_or(CancelReason::Requested);
                let newly_stopped = active.end.set(reason).is_ok();
                // A pending launcher task is always this guard's responsibility, even when
                // `cancel` or `close` already recorded the reason while it was spawning. Once
                // the launcher handed us a child, an existing stop owns that process and must
                // not receive a second signal from this drop path.
                (spawn.is_some() || owned_control.is_some() && !active.stopped || newly_stopped)
                    .then(|| TakenTurn {
                        end: Arc::clone(&active.end),
                        control: active.control.as_ref().map(Arc::clone),
                    })
            })
        };
        let Some(taken) = taken else {
            return;
        };
        let shared = Arc::clone(&self.shared);
        let reason = taken.end.get().copied().unwrap_or(CancelReason::Requested);
        let lease = self.mcp_lease.take();
        spawn_owned(&self.shared, async move {
            let control = match spawn {
                Some(task) => match task.await {
                    Ok(Ok(transport)) => Some(transport.control),
                    Ok(Err(_)) | Err(_) => None,
                },
                None => owned_control.or(taken.control),
            };
            let Some(control) = control else {
                finish_unstarted_teardown(&shared, &taken.end);
                crate::mcp::release_off_worker(lease).await;
                return;
            };
            if start_late_teardown(&shared, &taken.end, Arc::clone(&control), reason, true)
                .is_some()
            {
                crate::mcp::release_off_worker(lease).await;
                return;
            }
            let outcome = end_turn(Some(control), reason, shared.host.limits()).await;
            crate::mcp::release_off_worker(lease).await;
            if let Ok(outcome) = outcome {
                record_stop(
                    &shared,
                    &taken.end,
                    !matches!(outcome, Some(StopOutcome::Interrupted)),
                    reason,
                    true,
                );
            }
        });
    }
}

/// Ends a child requested through [`request_stop`], if it had one yet. The pump writes events.
async fn end_turn(
    control: Option<Arc<dyn mango_external_agents::ProcessControl>>,
    reason: CancelReason,
    limits: &mango_external_agents::Limits,
) -> Result<Option<StopOutcome>> {
    if let Some(control) = control {
        return stop_process_with_limits(control.as_ref(), reason, limits)
            .await
            .map(Some);
    }
    Ok(None)
}

/// Records that native work has ended without allowing an older attempt to touch a newer slot.
fn record_stop(
    shared: &Shared,
    end: &TurnEnd,
    taint_continuation: bool,
    reason: CancelReason,
    settled: bool,
) {
    let mut state = shared.lock();
    let should_clear = if let Some(active) = state
        .active
        .as_mut()
        .filter(|active| std::ptr::eq(active.end.as_ref(), end))
    {
        active.stopped = true;
        active.settled |= settled;
        active.settled
    } else {
        false
    };
    // This belongs in the same critical section as clearing the owner: otherwise a pump can
    // retire the slot between the reap and its forced-stop taint.
    if taint_continuation {
        state.nonresumable = Some(reason);
    }
    if should_clear {
        state.active = None;
    }
}

/// Records a terminal commit (or receiver abandonment) and releases an already reaped owner.
fn settle_owner(shared: &Shared, end: &Arc<TurnEnd>) {
    let mut state = shared.lock();
    if state
        .active
        .as_mut()
        .filter(|active| Arc::ptr_eq(&active.end, end))
        .is_some_and(|active| {
            active.settled = true;
            active.stopped
        })
    {
        state.active = None;
    }
}

/// Schedules a stop from a synchronous ownership-release path.
fn spawn_teardown(shared: &Arc<Shared>, taken: TakenTurn, reason: CancelReason) {
    let limits = *shared.host.limits();
    if let Some(runtime) = shared
        .runtime
        .as_ref()
        .cloned()
        .or_else(|| tokio::runtime::Handle::try_current().ok())
    {
        let shared = Arc::clone(shared);
        runtime.spawn(async move {
            if let Ok(outcome) = end_turn(taken.control, reason, &limits).await {
                record_stop(
                    &shared,
                    &taken.end,
                    !matches!(outcome, Some(StopOutcome::Interrupted)),
                    reason,
                    false,
                );
            }
        });
    }
}

/// Runs a synchronous-owner cleanup on the runtime retained by the session.
fn spawn_owned<F>(shared: &Arc<Shared>, cleanup: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Some(runtime) = shared
        .runtime
        .as_ref()
        .cloned()
        .or_else(|| tokio::runtime::Handle::try_current().ok())
    {
        runtime.spawn(cleanup);
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
        self.validate_turn_request(&request)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        if !request.attachments.is_empty() {
            // Claude Code's stream-json input takes content blocks, but nothing here encodes one
            // and `Capabilities::images` is false. Refusing is the honest answer: silently
            // dropping a file the user attached would run a turn about something that was never
            // sent.
            return Err(Error::HostConfiguration {
                expected: "a turn with no attachments, which this harness does not forward",
                received: format!("{} attachments", request.attachments.len()),
            }
            .with_dispatch(Dispatch::NotSubmitted));
        }

        let requested_configuration = request.configuration.clone();
        if let Some(patch) = &requested_configuration {
            // Same rule as opening: a fresh child is the only place Claude's mode and model land,
            // so there is no argv this harness could encode that un-sets one mid-session.
            mango_external_agents::configuration::refuse_unsupported_native(patch)
                .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
            mango_external_agents::configuration::refuse_unsupported_reset(patch)
                .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        }
        // Read from `requested`, not `accepted`: opening starts nothing, so the first turn has
        // nothing accepted to inherit yet, and every later turn's `requested` already carries
        // forward whatever the last explicit ask resolved to — see the update below.
        let current = self
            .shared
            .core_state
            .snapshot()
            .configuration
            .requested
            .clone();
        let configuration = requested_configuration
            .as_ref()
            .map_or_else(|| current.clone(), |patch| current.patched(patch));
        let mode = self
            .shared
            .resolve_mode(&configuration)
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;
        // Configuration is caller-owned and becomes a value position after a Claude option.
        // Reject it before reserving a child: omitting an invalid explicit value would run the
        // turn under a setting the host did not select.
        models::validate_configuration(&configuration, self.shared.surface.as_ref())
            .map_err(|error| error.with_dispatch(Dispatch::NotSubmitted))?;

        // Admission is a refusal boundary. A second request never silently replaces, queues, or
        // cancels the work that already owns this session: the caller that wants to stop work
        // says so through `cancel` and waits for the resulting terminal state. The slot is
        // nevertheless reserved before spawning, so a stop landing while `stdio::open` is
        // pending has a reason to record against rather than an empty window to race through.
        let end = Arc::new(TurnEnd::default());
        let mut mcp_lease = {
            let Some(_lifecycle) = self.shared.lifecycle.begin_start() else {
                return Err(
                    Error::Closed { subject: "session" }.with_dispatch(Dispatch::NotSubmitted)
                );
            };
            if self.shared.host.cancel().is_cancelled() {
                return Err(Error::Cancelled {
                    reason: CancelReason::Shutdown,
                }
                .with_dispatch(Dispatch::NotSubmitted));
            }
            let mut state = self.shared.lock();
            if let Some(reason) = state.nonresumable {
                return Err(Error::Cancelled { reason }.with_dispatch(Dispatch::NotSubmitted));
            }
            if state.active.is_some() {
                return Err(Error::Busy.with_dispatch(Dispatch::NotSubmitted));
            }
            state.active = Some(ActiveTurn {
                end: Arc::clone(&end),
                control: None,
                stopped: false,
                settled: false,
                teardown: None,
            });
            // The reservation and this clone share the same critical section. A close that wins
            // after it can release the session's reference, but this attempt still owns the file
            // until it has either installed or reaped the child it launches.
            // In a cancellation-safe owner for the same reason `open_session`'s artifact is:
            // a caller that drops this future between here and the return reaches no explicit
            // release, and a close that won the race leaves this clone the last reference.
            crate::mcp::Prepared::new(state.mcp_config.as_ref().map(Arc::clone))
        };
        // Armed at admission, before the launcher future exists. If the caller drops while the
        // host is still spawning, this guard keeps that spawn owned and reaps the late child.
        let mut abandoned = AbandonedStart {
            shared: Arc::clone(&self.shared),
            end: Arc::clone(&end),
            spawn: None,
            control: None,
            mcp_lease: crate::mcp::Prepared::new(mcp_lease.get().cloned()),
            armed: true,
        };

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
        let mcp_config = abandoned
            .mcp_lease
            .get()
            .map(|file| file.argument().to_owned());
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
                abandoned.disarm();
                finish_unstarted_teardown(&self.shared, &end);
                forget_reservation(&self.shared, &end);
                // A close that won the race already released the session's own reference, which
                // makes this lease the last one and its drop the `remove_dir_all`. Off the worker,
                // for the same reason the write is.
                crate::mcp::release_off_worker(mcp_lease.take()).await;
                crate::mcp::release_off_worker(abandoned.mcp_lease.take()).await;
                return Err(error.with_dispatch(Dispatch::NotSubmitted));
            }
        };

        let host = self.shared.host.clone();
        let executable = self.shared.executable.clone();
        abandoned.spawn = Some(tokio::spawn(async move {
            stdio::open(
                &host,
                &StdioSpec::new(argv),
                &executable,
                VENDOR_ENVIRONMENT_KEYS,
            )
            .await
        }));
        let transport = match abandoned.spawn().await {
            Ok(transport) => transport,
            // No child ever launched, so there is nothing to reap here — only the reservation
            // this call made above, and only if a stop has not already taken it.
            Err(error) => {
                abandoned.disarm();
                finish_unstarted_teardown(&self.shared, &end);
                forget_reservation(&self.shared, &end);
                crate::mcp::release_off_worker(mcp_lease.take()).await;
                crate::mcp::release_off_worker(abandoned.mcp_lease.take()).await;
                return Err(error);
            }
        };

        // The lease is still held past `stdio::open`, deliberately: `close` may have taken the
        // session's `Arc` while the launcher awaited, but it cannot remove the file until this
        // call either installs the child or kills the stopped child below. Installing hands the
        // artifact back to the session, and this clone is then the cheap one to drop.
        let (sink, events) = EventSink::with_limits(
            self.shared.core_state.snapshot().ids.session_id.clone(),
            request.turn_id.clone(),
            request.attempt,
            Arc::clone(self.shared.host.clock()),
            self.shared.host.limits(),
        );
        let control = Arc::clone(&transport.control);
        abandoned.control = Some(Arc::clone(&control));
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
                && end.get().is_none()
            {
                state.active = Some(ActiveTurn {
                    end: Arc::clone(&end),
                    control: Some(Arc::clone(&control)),
                    stopped: false,
                    settled: false,
                    teardown: None,
                });
                None
            } else {
                end.get().copied()
            }
        };
        if let Some(reason) = stopped {
            if let Some(teardown) =
                start_late_teardown(&self.shared, &end, Arc::clone(&control), reason, true)
            {
                // The close worker owns the session lease until this detached native stop reports
                // success. These start-local clones can leave now without exposing the file to a
                // child that is still in the stop gate.
                crate::mcp::release_off_worker(mcp_lease.take()).await;
                crate::mcp::release_off_worker(abandoned.mcp_lease.take()).await;
                teardown
                    .wait()
                    .await
                    .map_err(|error| error.with_dispatch(Dispatch::Accepted))?;
                abandoned.disarm();
                return Err((if self.shared.lifecycle.is_closed() {
                    Error::Closed { subject: "session" }
                } else {
                    Error::Cancelled { reason }
                })
                .with_dispatch(Dispatch::Accepted));
            }
            let outcome =
                stop_process_with_limits(control.as_ref(), reason, self.shared.host.limits()).await;
            // After the kill, never before: the child read `--mcp-config` at startup. And off the
            // worker, because a close that won the race left this lease holding the last
            // reference, so this is where the `remove_dir_all` happens.
            crate::mcp::release_off_worker(mcp_lease.take()).await;
            crate::mcp::release_off_worker(abandoned.mcp_lease.take()).await;
            let outcome = outcome.map_err(|error| error.with_dispatch(Dispatch::Accepted))?;
            record_stop(
                &self.shared,
                &end,
                matches!(outcome, StopOutcome::Terminated),
                reason,
                true,
            );
            return Err((if self.shared.lifecycle.is_closed() {
                Error::Closed { subject: "session" }
            } else {
                Error::Cancelled { reason }
            })
            .with_dispatch(Dispatch::Accepted));
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
        crate::mcp::release_off_worker(mcp_lease.take()).await;
        crate::mcp::release_off_worker(abandoned.mcp_lease.take()).await;

        // Re-checked for the same reason the guard above re-checks after `stdio::open`: the
        // release is an await, so a `close`, a `cancel` or a second `start_turn` can have taken
        // this turn while it was parked there. Such a stop records its reason and kills the child,
        // and spawning the pump afterwards would write the prompt to a child somebody is killing
        // and hand the caller a successful stream for a turn that is already over. No stream has
        // been handed out yet, so refusing here is still the honest answer.
        let stopped = {
            let lifecycle = self.shared.lifecycle.lock();
            let state = self.shared.lock();
            if lifecycle.is_closed()
                || !state
                    .active
                    .as_ref()
                    .is_some_and(|active| Arc::ptr_eq(&active.end, &end))
                || end.get().is_some()
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
                // The successful CLI spawn does not settle this boundary by itself: releasing
                // the MCP lease above awaits, so a close, cancel, or newer turn can still refuse
                // this attempt. Only now is the argv configuration accepted. An unconfigured
                // turn inherits accepted settings without replacing the last explicit request.
                let requested = if requested_configuration.is_some() {
                    configuration.clone()
                } else {
                    current
                };
                self.shared
                    .core_state
                    .set_configuration(ConfigurationState::new(
                        requested,
                        configuration.clone(),
                        Configuration::unknown(),
                    ));
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
            // No stream reached the caller, so no pump will retire an owner whose stop finished
            // while the MCP lease was releasing. The helper retains a still-stopping owner.
            settle_owner(&self.shared, &end);
            return Err((if self.shared.lifecycle.is_closed() {
                Error::Closed { subject: "session" }
            } else {
                Error::Cancelled { reason }
            })
            .with_dispatch(Dispatch::Accepted));
        }

        // The pump owns the child from here, after the turn-start event is enqueued.
        // `claude --print` names no turn, so the handle is the host's own id: a value the host can
        // reproduce, which is what a retry needs. Emitted here, before the pump task exists to
        // race it with anything the vendor says, so it is always the first thing on the stream —
        // including on a run whose own `system/init` never arrives.
        let native_turn_id = request.turn_id.as_str().to_owned();
        if let Err(error) = sink
            .emit(EventKind::TurnStarted {
                native_turn_id: native_turn_id.clone(),
            })
            .await
        {
            // The child is already running and installed in `state.active` by this point, so an
            // id too strange for `EventKind::normalized` to keep — an id `TurnId::new` never
            // validates — must not plant a live process nobody holds a handle to. Reaped the same
            // way a stop landing in this window is.
            let stopped = stop_process_with_limits(
                control.as_ref(),
                CancelReason::Requested,
                self.shared.host.limits(),
            )
            .await;
            if stopped.is_ok() {
                record_stop(&self.shared, &end, true, CancelReason::Requested, true);
            }
            abandoned.disarm();
            return Err(error.with_dispatch(Dispatch::Accepted));
        }
        abandoned.disarm();
        abandoned.control = None;
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
        self.stop_active_turn(reason).await
    }

    async fn close(&self, reason: CloseReason) -> Result<()> {
        // The first close installs a durable worker before either caller can await. Later closes
        // observe that worker's result instead of reporting success while its child still runs.
        let claim = {
            let mut lifecycle = self.shared.lifecycle.lock();
            if !lifecycle.close() {
                CloseClaim::Existing(self.shared.lock().close_teardown.as_ref().cloned())
            } else {
                let mut state = self.shared.lock();
                let taken = request_stop(&mut state, CancelReason::from(reason))
                    .or_else(|| active_turn(&state));
                let active = state.active.as_mut().map(|turn| {
                    Arc::clone(
                        turn.teardown
                            .get_or_insert_with(|| Arc::new(Teardown::new())),
                    )
                });
                let close = Arc::new(Teardown::new());
                state.close_teardown = Some(Arc::clone(&close));
                CloseClaim::New {
                    close,
                    active,
                    taken,
                    mcp_config: crate::mcp::Prepared::new(state.mcp_config.take()),
                }
            }
        };
        let (close, active, taken, mcp_config) = match claim {
            CloseClaim::Existing(Some(teardown)) => return teardown.wait().await,
            CloseClaim::Existing(None) => return Ok(()),
            CloseClaim::New {
                close,
                active,
                taken,
                mcp_config,
            } => (close, active, taken, mcp_config),
        };
        self.shared.core_state.set_status(SessionStatus::Closing);
        if let (Some(active), Some(taken)) = (&active, taken)
            && taken.control.is_some()
        {
            start_teardown(
                &self.shared,
                active,
                taken,
                CancelReason::from(reason),
                false,
            );
        }
        let shared = Arc::clone(&self.shared);
        let close_for_worker = Arc::clone(&close);
        spawn_owned(&self.shared, async move {
            run_close_teardown(&shared, &close_for_worker, active, mcp_config).await;
        });
        close.wait().await
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
    let idle_timeout = shared.host.limits().idle_timeout;
    let (mut sender, mut receiver) = link.split();

    // One message, then end of input. A second message would run as its own turn with its own
    // result, which is a queued follow-up rather than steering — see `Capabilities::steering`.
    // Ending the input is also what the vendor documents as cancelling a pending prompt, so a run
    // that would have waited for an answer nobody can give stops waiting.
    let written = sender.send(prompt_line(&input)).await;
    let closed = sender.close().await;
    if let Err(error) = written.and(closed) {
        finish(
            &shared,
            &mut reducer,
            &sink,
            &end,
            &control,
            Some(error),
            exit_grace,
        )
        .await;
        settle_owner(&shared, &end);
        return;
    }

    let cancel_token = shared.host.cancel().clone();
    let mut failure = None;
    loop {
        let line = tokio::select! {
            () = sink.closed() => {
                if let Some(taken) = take_matching_turn(&shared, &end, CancelReason::Requested)
                    && let Ok(outcome) = end_turn(taken.control, CancelReason::Requested, shared.host.limits()).await
                {
                    record_stop(
                        &shared,
                        &taken.end,
                        !matches!(outcome, Some(StopOutcome::Interrupted)),
                        CancelReason::Requested,
                        true,
                    );
                }
                settle_owner(&shared, &end);
                return;
            }
            () = cancel_token.cancelled() => {
                if let Some(taken) = take_matching_turn(&shared, &end, CancelReason::Shutdown)
                    && let Ok(outcome) = end_turn(taken.control, CancelReason::Shutdown, shared.host.limits()).await
                {
                    record_stop(
                        &shared,
                        &taken.end,
                        !matches!(outcome, Some(StopOutcome::Interrupted)),
                        CancelReason::Shutdown,
                        false,
                    );
                }
                break;
            }
            received = tokio::time::timeout(idle_timeout, receiver.recv()) => received,
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
                    after: idle_timeout,
                });
                break;
            }
        };

        let Some(record) = StreamRecord::parse(&line) else {
            continue;
        };
        let reduction = reducer.reduce(&record);
        if let Some(init) = reduction.init {
            apply_init(&shared, &end, init);
        }
        for event in reduction.events {
            match sink.emit(event).await {
                Ok(()) => {}
                // The host dropped the stream. There is nobody to tell, and a turn nobody is
                // reading is a turn to stop feeding.
                Err(Error::Closed { .. }) => {
                    if let Some(taken) = take_matching_turn(&shared, &end, CancelReason::Requested)
                        && let Ok(outcome) =
                            end_turn(taken.control, CancelReason::Requested, shared.host.limits())
                                .await
                    {
                        record_stop(
                            &shared,
                            &taken.end,
                            !matches!(outcome, Some(StopOutcome::Interrupted)),
                            CancelReason::Requested,
                            true,
                        );
                    }
                    settle_owner(&shared, &end);
                    return;
                }
                // EventSink commits its bounded overflow terminal before returning this error.
                // No later vendor frame may follow that terminal, and a process whose output the
                // stream rejected must not keep working for an absent consumer.
                Err(_) if sink.is_terminal() => {
                    if let Some(taken) = take_matching_turn(&shared, &end, CancelReason::Requested)
                        && let Ok(outcome) =
                            end_turn(taken.control, CancelReason::Requested, shared.host.limits())
                                .await
                    {
                        record_stop(
                            &shared,
                            &taken.end,
                            !matches!(outcome, Some(StopOutcome::Interrupted)),
                            CancelReason::Requested,
                            true,
                        );
                    }
                    settle_owner(&shared, &end);
                    return;
                }
                Err(_) => {}
            }
        }
        if reducer.finished() {
            break;
        }
    }

    finish(
        &shared,
        &mut reducer,
        &sink,
        &end,
        &control,
        failure,
        exit_grace,
    )
    .await;
    settle_owner(&shared, &end);
}

/// Writes the turn's terminal event, whichever way it ended, and reaps the child.
async fn finish(
    shared: &Shared,
    reducer: &mut TurnReducer,
    sink: &EventSink,
    end: &TurnEnd,
    control: &Arc<dyn mango_external_agents::ProcessControl>,
    failure: Option<Error>,
    exit_grace: Duration,
) {
    let native_finished = reducer.finished();
    if native_finished {
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
    // A caller that took an active turn already owns its stop request. Asking the launcher again
    // would violate ProcessControl's one-stop contract; a native result or a broken stream has no
    // such owner and still needs the post-terminal process-tree reap.
    let abnormal = !native_finished && end.get().is_none();
    if (native_finished || abnormal)
        && let Ok(outcome) = stop_process_with_limits(
            control.as_ref(),
            CancelReason::Shutdown,
            shared.host.limits(),
        )
        .await
    {
        // A terminal without Claude's own `result` leaves the native conversation at an
        // unknown point. Do not resume it merely because the harness contained the process.
        record_stop(
            shared,
            end,
            abnormal || matches!(outcome, StopOutcome::Terminated) && !native_finished,
            CancelReason::Shutdown,
            false,
        );
    }
}

/// Releases a reservation that never handed the launcher a child.
fn forget_reservation(shared: &Shared, end: &Arc<TurnEnd>) {
    let mut state = shared.lock();
    if state
        .active
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(&active.end, end) && active.control.is_none())
    {
        state.active = None;
    }
}

/// Takes this exact attempt for teardown, leaving a newer attempt untouched.
fn take_matching_turn(
    shared: &Shared,
    end: &Arc<TurnEnd>,
    reason: CancelReason,
) -> Option<TakenTurn> {
    let mut state = shared.lock();
    if !state
        .active
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(&active.end, end))
    {
        return None;
    }
    request_stop(&mut state, reason)
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
fn apply_init(shared: &Shared, end: &Arc<TurnEnd>, init: RunInit) {
    // Buffered output from a stopped attempt must not re-establish its native continuation after
    // a retry has claimed the slot. Keep the admission check and each state publication under the
    // mutable state lock so cancel's reset cannot be overtaken between the check and the write.
    let mut state = shared.lock();
    if !state
        .active
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(&active.end, end))
    {
        return;
    }
    if let Some(commands) = init.commands {
        shared
            .core_state
            .set_commands(event::normalized_catalog(commands));
    }

    if let Some(model) = init.model {
        let configuration = shared.core_state.snapshot().configuration.clone();
        let observed = configuration.observed.clone().with_model(model);
        shared
            .core_state
            .set_configuration(configuration.with_observed(observed));
    }

    let Some(session_id) = init.session_id else {
        return;
    };
    // The conversation exists either way — that is what the record proves, and it is what makes
    // the next turn a `--resume`.
    state.established = true;
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
    use super::{Mutable, TurnEnd, no_result_error, prompt_line};
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
            let mut state = Mutable::default();
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
