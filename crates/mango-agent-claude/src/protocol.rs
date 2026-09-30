//! The `stream-json` record shapes this harness reads, hand-written.
//!
//! Not generated, because Claude Code publishes no machine-readable schema for this stream the way
//! Codex publishes one for `app-server`. Every shape below was observed on a live run and is
//! deliberately **partial**: each view names only the fields the reducer consumes, and every one of
//! them is optional. That is the point. The vocabulary is wider than any plan enumerated and will
//! keep growing, so a record type this module has never heard of has to be ignorable rather than
//! fatal, and a field that changed shape has to narrow to "absent" rather than fail the turn.
//!
//! Which is why a record is a [`serde_json::Value`] behind typed accessors rather than a
//! `#[derive(Deserialize)]` struct: a derived struct refuses the whole record when one field
//! changes type, and refusing a record is how a vendor's additive change ends a conversation
//! somebody is in the middle of.
//!
//! `type` is the only discriminator that is trusted, and even it is read as a plain string so that
//! an unknown value falls through to the ignore path.

use std::borrow::Cow;
use std::fmt;

use serde_json::{Map, Value};

use crate::redacted;

/// One line of `claude --print --output-format stream-json`, before it has been recognised.
#[derive(Clone, PartialEq)]
pub struct StreamRecord {
    fields: Map<String, Value>,
}

impl StreamRecord {
    /// Parses one stdout line, or nothing when the line is not a JSON object.
    ///
    /// A line that is not JSON is dropped rather than failing the turn. The CLI writes diagnostics
    /// to stderr, but a stray non-JSON line on stdout — a deprecation notice from a wrapper
    /// script, say — must not be able to end a conversation the user is in the middle of.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_agent_claude::protocol::StreamRecord;
    ///
    /// let record = StreamRecord::parse(r#"{"type":"result"}"#).expect("expected a record");
    /// assert_eq!(record.kind(), Some("result"));
    /// assert!(StreamRecord::parse("Debugger attached.").is_none());
    /// ```
    pub fn parse(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        if !trimmed.starts_with('{') {
            return None;
        }
        match serde_json::from_str::<Value>(trimmed) {
            Ok(Value::Object(fields)) => Some(Self { fields }),
            _ => None,
        }
    }

    /// The `type` discriminator, when it is a string.
    pub fn kind(&self) -> Option<&str> {
        text(&self.fields, "type")
    }

    /// The `subtype` discriminator, when it is a string.
    pub fn subtype(&self) -> Option<&str> {
        text(&self.fields, "subtype")
    }

    /// Which tool call this record belongs to, or nothing for the main conversation.
    ///
    /// `null` is the documented value for the main conversation, so only a non-empty string means
    /// "this belongs to a subagent". `--forward-subagent-text` sets it on every message a subagent
    /// produced, which is what lets those be nested rather than promoted.
    pub fn parent_tool_use_id(&self) -> Option<&str> {
        non_empty(&self.fields, "parent_tool_use_id")
    }

    /// This record read as `system/init`.
    pub fn init(&self) -> InitRecord<'_> {
        InitRecord {
            fields: &self.fields,
        }
    }

    /// This record read as `system/permission_denied`.
    pub fn permission_denied(&self) -> PermissionDenied<'_> {
        PermissionDenied {
            fields: &self.fields,
        }
    }

    /// This record read as `result`.
    pub fn result(&self) -> ResultRecord<'_> {
        ResultRecord {
            fields: &self.fields,
        }
    }

    /// This record read as `stream_event`, when it carries an `event` object.
    pub fn stream_event(&self) -> Option<StreamEvent<'_>> {
        Some(StreamEvent {
            fields: object(&self.fields, "event")?,
        })
    }

    /// The blocks inside an `assistant` or `user` message, in order.
    pub fn content_blocks(&self) -> Vec<ContentBlock<'_>> {
        let Some(message) = object(&self.fields, "message") else {
            return Vec::new();
        };
        let Some(Value::Array(content)) = message.get("content") else {
            return Vec::new();
        };
        content
            .iter()
            .filter_map(|block| block.as_object())
            .map(|fields| ContentBlock { fields })
            .collect()
    }
}

/// `system/init` — the first record of every run.
#[derive(Clone, Copy)]
pub struct InitRecord<'a> {
    fields: &'a Map<String, Value>,
}

impl<'a> InitRecord<'a> {
    /// The vendor's own session handle, which proves the conversation now exists on disk.
    pub fn session_id(&self) -> Option<&'a str> {
        non_empty(self.fields, "session_id")
    }

    /// What this build announces it implements, for a drift probe to compare.
    pub fn capabilities(&self) -> Option<Vec<&'a str>> {
        strings(self.fields, "capabilities")
    }

    /// The permission mode the run is actually operating under.
    pub fn permission_mode(&self) -> Option<&'a str> {
        text(self.fields, "permissionMode")
    }

    /// The model the run resolved to.
    pub fn model(&self) -> Option<&'a str> {
        non_empty(self.fields, "model")
    }

    /// Every name this run will expand as `/name`, before any provenance rule is applied.
    ///
    /// One flat list: user commands from disk, plugin and MCP commands, the CLI's own builtins,
    /// and the skills that `skills` repeats. Claude Code sends no help text with them, which is
    /// why [`Command::description`](mango_external_agents::Command::description) is optional
    /// rather than the vendors being normalised to a common shape.
    pub fn slash_commands(&self) -> Option<Vec<&'a str>> {
        strings(self.fields, "slash_commands")
    }

    /// The subset of [`slash_commands`](Self::slash_commands) that only does something in the
    /// interactive terminal, when this build states one.
    pub fn terminal_slash_commands(&self) -> Option<Vec<&'a str>> {
        strings(self.fields, "terminal_slash_commands")
    }

    /// The skills this run loaded, which are also listed in `slash_commands`.
    pub fn skills(&self) -> Vec<&'a str> {
        strings(self.fields, "skills").unwrap_or_default()
    }

    /// Every marketplace plugin's own name, read as defensively as everything else here.
    pub fn plugin_names(&self) -> Vec<&'a str> {
        let Some(Value::Array(plugins)) = self.fields.get("plugins") else {
            return Vec::new();
        };
        plugins
            .iter()
            .filter_map(|plugin| plugin.as_object())
            .filter_map(|plugin| text(plugin, "name"))
            .collect()
    }
}

/// `system/permission_denied` — the vendor's own statement of why a call was refused.
///
/// Reported before the `tool_result` that closes the call arrives.
#[derive(Clone, Copy)]
pub struct PermissionDenied<'a> {
    fields: &'a Map<String, Value>,
}

impl<'a> PermissionDenied<'a> {
    /// The call this refusal is about.
    pub fn tool_use_id(&self) -> Option<&'a str> {
        non_empty(self.fields, "tool_use_id")
    }

    /// The vendor's own explanation.
    pub fn message(&self) -> Option<&'a str> {
        non_empty(self.fields, "message")
    }
}

/// One block inside an `assistant` or `user` message.
#[derive(Clone, Copy)]
pub struct ContentBlock<'a> {
    fields: &'a Map<String, Value>,
}

impl<'a> ContentBlock<'a> {
    /// What kind of block this is: `text`, `thinking`, `tool_use`, `tool_result`, …
    pub fn kind(&self) -> Option<&'a str> {
        text(self.fields, "type")
    }

    /// A `text` block's text.
    pub fn text(&self) -> Option<&'a str> {
        non_empty(self.fields, "text")
    }

    /// A `thinking` block's reasoning.
    pub fn thinking(&self) -> Option<&'a str> {
        non_empty(self.fields, "thinking")
    }

    /// A `tool_use` block's vendor tool name.
    pub fn name(&self) -> Option<&'a str> {
        non_empty(self.fields, "name")
    }

    /// A `tool_use` block's call id.
    pub fn id(&self) -> Option<&'a str> {
        non_empty(self.fields, "id")
    }

    /// A `tool_use` block's arguments, whatever shape they came in.
    pub fn input(&self) -> Option<&'a Value> {
        self.fields.get("input")
    }

    /// A `tool_result` block's call id.
    pub fn tool_use_id(&self) -> Option<&'a str> {
        non_empty(self.fields, "tool_use_id")
    }

    /// Whether a `tool_result` reported a failure. Only a literal `true` counts.
    pub fn is_error(&self) -> bool {
        self.fields.get("is_error") == Some(&Value::Bool(true))
    }

    /// A `tool_result` block's payload, flattened.
    ///
    /// The payload is text on some calls and an array of blocks on others, so both are read here
    /// rather than at the one call site that would then have to know the difference. Borrowed for
    /// the plain-string case rather than cloned: a `Read` or `Bash` result can run to hundreds of
    /// kilobytes, and every caller bounds this to a few thousand characters before it is kept.
    pub fn result_text(&self) -> Cow<'a, str> {
        match self.fields.get("content") {
            Some(Value::String(content)) => Cow::Borrowed(content.as_str()),
            Some(Value::Array(blocks)) => Cow::Owned(
                blocks
                    .iter()
                    .filter_map(block_text)
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => Cow::Borrowed(""),
        }
    }

    /// The start of [`Self::result_text`]: its first `max_chars` characters, without building the
    /// rest of an array payload.
    ///
    /// The characters are byte-identical to flattening and then cutting: the same `\n` between
    /// the non-empty parts, the same skipped elements, and a cut that never splits a character. An
    /// array is read only until it has `max_chars`, so a multi-megabyte result costs a few
    /// thousand characters instead of two full copies, and an array with one text part is
    /// borrowed rather than copied. A separator that fits inside the bound is kept even when
    /// nothing follows it, as the flatten-then-cut path keeps it.
    ///
    /// A string payload is returned whole, as [`Self::result_text`] does: it is borrowed, so
    /// cutting it here would only scan it a second time before the caller cuts it.
    pub(crate) fn result_text_head(&self, max_chars: usize) -> Cow<'a, str> {
        match self.fields.get("content") {
            Some(Value::String(content)) => Cow::Borrowed(content.as_str()),
            Some(Value::Array(blocks)) => join_head(blocks, max_chars),
            _ => Cow::Borrowed(""),
        }
    }
}

/// The array case of [`ContentBlock::result_text_head`].
fn join_head(blocks: &[Value], max_chars: usize) -> Cow<'_, str> {
    let mut parts = blocks
        .iter()
        .filter_map(block_text)
        .filter(|text| !text.is_empty());
    let Some(first) = parts.next() else {
        return Cow::Borrowed("");
    };
    // A first part that fills the bound is the whole head: no later part, however many the array
    // holds, is read.
    let (head, count) = take_chars(first, max_chars);
    if count == max_chars {
        return Cow::Borrowed(head);
    }
    let Some(second) = parts.next() else {
        return Cow::Borrowed(head);
    };
    let mut joined = String::with_capacity(capacity_hint(blocks, max_chars));
    let mut remaining = max_chars;
    for (index, part) in [first, second].into_iter().chain(parts).enumerate() {
        if remaining == 0 {
            break;
        }
        if index > 0 {
            joined.push('\n');
            remaining -= 1;
            if remaining == 0 {
                break;
            }
        }
        let (kept, count) = take_chars(part, remaining);
        remaining -= count;
        joined.push_str(kept);
    }
    Cow::Owned(joined)
}

/// Bytes to reserve for the joined head: the payload's own size, walked no further than the bound
/// needs. Only a hint, since a bound in characters can span more bytes than it counts.
fn capacity_hint(blocks: &[Value], max_chars: usize) -> usize {
    let mut bytes = 0;
    for part in blocks.iter().filter_map(block_text) {
        bytes += part.len() + 1;
        if bytes >= max_chars {
            break;
        }
    }
    bytes.min(max_chars)
}

/// The text of one element of a `tool_result` array: a bare string, or an object's `text`.
fn block_text(block: &Value) -> Option<&str> {
    match block {
        Value::String(text) => Some(text),
        Value::Object(fields) => fields.get("text").and_then(Value::as_str),
        _ => None,
    }
}

/// The first `max` characters of `text`, never cutting one in half.
///
/// Text that opens with `max` ASCII bytes is cut at byte `max` without decoding it, which is the
/// common case for the source and command output this is applied to. Anything else falls back to
/// finding the boundary character by character.
pub(crate) fn char_head(text: &str, max: usize) -> &str {
    let prefix = text.len().min(max);
    if text.as_bytes()[..prefix].is_ascii() {
        return &text[..prefix];
    }
    text.char_indices()
        .nth(max)
        .map_or(text, |(boundary, _)| &text[..boundary])
}

/// The first `max` characters of `text` and how many that is, never cutting one in half.
///
/// A prefix of `max` ASCII bytes is `max` characters, and the next byte then starts a character,
/// so text that opens with one is cut without decoding it.
fn take_chars(text: &str, max: usize) -> (&str, usize) {
    let prefix = text.len().min(max);
    if text.as_bytes()[..prefix].is_ascii() {
        return (&text[..prefix], prefix);
    }
    for (count, (boundary, _)) in text.char_indices().enumerate() {
        if count == max {
            return (&text[..boundary], max);
        }
    }
    (text, text.chars().count())
}

/// `stream_event` — a raw Anthropic streaming event, forwarded verbatim.
#[derive(Clone, Copy)]
pub struct StreamEvent<'a> {
    fields: &'a Map<String, Value>,
}

impl<'a> StreamEvent<'a> {
    /// `message_start`, `content_block_start`, `content_block_delta`, …
    pub fn kind(&self) -> Option<&'a str> {
        text(self.fields, "type")
    }

    /// Which block of the message now streaming this event is about.
    ///
    /// Indices restart at zero for each message, so one is only meaningful inside the message that
    /// stated it.
    pub fn index(&self) -> Option<u64> {
        self.fields.get("index").and_then(Value::as_u64)
    }

    /// The kind of block a `content_block_start` opened.
    pub fn content_block_type(&self) -> Option<&'a str> {
        text(object(self.fields, "content_block")?, "type")
    }

    /// A `content_block_delta`'s payload.
    pub fn delta(&self) -> Option<Delta<'a>> {
        Some(Delta {
            fields: object(self.fields, "delta")?,
        })
    }
}

/// One `content_block_delta` payload.
#[derive(Clone, Copy)]
pub struct Delta<'a> {
    fields: &'a Map<String, Value>,
}

impl<'a> Delta<'a> {
    /// `text_delta`, `thinking_delta`, `signature_delta`, `input_json_delta`, …
    pub fn kind(&self) -> Option<&'a str> {
        text(self.fields, "type")
    }

    /// A `text_delta`'s text.
    pub fn text(&self) -> Option<&'a str> {
        non_empty(self.fields, "text")
    }

    /// A `thinking_delta`'s reasoning.
    pub fn thinking(&self) -> Option<&'a str> {
        non_empty(self.fields, "thinking")
    }
}

/// `result` — the last record of a completed run.
#[derive(Clone, Copy)]
pub struct ResultRecord<'a> {
    fields: &'a Map<String, Value>,
}

impl<'a> ResultRecord<'a> {
    /// Whether the run failed. Only a literal `true` counts.
    pub fn is_error(&self) -> bool {
        self.fields.get("is_error") == Some(&Value::Bool(true))
    }

    /// The success arm's own text.
    pub fn result_text(&self) -> Option<&'a str> {
        non_empty(self.fields, "result")
    }

    /// The vendor's own stable code for why the run ended.
    ///
    /// `max_turns`, `aborted_streaming`, `hook_stopped`, `prompt_too_long`, `budget_exhausted`,
    /// among others. Read as a fallback: `errors` and `result` are prose written for a person,
    /// this is a code written for a caller.
    pub fn terminal_reason(&self) -> Option<&'a str> {
        non_empty(self.fields, "terminal_reason")
    }

    /// The error arm's own text.
    ///
    /// `error_max_turns`, `error_during_execution`, `error_max_budget_usd` and
    /// `error_max_structured_output_retries` carry their explanation here and have no `result`
    /// field at all — reading only `result`, which is where the success arm puts its text, leaves
    /// every one of these showing a generic fallback instead of what actually happened.
    pub fn error_texts(&self) -> Vec<&'a str> {
        strings(self.fields, "errors").unwrap_or_default()
    }

    /// The HTTP status the vendor's own API answered with, when one failed.
    pub fn api_error_status(&self) -> Option<i64> {
        self.fields.get("api_error_status").and_then(Value::as_i64)
    }

    /// Tokens this run used, or nothing when the vendor reported no count at all.
    pub fn usage(&self) -> Option<mango_external_agents::Usage> {
        let usage = object(self.fields, "usage")?;
        let reported = mango_external_agents::Usage {
            input_tokens: count(usage, "input_tokens"),
            output_tokens: count(usage, "output_tokens"),
            cache_read_tokens: count(usage, "cache_read_input_tokens"),
            cache_write_tokens: count(usage, "cache_creation_input_tokens"),
            reasoning_tokens: None,
            total_tokens: None,
        };
        if reported == mango_external_agents::Usage::default() {
            return None;
        }
        Some(reported)
    }
}

// Metadata-only `Debug` for the raw records and the views borrowed from them: the record kind, how
// many members it has and how large they are, never a member's value. A `tool_result` body is one
// of them, and a `Read` of a `.env` is that body. `docs/compliance.md` states the boundary and
// `crate::redacted` is the placeholder.

impl fmt::Debug for StreamRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamRecord")
            .field("kind", &redacted::label(self.kind()))
            .field("subtype", &redacted::label(self.subtype()))
            .field("members", &self.fields.len())
            .field("size", &redacted::object(&self.fields))
            .finish()
    }
}

impl fmt::Debug for InitRecord<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InitRecord")
            .field("members", &self.fields.len())
            .field("size", &redacted::object(self.fields))
            .finish()
    }
}

impl fmt::Debug for PermissionDenied<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PermissionDenied")
            .field("members", &self.fields.len())
            .field("size", &redacted::object(self.fields))
            .finish()
    }
}

impl fmt::Debug for ContentBlock<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContentBlock")
            .field("kind", &redacted::label(self.kind()))
            .field("is_error", &self.is_error())
            .field("members", &self.fields.len())
            .field("size", &redacted::object(self.fields))
            .finish()
    }
}

impl fmt::Debug for StreamEvent<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StreamEvent")
            .field("kind", &redacted::label(self.kind()))
            .field("index", &self.index())
            .field("members", &self.fields.len())
            .field("size", &redacted::object(self.fields))
            .finish()
    }
}

impl fmt::Debug for Delta<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Delta")
            .field("kind", &redacted::label(self.kind()))
            .field("members", &self.fields.len())
            .field("size", &redacted::object(self.fields))
            .finish()
    }
}

impl fmt::Debug for ResultRecord<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResultRecord")
            .field("is_error", &self.is_error())
            .field("api_error_status", &self.api_error_status())
            .field("members", &self.fields.len())
            .field("size", &redacted::object(self.fields))
            .finish()
    }
}

/// A string field, when the vendor filled it in with a string.
pub(crate) fn text<'a>(fields: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    fields.get(key)?.as_str()
}

/// A string field that has to say something to mean anything.
pub(crate) fn non_empty<'a>(fields: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    text(fields, key).filter(|value| !value.is_empty())
}

/// A nested object, when the field is one.
pub(crate) fn object<'a>(
    fields: &'a Map<String, Value>,
    key: &str,
) -> Option<&'a Map<String, Value>> {
    fields.get(key)?.as_object()
}

/// An array of strings, keeping only the members that are strings.
///
/// `None` for a field that is not an array at all, because "this build said nothing" and "this
/// build said nothing usable" are the same answer, and both differ from an empty list the vendor
/// deliberately sent.
pub(crate) fn strings<'a>(fields: &'a Map<String, Value>, key: &str) -> Option<Vec<&'a str>> {
    let Value::Array(values) = fields.get(key)? else {
        return None;
    };
    Some(values.iter().filter_map(Value::as_str).collect())
}

/// A non-negative integer count, which is the only shape a token total can honestly have.
fn count(fields: &Map<String, Value>, key: &str) -> Option<u64> {
    fields.get(key)?.as_u64()
}

#[cfg(test)]
mod tests {
    use super::{StreamRecord, capacity_hint, char_head, take_chars};

    fn record(line: &str) -> StreamRecord {
        StreamRecord::parse(line).expect("expected a parseable record")
    }

    #[test]
    fn a_line_that_is_not_json_is_dropped_rather_than_failing_the_turn() {
        assert_eq!(StreamRecord::parse(""), None);
        assert_eq!(StreamRecord::parse("Debugger listening on ws://…"), None);
        assert_eq!(StreamRecord::parse("{not json}"), None);
        assert_eq!(StreamRecord::parse("[1, 2]"), None);
    }

    #[test]
    fn a_discriminator_of_the_wrong_type_narrows_to_absent() {
        let record = record(r#"{"type":7,"subtype":null}"#);
        assert_eq!(
            record.kind(),
            None,
            "expected a numeric type to read as absent"
        );
        assert_eq!(record.subtype(), None);
    }

    #[test]
    fn the_main_conversation_has_no_parent_tool_use_id() {
        assert_eq!(
            record(r#"{"parent_tool_use_id":null}"#).parent_tool_use_id(),
            None
        );
        assert_eq!(
            record(r#"{"parent_tool_use_id":""}"#).parent_tool_use_id(),
            None
        );
        assert_eq!(
            record(r#"{"parent_tool_use_id":"toolu_1"}"#).parent_tool_use_id(),
            Some("toolu_1")
        );
    }

    #[test]
    fn a_malformed_content_block_is_skipped_rather_than_failing_the_message() {
        let record =
            record(r#"{"message":{"content":["bare", 7, {"type":"text","text":"kept"}]}}"#);
        let blocks = record.content_blocks();
        assert_eq!(
            blocks.len(),
            1,
            "expected only the object block, received {blocks:?}"
        );
        assert_eq!(blocks[0].text(), Some("kept"));
    }

    #[test]
    fn a_message_with_no_content_array_has_no_blocks() {
        assert!(
            record(r#"{"message":{"content":"plain"}}"#)
                .content_blocks()
                .is_empty()
        );
        assert!(
            record(r#"{"type":"assistant"}"#)
                .content_blocks()
                .is_empty()
        );
    }

    #[test]
    fn a_tool_result_payload_reads_as_text_whether_it_is_a_string_or_blocks() {
        let string =
            record(r#"{"message":{"content":[{"type":"tool_result","content":"1\tmango"}]}}"#);
        assert_eq!(string.content_blocks()[0].result_text(), "1\tmango");

        let blocks = record(
            r#"{"message":{"content":[{"type":"tool_result","content":[{"text":"one"},{"text":""},"two",5]}]}}"#,
        );
        assert_eq!(blocks.content_blocks()[0].result_text(), "one\ntwo");
    }

    #[test]
    fn usage_is_absent_rather_than_zero_when_the_vendor_counted_nothing() {
        assert_eq!(record(r#"{"type":"result"}"#).result().usage(), None);
        assert_eq!(
            record(r#"{"type":"result","usage":{"input_tokens":"lots"}}"#)
                .result()
                .usage(),
            None,
            "expected a non-numeric count to read as absent"
        );

        let usage = record(
            r#"{"type":"result","usage":{"input_tokens":4,"cache_read_input_tokens":30122}}"#,
        )
        .result()
        .usage()
        .expect("expected a usage report");
        assert_eq!(usage.input_tokens, Some(4));
        assert_eq!(usage.cache_read_tokens, Some(30122));
        assert_eq!(usage.output_tokens, None);
    }

    #[test]
    fn a_negative_token_count_is_refused_rather_than_wrapped() {
        assert_eq!(
            record(r#"{"type":"result","usage":{"input_tokens":-1}}"#)
                .result()
                .usage(),
            None
        );
    }

    #[test]
    fn a_block_index_the_stream_did_not_state_reads_as_absent() {
        let event =
            record(r#"{"type":"stream_event","event":{"type":"content_block_delta","index":-1}}"#);
        assert_eq!(
            event.stream_event().expect("expected an event").index(),
            None,
            "expected a negative index to read as absent"
        );
    }

    #[test]
    fn a_record_with_no_event_object_is_not_a_stream_event() {
        assert!(
            record(r#"{"type":"stream_event","event":"oops"}"#)
                .stream_event()
                .is_none()
        );
    }

    #[test]
    fn a_plugin_entry_that_is_not_an_object_is_skipped() {
        let record = record(r#"{"plugins":["bare",{"name":"code-review"},{"path":"/x"}]}"#);
        assert_eq!(record.init().plugin_names(), vec!["code-review"]);
    }

    #[test]
    fn an_absent_list_and_an_empty_one_are_different_answers() {
        assert_eq!(record(r#"{"type":"system"}"#).init().slash_commands(), None);
        assert_eq!(
            record(r#"{"type":"system","slash_commands":"clear"}"#)
                .init()
                .slash_commands(),
            None,
            "expected a non-array to read as absent, not as empty"
        );
        assert_eq!(
            record(r#"{"type":"system","slash_commands":[]}"#)
                .init()
                .slash_commands(),
            Some(Vec::new())
        );
    }

    /// A record whose one `tool_result` carries `content`.
    fn tool_result(content: &serde_json::Value) -> StreamRecord {
        record(
            &serde_json::json!({"message": {"content": [
                {"type": "tool_result", "tool_use_id": "call", "content": content}
            ]}})
            .to_string(),
        )
    }

    /// The path the reducer used before `result_text_head`: flatten everything, then cut.
    fn flatten_then_cut(record: &StreamRecord, max_chars: usize) -> String {
        record.content_blocks()[0]
            .result_text()
            .chars()
            .take(max_chars)
            .collect()
    }

    /// Payloads that exercise separators, empty and non-text elements, and characters of one to
    /// four bytes, so a cut can land on each kind of boundary.
    fn payloads() -> Vec<serde_json::Value> {
        use serde_json::json;
        let long = "x".repeat(40);
        vec![
            json!("plain string \u{e9}\u{65e5}"),
            json!(""),
            json!(null),
            json!(7),
            json!([]),
            json!([""]),
            json!(["one"]),
            json!(["one", "two"]),
            json!([{"text": "one"}, {"text": ""}, "two", 5, {"type": "image"}, {"text": 5}]),
            json!(["", "", "a", "", "b", ""]),
            json!([
                "h\u{e9}llo",
                "\u{65e5}\u{672c}\u{8a9e}",
                "\u{1f600}\u{1f600}",
                "z"
            ]),
            json!(["\u{1f600}", "\u{1f600}", "\u{1f600}"]),
            json!(["ab", "cd", "ef"]),
            json!([long, long, {"text": long}, null, long]),
            json!([null, {"text": null}, {"nested": {"text": "hidden"}}, "seen"]),
        ]
    }

    #[test]
    fn a_bounded_tool_result_matches_flattening_then_cutting_at_every_bound() {
        for payload in payloads() {
            let record = tool_result(&payload);
            for max_chars in 0..=140 {
                let expected = flatten_then_cut(&record, max_chars);
                let head = record.content_blocks()[0].result_text_head(max_chars);
                // A string payload comes back whole; an array payload must stop at the bound.
                if payload.is_array() {
                    assert!(
                        head.chars().count() <= max_chars,
                        "expected at most {max_chars} chars of {payload}, received {} chars",
                        head.chars().count()
                    );
                }
                let received: String = head.chars().take(max_chars).collect();
                assert_eq!(
                    received, expected,
                    "expected the head of {payload} at {max_chars} chars to be {expected:?}, received {received:?}"
                );
            }
        }
    }

    #[test]
    fn a_bound_that_lands_exactly_on_a_separator_keeps_the_separator_only_when_it_fits() {
        let record = tool_result(&serde_json::json!(["abc", "def"]));
        let block = &record.content_blocks()[0];
        assert_eq!(block.result_text_head(3), "abc");
        assert_eq!(block.result_text_head(4), "abc\n");
        assert_eq!(block.result_text_head(5), "abc\nd");
    }

    #[test]
    fn a_bound_never_splits_a_multi_byte_character() {
        let record = tool_result(&serde_json::json!(["\u{1f600}\u{1f600}", "\u{65e5}"]));
        let block = &record.content_blocks()[0];
        assert_eq!(block.result_text_head(1), "\u{1f600}");
        assert_eq!(block.result_text_head(3), "\u{1f600}\u{1f600}\n");
        assert_eq!(block.result_text_head(4), "\u{1f600}\u{1f600}\n\u{65e5}");
    }

    #[test]
    fn a_large_array_result_is_cut_at_the_bound() {
        let part = "y".repeat(1000);
        let parts: Vec<&str> = std::iter::repeat_n(part.as_str(), 2000).collect();
        let record = tool_result(&serde_json::json!(parts));
        let head = record.content_blocks()[0].result_text_head(4096);
        assert_eq!(
            head.chars().count(),
            4096,
            "expected exactly the bound in characters, received {}",
            head.chars().count()
        );
        assert_eq!(head, flatten_then_cut(&record, 4096));
    }

    #[test]
    fn taking_characters_agrees_with_counting_them_for_ascii_and_multi_byte_text() {
        for text in [
            "",
            "abc",
            "h\u{e9}llo",
            "\u{1f600}ab",
            "ab\u{1f600}",
            "\u{65e5}\u{672c}",
        ] {
            for max in 0..=8 {
                let expected: String = text.chars().take(max).collect();
                let (kept, count) = take_chars(text, max);
                assert_eq!(
                    (kept, count),
                    (expected.as_str(), expected.chars().count()),
                    "expected the first {max} characters of {text:?}, received {kept:?} ({count})"
                );
            }
        }
    }

    #[test]
    fn the_capacity_hint_is_the_payload_size_capped_at_the_bound() {
        let blocks = [
            serde_json::json!("abcd"),
            serde_json::json!({"text": "ef"}),
            serde_json::json!(5),
        ];
        // "abcd" and "ef" plus a separator's byte after each: 5 + 3.
        assert_eq!(capacity_hint(&blocks, 100), 8);
        assert_eq!(capacity_hint(&blocks, 6), 6);
        assert_eq!(capacity_hint(&blocks, 0), 0);
        assert_eq!(capacity_hint(&[], 10), 0);
    }

    #[test]
    fn a_first_part_that_fills_the_bound_is_borrowed_without_reading_further_parts() {
        let record = tool_result(&serde_json::json!(["abcdef", "", 5, "later part"]));
        let head = record.content_blocks()[0].result_text_head(4);
        assert_eq!(head, "abcd");
        assert!(
            matches!(head, std::borrow::Cow::Borrowed(_)),
            "expected a borrowed head once the first part fills the bound, received {head:?}"
        );
        let exact = tool_result(&serde_json::json!(["abcd", "later part"]));
        let head = exact.content_blocks()[0].result_text_head(4);
        assert!(
            matches!(head, std::borrow::Cow::Borrowed("abcd")),
            "expected the first part alone at a bound equal to its length, received {head:?}"
        );
    }

    /// What the reducer's `head` did before the ASCII fast path: decode until the boundary.
    fn decoding_head(text: &str, max: usize) -> &str {
        text.char_indices()
            .nth(max)
            .map_or(text, |(boundary, _)| &text[..boundary])
    }

    #[test]
    fn the_character_head_matches_decoding_for_ascii_and_multi_byte_text_at_every_cut() {
        let long_ascii = "a".repeat(40);
        let mixed_tail = format!("{}\u{e9}{}", "a".repeat(20), "b".repeat(20));
        let mixed_head = format!("\u{1f600}{}", "a".repeat(20));
        let texts = [
            "",
            "abc",
            "h\u{e9}llo",
            "\u{65e5}\u{672c}\u{8a9e}",
            "\u{1f600}\u{1f600}\u{1f600}",
            "ab\u{1f600}cd",
            "\u{e9}",
            long_ascii.as_str(),
            mixed_tail.as_str(),
            mixed_head.as_str(),
        ];
        for text in texts {
            for max in 0..=45 {
                let expected = decoding_head(text, max);
                let received = char_head(text, max);
                assert_eq!(
                    received, expected,
                    "expected the first {max} chars of {text:?} to be {expected:?}, received {received:?}"
                );
            }
        }
    }
}
