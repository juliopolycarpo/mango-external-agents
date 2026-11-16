//! `RequestFingerprint::of` over requests whose attachments carry most of the bytes.
//!
//! The fingerprint hashes every attachment byte each time a host builds a `RecoveryRecord` or
//! validates a retry against one, so its cost grows with the attachments and is mostly SHA-256 throughput. `prompt-only` is the floor a request without
//! attachments pays, timed over a batch because one call is too short for the clock.
//! `attachments-4x2MiB` is the most a turn can carry (`TURN_MAX_ATTACHMENTS` of
//! `ATTACHMENT_MAX_BYTES`); `attachment-64MiB` is past that cap and shows the throughput alone.
//! Requests are built in `setup` and freed after the clock stops. Run with:
//!
//! ```sh
//! cargo bench -p mango-external-agents --bench fingerprint
//! ```

mod support;

use mango_external_agents::session::{ATTACHMENT_MAX_BYTES, TURN_MAX_ATTACHMENTS};
use mango_external_agents::{Attachment, AttachmentKind, RequestFingerprint, TurnRequest};
use support::{Bench, Unit};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// Prompt-only fingerprints per sample.
const PROMPT_ONLY_BATCH: usize = 1000;

/// One binary attachment of `size` bytes, in a pattern no branch predictor shortcuts.
fn binary_attachment(index: usize, size: usize) -> Attachment {
    Attachment {
        id: format!("file-{index}"),
        name: format!("blob-{index}.bin"),
        mime_type: "application/octet-stream".into(),
        kind: AttachmentKind::Data,
        bytes: (0..size).map(|offset| (offset % 251) as u8).collect(),
    }
}

/// A request carrying `count` binary attachments of `size` bytes each.
fn request_with_attachments(count: usize, size: usize) -> TurnRequest {
    TurnRequest::new("turn", "summarize the attachments").with_attachments(
        (0..count)
            .map(|index| binary_attachment(index, size))
            .collect(),
    )
}

fn fingerprint(request: &TurnRequest) -> RequestFingerprint {
    RequestFingerprint::of(request).expect("a benchmark request is serializable")
}

fn main() {
    let bench = Bench::new("fingerprint");
    bench.run(
        "fingerprint/prompt-only",
        Unit::new(PROMPT_ONLY_BATCH as u64, "request"),
        || TurnRequest::new("turn", "hello"),
        |request| {
            for _ in 0..PROMPT_ONLY_BATCH {
                std::hint::black_box(fingerprint(std::hint::black_box(&request)));
            }
            request
        },
    );
    let cases = [
        ("attachment-1KiB", 1, KIB),
        ("attachment-1MiB", 1, MIB),
        (
            "attachments-4x2MiB",
            TURN_MAX_ATTACHMENTS,
            ATTACHMENT_MAX_BYTES,
        ),
        ("attachment-64MiB", 1, 64 * MIB),
    ];
    for (label, count, size) in cases {
        bench.run(
            &format!("fingerprint/{label}"),
            Unit::new((count * size) as u64, "byte"),
            || request_with_attachments(count, size),
            // Returning the request keeps freeing its attachments off the clock.
            |request| (fingerprint(&request), request),
        );
    }
}
