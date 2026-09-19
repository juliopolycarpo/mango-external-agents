//! A reference host that makes the library's retry and recovery contract executable.
//!
//! **This is not a vendor harness and it is not published.** It drives no CLI, speaks no vendor
//! dialect and ships on no registry. It is the other side of the boundary: the *host* half of
//! what `docs/lifecycle.md` describes under "Safe host retries", written against nothing but the
//! public `mango_external_agents` API so that the contract can be read as code rather than as
//! prose.
//!
//! The division it demonstrates is the one the library insists on:
//!
//! | Owned by the library                               | Owned by this crate                        |
//! | -------------------------------------------------- | ------------------------------------------ |
//! | [`RequestFingerprint`], [`RecoveryRecord`]         | the loop that calls them                   |
//! | [`Dispatch`] certainty and its transition rules    | durable storage, backoff, jitter, deadlines |
//! | [`TurnStream`] and its terminal                    | who is allowed to watch it, and who is not |
//!
//! The **Hub** is this host's own control plane — the external service that owns its operations.
//! It is injected as the [`HubApi`] port and never constructed by the supervisor, so a test can
//! hand it a [`FakeHubApi`](testing::FakeHubApi) that drops acknowledgements, has no
//! reconciliation query, or refuses outright. The library knows nothing about it, and nothing
//! here reaches back into the library to add a `Backoff` type or a `retry_after` field.
//!
//! # Example
//!
//! ```
//! use hub_host::testing::{FakeHubApi, FakeVendorSession, ScriptedJitter};
//! use hub_host::{RetryPolicy, Settled, Stop, Supervisor};
//! use mango_external_agents::{SystemClock, TerminalStatus, TurnRequest};
//! use std::sync::Arc;
//! use std::time::Duration;
//!
//! let runtime = tokio::runtime::Builder::new_current_thread()
//!     .enable_time()
//!     .build()
//!     .expect("expected a current-thread runtime");
//! runtime.block_on(async {
//!     let hub = Arc::new(FakeHubApi::new());
//!     let policy = RetryPolicy::new(
//!         Duration::from_millis(1),
//!         Duration::from_millis(10),
//!         Duration::from_secs(5),
//!         Arc::new(ScriptedJitter::maximum()),
//!     );
//!     let mut supervisor = Supervisor::new(
//!         Box::new(FakeVendorSession::new()),
//!         Arc::clone(&hub) as Arc<dyn hub_host::HubApi>,
//!         policy,
//!         Arc::new(Stop::new()),
//!         Arc::new(SystemClock),
//!     );
//!     let settled = supervisor
//!         .run(TurnRequest::new("turn-1", "say hello"))
//!         .await
//!         .expect("expected the operation to settle");
//!     assert!(matches!(
//!         settled,
//!         Settled::Committed { terminal: TerminalStatus::Completed, .. }
//!     ));
//! });
//! ```
//!
//! [`RequestFingerprint`]: mango_external_agents::RequestFingerprint
//! [`RecoveryRecord`]: mango_external_agents::RecoveryRecord
//! [`Dispatch`]: mango_external_agents::Dispatch
//! [`TurnStream`]: mango_external_agents::TurnStream

pub mod hub;
pub mod retry;
pub mod stop;
pub mod subscriber;
pub mod supervisor;
pub mod testing;

pub use hub::{Commit, HubApi, HubError, HubReceipt, HubStatus, Reconciliation, RetryHint};
pub use retry::{HashJitter, Jitter, RetryPolicy, WaitOutcome};
pub use stop::Stop;
pub use subscriber::{TurnBroadcast, TurnSubscriber};
pub use supervisor::{
    SUGGESTED_ATTEMPT_DEADLINE, SUGGESTED_BASE_DELAY, SUGGESTED_MAX_DELAY, Settled, Supervisor,
};
