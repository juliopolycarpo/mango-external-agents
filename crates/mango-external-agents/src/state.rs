//! What a session *is* right now, as opposed to what one turn said.
//!
//! Session state and turn transcript are different things and used to share a channel. The vendor
//! handle, the slash-command catalog, the settings in force and what the session can do are all
//! facts about the session; they change between turns, before the first one, and after the last.
//! Carrying them as turn events meant a host could only learn them by running a turn, and meant
//! inventing a turn id for something no turn produced.
//!
//! So they live here, behind two methods on [`Session`](crate::Session):
//!
//! - [`Session::snapshot`](crate::Session::snapshot) reads the current picture.
//! - [`Session::subscribe`](crate::Session::subscribe) watches it change.
//!
//! # The race that is not possible
//!
//! Read-then-subscribe is the classic way to lose an update: the change lands between the read and
//! the subscription and nobody ever hears about it. [`SessionSubscription`] is built the other way
//! round — subscribing *is* reading. A subscription is created from the live value, so
//! [`SessionSubscription::current`] answers with the snapshot the subscription was opened at, and
//! [`SessionSubscription::changed`] wakes for anything after it. There is no window between the
//! two calls because there are not two calls.
//!
//! Every snapshot also carries a [`SessionRevision`], which only ever increases. A consumer that
//! persists one and later sees a lower number is looking at a stale read, and one that sees a gap
//! knows updates were coalesced rather than lost — the last one wins, which is the right semantics
//! for a picture of the present.

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::sync::watch;

use crate::configuration::{ConfigurationCatalog, ConfigurationState};
use crate::event::Command;
use crate::harness::SessionCapabilities;
use crate::identity::HarnessIdentity;
use crate::session::SessionIds;
use crate::transport::TransportKind;

/// Which version of a session's picture this is.
///
/// Monotonic within one session and meaningless across sessions. A consumer compares two of them
/// to order what it has seen; it never reads one as a count of changes, because coalesced updates
/// share the revision of the last one to land.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionRevision(u64);

impl SessionRevision {
    /// The revision a session opens at.
    pub const INITIAL: Self = Self(0);

    /// The next revision after this one.
    ///
    /// Saturating rather than wrapping: a session that somehow reached `u64::MAX` updates would,
    /// on a wrap, start answering with revisions that read as older than what a host already has.
    /// A stuck counter is a bug nobody can act on; a counter that goes backwards is a bug that
    /// makes a host discard the truth.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The revision as a number, for persisting or comparing.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for SessionRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Where a session is in its life.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SessionStatus {
    /// Open and usable.
    #[default]
    Ready,
    /// Closing: no new turn will be accepted, and whatever was running is being wound down.
    Closing,
    /// Closed. Nothing more will happen on it.
    Closed,
}

impl SessionStatus {
    /// Whether a new turn would be accepted.
    pub const fn is_usable(self) -> bool {
        matches!(self, Self::Ready)
    }
}

impl fmt::Display for SessionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Ready => "ready",
            Self::Closing => "closing",
            Self::Closed => "closed",
        })
    }
}

/// Which carrier a session asked for and which one it got.
///
/// Both, because they are allowed to differ only in one direction: a host that asked for nothing
/// gets the harness's own preference, and a host that asked for something either gets it or is
/// refused. A session that quietly ran on a different carrier would leave a host debugging the
/// wrong connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TransportSelection {
    /// What the host asked for, when it asked for anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<TransportKind>,
    /// What the session is actually running on.
    pub effective: TransportKind,
}

impl TransportSelection {
    /// A session running on the carrier the host asked for, or on the harness's default.
    pub fn new(requested: Option<TransportKind>, effective: TransportKind) -> Self {
        Self {
            requested,
            effective,
        }
    }

    /// Whether the host asked for something other than what it got.
    ///
    /// Always false in practice — an undeclared request is refused at
    /// [`validate_open_session`](crate::Harness::validate_open_session) — and worth being able to
    /// assert.
    pub fn was_substituted(&self) -> bool {
        self.requested
            .is_some_and(|requested| requested != self.effective)
    }
}

/// Everything true about a session at one instant.
///
/// A value, not a view: reading two fields off one snapshot reads them from the same instant,
/// which a set of getters on a live session could not promise.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionSnapshot {
    /// Which version of the picture this is.
    pub revision: SessionRevision,
    /// The two ids this session answers to.
    pub ids: SessionIds,
    /// Which harness is driving it.
    pub harness: HarnessIdentity,
    /// Which carrier it asked for and which it is on.
    pub transport: TransportSelection,
    /// Where it is in its life.
    pub status: SessionStatus,
    /// What this session can do, after whatever the handshake narrowed.
    pub capabilities: SessionCapabilities,
    /// What it is set to, split by who says so.
    pub configuration: ConfigurationState,
    /// What it can be set to, as the vendor enumerates it.
    pub catalog: ConfigurationCatalog,
    /// The slash commands this session will expand, as the vendor announced them.
    ///
    /// Session state, not transcript: it says what a person may type next, so the last
    /// announcement wins and none of it is ever persisted as a message.
    pub commands: Vec<Command>,
    /// Whether the vendor continued a conversation rather than starting one.
    pub resumed: bool,
    /// Why a requested resume did not happen, when one was asked for and did not.
    pub fallback_reason: Option<String>,
    /// When this picture was taken, from the host's clock.
    pub observed_at: SystemTime,
}

impl SessionSnapshot {
    /// The picture a session opens with.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{
    ///     HarnessIdentity, SessionIds, SessionId, SessionRevision, SessionSnapshot,
    ///     TransportKind, TransportSelection,
    /// };
    /// use std::time::SystemTime;
    ///
    /// let snapshot = SessionSnapshot::opening(
    ///     SessionIds {
    ///         session_id: SessionId::new("chat-1"),
    ///         native_session_id: String::from("native-1"),
    ///     },
    ///     HarnessIdentity::claude(),
    ///     TransportSelection::new(None, TransportKind::Stdio),
    ///     SystemTime::UNIX_EPOCH,
    /// );
    /// assert_eq!(snapshot.revision, SessionRevision::INITIAL);
    /// assert!(snapshot.commands.is_empty());
    /// ```
    pub fn opening(
        ids: SessionIds,
        harness: HarnessIdentity,
        transport: TransportSelection,
        observed_at: SystemTime,
    ) -> Self {
        Self {
            revision: SessionRevision::INITIAL,
            ids,
            harness,
            transport,
            status: SessionStatus::Ready,
            capabilities: SessionCapabilities::none(),
            configuration: ConfigurationState::unknown(),
            catalog: ConfigurationCatalog::empty(),
            commands: Vec::new(),
            resumed: false,
            fallback_reason: None,
            observed_at,
        }
    }

    /// Records what this session can do.
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: SessionCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Records what it is set to.
    #[must_use]
    pub fn with_configuration(mut self, configuration: ConfigurationState) -> Self {
        self.configuration = configuration;
        self
    }

    /// Records what it can be set to.
    #[must_use]
    pub fn with_catalog(mut self, catalog: ConfigurationCatalog) -> Self {
        self.catalog = catalog;
        self
    }

    /// Records that the vendor continued a conversation rather than starting one.
    #[must_use]
    pub fn resumed(mut self) -> Self {
        self.resumed = true;
        self
    }

    /// Records why a requested resume did not happen.
    #[must_use]
    pub fn with_fallback_reason(mut self, reason: impl Into<String>) -> Self {
        self.fallback_reason = Some(reason.into());
        self
    }
}

/// The live, observable state of one session.
///
/// Held by the harness's session implementation and read through
/// [`Session::snapshot`](crate::Session::snapshot). Cheap to clone: every clone shares one
/// picture, so a reducer task and the session handle cannot disagree about what is current.
#[derive(Clone, Debug)]
pub struct SessionState {
    sender: watch::Sender<Arc<SessionSnapshot>>,
}

impl SessionState {
    /// A state holding this opening picture.
    pub fn new(snapshot: SessionSnapshot) -> Self {
        Self {
            sender: watch::Sender::new(Arc::new(snapshot)),
        }
    }

    /// The picture as it stands.
    pub fn snapshot(&self) -> Arc<SessionSnapshot> {
        Arc::clone(&self.sender.borrow())
    }

    /// A subscription that cannot miss a change made after it was opened.
    ///
    /// See the module documentation for why there is no read-then-subscribe window.
    pub fn subscribe(&self) -> SessionSubscription {
        SessionSubscription {
            receiver: self.sender.subscribe(),
        }
    }

    /// Applies a change and publishes the result, bumping the revision.
    ///
    /// The revision is bumped here rather than by the caller, so a harness cannot publish a
    /// changed picture under a revision a consumer has already seen.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{
    ///     HarnessIdentity, SessionId, SessionIds, SessionSnapshot, SessionState, SessionStatus,
    ///     TransportKind, TransportSelection,
    /// };
    /// use std::time::SystemTime;
    ///
    /// let state = SessionState::new(SessionSnapshot::opening(
    ///     SessionIds {
    ///         session_id: SessionId::new("chat-1"),
    ///         native_session_id: String::from("native-1"),
    ///     },
    ///     HarnessIdentity::claude(),
    ///     TransportSelection::new(None, TransportKind::Stdio),
    ///     SystemTime::UNIX_EPOCH,
    /// ));
    ///
    /// let before = state.snapshot().revision;
    /// state.update(|snapshot| snapshot.status = SessionStatus::Closing);
    /// assert!(state.snapshot().revision > before);
    /// assert_eq!(state.snapshot().status, SessionStatus::Closing);
    /// ```
    pub fn update(&self, change: impl FnOnce(&mut SessionSnapshot)) {
        self.sender.send_modify(|current| {
            let mut next = SessionSnapshot::clone(current);
            change(&mut next);
            next.revision = current.revision.next();
            *current = Arc::new(next);
        });
    }

    /// Records the vendor's own handle, which a vendor may mint after opening has answered.
    ///
    /// The shape Claude Code has: `--session-id` proposes one and the run's own announcement is
    /// free to report another, which is then the only handle a resume can use.
    pub fn set_native_session_id(&self, native_session_id: impl Into<String>) {
        let native_session_id = native_session_id.into();
        if self.snapshot().ids.native_session_id == native_session_id {
            return;
        }
        self.update(|snapshot| snapshot.ids.native_session_id = native_session_id);
    }

    /// Records the slash commands the vendor announced.
    ///
    /// The last announcement wins: this is what a person may type next, not a log of what the
    /// vendor has said about it.
    pub fn set_commands(&self, commands: Vec<Command>) {
        if self.snapshot().commands == commands {
            return;
        }
        self.update(|snapshot| snapshot.commands = commands);
    }

    /// Records what the session is now set to.
    pub fn set_configuration(&self, configuration: ConfigurationState) {
        if self.snapshot().configuration == configuration {
            return;
        }
        self.update(|snapshot| snapshot.configuration = configuration);
    }

    /// Records where the session is in its life.
    pub fn set_status(&self, status: SessionStatus) {
        if self.snapshot().status == status {
            return;
        }
        self.update(|snapshot| snapshot.status = status);
    }
}

/// A view of one session's state that wakes when it changes.
///
/// Coalescing: a consumer that is slow sees the latest picture rather than every intermediate one.
/// That is the right behaviour for a snapshot — an old picture of the present is not useful — and
/// it is why [`SessionRevision`] exists, so a consumer that cares can tell coalescing from
/// stillness.
#[derive(Clone, Debug)]
pub struct SessionSubscription {
    receiver: watch::Receiver<Arc<SessionSnapshot>>,
}

impl SessionSubscription {
    /// The picture as of this subscription's last wake-up, or of when it was opened.
    pub fn current(&self) -> Arc<SessionSnapshot> {
        Arc::clone(&self.receiver.borrow())
    }

    /// Waits for the next change and answers with it.
    ///
    /// `None` once the session's state is gone, which is how a consumer learns the session handle
    /// itself was dropped.
    pub async fn changed(&mut self) -> Option<Arc<SessionSnapshot>> {
        self.receiver.changed().await.ok()?;
        Some(Arc::clone(&self.receiver.borrow_and_update()))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SessionRevision, SessionSnapshot, SessionState, SessionStatus, SessionSubscription,
        TransportSelection,
    };
    use crate::configuration::{Configuration, ConfigurationState};
    use crate::event::{Command, SessionId};
    use crate::identity::HarnessIdentity;
    use crate::session::SessionIds;
    use crate::transport::TransportKind;
    use std::time::SystemTime;

    fn state() -> SessionState {
        SessionState::new(SessionSnapshot::opening(
            SessionIds {
                session_id: SessionId::new("chat-1"),
                native_session_id: String::from("native-1"),
            },
            HarnessIdentity::claude(),
            TransportSelection::new(None, TransportKind::Stdio),
            SystemTime::UNIX_EPOCH,
        ))
    }

    fn command(name: &str) -> Command {
        Command {
            name: String::from(name),
            description: None,
        }
    }

    /// The classic read-then-subscribe race: the change lands between the read and the
    /// subscription. It cannot happen here because subscribing *is* reading — the subscription is
    /// created from the live value, so anything published after it is one this consumer wakes for.
    #[tokio::test]
    async fn a_change_published_after_subscribing_is_never_lost() {
        let state = state();
        let mut subscription = state.subscribe();
        let opened_at = subscription.current().revision;

        state.set_commands(vec![command("review")]);

        let seen = subscription
            .changed()
            .await
            .expect("expected the change to reach the subscriber");
        assert!(seen.revision > opened_at);
        assert_eq!(seen.commands, vec![command("review")]);
    }

    /// A consumer that subscribes after a change has already landed sees it in `current()`, not as
    /// a missed wake-up. This is the other half of why there is no window: the subscription's
    /// first read is the live value, whatever happened before it.
    #[tokio::test]
    async fn a_change_published_before_subscribing_is_already_in_the_first_read() {
        let state = state();
        state.set_native_session_id("native-2");

        let subscription = state.subscribe();
        assert_eq!(subscription.current().ids.native_session_id, "native-2");
    }

    /// Several changes while a consumer is away coalesce into the latest picture. The revision is
    /// what lets it tell coalescing from stillness.
    #[tokio::test]
    async fn several_changes_coalesce_into_the_latest_picture() {
        let state = state();
        let mut subscription = state.subscribe();

        state.set_commands(vec![command("one")]);
        state.set_commands(vec![command("two")]);
        state.set_status(SessionStatus::Closing);

        let seen = subscription.changed().await.expect("expected a change");
        assert_eq!(seen.commands, vec![command("two")]);
        assert_eq!(seen.status, SessionStatus::Closing);
        assert_eq!(
            seen.revision.get(),
            3,
            "expected one revision per published change, received {}",
            seen.revision
        );
    }

    /// Session facts change outside any turn: before the first one, between two, and after the
    /// last. That is the whole reason they are not turn events.
    #[tokio::test]
    async fn session_state_changes_with_no_turn_anywhere_in_sight() {
        let state = state();
        let mut subscription = state.subscribe();

        state.set_configuration(
            ConfigurationState::unknown()
                .with_observed(Configuration::unknown().with_model("opus")),
        );

        let seen = subscription.changed().await.expect("expected a change");
        assert_eq!(
            seen.configuration.observed.model.as_deref(),
            Some("opus"),
            "expected a settings change with no turn to reach a subscriber"
        );
    }

    /// A republished identical value would wake every subscriber for nothing and burn a revision a
    /// host would read as a change.
    #[test]
    fn publishing_the_same_value_twice_does_not_bump_the_revision() {
        let state = state();
        state.set_commands(vec![command("review")]);
        let after_first = state.snapshot().revision;

        state.set_commands(vec![command("review")]);
        assert_eq!(state.snapshot().revision, after_first);

        state.set_native_session_id("native-1");
        assert_eq!(state.snapshot().revision, after_first);
    }

    #[test]
    fn a_revision_only_ever_goes_forward() {
        assert_eq!(SessionRevision::INITIAL.get(), 0);
        assert!(SessionRevision::INITIAL.next() > SessionRevision::INITIAL);
        assert_eq!(SessionRevision::INITIAL.next().to_string(), "1");
    }

    /// A host that asked for a carrier and silently got another would debug the wrong connection.
    #[test]
    fn a_selection_reports_what_was_asked_for_next_to_what_is_running() {
        let asked = TransportSelection::new(Some(TransportKind::Stdio), TransportKind::Stdio);
        assert!(!asked.was_substituted());

        let defaulted = TransportSelection::new(None, TransportKind::Stdio);
        assert_eq!(defaulted.requested, None);
        assert!(!defaulted.was_substituted());

        let substituted =
            TransportSelection::new(Some(TransportKind::WebSocket), TransportKind::Stdio);
        assert!(substituted.was_substituted());
    }

    #[tokio::test]
    async fn a_subscription_ends_when_the_session_state_is_dropped() {
        let state = state();
        let mut subscription: SessionSubscription = state.subscribe();
        drop(state);
        assert!(
            subscription.changed().await.is_none(),
            "expected the subscription to end with the session"
        );
    }

    #[test]
    fn a_closing_session_stops_being_usable() {
        assert!(SessionStatus::Ready.is_usable());
        assert!(!SessionStatus::Closing.is_usable());
        assert!(!SessionStatus::Closed.is_usable());
        assert_eq!(SessionStatus::Closing.to_string(), "closing");
    }
}
