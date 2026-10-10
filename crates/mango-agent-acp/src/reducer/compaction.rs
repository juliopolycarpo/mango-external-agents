//! `compaction_update` and `compaction_summary_chunk` in, compaction activity events out.
//!
//! A compaction is the agent rewriting its own context. ACP reports it as an entity with an id, a
//! status, a summary and an error, patched in place by later frames. It reaches a host as the same
//! activity bracket the Codex harness produces for its `contextCompaction` item: a
//! [`EventKind::ActivityStarted`] named [`COMPACTION_NAME`] of kind [`ActivityKind::Compaction`],
//! updates carrying the summary, and one [`EventKind::ActivityCompleted`].
//!
//! The mapping, frame by frame:
//!
//! * **First sighting of an id**, through either frame, opens the activity. A chunk for an id this
//!   turn never saw announced creates an in-progress compaction, as the protocol says.
//! * **Status.** `completed`, `failed` and `cancelled` close it with the matching
//!   [`ActivityStatus`]. `in_progress` and a status this build does not know leave it running: the
//!   protocol forbids inferring a lifecycle from an unknown status, and guessing "completed" would
//!   close an activity that is still going. A frame for a compaction that already ended is dropped.
//! * **Summary.** The text of its text blocks, joined in order with nothing between them (an agent
//!   streams a summary as chunks the way it streams a message). An image, audio or resource block is
//!   dropped, as in an agent message, because describing one would put words in a host's transcript
//!   that the agent never wrote. It arrives as [`ActivityContent::Output`]. A chunk appends; a
//!   `summary` on an update replaces everything received so far; `null`, `[]` and a replacement with
//!   no text clear it ([`ActivityContent::Empty`]); an omitted `summary` changes nothing.
//! * **Error.** Patched the same way and carried as the activity's detail, which is where a failed
//!   tool call's own words go.
//! * **Bounds.** The summary is sanitised and cut to the size the core publishes as it accumulates,
//!   so an endless stream of chunks holds a bounded string. Updates are coalesced with the tool
//!   calls' own interval (see [`TOOL_UPDATE_INTERVAL`](super::TOOL_UPDATE_INTERVAL)): every update
//!   carries the whole summary so far, and forwarding one per chunk would spend the turn's budget on
//!   copies a host overwrites at once.
//!
//! A compaction the agent never ends is closed with the turn by
//! [`Reducer::finish_with`](super::Reducer::finish_with), like a tool call left running.
//!
//! Protocol: <https://agentclientprotocol.com/rfds/session-compaction>, stable in v1 from schema
//! 1.11.

use std::collections::HashMap;
use std::fmt;
use std::time::Instant;

use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::schema::v1::{
    CompactionId, CompactionStatus, CompactionSummaryChunk, CompactionUpdate, ContentBlock,
};
use mango_external_agents::ActivityContent;
use mango_external_agents::event::{
    Activity, ActivityKind, ActivityResult, ActivityStatus, ActivityUpdate, EventKind,
};
use mango_external_agents::normalize::{self, TextLimit};

use super::{Held, MINTED_ID_PREFIX, Reducer, plain_text};

/// The activity name every compaction carries, the one the Codex harness uses for its own.
pub const COMPACTION_NAME: &str = "compact";

/// The activity title every compaction carries, the one the Codex harness uses for its own.
///
/// ACP's compaction has no title field, so the label is this crate's and the same for both vendors.
pub const COMPACTION_TITLE: &str = "Compacting the conversation";

/// What a compaction's activity is called in the events a host receives.
///
/// `acp:compaction:` followed by the agent's own `compactionId`. A compaction id and a tool call
/// id are separate namespaces on the wire, so an agent may use the same string for both, and two
/// activities would then share one call id. No tool call can carry this prefix: every tool call id
/// starting with `acp:` is given `acp:vendor:` in front (see the reducer's own `clear_of_plan`).
/// The agent's own id travels as [`Activity::item_id`].
///
/// An id the core would refuse on its own (blank, or carrying characters it strips) is returned
/// as it arrived, so the core refuses the event that names it and the turn fails with
/// `acp-refused-event`, exactly as for a tool call. The prefix counts against the length the core
/// publishes, so an id over 113 code points is refused too.
///
/// # Example
///
/// ```
/// use agent_client_protocol::schema::v1::CompactionId;
/// use mango_agent_acp::reducer::compaction_call_id;
///
/// assert_eq!(compaction_call_id(&CompactionId::new("c1")), "acp:compaction:c1");
/// assert_eq!(compaction_call_id(&CompactionId::new("  ")), "  ");
/// ```
#[must_use]
pub fn compaction_call_id(id: &CompactionId) -> String {
    match normalize::opaque_id(&id.0, "activity call id") {
        Ok(_) => format!("{MINTED_ID_PREFIX}compaction:{}", id.0),
        Err(_) => id.0.to_string(),
    }
}

/// What the open compactions of one turn have said so far, by activity call id.
///
/// Only what a later frame patches: whether a compaction is running is the reducer's own
/// `open_calls`, so the turn's end closes a compaction with the same code that closes a tool call.
#[derive(Debug, Default)]
pub(super) struct Compactions {
    open: HashMap<String, Compaction>,
}

impl Compactions {
    /// Forgets every compaction, for a turn that is closing them all.
    pub(super) fn clear(&mut self) {
        self.open.clear();
    }
}

/// One running compaction's patchable fields.
#[derive(Default)]
struct Compaction {
    summary: Summary,
    error: Option<String>,
    error_cut: bool,
}

impl fmt::Debug for Compaction {
    /// Reports the compaction's shape without logging the agent's summary or error text.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Compaction")
            .field("summary", &self.summary)
            .field("has_error", &self.error.is_some())
            .field("error_cut", &self.error_cut)
            .finish()
    }
}

/// A compaction's summary text, never longer than the core publishes.
#[derive(Default)]
struct Summary {
    text: String,
    code_points: usize,
    /// Whether anything the agent sent is missing from `text`: stripped, or past the bound.
    cut: bool,
}

impl fmt::Debug for Summary {
    /// Reports the summary's size without logging its text.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Summary")
            .field("code_points", &self.code_points)
            .field("cut", &self.cut)
            .finish()
    }
}

impl Summary {
    /// Appends `raw`, sanitised, up to the bound. Returns whether a host would see a difference.
    fn append(&mut self, raw: &str) -> bool {
        let clean = normalize::sanitize_field(raw);
        let room = TextLimit::Detail
            .max_code_points()
            .saturating_sub(self.code_points);
        let end = clean
            .text
            .char_indices()
            .nth(room)
            .map_or(clean.text.len(), |(index, _)| index);
        let fitting = &clean.text[..end];
        self.text.push_str(fitting);
        self.code_points += fitting.chars().count();
        let newly_cut = !self.cut && (clean.truncated || end < clean.text.len());
        self.cut |= newly_cut;
        !fitting.is_empty() || newly_cut
    }

    /// Replaces everything with the text of `blocks`. Non-text blocks are dropped.
    fn replace(&mut self, blocks: Vec<ContentBlock>) {
        *self = Self::default();
        for text in blocks.into_iter().filter_map(plain_text) {
            self.append(&text);
        }
    }

    /// Empties the summary. Returns whether there was anything to empty.
    fn clear(&mut self) -> bool {
        let had = !self.text.is_empty() || self.cut;
        *self = Self::default();
        had
    }

    /// The summary as activity content: its text, or the instruction to clear what a host holds.
    fn content(&self) -> ActivityContent {
        match self.text.is_empty() {
            true => ActivityContent::Empty,
            false => ActivityContent::Output {
                text: self.text.clone(),
            },
        }
    }
}

/// The outcome a compaction status names, or nothing while it is still running.
fn finished(status: &CompactionStatus) -> Option<ActivityStatus> {
    match status {
        CompactionStatus::Completed => Some(ActivityStatus::Completed),
        CompactionStatus::Failed => Some(ActivityStatus::Failed),
        CompactionStatus::Cancelled => Some(ActivityStatus::Cancelled),
        // `Other` and the `#[non_exhaustive]` tail with `in_progress`: the protocol reserves unknown
        // statuses for later versions and forbids reading a lifecycle into one.
        _ => None,
    }
}

/// The event that opens a compaction's bracket.
fn started(call_id: &str, id: &CompactionId) -> EventKind {
    EventKind::ActivityStarted {
        call_id: call_id.to_owned(),
        activity: Activity::new(COMPACTION_NAME, ActivityKind::Compaction, COMPACTION_TITLE)
            .with_item_id(id.0.to_string()),
    }
}

impl Reducer {
    /// A `compaction_update` frame: the entity's status, and patches to its summary and error.
    pub(super) fn compaction_update(
        &mut self,
        update: CompactionUpdate,
        now: Instant,
    ) -> Vec<EventKind> {
        let call_id = compaction_call_id(&update.compaction_id);
        let Some(mut events) = self.open_compaction(&call_id, &update.compaction_id) else {
            return Vec::new();
        };
        let Some(compaction) = self.compactions.open.get_mut(&call_id) else {
            return events;
        };
        let summary_patched = match update.summary {
            MaybeUndefined::Undefined => false,
            MaybeUndefined::Null => compaction.summary.clear(),
            MaybeUndefined::Value(blocks) => {
                let had = compaction.summary.clear();
                compaction.summary.replace(blocks);
                had || !compaction.summary.text.is_empty() || compaction.summary.cut
            }
        };
        let error_patched = match update.error {
            MaybeUndefined::Undefined => false,
            MaybeUndefined::Null => {
                compaction.error_cut = false;
                compaction.error.take().is_some()
            }
            MaybeUndefined::Value(error) => {
                let bounded = normalize::bound_text(&error, TextLimit::Detail);
                compaction.error_cut = bounded.truncated;
                compaction.error = Some(bounded.text);
                true
            }
        };
        let content = summary_patched.then(|| compaction.summary.content());
        let content_cut = summary_patched && compaction.summary.cut;
        if let Some(status) = finished(&update.status) {
            // The completion is the last word on the detail. An error cleared by this very frame
            // is said as an empty detail, so it replaces what a host shows and what is still held.
            let detail = compaction.error.clone().or(error_patched.then(String::new));
            let mut result = ActivityResult::new(status).with_optional_detail(detail);
            result.content = content;
            result.truncated = content_cut || compaction.error_cut;
            events.extend(self.close_compaction(call_id, result));
            return events;
        }
        let mut held = Held::unbounded(
            ActivityUpdate::new()
                .with_optional_detail(
                    error_patched.then(|| compaction.error.clone().unwrap_or_default()),
                )
                .with_optional_content(content),
        );
        held.detail_cut = error_patched && compaction.error_cut;
        held.content_cut = content_cut;
        events.extend(self.compaction_changed(call_id, held, now));
        events
    }

    /// A `compaction_summary_chunk` frame: one more block of the summary.
    pub(super) fn compaction_chunk(
        &mut self,
        chunk: CompactionSummaryChunk,
        now: Instant,
    ) -> Vec<EventKind> {
        let call_id = compaction_call_id(&chunk.compaction_id);
        let Some(mut events) = self.open_compaction(&call_id, &chunk.compaction_id) else {
            return Vec::new();
        };
        let Some(compaction) = self.compactions.open.get_mut(&call_id) else {
            return events;
        };
        let changed =
            plain_text(chunk.content).is_some_and(|text| compaction.summary.append(&text));
        if !changed {
            return events;
        }
        let mut held =
            Held::unbounded(ActivityUpdate::new().with_content(compaction.summary.content()));
        held.content_cut = compaction.summary.cut;
        events.extend(self.compaction_changed(call_id, held, now));
        events
    }

    /// Makes sure the compaction is one a host is rendering, announcing it on its first frame.
    ///
    /// `None` when the frame has nowhere to go: the compaction already ended this turn. An id the
    /// core refuses is announced once, so the core refuses it and the turn fails, and is then
    /// remembered as ended so its later frames are dropped at the fixed cost of a digest.
    fn open_compaction(&mut self, call_id: &str, id: &CompactionId) -> Option<Vec<EventKind>> {
        if self.open_calls.contains_key(call_id) {
            return Some(Vec::new());
        }
        if self.is_finished(call_id) {
            return None;
        }
        let refused = normalize::opaque_id(call_id, "activity call id").is_err();
        self.track(call_id.to_owned(), refused);
        if !refused {
            self.compactions
                .open
                .insert(call_id.to_owned(), Compaction::default());
        }
        Some(vec![started(call_id, id)])
    }

    /// A change to a running compaction, delivered now or held for the coalescing interval.
    fn compaction_changed(&mut self, call_id: String, later: Held, now: Instant) -> Vec<EventKind> {
        if later.is_empty() {
            return Vec::new();
        }
        let interval = self.update_interval;
        let Some(open) = self.open_calls.get_mut(&call_id) else {
            return Vec::new();
        };
        let recent = open
            .last_update
            .is_some_and(|last| now.saturating_duration_since(last) < interval);
        let merged = match open.held.take() {
            Some(held) => held.merged_with(later),
            None => later,
        };
        if recent {
            open.held = Some(merged);
            return Vec::new();
        }
        open.last_update = Some(now);
        vec![EventKind::ActivityUpdated {
            call_id,
            update: merged.into_update(),
        }]
    }

    /// Ends a running compaction: whatever is still held, then its completion.
    fn close_compaction(&mut self, call_id: String, result: ActivityResult) -> Vec<EventKind> {
        let mut events = Vec::with_capacity(2);
        let held = self
            .open_calls
            .remove(&call_id)
            .and_then(|open| open.held)
            .map(|held| held.superseded_by(&result))
            .filter(|held| !held.is_empty());
        if let Some(held) = held {
            events.push(EventKind::ActivityUpdated {
                call_id: call_id.clone(),
                update: held.into_update(),
            });
        }
        self.compactions.open.remove(&call_id);
        self.mark_finished(&call_id);
        events.push(EventKind::ActivityCompleted { call_id, result });
        events
    }
}

#[cfg(test)]
mod tests;
