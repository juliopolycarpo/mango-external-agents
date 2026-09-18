//! Host-owned reconciliation state. This module never submits or replays vendor work.

use std::fmt;
use std::io::Write;

use crate::stream::TerminalStatus;
use crate::{Dispatch, Error, OperationRef, Result, SessionId, TurnRequest};

/// SHA-256 of the versioned logical request content, excluding its IDs and attempt number.
///
/// Includes the prompt, every attachment field and byte, and the configuration patch. The host
/// must also preserve session configuration and execution authority across recovery. This digest
/// is input validation, not vendor idempotency or permission to replay uncertain work.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RequestFingerprint([u8; 32]);

impl fmt::Debug for RequestFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RequestFingerprint(SHA-256)")
    }
}

impl RequestFingerprint {
    /// Hashes exact request content without allocating an encoded copy of attachments.
    ///
    /// ```
    /// use mango_external_agents::{RequestFingerprint, TurnRequest, AttemptId};
    /// let first = TurnRequest::new("turn", "hello");
    /// let retry = first.clone().as_attempt(AttemptId::new(2));
    /// assert_eq!(RequestFingerprint::of(&first).unwrap(), RequestFingerprint::of(&retry).unwrap());
    /// ```
    pub fn of(request: &TurnRequest) -> Result<Self> {
        let mut writer = DigestWriter(ring::digest::Context::new(&ring::digest::SHA256));
        let unserializable = || Error::HostConfiguration {
            expected: "serializable turn content",
            received: String::from("unserializable turn content"),
        };
        serde_json::to_writer(
            &mut writer,
            &("mea-request-v1", &request.input, &request.configuration),
        )
        .map_err(|_| unserializable())?;
        // Attachment content goes in raw and length-framed rather than through JSON. A `Vec<u8>`
        // serialises as an array of decimal integers — three to four bytes of digest input per byte
        // of file, for no extra distinguishing power — and reaching it at all would mean deriving
        // `Serialize` on the public `Attachment`, handing a host's ids, names and file contents to
        // any serialiser. That is precisely what its redacting `Debug` refuses to do.
        writer.frame(&request.attachments.len().to_le_bytes());
        for attachment in &request.attachments {
            writer.frame(attachment.id.as_bytes());
            writer.frame(attachment.name.as_bytes());
            writer.frame(attachment.mime_type.as_bytes());
            serde_json::to_writer(&mut writer, &attachment.kind).map_err(|_| unserializable())?;
            writer.frame(&attachment.bytes);
        }
        let digest = writer.0.finish();
        let mut bytes = [0; 32];
        bytes.copy_from_slice(digest.as_ref());
        Ok(Self(bytes))
    }
}

struct DigestWriter(ring::digest::Context);

impl DigestWriter {
    /// Feeds one field, prefixed by its length, so a value cannot be split or joined differently
    /// by another arrangement of the same bytes.
    fn frame(&mut self, bytes: &[u8]) {
        self.0.update(&bytes.len().to_le_bytes());
        self.0.update(bytes);
    }
}

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The next safe action for a supervisor holding the logical operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryAction {
    /// Nothing was submitted. The supervisor may submit its current attempt once.
    Submit,
    /// Native work was acknowledged. Keep observing or explicitly cancel it.
    Observe,
    /// Acceptance is unknown. Reconcile through a supported native query; never replay blindly.
    Reconcile,
    /// A logical terminal was committed. Stop retrying this operation.
    Finished,
}

/// One supervisor's in-memory recovery record, independent of durable host storage.
///
/// The host owns uniqueness and persistence of logical IDs. It stores this record before dispatch,
/// records `AcceptanceUnknown` before a side-effecting call, and retains the stream until a terminal
/// or explicit abort. Browser subscribers do not own or drop the stream. A host requiring crash
/// recovery persists the same fields in its own database, outside the vendor library.
#[derive(Clone, Debug)]
pub struct RecoveryRecord {
    operation: OperationRef,
    fingerprint: RequestFingerprint,
    dispatch: Dispatch,
    terminal: Option<TerminalStatus>,
}

impl RecoveryRecord {
    /// Reserves one logical request before it is sent.
    ///
    /// ```
    /// use mango_external_agents::{RecoveryRecord, RecoveryAction, SessionId, TurnRequest};
    /// let record = RecoveryRecord::new(SessionId::new("chat"), &TurnRequest::new("turn", "hello")).unwrap();
    /// assert_eq!(record.action(), RecoveryAction::Submit);
    /// ```
    pub fn new(session: SessionId, request: &TurnRequest) -> Result<Self> {
        Ok(Self {
            operation: OperationRef::new(session, request.turn_id.clone(), request.attempt),
            fingerprint: RequestFingerprint::of(request)?,
            dispatch: Dispatch::NotSubmitted,
            terminal: None,
        })
    }

    /// The operation to persist or correlate, for example before publishing a host receipt.
    pub fn operation(&self) -> &OperationRef {
        &self.operation
    }

    /// The digest to persist beside the logical ID for recovery after host restart.
    pub fn fingerprint(&self) -> &RequestFingerprint {
        &self.fingerprint
    }

    /// Current dispatch certainty, which a reconnect alone cannot change.
    pub fn dispatch(&self) -> Dispatch {
        self.dispatch
    }

    /// The committed terminal, independent of whether a browser read it.
    pub fn terminal(&self) -> Option<&TerminalStatus> {
        self.terminal.as_ref()
    }

    /// Chooses observation or reconciliation instead of replay after a lost acknowledgement.
    ///
    /// For example, a transport error after dispatch leaves `Reconcile` until native evidence arrives.
    pub fn action(&self) -> RecoveryAction {
        if self.terminal.is_some() {
            return RecoveryAction::Finished;
        }
        match self.dispatch {
            Dispatch::NotSubmitted => RecoveryAction::Submit,
            Dispatch::Accepted => RecoveryAction::Observe,
            Dispatch::AcceptanceUnknown => RecoveryAction::Reconcile,
        }
    }

    /// Rejects logical ID reuse with changed content before the host dispatches anything.
    ///
    /// For example, an HTTP retry with the same ID and a different attachment receives a refusal.
    pub fn validate(&self, request: &TurnRequest) -> Result<()> {
        if request.turn_id != self.operation.turn_id
            || RequestFingerprint::of(request)? != self.fingerprint
        {
            return Err(refusal(
                "the original logical turn ID and identical request fingerprint",
                "different request identity or content",
            ));
        }
        Ok(())
    }

    /// Records certainty for the current attempt; stale results and certainty downgrades fail.
    ///
    /// Set `AcceptanceUnknown` before dispatch, then `Accepted` when the vendor acknowledges it.
    /// An unknown attempt can become NotSubmitted only through explicit reconciliation.
    pub fn record_dispatch(&mut self, operation: &OperationRef, dispatch: Dispatch) -> Result<()> {
        self.require_owner(operation)?;
        if self.terminal.is_some()
            || (self.dispatch == Dispatch::Accepted && dispatch != Dispatch::Accepted)
            || (self.dispatch == Dispatch::AcceptanceUnknown && dispatch == Dispatch::NotSubmitted)
        {
            return Err(refusal(
                "monotonic dispatch certainty for a live attempt",
                "a terminal attempt or unproven certainty downgrade",
            ));
        }
        self.dispatch = dispatch;
        Ok(())
    }

    /// Records native proof that an uncertain attempt never ran.
    ///
    /// Call only after a supported reconciliation query proves absence. A missing acknowledgement
    /// or reconnect is not proof; vendors without such a query cannot use this transition.
    pub fn reconcile_not_submitted(&mut self, operation: &OperationRef) -> Result<()> {
        self.require_owner(operation)?;
        if self.dispatch != Dispatch::AcceptanceUnknown || self.terminal.is_some() {
            return Err(refusal(
                "a live acceptance-unknown attempt",
                "another dispatch state",
            ));
        }
        self.dispatch = Dispatch::NotSubmitted;
        Ok(())
    }

    /// Advances a proven unsubmitted operation to a strictly newer attempt with identical input.
    ///
    /// An identical retry refers to this same record. Accepted or uncertain requests cannot advance.
    pub fn retry(&mut self, request: &TurnRequest) -> Result<OperationRef> {
        self.validate(request)?;
        if self.action() != RecoveryAction::Submit || request.attempt <= self.operation.attempt {
            return Err(refusal(
                "a newer attempt after proof of non-submission",
                "an unsafe replay or non-increasing attempt",
            ));
        }
        self.operation.attempt = request.attempt;
        Ok(self.operation.clone())
    }

    /// Commits one outcome from the current attempt; a duplicate identical outcome is harmless.
    ///
    /// For example, a supervisor copies `TurnStream::terminal_status()` into its recovery record.
    pub fn finish(&mut self, operation: &OperationRef, terminal: TerminalStatus) -> Result<()> {
        self.require_owner(operation)?;
        if let Some(previous) = &self.terminal {
            return if previous == &terminal {
                Ok(())
            } else {
                Err(refusal(
                    "one logical terminal outcome",
                    "a conflicting terminal outcome",
                ))
            };
        }
        self.terminal = Some(terminal);
        Ok(())
    }

    fn require_owner(&self, operation: &OperationRef) -> Result<()> {
        if operation != &self.operation {
            return Err(refusal(
                "the current session, logical turn and attempt",
                "a foreign or stale operation",
            ));
        }
        Ok(())
    }
}

fn refusal(expected: &'static str, received: &str) -> Error {
    Error::HostConfiguration {
        expected,
        received: received.into(),
    }
}

#[cfg(test)]
mod tests;
