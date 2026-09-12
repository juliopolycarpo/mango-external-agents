//! A live conversation with one vendor CLI, and the typed reasons it ends by.
//!
//! The library hands a host [`Session`] handles and reason enums; it keeps no
//! registry of live sessions, polls no consent and fans nothing out to a hub. Those are host
//! policy, and a host builds whatever registry it needs on top of these handles.

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::event::{AccountLimits, SessionId, TurnId};
use crate::harness::{Capabilities, Capability};
use crate::normalize::{self, TextLimit};
use crate::permission::{ApprovalRouting, PermissionLevel, PermissionResponse};
use crate::stream::{ReviewStream, TurnStream};

/// Why a turn was stopped.
///
/// A reason enum rather than a message: the library never returns copy, and a host maps these to
/// its own words. The distinction between the four is load-bearing — "you stopped this turn" is a
/// lie for a shutdown, and a timeout is not a user's decision.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CancelReason {
    /// Somebody asked for it: a stop button, an API call.
    Requested,
    /// The machine's owner withdrew permission to run external agents.
    ConsentRevoked,
    /// The turn passed a deadline the host set.
    Timeout,
    /// The host is going away.
    Shutdown,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Requested => "requested",
            Self::ConsentRevoked => "consent revoked",
            Self::Timeout => "timeout",
            Self::Shutdown => "shutdown",
        })
    }
}

/// Why a session was closed.
///
/// One reason shorter than [`CancelReason`]: a timeout ends a turn, never a session. A session
/// with a stalled turn is still a session, and closing it would throw away the vendor
/// conversation the host may still resume.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum CloseReason {
    /// Somebody asked for it.
    Requested,
    /// The machine's owner withdrew permission to run external agents.
    ConsentRevoked,
    /// The host is going away.
    Shutdown,
}

impl fmt::Display for CloseReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Requested => "requested",
            Self::ConsentRevoked => "consent revoked",
            Self::Shutdown => "shutdown",
        })
    }
}

impl From<CloseReason> for CancelReason {
    /// Closing a session cancels whatever turn was running, for the same reason.
    fn from(reason: CloseReason) -> Self {
        match reason {
            CloseReason::Requested => Self::Requested,
            CloseReason::ConsentRevoked => Self::ConsentRevoked,
            CloseReason::Shutdown => Self::Shutdown,
        }
    }
}

/// The two ids one session answers to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionIds {
    /// The host's own id, which the host minted and can rely on.
    pub session_id: SessionId,
    /// The vendor's own handle, opaque and the vendor's to recycle.
    pub native_session_id: String,
}

/// Settings that can change between turns without opening a new session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Configuration {
    /// The vendor's own model id, when the host chose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The vendor's own reasoning-effort id, when the host chose one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// What the agent may do.
    pub level: PermissionLevel,
    /// Who answers its prompts.
    pub routing: ApprovalRouting,
}

impl Default for Configuration {
    /// The narrow end of both axes.
    ///
    /// A default that granted more than the most restrictive pair would be a library deciding
    /// something only a person can.
    fn default() -> Self {
        Self {
            model: None,
            effort: None,
            level: PermissionLevel::RESTRICTIVE,
            routing: ApprovalRouting::RESTRICTIVE,
        }
    }
}

/// Whether a session that cannot be resumed should start fresh or fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ResumeMode {
    /// Fail if the vendor cannot resume this conversation.
    Strict,
    /// Start a new conversation instead, and say so in
    /// [`SessionInfo::fallback_reason`].
    Fallback,
}

/// A vendor conversation to continue.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resume {
    /// The vendor's own handle for it.
    pub native_session_id: String,
    /// What to do when the vendor will not.
    pub mode: ResumeMode,
}

/// What a host asks for when it opens a session.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenSession {
    /// The host's own id for the session.
    pub session_id: SessionId,
    /// What the agent may do, and which model.
    pub configuration: Configuration,
    /// A vendor conversation to continue, when the host is continuing one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<Resume>,
    /// Where this harness's executable is, when the host resolved it.
    ///
    /// Per request, because a resolved path belongs to one harness. It is usually
    /// [`Discovery::executable`](crate::Discovery::executable) from the probe of the same harness
    /// this session is being opened on.
    #[serde(default)]
    pub executable: crate::transport::ExecutablePath,
}

impl OpenSession {
    /// A new session under the most restrictive configuration.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::{OpenSession, PermissionLevel};
    ///
    /// let request = OpenSession::new("chat-42");
    /// assert_eq!(request.configuration.level, PermissionLevel::ReadOnly);
    /// assert!(request.resume.is_none());
    /// ```
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: SessionId::new(session_id),
            configuration: Configuration::default(),
            resume: None,
            executable: crate::transport::ExecutablePath::default(),
        }
    }

    /// Spawns this executable rather than leaving the launcher to resolve the program name.
    ///
    /// The path the host resolved for *this* harness — `Discovery::executable`, usually.
    #[must_use]
    pub fn with_executable(mut self, executable: crate::transport::ExecutablePath) -> Self {
        self.executable = executable;
        self
    }

    /// Runs under this configuration instead of the restrictive default.
    #[must_use]
    pub fn with_configuration(mut self, configuration: Configuration) -> Self {
        self.configuration = configuration;
        self
    }

    /// Continues the vendor conversation with this handle.
    #[must_use]
    pub fn resuming(mut self, native_session_id: impl Into<String>, mode: ResumeMode) -> Self {
        self.resume = Some(Resume {
            native_session_id: native_session_id.into(),
            mode,
        });
        self
    }
}

/// What opening produced, as the session will answer for its whole life.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// The two ids.
    pub ids: SessionIds,
    /// Whether the vendor continued a conversation rather than starting one.
    pub resumed: bool,
    /// Why a requested resume did not happen, when one was asked for and did not.
    pub fallback_reason: Option<String>,
    /// What the vendor actually accepted, which may not be what was asked for.
    pub effective_configuration: Configuration,
    /// What this session can do, as this build of the CLI reports it.
    pub capabilities: Capabilities,
}

/// The largest attachment the vendor wire carries, and how many of them.
pub const ATTACHMENT_MAX_BYTES: usize = 2 * 1024 * 1024;
/// How many attachments one turn may carry.
pub const TURN_MAX_ATTACHMENTS: usize = 4;

/// What one attachment is, for a vendor that takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum AttachmentKind {
    /// An image.
    Image,
    /// Text.
    Text,
    /// A PDF.
    Pdf,
    /// Bytes with a type the host recognised.
    Data,
    /// Bytes with a type nobody recognised.
    Unknown,
}

/// One file travelling with a turn.
///
/// Bytes rather than base64: a harness encodes for its own dialect, and a host that already has
/// the bytes should not have to encode them for a wire it cannot see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attachment {
    /// The host's own id for it.
    pub id: String,
    /// The name a person would recognise.
    pub name: String,
    /// Its media type.
    pub mime_type: String,
    /// What it is.
    pub kind: AttachmentKind,
    /// The bytes themselves.
    pub bytes: Vec<u8>,
}

/// One turn's input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnRequest {
    /// The host's own id for this turn, which is also its idempotency key.
    pub turn_id: TurnId,
    /// What to say to the agent.
    pub input: String,
    /// Files travelling with it.
    pub attachments: Vec<Attachment>,
    /// A configuration for this turn alone, when it differs from the session's.
    pub configuration: Option<Configuration>,
}

impl TurnRequest {
    /// A turn that is only text.
    ///
    /// The turn id is the host's: the library does not mint ids, because an id a host cannot
    /// reproduce is an id it cannot retry with.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::TurnRequest;
    ///
    /// let turn = TurnRequest::new("turn-1", "say hello");
    /// assert_eq!(turn.turn_id.as_str(), "turn-1");
    /// assert!(turn.attachments.is_empty());
    /// ```
    pub fn new(turn_id: impl Into<String>, input: impl Into<String>) -> Self {
        Self {
            turn_id: TurnId::new(turn_id),
            input: input.into(),
            attachments: Vec::new(),
            configuration: None,
        }
    }

    /// Carries these files with the turn.
    #[must_use]
    pub fn with_attachments(mut self, attachments: Vec<Attachment>) -> Self {
        self.attachments = attachments;
        self
    }

    /// Runs this turn under a configuration of its own.
    #[must_use]
    pub fn with_configuration(mut self, configuration: Configuration) -> Self {
        self.configuration = Some(configuration);
        self
    }
}

/// More input for a turn that is already running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Steer {
    /// The turn to steer.
    pub turn_id: TurnId,
    /// The vendor's handle for that turn.
    pub native_turn_id: String,
    /// What to add.
    pub input: String,
}

/// Whether a steer landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SteerOutcome {
    /// The vendor took it.
    Accepted,
    /// It did not, and why.
    Rejected {
        /// The reason the vendor gave.
        reason: SteerRejection,
    },
}

/// Why a vendor refused a steer.
///
/// Only the two a vendor can decide. "Not supported" and "no such session" are refused before a
/// harness is called at all, because neither is a fact about the vendor's turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SteerRejection {
    /// The turn ended before the steer reached it — a race a person can legitimately hit.
    TurnAlreadyCompleted,
    /// This particular turn refuses steering, such as a review or a compaction.
    TurnNotSteerable,
}

/// What a vendor-native review is pointed at.
///
/// A one-member enum deliberately: vendors also model base branches, commits and custom targets,
/// and modelling this as a bare flag would make adding them a breaking reshape rather than one
/// more member.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ReviewTarget {
    /// Staged, unstaged and untracked work, as the vendor defines it. The library does not narrow
    /// the definition.
    UncommittedChanges,
}

/// Start a vendor-native review on an open session.
///
/// A review is a turn that happens to be a review: it is deduplicated, ordered, cancelled and
/// persisted by the same machinery, so it carries a turn id like any other. It runs under the
/// permissions the session already has — reconfiguring mid-review would be a way around a choice
/// somebody already made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewRequest {
    /// The host's own id for this turn.
    pub turn_id: TurnId,
    /// What to review.
    pub target: ReviewTarget,
}

/// How many sessions one page may carry.
pub const SESSION_PAGE_LIMIT: usize = 50;

/// Which of a vendor's own sessions to list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionQuery {
    /// Where to continue from, from a previous page.
    pub cursor: Option<String>,
    /// How many to return, up to [`SESSION_PAGE_LIMIT`].
    pub limit: Option<usize>,
    /// Only sessions in this directory, exactly as the vendor spells it.
    pub workspace_path: Option<PathBuf>,
}

impl SessionQuery {
    /// This query with its page size brought inside [`SESSION_PAGE_LIMIT`].
    ///
    /// Applied by [`Session::list_sessions`] before the harness sees it, so the cap is something
    /// the vendor is asked for rather than something the library applies to the answer. A page cut
    /// down afterwards is a page whose tail has no cursor pointing at it — the vendor's cursor
    /// resumes after the last row it *sent*, not after the last row a host was shown.
    ///
    /// An absent limit becomes the cap for the same reason: "as many as you like" is the request
    /// that produces the page there is no way to finish reading.
    #[must_use]
    pub fn normalized(self) -> Self {
        Self {
            limit: Some(
                self.limit
                    .unwrap_or(SESSION_PAGE_LIMIT)
                    .min(SESSION_PAGE_LIMIT),
            ),
            ..self
        }
    }
}

/// One conversation the vendor already owns, as a row in a picker.
///
/// A pointer, not an import: nothing here carries transcript content. Adopting a session records
/// which vendor conversation a chat continues, and the vendor keeps the history it wrote.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeSession {
    /// The vendor's own handle.
    pub native_session_id: String,
    /// A title, when the vendor kept one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// A preview of the conversation, when the vendor offers one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    /// The vendor's own working directory for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// When it last changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<SystemTime>,
}

/// One page of a vendor's own sessions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionPage {
    /// The rows.
    pub sessions: Vec<NativeSession>,
    /// Where the next page starts, when there is one.
    pub next_cursor: Option<String>,
    /// True when the page held more usable rows than [`SESSION_PAGE_LIMIT`] and the rest were cut.
    ///
    /// Worth carrying because [`next_cursor`](Self::next_cursor) cannot stand in for it: the
    /// cursor is the vendor's own and resumes after the last row the vendor sent, so paging on it
    /// skips whatever the cap removed. A host that sees this set knows the gap is there and that
    /// no cursor will close it.
    ///
    /// A vendor given a bounded [`SessionQuery`] never trips this. Seeing it set means the vendor
    /// returned more than it was asked for.
    pub truncated: bool,
}

impl SessionPage {
    /// This page with unusable rows dropped rather than repaired.
    ///
    /// A row whose id could not survive bounding is dropped: a truncated session id points at a
    /// different conversation, and adopting one would be worse than not offering it. A path is
    /// sanitised but never shortened, for the same reason — a shortened path names nothing — so an
    /// unbounded one drops its row too.
    ///
    /// Cutting the page to [`SESSION_PAGE_LIMIT`] sets [`truncated`](Self::truncated), which those
    /// per-row drops do not: a row refused as unusable is a row nothing could have shown, while a
    /// row past the cap is one the vendor sent, the host will never see, and the cursor will skip.
    #[must_use]
    pub fn normalized(self) -> Self {
        let mut usable = self.sessions.into_iter().filter_map(|session| {
            let native_session_id =
                normalize::opaque_id(&session.native_session_id, "native session id").ok()?;
            let workspace_path = match session.workspace_path {
                Some(path) => Some(normalize::vendor_path(&path)?),
                None => None,
            };
            Some(NativeSession {
                native_session_id,
                title: bounded_label(session.title),
                preview: bounded_label(session.preview),
                workspace_path,
                updated_at: session.updated_at,
            })
        });
        // Taken lazily, then asked whether a usable row survived the cap, so a vendor that ignored
        // its limit is noticed without the whole overrun being built first.
        let sessions: Vec<NativeSession> = usable.by_ref().take(SESSION_PAGE_LIMIT).collect();
        Self {
            truncated: usable.next().is_some(),
            sessions,
            next_cursor: self.next_cursor,
        }
    }
}

fn bounded_label(raw: Option<String>) -> Option<String> {
    raw.map(|text| normalize::bound_text(&text, TextLimit::SessionTitle).text)
        .filter(|text| !text.is_empty())
}

/// Account-level plan quota, for a vendor that reports one.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountUsage {
    /// What the vendor said, or nothing.
    ///
    /// Absence is unknown, never zero: a harness that could not read a baseline — signed out,
    /// timed out — says so by omission rather than by reporting an empty quota.
    pub limits: Option<AccountLimits>,
}

/// One live conversation with a vendor CLI.
///
/// A handle, not a supervisor. The library keeps no registry of open sessions, polls no consent
/// and fans nothing out: those are host policy, and a host builds whatever registry it needs
/// around these handles.
///
/// The four optional methods default to [`Error::NotSupported`], so a harness implements what its
/// vendor has and a host that calls the rest receives a typed refusal.
#[async_trait::async_trait]
pub trait Session: Send + Sync {
    /// What opening this session produced.
    fn info(&self) -> &SessionInfo;

    /// The two ids this session answers to.
    fn ids(&self) -> &SessionIds {
        &self.info().ids
    }

    /// Starts a turn and returns its bounded event stream.
    ///
    /// Awaited rather than synchronous because a vendor answers with the turn's own handle, which
    /// the stream carries.
    ///
    /// # Errors
    ///
    /// Whatever the vendor or the link reported.
    async fn start_turn(&self, request: TurnRequest) -> Result<TurnStream>;

    /// Answers one approval.
    ///
    /// # Errors
    ///
    /// Whatever the vendor or the link reported.
    async fn respond(&self, response: PermissionResponse) -> Result<()>;

    /// Stops whatever turn is running.
    ///
    /// # Errors
    ///
    /// Whatever the vendor or the link reported.
    async fn cancel(&self, reason: CancelReason) -> Result<()>;

    /// Ends the session and releases what it holds.
    ///
    /// Idempotent: a close racing a cancel, or two closes from different tasks, must not fail the
    /// second caller.
    ///
    /// # Errors
    ///
    /// Whatever the vendor or the link reported.
    async fn close(&self, reason: CloseReason) -> Result<()>;

    /// Adds to a turn that is already running.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn steer(&self, _steer: Steer) -> Result<SteerOutcome> {
        Err(Error::not_supported(Capability::Steering))
    }

    /// Starts a vendor-native review.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn start_review(&self, _request: ReviewRequest) -> Result<ReviewStream> {
        Err(Error::not_supported(Capability::NativeReview))
    }

    /// Lists the vendor's own sessions, bounded before a host sees them.
    ///
    /// What a host calls. Like [`Harness::discover`](crate::Harness::discover), it is the half
    /// that applies [`SessionPage::normalized`], so a title or a workspace path a vendor wrote
    /// cannot reach a host's list unbounded.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements
    /// [`list_native_sessions`](Self::list_native_sessions).
    async fn list_sessions(&self, query: SessionQuery) -> Result<SessionPage> {
        Ok(self
            .list_native_sessions(query.normalized())
            .await?
            .normalized())
    }

    /// The page as the vendor returned it. Implemented by a harness that supports listing.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn list_native_sessions(&self, _query: SessionQuery) -> Result<SessionPage> {
        Err(Error::not_supported(Capability::SessionListing))
    }

    /// Reads account-level plan quota.
    ///
    /// # Errors
    ///
    /// [`Error::NotSupported`] unless the harness implements it.
    async fn refresh_account_usage(&self) -> Result<AccountUsage> {
        Err(Error::not_supported(Capability::AccountUsage))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CancelReason, CloseReason, Configuration, NativeSession, OpenSession, ResumeMode, Session,
        SessionIds, SessionInfo, SessionPage, TurnRequest,
    };
    use crate::permission::{ApprovalRouting, PermissionLevel};

    #[test]
    fn closing_carries_its_reason_into_the_turn_it_cancels() {
        let cases = [
            (CloseReason::Requested, CancelReason::Requested),
            (CloseReason::ConsentRevoked, CancelReason::ConsentRevoked),
            (CloseReason::Shutdown, CancelReason::Shutdown),
        ];
        for (close, expected) in cases {
            assert_eq!(
                CancelReason::from(close),
                expected,
                "expected {expected:?}, received a different cancel reason for {close:?}"
            );
        }
    }

    #[test]
    fn reasons_print_as_words_a_host_can_map() {
        assert_eq!(CancelReason::ConsentRevoked.to_string(), "consent revoked");
        assert_eq!(CloseReason::Shutdown.to_string(), "shutdown");
    }

    #[test]
    fn the_default_configuration_is_the_narrow_end_of_both_axes() {
        let configuration = Configuration::default();
        assert_eq!(configuration.level, PermissionLevel::ReadOnly);
        assert_eq!(configuration.routing, ApprovalRouting::User);
        assert_eq!(configuration.model, None);
    }

    #[test]
    fn opening_a_session_asks_for_nothing_a_host_did_not_choose() {
        let request = OpenSession::new("chat-42");
        assert_eq!(request.configuration, Configuration::default());
        assert_eq!(request.resume, None);

        let resuming = OpenSession::new("chat-42").resuming("thread_9", ResumeMode::Fallback);
        assert_eq!(
            resuming.resume.map(|resume| resume.native_session_id),
            Some(String::from("thread_9"))
        );
    }

    #[test]
    fn a_turn_carries_the_hosts_own_id_rather_than_one_the_library_minted() {
        let turn = TurnRequest::new("turn-7", "ship it");
        assert_eq!(turn.turn_id.as_str(), "turn-7");
        assert_eq!(turn.configuration, None);
    }

    fn row(native_session_id: &str) -> NativeSession {
        NativeSession {
            native_session_id: native_session_id.to_owned(),
            title: None,
            preview: None,
            workspace_path: None,
            updated_at: None,
        }
    }

    #[test]
    fn a_listing_drops_a_row_whose_id_would_have_to_be_truncated() {
        let page = SessionPage {
            sessions: vec![row(&"i".repeat(129)), row("thread_1"), row("  ")],
            next_cursor: Some(String::from("next")),
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), 1, "received {:?}", page.sessions);
        assert_eq!(page.sessions[0].native_session_id, "thread_1");
        assert_eq!(page.next_cursor, Some(String::from("next")));
    }

    #[test]
    fn a_listing_drops_a_row_whose_path_cannot_be_carried_whole() {
        let page = SessionPage {
            sessions: vec![
                NativeSession {
                    workspace_path: Some("p".repeat(4_097)),
                    ..row("thread_1")
                },
                NativeSession {
                    workspace_path: Some(String::from("/home/ada/mango")),
                    ..row("thread_2")
                },
            ],
            next_cursor: None,
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), 1, "received {:?}", page.sessions);
        assert_eq!(page.sessions[0].native_session_id, "thread_2");
    }

    #[test]
    fn a_listing_bounds_titles_and_drops_the_ones_that_are_left_empty() {
        let page = SessionPage {
            sessions: vec![NativeSession {
                title: Some("t".repeat(300)),
                preview: Some(String::from("\u{202e}")),
                ..row("thread_1")
            }],
            next_cursor: None,
            truncated: false,
        }
        .normalized();

        assert_eq!(
            page.sessions[0]
                .title
                .as_ref()
                .map(|title| title.chars().count()),
            Some(256)
        );
        assert_eq!(page.sessions[0].preview, None);
    }

    /// The cap and the cursor describe different points in the same page: the cap stops at row 50,
    /// the vendor's cursor resumes after row 80. Paging on it steps over rows 51-80, so a page
    /// that was cut has to say it was — nothing else in the page can be read to mean it.
    #[test]
    fn a_listing_cut_to_one_page_says_so_because_the_cursor_will_skip_the_rest() {
        let page = SessionPage {
            sessions: (0..80)
                .map(|index| row(&format!("thread_{index}")))
                .collect(),
            next_cursor: Some(String::from("after-thread-79")),
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), 50);
        assert!(
            page.truncated,
            "expected a page of 80 cut to 50 to report the cut, received {:?}",
            page.truncated
        );
        // The cursor is the vendor's and is carried as it was: dropping it would lose rows 81 and
        // beyond on top of the ones the cap already took.
        assert_eq!(page.next_cursor, Some(String::from("after-thread-79")));
    }

    /// The flag means "the cap fired", not "something was dropped". A row refused as unusable is a
    /// row nothing could have shown, and reporting it here would make the flag unreadable on the
    /// one page a host can actually act on.
    #[test]
    fn a_page_inside_the_cap_is_not_truncated_by_a_row_it_refused() {
        let page = SessionPage {
            sessions: vec![row(&"i".repeat(129)), row("thread_1")],
            next_cursor: None,
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), 1);
        assert!(!page.truncated, "received {:?}", page.truncated);
    }

    /// A page of exactly the cap is whole, not cut. The boundary is worth pinning: the check asks
    /// whether a row survived *past* the cap, and an off-by-one here would mark every full page.
    #[test]
    fn a_page_of_exactly_the_cap_is_not_reported_as_cut() {
        let page = SessionPage {
            sessions: (0..super::SESSION_PAGE_LIMIT)
                .map(|index| row(&format!("thread_{index}")))
                .collect(),
            next_cursor: None,
            truncated: false,
        }
        .normalized();

        assert_eq!(page.sessions.len(), super::SESSION_PAGE_LIMIT);
        assert!(!page.truncated, "received {:?}", page.truncated);
    }

    /// The cap has to be something the vendor is *asked* for. Applied only to the answer, it cuts
    /// a page whose tail no cursor points at; applied to the query, a well-behaved vendor never
    /// sends the overrun in the first place.
    #[tokio::test]
    async fn a_listing_query_reaches_the_harness_already_bounded() {
        use super::{SESSION_PAGE_LIMIT, SessionQuery};

        let session = RecordingListing::default();
        session
            .list_sessions(SessionQuery {
                limit: Some(500),
                ..SessionQuery::default()
            })
            .await
            .expect("expected a page");
        assert_eq!(session.last_limit().await, Some(SESSION_PAGE_LIMIT));

        // Absent means "as many as you like", which is the request that produces the unfinishable
        // page, so it is spelled out rather than left to the vendor's default.
        session
            .list_sessions(SessionQuery::default())
            .await
            .expect("expected a page");
        assert_eq!(session.last_limit().await, Some(SESSION_PAGE_LIMIT));

        // A host that asked for less still gets less.
        session
            .list_sessions(SessionQuery {
                limit: Some(5),
                ..SessionQuery::default()
            })
            .await
            .expect("expected a page");
        assert_eq!(session.last_limit().await, Some(5));
    }

    /// A session that answers listings with nothing and remembers what it was asked for.
    struct RecordingListing {
        info: SessionInfo,
        last_query: tokio::sync::Mutex<Option<super::SessionQuery>>,
    }

    impl Default for RecordingListing {
        fn default() -> Self {
            Self {
                info: SessionInfo {
                    ids: SessionIds {
                        session_id: crate::event::SessionId::new("chat-1"),
                        native_session_id: String::from("native-1"),
                    },
                    resumed: false,
                    fallback_reason: None,
                    effective_configuration: Configuration::default(),
                    capabilities: crate::harness::Capabilities::none(),
                },
                last_query: tokio::sync::Mutex::new(None),
            }
        }
    }

    impl RecordingListing {
        async fn last_limit(&self) -> Option<usize> {
            self.last_query
                .lock()
                .await
                .as_ref()
                .and_then(|query| query.limit)
        }
    }

    #[async_trait::async_trait]
    impl super::Session for RecordingListing {
        fn info(&self) -> &SessionInfo {
            &self.info
        }

        async fn start_turn(&self, _request: TurnRequest) -> crate::Result<crate::TurnStream> {
            Err(crate::Error::Closed { subject: "session" })
        }

        async fn respond(
            &self,
            _response: crate::permission::PermissionResponse,
        ) -> crate::Result<()> {
            Err(crate::Error::Closed { subject: "session" })
        }

        async fn cancel(&self, _reason: CancelReason) -> crate::Result<()> {
            Ok(())
        }

        async fn close(&self, _reason: CloseReason) -> crate::Result<()> {
            Ok(())
        }

        async fn list_native_sessions(
            &self,
            query: super::SessionQuery,
        ) -> crate::Result<SessionPage> {
            *self.last_query.lock().await = Some(query);
            Ok(SessionPage::default())
        }
    }
}
