//! A broker that records what it was asked and answers from a script.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use crate::host::Clock;
use crate::permission::{BrokerDecision, PermissionBroker, PermissionRequest};

/// A [`PermissionBroker`] that answers the same way every time and remembers every question.
///
/// # Example
///
/// ```
/// use mango_external_agents::testing::RecordingBroker;
/// use mango_external_agents::BrokerDecision;
///
/// let broker = RecordingBroker::new(BrokerDecision::Allow);
/// assert!(broker.requests().is_empty());
/// ```
#[derive(Clone, Debug)]
pub struct RecordingBroker {
    decision: BrokerDecision,
    requests: Arc<Mutex<Vec<PermissionRequest>>>,
}

impl RecordingBroker {
    /// A broker that always answers this way.
    pub fn new(decision: BrokerDecision) -> Self {
        Self {
            decision,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Every request it was asked about, in order.
    pub fn requests(&self) -> Vec<PermissionRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait::async_trait]
impl PermissionBroker for RecordingBroker {
    async fn decide(&self, request: &PermissionRequest) -> BrokerDecision {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        self.decision.clone()
    }
}

/// A [`Clock`] that does not move unless a test moves it.
///
/// An event's timestamp becomes a value a test can assert on rather than a moving target.
///
/// # Example
///
/// ```
/// use mango_external_agents::testing::FrozenClock;
/// use mango_external_agents::Clock;
/// use std::time::{Duration, SystemTime};
///
/// let clock = FrozenClock::at(SystemTime::UNIX_EPOCH);
/// assert_eq!(clock.now(), SystemTime::UNIX_EPOCH);
/// clock.advance(Duration::from_secs(5));
/// assert_eq!(clock.now(), SystemTime::UNIX_EPOCH + Duration::from_secs(5));
/// ```
#[derive(Clone, Debug)]
pub struct FrozenClock {
    now: Arc<Mutex<SystemTime>>,
}

impl FrozenClock {
    /// A clock stopped at this instant.
    pub fn at(now: SystemTime) -> Self {
        Self {
            now: Arc::new(Mutex::new(now)),
        }
    }

    /// Moves it forward.
    pub fn advance(&self, by: std::time::Duration) {
        let mut now = self.now.lock().unwrap_or_else(PoisonError::into_inner);
        *now += by;
    }
}

impl Default for FrozenClock {
    fn default() -> Self {
        Self::at(SystemTime::UNIX_EPOCH)
    }
}

impl Clock for FrozenClock {
    fn now(&self) -> SystemTime {
        *self.now.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::{FrozenClock, RecordingBroker};
    use crate::event::ActivityKind;
    use crate::host::Clock;
    use crate::permission::{
        BrokerDecision, PermissionBroker, PermissionOption, PermissionOptionKind, PermissionRequest,
    };
    use std::time::{Duration, SystemTime};

    fn request() -> PermissionRequest {
        PermissionRequest {
            id: String::from("req-1"),
            kind: ActivityKind::Command,
            title: String::from("Run `ls`"),
            detail: None,
            options: vec![PermissionOption::new(
                "yes",
                PermissionOptionKind::AllowOnce,
            )],
            expires_at: SystemTime::UNIX_EPOCH,
            truncated: false,
        }
    }

    #[tokio::test]
    async fn records_every_question_and_answers_the_same_way() {
        let broker = RecordingBroker::new(BrokerDecision::Deny {
            reason: String::from("read-only workspace"),
        });

        let decision = broker.decide(&request()).await;
        assert!(matches!(decision, BrokerDecision::Deny { .. }));
        assert_eq!(broker.requests().len(), 1);
        assert_eq!(broker.requests()[0].id, "req-1");
    }

    #[test]
    fn a_frozen_clock_only_moves_when_a_test_moves_it() {
        let clock = FrozenClock::default();
        let before = clock.now();
        assert_eq!(clock.now(), before);

        clock.advance(Duration::from_secs(90));
        assert_eq!(clock.now(), before + Duration::from_secs(90));
    }
}
