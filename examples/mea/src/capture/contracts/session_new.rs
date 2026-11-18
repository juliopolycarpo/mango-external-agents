//! The `session/new` answer an ACP capture keeps when the operator opts in with `--open-session`.
//!
//! `initialize` says what an adapter can do; only `session/new` returns the permission mode ids and
//! the `configOptions` (the model catalog among them) a host reads. The request carries no login and
//! no credential: the capture never sends `authenticate`, and an adapter that wants a login refuses
//! with a JSON-RPC error, which is recorded instead. Neither outcome is a failed capture.
//!
//! The record keeps the members a host reads and drops the rest. The session id is per run, the
//! descriptions carry prices and wording that change between releases, and `_meta` is the adapter's
//! own extension space.

use mango_external_agents::error::{Error, Result};
use mango_external_agents::{ErrorCode, JsonRpcError};
use serde_json::{Map, Value, json};

/// What a `session/new` request answered, ready to write as `session-new.json`.
#[derive(Debug, PartialEq)]
pub(super) struct SessionNewRecord {
    /// `{"result": {...}}` for an opened session, `{"error": {"code", "message"}}` for a refusal.
    pub record: Value,
    /// Whether the adapter opened a session. A refusal is a recorded answer, not an opened session.
    pub opened: bool,
}

/// The members of a mode, a config option or one of its values that a host reads, in the order the
/// fixture lists them. A member holding anything but a scalar or the nested `options` is dropped.
const PUBLIC_MEMBERS: &[&str] = &[
    "id",
    "name",
    "category",
    "type",
    "currentValue",
    "value",
    "group",
];

/// Normalizes a `session/new` reply frame.
///
/// # Examples
///
/// A reply carrying `modes` and `configOptions` becomes `{"result": {"modes": ..., "configOptions": ...}}`
/// with `opened` true; `{"error": {"code": -32000, "message": "Authentication required"}}` becomes
/// the same error with `opened` false.
///
/// # Errors
///
/// Returns [`Error::Protocol`] naming the received shape when the frame holds neither an error
/// object with an integer `code` and a string `message` nor a result object with a string
/// `sessionId`.
pub(super) fn normalize_session_new(frame: &Value) -> Result<SessionNewRecord> {
    // A `null` error member is no error: some peers write one beside a result.
    if let Some(error) = frame.get("error").filter(|error| !error.is_null()) {
        let refusal =
            serde_json::from_value::<JsonRpcError>(error.clone()).map_err(|_| Error::Protocol {
                expected: String::from(
                    "an ACP session/new JSON-RPC error with an integer code and a string message",
                ),
                received: super::value_shape(error),
            })?;
        let code = refusal.code;
        // The same redaction and bound the harness applies to an agent's message.
        let vendor = refusal.into_vendor_error(ErrorCode::from_static("acp-request-failed"), None);
        return Ok(SessionNewRecord {
            record: json!({"error": {"code": code, "message": vendor.message}}),
            opened: false,
        });
    }
    let result = frame
        .get("result")
        .and_then(Value::as_object)
        .filter(|result| result.get("sessionId").is_some_and(Value::is_string))
        .ok_or_else(|| Error::Protocol {
            expected: String::from("an ACP session/new result object with a string sessionId"),
            received: super::value_shape(frame),
        })?;
    let mut kept = Map::new();
    if let Some(modes) = result.get("modes").and_then(Value::as_object) {
        kept.insert(String::from("modes"), public_modes(modes));
    }
    if let Some(options) = result.get("configOptions").and_then(Value::as_array) {
        kept.insert(
            String::from("configOptions"),
            Value::Array(options.iter().filter_map(public_entry).collect()),
        );
    }
    Ok(SessionNewRecord {
        record: json!({"result": kept}),
        opened: true,
    })
}

fn public_modes(modes: &Map<String, Value>) -> Value {
    let mut kept = Map::new();
    if let Some(current) = modes.get("currentModeId").filter(|value| value.is_string()) {
        kept.insert(String::from("currentModeId"), current.clone());
    }
    if let Some(available) = modes.get("availableModes").and_then(Value::as_array) {
        kept.insert(
            String::from("availableModes"),
            Value::Array(available.iter().filter_map(public_entry).collect()),
        );
    }
    Value::Object(kept)
}

/// One mode, config option, option value or option group, reduced to [`PUBLIC_MEMBERS`] and its
/// nested `options`. Anything that is not an object is dropped.
fn public_entry(entry: &Value) -> Option<Value> {
    let members = entry.as_object()?;
    let mut kept = Map::new();
    for key in PUBLIC_MEMBERS {
        if let Some(value) = members
            .get(*key)
            .filter(|value| matches!(value, Value::String(_) | Value::Bool(_) | Value::Number(_)))
        {
            kept.insert((*key).to_owned(), value.clone());
        }
    }
    if let Some(nested) = members.get("options").and_then(Value::as_array) {
        kept.insert(
            String::from("options"),
            Value::Array(nested.iter().filter_map(public_entry).collect()),
        );
    }
    Some(Value::Object(kept))
}

#[cfg(test)]
mod tests {
    use super::normalize_session_new;
    use mango_external_agents::Error;
    use serde_json::json;

    fn opened_session_reply() -> serde_json::Value {
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "sessionId": "90f21df2-5313-455a-8e9e-57b777f13770",
                "_meta": {"installationPath": "/private/agent"},
                "modes": {
                    "currentModeId": "auto",
                    "availableModes": [
                        {"id": "default", "name": "Manual", "description": "Always ask"},
                        {"id": "auto", "name": "Auto", "description": "Decides"}
                    ]
                },
                "configOptions": [{
                    "id": "model",
                    "name": "Model",
                    "description": "AI model to use",
                    "category": "model",
                    "type": "select",
                    "currentValue": "default",
                    "_meta": {"path": "/private/agent"},
                    "options": [
                        {"value": "default", "name": "Default", "description": "Opus $4/$20 per Mtok"},
                        {"group": "legacy", "name": "Legacy", "options": [{"value": "old", "name": "Old"}]}
                    ]
                }]
            }
        })
    }

    #[test]
    fn an_opened_session_keeps_the_mode_ids_and_the_model_catalog_only() {
        let record = normalize_session_new(&opened_session_reply())
            .expect("expected a session/new result to be recorded");

        assert!(record.opened, "expected an opened session");
        assert_eq!(
            record.record,
            json!({"result": {
                "modes": {
                    "currentModeId": "auto",
                    "availableModes": [
                        {"id": "default", "name": "Manual"},
                        {"id": "auto", "name": "Auto"}
                    ]
                },
                "configOptions": [{
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "default",
                    "options": [
                        {"value": "default", "name": "Default"},
                        {"group": "legacy", "name": "Legacy", "options": [{"value": "old", "name": "Old"}]}
                    ]
                }]
            }}),
            "expected ids and names without the session id, descriptions or extensions"
        );
        let text = record.record.to_string();
        for dropped in ["90f21df2", "/private/agent", "$4/$20", "Always ask"] {
            assert!(
                !text.contains(dropped),
                "expected {dropped:?} left out of the record | received {text}"
            );
        }
    }

    #[test]
    fn a_login_refusal_is_recorded_with_its_code_and_redacted_message() {
        let record = normalize_session_new(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "error": {
                "code": -32000,
                "message": "Authentication required API_KEY=private-token",
                "data": {"detail": "private"}
            }
        }))
        .expect("expected a refusal to be a recorded answer");

        assert!(
            !record.opened,
            "expected a refusal not to count as a session"
        );
        assert_eq!(record.record["error"]["code"], -32000);
        let message = record.record["error"]["message"]
            .as_str()
            .expect("expected a message string");
        assert!(
            message.starts_with("Authentication required"),
            "expected the agent's message | received {message}"
        );
        assert!(
            !record.record.to_string().contains("private"),
            "expected credential-shaped text and data redacted | received {}",
            record.record
        );
    }

    #[test]
    fn a_reply_without_a_session_id_or_an_error_is_refused_by_shape() {
        for frame in [
            json!({"id": 2, "result": {"modes": {}}}),
            json!({"id": 2, "result": {"sessionId": 7}}),
            json!({"id": 2, "result": "ok"}),
            json!({"id": 2, "error": "boom"}),
            json!({"id": 2, "error": {"message": "no code"}}),
        ] {
            let refused = normalize_session_new(&frame)
                .expect_err("expected a malformed session/new reply to be refused");
            assert!(
                matches!(&refused, Error::Protocol { expected, .. } if expected.contains("session/new")),
                "expected a session/new shape refusal for {frame} | received {refused:?}"
            );
        }
    }

    #[test]
    fn a_null_error_beside_a_result_is_no_refusal() {
        let record = normalize_session_new(&json!({
            "id": 2,
            "error": null,
            "result": {"sessionId": "s"}
        }))
        .expect("expected the result to be recorded");

        assert!(record.opened);
        assert_eq!(record.record, json!({"result": {}}));
    }
}
