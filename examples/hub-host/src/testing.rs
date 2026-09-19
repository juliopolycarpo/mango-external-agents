//! Named fakes for the two things a host cannot ask a real deployment to do on demand.
//!
//! Neither is a kind fake. [`FakeHubApi`] drops acknowledgements, declares that it has no
//! reconciliation query and refuses outright, and it records every call it received so a test can
//! assert an **exact** count rather than "it looked like it only submitted once".
//! [`FakeVendorSession`] is here rather than in the library's own `testing` feature because what
//! it has to express — a `start_turn` whose acknowledgement is lost after the vendor started the
//! work — is a host-side failure, not a vendor dialect.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use mango_external_agents::{
    AttemptId, CancelReason, CloseReason, Dispatch, Error, EventKind, EventSink, HarnessIdentity,
    OperationRef, PermissionResponse, RequestFingerprint, Result, Session, SessionId, SessionIds,
    SessionSnapshot, SessionState, SystemClock, TerminalStatus, TransportKind, TransportSelection,
    TurnId, TurnRequest, TurnStream,
};
use tokio::sync::Notify;

use crate::hub::{Commit, HubApi, HubError, HubReceipt, HubStatus, Reconciliation};
use crate::retry::Jitter;

/// A [`Jitter`] that always answers the same factor.
///
/// Exists so a backoff test asserts an exact [`Duration`](std::time::Duration). A policy proven
/// against a range is a policy whose cap can be off by a factor of four without a test noticing.
#[derive(Clone, Copy, Debug)]
pub struct ScriptedJitter {
    factor: f64,
}

impl ScriptedJitter {
    /// The top of the jitter band: nothing is removed from the computed delay.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{Jitter, testing::ScriptedJitter};
    ///
    /// assert_eq!(ScriptedJitter::maximum().factor(), 1.0);
    /// ```
    pub fn maximum() -> Self {
        Self { factor: 1.0 }
    }

    /// The bottom of the jitter band: the most the policy will ever shorten a delay.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{Jitter, testing::ScriptedJitter};
    ///
    /// assert_eq!(ScriptedJitter::minimum().factor(), 0.0);
    /// ```
    pub fn minimum() -> Self {
        Self { factor: 0.0 }
    }

    /// A factor anywhere in between.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{Jitter, testing::ScriptedJitter};
    ///
    /// assert_eq!(ScriptedJitter::at(0.5).factor(), 0.5);
    /// ```
    pub fn at(factor: f64) -> Self {
        Self { factor }
    }
}

impl Jitter for ScriptedJitter {
    fn factor(&self) -> f64 {
        self.factor
    }
}

/// Which [`HubApi`] method a recorded call was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HubCallKind {
    /// [`HubApi::reserve`].
    Reserve,
    /// [`HubApi::reconcile`].
    Reconcile,
    /// [`HubApi::commit`].
    Commit,
}

/// One call a [`FakeHubApi`] received.
#[derive(Clone, Debug)]
pub struct HubCall {
    /// Which method it was.
    pub kind: HubCallKind,
    /// Which session, turn and attempt it named.
    pub operation: OperationRef,
    /// The fingerprint, for the one method that carries it.
    pub fingerprint: Option<RequestFingerprint>,
}

/// One scripted answer to [`HubApi::reserve`].
#[derive(Clone, Debug)]
pub enum ReserveAnswer {
    /// Record the reservation and acknowledge it.
    Accept,
    /// Record the reservation and then lose the acknowledgement.
    ///
    /// The caller sees a recoverable transport failure *after* the Hub has already recorded the
    /// submission — the case a host that reads "error" as "it did not happen" duplicates.
    AcceptThenDropAcknowledgement,
    /// Fail without recording anything.
    Fail(HubError),
}

/// One scripted answer to [`HubApi::reconcile`].
#[derive(Clone, Debug)]
pub enum ReconcileAnswer {
    /// Answer from what this Hub has actually recorded.
    FromRecord,
    /// Answer with exactly this, whatever the Hub has recorded.
    Answer(Reconciliation),
    /// Fail the call.
    Fail(HubError),
}

/// One scripted answer to [`HubApi::commit`].
#[derive(Clone, Debug)]
pub enum CommitAnswer {
    /// Record the terminal, idempotently by logical operation identity.
    Record,
    /// Answer that an earlier call already recorded *this* terminal, whatever is offered now.
    ///
    /// The case a host cannot produce on its own, because the outcome the Hub kept may differ
    /// from the one this run has in hand: an earlier attempt that ended `Failed` beats a later
    /// one that ended `Completed`, and the host has to report what the Hub holds.
    AlreadyRecorded {
        /// The outcome the Hub is holding.
        terminal: TerminalStatus,
    },
    /// Fail the call.
    Fail(HubError),
}

/// The logical identity a Hub is idempotent over: a session and a turn, never an attempt.
type LogicalId = (SessionId, TurnId);

#[derive(Default)]
struct HubLedger {
    reserved: HashSet<LogicalId>,
    committed: HashMap<LogicalId, TerminalStatus>,
}

/// A [`HubApi`] that answers from a script and remembers every call.
///
/// # Example
///
/// ```
/// use hub_host::testing::{FakeHubApi, HubCallKind};
///
/// let hub = FakeHubApi::new();
/// assert_eq!(hub.count(HubCallKind::Reserve), 0);
/// ```
#[derive(Default)]
pub struct FakeHubApi {
    reserve: Mutex<VecDeque<ReserveAnswer>>,
    reconcile: Mutex<VecDeque<ReconcileAnswer>>,
    commit: Mutex<VecDeque<CommitAnswer>>,
    calls: Mutex<Vec<HubCall>>,
    ledger: Mutex<HubLedger>,
    receipts: AtomicU64,
}

impl std::fmt::Debug for FakeHubApi {
    /// Reports how much it has been asked, not the ids it was asked about.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FakeHubApi")
            .field("calls", &self.calls().len())
            .finish_non_exhaustive()
    }
}

impl FakeHubApi {
    /// A Hub that accepts everything and answers reconciliation from its own record.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::FakeHubApi;
    ///
    /// assert!(FakeHubApi::new().calls().is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Scripts the next reservations, in order. Later calls fall back to accepting.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeHubApi, ReserveAnswer};
    ///
    /// let hub = FakeHubApi::new().reserving([ReserveAnswer::AcceptThenDropAcknowledgement]);
    /// assert!(hub.calls().is_empty());
    /// ```
    #[must_use]
    pub fn reserving(self, answers: impl IntoIterator<Item = ReserveAnswer>) -> Self {
        self.reserve
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(answers);
        self
    }

    /// Scripts `count` recoverable reservation failures before the first success.
    ///
    /// The shape that proves there is no retry-count ceiling: set it higher than any number a
    /// policy might have been tempted to stop at.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::FakeHubApi;
    ///
    /// let hub = FakeHubApi::new().reserving_after_recoverable_failures(8);
    /// assert!(hub.calls().is_empty());
    /// ```
    #[must_use]
    pub fn reserving_after_recoverable_failures(self, count: usize) -> Self {
        let failures = (0..count)
            .map(|attempt| ReserveAnswer::Fail(HubError::recoverable(format!("reset {attempt}"))));
        self.reserving(failures)
    }

    /// Scripts the next reconciliations, in order. Later calls answer from the Hub's record.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeHubApi, ReconcileAnswer};
    /// use hub_host::Reconciliation;
    ///
    /// let hub = FakeHubApi::new()
    ///     .reconciling([ReconcileAnswer::Answer(Reconciliation::Unsupported)]);
    /// assert!(hub.calls().is_empty());
    /// ```
    #[must_use]
    pub fn reconciling(self, answers: impl IntoIterator<Item = ReconcileAnswer>) -> Self {
        self.reconcile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(answers);
        self
    }

    /// Scripts the next commits, in order. Later calls record the terminal.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{CommitAnswer, FakeHubApi};
    /// use hub_host::HubError;
    ///
    /// let hub = FakeHubApi::new()
    ///     .committing([CommitAnswer::Fail(HubError::recoverable("502"))]);
    /// assert!(hub.calls().is_empty());
    /// ```
    #[must_use]
    pub fn committing(self, answers: impl IntoIterator<Item = CommitAnswer>) -> Self {
        self.commit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(answers);
        self
    }

    /// Every call it received, in order.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::FakeHubApi;
    ///
    /// assert!(FakeHubApi::new().calls().is_empty());
    /// ```
    pub fn calls(&self) -> Vec<HubCall> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The methods it was called through, in order, for asserting on sequence.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::FakeHubApi;
    ///
    /// assert!(FakeHubApi::new().sequence().is_empty());
    /// ```
    pub fn sequence(&self) -> Vec<HubCallKind> {
        self.calls().into_iter().map(|call| call.kind).collect()
    }

    /// Exactly how many times one method was called.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeHubApi, HubCallKind};
    ///
    /// assert_eq!(FakeHubApi::new().count(HubCallKind::Commit), 0);
    /// ```
    pub fn count(&self, kind: HubCallKind) -> usize {
        self.calls().iter().filter(|call| call.kind == kind).count()
    }

    /// The attempt generations one method was called with, in order.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeHubApi, HubCallKind};
    ///
    /// assert!(FakeHubApi::new().attempts(HubCallKind::Reserve).is_empty());
    /// ```
    pub fn attempts(&self, kind: HubCallKind) -> Vec<AttemptId> {
        self.calls()
            .iter()
            .filter(|call| call.kind == kind)
            .map(|call| call.operation.attempt)
            .collect()
    }

    /// The fingerprints one method was called with, in order.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeHubApi, HubCallKind};
    ///
    /// assert!(FakeHubApi::new().fingerprints(HubCallKind::Reserve).is_empty());
    /// ```
    pub fn fingerprints(&self, kind: HubCallKind) -> Vec<RequestFingerprint> {
        self.calls()
            .iter()
            .filter(|call| call.kind == kind)
            .filter_map(|call| call.fingerprint.clone())
            .collect()
    }

    fn record(
        &self,
        kind: HubCallKind,
        operation: &OperationRef,
        fingerprint: Option<&RequestFingerprint>,
    ) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(HubCall {
                kind,
                operation: operation.clone(),
                fingerprint: fingerprint.cloned(),
            });
    }

    fn next_receipt(&self) -> HubReceipt {
        let serial = self.receipts.fetch_add(1, Ordering::Relaxed) + 1;
        HubReceipt::new(format!("hub-op-{serial}"), SystemTime::UNIX_EPOCH)
    }
}

fn logical(operation: &OperationRef) -> LogicalId {
    (operation.session_id.clone(), operation.turn_id.clone())
}

#[async_trait::async_trait]
impl HubApi for FakeHubApi {
    async fn reserve(
        &self,
        operation: &OperationRef,
        fingerprint: &RequestFingerprint,
    ) -> std::result::Result<HubReceipt, HubError> {
        self.record(HubCallKind::Reserve, operation, Some(fingerprint));
        let answer = self
            .reserve
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or(ReserveAnswer::Accept);
        match answer {
            ReserveAnswer::Fail(error) => Err(error),
            ReserveAnswer::Accept => {
                self.ledger
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .reserved
                    .insert(logical(operation));
                Ok(self.next_receipt())
            }
            // Recorded first, reported as a failure second. That order is the bug this fake
            // exists to reproduce.
            ReserveAnswer::AcceptThenDropAcknowledgement => {
                self.ledger
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .reserved
                    .insert(logical(operation));
                Err(HubError::recoverable(
                    "expected an acknowledgement, received a closed connection",
                ))
            }
        }
    }

    async fn reconcile(
        &self,
        operation: &OperationRef,
    ) -> std::result::Result<Reconciliation, HubError> {
        self.record(HubCallKind::Reconcile, operation, None);
        let answer = self
            .reconcile
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or(ReconcileAnswer::FromRecord);
        match answer {
            ReconcileAnswer::Fail(error) => Err(error),
            ReconcileAnswer::Answer(answer) => Ok(answer),
            ReconcileAnswer::FromRecord => {
                let ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
                let id = logical(operation);
                if let Some(terminal) = ledger.committed.get(&id) {
                    return Ok(Reconciliation::Answered(HubStatus::Committed {
                        terminal: terminal.clone(),
                    }));
                }
                Ok(Reconciliation::Answered(if ledger.reserved.contains(&id) {
                    HubStatus::Accepted
                } else {
                    HubStatus::NeverArrived
                }))
            }
        }
    }

    async fn commit(
        &self,
        operation: &OperationRef,
        terminal: &TerminalStatus,
    ) -> std::result::Result<Commit, HubError> {
        self.record(HubCallKind::Commit, operation, None);
        let answer = self
            .commit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or(CommitAnswer::Record);
        match answer {
            CommitAnswer::Fail(error) => return Err(error),
            CommitAnswer::AlreadyRecorded { terminal } => {
                return Ok(Commit::AlreadyRecorded { terminal });
            }
            CommitAnswer::Record => {}
        }
        let mut ledger = self.ledger.lock().unwrap_or_else(PoisonError::into_inner);
        let id = logical(operation);
        if let Some(recorded) = ledger.committed.get(&id) {
            return Ok(Commit::AlreadyRecorded {
                terminal: recorded.clone(),
            });
        }
        ledger.committed.insert(id, terminal.clone());
        Ok(Commit::Recorded)
    }
}

/// One scripted answer to [`Session::start_turn`].
#[derive(Clone, Copy, Debug)]
pub enum TurnAnswer {
    /// Run a short transcript and complete it before answering.
    Complete,
    /// Emit the opening events, then hold the turn open until [`FakeVendorSession::release`].
    ///
    /// The only way to have a watcher disconnect *mid-turn* rather than after it.
    CompleteWhenReleased,
    /// Refuse before the request left this host.
    ///
    /// Carries [`Dispatch::NotSubmitted`], which is the library's own proof of absence.
    NotSubmitted,
    /// Fail after the vendor may already have started the work.
    ///
    /// Carries [`Dispatch::AcceptanceUnknown`]: the acknowledgement is gone and nothing local can
    /// say whether the turn ran.
    AcknowledgementLost,
}

struct SessionScript {
    turns: VecDeque<TurnAnswer>,
    default: TurnAnswer,
}

/// A [`Session`] with no vendor behind it, scripted per turn and counting what it was asked.
///
/// Cloning shares one session: the test keeps a handle while the supervisor owns a `Box<dyn
/// Session>` built from another.
///
/// # Example
///
/// ```
/// use hub_host::testing::FakeVendorSession;
///
/// let session = FakeVendorSession::new();
/// assert_eq!(session.start_count(), 0);
/// ```
#[derive(Clone)]
pub struct FakeVendorSession {
    inner: Arc<VendorInner>,
}

struct VendorInner {
    state: SessionState,
    script: Mutex<SessionScript>,
    starts: AtomicU64,
    cancels: Mutex<Vec<CancelReason>>,
    gate: Notify,
    released: AtomicBool,
}

impl VendorInner {
    /// Waits for the gate, and returns at once when it is already open.
    ///
    /// The flag is read *after* the wait is registered, for the same reason
    /// [`CancelToken`](mango_external_agents::CancelToken) does it in that order: a `release`
    /// landing between the two would otherwise be missed and the held turn would never finish.
    async fn released(&self) {
        loop {
            let opened = self.gate.notified();
            if self.released.load(Ordering::Acquire) {
                return;
            }
            opened.await;
        }
    }

    fn open_gate(&self) {
        self.released.store(true, Ordering::Release);
        self.gate.notify_waiters();
    }
}

impl std::fmt::Debug for FakeVendorSession {
    /// Reports what it has been asked, not the ids it answers to.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FakeVendorSession")
            .field("start_count", &self.start_count())
            .finish_non_exhaustive()
    }
}

impl Default for FakeVendorSession {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeVendorSession {
    /// A session whose every turn runs a short transcript and completes.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::FakeVendorSession;
    ///
    /// assert_eq!(FakeVendorSession::new().start_count(), 0);
    /// ```
    pub fn new() -> Self {
        let snapshot = SessionSnapshot::opening(
            SessionIds {
                session_id: SessionId::new("hub-chat-1"),
                native_session_id: String::from("vendor-session-1"),
            },
            HarnessIdentity::claude(),
            TransportSelection::new(None, TransportKind::Stdio),
            SystemTime::UNIX_EPOCH,
        );
        Self {
            inner: Arc::new(VendorInner {
                state: SessionState::new(Arc::new(SystemClock), snapshot),
                script: Mutex::new(SessionScript {
                    turns: VecDeque::new(),
                    default: TurnAnswer::Complete,
                }),
                starts: AtomicU64::new(0),
                cancels: Mutex::new(Vec::new()),
                gate: Notify::new(),
                released: AtomicBool::new(false),
            }),
        }
    }

    /// Scripts the next turns, in order. Later turns fall back to the default answer.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeVendorSession, TurnAnswer};
    ///
    /// let session = FakeVendorSession::new().answering([TurnAnswer::AcknowledgementLost]);
    /// assert_eq!(session.start_count(), 0);
    /// ```
    #[must_use]
    pub fn answering(self, answers: impl IntoIterator<Item = TurnAnswer>) -> Self {
        self.inner
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .turns
            .extend(answers);
        self
    }

    /// Changes what an unscripted turn does.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::{FakeVendorSession, TurnAnswer};
    ///
    /// let session = FakeVendorSession::new().by_default(TurnAnswer::CompleteWhenReleased);
    /// assert_eq!(session.start_count(), 0);
    /// ```
    #[must_use]
    pub fn by_default(self, answer: TurnAnswer) -> Self {
        self.inner
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .default = answer;
        self
    }

    /// Exactly how many turns this session was asked to start.
    ///
    /// The count test 3 turns on: a terminal consumed from the Hub means the vendor was not asked
    /// a second time, and only an exact number says so.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::FakeVendorSession;
    ///
    /// assert_eq!(FakeVendorSession::new().start_count(), 0);
    /// ```
    pub fn start_count(&self) -> usize {
        self.inner.starts.load(Ordering::Acquire) as usize
    }

    /// Every reason this session was cancelled with, in order.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::FakeVendorSession;
    ///
    /// assert!(FakeVendorSession::new().cancels().is_empty());
    /// ```
    pub fn cancels(&self) -> Vec<CancelReason> {
        self.inner
            .cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Lets every turn held open by [`TurnAnswer::CompleteWhenReleased`] finish.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::testing::FakeVendorSession;
    ///
    /// // Releasing a session with no held turn is harmless.
    /// FakeVendorSession::new().release();
    /// ```
    pub fn release(&self) {
        self.inner.open_gate();
    }

    fn next_answer(&self) -> TurnAnswer {
        let mut script = self
            .inner
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        script.turns.pop_front().unwrap_or(script.default)
    }
}

fn lost_acknowledgement() -> Error {
    Error::Link {
        peer: String::from("hub vendor session"),
        message: String::from("expected a turn acknowledgement, received a closed link"),
    }
    .with_dispatch(Dispatch::AcceptanceUnknown)
}

fn never_left() -> Error {
    Error::Link {
        peer: String::from("hub vendor session"),
        message: String::from("expected an open link, received one closed before the write"),
    }
    .with_dispatch(Dispatch::NotSubmitted)
}

#[async_trait::async_trait]
impl Session for FakeVendorSession {
    fn state(&self) -> &SessionState {
        &self.inner.state
    }

    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream> {
        let answer = self.next_answer();
        match answer {
            TurnAnswer::NotSubmitted => return Err(never_left()),
            TurnAnswer::AcknowledgementLost => {
                // Counted before the failure: the vendor started the work, and only the answer
                // was lost. A fake that did not count it would let a duplicate run go unnoticed.
                self.inner.starts.fetch_add(1, Ordering::AcqRel);
                return Err(lost_acknowledgement());
            }
            TurnAnswer::Complete | TurnAnswer::CompleteWhenReleased => {}
        }
        self.inner.starts.fetch_add(1, Ordering::AcqRel);

        let (sink, events) = EventSink::new(
            self.inner.state.snapshot().ids.session_id.clone(),
            request.turn_id.clone(),
            request.attempt,
            Arc::new(SystemClock),
            64,
        );
        sink.emit(EventKind::TurnStarted {
            native_turn_id: format!("vendor-turn-{}", request.attempt),
        })
        .await?;
        sink.emit(EventKind::TextDelta {
            text: format!("working on {}", request.input),
        })
        .await?;
        let stream = TurnStream::accepted(
            request.turn_id.clone(),
            request.attempt,
            format!("vendor-turn-{}", request.attempt),
            events,
        );
        if matches!(answer, TurnAnswer::Complete) {
            sink.complete().await?;
            return Ok(stream);
        }
        let inner = Arc::clone(&self.inner);
        tokio::spawn(async move {
            inner.released().await;
            let _ = sink.complete().await;
        });
        Ok(stream)
    }

    async fn respond(&self, _response: PermissionResponse) -> Result<()> {
        Ok(())
    }

    async fn cancel(&self, reason: CancelReason) -> Result<()> {
        self.inner
            .cancels
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(reason);
        self.inner.open_gate();
        Ok(())
    }

    async fn close(&self, _reason: CloseReason) -> Result<()> {
        self.inner.open_gate();
        Ok(())
    }
}
