//! How long a host should allow the ACP harness for stopping an agent.
//!
//! Nothing in ACP v1 bounds a stop, so every stage of one is bounded by the host's [`Limits`]:
//! `kill_grace`, `shutdown_timeout` and `request_timeout`. The stages add up, and a host that
//! puts its own timeout around `close`, around a cancel, or around "wait for `Closed`" needs the
//! sum. [`Deadlines`] is that sum, as [`Duration`]s, computed from the same `Limits` the harness
//! enforces, so a host reads the bound instead of re-deriving it.
//!
//! The figures are the worst-case rows of the guide's Deadlines tables (`docs/harness-acp.md`),
//! one method per row, and a test holds each to its row at the default limits. The stage values
//! the harness enforces are read through this module too (see the `pub(crate)` functions below),
//! so an enforced bound and the sum that counts it cannot drift apart.
//!
//! Every figure is an upper bound, and each says where it is measured from. A host that times
//! from the wrong event reads a figure that is too small: [`Deadlines::close`] is measured from the
//! call to `close`, and [`Deadlines::exit_to_closed`], which is longer, from the moment the agent
//! exits. All arithmetic saturates, so a limit set to [`Duration::MAX`] ("no bound") yields
//! [`Duration::MAX`] and never panics.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use mango_agent_acp::deadlines::Deadlines;
//! use mango_external_agents::Limits;
//!
//! let deadlines = Deadlines::from_limits(&Limits::default());
//! // A host's own timeout around `Session::close` allows the whole teardown.
//! assert_eq!(deadlines.close_worst_case(), Duration::from_secs(29));
//! ```

use std::time::Duration;

use mango_external_agents::Limits;

/// The worst-case time of each stop path, computed from one set of [`Limits`].
///
/// With `rt` for `request_timeout`, `st` for `shutdown_timeout` and `kg` for `kill_grace`:
///
/// | Method                                          | Worst case                   | At the defaults |
/// | ----------------------------------------------- | ---------------------------- | --------------- |
/// | [`process_stop`](Self::process_stop)            | `kg` + `kg` + `st` + `st`    | 14 s            |
/// | [`teardown`](Self::teardown)                    | `st` + process stop          | 19 s            |
/// | [`close`](Self::close)                          | `st` + teardown + `st`       | 29 s            |
/// | [`close_worst_case`](Self::close_worst_case)    | the same as `close`          | 29 s            |
/// | [`cancel_to_terminal`](Self::cancel_to_terminal)| `kg` + teardown              | 21 s            |
/// | [`request_timeout_to_closed`](Self::request_timeout_to_closed) | `rt` + teardown + `st` | 144 s |
/// | [`exit_to_closed`](Self::exit_to_closed)        | `st` + `st` + teardown + `st`| 34 s            |
/// | [`dropped_session_mid_turn`](Self::dropped_session_mid_turn)   | teardown      | 19 s            |
///
/// It holds a copy of the limits, so it is as current as the host context it was made from.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use mango_agent_acp::deadlines::Deadlines;
/// use mango_external_agents::Limits;
///
/// let limits = Limits {
///     shutdown_timeout: Duration::from_secs(10),
///     ..Limits::default()
/// };
/// // 10 s of connection close, then 2 + 2 s of grace and 10 + 10 s of kill and reap.
/// assert_eq!(Deadlines::from_limits(&limits).teardown(), Duration::from_secs(34));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
    limits: Limits,
}

impl Deadlines {
    /// The bounds the harness enforces under `limits`.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines, Deadlines::from_limits(&Limits::default()));
    /// ```
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self { limits: *limits }
    }

    /// The longest the child's stop can take, however the host's launcher implements it.
    ///
    /// Measured from the start of the stop. Formula: `kill_grace` for the graceful interrupt,
    /// `kill_grace` for the exit it should cause, `shutdown_timeout` for the kill, and
    /// `shutdown_timeout` for the reap. At `Limits::default()` that is 2 + 2 + 5 + 5 = 14 s.
    ///
    /// This is the formula that holds for any [`ProcessLauncher`](mango_external_agents::ProcessLauncher):
    /// the built-in launcher finishes sooner, a host's own control may use all of it. It repeats the
    /// stop the core crate's `process::stop_process_with_limits` enforces, and a test runs that
    /// function against a stalling control to prove the two agree.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.process_stop(), Duration::from_secs(14));
    /// ```
    #[must_use]
    pub fn process_stop(&self) -> Duration {
        process_stop_bound(&self.limits)
    }

    /// The longest the connection's end and the child's stop take together.
    ///
    /// Measured from the moment the harness begins the teardown: a `close` that reaches the
    /// connection, a request that passed its deadline, a turn that was cancelled and not honoured,
    /// a dropped session. Formula: `shutdown_timeout` to close the connection, then
    /// [`process_stop`](Self::process_stop). At `Limits::default()` that is 5 + 14 = 19 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.teardown(), Duration::from_secs(19));
    /// ```
    #[must_use]
    pub fn teardown(&self) -> Duration {
        connection_close_bound(&self.limits).saturating_add(self.process_stop())
    }

    /// The longest `Session::close` takes when the agent advertised `session/close`.
    ///
    /// Measured from the call to `close`. Formula: `shutdown_timeout` for the `session/close`
    /// request, then [`teardown`](Self::teardown), then `shutdown_timeout` for a turn another task
    /// is still settling. At `Limits::default()` that is 5 + 19 + 5 = 29 s.
    ///
    /// An upper bound: the last wait runs only after a cleanup that succeeded, and a stage that
    /// runs out its bound fails the cleanup, so the three terms are approached together by stages
    /// that finish just inside their limits. A host timing a `close` should use
    /// [`close_worst_case`](Self::close_worst_case), which names the same figure for every path.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.close(), Duration::from_secs(29));
    /// ```
    #[must_use]
    pub fn close(&self) -> Duration {
        session_close_bound(&self.limits)
            .saturating_add(self.teardown())
            .saturating_add(turn_release_bound(&self.limits))
    }

    /// The longest any `Session::close` call can take, whichever path the close joins.
    ///
    /// Measured from the call to `close`, which is what a host's own timeout around the call
    /// measures. It equals [`close`](Self::close). A close that joins a teardown another path
    /// already began (the connection-loss watcher, a dropped request, an earlier close) waits for
    /// that teardown, which began no later than the call, so it cannot run past it; and the close
    /// itself never waits for the agent's exit to be noticed, which is what makes a
    /// [`exit_to_closed`](Self::exit_to_closed) longer. A second call to `close` joins the first
    /// one's result under the same bound. At `Limits::default()` that is 29 s.
    ///
    /// The bound excludes time spent in the host's own code, such as a consumer that stops
    /// reading its event stream: the harness never waits for one.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.close_worst_case(), Duration::from_secs(29));
    /// assert_eq!(deadlines.close_worst_case(), deadlines.close());
    /// ```
    #[must_use]
    pub fn close_worst_case(&self) -> Duration {
        self.close()
    }

    /// How long a turn the agent ignores `session/cancel` for can take to write its terminal.
    ///
    /// Measured from the `session/cancel` the harness queues, which is when `Session::cancel`
    /// returns. Formula: `kill_grace` for the agent to honour it, then
    /// [`teardown`](Self::teardown). At `Limits::default()` that is 2 + 19 = 21 s. The same
    /// figure bounds the stop the idle timeout starts and a dropped `TurnStream`.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.cancel_to_terminal(), Duration::from_secs(21));
    /// ```
    #[must_use]
    pub fn cancel_to_terminal(&self) -> Duration {
        cancel_grace(&self.limits).saturating_add(self.teardown())
    }

    /// How long from the queueing of a request the agent never answers until the session is
    /// `Closed`.
    ///
    /// Measured from when the request is queued. Formula: `request_timeout`, then
    /// [`teardown`](Self::teardown), then `shutdown_timeout` for the turn another task is
    /// settling. At `Limits::default()` that is 120 + 19 + 5 = 144 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.request_timeout_to_closed(), Duration::from_secs(144));
    /// ```
    #[must_use]
    pub fn request_timeout_to_closed(&self) -> Duration {
        self.limits
            .request_timeout
            .saturating_add(self.teardown())
            .saturating_add(turn_release_bound(&self.limits))
    }

    /// How long from the agent's exit, or from a transport budget being passed, until the session
    /// is `Closed`, when no `close` was called.
    ///
    /// Measured from the agent's exit (its output ending) or the overflow event, not from any
    /// call. Formula: `shutdown_timeout` for the peer to hand the handler what it had already
    /// read, `shutdown_timeout` for a stretch the handler holds under
    /// `jsonrpc::Client::hold_cancellation` at that moment, then [`teardown`](Self::teardown), then
    /// `shutdown_timeout` for the turn's terminal. At `Limits::default()` that is
    /// 5 + 5 + 19 + 5 = 34 s.
    ///
    /// It is longer than [`close`](Self::close) because it starts earlier: a `close` called at
    /// the same moment starts the teardown itself and ends within
    /// [`close_worst_case`](Self::close_worst_case) of its own call.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.exit_to_closed(), Duration::from_secs(34));
    /// ```
    #[must_use]
    pub fn exit_to_closed(&self) -> Duration {
        exit_notice_bound(&self.limits)
            .saturating_add(self.teardown())
            .saturating_add(turn_release_bound(&self.limits))
    }

    /// How long from the drop of the last session handle on a running turn until that turn's
    /// terminal is written.
    ///
    /// Measured from the drop. The drop records `Shutdown` and begins the teardown at once, so the
    /// bound is [`teardown`](Self::teardown). At `Limits::default()` that is 19 s. The terminal
    /// is the held `TurnStream`'s `Cancelled { reason: Shutdown }`; the session's `Closed` follows
    /// within `shutdown_timeout` of it.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_acp::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.dropped_session_mid_turn(), Duration::from_secs(19));
    /// ```
    #[must_use]
    pub fn dropped_session_mid_turn(&self) -> Duration {
        self.teardown()
    }
}

/// How long a connection's close is given before the child goes to the reaper.
///
/// The peer's own close may take twice this; the harness gives it one `shutdown_timeout` and moves
/// on. Read by `ConnectionHandle::shutdown`.
///
/// ```ignore
/// tokio::time::timeout(connection_close_bound(&limits), client.close()).await
/// ```
pub(crate) fn connection_close_bound(limits: &Limits) -> Duration {
    limits.shutdown_timeout
}

/// How long `session/close` is given before the harness abandons it and carries on. Read by
/// `finish_close`.
///
/// ```ignore
/// tokio::time::timeout(session_close_bound(&limits), await_sent(..)).await
/// ```
pub(crate) fn session_close_bound(limits: &Limits) -> Duration {
    limits.shutdown_timeout
}

/// How long a turn another task is settling is waited for after a cleanup. Read by `finish_close`
/// and by the connection-loss watcher.
///
/// ```ignore
/// wait_for_turn_release(&state, turn_release_bound(&limits)).await
/// ```
pub(crate) fn turn_release_bound(limits: &Limits) -> Duration {
    limits.shutdown_timeout
}

/// How long an agent is given to honour `session/cancel` before the connection is torn down. Read
/// by the prompt task.
///
/// ```ignore
/// tokio::time::timeout(cancel_grace(&limits), &mut prompt).await
/// ```
pub(crate) fn cancel_grace(limits: &Limits) -> Duration {
    limits.kill_grace
}

/// The stop of the child: interrupt and its exit, kill and its reap.
///
/// Mirrors `mango_external_agents::process::stop_process_with_limits`, which the child reaper
/// calls and this crate cannot edit; a test runs that function against a stalling control and
/// holds the two together.
///
/// ```ignore
/// let stop = process_stop_bound(&limits);
/// ```
pub(crate) fn process_stop_bound(limits: &Limits) -> Duration {
    limits
        .kill_grace
        .saturating_mul(2)
        .saturating_add(limits.shutdown_timeout.saturating_mul(2))
}

/// How long the peer may take to tell the handler the agent is gone: one `shutdown_timeout` to
/// hand over what it had read, one for a stretch a handler holds. Enforced by the core's JSON-RPC
/// peer, which reads the same `shutdown_timeout`.
///
/// ```ignore
/// let notice = exit_notice_bound(&limits);
/// ```
pub(crate) fn exit_notice_bound(limits: &Limits) -> Duration {
    limits.shutdown_timeout.saturating_mul(2)
}

#[cfg(test)]
mod tests;
