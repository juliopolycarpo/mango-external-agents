//! A prompt whose encoded request cannot leave the transport is refused before the turn starts.
//!
//! The transport refuses any outgoing frame over `Limits::turn_buffer_bytes`. Refused there, the
//! turn has already started: the connection fails, the session closes and the host is left with
//! `AcceptanceUnknown`. These tests drive the real outbound path (the SDK's own serialisation and
//! the bounded transport's own check), so the boundary they pin is the one a host meets.

use super::*;

use mango_external_agents::session::ATTACHMENT_MAX_BYTES;
use mango_external_agents::{Attachment, AttachmentKind};

const SMALL_BUDGET: usize = 64 * 1024;

async fn open_with(agent: FakeAcpAgent, limits: Limits) -> (Box<dyn Session>, FakeLauncher) {
    let launcher = FakeLauncher::new();
    launcher.push(agent.process());
    let host = HostContext::builder()
        .launcher(Arc::new(launcher.clone()))
        .cwd(std::env::temp_dir())
        .client_info("mea-tests", "0.1.0")
        .limits(limits)
        .build()
        .expect("expected a host");
    let session = AcpHarness::new(profile())
        .open_session(&host, OpenSession::new("chat-1"))
        .await
        .expect("expected a session");
    (session, launcher)
}

fn small_budget() -> Limits {
    Limits {
        turn_buffer_bytes: SMALL_BUDGET,
        ..Limits::default()
    }
}

/// The byte length of every `session/prompt` frame the harness wrote, without its newline.
fn prompt_frame_lengths(launcher: &FakeLauncher) -> Vec<usize> {
    launcher
        .written()
        .iter()
        .filter(|line| line.contains("\"method\":\"session/prompt\""))
        .map(String::len)
        .collect()
}

fn attachment(kind: AttachmentKind, mime_type: &str, bytes: Vec<u8>) -> Attachment {
    Attachment {
        id: String::from("attachment-1"),
        name: String::from("attachment.bin"),
        mime_type: mime_type.to_owned(),
        kind,
        bytes,
    }
}

fn image(bytes: usize) -> Attachment {
    attachment(AttachmentKind::Image, "image/png", vec![0; bytes])
}

/// Runs `request` to its terminal and asserts the turn completed.
async fn assert_completes(session: &dyn Session, request: TurnRequest) {
    let mut turn = match session.start_turn(request).await {
        Ok(turn) => turn,
        Err(error) => panic!("expected an accepted turn | received refusal: {error}"),
    };
    let events = drain(&mut turn).await;
    assert!(
        matches!(events.last(), Some(EventKind::Completed)),
        "expected the turn to end Completed | received {:?}",
        events
            .iter()
            .map(|kind| format!("{kind:?}").chars().take(160).collect::<String>())
            .collect::<Vec<_>>()
    );
}

/// Asserts `request` is refused as `LimitExceeded` before submission and returns `(limit, received)`.
async fn assert_refused_before_submission(
    session: &dyn Session,
    request: TurnRequest,
) -> (usize, usize) {
    let error = match session.start_turn(request).await {
        Err(error) => error,
        Ok(mut turn) => {
            let events = drain(&mut turn).await;
            panic!(
                "expected LimitExceeded with NotSubmitted | received an accepted turn ({:?}) ending {:?}",
                turn.dispatch(),
                events
                    .iter()
                    .map(|kind| format!("{kind:?}").chars().take(160).collect::<String>())
                    .collect::<Vec<_>>()
            );
        }
    };
    assert_eq!(
        error.dispatch(),
        Dispatch::NotSubmitted,
        "expected dispatch NotSubmitted | received {:?} for {error}",
        error.dispatch()
    );
    match error.cause() {
        Error::LimitExceeded {
            subject,
            limit,
            received,
        } => {
            assert_eq!(
                *subject, "bytes in one frame to the ACP agent",
                "expected the outgoing frame subject | received {subject:?}"
            );
            (*limit, *received)
        }
        other => panic!("expected Error::LimitExceeded | received {other:?}"),
    }
}

/// The largest request that fits is sent; one byte more is refused before submission, and the same
/// session then carries the next turn.
#[tokio::test]
async fn the_exact_frame_budget_is_sent_and_one_byte_more_is_refused_before_submission() {
    let (session, launcher) = open_with(FakeAcpAgent::new(), small_budget()).await;

    assert_completes(session.as_ref(), TurnRequest::new("turn-empty", "")).await;
    let base = prompt_frame_lengths(&launcher)[0];
    assert!(
        base < SMALL_BUDGET,
        "expected an empty prompt to fit {SMALL_BUDGET} bytes | received {base}"
    );

    let fitting = SMALL_BUDGET - base;
    assert_completes(
        session.as_ref(),
        TurnRequest::new("turn-fits", "a".repeat(fitting)),
    )
    .await;
    assert_eq!(
        prompt_frame_lengths(&launcher).last().copied(),
        Some(SMALL_BUDGET),
        "expected the largest fitting prompt to be sent as one {SMALL_BUDGET}-byte frame"
    );

    let (limit, received) = assert_refused_before_submission(
        session.as_ref(),
        TurnRequest::new("turn-over", "a".repeat(fitting + 1)),
    )
    .await;
    assert_eq!(
        (limit, received),
        (SMALL_BUDGET, SMALL_BUDGET + 1),
        "expected limit {SMALL_BUDGET} and received {} | received limit {limit} and received {received}",
        SMALL_BUDGET + 1
    );
    assert_eq!(
        prompt_frame_lengths(&launcher).len(),
        2,
        "expected the refused prompt to reach no frame | received {:?}",
        prompt_frame_lengths(&launcher)
    );

    assert_completes(
        session.as_ref(),
        TurnRequest::new("turn-after", "still here"),
    )
    .await;
    assert_eq!(
        session.snapshot().status,
        SessionStatus::Ready,
        "expected the session to stay usable after the refusal"
    );
}

/// The escaping counts: a control character is six bytes on the wire, so a text that is a sixth of
/// the budget in bytes can still be over it.
#[tokio::test]
async fn control_characters_are_counted_as_their_escaped_size() {
    let (session, launcher) = open_with(FakeAcpAgent::new(), small_budget()).await;

    let request = TurnRequest::new("turn-control", "inspect").with_attachments(vec![attachment(
        AttachmentKind::Text,
        "text/plain",
        vec![0x01; SMALL_BUDGET / 2],
    )]);
    let (limit, received) = assert_refused_before_submission(session.as_ref(), request).await;
    assert!(
        limit == SMALL_BUDGET && received > 6 * (SMALL_BUDGET / 2),
        "expected limit {SMALL_BUDGET} and received over {} | received limit {limit} and received {received}",
        6 * (SMALL_BUDGET / 2)
    );
    assert!(
        prompt_frame_lengths(&launcher).is_empty(),
        "expected no prompt frame | received {:?}",
        prompt_frame_lengths(&launcher)
    );
    assert_completes(
        session.as_ref(),
        TurnRequest::new("turn-after", "still here"),
    )
    .await;
}

/// Three attachments at the per-attachment cap pass every check the builder makes and still
/// encode past the default 8 MiB frame budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_images_at_the_attachment_cap_are_refused_under_default_limits() {
    let (session, launcher) = open_with(FakeAcpAgent::new(), Limits::default()).await;
    let budget = Limits::default().turn_buffer_bytes;

    let request = TurnRequest::new("turn-images", "inspect")
        .with_attachments((0..3).map(|_| image(ATTACHMENT_MAX_BYTES)).collect());
    let (limit, received) = assert_refused_before_submission(session.as_ref(), request).await;
    assert!(
        limit == budget && received > budget,
        "expected limit {budget} and received over it | received limit {limit} and received {received}"
    );
    assert!(
        prompt_frame_lengths(&launcher).is_empty(),
        "expected no prompt frame | received {:?}",
        prompt_frame_lengths(&launcher)
    );
    assert_completes(
        session.as_ref(),
        TurnRequest::new("turn-after", "still here"),
    )
    .await;
}

/// Four plain texts at the cap add up to exactly the budget before the request's own bytes, so the
/// envelope alone decides it: the real frame is a few hundred bytes over, which a hand-built
/// envelope only approximates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_capped_plain_texts_are_refused_by_the_real_envelope_under_default_limits() {
    let (session, launcher) = open_with(FakeAcpAgent::new(), Limits::default()).await;
    let budget = Limits::default().turn_buffer_bytes;

    let request = TurnRequest::new("turn-texts", "inspect").with_attachments(
        (0..4)
            .map(|_| {
                attachment(
                    AttachmentKind::Text,
                    "text/plain",
                    vec![b'a'; ATTACHMENT_MAX_BYTES],
                )
            })
            .collect(),
    );
    let (limit, received) = assert_refused_before_submission(session.as_ref(), request).await;
    assert!(
        limit == budget && received > budget && received < budget + 4096,
        "expected limit {budget} and received just over it | received limit {limit} and received {received}"
    );
    assert!(
        prompt_frame_lengths(&launcher).is_empty(),
        "expected no prompt frame | received {:?}",
        prompt_frame_lengths(&launcher)
    );
    assert_completes(
        session.as_ref(),
        TurnRequest::new("turn-after", "still here"),
    )
    .await;
}

/// One text attachment of control characters at the cap encodes to about 12.6 MB, well past the
/// default budget although it is one attachment of the allowed size.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_capped_text_of_control_characters_is_refused_under_default_limits() {
    let (session, launcher) = open_with(FakeAcpAgent::new(), Limits::default()).await;
    let budget = Limits::default().turn_buffer_bytes;

    let request = TurnRequest::new("turn-control", "inspect").with_attachments(vec![attachment(
        AttachmentKind::Text,
        "text/plain",
        vec![0x01; ATTACHMENT_MAX_BYTES],
    )]);
    let (limit, received) = assert_refused_before_submission(session.as_ref(), request).await;
    assert!(
        limit == budget && received >= 6 * ATTACHMENT_MAX_BYTES,
        "expected limit {budget} and received at least {} | received limit {limit} and received {received}",
        6 * ATTACHMENT_MAX_BYTES
    );
    assert!(
        prompt_frame_lengths(&launcher).is_empty(),
        "expected no prompt frame | received {:?}",
        prompt_frame_lengths(&launcher)
    );
    assert_completes(
        session.as_ref(),
        TurnRequest::new("turn-after", "still here"),
    )
    .await;
}

/// Refusing what would overflow must not refuse what fits: two capped images are sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_images_at_the_attachment_cap_still_fit_under_default_limits() {
    let (session, launcher) = open_with(FakeAcpAgent::new(), Limits::default()).await;
    let budget = Limits::default().turn_buffer_bytes;

    assert_completes(
        session.as_ref(),
        TurnRequest::new("turn-images", "inspect")
            .with_attachments((0..2).map(|_| image(ATTACHMENT_MAX_BYTES)).collect()),
    )
    .await;
    let lengths = prompt_frame_lengths(&launcher);
    assert!(
        lengths.len() == 1 && lengths[0] < budget,
        "expected one prompt frame under {budget} bytes | received {lengths:?}"
    );
}
