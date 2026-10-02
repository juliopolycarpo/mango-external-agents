//! The size of a `session/prompt` frame, measured before the turn is submitted.
//!
//! The bounded transport refuses any outgoing frame over `HostContext::outbound_buffer_bytes()`.
//! It sees the frame only after the turn has started: the connection fails, the session closes and the host
//! is told the acceptance is unknown. Each attachment is capped alone, and the encoding grows what
//! it carries (base64 by a third, a control character sixfold), so inputs that pass every earlier
//! check can still overflow. This module measures the frame the SDK will write so the overflow is
//! refused while nothing has been submitted.

use std::io;

use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, RequestId, SessionId};
use agent_client_protocol::{JsonRpcMessage, RawJsonRpcMessage, TransportFrame};
use mango_external_agents::{Error, Result};

use crate::transport::{Overflow, outgoing_frame_limit};

/// Stands in for the request id the SDK assigns, which is a hyphenated UUID of this exact length.
///
/// The SDK draws the real id at send time, so it cannot be read beforehand; only its length matters
/// to the frame's size. A test compares the prediction with the frame the transport really wrote.
const REQUEST_ID_PLACEHOLDER: &str = "00000000-0000-4000-8000-000000000000";

/// Refuses a prompt whose `session/prompt` frame would exceed the outgoing frame budget.
///
/// Call this before anything is submitted: the returned error is a local refusal, so the caller
/// marks it not submitted and the session stays usable.
///
/// # Errors
///
/// [`Error::LimitExceeded`] naming the frame's byte count and effective outgoing budget when the
/// frame is over; [`Error::Protocol`] when the prompt cannot be serialised at all.
///
/// # Example
///
/// ```ignore
/// let prompt = content::prompt("hello", &[], &capabilities)?;
/// refuse_oversized_prompt(&session_id, &prompt, host.outbound_buffer_bytes())?;
/// ```
pub(crate) fn refuse_oversized_prompt(
    session_id: &SessionId,
    prompt: &[ContentBlock],
    outbound_buffer_bytes: usize,
) -> Result<()> {
    let limit = outgoing_frame_limit(outbound_buffer_bytes);
    let received = prompt_frame_bytes(session_id, prompt)?;
    match received > limit {
        true => Err(Overflow::outgoing_frame(limit, received).error()),
        false => Ok(()),
    }
}

/// The exact byte length of the `session/prompt` frame the SDK writes for `prompt`, without its
/// trailing newline.
///
/// The fixed part is measured by the official crate: a request with an empty prompt is put through
/// the SDK's own frame serialiser under a request id of the length it draws. The blocks are then
/// counted as they serialise, into a sink that keeps nothing, so no second copy of an attachment is
/// built.
///
/// This assumes the SDK writes the request exactly as it serialises it. Between the two the SDK
/// runs a protocol-compatibility step and a role-style step, and both return the message unchanged
/// for a client-to-agent v1 request in this build (`unstable_protocol_v2` is off). If a later SDK
/// starts adding bytes there, the session-level boundary test in `tests/session/prompt_frame.rs`,
/// which compares this count with the frame the transport really wrote, is what fails.
///
/// # Errors
///
/// [`Error::Protocol`] when the request cannot be serialised.
pub(crate) fn prompt_frame_bytes(session_id: &SessionId, prompt: &[ContentBlock]) -> Result<usize> {
    let empty = PromptRequest::new(session_id.clone(), Vec::<ContentBlock>::new());
    let untyped = empty.to_untyped_message().map_err(unserialisable)?;
    let (method, params) = untyped.into_parts();
    let message = RawJsonRpcMessage::request(
        method,
        params,
        RequestId::Str(REQUEST_ID_PLACEHOLDER.to_owned()),
    )
    .map_err(unserialisable)?;
    let envelope = TransportFrame::Single(message)
        .to_json()
        .map_err(unserialisable)?
        .len();

    let mut counter = ByteCounter::default();
    serde_json::to_writer(&mut counter, prompt).map_err(|error| Error::Protocol {
        expected: String::from("a serialisable session/prompt content array"),
        received: error.to_string(),
    })?;
    // The empty request already carries `[]`; the counted array replaces it.
    Ok(envelope - "[]".len() + counter.bytes)
}

fn unserialisable(error: agent_client_protocol::Error) -> Error {
    Error::Protocol {
        expected: String::from("a serialisable session/prompt request"),
        received: format!("{error}"),
    }
}

/// An `io::Write` that counts what it is given and keeps none of it.
#[derive(Default)]
struct ByteCounter {
    bytes: usize,
}

impl io::Write for ByteCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        BlobResourceContents, EmbeddedResource, EmbeddedResourceResource, ImageContent,
        TextContent, TextResourceContents,
    };

    use super::*;
    use mango_external_agents::Limits;

    fn session_id() -> SessionId {
        SessionId::new("sess_0123456789abcdef")
    }

    /// The frame the SDK would write, built the long way: the whole request into one value, then
    /// the SDK's own serialiser, under an id of the length the SDK draws.
    fn frame_by_serialising(prompt: Vec<ContentBlock>) -> String {
        let request = PromptRequest::new(session_id(), prompt);
        let (method, params) = request
            .to_untyped_message()
            .expect("expected an untyped prompt")
            .into_parts();
        let message = RawJsonRpcMessage::request(method, params, RequestId::Str(uuid_shaped_id()))
            .expect("expected a raw request");
        TransportFrame::Single(message)
            .to_json()
            .expect("expected a frame")
    }

    fn uuid_shaped_id() -> String {
        String::from("f47ac10b-58cc-4372-a567-0e02b2c3d479")
    }

    fn text(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text))
    }

    fn resource(text: &str) -> ContentBlock {
        ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(
                TextResourceContents::new(text, "attachment:a/b.txt").mime_type("text/plain"),
            ),
        ))
    }

    fn blob(data: &str) -> ContentBlock {
        ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(
                BlobResourceContents::new(data, "attachment:a/b.bin")
                    .mime_type("application/octet-stream"),
            ),
        ))
    }

    fn image(data: &str) -> ContentBlock {
        ContentBlock::Image(ImageContent::new(data, "image/png"))
    }

    #[test]
    fn the_counted_size_is_the_size_of_the_frame_the_sdk_serialises() {
        let escapes = "\u{1}\u{1f}\"\\\n\t\u{7f}é世🦀";
        let prompts: Vec<(&str, Vec<ContentBlock>)> = vec![
            ("no blocks", Vec::new()),
            ("empty text", vec![text("")]),
            ("plain text", vec![text("inspect the diff")]),
            ("escapes", vec![text(escapes), resource(escapes)]),
            (
                "every attachment shape",
                vec![
                    text("look"),
                    image("AAAA"),
                    resource("fn main() {}"),
                    blob("BBBB"),
                ],
            ),
            ("control characters", vec![text(&"\u{1}".repeat(4096))]),
        ];
        for (label, prompt) in prompts {
            let expected = frame_by_serialising(prompt.clone()).len();
            let received = prompt_frame_bytes(&session_id(), &prompt).expect("expected a size");
            assert_eq!(
                received, expected,
                "expected the counted size to equal the serialised frame for {label} | received {received} against {expected}"
            );
        }
    }

    #[test]
    fn a_longer_session_id_lengthens_the_frame_by_exactly_its_length() {
        let prompt = vec![text("hello")];
        let short = prompt_frame_bytes(&SessionId::new("a"), &prompt).expect("expected a size");
        let long =
            prompt_frame_bytes(&SessionId::new("a".repeat(101)), &prompt).expect("expected a size");
        assert_eq!(
            long - short,
            100,
            "expected 100 more bytes for a 100-byte-longer session id | received {}",
            long - short
        );
    }

    #[test]
    fn a_prompt_at_the_frame_limit_is_accepted_and_one_byte_more_is_refused() {
        let session_id = session_id();
        let base = prompt_frame_bytes(&session_id, &[text("")]).expect("expected a size");
        let limits = Limits {
            turn_buffer_bytes: base + 100,
            ..Limits::default()
        };

        let fits = [text(&"a".repeat(100))];
        assert_eq!(
            prompt_frame_bytes(&session_id, &fits).expect("expected a size"),
            limits.turn_buffer_bytes,
            "expected the fixture to fill the limit exactly"
        );
        assert!(
            refuse_oversized_prompt(&session_id, &fits, limits.turn_buffer_bytes).is_ok(),
            "expected a frame of exactly {} bytes to be accepted",
            limits.turn_buffer_bytes
        );

        let over = [text(&"a".repeat(101))];
        let error = refuse_oversized_prompt(&session_id, &over, limits.turn_buffer_bytes)
            .expect_err("expected a frame one byte over to be refused");
        assert!(
            matches!(
                &error,
                Error::LimitExceeded { subject, limit, received }
                    if *subject == "bytes in one frame to the ACP agent"
                        && *limit == limits.turn_buffer_bytes
                        && *received == limits.turn_buffer_bytes + 1
            ),
            "expected LimitExceeded at {} + 1 bytes | received {error:?}",
            limits.turn_buffer_bytes
        );
    }

    #[test]
    fn a_limit_beyond_the_permit_range_is_capped_like_the_transport() {
        let limits = Limits {
            turn_buffer_bytes: usize::MAX,
            ..Limits::default()
        };
        let prompt = [text("hello")];
        assert!(
            refuse_oversized_prompt(&session_id(), &prompt, limits.turn_buffer_bytes).is_ok(),
            "expected a small prompt to fit an unbounded budget"
        );
    }

    #[test]
    fn the_byte_counter_counts_every_write_and_keeps_none() {
        use std::io::Write as _;

        let mut counter = ByteCounter::default();
        counter.write_all(b"abc").expect("expected a write");
        counter.write_all(b"").expect("expected a write");
        counter.write_all(&[0; 10]).expect("expected a write");
        counter.flush().expect("expected a flush");
        assert_eq!(
            counter.bytes, 13,
            "expected 13 counted bytes | received {}",
            counter.bytes
        );
    }
}
