//! How long the Claude harness can take, as `Duration`s computed from the host's [`Limits`].
//!
//! The Deadlines section of `docs/harness-claude.md` lists every stage of a stop, a close and a
//! turn's end, each bounded by `kill_grace`, `shutdown_timeout` or the idle floor, and adds them
//! into the worst case of each path. This module is those sums as code. A host that wraps a
//! `cancel`, a `close` or a drop in a timeout of its own asks [`Deadlines`] for the number instead
//! of re-deriving it from the guide, and the figures here are the guide's tables: at
//! `Limits::default()` they come to the values the tables print.
//!
//! The figures are the bounds that hold for **any** launcher. The core `TokioLauncher` finishes
//! sooner, because its interrupt returns at once and its kill answers only when the tree is gone;
//! that is a property of one launcher, so nothing here folds it in. Every method names its start
//! point, because a bound is only useful to a caller that knows when to start counting.
//!
//! Arithmetic saturates, so a `Limits` holding `Duration::MAX` yields `Duration::MAX` rather than
//! a panic.
//!
//! The session reads the expressions it enforces from here where they can be shared. The private
//! `launch_wait`, `removal_bound` and `exit_grace` are what the close and the stream pump wait on, and the idle floor is [`crate::pinned::stream_idle_timeout`], which
//! [`Deadlines::silent_stream_to_terminal`] reads too. The stop's own formula lives in the core
//! (`stop_process_with_limits`) and is repeated by [`Deadlines::process_stop`]; a test runs the core
//! function against a stalling control and compares it with this one.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use mango_agent_claude::deadlines::Deadlines;
//! use mango_external_agents::Limits;
//!
//! let deadlines = Deadlines::from_limits(&Limits::default());
//! // Give a close the whole of its worst case, counted from the call.
//! assert_eq!(deadlines.close_worst_case(), Duration::from_secs(24));
//! ```

use std::time::Duration;

use mango_external_agents::Limits;

/// How long a close waits for a launch the host's launcher has not returned yet.
///
/// The host's `shutdown_timeout`. A launcher that has not handed back a child by then leaves the
/// close with `Error::Timeout` naming the pending launch, and the session `Closing`.
pub(crate) fn launch_wait(limits: &Limits) -> Duration {
    limits.shutdown_timeout
}

/// How long a close waits for the removal of the session's MCP artifact.
///
/// The host's `shutdown_timeout`; the removal starts once the child is gone.
pub(crate) fn removal_bound(limits: &Limits) -> Duration {
    limits.shutdown_timeout
}

/// How long the stream pump reads on after the child exited, and how long it waits for an exit
/// status before it writes a failure's terminal.
///
/// The host's `kill_grace`.
pub(crate) fn exit_grace(limits: &Limits) -> Duration {
    limits.kill_grace
}

/// The worst-case time of each path of the Claude harness under one set of [`Limits`].
///
/// Built with [`from_limits`](Self::from_limits). Every method says where its clock starts, and
/// every figure is an upper bound that holds whatever launcher the host injected. A path ends
/// sooner when the child answers sooner.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use mango_agent_claude::deadlines::Deadlines;
/// use mango_external_agents::Limits;
///
/// let deadlines = Deadlines::from_limits(&Limits::default());
/// assert_eq!(deadlines.process_stop(), Duration::from_secs(14));
/// assert_eq!(deadlines.close_with_turn(), Duration::from_secs(19));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
    limits: Limits,
}

impl Deadlines {
    /// The bounds the Claude harness keeps under `limits`.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let limits = Limits { kill_grace: Duration::from_secs(1), ..Limits::default() };
    /// // Two kill graces and two shutdown stages: 2 * 1 s + 2 * 5 s.
    /// assert_eq!(Deadlines::from_limits(&limits).process_stop(), Duration::from_secs(12));
    /// ```
    #[must_use]
    pub fn from_limits(limits: &Limits) -> Self {
        Self { limits: *limits }
    }

    /// The longest one process stop takes, counted from the moment the stop begins.
    ///
    /// The stop is the core's `stop_process_with_limits`, which every Claude stop goes through:
    /// the interrupt call (`kill_grace`), the exit wait after a delivered interrupt (`kill_grace`),
    /// the kill (`shutdown_timeout`) and the reap (`shutdown_timeout`). Formula:
    /// `2 * kill_grace + 2 * shutdown_timeout`. This is the bound for any launcher, not the smaller
    /// figure the core `TokioLauncher` comes to. At `Limits::default()` it is 14 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.process_stop(), Duration::from_secs(14));
    /// ```
    #[must_use]
    pub fn process_stop(&self) -> Duration {
        // Core's `stop_process_with_limits`: two `kill_grace` and two `shutdown_timeout`.
        self.limits
            .kill_grace
            .saturating_mul(2)
            .saturating_add(self.limits.shutdown_timeout.saturating_mul(2))
    }

    /// The teardown of a child, counted from the moment the teardown begins.
    ///
    /// The Claude harness has no protocol stop before the process stop, so its teardown is the
    /// stop: the same value as [`process_stop`](Self::process_stop). The method exists so a host
    /// that reads the teardown row of every harness's guide finds it under the same name.
    /// Formula: `2 * kill_grace + 2 * shutdown_timeout`. At `Limits::default()` it is 14 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.teardown(), Duration::from_secs(14));
    /// ```
    #[must_use]
    pub fn teardown(&self) -> Duration {
        self.process_stop()
    }

    /// From the call to `Session::cancel` on a running turn, until the turn's terminal is written.
    ///
    /// Zero: the pump writes `Cancelled` and the terminal as soon as the stop is recorded, without
    /// waiting for the process. At `Limits::default()` it is zero.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.cancel_to_terminal(), Duration::ZERO);
    /// ```
    #[must_use]
    pub fn cancel_to_terminal(&self) -> Duration {
        Duration::ZERO
    }

    /// From the call to `Session::cancel` on a running turn, until `cancel` returns.
    ///
    /// `cancel` returns when the stop does. Formula: [`process_stop`](Self::process_stop). A
    /// `cancel` that finds the launch still pending returns `Ok` at once, so this is the bound for
    /// a turn whose child is installed. At `Limits::default()` it is 14 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.cancel_to_return(), Duration::from_secs(14));
    /// ```
    #[must_use]
    pub fn cancel_to_return(&self) -> Duration {
        self.process_stop()
    }

    /// From the call to `Session::close` on a session with no turn, until `close` returns.
    ///
    /// Only the removal of the session's MCP artifact is left to wait for. Formula:
    /// `shutdown_timeout`. At `Limits::default()` it is 5 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.close_no_turn(), Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn close_no_turn(&self) -> Duration {
        removal_bound(&self.limits)
    }

    /// From the call to `Session::close` while a turn's child is installed, until `close` returns.
    ///
    /// The stop, then the artifact removal. Formula: `process_stop + shutdown_timeout`. A stop that
    /// fails skips the removal, so the removal term is spent only by a stop that succeeded. At
    /// `Limits::default()` it is 19 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.close_with_turn(), Duration::from_secs(19));
    /// ```
    #[must_use]
    pub fn close_with_turn(&self) -> Duration {
        self.process_stop()
            .saturating_add(removal_bound(&self.limits))
    }

    /// From the call to `Session::close` while the turn's launch is still inside the host's
    /// launcher, until `close` returns, in the branch where the child arrives in time.
    ///
    /// The close waits `shutdown_timeout` for the launcher, then the stop of the child that arrived
    /// before that wait ended, then the removal. Formula:
    /// `shutdown_timeout + process_stop + shutdown_timeout`. At `Limits::default()` it is 24 s.
    ///
    /// The other branch, a launcher that returns nothing within `shutdown_timeout`, ends the close
    /// at [`close_no_turn`](Self::close_no_turn)'s figure with `Error::Timeout` naming the pending
    /// launch and leaves the session `Closing`.
    ///
    /// The 24 s branch needs the stop to have started by the end of the first wait, and the stop
    /// starts when the `start_turn` call that owns the launch runs again after the launcher
    /// returned, or is dropped. A `start_turn` future that is kept and never polled again starts no
    /// stop, and the close takes the short branch. The close never waits longer than this figure
    /// either way. This is a ceiling: the start that owns the launch holds a lease on the artifact
    /// until its own stop returns, and a close that finds the lease still held has nothing to
    /// remove, so the last `shutdown_timeout` is spent only when the start releases first. With
    /// the fake launcher the close returns after the wait and the stop alone, 19 s at the
    /// defaults.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.close_pending_launch(), Duration::from_secs(24));
    /// ```
    #[must_use]
    pub fn close_pending_launch(&self) -> Duration {
        launch_wait(&self.limits)
            .saturating_add(self.process_stop())
            .saturating_add(removal_bound(&self.limits))
    }

    /// How long any `Session::close` can take, counted from the call to `close`.
    ///
    /// The largest of [`close_no_turn`](Self::close_no_turn),
    /// [`close_with_turn`](Self::close_with_turn) and
    /// [`close_pending_launch`](Self::close_pending_launch). A host that puts a timeout around
    /// `close` uses this one. At `Limits::default()` it is 24 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.close(), Duration::from_secs(24));
    /// ```
    #[must_use]
    pub fn close(&self) -> Duration {
        self.close_worst_case()
    }

    /// How long any `Session::close` can take, counted from the call to `close`.
    ///
    /// The same value as [`close`](Self::close): the largest of the three close rows. At
    /// `Limits::default()` it is 24 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.close_worst_case(), Duration::from_secs(24));
    /// ```
    #[must_use]
    pub fn close_worst_case(&self) -> Duration {
        self.close_no_turn()
            .max(self.close_with_turn())
            .max(self.close_pending_launch())
    }

    /// From the child's exit, or the end of its stream, until the turn's terminal is written.
    ///
    /// The pump reads on for `kill_grace` after an exit it saw, and waits `kill_grace` for an exit
    /// status when the stream ended under a child that has not exited. Formula: `kill_grace`. A
    /// stream that keeps producing lines is read for as long as it does. At `Limits::default()` it
    /// is 2 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.exit_to_terminal(), Duration::from_secs(2));
    /// ```
    #[must_use]
    pub fn exit_to_terminal(&self) -> Duration {
        exit_grace(&self.limits)
    }

    /// From the child's exit, or the end of its stream, until the child is reaped.
    ///
    /// The terminal first, then the post-terminal stop. Formula: `kill_grace + process_stop`. At
    /// `Limits::default()` it is 16 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.exit_to_reaped(), Duration::from_secs(16));
    /// ```
    #[must_use]
    pub fn exit_to_reaped(&self) -> Duration {
        exit_grace(&self.limits).saturating_add(self.process_stop())
    }

    /// From the last line read from a silent child, until the turn's terminal is written.
    ///
    /// The idle timer restarts on every record, so the clock starts at the last record (at the
    /// close of the prompt's stdin for a child that has said nothing). The timer is the host's
    /// `idle_timeout` raised to the harness's ten-minute floor, as
    /// [`crate::pinned::stream_idle_timeout`] computes it, and the failure's terminal waits
    /// `kill_grace` for an exit status. Formula: `max(idle_timeout, 10 min) + kill_grace`. At
    /// `Limits::default()` it is 602 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.silent_stream_to_terminal(), Duration::from_secs(602));
    /// ```
    #[must_use]
    pub fn silent_stream_to_terminal(&self) -> Duration {
        crate::pinned::stream_idle_timeout(&self.limits).saturating_add(exit_grace(&self.limits))
    }

    /// From the host's shutdown token being cancelled, until the running turn's terminal is
    /// written.
    ///
    /// The pump stops the child before it writes the terminal, so the terminal waits for the stop.
    /// Formula: [`process_stop`](Self::process_stop). At `Limits::default()` it is 14 s.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.host_shutdown_to_terminal(), Duration::from_secs(14));
    /// ```
    #[must_use]
    pub fn host_shutdown_to_terminal(&self) -> Duration {
        self.process_stop()
    }

    /// From the drop of the last handle of a session that has a child installed, until that child
    /// is reaped.
    ///
    /// The stop runs on the runtime that opened the session. Formula:
    /// [`process_stop`](Self::process_stop). At `Limits::default()` it is 14 s. A session dropped
    /// while its launch is still pending stops the child only after the host's launcher returns
    /// it, and the library does not bound that launcher.
    ///
    /// # Example
    ///
    /// ```
    /// use std::time::Duration;
    /// use mango_agent_claude::deadlines::Deadlines;
    /// use mango_external_agents::Limits;
    ///
    /// let deadlines = Deadlines::from_limits(&Limits::default());
    /// assert_eq!(deadlines.dropped_session_to_reaped(), Duration::from_secs(14));
    /// ```
    #[must_use]
    pub fn dropped_session_to_reaped(&self) -> Duration {
        self.process_stop()
    }
}

#[cfg(test)]
mod tests;
