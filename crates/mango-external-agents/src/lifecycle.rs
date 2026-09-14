//! The close boundary shared by session harnesses.
//!
//! A session can start work asynchronously, so an atomic "closed" flag alone does not prevent a
//! close from landing between a start's check and its claim of the vendor resource. Harnesses hold
//! this gate while they synchronously claim or release that resource; they release it before every
//! await.

use std::sync::{Mutex, MutexGuard, PoisonError};

/// Serialises a session's start and close transitions.
///
/// A harness holds the returned guard while it synchronously reserves a turn or takes work for
/// teardown. It must release the guard before awaiting vendor or host I/O.
///
/// # Example
///
/// ```
/// use mango_external_agents::SessionLifecycle;
///
/// let lifecycle = SessionLifecycle::default();
/// let mut transition = lifecycle.lock();
/// assert!(transition.close());
/// ```
#[derive(Default)]
pub struct SessionLifecycle {
    state: Mutex<SessionLifecycleState>,
}

#[derive(Default)]
struct SessionLifecycleState {
    closed: bool,
}

/// One synchronous session start or close transition.
pub struct SessionLifecycleGuard<'a> {
    state: MutexGuard<'a, SessionLifecycleState>,
}

impl SessionLifecycle {
    /// Takes the gate for one synchronous start or close transition.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::SessionLifecycle;
    ///
    /// let lifecycle = SessionLifecycle::default();
    /// assert!(!lifecycle.lock().is_closed());
    /// ```
    #[must_use]
    pub fn lock(&self) -> SessionLifecycleGuard<'_> {
        SessionLifecycleGuard {
            state: self.state.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }

    /// Whether a close transition has already won.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::SessionLifecycle;
    ///
    /// let lifecycle = SessionLifecycle::default();
    /// assert!(!lifecycle.is_closed());
    /// ```
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lock().is_closed()
    }

    /// Takes the start transition when the session is still open.
    ///
    /// `None` means a close won after an earlier caller's observation but before this synchronous
    /// claim. A harness holds the returned guard while it reserves its vendor resource.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::SessionLifecycle;
    ///
    /// let lifecycle = SessionLifecycle::default();
    /// assert!(lifecycle.begin_start().is_some());
    /// lifecycle.lock().close();
    /// assert!(lifecycle.begin_start().is_none());
    /// ```
    #[must_use]
    pub fn begin_start(&self) -> Option<SessionLifecycleGuard<'_>> {
        let guard = self.lock();
        (!guard.is_closed()).then_some(guard)
    }
}

impl SessionLifecycleGuard<'_> {
    /// Whether a close transition has already won while this gate is held.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::SessionLifecycle;
    ///
    /// let lifecycle = SessionLifecycle::default();
    /// let transition = lifecycle.lock();
    /// assert!(!transition.is_closed());
    /// ```
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.closed
    }

    /// Claims the close transition.
    ///
    /// Returns `true` for the caller that first closes the session and `false` for every later
    /// caller.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::SessionLifecycle;
    ///
    /// let lifecycle = SessionLifecycle::default();
    /// assert!(lifecycle.lock().close());
    /// assert!(!lifecycle.lock().close());
    /// ```
    #[must_use]
    pub fn close(&mut self) -> bool {
        !std::mem::replace(&mut self.state.closed, true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;

    use super::SessionLifecycle;

    #[test]
    fn a_close_waits_for_an_in_progress_start_claim_then_prevents_the_next_one() {
        let lifecycle = Arc::new(SessionLifecycle::default());
        let (start_entered, start_entered_by_test) = mpsc::channel();
        let (release_start_by_test, release_start) = mpsc::channel();
        let (start_observed_close, start_observed_by_test) = mpsc::channel();
        let starting_lifecycle = Arc::clone(&lifecycle);
        let starting = thread::spawn(move || {
            let transition = starting_lifecycle.lock();
            start_entered
                .send(())
                .expect("expected the test to observe start");
            release_start
                .recv()
                .expect("expected the test to release start");
            start_observed_close
                .send(transition.is_closed())
                .expect("expected the test to observe start's result");
        });

        start_entered_by_test
            .recv()
            .expect("expected start to hold the gate");
        let (closed, close_finished) = mpsc::channel();
        let closing_lifecycle = Arc::clone(&lifecycle);
        let closing = thread::spawn(move || {
            let mut transition = closing_lifecycle.lock();
            closed
                .send(transition.close())
                .expect("expected the test to observe close's result");
        });

        assert!(
            close_finished.try_recv().is_err(),
            "close must wait until the in-progress start releases the gate"
        );
        release_start_by_test
            .send(())
            .expect("expected the test to release start");
        assert!(
            !start_observed_by_test
                .recv()
                .expect("expected the start transition to finish"),
            "a close that was waiting must not change an in-progress start claim"
        );
        starting.join().expect("expected start thread to finish");
        assert!(
            close_finished.recv().expect("expected close to finish"),
            "the first close must claim the lifecycle"
        );
        closing.join().expect("expected close thread to finish");
        assert!(
            lifecycle.is_closed(),
            "a later start claim must observe the closed lifecycle"
        );
        assert!(
            !lifecycle.lock().close(),
            "a second close must leave the lifecycle claimed by the first one"
        );
    }

    #[test]
    fn a_close_after_an_open_check_refuses_the_start_claim() {
        let lifecycle = Arc::new(SessionLifecycle::default());
        let (checked_open, observed_by_test) = mpsc::channel();
        let (release_start, released_by_test) = mpsc::channel();
        let (claimed_start, claimed_by_test) = mpsc::channel();
        let starting_lifecycle = Arc::clone(&lifecycle);
        let starting = thread::spawn(move || {
            assert!(
                !starting_lifecycle.is_closed(),
                "expected the start's first observation to see an open session"
            );
            checked_open
                .send(())
                .expect("expected the test to observe start's first check");
            released_by_test
                .recv()
                .expect("expected the test to release start's claim");
            claimed_start
                .send(starting_lifecycle.begin_start().is_some())
                .expect("expected the test to observe start's claim");
        });

        observed_by_test
            .recv()
            .expect("expected the start's first check");
        assert!(
            lifecycle.lock().close(),
            "expected close to win before start claimed its vendor resource"
        );
        release_start
            .send(())
            .expect("expected the test to release start's claim");
        assert!(
            !claimed_by_test
                .recv()
                .expect("expected the start claim to finish"),
            "a start that lost the close race must not claim a vendor resource"
        );
        starting.join().expect("expected start thread to finish");
    }
}
