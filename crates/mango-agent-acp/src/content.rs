//! A turn's text and files as ACP v1 content blocks.
//!
//! Every refusal here is a refusal to send something the agent said it could not take. ACP's
//! `initialize` reports `promptCapabilities`, so an image reaching an agent that never advertised
//! `image` is a request the agent will reject with a protocol error halfway through a turn — and a
//! turn that failed for that reason reads to a user as "the agent broke", not as "that file was never
//! going to work".
//!
//! ACP v1 reference: <https://agentclientprotocol.com/protocol/v1/content>
//!
//! The library never reads a file: the bytes arrive on
//! [`Attachment`](mango_external_agents::Attachment) from the host, which owns the filesystem.

use agent_client_protocol::schema::v1::{
    BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource, ImageContent,
    PromptCapabilities, TextContent, TextResourceContents,
};
use base64::Engine as _;
use mango_external_agents::session::{ATTACHMENT_MAX_BYTES, TURN_MAX_ATTACHMENTS};
use mango_external_agents::{Attachment, AttachmentKind, Error, Result};

/// One turn's prompt: its text, then its files.
///
/// # Errors
///
/// [`Error::Protocol`] when the turn carries more than [`TURN_MAX_ATTACHMENTS`] files, an attachment
/// over [`ATTACHMENT_MAX_BYTES`], or a kind this agent's `promptCapabilities` did not advertise.
pub fn prompt(
    input: &str,
    attachments: &[Attachment],
    capabilities: &PromptCapabilities,
) -> Result<Vec<ContentBlock>> {
    if attachments.len() > TURN_MAX_ATTACHMENTS {
        return Err(Error::Protocol {
            expected: format!("at most {TURN_MAX_ATTACHMENTS} attachments"),
            received: attachments.len().to_string(),
        });
    }
    let mut blocks = Vec::with_capacity(attachments.len() + 1);
    blocks.push(ContentBlock::Text(TextContent::new(input)));
    for attachment in attachments {
        blocks.push(block(attachment, capabilities)?);
    }
    Ok(blocks)
}

fn block(attachment: &Attachment, capabilities: &PromptCapabilities) -> Result<ContentBlock> {
    if attachment.bytes.len() > ATTACHMENT_MAX_BYTES {
        return Err(Error::Protocol {
            expected: format!("an attachment of at most {ATTACHMENT_MAX_BYTES} bytes"),
            received: format!("{:?} at {} bytes", attachment.name, attachment.bytes.len()),
        });
    }
    match attachment.kind {
        AttachmentKind::Image => {
            require(capabilities.image, "image", attachment)?;
            Ok(ContentBlock::Image(ImageContent::new(
                encode(&attachment.bytes),
                attachment.mime_type.clone(),
            )))
        }
        AttachmentKind::Text => {
            require(
                capabilities.embedded_context,
                "embedded context",
                attachment,
            )?;
            // Text goes as text when it is text. An agent that received a base64 blob for a source
            // file would have to decode it before it could read one line of it.
            let text =
                String::from_utf8(attachment.bytes.clone()).map_err(|_| Error::Protocol {
                    expected: String::from("valid UTF-8 in a text attachment"),
                    received: format!("{:?}, which is not UTF-8", attachment.name),
                })?;
            Ok(ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::TextResourceContents(
                    TextResourceContents::new(text, uri(attachment))
                        .mime_type(attachment.mime_type.clone()),
                ),
            )))
        }
        AttachmentKind::Pdf | AttachmentKind::Data | AttachmentKind::Unknown => {
            require(
                capabilities.embedded_context,
                "embedded context",
                attachment,
            )?;
            Ok(ContentBlock::Resource(EmbeddedResource::new(
                EmbeddedResourceResource::BlobResourceContents(
                    BlobResourceContents::new(encode(&attachment.bytes), uri(attachment))
                        .mime_type(attachment.mime_type.clone()),
                ),
            )))
        }
        // `#[non_exhaustive]`: a kind this build does not know cannot be given a wire shape, and
        // guessing one would send bytes the agent reads as something else.
        _ => Err(Error::Protocol {
            expected: String::from("an attachment kind this harness can encode"),
            received: format!("{:?}", attachment.kind),
        }),
    }
}

fn require(advertised: bool, capability: &str, attachment: &Attachment) -> Result<()> {
    if advertised {
        return Ok(());
    }
    Err(Error::Protocol {
        expected: format!("an agent advertising the {capability} prompt capability"),
        received: format!("one that did not, for the attachment {:?}", attachment.name),
    })
}

/// The URI an embedded resource is named by.
///
/// `attachment:` rather than `file:`: the host holds the bytes and has not said where they came
/// from, and a `file:` URI would name a path on the agent's own machine that an agent could then try
/// to read. The scheme is opaque and the name is what a person would recognise.
fn uri(attachment: &Attachment) -> String {
    format!("attachment:{}/{}", attachment.id, attachment.name)
}

fn encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::prompt;
    use agent_client_protocol::schema::v1::{
        ContentBlock, EmbeddedResourceResource, PromptCapabilities,
    };
    use mango_external_agents::session::TURN_MAX_ATTACHMENTS;
    use mango_external_agents::{Attachment, AttachmentKind, Error};

    fn everything() -> PromptCapabilities {
        serde_json::from_value(serde_json::json!({
            "image": true, "audio": true, "embeddedContext": true
        }))
        .expect("expected v1 prompt capabilities")
    }

    fn nothing() -> PromptCapabilities {
        PromptCapabilities::default()
    }

    fn attachment(kind: AttachmentKind, bytes: Vec<u8>) -> Attachment {
        Attachment {
            id: String::from("att-1"),
            name: String::from("notes.txt"),
            mime_type: String::from("text/plain"),
            kind,
            bytes,
        }
    }

    #[test]
    fn the_prompt_text_always_comes_first() {
        let blocks = prompt("ship it", &[], &nothing()).expect("expected a prompt");
        let [ContentBlock::Text(text)] = blocks.as_slice() else {
            panic!("expected one text block, received {blocks:?}");
        };
        assert_eq!(text.text, "ship it");
    }

    /// A source file sent as a base64 blob is a file the agent has to decode before it can read one
    /// line of it.
    #[test]
    fn a_text_attachment_goes_as_text_rather_than_as_a_blob() {
        let blocks = prompt(
            "review this",
            &[attachment(AttachmentKind::Text, b"fn main() {}".to_vec())],
            &everything(),
        )
        .expect("expected a prompt");

        let [_, ContentBlock::Resource(resource)] = blocks.as_slice() else {
            panic!("expected a text then a resource, received {blocks:?}");
        };
        let EmbeddedResourceResource::TextResourceContents(contents) = &resource.resource else {
            panic!("expected text resource contents, received {resource:?}");
        };
        assert_eq!(contents.text, "fn main() {}");
        assert_eq!(contents.uri, "attachment:att-1/notes.txt");
        assert_eq!(contents.mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn an_image_attachment_is_base64_with_the_hosts_own_media_type() {
        let mut image = attachment(AttachmentKind::Image, vec![0x89, 0x50, 0x4e, 0x47]);
        image.mime_type = String::from("image/png");
        let blocks = prompt("look", &[image], &everything()).expect("expected a prompt");

        let [_, ContentBlock::Image(content)] = blocks.as_slice() else {
            panic!("expected an image block, received {blocks:?}");
        };
        assert_eq!(content.data, "iVBORw==");
        assert_eq!(content.mime_type, "image/png");
    }

    #[test]
    fn a_binary_attachment_is_an_embedded_blob() {
        let mut pdf = attachment(AttachmentKind::Pdf, b"%PDF-1.7".to_vec());
        pdf.mime_type = String::from("application/pdf");
        pdf.name = String::from("spec.pdf");
        let blocks = prompt("read", &[pdf], &everything()).expect("expected a prompt");

        let [_, ContentBlock::Resource(resource)] = blocks.as_slice() else {
            panic!("expected a resource block, received {blocks:?}");
        };
        let EmbeddedResourceResource::BlobResourceContents(contents) = &resource.resource else {
            panic!("expected blob resource contents, received {resource:?}");
        };
        assert_eq!(contents.uri, "attachment:att-1/spec.pdf");
        assert_eq!(contents.mime_type.as_deref(), Some("application/pdf"));
    }

    /// Refused here rather than rejected by the agent mid-turn: a turn that failed because the agent
    /// cannot take images reads to a user as "the agent broke".
    #[test]
    fn an_attachment_the_agent_never_advertised_is_refused_before_the_turn_starts() {
        let error = prompt(
            "look",
            &[attachment(AttachmentKind::Image, vec![1, 2, 3])],
            &nothing(),
        )
        .expect_err("expected a refusal, received a prompt");
        assert!(
            matches!(&error, Error::Protocol { expected, .. } if expected.contains("image")),
            "received {error:?}"
        );
    }

    #[test]
    fn more_attachments_than_one_turn_carries_are_refused() {
        let many: Vec<Attachment> = (0..=TURN_MAX_ATTACHMENTS)
            .map(|_| attachment(AttachmentKind::Text, b"x".to_vec()))
            .collect();
        let error = prompt("look", &many, &everything())
            .expect_err("expected a refusal, received a prompt");
        assert!(
            matches!(&error, Error::Protocol { expected, .. } if expected.contains("attachments")),
            "received {error:?}"
        );
    }

    #[test]
    fn an_oversized_attachment_is_refused_with_its_own_size() {
        let big = attachment(
            AttachmentKind::Text,
            vec![b'x'; mango_external_agents::session::ATTACHMENT_MAX_BYTES + 1],
        );
        let error = prompt("look", &[big], &everything())
            .expect_err("expected a refusal, received a prompt");
        let Error::Protocol { received, .. } = &error else {
            panic!("received {error:?}");
        };
        assert!(received.contains("notes.txt"), "received {received:?}");
    }

    #[test]
    fn a_text_attachment_that_is_not_utf8_is_refused_rather_than_mangled() {
        let error = prompt(
            "look",
            &[attachment(AttachmentKind::Text, vec![0xff, 0xfe])],
            &everything(),
        )
        .expect_err("expected a refusal, received a prompt");
        assert!(
            matches!(&error, Error::Protocol { expected, .. } if expected.contains("UTF-8")),
            "received {error:?}"
        );
    }
}
