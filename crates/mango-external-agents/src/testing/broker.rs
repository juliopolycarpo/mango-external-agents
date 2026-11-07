//! A broker that records what it was asked and answers from a script.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;
use tokio::sync::Notify;

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
    requested: Arc<Notify>,
}

impl RecordingBroker {
    /// A broker that always answers this way.
    pub fn new(decision: BrokerDecision) -> Self {
        Self {
            decision,
            requests: Arc::new(Mutex::new(Vec::new())),
            requested: Arc::new(Notify::new()),
        }
    }

    /// Every request it was asked about, in order.
    pub fn requests(&self) -> Vec<PermissionRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until at least `count` questions have reached this broker.
    ///
    /// Bound the wait in tests where failure could prevent the question from arriving.
    pub async fn wait_for_requests(&self, count: usize) {
        loop {
            let mut requested = std::pin::pin!(self.requested.notified());
            requested.as_mut().enable();
            let ready = self
                .requests
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .len()
                >= count;
            if ready {
                return;
            }
            requested.await;
        }
    }
}

#[async_trait::async_trait]
impl PermissionBroker for RecordingBroker {
    async fn decide(&self, request: &PermissionRequest) -> BrokerDecision {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        self.requested.notify_waiters();
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
    use crate::event::{ActivityKind, SessionId};
    use crate::host::Clock;
    use crate::interaction::{Interaction, InteractionId, InteractionKind};
    use crate::permission::{
        BrokerDecision, PermissionBroker, PermissionEffect, PermissionOption, PermissionRequest,
        PermissionScope,
    };
    use std::time::{Duration, SystemTime};

    fn request() -> PermissionRequest {
        PermissionRequest::new(
            Interaction::new(
                InteractionId::new("req-1"),
                InteractionKind::Permission,
                SessionId::new("chat-1"),
                SystemTime::UNIX_EPOCH,
            ),
            ActivityKind::Command,
            "Run `ls`",
            vec![
                PermissionOption::new("yes", PermissionEffect::Allow)
                    .with_scope(PermissionScope::Once),
            ],
        )
    }

    #[tokio::test]
    async fn records_every_question_and_answers_the_same_way() {
        let broker = RecordingBroker::new(BrokerDecision::Deny {
            reason: String::from("read-only workspace"),
        });

        let decision = broker.decide(&request()).await;
        assert!(matches!(decision, BrokerDecision::Deny { .. }));
        assert_eq!(broker.requests().len(), 1);
        assert_eq!(broker.requests()[0].id().as_str(), "req-1");
    }

    #[tokio::test]
    async fn request_waiters_acknowledge_recorded_questions_and_wake_every_waiter() {
        use std::future::Future;
        use std::task::Poll;
        let broker = RecordingBroker::new(BrokerDecision::Ask);
        broker.wait_for_requests(0).await;
        let mut tasks = Vec::new();
        let mut ready = Vec::new();
        for _ in 0..2 {
            let observed = broker.clone();
            let (registered, receiver) = tokio::sync::oneshot::channel();
            ready.push(receiver);
            tasks.push(tokio::spawn(async move {
                let mut wait = std::pin::pin!(observed.wait_for_requests(1));
                std::future::poll_fn(|cx| {
                    assert!(wait.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                registered.send(()).expect("waiting test");
                wait.await;
            }));
        }
        for registered in ready {
            registered.await.expect("waiter registration");
        }
        assert!(tasks.iter().all(|task| !task.is_finished()));
        assert!(matches!(
            broker.decide(&request()).await,
            BrokerDecision::Ask
        ));
        tokio::time::timeout(Duration::from_secs(1), async {
            for task in tasks {
                task.await.expect("notified waiter");
            }
        })
        .await
        .expect("every registered waiter must wake");
        broker.wait_for_requests(1).await;
        assert_eq!(broker.requests().len(), 1);
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
