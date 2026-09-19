//! The external service that owns this host's operations, as a port.
//!
//! Nothing here is a vendor surface. The Hub is the host's own control plane: the place a
//! logical operation is reserved before it is dispatched, asked about after an acknowledgement
//! is lost, and committed to once it ends. The library never calls it — the supervisor in
//! [`crate::supervisor`] does, through this trait, so a test can script what it answers.

use std::fmt;
use std::time::{Duration, SystemTime};

use mango_external_agents::{OperationRef, RequestFingerprint, TerminalStatus};

/// What the Hub handed back when it reserved an operation.
///
/// The host persists this beside the logical id: it is the durable proof that a submission was
/// announced, which is what makes a lost acknowledgement recoverable rather than a guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubReceipt {
    /// The Hub's own handle for the reserved operation.
    pub hub_operation_id: String,
    /// When the Hub recorded the reservation, by the Hub's clock.
    pub reserved_at: SystemTime,
}

impl HubReceipt {
    /// A receipt for one reserved operation.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::HubReceipt;
    /// use std::time::SystemTime;
    ///
    /// let receipt = HubReceipt::new("hub-op-1", SystemTime::UNIX_EPOCH);
    /// assert_eq!(receipt.hub_operation_id, "hub-op-1");
    /// ```
    pub fn new(hub_operation_id: impl Into<String>, reserved_at: SystemTime) -> Self {
        Self {
            hub_operation_id: hub_operation_id.into(),
            reserved_at,
        }
    }
}

/// What the Hub knows about one logical operation.
///
/// The four answers a host can act on differently. `Unknown` is deliberately separate from
/// [`Reconciliation::Unsupported`]: a Hub that looked and cannot tell is not a Hub that has no
/// query at all, and only the second one makes the operation permanently uncertain.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HubStatus {
    /// No submission for this logical operation ever reached the Hub.
    ///
    /// The only answer that proves absence, so the only one that permits a newer attempt through
    /// [`RecoveryRecord::reconcile_not_submitted`](mango_external_agents::RecoveryRecord::reconcile_not_submitted).
    NeverArrived,
    /// The Hub accepted a submission and the work has not ended yet.
    Accepted,
    /// The Hub already holds this operation's terminal outcome.
    Committed {
        /// The outcome the Hub recorded, to be consumed instead of re-run.
        terminal: TerminalStatus,
    },
    /// The Hub has a record but cannot say how far this operation got.
    Unknown,
}

/// What a reconciliation query answered.
///
/// A Hub deployment with no reconciliation surface is a typed outcome rather than an error,
/// because it is not a failure: the call worked, and the honest answer is that nothing can prove
/// what happened. A host that reads it as an error retries, and retrying is the replay the
/// library exists to prevent.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reconciliation {
    /// The Hub answered with what it knows.
    Answered(HubStatus),
    /// This Hub exposes no reconciliation query, so nothing can prove what happened.
    Unsupported,
}

/// What committing a terminal outcome did.
///
/// Commit is idempotent by logical operation identity — session and turn, not attempt — so a
/// second call after a lost acknowledgement reports the first call's outcome rather than
/// overwriting it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Commit {
    /// This call recorded the terminal.
    Recorded,
    /// A previous call already recorded one for this logical operation.
    AlreadyRecorded {
        /// The outcome the Hub is holding, which may differ from the one just offered.
        terminal: TerminalStatus,
    },
}

/// When a Hub says a recoverable failure may be tried again.
///
/// An absolute instant rather than a duration, matching the shape the library already carries in
/// [`RateLimitWindow::resets_at`](mango_external_agents::RateLimitWindow): a vendor that reports a
/// reset time reports a time. The host resolves it against its own [`Clock`] at the moment it is
/// about to wait, so a hint that arrived minutes ago does not turn into a fresh full-length delay.
///
/// [`Clock`]: mango_external_agents::Clock
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryHint {
    /// The earliest instant the Hub will accept another call.
    pub not_before: SystemTime,
}

impl RetryHint {
    /// A hint that nothing may be retried until this instant.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::RetryHint;
    /// use std::time::{Duration, SystemTime};
    ///
    /// let hint = RetryHint::not_before(SystemTime::UNIX_EPOCH + Duration::from_secs(30));
    /// assert_eq!(hint.remaining(SystemTime::UNIX_EPOCH), Duration::from_secs(30));
    /// ```
    pub fn not_before(not_before: SystemTime) -> Self {
        Self { not_before }
    }

    /// How long is left before the hint expires, as seen from `now`.
    ///
    /// Zero once the instant has passed, and zero for a Hub whose clock ran behind the host's:
    /// a hint in the past is a hint that has already been honoured.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::RetryHint;
    /// use std::time::{Duration, SystemTime};
    ///
    /// let hint = RetryHint::not_before(SystemTime::UNIX_EPOCH);
    /// assert_eq!(hint.remaining(SystemTime::UNIX_EPOCH + Duration::from_secs(5)), Duration::ZERO);
    /// ```
    pub fn remaining(&self, now: SystemTime) -> Duration {
        self.not_before
            .duration_since(now)
            .unwrap_or(Duration::ZERO)
    }
}

/// Why a Hub call failed, split by whether calling again could ever change the answer.
///
/// The split is the whole point. A recoverable failure is retried until it succeeds or the host
/// stops; a refusal is a terminal outcome for the logical operation and ends the loop.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum HubError {
    /// Transport, congestion or a rate limit. Calling again may work.
    Recoverable {
        /// What the Hub or the transport reported, for the host's diagnostics.
        reason: String,
        /// When the Hub said it would accept another call, when it said.
        retry_hint: Option<RetryHint>,
    },
    /// The Hub refused this operation for good. Calling again cannot change the answer.
    Refused {
        /// What the Hub gave as its reason, for the host's diagnostics.
        reason: String,
    },
}

impl HubError {
    /// A failure worth trying again, with no hint from the Hub.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::HubError;
    ///
    /// assert!(HubError::recoverable("connection reset").is_recoverable());
    /// ```
    pub fn recoverable(reason: impl Into<String>) -> Self {
        Self::Recoverable {
            reason: reason.into(),
            retry_hint: None,
        }
    }

    /// The same failure with the instant the Hub asked the host to wait for.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::{HubError, RetryHint};
    /// use std::time::SystemTime;
    ///
    /// let error = HubError::recoverable("rate limited")
    ///     .with_hint(RetryHint::not_before(SystemTime::UNIX_EPOCH));
    /// assert!(error.retry_hint().is_some());
    /// ```
    #[must_use]
    pub fn with_hint(self, hint: RetryHint) -> Self {
        match self {
            Self::Recoverable { reason, .. } => Self::Recoverable {
                reason,
                retry_hint: Some(hint),
            },
            refusal @ Self::Refused { .. } => refusal,
        }
    }

    /// A refusal no retry can change.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::HubError;
    ///
    /// assert!(!HubError::refused("operation withdrawn").is_recoverable());
    /// ```
    pub fn refused(reason: impl Into<String>) -> Self {
        Self::Refused {
            reason: reason.into(),
        }
    }

    /// Whether calling again could change the answer.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::HubError;
    ///
    /// assert!(HubError::recoverable("502").is_recoverable());
    /// ```
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Recoverable { .. })
    }

    /// The instant the Hub asked the host to wait for, when it named one.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::HubError;
    ///
    /// assert!(HubError::refused("withdrawn").retry_hint().is_none());
    /// ```
    pub fn retry_hint(&self) -> Option<RetryHint> {
        match self {
            Self::Recoverable { retry_hint, .. } => *retry_hint,
            Self::Refused { .. } => None,
        }
    }

    /// What the Hub reported, as written.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::HubError;
    ///
    /// assert_eq!(HubError::refused("withdrawn").reason(), "withdrawn");
    /// ```
    pub fn reason(&self) -> &str {
        match self {
            Self::Recoverable { reason, .. } | Self::Refused { reason } => reason,
        }
    }
}

impl fmt::Display for HubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Recoverable { reason, .. } => {
                write!(formatter, "recoverable hub failure: {reason}")
            }
            Self::Refused { reason } => write!(formatter, "hub refusal: {reason}"),
        }
    }
}

impl std::error::Error for HubError {}

/// The external service that owns this host's operations.
///
/// Injected into [`Supervisor`](crate::Supervisor) rather than constructed by it, so the loop can
/// be proven against a Hub that drops acknowledgements, has no reconciliation query, or refuses
/// outright — none of which a real deployment can be asked to do on demand.
#[async_trait::async_trait]
pub trait HubApi: Send + Sync {
    /// Announces one attempt before anything side-effecting happens, returning a receipt.
    ///
    /// The fingerprint travels so the Hub can refuse a logical id reused with different content
    /// the same way [`RecoveryRecord::validate`](mango_external_agents::RecoveryRecord::validate)
    /// does in memory.
    ///
    /// # Errors
    ///
    /// [`HubError::Recoverable`] when the call may be repeated, [`HubError::Refused`] when the Hub
    /// will never accept this operation.
    async fn reserve(
        &self,
        operation: &OperationRef,
        fingerprint: &RequestFingerprint,
    ) -> Result<HubReceipt, HubError>;

    /// Asks what the Hub knows about the logical operation `operation` belongs to.
    ///
    /// Keyed by session and turn rather than by attempt: the question is whether *any* attempt
    /// arrived, which is what decides between observing and dispatching a newer one.
    ///
    /// # Errors
    ///
    /// [`HubError::Recoverable`] or [`HubError::Refused`]. A Hub with no such query answers
    /// [`Reconciliation::Unsupported`] successfully; it is not a failure.
    async fn reconcile(&self, operation: &OperationRef) -> Result<Reconciliation, HubError>;

    /// Records one terminal outcome, idempotently by logical operation identity.
    ///
    /// # Errors
    ///
    /// [`HubError::Recoverable`] when the commit may be repeated, [`HubError::Refused`] when the
    /// Hub will not take it.
    async fn commit(
        &self,
        operation: &OperationRef,
        terminal: &TerminalStatus,
    ) -> Result<Commit, HubError>;
}
