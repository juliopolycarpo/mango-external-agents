//! The host's stop signal, and why it was pulled.
//!
//! [`CancelToken`] is a bare flag: it says that work must end, not which of the three things that
//! end work happened. A host has to tell them apart — an explicit abort, a revoked consent and an
//! owner shutdown are three different rows in an audit log and three different things to show a
//! person — so this pairs the library's token with the library's own [`CancelReason`] vocabulary
//! rather than adding a reason to the library.

use std::sync::OnceLock;

use mango_external_agents::{CancelReason, CancelToken};

/// One operation's stop signal.
///
/// Stopping is final and idempotent: the first reason wins, so a shutdown racing an abort cannot
/// rewrite what a later reader sees.
#[derive(Debug, Default)]
pub struct Stop {
    token: CancelToken,
    reason: OnceLock<CancelReason>,
}

impl Stop {
    /// A signal nobody has pulled.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::Stop;
    ///
    /// let stop = Stop::new();
    /// assert!(!stop.is_stopped());
    /// assert!(stop.reason().is_none());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Pulls it, waking everything waiting. Idempotent; the first reason is kept.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::Stop;
    /// use mango_external_agents::CancelReason;
    ///
    /// let stop = Stop::new();
    /// stop.stop(CancelReason::ConsentRevoked);
    /// stop.stop(CancelReason::Shutdown);
    /// assert_eq!(stop.reason(), Some(CancelReason::ConsentRevoked));
    /// ```
    pub fn stop(&self, reason: CancelReason) {
        let _ = self.reason.set(reason);
        self.token.cancel();
    }

    /// Whether it has been pulled.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::Stop;
    /// use mango_external_agents::CancelReason;
    ///
    /// let stop = Stop::new();
    /// stop.stop(CancelReason::Requested);
    /// assert!(stop.is_stopped());
    /// ```
    pub fn is_stopped(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Why it was pulled, once it has been.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::Stop;
    /// use mango_external_agents::CancelReason;
    ///
    /// let stop = Stop::new();
    /// stop.stop(CancelReason::Timeout);
    /// assert_eq!(stop.reason(), Some(CancelReason::Timeout));
    /// ```
    pub fn reason(&self) -> Option<CancelReason> {
        self.reason.get().copied()
    }

    /// Resolves once it is pulled, immediately if it already was.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::Stop;
    /// use mango_external_agents::CancelReason;
    ///
    /// let runtime = tokio::runtime::Builder::new_current_thread()
    ///     .build()
    ///     .expect("expected a current-thread runtime");
    /// runtime.block_on(async {
    ///     let stop = Stop::new();
    ///     stop.stop(CancelReason::Shutdown);
    ///     stop.stopped().await;
    /// });
    /// ```
    pub async fn stopped(&self) {
        self.token.cancelled().await;
    }

    /// The library's own token, for the calls that take one.
    ///
    /// # Example
    ///
    /// ```
    /// use hub_host::Stop;
    ///
    /// let stop = Stop::new();
    /// assert!(!stop.token().is_cancelled());
    /// ```
    pub fn token(&self) -> &CancelToken {
        &self.token
    }
}
