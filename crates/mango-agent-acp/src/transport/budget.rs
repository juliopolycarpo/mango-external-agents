//! The queue budgets of one ACP connection, derived from the host's limits, and the failure a
//! host is told when one of them is passed.
//!
//! The JSON-RPC peer enforces every budget here and reports a passed one as a
//! [`PeerTermination`]. That type names the peer generically; [`Overflow`] turns it back into the
//! [`Error::LimitExceeded`](mango_external_agents::Error::LimitExceeded) an ACP host has always
//! been given, with this crate's own subjects.

use agent_client_protocol_schema::v1::PROTOCOL_LEVEL_METHOD_NAMES;
use mango_external_agents::Limits;
use mango_external_agents::jsonrpc::{ClientOptions, PeerTermination, WireOptions};

/// How many `-32700` replies may wait to be written at once.
///
/// One reply answers one line that was not JSON. An agent that prints a burst of them while it is
/// not reading gets this many replies and no more; the rest of the burst is dropped unanswered.
const PARSE_ERROR_REPLIES: usize = 8;

/// Which budget a queue passed, and by how much, as
/// [`Error::LimitExceeded`](mango_external_agents::Error::LimitExceeded) fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Overflow {
    budget: Budget,
    limit: usize,
    received: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Budget {
    IncomingMessages,
    IncomingBytes,
    OutgoingFrameBytes,
    OutgoingBytes,
    OutgoingFrames,
}

impl Budget {
    fn subject(self) -> &'static str {
        match self {
            Self::IncomingMessages => "JSON-RPC messages queued from the ACP agent",
            Self::IncomingBytes => "bytes queued from the ACP agent",
            Self::OutgoingFrameBytes => "bytes in one frame to the ACP agent",
            Self::OutgoingBytes => "bytes queued for the ACP agent",
            Self::OutgoingFrames => "frames queued for the ACP agent",
        }
    }
}

impl Overflow {
    /// An incoming message-count overflow, for tests outside this module.
    #[cfg(test)]
    pub(crate) fn incoming_messages(limit: usize, received: usize) -> Self {
        Self {
            budget: Budget::IncomingMessages,
            limit,
            received,
        }
    }

    /// The overflow of one outgoing frame of `received` bytes under `limit`.
    ///
    /// For example, `Overflow::outgoing_frame(host.outbound_buffer_bytes(), 9_000_000).error()`.
    pub(crate) fn outgoing_frame(limit: usize, received: usize) -> Self {
        Self {
            budget: Budget::OutgoingFrameBytes,
            limit,
            received,
        }
    }

    /// The frame budget a refused request names, or `None` for any other refusal.
    ///
    /// The peer refuses one frame larger than its outgoing bound without touching the
    /// connection, and names what it counted in the error's subject. For example, a 9-byte frame
    /// under an 8-byte bound is refused as
    /// `LimitExceeded { subject: "bytes of one outgoing JSON-RPC frame", limit: 8, received: 9 }`,
    /// which is `Overflow::outgoing_frame(8, 9)` here.
    pub(crate) fn from_refused_frame(error: &mango_external_agents::Error) -> Option<Self> {
        let mango_external_agents::Error::LimitExceeded {
            subject,
            limit,
            received,
        } = error.cause()
        else {
            return None;
        };
        match outgoing_budget(subject)? {
            Budget::OutgoingFrameBytes => Some(Self::outgoing_frame(*limit, *received)),
            _ => None,
        }
    }

    /// The budget a connection's end names, or `None` when it ended for any other reason.
    ///
    /// For example, a 50-message batch under an 8-message cap ends the connection with
    /// `PeerTermination::NotificationBackpressure { limit: 8, received: 50, .. }`, which is the
    /// incoming message budget here.
    pub(crate) fn from_termination(termination: &PeerTermination) -> Option<Self> {
        let (budget, limit, received) = match termination {
            PeerTermination::NotificationBackpressure {
                limit, received, ..
            } => (Budget::IncomingMessages, *limit, *received),
            PeerTermination::NotificationByteBackpressure {
                limit, received, ..
            } => (Budget::IncomingBytes, *limit, *received),
            PeerTermination::OutboundBackpressure {
                subject,
                limit,
                received,
                ..
            } => (outgoing_budget(subject)?, *limit, *received),
            _ => return None,
        };
        Some(Self {
            budget,
            limit,
            received,
        })
    }

    /// The typed error a host sees for this budget.
    ///
    /// For example, 50 queued messages under an 8-message cap displays as
    /// `expected at most 8 JSON-RPC messages queued from the ACP agent, received 50`.
    pub(crate) fn error(self) -> mango_external_agents::Error {
        mango_external_agents::Error::LimitExceeded {
            subject: self.budget.subject(),
            limit: self.limit,
            received: self.received,
        }
    }
}

/// Which outgoing budget one of the peer's own subjects names, or `None` for a subject this
/// crate does not know.
///
/// The one place this crate reads the peer's wording. The peer says what it counted in the
/// `subject` of its `LimitExceeded` and of its `OutboundBackpressure`, as text, and has no type
/// for it. The three it uses are matched whole, so a subject the peer rewords stops matching
/// and the failure reads as a closed link instead of as the wrong budget;
/// `the_peers_own_subjects_are_the_ones_matched` ends a real connection each way and fails when
/// that happens. For example, `"outgoing JSON-RPC frames awaiting their write"` is the frame
/// count of the queue.
fn outgoing_budget(subject: &str) -> Option<Budget> {
    match subject {
        "bytes of one outgoing JSON-RPC frame" => Some(Budget::OutgoingFrameBytes),
        "bytes of outgoing JSON-RPC frames awaiting their write" => Some(Budget::OutgoingBytes),
        "outgoing JSON-RPC frames awaiting their write" => Some(Budget::OutgoingFrames),
        _ => None,
    }
}

/// The most notifications and requests from the agent that may wait for the handler.
///
/// The larger of the two counts a host sets, so a burst of `session/update` notifications is
/// not held to the request limit and never to less than one. The outgoing frame count builds
/// on it ([`outgoing_frame_limit`]). For example, the default limits allow 1,024 queued
/// messages.
fn message_limit(limits: &Limits) -> usize {
    limits
        .turn_channel_capacity
        .max(limits.max_pending_requests)
        .max(1)
}

/// The most frames the outgoing queue may hold, the one being written included.
///
/// Sized so that what this side queues on its own account always fits, whatever the agent
/// does, and only answers the agent itself keeps asking for can fill it. With `r` for
/// `max_pending_requests`, this side's own frames at one moment are at most:
///
/// - the prompt: 1;
/// - the `r` requests admitted beside it: `r`;
/// - the answers to the `r` questions that may be parked, all withdrawn at once by a cancel: `r`;
/// - `session/cancel`, and the one repeat a cancel that beat its prompt is owed: 2.
///
/// That is 2`r` + 3. A `$/cancel_request` for a request given up, and a `-32700`, are not in
/// it: the peer queues those beside its bounds. On top of it the queue takes [`message_limit`]
/// answers to requests the agent makes and this side answers on the spot (`-32601`, or a
/// question past the cap): the agent sets how many of those there are, so they are what a
/// frame count that fills is made of. For example, the default limits allow
/// 1,024 + 2 x 64 + 3 = 1,155 frames.
fn outgoing_frame_limit(limits: &Limits) -> usize {
    limits
        .max_pending_requests
        .saturating_mul(2)
        .saturating_add(3)
        .saturating_add(message_limit(limits))
}

/// The places reserved for answers that wait behind earlier notifications.
///
/// Every request this side makes asks for one. `max_pending_requests` requests are admitted at
/// a time, and the prompt apart from them. One more is slack: the peer gives a place back a
/// moment after it has handed the answer over, on its own task, while the caller it woke has
/// already returned its admission and may be asking again. The peer hands answers over one at
/// a time, so at most one place is ever in that state. For example, the default limits reserve
/// 66 places.
fn ordered_places(limits: &Limits) -> usize {
    limits.max_pending_requests.saturating_add(2)
}

/// How the connection to one ACP agent speaks and what it holds of the agent's traffic.
///
/// Every bound comes from the host's [`Limits`]:
///
/// - `turn_channel_capacity` or `max_pending_requests`, whichever is larger, bounds the
///   notifications and questions waiting for the handler;
/// - `turn_buffer_bytes` bounds their encoded bytes;
/// - `request_timeout` is the deadline of every request that is not a prompt, and of each
///   frame's write;
/// - `shutdown_timeout` bounds each stage of closing the connection.
///
/// A request's answer is delivered behind the notifications that arrived before it, which
/// takes a reserved place per request: [`ordered_places`] of them. The agent's own questions
/// are handed over one at a time, at most as many out as could be queued and parked at once.
///
/// For example, `client_options(host.limits()).max_pending_notifications` is 1,024 under the
/// default limits.
pub(crate) fn client_options(limits: &Limits) -> ClientOptions {
    let messages = message_limit(limits);
    let mut options = ClientOptions::new("ACP agent")
        .with_code_prefix("acp")
        .with_limits(limits)
        .with_max_pending_notifications(messages)
        .with_max_in_flight_requests(messages.saturating_add(limits.max_pending_requests));
    options.max_pending_requests = ordered_places(limits);
    options
}

/// The bytes the outgoing queue may hold: one frame of the largest size the host's budget allows,
/// and as much again behind it.
///
/// The queue counts the frame being written with the ones waiting. Bounded by the frame budget
/// alone, a prompt that filled that budget left no room for anything queued in the same instant,
/// so the `session/cancel` meant to stop it ended the connection instead. Twice the budget is
/// room for that largest frame and a full budget of frames behind it. For example, the default
/// 8 MiB budget gives a 16 MiB queue.
fn queued_bytes_limit(outbound_buffer_bytes: usize) -> usize {
    outbound_buffer_bytes.saturating_mul(2)
}

/// What the connection holds for the agent, and which protocol behaviours it turns on.
///
/// `outbound_buffer_bytes` is the host's outgoing budget
/// ([`HostContext::outbound_buffer_bytes`](mango_external_agents::HostContext::outbound_buffer_bytes)).
/// Two bounds come from it, and a host sees a different thing at each:
///
/// - One frame may be that many bytes. A larger one is refused alone, as `LimitExceeded` naming
///   "bytes in one frame to the ACP agent" with `Dispatch::NotSubmitted`: nothing is queued and
///   the session stays usable.
/// - The queue, the frame being written included, may hold twice that many
///   ([`queued_bytes_limit`]). A frame that would pass it means the agent stopped reading: the
///   connection ends, a turn in flight fails with `acp-transport-overflow`, and a request still
///   waiting fails as `LimitExceeded` naming "bytes queued for the ACP agent".
///
/// The frame count is bounded by [`outgoing_frame_limit`], the incoming message limit plus
/// room for this side's own frames, and passing it ends the connection the same way, naming
/// "frames queued for the ACP agent".
///
/// ACP is JSON-RPC 2.0, so a batch is read member by member, a line that is not JSON is
/// answered with `-32700`, and a request this side gives up is followed by `$/cancel_request`.
/// The agent's own requests are handed over in wire order with a responder, so their answers
/// are queued, in order and against the same bounds, with everything else this side writes.
///
/// For example, `wire_options(host.limits(), host.outbound_buffer_bytes())`.
pub(crate) fn wire_options(limits: &Limits, outbound_buffer_bytes: usize) -> WireOptions {
    WireOptions::new()
        .with_max_outbound_frame_bytes(outbound_buffer_bytes)
        .with_max_outbound_queued_bytes(queued_bytes_limit(outbound_buffer_bytes))
        .with_max_outbound_queued_frames(outgoing_frame_limit(limits))
        .with_batch_decoding()
        .with_parse_error_replies(PARSE_ERROR_REPLIES)
        .with_cancel_on_abandon(PROTOCOL_LEVEL_METHOD_NAMES.cancel_request)
        .with_request_handoff()
}

#[cfg(test)]
mod tests {
    use super::Budget;
    use super::{
        Overflow, client_options, message_limit, ordered_places, outgoing_budget,
        outgoing_frame_limit, queued_bytes_limit, wire_options,
    };
    use mango_external_agents::jsonrpc::{
        Client, ClientOptions, ConnectionEnd, PeerHandler, PeerTermination, ServerRequestOutcome,
        WireOptions,
    };
    use mango_external_agents::{Limits, Link, LinkReceiver, LinkSender};
    use std::sync::Arc;

    /// The three fields of the `LimitExceeded` an overflow displays as.
    fn fields(overflow: Overflow) -> (&'static str, usize, usize) {
        match overflow.error() {
            mango_external_agents::Error::LimitExceeded {
                subject,
                limit,
                received,
            } => (subject, limit, received),
            other => panic!("expected Error::LimitExceeded | received: {other:?}"),
        }
    }

    #[test]
    fn the_message_limit_is_the_larger_of_the_two_counts_and_never_zero() {
        let cases = [
            ((1_024, 64), 1_024),
            ((8, 64), 64),
            ((0, 0), 1),
            ((usize::MAX, 1), usize::MAX),
        ];
        for ((turn_channel_capacity, max_pending_requests), expected) in cases {
            let limits = Limits {
                turn_channel_capacity,
                max_pending_requests,
                ..Limits::default()
            };
            let received = message_limit(&limits);
            assert_eq!(
                received, expected,
                "expected the message limit for ({turn_channel_capacity}, {max_pending_requests}): {expected} | received: {received}"
            );
        }
    }

    /// A peer that never takes a write: the first frame stays in flight and the rest queue.
    struct StalledAgent;

    #[async_trait::async_trait]
    impl LinkSender for StalledAgent {
        async fn send(&mut self, _message: String) -> mango_external_agents::Result<()> {
            std::future::pending().await
        }

        async fn close(&mut self) -> mango_external_agents::Result<()> {
            Ok(())
        }
    }

    /// A peer that never says anything.
    struct SilentAgent;

    #[async_trait::async_trait]
    impl LinkReceiver for SilentAgent {
        async fn recv(&mut self) -> mango_external_agents::Result<Option<String>> {
            std::future::pending().await
        }
    }

    /// A handler nothing is ever sent to.
    struct Unused;

    #[async_trait::async_trait]
    impl PeerHandler for Unused {
        async fn on_notification(&self, _method: String, _params: serde_json::Value) {}

        async fn on_request(
            &self,
            _method: String,
            _params: serde_json::Value,
            _id: mango_external_agents::jsonrpc::RequestId,
        ) -> ServerRequestOutcome {
            ServerRequestOutcome::Answer(serde_json::Value::Null)
        }
    }

    fn stalled_client(wire: WireOptions) -> Client {
        Client::connect_with(
            Link::new(Box::new(StalledAgent), Box::new(SilentAgent)),
            Arc::new(Unused),
            ClientOptions::new("ACP agent"),
            wire,
        )
    }

    /// Queues notifications of `padding` bytes on `client` until one is refused, and returns
    /// the budget the connection then says it ended for.
    fn overflow_after_filling(client: &Client, padding: usize) -> Option<(&'static str, usize)> {
        let params = serde_json::json!({ "pad": "x".repeat(padding) });
        let refused = (0..64).any(|_| client.submit_notification("ping", &params).is_err());
        assert!(
            refused,
            "expected a queued notification refused within 64 | received: none refused"
        );
        let Some(ConnectionEnd::Peer(termination)) = client.ended() else {
            panic!(
                "expected a connection ended by its peer | received: {:?}",
                client.ended()
            );
        };
        Overflow::from_termination(&termination).map(|overflow| {
            let (subject, limit, _) = fields(overflow);
            (subject, limit)
        })
    }

    /// The peer names what it counted as text. Each of its three outgoing subjects is produced
    /// here by a real connection and has to come back as this crate's own budget; a subject the
    /// peer reworded comes back as `None` and fails this by name.
    #[tokio::test]
    async fn the_peers_own_subjects_are_the_ones_matched() {
        let one_frame = stalled_client(WireOptions::new().with_max_outbound_frame_bytes(64));
        let refused = one_frame
            .submit_notification("ping", serde_json::json!({ "pad": "x".repeat(128) }))
            .expect_err("expected a frame over the bound refused");
        let received = Overflow::from_refused_frame(&refused).map(|overflow| fields(overflow).0);
        let expected = Some("bytes in one frame to the ACP agent");
        assert_eq!(
            received, expected,
            "expected the budget of a refused frame: {expected:?} | received: {received:?} from {refused:?}"
        );

        let bytes = stalled_client(WireOptions::new().with_max_outbound_queued_bytes(256));
        let received = overflow_after_filling(&bytes, 64);
        let expected = Some(("bytes queued for the ACP agent", 256));
        assert_eq!(
            received, expected,
            "expected the budget of a queue over its bytes: {expected:?} | received: {received:?}"
        );

        let frames = stalled_client(WireOptions::new().with_max_outbound_queued_frames(2));
        let received = overflow_after_filling(&frames, 8);
        let expected = Some(("frames queued for the ACP agent", 2));
        assert_eq!(
            received, expected,
            "expected the budget of a queue over its frame count: {expected:?} | received: {received:?}"
        );
    }

    #[test]
    fn a_subject_the_peer_does_not_use_names_no_budget() {
        let received = outgoing_budget("bytes of something else").map(Budget::subject);
        assert_eq!(
            received, None,
            "expected no budget for an unknown subject | received: {received:?}"
        );
    }

    #[test]
    fn only_the_peers_frame_refusal_reads_as_the_frame_budget() {
        let frame = mango_external_agents::Error::LimitExceeded {
            subject: "bytes of one outgoing JSON-RPC frame",
            limit: 8,
            received: 9,
        };
        let received = Overflow::from_refused_frame(&frame).map(fields);
        let expected = Some(("bytes in one frame to the ACP agent", 8, 9));
        assert_eq!(
            received, expected,
            "expected (subject, limit, received): {expected:?} | received: {received:?}"
        );

        for other in [
            mango_external_agents::Error::LimitExceeded {
                subject: "outstanding ACP requests",
                limit: 1,
                received: 2,
            },
            mango_external_agents::Error::Closed { subject: "link" },
        ] {
            let received = Overflow::from_refused_frame(&other);
            assert_eq!(
                received, None,
                "expected no frame overflow for {other:?} | received: {received:?}"
            );
        }
    }

    #[test]
    fn an_end_that_is_not_a_budget_names_no_overflow() {
        for termination in [
            PeerTermination::Exited,
            PeerTermination::LinkFailed(String::from("broken pipe")),
        ] {
            let received = Overflow::from_termination(&termination);
            assert_eq!(
                received, None,
                "expected no overflow for {termination:?} | received: {received:?}"
            );
        }
    }

    #[test]
    fn an_outgoing_frame_overflow_names_the_frame_budget() {
        let received = fields(Overflow::outgoing_frame(4_096, 4_097));
        let expected = ("bytes in one frame to the ACP agent", 4_096, 4_097);
        assert_eq!(
            received, expected,
            "expected (subject, limit, received): {expected:?} | received: {received:?}"
        );
    }

    #[test]
    fn the_queue_holds_twice_the_frame_budget_and_never_wraps() {
        for (budget, expected) in [(0, 0), (4_096, 8_192), (usize::MAX, usize::MAX)] {
            let received = queued_bytes_limit(budget);
            assert_eq!(
                received, expected,
                "expected the queued byte limit for a budget of {budget}: {expected} | received: {received}"
            );
        }
    }

    #[test]
    fn the_frame_limit_holds_this_sides_own_frames_on_top_of_the_message_limit() {
        // (turn_channel_capacity, max_pending_requests) -> message limit + 2 requests' worth + 3.
        let cases = [
            ((1, 1), 1 + 2 + 3),
            ((2, 4), 4 + 8 + 3),
            ((1_024, 64), 1_024 + 128 + 3),
            ((usize::MAX, usize::MAX), usize::MAX),
        ];
        for ((turn_channel_capacity, max_pending_requests), expected) in cases {
            let limits = Limits {
                turn_channel_capacity,
                max_pending_requests,
                ..Limits::default()
            };
            let received = outgoing_frame_limit(&limits);
            assert_eq!(
                received, expected,
                "expected the frame limit for ({turn_channel_capacity}, {max_pending_requests}): {expected} | received: {received}"
            );
        }
    }

    #[test]
    fn the_ordered_places_are_the_requests_the_prompt_and_one_of_slack() {
        let cases = [(1, 3), (64, 66), (usize::MAX, usize::MAX)];
        for (max_pending_requests, expected) in cases {
            let limits = Limits {
                max_pending_requests,
                ..Limits::default()
            };
            let received = ordered_places(&limits);
            assert_eq!(
                received, expected,
                "expected the ordered places for {max_pending_requests} requests: {expected} | received: {received}"
            );
        }
    }

    #[test]
    fn the_client_options_take_every_bound_from_the_hosts_limits() {
        let limits = Limits {
            turn_channel_capacity: 8,
            max_pending_requests: 3,
            turn_buffer_bytes: 4_096,
            request_timeout: std::time::Duration::from_secs(7),
            shutdown_timeout: std::time::Duration::from_secs(2),
            ..Limits::default()
        };
        let options = client_options(&limits);
        let received = (
            options.code_prefix,
            options.include_version_header,
            options.max_pending_notifications,
            options.max_pending_requests,
            options.max_in_flight_requests,
            options.max_pending_bytes,
            options.request_timeout,
            options.shutdown_timeout,
        );
        let expected = (
            "acp",
            true,
            8,
            5,
            11,
            4_096,
            limits.request_timeout,
            limits.shutdown_timeout,
        );
        assert_eq!(
            received, expected,
            "expected (prefix, version header, notifications, ordered places, in flight, bytes, request timeout, shutdown timeout): {expected:?} | received: {received:?}"
        );
    }

    #[test]
    fn the_wire_options_bound_the_frame_and_the_queue_and_speak_json_rpc_2() {
        let limits = Limits {
            turn_channel_capacity: 8,
            max_pending_requests: 3,
            ..Limits::default()
        };
        let wire = wire_options(&limits, 4_096);
        let received = (
            wire.max_outbound_frame_bytes(),
            wire.max_outbound_queued_bytes(),
            wire.max_outbound_queued_frames(),
            wire.decodes_batches(),
            wire.parse_error_replies(),
            wire.cancel_on_abandon(),
        );
        let expected = (4_096, 8_192, 17, true, Some(8), Some("$/cancel_request"));
        assert_eq!(
            received, expected,
            "expected (frame bytes, queued bytes, queued frames, batches, parse replies, cancel method): {expected:?} | received: {received:?}"
        );
    }
}
