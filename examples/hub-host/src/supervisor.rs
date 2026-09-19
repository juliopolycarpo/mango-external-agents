//! The loop the library deliberately does not contain.
//!
//! `docs/lifecycle.md` says the library owns the retry *contract* — the fingerprint, the dispatch
//! certainty, the transition rules — and the host owns durable storage, network retries and
//! backoff. This is that host half, written against nothing but the library's public API.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use mango_external_agents::{
    CancelReason, Clock, Dispatch, Error, OperationRef, RecoveryAction, RecoveryRecord, Result,
    Session, SessionId, TerminalStatus, TurnId, TurnRequest, TurnStream,
};

use crate::hub::{Commit, HubApi, HubError, HubStatus, Reconciliation, RetryHint};
use crate::retry::{RetryPolicy, WaitOutcome};
use crate::stop::Stop;
use crate::subscriber::{TurnBroadcast, TurnSubscriber};

/// How many events one operation's fan-out holds for a watcher that has fallen behind.
const DEFAULT_EVENT_CAPACITY: usize = 256;

/// How one logical operation ended.
///
/// Five outcomes rather than a `Result`, because three of them are neither success nor failure:
/// a stopped operation, a refused one and an *uncertain* one are each something a caller has to
/// handle differently, and collapsing them into an error string is how a host ends up replaying
/// work it cannot prove did not run.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Settled {
    /// This run produced the terminal and committed it to the Hub.
    Committed {
        /// The outcome the vendor produced.
        terminal: TerminalStatus,
        /// What the Hub did with it. Always [`Commit::Recorded`]: a commit the Hub answered
        /// [`Commit::AlreadyRecorded`] is [`Settled::AlreadyCommitted`] instead, because the
        /// outcome to report is then the Hub's and not this run's.
        commit: Commit,
    },
    /// The Hub already held a terminal, so no outcome this run has in hand is the answer.
    ///
    /// Either the reconciliation found it before the vendor was asked again, or the commit did.
    /// The terminal is always the **Hub's**, which a host may not assume equals the one it was
    /// about to record: an earlier attempt that ended `Failed` outranks a later `Completed`.
    AlreadyCommitted {
        /// The outcome the Hub was holding.
        terminal: TerminalStatus,
    },
    /// Acceptance is unknown and this Hub has no reconciliation query.
    ///
    /// The caller must decide — ask a person, quarantine the operation, escalate. What it must not
    /// do is call [`Supervisor::run`] again with the same request and hope: nothing proved the
    /// work did not run, so a second dispatch may be a second execution.
    Uncertain {
        /// The attempt whose fate nothing can establish, for the caller to record.
        operation: OperationRef,
    },
    /// The Hub refused the operation for good.
    ///
    /// Remembered by the supervisor: running the same logical turn id again answers with this
    /// same refusal rather than reconciling or dispatching anything.
    Refused {
        /// What the Hub gave as its reason.
        reason: String,
    },
    /// The host stopped the operation.
    Stopped {
        /// Which of the three stops it was.
        reason: CancelReason,
    },
}

/// What one turn of the loop decided to do next.
enum Step {
    /// Make progress again immediately; the last step changed the record's state.
    Continue,
    /// A recoverable failure. Back off, honouring the Hub's hint when it gave one.
    Backoff(Option<RetryHint>),
    /// The operation is over.
    Settled(Settled),
}

/// How draining a turn's transcript ended.
enum Drained {
    /// The turn committed this terminal.
    Terminal(TerminalStatus),
    /// The transcript ended without one, so only the Hub can say what happened.
    Ended,
    /// The host stopped the operation mid-turn.
    Stopped,
}

/// The two signals one run answers to.
///
/// Shutdown is the session's: the owner going away ends every operation this supervisor has left,
/// which is what makes it the right scope for the injected [`Stop`]. An abort is one logical
/// turn's, and answering it with the session's signal is how a supervisor built to drive many
/// turns becomes single-use — the first abort settles every later run, for any turn id, as stopped
/// on that first reason before it dispatches anything.
struct RunStop {
    shutdown: Arc<Stop>,
    turn: Arc<Stop>,
}

impl RunStop {
    /// Why this run must end, if it must.
    ///
    /// The turn's own reason wins when both are pulled: it is the more specific of the two, and
    /// the one a person reading an audit row asked for.
    fn reason(&self) -> Option<CancelReason> {
        self.turn.reason().or_else(|| self.shutdown.reason())
    }

    /// Resolves once either signal is pulled.
    async fn stopped(&self) {
        tokio::select! {
            () = self.turn.stopped() => {}
            () = self.shutdown.stopped() => {}
        }
    }

    /// Waits out `delay`, ending early on either signal.
    ///
    /// The turn's signal goes through [`RetryPolicy::wait`] rather than a third arm here, so the
    /// policy's own cancellation-aware wait stays the thing being exercised.
    async fn wait(&self, policy: &RetryPolicy, delay: Duration) -> WaitOutcome {
        tokio::select! {
            // Biased for the same reason `RetryPolicy::wait` is: an already-pulled shutdown must
            // not lose a coin toss against a zero delay.
            biased;
            () = self.shutdown.stopped() => WaitOutcome::Stopped,
            outcome = policy.wait(delay, &self.turn) => outcome,
        }
    }
}

/// The mutable state of one `run`, kept out of the arms' parameter lists.
struct Progress {
    stop: RunStop,
    stream: Option<TurnStream>,
    /// Every recoverable failure this run has seen, never reset.
    ///
    /// Not *consecutive* failures. A Hub that refuses every reservation but answers every
    /// reconciliation would reset a consecutive counter on each reconciliation, and the host would
    /// hammer it at the base delay for as long as it stayed down.
    failures: u32,
    dispatched: bool,
    terminal_came_from_hub: bool,
}

/// A host supervisor that drives one session's logical operations to a committed terminal.
///
/// It owns the `Box<dyn Session>`, one [`RecoveryRecord`] per logical turn, the [`HubApi`] port
/// and the [`Stop`] signals. Watchers get a [`TurnSubscriber`], never the
/// [`TurnStream`]: dropping the stream is abandonment, and a
/// browser must not be able to abandon an operation by closing a tab.
///
/// Stopping has two scopes, because the two questions are different. The injected [`Stop`] is the
/// **session's**: the owner is going away and everything this supervisor has left is over.
/// [`Supervisor::abort_signal`] hands back one **logical turn's**, for a stop button or a revoked
/// consent that concerns a single operation. A supervisor with only the first cannot express the
/// second — one abort ends the session for every turn id it has not run yet.
pub struct Supervisor {
    inner: SupervisorInner,
    session_id: SessionId,
    records: HashMap<TurnId, LogicalTurn>,
    aborts: HashMap<TurnId, Arc<Stop>>,
}

/// Everything the supervisor remembers about one logical turn between runs.
///
/// The record carries what the library owns. The refusal is the host's half of the same question:
/// [`HubError::Refused`] is documented as terminal for the logical operation, and the record has
/// no state for "the control plane will never take this", because the control plane is not the
/// library's business. Without it the refusal lives only as long as the `Settled` value a caller
/// may drop, and the next run reconciles and dispatches work the Hub refused for good.
struct LogicalTurn {
    record: RecoveryRecord,
    refusal: Option<String>,
}

impl LogicalTurn {
    fn new(record: RecoveryRecord) -> Self {
        Self {
            record,
            refusal: None,
        }
    }
}

/// Everything one run borrows immutably, split out so the records never leave the map.
///
/// The split *is* the cancellation safety. A `run` that took its record out of the map, drove it
/// and put it back would hold the only copy on its own stack for the whole of every await, and a
/// dropped future would take it with it. Borrowing `&self.inner` and `&mut self.records` as
/// disjoint fields lets the loop mutate the record in place instead, so whatever it had recorded
/// when the future was dropped is what the next run reads.
struct SupervisorInner {
    session: Box<dyn Session>,
    hub: Arc<dyn HubApi>,
    policy: RetryPolicy,
    stop: Arc<Stop>,
    clock: Arc<dyn Clock>,
    events: TurnBroadcast,
}

impl Supervisor {
    /// A supervisor over one open session, with every collaborator injected.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeHubApi, FakeVendorSession, ScriptedJitter};
    /// use hub_host::{RetryPolicy, Stop, Supervisor};
    /// use mango_external_agents::SystemClock;
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let session = FakeVendorSession::new();
    /// let policy = RetryPolicy::new(
    ///     Duration::from_millis(10),
    ///     Duration::from_secs(1),
    ///     Duration::from_secs(5),
    ///     Arc::new(ScriptedJitter::maximum()),
    /// );
    /// let supervisor = Supervisor::new(
    ///     Box::new(session),
    ///     Arc::new(FakeHubApi::new()),
    ///     policy,
    ///     Arc::new(Stop::new()),
    ///     Arc::new(SystemClock),
    /// );
    /// assert_eq!(supervisor.subscriber_count(), 0);
    /// ```
    pub fn new(
        session: Box<dyn Session>,
        hub: Arc<dyn HubApi>,
        policy: RetryPolicy,
        stop: Arc<Stop>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let session_id = session.ids().session_id;
        Self {
            inner: SupervisorInner {
                session,
                hub,
                policy,
                stop,
                clock,
                events: TurnBroadcast::new(DEFAULT_EVENT_CAPACITY),
            },
            session_id,
            records: HashMap::new(),
            aborts: HashMap::new(),
        }
    }

    /// The abort signal for one logical turn, created the first time it is asked for.
    ///
    /// Taken **before** the run it belongs to: `run` borrows the supervisor exclusively, so the
    /// handle a caller uses to stop one operation has to be in hand before that operation starts.
    /// Pulling it ends that turn and nothing else; the session's own [`Stop`] is still what ends
    /// everything.
    ///
    /// One signal per logical turn id, kept for the supervisor's life, so an abort is **final for
    /// that turn id**: a later `run` of it settles as [`Settled::Stopped`] without reconciling or
    /// dispatching, the same way a refusal does. That is deliberate — an operation the owner
    /// stopped is not one a retry loop gets to resume — and it is why the signal is per logical
    /// turn rather than per run. A caller that wants the work after all gives it a new turn id,
    /// which is a new logical operation and is the honest way to say so.
    ///
    /// # Example
    ///
    /// ```
    /// # use hub_host::testing::{FakeHubApi, FakeVendorSession, ScriptedJitter};
    /// # use hub_host::{RetryPolicy, Stop, Supervisor};
    /// # use mango_external_agents::{CancelReason, SystemClock, TurnId};
    /// # use std::sync::Arc;
    /// # use std::time::Duration;
    /// # let policy = RetryPolicy::new(
    /// #     Duration::from_millis(10),
    /// #     Duration::from_secs(1),
    /// #     Duration::from_secs(5),
    /// #     Arc::new(ScriptedJitter::maximum()),
    /// # );
    /// # let mut supervisor = Supervisor::new(
    /// #     Box::new(FakeVendorSession::new()),
    /// #     Arc::new(FakeHubApi::new()),
    /// #     policy,
    /// #     Arc::new(Stop::new()),
    /// #     Arc::new(SystemClock),
    /// # );
    /// let aborting = supervisor.abort_signal(&TurnId::new("turn-1"));
    /// aborting.stop(CancelReason::Requested);
    /// // Asking twice answers the same signal, so an abort cannot be pulled on a stale handle.
    /// assert_eq!(
    ///     supervisor.abort_signal(&TurnId::new("turn-1")).reason(),
    ///     Some(CancelReason::Requested)
    /// );
    /// // A different logical turn is untouched.
    /// assert!(supervisor.abort_signal(&TurnId::new("turn-2")).reason().is_none());
    /// ```
    pub fn abort_signal(&mut self, turn_id: &TurnId) -> Arc<Stop> {
        Arc::clone(
            self.aborts
                .entry(turn_id.clone())
                .or_insert_with(|| Arc::new(Stop::new())),
        )
    }

    /// Attaches one watcher to everything this supervisor publishes.
    ///
    /// # Example
    ///
    /// ```
    /// # use hub_host::testing::{FakeHubApi, FakeVendorSession, ScriptedJitter};
    /// # use hub_host::{RetryPolicy, Stop, Supervisor};
    /// # use mango_external_agents::SystemClock;
    /// # use std::sync::Arc;
    /// # use std::time::Duration;
    /// # let policy = RetryPolicy::new(
    /// #     Duration::from_millis(10),
    /// #     Duration::from_secs(1),
    /// #     Duration::from_secs(5),
    /// #     Arc::new(ScriptedJitter::maximum()),
    /// # );
    /// # let supervisor = Supervisor::new(
    /// #     Box::new(FakeVendorSession::new()),
    /// #     Arc::new(FakeHubApi::new()),
    /// #     policy,
    /// #     Arc::new(Stop::new()),
    /// #     Arc::new(SystemClock),
    /// # );
    /// let watcher = supervisor.subscribe();
    /// assert_eq!(supervisor.subscriber_count(), 1);
    /// drop(watcher);
    /// assert_eq!(supervisor.subscriber_count(), 0);
    /// ```
    pub fn subscribe(&self) -> TurnSubscriber {
        self.inner.events.subscribe()
    }

    /// How many watchers are attached right now.
    ///
    /// # Example
    ///
    /// ```
    /// # use hub_host::testing::{FakeHubApi, FakeVendorSession, ScriptedJitter};
    /// # use hub_host::{RetryPolicy, Stop, Supervisor};
    /// # use mango_external_agents::SystemClock;
    /// # use std::sync::Arc;
    /// # use std::time::Duration;
    /// # let policy = RetryPolicy::new(
    /// #     Duration::from_millis(10),
    /// #     Duration::from_secs(1),
    /// #     Duration::from_secs(5),
    /// #     Arc::new(ScriptedJitter::maximum()),
    /// # );
    /// # let supervisor = Supervisor::new(
    /// #     Box::new(FakeVendorSession::new()),
    /// #     Arc::new(FakeHubApi::new()),
    /// #     policy,
    /// #     Arc::new(Stop::new()),
    /// #     Arc::new(SystemClock),
    /// # );
    /// assert_eq!(supervisor.subscriber_count(), 0);
    /// ```
    pub fn subscriber_count(&self) -> usize {
        self.inner.events.subscriber_count()
    }

    /// What the record for this logical turn currently says, for a host persisting alongside it.
    ///
    /// # Example
    ///
    /// ```
    /// # use hub_host::testing::{FakeHubApi, FakeVendorSession, ScriptedJitter};
    /// # use hub_host::{RetryPolicy, Stop, Supervisor};
    /// # use mango_external_agents::{SystemClock, TurnId};
    /// # use std::sync::Arc;
    /// # use std::time::Duration;
    /// # let policy = RetryPolicy::new(
    /// #     Duration::from_millis(10),
    /// #     Duration::from_secs(1),
    /// #     Duration::from_secs(5),
    /// #     Arc::new(ScriptedJitter::maximum()),
    /// # );
    /// # let supervisor = Supervisor::new(
    /// #     Box::new(FakeVendorSession::new()),
    /// #     Arc::new(FakeHubApi::new()),
    /// #     policy,
    /// #     Arc::new(Stop::new()),
    /// #     Arc::new(SystemClock),
    /// # );
    /// assert!(supervisor.record(&TurnId::new("never-run")).is_none());
    /// ```
    pub fn record(&self, turn_id: &TurnId) -> Option<&RecoveryRecord> {
        self.records.get(turn_id).map(|turn| &turn.record)
    }

    /// Drives one logical operation until it is committed, refused, stopped or uncertain.
    ///
    /// Reusing a `turn_id` with different content is refused before anything is dispatched;
    /// reusing it with identical content resumes the same logical operation rather than starting a
    /// second one. A logical turn the Hub has already refused answers [`Settled::Refused`] again,
    /// without a reconciliation and without a dispatch.
    ///
    /// # Errors
    ///
    /// [`Error::HostConfiguration`] when the
    /// request reuses a logical turn id with different content, or when the Hub answered something
    /// that contradicts what it had already said.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeHubApi, FakeVendorSession, ScriptedJitter};
    /// use hub_host::{RetryPolicy, Settled, Stop, Supervisor};
    /// use mango_external_agents::{SystemClock, TerminalStatus, TurnRequest};
    /// use std::sync::Arc;
    /// use std::time::Duration;
    ///
    /// let runtime = tokio::runtime::Builder::new_current_thread()
    ///     .enable_time()
    ///     .build()
    ///     .expect("expected a current-thread runtime");
    /// runtime.block_on(async {
    ///     let policy = RetryPolicy::new(
    ///         Duration::from_millis(1),
    ///         Duration::from_millis(10),
    ///         Duration::from_secs(5),
    ///         Arc::new(ScriptedJitter::maximum()),
    ///     );
    ///     let mut supervisor = Supervisor::new(
    ///         Box::new(FakeVendorSession::new()),
    ///         Arc::new(FakeHubApi::new()),
    ///         policy,
    ///         Arc::new(Stop::new()),
    ///         Arc::new(SystemClock),
    ///     );
    ///     let settled = supervisor
    ///         .run(TurnRequest::new("turn-1", "say hello"))
    ///         .await
    ///         .expect("expected the operation to settle");
    ///     assert!(matches!(
    ///         settled,
    ///         Settled::Committed { terminal: TerminalStatus::Completed, .. }
    ///     ));
    /// });
    /// ```
    pub async fn run(&mut self, request: TurnRequest) -> Result<Settled> {
        let stop = RunStop {
            shutdown: Arc::clone(&self.inner.stop),
            turn: self.abort_signal(&request.turn_id),
        };
        // The record is reached through the map for the whole run and never moved out of it. A
        // `run` future can be dropped at any await inside `drive` — a caller's deadline, a losing
        // `select!` arm, a cancelled request handler — and a record living on this stack frame
        // would be destroyed with it. The next run would then build a fresh one, dispatch a second
        // time under the same logical id, and reserve it with an attempt the Hub cannot tell from
        // the first.
        let turn = match self.records.entry(request.turn_id.clone()) {
            // Validated through the entry rather than after a removal: a refused reuse must not
            // also lose the record that refused it, or the next identical retry would start a
            // second operation.
            Entry::Occupied(occupied) => {
                occupied.get().record.validate(&request)?;
                occupied.into_mut()
            }
            Entry::Vacant(vacant) => vacant.insert(LogicalTurn::new(RecoveryRecord::new(
                self.session_id.clone(),
                &request,
            )?)),
        };
        // Answered before anything is reconciled or dispatched. A refusal is the Hub's final word
        // on the logical operation, so a caller that runs the same request again — a crash
        // recovery, or one that read `Settled::Refused` as "try that id again" — gets the same
        // answer rather than a second execution of work the Hub will not take.
        if let Some(reason) = &turn.refusal {
            return Ok(Settled::Refused {
                reason: reason.clone(),
            });
        }
        self.inner.drive(turn, &request, stop).await
    }
}

impl SupervisorInner {
    async fn drive(
        &self,
        turn: &mut LogicalTurn,
        request: &TurnRequest,
        stop: RunStop,
    ) -> Result<Settled> {
        let mut progress = Progress {
            stop,
            stream: None,
            failures: 0,
            dispatched: false,
            terminal_came_from_hub: false,
        };
        loop {
            // One of a **set of three**, and the only one that decides what a stop settles as.
            // This catches a stop that landed while the last step was running; the one inside
            // `back_off` catches a stop that lands while this loop is sleeping between attempts;
            // the one in `submit` catches a stop that lands while the vendor is acknowledging a
            // dispatch, which is the one window with neither a transcript nor a backoff to
            // interrupt. None is redundant and none covers another in the case it was written for
            // — but each *does* cover the others well enough that deleting one leaves the stop
            // tests green, because the loop reaches a survivor on its next pass. So: do not delete
            // one on the evidence of a passing suite. The exception is `submit`'s, whose absence
            // is visible as the full attempt deadline elapsing. Delete this one and `back_off`'s
            // and the suite fails; that is what the coverage is actually proving.
            if let Some(reason) = progress.stop.reason() {
                self.abandon(&mut progress, reason).await;
                return Ok(Settled::Stopped { reason });
            }
            let record = &mut turn.record;
            let step = match record.action() {
                RecoveryAction::Submit => self.submit(record, request, &mut progress).await?,
                RecoveryAction::Observe => self.observe(record, &mut progress).await?,
                RecoveryAction::Reconcile => self.reconcile(record, &mut progress).await?,
                RecoveryAction::Finished => self.settle(record, &progress).await?,
                // `RecoveryAction` is `#[non_exhaustive]`. A host that met a newer action by
                // guessing would be guessing about whether replaying is safe, so it refuses.
                unknown => {
                    return Err(contradiction(
                        "one of submit, observe, reconcile or finished",
                        &format!("an unhandled recovery action: {unknown:?}"),
                    ));
                }
            };
            match step {
                Step::Settled(settled) => {
                    // Recorded on the logical turn, not just returned: the refusal has to outlive
                    // the `Settled` value the caller is free to drop.
                    if let Settled::Refused { reason } = &settled {
                        turn.refusal = Some(reason.clone());
                    }
                    return Ok(settled);
                }
                Step::Continue => {}
                Step::Backoff(hint) => {
                    // The second of the three described at the top of this loop.
                    if let Some(reason) = self.back_off(&mut progress, hint).await {
                        self.abandon(&mut progress, reason).await;
                        return Ok(Settled::Stopped { reason });
                    }
                }
            }
        }
    }

    /// Dispatches one attempt, recording uncertainty before anything side-effecting happens.
    async fn submit(
        &self,
        record: &mut RecoveryRecord,
        request: &TurnRequest,
        progress: &mut Progress,
    ) -> Result<Step> {
        let operation = self.next_operation(record, request, progress.dispatched)?;
        // Before the Hub reservation, not after it: reserving *is* the side-effecting submission
        // in a Hub-owned model, so a reservation whose acknowledgement is lost must leave this
        // record on `Reconcile` rather than on "nothing happened".
        record.record_dispatch(&operation, Dispatch::AcceptanceUnknown)?;
        progress.dispatched = true;

        match self
            .bounded(self.hub.reserve(&operation, record.fingerprint()))
            .await
        {
            Ok(_receipt) => {}
            Err(HubError::Refused { reason }) => {
                return Ok(Step::Settled(Settled::Refused { reason }));
            }
            Err(error) => return Ok(Step::Backoff(error.retry_hint())),
        }

        let dispatched = request.clone().as_attempt(operation.attempt);
        // The third of the set described at the top of `drive`. Without it the attempt deadline is
        // the only other future in this race, so a stop landing while the vendor is acknowledging
        // is not seen until the deadline expires — the whole of it, for an abort, a revoked
        // consent or a shutdown. Answering `Continue` hands the stop back to the loop's own guard
        // rather than duplicating the abandon-and-settle it already does.
        let started = tokio::select! {
            biased;
            () = progress.stop.stopped() => return Ok(Step::Continue),
            started = tokio::time::timeout(
                self.policy.attempt_deadline(),
                self.session.start_turn(dispatched),
            ) => started,
        };
        match started {
            Ok(Ok(stream)) => {
                record.record_dispatch(&operation, Dispatch::Accepted)?;
                progress.stream = Some(stream);
                Ok(Step::Continue)
            }
            // The library's own certainty is native proof: a failure carrying
            // `Dispatch::NotSubmitted` says the request never left, which is exactly what
            // `reconcile_not_submitted` asks for.
            //
            // It proves absence at the *vendor*, though, and the reservation is at the Hub. The
            // record is about to advance to a strictly newer attempt, so without the withdrawal
            // this one becomes an orphan: a reservation nothing will ever commit a terminal for,
            // that the Hub answers `Accepted` about to whoever asks. The withdrawal is safe here
            // and nowhere else, because this is the one failure that proves nothing is running
            // under the attempt being released.
            Ok(Err(error)) if error.dispatch().is_safe_to_replay() => {
                // A withdrawal this host cannot deliver is not a reason to stall an operation it
                // has already proved did not run, and there is no record state for "still owes
                // the Hub a withdrawal" to retry it from. So the hint is carried into the backoff
                // and the orphan falls back to the Hub's expiry, which `HubApi::reserve` says is
                // the Hub's half of this bargain.
                let withdrawn = self.bounded(self.hub.withdraw(&operation)).await;
                record.reconcile_not_submitted(&operation)?;
                Ok(Step::Backoff(
                    withdrawn.err().and_then(|error| error.retry_hint()),
                ))
            }
            // Everything else — an uncertain failure or a deadline — leaves `AcceptanceUnknown`,
            // so the next turn of the loop reconciles instead of replaying.
            Ok(Err(_)) | Err(_) => Ok(Step::Backoff(None)),
        }
    }

    /// Reads the owned transcript to its terminal, publishing every event to the watchers.
    async fn observe(&self, record: &mut RecoveryRecord, progress: &mut Progress) -> Result<Step> {
        let Some(live) = progress.stream.as_mut() else {
            return self.observe_through_hub(record, progress).await;
        };
        match self.drain(live, &progress.stop).await {
            // The stop is handled at the top of the loop, which still holds the stream and so can
            // cancel the vendor's own work rather than just dropping it.
            Drained::Stopped => Ok(Step::Continue),
            Drained::Terminal(terminal) => {
                let operation = record.operation().clone();
                record.finish(&operation, terminal)?;
                progress.stream = None;
                Ok(Step::Continue)
            }
            Drained::Ended => {
                progress.stream = None;
                self.observe_through_hub(record, progress).await
            }
        }
    }

    /// Waits out an acknowledged operation whose transcript this host no longer holds.
    async fn observe_through_hub(
        &self,
        record: &mut RecoveryRecord,
        progress: &mut Progress,
    ) -> Result<Step> {
        let operation = record.operation().clone();
        match self.bounded(self.hub.reconcile(&operation)).await {
            Ok(Reconciliation::Unsupported) => Ok(Step::Settled(Settled::Uncertain { operation })),
            Ok(Reconciliation::Answered(HubStatus::Committed { terminal })) => {
                record.finish(&operation, terminal)?;
                progress.terminal_came_from_hub = true;
                Ok(Step::Continue)
            }
            Ok(Reconciliation::Answered(HubStatus::Accepted | HubStatus::Unknown)) => {
                Ok(Step::Backoff(None))
            }
            Ok(Reconciliation::Answered(HubStatus::NeverArrived)) => Err(contradiction(
                "a hub that still holds the operation it acknowledged",
                "never arrived, after the same hub acknowledged it",
            )),
            Err(HubError::Refused { reason }) => Ok(Step::Settled(Settled::Refused { reason })),
            Err(error) => Ok(Step::Backoff(error.retry_hint())),
        }
    }

    /// Asks the Hub what became of an attempt whose acknowledgement was lost.
    ///
    /// Nothing here dispatches. Reconciliation runs to completion first, and only a Hub that
    /// proves absence unlocks a newer attempt.
    async fn reconcile(
        &self,
        record: &mut RecoveryRecord,
        progress: &mut Progress,
    ) -> Result<Step> {
        let operation = record.operation().clone();
        match self.bounded(self.hub.reconcile(&operation)).await {
            // No query exists, so nothing can prove what happened. The caller is handed the
            // uncertainty rather than a replay or a fabricated success.
            Ok(Reconciliation::Unsupported) => Ok(Step::Settled(Settled::Uncertain { operation })),
            Ok(Reconciliation::Answered(HubStatus::NeverArrived)) => {
                record.reconcile_not_submitted(&operation)?;
                Ok(Step::Continue)
            }
            Ok(Reconciliation::Answered(HubStatus::Committed { terminal })) => {
                record.finish(&operation, terminal)?;
                progress.terminal_came_from_hub = true;
                Ok(Step::Continue)
            }
            Ok(Reconciliation::Answered(HubStatus::Accepted)) => {
                record.record_dispatch(&operation, Dispatch::Accepted)?;
                Ok(Step::Continue)
            }
            Ok(Reconciliation::Answered(HubStatus::Unknown)) => Ok(Step::Backoff(None)),
            Err(HubError::Refused { reason }) => Ok(Step::Settled(Settled::Refused { reason })),
            Err(error) => Ok(Step::Backoff(error.retry_hint())),
        }
    }

    /// Commits the terminal this run produced, or hands back the one the Hub already held.
    async fn settle(&self, record: &RecoveryRecord, progress: &Progress) -> Result<Step> {
        let terminal = record.terminal().cloned().ok_or_else(|| {
            contradiction(
                "a committed terminal on a finished record",
                "a finished record with no terminal",
            )
        })?;
        if progress.terminal_came_from_hub {
            return Ok(Step::Settled(Settled::AlreadyCommitted { terminal }));
        }
        let operation = record.operation().clone();
        match self.bounded(self.hub.commit(&operation, &terminal)).await {
            Ok(commit @ Commit::Recorded) => {
                Ok(Step::Settled(Settled::Committed { terminal, commit }))
            }
            // The Hub kept an earlier call's outcome, and it is explicitly allowed to differ from
            // the one this run is holding. Reporting the local value as `Committed` would tell a
            // host that reads the terminal and ignores the `commit` field that it produced an
            // outcome nobody recorded, so the Hub's own answer is what comes back.
            Ok(Commit::AlreadyRecorded { terminal }) => {
                Ok(Step::Settled(Settled::AlreadyCommitted { terminal }))
            }
            Err(HubError::Refused { reason }) => Ok(Step::Settled(Settled::Refused { reason })),
            Err(error) => Ok(Step::Backoff(error.retry_hint())),
        }
    }

    /// The operation this dispatch belongs to, taking a strictly newer attempt on every re-send.
    fn next_operation(
        &self,
        record: &mut RecoveryRecord,
        request: &TurnRequest,
        dispatched: bool,
    ) -> Result<OperationRef> {
        if !dispatched {
            return Ok(record.operation().clone());
        }
        let next = record.operation().attempt.next();
        record.retry(&request.clone().as_attempt(next))
    }

    /// Waits out the backoff, answering with the stop reason when one landed instead.
    async fn back_off(
        &self,
        progress: &mut Progress,
        hint: Option<RetryHint>,
    ) -> Option<CancelReason> {
        progress.failures = progress.failures.saturating_add(1);
        // Resolved against the host's clock now rather than when it arrived: a hint that named an
        // instant two minutes ago has nothing left to ask for and resolves to zero. Zero is not a
        // licence to retry immediately — `delay_for` floors it with this policy's own backoff, or
        // a Hub echoing an elapsed `Retry-After` would be hammered at full rate.
        let remaining = hint.map(|hint| hint.remaining(self.clock.now()));
        let delay = self.policy.delay_for(progress.failures, remaining);
        match progress.stop.wait(&self.policy, delay).await {
            WaitOutcome::Elapsed => None,
            WaitOutcome::Stopped => Some(progress.stop.reason().unwrap_or(CancelReason::Requested)),
        }
    }

    /// Reads one turn's transcript, publishing as it goes.
    async fn drain(&self, stream: &mut TurnStream, stop: &RunStop) -> Drained {
        loop {
            let next = tokio::select! {
                biased;
                () = stop.stopped() => return Drained::Stopped,
                event = stream.recv() => event,
            };
            let Some(event) = next else { break };
            // Publishing to nobody succeeds. A closed browser tab is not a reason to stop reading
            // the vendor, and the terminal still has to reach the Hub.
            self.events.publish(event);
        }
        stream
            .terminal_status()
            .map_or(Drained::Ended, Drained::Terminal)
    }

    /// Ends the vendor's own work when the host stops, rather than leaving it running.
    async fn abandon(&self, progress: &mut Progress, reason: CancelReason) {
        let Some(stream) = progress.stream.take() else {
            return;
        };
        let _ = self.session.cancel(reason).await;
        drop(stream);
    }

    /// Bounds one Hub call by the policy's per-attempt deadline.
    async fn bounded<T>(
        &self,
        call: impl Future<Output = std::result::Result<T, HubError>>,
    ) -> std::result::Result<T, HubError> {
        let deadline = self.policy.attempt_deadline();
        match tokio::time::timeout(deadline, call).await {
            Ok(answer) => answer,
            Err(_) => Err(HubError::recoverable(format!(
                "expected an answer within {deadline:?}, received no answer"
            ))),
        }
    }
}

/// A Hub that contradicted itself is a host-configuration problem, not a retryable one.
fn contradiction(expected: &'static str, received: &str) -> Error {
    Error::HostConfiguration {
        expected,
        received: received.into(),
    }
}

/// The bounds a host with no policy of its own can start from.
///
/// Not a default `impl`: a host that never thought about its own backoff should have to write
/// these three numbers down.
pub const SUGGESTED_BASE_DELAY: Duration = Duration::from_millis(250);
/// The longest this reference host ever waits between attempts.
pub const SUGGESTED_MAX_DELAY: Duration = Duration::from_secs(30);
/// The longest this reference host lets one Hub or vendor call run.
pub const SUGGESTED_ATTEMPT_DEADLINE: Duration = Duration::from_secs(60);
