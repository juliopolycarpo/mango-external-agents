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

use serde_json::{Map, Value};

/// One line of `claude --print --output-format stream-json`, before it has been recognised.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Copy, Debug)]
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
#[derive(Clone, Copy, Debug)]
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
#[derive(Clone, Copy, Debug)]
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
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => Cow::Borrowed(""),
        }
    }
}

fn block_text(block: &Value) -> Option<String> {
    match block {
        Value::String(text) => Some(text.clone()),
        Value::Object(fields) => fields
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned),
        _ => None,
    }
}

/// `stream_event` — a raw Anthropic streaming event, forwarded verbatim.
#[derive(Clone, Copy, Debug)]
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
#[derive(Clone, Copy, Debug)]
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
#[derive(Clone, Copy, Debug)]
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
    use super::StreamRecord;

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
}
