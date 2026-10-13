//! The worst-case times a host can wait on a Codex session, computed from its [`Limits`].
//!
//! `docs/harness-codex.md` carries these as formulas in its Deadlines tables. A host that wraps
//! its own timeout around `close` or `cancel` needs the numbers, and copying a formula by hand
//! lets it drift from the code. [`Deadlines`] is the same formula as code: every method is one
//! row of those tables, and the teardown's terms are the very expressions the teardown enforces
//! (`close_backstop` and `interrupt_bound` are called from `session.rs`).
//!
//! Opening a session has no method here: its `codex --version` probe (the host's `request_timeout`
//! plus [`Deadlines::process_stop`]) is stated in the doc's table and is not modelled by this type.
//!
//! Each method's documentation names where its clock starts. That is half of every bound: `close`
//! is bounded from the call to `close`, while a connection loss is bounded from the event.
//!
//! ```
//! use std::time::Duration;
//!
//! use mango_agent_codex::deadlines::Deadlines;
//! use mango_external_agents::Limits;
//!
//! let deadlines = Deadlines::from_limits(&Limits::default());
//! // A host's own timeout around `close` leaves room for the teardown to finish.
//! let host_timeout = deadlines.close_worst_case() + Duration::from_secs(1);
//! assert_eq!(host_timeout, Duration::from_secs(37));
//! ```
//!
//! Every sum saturates: limits near [`Duration::MAX`] give [`Duration::MAX`], never a panic.
//!
//! This is the bound the harness enforces, not a promise about a typical stop. The built-in
//! launcher's `TokioLauncher` stops a child sooner than the figures here, which hold for any
//! launcher the host injects.

use std::time::Duration;

use mango_external_agents::Limits;

/// How long the teardown waits for the discarded protocol interrupt it sends first.
///
/// One `kill_grace`. The answer is thrown away, so the wait only has to be long enough for the
/// request to leave; the teardown goes on when it expires.
///
/// ```ignore
/// let bound = interrupt_bound(&Limits::default());
/// assert_eq!(bound, Duration::from_secs(2));
/// ```
pub(crate) fn interrupt_bound(limits: &Limits) -> Duration {
    limits.kill_grace
}

/// How long the teardown gives the JSON-RPC client's close before it reaps the child anyway.
///
/// The client bounds its own close at twice `shutdown_timeout`: one for a notification call
/// that holds its cancellation off and for the answers still owed to the app-server, and one for
/// the queue and the link; a connection that had already ended has the report of that end waited
/// for inside the same two. A handler that blocks its thread costs the client up to 100 ms more,
/// 50 ms for each of two aborts it waits to land. This is one `shutdown_timeout` more than the
/// two, so a close that uses all of its own time, those 100 ms among it, still returns its own
/// result as long as `shutdown_timeout` is at least 100 ms, and the reap never depends on a client
/// that overran it. A bound equal to the client's would cut such a close short and report a timeout
/// for a teardown that delivered the terminal and reaped the child.
///
/// ```ignore
/// let limits = Limits::default();
/// assert_eq!(close_backstop(&limits), limits.shutdown_timeout * 3);
/// ```
pub(crate) fn close_backstop(limits: &Limits) -> Duration {
    limits.shutdown_timeout.saturating_mul(3)
}

/// Adds durations without overflowing.
fn total(terms: &[Duration]) -> Duration {
    terms
        .iter()
        .fold(Duration::ZERO, |sum, term| sum.saturating_add(*term))
}

/// The worst-case times of a Codex session, as `Duration`s computed from the host's [`Limits`].
///
/// Build one from the same `Limits` the host gave its `HostContext`, then read the row you need.
/// With `rt` for `request_timeout`, `st` for `shutdown_timeout`, `kg` for `kill_grace` and `cs`
/// for `cancel_settle_timeout`, the figures at the defaults are:
///
/// | Method                                | Formula                  | Defaults |
/// | ------------------------------------- | ------------------------ | -------- |
/// | [`process_stop`](Self::process_stop)  | `kg` + `kg` + `st` + `st` | 14 s    |
/// | [`teardown`](Self::teardown)          | `kg` + `st` + 3 `st` + stop | 36 s  |
/// | [`close`](Self::close)                | the teardown             | 36 s     |
/// | [`overflow_to_closed`](Self::overflow_to_closed) | `st` + the teardown | 41 s |
/// | [`connection_lost_to_closed`](Self::connection_lost_to_closed) | 2 `st` + the teardown | 46 s |
/// | [`cancel_interrupt_accepted`](Self::cancel_interrupt_accepted) | `rt` + `rt` + `cs` + teardown | 336 s |
/// | [`cancel_interrupt_failed`](Self::cancel_interrupt_failed) | 3 `rt` + `st` + teardown | 401 s |
///
/// ```
/// use std::time::Duration;
///
/// use mango_agent_codex::deadlines::Deadlines;
/// use mango_external_agents::Limits;
///
/// let limits = Limits {
///     shutdown_timeout: Duration::from_secs(10),
///     ..Limits::default()
/// };
/// // The same host, with a longer shutdown stage: 2 + 10 + 30 + (2 + 2 + 10 + 10).
/// assert_eq!(
///     Deadlines::from_limits(&limits).close_worst_case(),
///     Duration::from_secs(66)
/// );
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
    limits: Limits,
}

impl Deadlines {
    /// The deadlines of a session that runs under `limits`.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.teardown().as_secs(), 36);
    /// ```
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self { limits: *limits }
    }

    /// Stopping the child: a graceful interrupt and its wait, then the kill and its reap.
    ///
    /// Two `kill_grace` and two `shutdown_timeout`, 14 s at the defaults. This is core's
    /// `stop_process_with_limits`, and holds for any launcher. The clock starts when the stop
    /// begins.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let stop = Deadlines::from_limits(&Limits::default()).process_stop();
    /// assert_eq!(stop.as_secs(), 14);
    /// ```
    #[must_use]
    pub fn process_stop(&self) -> Duration {
        let Limits {
            kill_grace,
            shutdown_timeout,
            ..
        } = self.limits;
        total(&[kill_grace, kill_grace, shutdown_timeout, shutdown_timeout])
    }

    /// The teardown `T`: everything between a request to end the session and the child's reap.
    ///
    /// Its terms, in order: the discarded protocol interrupt (`kill_grace`), the wait for a
    /// terminal another task claimed (`shutdown_timeout`, only when there is one), the client's
    /// close (three `shutdown_timeout`: the client's own bound is two and the harness adds one as
    /// a backstop) and [`process_stop`](Self::process_stop). 36 s at the defaults.
    ///
    /// The clock starts when the teardown does. The third `shutdown_timeout` is a backstop for a
    /// client that overruns its own bound, so a client within its bound makes the teardown one
    /// `shutdown_timeout` shorter, 31 s at the defaults. The value here is the enforced ceiling.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let teardown = Deadlines::from_limits(&Limits::default()).teardown();
    /// assert_eq!(teardown.as_secs(), 36);
    /// ```
    #[must_use]
    pub fn teardown(&self) -> Duration {
        total(&[
            interrupt_bound(&self.limits),
            self.limits.shutdown_timeout,
            close_backstop(&self.limits),
            self.process_stop(),
        ])
    }

    /// `Session::close`, measured from the call to `close`.
    ///
    /// A close goes straight to the teardown: it starts it, or joins the one already running, so
    /// it never takes longer than [`teardown`](Self::teardown), 36 s at the defaults. That holds
    /// when the teardown it joins was started by an inbound overflow, a lost connection or an
    /// abandoned turn, because a teardown that has started has only its own stages left.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let close = Deadlines::from_limits(&Limits::default()).close();
    /// assert_eq!(close.as_secs(), 36);
    /// ```
    #[must_use]
    pub fn close(&self) -> Duration {
        self.teardown()
    }

    /// How long any `Session::close` can take, measured from the call to `close`.
    ///
    /// The maximum over every teardown a close can start or join, which is the teardown itself:
    /// [`close`](Self::close). A host that wraps its own timeout around `close` uses this and
    /// adds its own margin. A host that times from an event instead of from its `close` call
    /// uses [`overflow_to_closed`](Self::overflow_to_closed) for an inbound overflow.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let worst = Deadlines::from_limits(&Limits::default()).close_worst_case();
    /// assert_eq!(worst, Duration::from_secs(36));
    /// ```
    #[must_use]
    pub fn close_worst_case(&self) -> Duration {
        self.close()
    }

    /// An inbound overflow to the child's reap, measured from the overflow.
    ///
    /// The client first gives a notification call that holds its cancellation off one
    /// `shutdown_timeout` before it reports the connection's end, and the teardown starts after
    /// that report: `shutdown_timeout` plus [`teardown`](Self::teardown), 41 s at the defaults.
    /// A `close` called at any point inside that window is still bounded by
    /// [`close_worst_case`](Self::close_worst_case) from its own call.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let overflow = Deadlines::from_limits(&Limits::default()).overflow_to_closed();
    /// assert_eq!(overflow.as_secs(), 41);
    /// ```
    #[must_use]
    pub fn overflow_to_closed(&self) -> Duration {
        total(&[self.limits.shutdown_timeout, self.teardown()])
    }

    /// The app-server's exit or a failed link, to the child's reap, measured from the event.
    ///
    /// Those ends drain the queue for up to one `shutdown_timeout`, and a stretch still held when
    /// that runs out is waited for one more, so the client reports the connection's end within
    /// two. The teardown starts after that report: two `shutdown_timeout` plus
    /// [`teardown`](Self::teardown), 46 s at the defaults. A `close` called inside that window
    /// is still bounded by [`close_worst_case`](Self::close_worst_case) from its own call.
    ///
    /// This is the enforced ceiling. An exit behind a held stretch reaches the report at exactly
    /// two `shutdown_timeout` and the reap 12 s short of it at the defaults, because the ended
    /// turn leaves the teardown no interrupt to wait for and the given-up stretch no hold for the
    /// client's close.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let lost = Deadlines::from_limits(&Limits::default()).connection_lost_to_closed();
    /// assert_eq!(lost.as_secs(), 46);
    /// ```
    #[must_use]
    pub fn connection_lost_to_closed(&self) -> Duration {
        total(&[
            self.limits.shutdown_timeout,
            self.limits.shutdown_timeout,
            self.teardown(),
        ])
    }

    /// `Session::cancel` of a named turn whose interrupt the app-server accepted, measured from
    /// the call.
    ///
    /// The pending start (`request_timeout`), the interrupt (`request_timeout`), the settle
    /// deadline (`cancel_settle_timeout`) and then the teardown: 336 s at the defaults. A start
    /// whose answer was lost and that a notification names later pays one more interrupt and
    /// settle round, which this figure does not include.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let cancel = Deadlines::from_limits(&Limits::default()).cancel_interrupt_accepted();
    /// assert_eq!(cancel.as_secs(), 336);
    /// ```
    #[must_use]
    pub fn cancel_interrupt_accepted(&self) -> Duration {
        total(&[
            self.limits.request_timeout,
            self.limits.request_timeout,
            self.limits.cancel_settle_timeout,
            self.teardown(),
        ])
    }

    /// `Session::cancel` whose interrupt failed, measured from the call.
    ///
    /// The pending start, the interrupt and the steer hold (each `request_timeout`), the wait for
    /// a closing connection to fail the turn (`shutdown_timeout`) and then the teardown: 401 s
    /// at the defaults. The steer hold is paid only while a steer is in flight and the
    /// connection wait only while the connection is ending. A late-named start's extra round is
    /// not included.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_codex::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let cancel = Deadlines::from_limits(&Limits::default()).cancel_interrupt_failed();
    /// assert_eq!(cancel.as_secs(), 401);
    /// ```
    #[must_use]
    pub fn cancel_interrupt_failed(&self) -> Duration {
        total(&[
            self.limits.request_timeout,
            self.limits.request_timeout,
            self.limits.request_timeout,
            self.limits.shutdown_timeout,
            self.teardown(),
        ])
    }
}

#[cfg(test)]
pub(crate) mod stalls;
#[cfg(test)]
mod tests;
