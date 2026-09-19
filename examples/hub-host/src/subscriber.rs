//! The fan-out between the operation and whatever is watching it.
//!
//! The library is explicit that dropping a [`TurnStream`](mango_external_agents::TurnStream) is
//! abandonment and that a browser disconnect must only detach from the host's supervisor. That is
//! only enforceable if the browser is holding something *other* than the stream — so the
//! supervisor keeps the stream and hands every watcher one of these instead. Dropping the last one
//! is an ordinary state of the world, not an error.

use std::sync::Arc;

use mango_external_agents::AgentEvent;
use tokio::sync::broadcast;

/// The supervisor's side of the fan-out.
///
/// Publishing to nobody succeeds. `broadcast::Sender::send` reports "no receivers" as an `Err`,
/// and a supervisor that propagated it with `?` would abandon an operation the moment a browser
/// tab closed — which is the exact failure this type exists to make impossible.
#[derive(Debug)]
pub struct TurnBroadcast {
    sender: broadcast::Sender<Arc<AgentEvent>>,
}

impl TurnBroadcast {
    /// A fan-out holding at most `capacity` events for a subscriber that has fallen behind.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// let events = TurnBroadcast::new(16);
    /// assert_eq!(events.subscriber_count(), 0);
    /// ```
    pub fn new(capacity: usize) -> Self {
        Self {
            sender: broadcast::Sender::new(capacity),
        }
    }

    /// One watcher's handle, which it may drop at any time.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// let events = TurnBroadcast::new(16);
    /// let watcher = events.subscribe();
    /// assert_eq!(events.subscriber_count(), 1);
    /// drop(watcher);
    /// assert_eq!(events.subscriber_count(), 0);
    /// ```
    pub fn subscribe(&self) -> TurnSubscriber {
        TurnSubscriber {
            receiver: self.sender.subscribe(),
        }
    }

    /// How many watchers are attached right now.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// assert_eq!(TurnBroadcast::new(4).subscriber_count(), 0);
    /// ```
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }

    /// Offers one event to every watcher, ignoring the fact that there may be none.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    /// use mango_external_agents::{AttemptId, EventKind, EventSink, SessionId, SystemClock, TurnId};
    /// use std::sync::Arc;
    ///
    /// let runtime = tokio::runtime::Builder::new_current_thread()
    ///     .build()
    ///     .expect("expected a current-thread runtime");
    /// runtime.block_on(async {
    ///     let (sink, mut source) = EventSink::new(
    ///         SessionId::new("chat-1"),
    ///         TurnId::new("turn-1"),
    ///         AttemptId::FIRST,
    ///         Arc::new(SystemClock),
    ///         4,
    ///     );
    ///     sink.emit(EventKind::TextDelta { text: String::from("hello") })
    ///         .await
    ///         .expect("expected the sink to take the event");
    ///     let event = source.try_recv().expect("expected the event back");
    ///
    ///     // Nobody is watching, and publishing still succeeds.
    ///     let events = TurnBroadcast::new(4);
    ///     assert_eq!(events.subscriber_count(), 0);
    ///     events.publish(event);
    /// });
    /// ```
    pub fn publish(&self, event: AgentEvent) {
        let _ = self.sender.send(Arc::new(event));
    }
}

/// One watcher's handle on a running operation.
///
/// Dropping it detaches that watcher. It does not cancel, abandon or even slow the operation: a
/// lagged subscriber is told it lagged and carries on.
#[derive(Debug)]
pub struct TurnSubscriber {
    receiver: broadcast::Receiver<Arc<AgentEvent>>,
}

impl TurnSubscriber {
    /// The next event, or `None` once the supervisor has finished publishing.
    ///
    /// A watcher that fell too far behind skips to the oldest event still held rather than being
    /// disconnected, because a UI that missed three deltas still wants the fourth.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// let runtime = tokio::runtime::Builder::new_current_thread()
    ///     .build()
    ///     .expect("expected a current-thread runtime");
    /// runtime.block_on(async {
    ///     let events = TurnBroadcast::new(4);
    ///     let mut watcher = events.subscribe();
    ///     drop(events);
    ///     assert!(watcher.recv().await.is_none());
    /// });
    /// ```
    pub async fn recv(&mut self) -> Option<Arc<AgentEvent>> {
        loop {
            return match self.receiver.recv().await {
                Ok(event) => Some(event),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => None,
            };
        }
    }

    /// The next event if one is already waiting, without suspending the caller.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::TurnBroadcast;
    ///
    /// let events = TurnBroadcast::new(4);
    /// let mut watcher = events.subscribe();
    /// assert!(watcher.try_recv().is_none());
    /// ```
    pub fn try_recv(&mut self) -> Option<Arc<AgentEvent>> {
        loop {
            return match self.receiver.try_recv() {
                Ok(event) => Some(event),
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(
                    broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed,
                ) => None,
            };
        }
    }
}
