use super::*;
use crate::{
    Attachment, AttachmentKind, AttemptId, CancelReason, ConfigurationChange, ConfigurationPatch,
};

#[test]
fn fingerprint_preserves_every_input_field_but_not_attempt_identity() {
    let request = TurnRequest::new("logical", "prompt").with_attachments(vec![Attachment {
        id: "file".into(),
        name: "input.txt".into(),
        mime_type: "text/plain".into(),
        kind: AttachmentKind::Text,
        bytes: b"content".to_vec(),
    }]);
    let fingerprint = RequestFingerprint::of(&request).expect("fingerprint");
    assert_eq!(
        fingerprint,
        RequestFingerprint::of(&request.clone().as_attempt(AttemptId::new(2)))
            .expect("retry fingerprint")
    );
    let changes: Vec<TurnRequest> = (0..7)
        .map(|index| {
            let mut changed = request.clone();
            match index {
                0 => changed.input.push('!'),
                1 => changed.attachments[0].id.push('!'),
                2 => changed.attachments[0].name.push('!'),
                3 => changed.attachments[0].mime_type.push('!'),
                4 => changed.attachments[0].kind = AttachmentKind::Data,
                5 => changed.attachments[0].bytes.push(0),
                _ => {
                    changed.configuration =
                        Some(ConfigurationPatch::new().model(ConfigurationChange::Reset))
                }
            }
            changed
        })
        .collect();
    for changed in changes {
        assert_ne!(
            fingerprint,
            RequestFingerprint::of(&changed).expect("changed digest")
        );
    }
    assert_eq!(
        serde_json::from_str::<RequestFingerprint>(
            &serde_json::to_string(&fingerprint).expect("serialize")
        )
        .expect("deserialize"),
        fingerprint
    );
    assert!(!format!("{fingerprint:?}").contains("prompt"));
}

#[test]
fn lost_acknowledgement_never_authorizes_another_submission() {
    let request = TurnRequest::new("logical", "write file");
    let mut record = RecoveryRecord::new(SessionId::new("s"), &request).expect("record");
    let first = record.operation().clone();
    assert_eq!(record.action(), RecoveryAction::Submit);
    assert_eq!(record.dispatch(), Dispatch::NotSubmitted);
    assert!(record.terminal().is_none());
    assert_eq!(
        record.fingerprint(),
        &RequestFingerprint::of(&request).expect("fingerprint")
    );
    record
        .record_dispatch(&first, Dispatch::AcceptanceUnknown)
        .expect("before wire write");
    record
        .validate(&request)
        .expect("identical request reconciles the same record");
    assert_eq!(record.action(), RecoveryAction::Reconcile);
    assert!(
        record
            .retry(&request.clone().as_attempt(AttemptId::new(2)))
            .is_err()
    );
    assert!(
        record
            .record_dispatch(&first, Dispatch::NotSubmitted)
            .is_err()
    );
    record
        .record_dispatch(&first, Dispatch::Accepted)
        .expect("late acknowledgement");
    assert_eq!(record.action(), RecoveryAction::Observe);
    assert!(
        record
            .record_dispatch(&first, Dispatch::AcceptanceUnknown)
            .is_err()
    );
    assert!(record.reconcile_not_submitted(&first).is_err());
    record
        .finish(&first, TerminalStatus::Completed)
        .expect("terminal");
    record
        .finish(&first, TerminalStatus::Completed)
        .expect("duplicate terminal");
    assert_eq!(record.action(), RecoveryAction::Finished);
    assert_eq!(record.terminal(), Some(&TerminalStatus::Completed));
    assert!(
        record
            .finish(
                &first,
                TerminalStatus::Cancelled {
                    reason: CancelReason::Requested
                }
            )
            .is_err()
    );
    assert!(record.record_dispatch(&first, Dispatch::Accepted).is_err());
    assert!(
        record
            .retry(&request.as_attempt(AttemptId::new(3)))
            .is_err()
    );
}

#[test]
fn proven_absence_allows_only_a_new_attempt_of_the_original_request() {
    let request = TurnRequest::new("logical", "write file");
    let mut record = RecoveryRecord::new(SessionId::new("s"), &request).expect("record");
    let old = record.operation().clone();
    assert!(record.retry(&request).is_err());
    record
        .record_dispatch(&old, Dispatch::AcceptanceUnknown)
        .expect("submitted");
    record
        .reconcile_not_submitted(&old)
        .expect("native proof of absence");
    let new = record
        .retry(&request.clone().as_attempt(AttemptId::new(2)))
        .expect("safe retry");
    assert_eq!(new.attempt, AttemptId::new(2));
    assert!(record.record_dispatch(&old, Dispatch::Accepted).is_err());
    assert!(record.finish(&old, TerminalStatus::Completed).is_err());
    assert!(
        record
            .validate(&TurnRequest::new("logical", "changed command"))
            .is_err()
    );
    assert!(
        record
            .validate(&TurnRequest::new("different ID", "write file"))
            .is_err()
    );
    let foreign = OperationRef::new(SessionId::new("other"), request.turn_id, new.attempt);
    assert!(
        record
            .record_dispatch(&foreign, Dispatch::Accepted)
            .is_err()
    );
}

/// An attachment whose fields are fed raw and adjacent.
///
/// Deliberately minimal: the collisions below are between neighbouring fields, so anything the
/// digest writes between them would hide the very ambiguity under test.
fn adjacent(id: &str, name: &str, bytes: &[u8]) -> Attachment {
    Attachment {
        id: id.into(),
        name: name.into(),
        mime_type: String::new(),
        kind: AttachmentKind::Text,
        bytes: bytes.to_vec(),
    }
}

fn fingerprint_of(attachments: Vec<Attachment>) -> RequestFingerprint {
    RequestFingerprint::of(&TurnRequest::new("logical", "prompt").with_attachments(attachments))
        .expect("fingerprint")
}

/// A byte moved from an attachment's id into its name must change the fingerprint.
///
/// Ids, names, media types and file contents are fed to the digest raw rather than through JSON,
/// so nothing in the encoding itself says where one field stops. Without a length prefix per
/// field, `("ab", "c")` and `("a", "bc")` hash the same bytes and `RecoveryRecord::validate`
/// accepts a retry describing a different file as the original request.
#[test]
fn a_byte_moved_between_two_attachment_fields_changes_the_fingerprint() {
    assert_ne!(
        fingerprint_of(vec![adjacent("ab", "c", b"")]),
        fingerprint_of(vec![adjacent("a", "bc", b"")])
    );
}

/// One attachment's contents must not be able to absorb the next attachment's id.
///
/// The same ambiguity across the boundary between two attachments: unframed, one set's trailing
/// bytes and the next set's leading id are one run of bytes, and a retry that rebalanced them
/// would validate.
#[test]
fn a_byte_moved_across_the_attachment_boundary_changes_the_fingerprint() {
    assert_ne!(
        fingerprint_of(vec![adjacent("", "", b"xy"), adjacent("z", "", b"")]),
        fingerprint_of(vec![adjacent("", "", b"x"), adjacent("yz", "", b"")])
    );
}

/// Lowercase hex of a fingerprint's 32 bytes, the form a host persists it in.
fn fingerprint_hex(fingerprint: &RequestFingerprint) -> String {
    fingerprint
        .0
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The fingerprint is persisted and compared after a host restart, so its bytes must not move
/// when the SHA-256 implementation behind it changes. These values were computed by the
/// implementation that shipped in 0.9.0 and pin the whole framing: the JSON head, the
/// length-framed attachment fields, and contents that span many SHA-256 blocks.
#[test]
fn fingerprint_bytes_are_pinned_to_the_values_the_shipped_digest_produced() {
    let prompt_only = RequestFingerprint::of(&TurnRequest::new("turn", "hello")).expect("digest");
    assert_eq!(
        fingerprint_hex(&prompt_only),
        "4cfcacc07e73a4d8bc5e85107dcb4589176b23b04ae5270d2e47a4fff58e75fe",
        "fingerprint of a prompt-only request"
    );

    // 200_000 bytes cover thousands of 64-byte blocks and every residue of the final block.
    let large: Vec<u8> = (0..200_000_u32).map(|index| (index % 251) as u8).collect();
    let rich = TurnRequest::new("turn", "naïve prompt — with \"quotes\"")
        .with_configuration(ConfigurationPatch::new().model(ConfigurationChange::Reset))
        .with_attachments(vec![
            Attachment {
                id: "file-1".into(),
                name: "notes.txt".into(),
                mime_type: "text/plain".into(),
                kind: AttachmentKind::Text,
                bytes: b"content".to_vec(),
            },
            Attachment {
                id: "file-2".into(),
                name: "blob.bin".into(),
                mime_type: "application/octet-stream".into(),
                kind: AttachmentKind::Data,
                bytes: large,
            },
        ]);
    assert_eq!(
        fingerprint_hex(&RequestFingerprint::of(&rich).expect("digest")),
        "9fe359cf63a0d603eccbfa1f5474f86da9c19a7c2d01e87e097edc0737b7ec82",
        "fingerprint of a request with configuration and two attachments"
    );
}
