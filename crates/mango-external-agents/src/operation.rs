//! Which work an event, a question or a failure belongs to, and how certain its dispatch is.
//!
//! Three identities, never one:
//!
//! | Identity | Minted by | Stable across |
//! | -- | -- | -- |
//! | [`crate::TurnId`] | the host | every attempt at the same logical turn |
//! | [`AttemptId`] | the host | one dispatch of it |
//! | `native_turn_id` | the vendor | whatever the vendor decides |
//!
//! Collapsing any two of them loses something a host needs. One [`TurnId`] with two
//! [`AttemptId`]s is a retry; two [`TurnId`]s is two turns. A late event naming an older attempt
//! belongs to work the host already gave up on, and a host that cannot see that would let it
//! mutate the attempt that replaced it.
//!
//! None of this makes a turn idempotent. The vendor decides what a second dispatch does, and no
//! identity minted here changes that — which is what [`Dispatch`] is for.

use std::fmt;

use crate::event::{SessionId, TurnId};

/// Which dispatch of one logical turn this is.
///
/// A **generation**, not a name. A retry that means "the same turn, again" keeps its
/// [`crate::TurnId`] and takes the [`AttemptId::next`] generation, and the whole point of the type
/// is that two of them can be compared: a result carrying an older generation belongs to work the
/// host already replaced.
///
/// It is a number rather than an opaque string for exactly that reason. An opaque id would have to
/// be ordered somehow, and every ordering available to this library would be wrong — `attempt-10`
/// sorts before `attempt-2` on any lexicographic comparison, which is the shape a host naming its
/// attempts would reach for first. A host that also wants its own opaque handle per attempt keeps
/// one beside this; what the library needs in order to recognise stale work is the generation.
///
/// # Example
///
/// ```
/// use mango_external_agents::AttemptId;
///
/// let first = AttemptId::FIRST;
/// let retry = first.next();
/// assert!(retry > first);
/// assert_eq!(retry.get(), 2);
/// ```
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct AttemptId(u64);

impl Default for AttemptId {
    /// [`AttemptId::FIRST`], not generation zero.
    ///
    /// Derived, this would have been `0`, which is a generation no host would mint and one that
    /// reads as "before the first attempt" everywhere it is compared.
    fn default() -> Self {
        Self::FIRST
    }
}

impl AttemptId {
    /// The first dispatch of a turn, and what a host that never retries always uses.
    ///
    /// Also [`Default`], so a host with no recovery policy of its own does not have to think about
    /// generations at all.
    pub const FIRST: Self = Self(1);

    /// One generation, numbered by the host.
    pub const fn new(generation: u64) -> Self {
        Self(generation)
    }

    /// The generation as a number, for persisting or comparing.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The generation after this one.
    ///
    /// Saturating rather than wrapping, for the same reason a session revision is: a counter that
    /// went backwards would make a host discard the attempt that replaced the one it has.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

impl fmt::Display for AttemptId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Which session, turn and attempt something belongs to.
///
/// Carried by everything that outlives the call that produced it — events, questions, approvals —
/// so a host can route or discard one without holding the handle that made it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct OperationRef {
    /// The session.
    pub session_id: SessionId,
    /// The logical turn.
    pub turn_id: TurnId,
    /// Which dispatch of it.
    pub attempt: AttemptId,
}

impl fmt::Debug for OperationRef {
    /// Reports an operation generation without logging host session or turn ids.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationRef")
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

impl OperationRef {
    /// Names one attempt at one turn in one session.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{AttemptId, OperationRef, SessionId, TurnId};
    ///
    /// let reference = OperationRef::new(
    ///     SessionId::new("chat-1"),
    ///     TurnId::new("turn-1"),
    ///     AttemptId::new(2),
    /// );
    /// assert_eq!(reference.attempt.get(), 2);
    /// ```
    pub fn new(session_id: SessionId, turn_id: TurnId, attempt: AttemptId) -> Self {
        Self {
            session_id,
            turn_id,
            attempt,
        }
    }

    /// Whether `other` names the same logical turn, whichever attempt it was.
    pub fn is_same_turn(&self, other: &Self) -> bool {
        self.session_id == other.session_id && self.turn_id == other.turn_id
    }

    /// Whether `other` is a later attempt at the same turn.
    ///
    /// What a host asks before letting something apply: a result carrying an attempt the host has
    /// already replaced must not mutate the one that replaced it. The comparison is between
    /// generations, so it is right for `attempt 10` against `attempt 2` — which is the case any
    /// string-shaped attempt id would have got wrong.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{AttemptId, OperationRef, SessionId, TurnId};
    ///
    /// let first = OperationRef::new(
    ///     SessionId::new("chat-1"),
    ///     TurnId::new("turn-1"),
    ///     AttemptId::FIRST,
    /// );
    /// let retry = first.clone().retried_as(first.attempt.next());
    /// assert!(first.is_superseded_by(&retry));
    /// assert!(!retry.is_superseded_by(&first));
    /// ```
    pub fn is_superseded_by(&self, other: &Self) -> bool {
        self.is_same_turn(other) && other.attempt > self.attempt
    }

    /// The same turn, dispatched again under a new attempt.
    #[must_use]
    pub fn retried_as(mut self, attempt: AttemptId) -> Self {
        self.attempt = attempt;
        self
    }
}

impl fmt::Display for OperationRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}/{}/{}",
            self.session_id, self.turn_id, self.attempt
        )
    }
}

/// How far an attempt got before the caller was told about it.
///
/// The question a host has to answer before retrying, and the one a bare `Result` cannot: a
/// refusal that never reached the vendor is safe to replay, and a socket that closed after the
/// request went out is not.
///
/// Nothing here claims the vendor will *do* the right thing with a replay. Exactly-once execution
/// is a vendor property, and no identity this library mints creates one.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Dispatch {
    /// Nothing reached the vendor. Replaying this attempt cannot duplicate work.
    NotSubmitted,
    /// The vendor acknowledged the request. Replaying it starts a second one.
    Accepted,
    /// It may or may not have arrived, and nothing here can tell.
    ///
    /// The case that needs reconciliation rather than a decision: a host must ask the vendor what
    /// it has before replaying, and a surface that cannot be asked must not be replayed blindly.
    AcceptanceUnknown,
}

impl Dispatch {
    /// Whether replaying this attempt unchanged is known not to duplicate vendor work.
    ///
    /// True only for [`Dispatch::NotSubmitted`]. [`Dispatch::AcceptanceUnknown`] is deliberately
    /// false: "probably did not arrive" is the reading that duplicates a turn.
    pub const fn is_safe_to_replay(self) -> bool {
        matches!(self, Self::NotSubmitted)
    }

    /// Whether a host must ask the vendor what happened before deciding.
    pub const fn needs_reconciliation(self) -> bool {
        matches!(self, Self::AcceptanceUnknown)
    }
}

impl fmt::Display for Dispatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::NotSubmitted => "not submitted",
            Self::Accepted => "accepted",
            Self::AcceptanceUnknown => "acceptance unknown",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{AttemptId, Dispatch, OperationRef};
    use crate::event::{SessionId, TurnId};

    fn reference(turn: &str, attempt: u64) -> OperationRef {
        OperationRef::new(
            SessionId::new("chat-1"),
            TurnId::new(turn),
            AttemptId::new(attempt),
        )
    }

    #[test]
    fn two_attempts_at_one_turn_are_the_same_turn() {
        assert!(reference("turn-1", 1).is_same_turn(&reference("turn-1", 2)));
        assert!(!reference("turn-1", 1).is_same_turn(&reference("turn-2", 1)));
    }

    /// The check a host runs before letting a late result apply. Without it a result from an
    /// abandoned attempt would mutate the attempt that replaced it.
    #[test]
    fn a_later_attempt_supersedes_an_earlier_one_and_not_the_other_way_round() {
        let first = reference("turn-1", 1);
        let retry = reference("turn-1", 2);
        assert!(first.is_superseded_by(&retry));
        assert!(!retry.is_superseded_by(&first));
        assert!(!first.is_superseded_by(&first));
    }

    /// The case that decided the type. Any string-shaped attempt id a host would reach for —
    /// `attempt-2`, `attempt-10` — sorts the wrong way lexicographically, so a host would quietly
    /// let a result from generation 2 overwrite the work of generation 10.
    #[test]
    fn the_tenth_attempt_supersedes_the_second_rather_than_sorting_before_it() {
        let second = reference("turn-1", 2);
        let tenth = reference("turn-1", 10);

        assert!(second.is_superseded_by(&tenth));
        assert!(!tenth.is_superseded_by(&second));
        assert!(
            AttemptId::new(10) > AttemptId::new(2),
            "expected generations to compare as numbers"
        );
    }

    /// A different turn is not a supersession, however its attempts sort.
    #[test]
    fn an_attempt_at_another_turn_never_supersedes_this_one() {
        assert!(!reference("turn-1", 9).is_superseded_by(&reference("turn-2", 1)));
    }

    #[test]
    fn a_reference_round_trips_and_prints_all_three_identities() {
        let reference = reference("turn-1", 2);
        assert_eq!(reference.to_string(), "chat-1/turn-1/2");
        let encoded = serde_json::to_value(&reference).expect("expected a serializable reference");
        assert_eq!(encoded["sessionId"], "chat-1");
        assert_eq!(encoded["turnId"], "turn-1");
        assert_eq!(encoded["attempt"], 2);
        assert_eq!(
            serde_json::from_value::<OperationRef>(encoded).expect("expected the reference back"),
            reference
        );
    }

    /// "Probably did not arrive" is the reading that runs a turn twice, so unknown is not safe.
    #[test]
    fn only_a_request_that_never_left_is_safe_to_replay() {
        assert!(Dispatch::NotSubmitted.is_safe_to_replay());
        assert!(!Dispatch::Accepted.is_safe_to_replay());
        assert!(!Dispatch::AcceptanceUnknown.is_safe_to_replay());
        assert!(Dispatch::AcceptanceUnknown.needs_reconciliation());
        assert!(!Dispatch::NotSubmitted.needs_reconciliation());
    }

    /// A host with no recovery policy still has to put something on every turn, and it should not
    /// have to think about generations to do it.
    #[test]
    fn the_default_attempt_is_the_first_generation() {
        assert_eq!(AttemptId::default(), AttemptId::FIRST);
        assert_eq!(AttemptId::FIRST.get(), 1);
        assert_eq!(AttemptId::FIRST.next().get(), 2);
        assert_eq!(
            AttemptId::new(u64::MAX).next().get(),
            u64::MAX,
            "expected the generation to stick rather than wrap backwards"
        );
    }
}
