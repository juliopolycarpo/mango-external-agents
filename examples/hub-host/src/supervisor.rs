//! The loop the library deliberately does not contain.
//!
//! `docs/lifecycle.md` says the library owns the retry *contract* — the fingerprint, the dispatch
//! certainty, the transition rules — and the host owns durable storage, network retries and
//! backoff. This is that host half, written against nothing but the library's public API.

use std::collections::HashMap;
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
        /// What the Hub did with it, which says whether this run or an earlier one recorded it.
        commit: Commit,
    },
    /// The Hub already held the terminal, so the vendor work was consumed rather than re-run.
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

/// The mutable state of one `run`, kept out of the arms' parameter lists.
struct Progress {
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
/// and the [`Stop`] signal. Watchers get a [`TurnSubscriber`], never the
/// [`TurnStream`]: dropping the stream is abandonment, and a
/// browser must not be able to abandon an operation by closing a tab.
pub struct Supervisor {
    session: Box<dyn Session>,
    session_id: SessionId,
    hub: Arc<dyn HubApi>,
    policy: RetryPolicy,
    stop: Arc<Stop>,
    clock: Arc<dyn Clock>,
    events: TurnBroadcast,
    records: HashMap<TurnId, RecoveryRecord>,
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
            session,
            session_id,
            hub,
            policy,
            stop,
            clock,
            events: TurnBroadcast::new(DEFAULT_EVENT_CAPACITY),
            records: HashMap::new(),
        }
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
        self.events.subscribe()
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
        self.events.subscriber_count()
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
        self.records.get(turn_id)
    }

    /// Drives one logical operation until it is committed, refused, stopped or uncertain.
    ///
    /// Reusing a `turn_id` with different content is refused before anything is dispatched;
    /// reusing it with identical content resumes the same logical operation rather than starting a
    /// second one.
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
        // Validated before the record leaves the map: a refused reuse must not also lose the
        // record that refused it, or the next identical retry would start a second operation.
        if let Some(existing) = self.records.get(&request.turn_id) {
            existing.validate(&request)?;
        }
        let mut record = match self.records.remove(&request.turn_id) {
            Some(record) => record,
            None => RecoveryRecord::new(self.session_id.clone(), &request)?,
        };
        let outcome = self.drive(&mut record, &request).await;
        self.records.insert(request.turn_id.clone(), record);
        outcome
    }

    async fn drive(&self, record: &mut RecoveryRecord, request: &TurnRequest) -> Result<Settled> {
        let mut progress = Progress {
            stream: None,
            failures: 0,
            dispatched: false,
            terminal_came_from_hub: false,
        };
        loop {
            // One of a **pair**. This catches a stop that landed while the last step was running;
            // the one inside `back_off` catches a stop that lands while this loop is sleeping
            // between attempts. Neither is redundant and neither covers the other in the case it
            // was written for — but each *does* cover the other well enough that deleting one
            // leaves the stop tests green, because the loop reaches the survivor on its next pass.
            // So: do not delete one on the evidence of a passing suite. Delete both and the
            // suite fails; that is what the coverage is actually proving.
            if let Some(reason) = self.stop.reason() {
                self.abandon(&mut progress, reason).await;
                return Ok(Settled::Stopped { reason });
            }
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
                Step::Settled(settled) => return Ok(settled),
                Step::Continue => {}
                Step::Backoff(hint) => {
                    // The second half of the pair described at the top of this loop.
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
        let started = tokio::time::timeout(
            self.policy.attempt_deadline(),
            self.session.start_turn(dispatched),
        )
        .await;
        match started {
            Ok(Ok(stream)) => {
                record.record_dispatch(&operation, Dispatch::Accepted)?;
                progress.stream = Some(stream);
                Ok(Step::Continue)
            }
            // The library's own certainty is native proof: a failure carrying
            // `Dispatch::NotSubmitted` says the request never left, which is exactly what
            // `reconcile_not_submitted` asks for.
            Ok(Err(error)) if error.dispatch().is_safe_to_replay() => {
                record.reconcile_not_submitted(&operation)?;
                Ok(Step::Backoff(None))
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
        match self.drain(live).await {
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
            Ok(commit) => Ok(Step::Settled(Settled::Committed { terminal, commit })),
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
        match self.policy.wait(delay, &self.stop).await {
            WaitOutcome::Elapsed => None,
            WaitOutcome::Stopped => Some(self.stop.reason().unwrap_or(CancelReason::Requested)),
        }
    }

    /// Reads one turn's transcript, publishing as it goes.
    async fn drain(&self, stream: &mut TurnStream) -> Drained {
        loop {
            let next = tokio::select! {
                biased;
                () = self.stop.stopped() => return Drained::Stopped,
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
