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

/// What the peer's refusal of one oversized outgoing frame counts, as its error names it.
const REFUSED_FRAME_SUBJECT: &str = "bytes of one outgoing JSON-RPC frame";

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
        match error.cause() {
            mango_external_agents::Error::LimitExceeded {
                subject: REFUSED_FRAME_SUBJECT,
                limit,
                received,
            } => Some(Self::outgoing_frame(*limit, *received)),
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
            } => (outgoing_budget(subject), *limit, *received),
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

/// Which outgoing queue budget the peer's own subject names.
///
/// The peer counts its outbound queue in bytes and in frames and says which in a subject that
/// carries the unit first. For example, `"bytes of outgoing JSON-RPC frames awaiting their
/// write"` is the byte budget and `"outgoing JSON-RPC frames awaiting their write"` the frame
/// budget.
fn outgoing_budget(subject: &str) -> Budget {
    match subject.starts_with("bytes") {
        true => Budget::OutgoingBytes,
        false => Budget::OutgoingFrames,
    }
}

/// The most JSON-RPC messages queued in either direction.
///
/// The queue carries `session/update` notifications and the frames of the requests admitted
/// under `max_pending_requests`, so it holds at least as many messages as either. For example,
/// the default limits allow 1,024 queued messages.
fn message_limit(limits: &Limits) -> usize {
    limits
        .turn_channel_capacity
        .max(limits.max_pending_requests)
        .max(1)
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
/// takes a reserved place per request: `max_pending_requests` of them for the requests this
/// crate admits, and one more for the prompt, which is admitted apart. The agent's own
/// questions are answered on a task each, at most as many as could be queued and parked at once.
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
    options.max_pending_requests = limits.max_pending_requests.saturating_add(1);
    options
}

/// What the connection holds for the agent, and which protocol behaviours it turns on.
///
/// `outbound_buffer_bytes` is the host's outgoing budget
/// ([`HostContext::outbound_buffer_bytes`](mango_external_agents::HostContext::outbound_buffer_bytes)).
/// It bounds one frame, where passing it refuses that frame alone, and the bytes of every frame
/// queued or being written, where passing it means the agent stopped reading and ends the
/// connection. The frame count is bounded by the same number as the incoming queue.
///
/// ACP is JSON-RPC 2.0, so a batch is read member by member, a line that is not JSON is
/// answered with `-32700`, and a request this side gives up is followed by `$/cancel_request`.
///
/// For example, `wire_options(host.limits(), host.outbound_buffer_bytes())`.
pub(crate) fn wire_options(limits: &Limits, outbound_buffer_bytes: usize) -> WireOptions {
    WireOptions::new()
        .with_max_outbound_frame_bytes(outbound_buffer_bytes)
        .with_max_outbound_queued_bytes(outbound_buffer_bytes)
        .with_max_outbound_queued_frames(message_limit(limits))
        .with_batch_decoding()
        .with_parse_error_replies(PARSE_ERROR_REPLIES)
        .with_cancel_on_abandon(PROTOCOL_LEVEL_METHOD_NAMES.cancel_request)
}

#[cfg(test)]
mod tests {
    use super::{Overflow, client_options, message_limit, outgoing_budget, wire_options};
    use mango_external_agents::Limits;
    use mango_external_agents::jsonrpc::PeerTermination;

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

    /// Core's own terminations carry `#[non_exhaustive]` variants, so they are made by ending a
    /// real connection rather than written out here; see `tests/session/pins/budgets.rs` and
    /// `tests/session/burst.rs` for each one arriving through a session. What is pinned here is
    /// the reading of the subject, which is the one thing a core rewording could change silently.
    #[test]
    fn the_outgoing_subject_is_read_by_its_unit() {
        let cases = [
            (
                "bytes of outgoing JSON-RPC frames awaiting their write",
                "bytes queued for the ACP agent",
            ),
            (
                "outgoing JSON-RPC frames awaiting their write",
                "frames queued for the ACP agent",
            ),
        ];
        for (subject, expected) in cases {
            let received = outgoing_budget(subject).subject();
            assert_eq!(
                received, expected,
                "expected the budget for {subject:?}: {expected:?} | received: {received:?}"
            );
        }
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
            4,
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
        let expected = (4_096, 4_096, 8, true, Some(8), Some("$/cancel_request"));
        assert_eq!(
            received, expected,
            "expected (frame bytes, queued bytes, queued frames, batches, parse replies, cancel method): {expected:?} | received: {received:?}"
        );
    }
}
