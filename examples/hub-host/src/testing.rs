//! Named fakes for what a host cannot ask a real deployment to do on demand.
//!
//! [`FakeHubApi`] is not a kind fake. It drops acknowledgements, declares that it has no
//! reconciliation query and refuses outright, and it records every call it received so a test can
//! assert an **exact** count rather than "it looked like it only submitted once".

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use mango_external_agents::{
    AttemptId, OperationRef, RequestFingerprint, SessionId, TerminalStatus, TurnId,
};

use crate::hub::{Commit, HubApi, HubError, HubReceipt, HubStatus, Reconciliation};

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
        if let CommitAnswer::Fail(error) = answer {
            return Err(error);
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
