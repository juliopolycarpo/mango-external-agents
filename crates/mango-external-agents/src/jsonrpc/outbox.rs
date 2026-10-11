//! The one queue every outgoing frame passes through, and the task that writes it.
//!
//! A frame is admitted and queued in one synchronous step, so frames reach the wire in the order
//! their callers queued them, whatever task or lock each caller was under. One task takes the
//! queue's entries off in that order. A frame queued by a caller that does not wait is written by
//! that task. A caller that does wait for its own write queues a turn instead, is handed the link
//! when the turn comes up, and writes on its own task; when nothing is queued and the link is
//! free it takes the link without queueing at all.
//!
//! None of that starts until a frame has been queued. A connection whose callers have all waited
//! for their own writes takes the link's lock directly, contended or not, and this queue and its
//! task have no part in it: those callers wait on each other exactly as they did before there
//! was a queue. The first queued frame switches the connection over, once, behind the callers
//! already waiting on the lock.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::task::{Context, Poll};

use tokio::sync::{Mutex, Notify, OwnedMutexGuard, mpsc, oneshot};
use tokio::time::Instant;

use super::{ClientState, MidSend, Registration};
use crate::error::{Error, Result};
use crate::link::LinkSender;

/// The link's sending half, held for the length of one frame's write.
pub(super) type SenderGuard = OwnedMutexGuard<Box<dyn LinkSender>>;

/// Bounds on what a client holds on its way to the peer.
///
/// Passed to [`Client::connect_with`](super::Client::connect_with). The default bounds nothing,
/// which is what [`Client::connect`](super::Client::connect) uses: a caller of the asynchronous
/// methods waits for its own frame to be written, so it holds one frame at a time. A caller of
/// [`Client::submit_request`](super::Client::submit_request) or
/// [`Client::submit_notification`](super::Client::submit_notification) does not wait, so nothing
/// slows it down but these.
///
/// Every size is the encoded frame in bytes, as handed to the link, without the terminator a
/// framed transport adds.
///
/// # Example
///
/// ```
/// use mango_external_agents::jsonrpc::WireOptions;
///
/// let wire = WireOptions::new()
///     .with_max_outbound_frame_bytes(1024 * 1024)
///     .with_max_outbound_queued_bytes(8 * 1024 * 1024)
///     .with_max_outbound_queued_frames(1024);
/// assert_eq!(wire.max_outbound_frame_bytes(), 1024 * 1024);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WireOptions {
    max_outbound_frame_bytes: usize,
    max_outbound_queued_bytes: usize,
    max_outbound_queued_frames: usize,
    batch_decoding: bool,
    parse_error_replies: Option<usize>,
    cancel_on_abandon: Option<String>,
}

impl Default for WireOptions {
    fn default() -> Self {
        Self {
            max_outbound_frame_bytes: usize::MAX,
            max_outbound_queued_bytes: usize::MAX,
            max_outbound_queued_frames: usize::MAX,
            batch_decoding: false,
            parse_error_replies: None,
            cancel_on_abandon: None,
        }
    }
}

impl WireOptions {
    /// No bound on anything.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// assert_eq!(WireOptions::new().max_outbound_queued_frames(), usize::MAX);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Refuses any one outgoing frame larger than this, however it is sent.
    ///
    /// The refusal is of that call alone: nothing is queued or written, the error is
    /// [`Error::LimitExceeded`] with the frame's measured size and
    /// [`Dispatch::NotSubmitted`](crate::Dispatch::NotSubmitted), and the connection stays
    /// usable. A frame larger than
    /// [`with_max_outbound_queued_bytes`](Self::with_max_outbound_queued_bytes) is refused the
    /// same way when it is queued, since no queue could hold it. A reply to one of the peer's
    /// questions that is too large is not sent either: the peer is answered with a JSON-RPC
    /// internal error (`-32603`) for that question instead, so it is not left waiting.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// let wire = WireOptions::new().with_max_outbound_frame_bytes(4096);
    /// assert_eq!(wire.max_outbound_frame_bytes(), 4096);
    /// ```
    #[must_use]
    pub fn with_max_outbound_frame_bytes(mut self, bytes: usize) -> Self {
        self.max_outbound_frame_bytes = bytes;
        self
    }

    /// Holds at most this many bytes of queued frames, the one being written included.
    ///
    /// Counted are the frames queued with
    /// [`Client::submit_request`](super::Client::submit_request) and
    /// [`Client::submit_notification`](super::Client::submit_notification), from when each is
    /// queued until the writer has written or dropped it; a request given up while queued still
    /// counts until then. A caller of the asynchronous methods holds its own frame while it
    /// waits, and that frame is not counted.
    ///
    /// A frame that would pass it means the peer has stopped reading. The connection ends with
    /// [`PeerTermination::OutboundBackpressure`](super::PeerTermination::OutboundBackpressure),
    /// and the call that brought the frame receives [`Error::LimitExceeded`] with the same
    /// numbers.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// let wire = WireOptions::new().with_max_outbound_queued_bytes(8 * 1024 * 1024);
    /// assert_eq!(wire.max_outbound_queued_bytes(), 8 * 1024 * 1024);
    /// ```
    #[must_use]
    pub fn with_max_outbound_queued_bytes(mut self, bytes: usize) -> Self {
        self.max_outbound_queued_bytes = bytes;
        self
    }

    /// Holds at most this many queued frames, the one being written included, counted as
    /// [`with_max_outbound_queued_bytes`](Self::with_max_outbound_queued_bytes) counts bytes.
    ///
    /// Passing it ends the connection the way
    /// [`with_max_outbound_queued_bytes`](Self::with_max_outbound_queued_bytes) describes. A
    /// bound of zero is not passed by anything the peer did: it refuses every queued frame
    /// alone, and leaves the connection to callers that wait for their own writes. The frames
    /// [`with_parse_error_replies`](Self::with_parse_error_replies) and
    /// [`with_cancel_on_abandon`](Self::with_cancel_on_abandon) send are not held to this
    /// bound, whatever it is.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// let wire = WireOptions::new().with_max_outbound_queued_frames(1024);
    /// assert_eq!(wire.max_outbound_queued_frames(), 1024);
    /// ```
    #[must_use]
    pub fn with_max_outbound_queued_frames(mut self, frames: usize) -> Self {
        self.max_outbound_queued_frames = frames;
        self
    }

    /// Reads a line that is a JSON array as a JSON-RPC batch: each member that is an object is
    /// taken as a frame, in the order the array gives them.
    ///
    /// Off by default, where such a line is dropped like any other line that is not a frame.
    ///
    /// What a batch is held to:
    ///
    /// - Each notification and each question in it counts against
    ///   [`ClientOptions::max_pending_notifications`](super::ClientOptions::max_pending_notifications)
    ///   on its own. The whole batch is counted before any of it is queued, so a batch that
    ///   does not fit ends the connection with nothing of it handled, and the
    ///   [`PeerTermination::NotificationBackpressure`](super::PeerTermination::NotificationBackpressure)
    ///   reports what the queue held plus every such member as `received`.
    /// - The line's bytes, all of them, count against
    ///   [`ClientOptions::max_pending_bytes`](super::ClientOptions::max_pending_bytes) once
    ///   when the batch has any such member, shared out evenly among those members and given
    ///   back as each is handled. A batch of responses alone takes none, as a response on a
    ///   line of its own takes none.
    /// - A response in a batch reaches its caller as one on a line of its own does, ordered
    ///   answers behind the notifications before them in the batch.
    /// - A member that is not an object, and an empty array, are dropped.
    ///
    /// The answers to a batch's questions go out as frames of their own, not as one array.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// let wire = WireOptions::new().with_batch_decoding();
    /// assert!(wire.decodes_batches());
    /// ```
    #[must_use]
    pub fn with_batch_decoding(mut self) -> Self {
        self.batch_decoding = true;
        self
    }

    /// Answers a line that is not JSON with a JSON-RPC parse error: code `-32700`, `id` null.
    ///
    /// Off by default, where such a line is dropped: a vendor that prints a stray diagnostic
    /// on its output is not told about it. On, at most `max_queued` such replies wait to be
    /// written at a time, and a line that is not JSON while that many wait is dropped as
    /// before; `0` answers none. The replies are held beside the bounds of the outbound queue,
    /// not within them: the peer's noise takes none of the room those bounds leave this side's
    /// own frames, and cannot bring the queue to the bound that ends a connection. The replies
    /// go through the queue, so from the first one on, every write of this client takes its
    /// turn there.
    ///
    /// What turning this on costs: a reply is a write. A peer that sends lines that are not
    /// JSON and does not read what it is sent leaves a reply waiting to be written, and when
    /// that write's deadline ([`ClientOptions::request_timeout`](super::ClientOptions::request_timeout))
    /// runs out the connection ends, as after any write that could not finish: a line may be
    /// half written by then. With this off, such a peer is read for as long as it writes. A
    /// peer that reads what it is sent pays nothing for a line that is not JSON beyond the
    /// reply.
    ///
    /// The reply does not quote the line. A line that is empty or only whitespace gets none,
    /// and neither does a line that is JSON and not a frame (a number, a string, an array
    /// without [`with_batch_decoding`](Self::with_batch_decoding)).
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// let wire = WireOptions::new().with_parse_error_replies(8);
    /// assert_eq!(wire.parse_error_replies(), Some(8));
    /// ```
    #[must_use]
    pub fn with_parse_error_replies(mut self, max_queued: usize) -> Self {
        self.parse_error_replies = Some(max_queued);
        self
    }

    /// Tells the peer when this side gives a request up: a notification named `method`, with
    /// `{"requestId": <the request's id>}` for its parameters, queued behind the request.
    ///
    /// Off by default. A request is given up when its future or its
    /// [`Reply`](super::Reply) is dropped before an answer came. A deadline that runs out
    /// drops the future of [`Client::request`](super::Client::request) and so sends it at
    /// once; a [`Reply`](super::Reply) that resolved with a timeout sends it when it is
    /// dropped. Nothing is sent:
    ///
    /// - for a request whose frame never began to be written, since the peer never saw it;
    /// - once any response to it has been read, delivered yet or not;
    /// - once the connection has ended.
    ///
    /// There is at most one for each request the peer was sent, and they are held beside the
    /// bounds of the outbound queue, not within them: a caller letting go of requests takes
    /// none of the room those bounds leave other frames. It goes through the queue, so from
    /// the first one on, every write of this client takes its turn there.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// let wire = WireOptions::new().with_cancel_on_abandon("$/cancel_request");
    /// assert_eq!(wire.cancel_on_abandon(), Some("$/cancel_request"));
    /// ```
    #[must_use]
    pub fn with_cancel_on_abandon(mut self, method: impl Into<String>) -> Self {
        self.cancel_on_abandon = Some(method.into());
        self
    }

    /// Whether a line that is a JSON array is read as a batch.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// assert!(!WireOptions::new().decodes_batches());
    /// ```
    #[must_use]
    pub fn decodes_batches(&self) -> bool {
        self.batch_decoding
    }

    /// How many parse-error replies may wait to be written, or `None` when none are sent.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// assert_eq!(WireOptions::new().parse_error_replies(), None);
    /// ```
    #[must_use]
    pub fn parse_error_replies(&self) -> Option<usize> {
        self.parse_error_replies
    }

    /// The notification sent for a request given up, or `None` when none is.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// assert_eq!(WireOptions::new().cancel_on_abandon(), None);
    /// ```
    #[must_use]
    pub fn cancel_on_abandon(&self) -> Option<&str> {
        self.cancel_on_abandon.as_deref()
    }

    /// The largest single frame this client sends.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// assert_eq!(WireOptions::new().max_outbound_frame_bytes(), usize::MAX);
    /// ```
    #[must_use]
    pub fn max_outbound_frame_bytes(&self) -> usize {
        self.max_outbound_frame_bytes
    }

    /// The most bytes of unwritten frames this client holds.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// assert_eq!(WireOptions::new().max_outbound_queued_bytes(), usize::MAX);
    /// ```
    #[must_use]
    pub fn max_outbound_queued_bytes(&self) -> usize {
        self.max_outbound_queued_bytes
    }

    /// The most unwritten frames this client holds.
    ///
    /// # Example
    ///
    /// ```
    /// use mango_external_agents::jsonrpc::WireOptions;
    ///
    /// assert_eq!(WireOptions::new().max_outbound_queued_frames(), usize::MAX);
    /// ```
    #[must_use]
    pub fn max_outbound_queued_frames(&self) -> usize {
        self.max_outbound_queued_frames
    }
}

/// Why a frame was not queued.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Refusal {
    /// This one frame is larger than any frame may be. Nothing else is wrong.
    FrameTooLarge { limit: usize, received: usize },
    /// The queue cannot take it: the peer is not reading as fast as this side writes.
    QueueFull {
        subject: &'static str,
        limit: usize,
        received: usize,
    },
    /// The queue could never take it, empty or not: its bound is zero. Nothing about the peer.
    NoRoomAtAll {
        subject: &'static str,
        limit: usize,
        received: usize,
    },
    /// The connection has closed or ended: the queue takes nothing more.
    Sealed,
    /// The writer has gone, with the connection.
    Stopped,
}

/// What the queue holds, counted until each frame is written, abandoned or dropped.
#[derive(Debug, Default)]
struct Held {
    frames: usize,
    bytes: usize,
    /// Frames queued beside the bounds: in the queue, and in nobody's count of it. Counted
    /// here so that a queue holding only these is still not taken for an empty one.
    aside: usize,
    /// Turns queued for callers that write for themselves. Not bounded here: each is a caller
    /// waiting, and holds no frame of its own in the queue.
    turns: usize,
    /// Whether a frame has ever been queued. Until then callers that write for themselves share
    /// the link's lock directly and nothing here orders them.
    queueing: bool,
    /// Whether the connection has closed or ended. Set under the lock frames are admitted
    /// under, so a frame is either in the queue before this, or refused.
    sealed: bool,
    /// Callers that went for the link's lock directly, before queueing began, and do not hold it
    /// yet. The first queued frame waits for them: they called first.
    direct: usize,
}

#[derive(Debug, Default)]
pub(super) struct Budget {
    held: StdMutex<Held>,
    /// Told when the last caller counted in `Held::direct` has the link or has given up.
    direct_done: Notify,
}

impl Budget {
    fn release(&self, bytes: usize) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.frames = held.frames.saturating_sub(1);
        held.bytes = held.bytes.saturating_sub(bytes);
    }

    fn release_aside(&self) {
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        held.aside = held.aside.saturating_sub(1);
    }
}

/// Whether a frame of `bytes` may join `frames` frames holding `held_bytes`, under `limits`.
///
/// A frame larger than any frame may be, or than the whole queue may hold, is refused on that
/// alone, before the queue is looked at: it says nothing about the peer. So is any frame when
/// the queue may hold none. The queue's own bounds count the frame being added, so each is
/// the most the queue ever holds.
pub(super) fn admission(
    limits: &WireOptions,
    frames: usize,
    held_bytes: usize,
    bytes: usize,
) -> std::result::Result<(), Refusal> {
    // The queue's own byte bound is a bound on one frame too: a frame an empty queue could not
    // hold is too large, whatever the peer is doing.
    let frame_limit = limits
        .max_outbound_frame_bytes
        .min(limits.max_outbound_queued_bytes);
    if bytes > frame_limit {
        return Err(Refusal::FrameTooLarge {
            limit: frame_limit,
            received: bytes,
        });
    }
    if limits.max_outbound_queued_frames == 0 {
        return Err(Refusal::NoRoomAtAll {
            subject: QUEUED_FRAMES,
            limit: 0,
            received: 1,
        });
    }
    let frames = frames.saturating_add(1);
    if frames > limits.max_outbound_queued_frames {
        return Err(Refusal::QueueFull {
            subject: QUEUED_FRAMES,
            limit: limits.max_outbound_queued_frames,
            received: frames,
        });
    }
    let held_bytes = held_bytes.saturating_add(bytes);
    if held_bytes > limits.max_outbound_queued_bytes {
        return Err(Refusal::QueueFull {
            subject: "bytes of outgoing JSON-RPC frames awaiting their write",
            limit: limits.max_outbound_queued_bytes,
            received: held_bytes,
        });
    }
    Ok(())
}

const QUEUED_FRAMES: &str = "outgoing JSON-RPC frames awaiting their write";

const QUEUED: u8 = 0;
const WRITING: u8 = 1;
/// Done with, having been written or at least begun.
const FINISHED: u8 = 2;
/// Done with, and never begun: no byte of the frame reached the link.
const UNWRITTEN: u8 = 3;

/// One queued frame's passage, shared by whoever queued it and the writer.
///
/// The two can each decide the frame's fate at the same moment: the writer by beginning to write
/// it, its caller by giving up on it. Both go through this one atomic phase, so exactly one of
/// them wins, and "was any byte of this possibly written" is read from the same word the winner
/// wrote, with no second flag to lag behind it.
#[derive(Debug)]
pub(super) struct FrameCtl {
    phase: AtomicU8,
    bytes: usize,
}

impl FrameCtl {
    /// The writer takes the frame. False when its caller withdrew it first.
    fn begin(&self) -> bool {
        self.phase
            .compare_exchange(QUEUED, WRITING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Takes a frame that is still queued out of the running, so it is never written. True when
    /// that worked: no byte of it reached the link or will.
    ///
    /// The frame stays in the queue, and counted against its bounds, until the writer comes to
    /// it and drops it: what the bounds are for is what the queue holds, and it still holds this.
    pub(super) fn withdraw(&self) -> bool {
        self.phase
            .compare_exchange(QUEUED, UNWRITTEN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Whether the frame is still waiting for the writer.
    pub(super) fn queued(&self) -> bool {
        self.phase.load(Ordering::Acquire) == QUEUED
    }

    /// Whether the write began, so that some byte of the frame may have reached the peer.
    pub(super) fn began(&self) -> bool {
        matches!(self.phase.load(Ordering::Acquire), WRITING | FINISHED)
    }

    /// The writer is done with the frame, whichever way.
    fn settle(&self) {
        // Either it began, or nobody will begin it now.
        if self
            .phase
            .compare_exchange(WRITING, FINISHED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            let _ =
                self.phase
                    .compare_exchange(QUEUED, UNWRITTEN, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    pub(super) fn bytes(&self) -> usize {
        self.bytes
    }
}

/// A frame in the queue.
pub(super) struct Outgoing {
    frame: String,
    ctl: Arc<FrameCtl>,
    budget: Arc<Budget>,
    ack: Option<oneshot::Sender<Result<()>>>,
    /// When the write is given up, counted from when the frame was queued.
    deadline: Instant,
    /// The call this frame asks, when it is a request. Entered among the calls awaiting an
    /// answer by the writer, just before the frame goes out: no answer can come sooner, and the
    /// caller that queued it could not wait for that map.
    call: Option<Registration>,
    /// Whether the frame has a share of the queue's bounds to give back.
    counted: bool,
}

/// What the writer takes off the queue.
pub(super) enum Entry {
    Frame(Outgoing),
    /// A caller that writes for itself, waiting to be handed the link.
    Turn(oneshot::Sender<SenderGuard>),
    /// Answered when the writer reaches it, which is when every frame queued before it has been
    /// written or given up. A close waits for one before it closes the link.
    Barrier(oneshot::Sender<()>),
}

impl Drop for Outgoing {
    fn drop(&mut self) {
        // The one place a frame's share of the budget goes back: when the queue no longer holds
        // it, written or not. Also the path of a frame still queued when the writer is taken
        // down, where the dropped acknowledgement tells whoever waits.
        self.ctl.settle();
        if self.counted {
            self.budget.release(self.ctl.bytes);
        } else {
            self.budget.release_aside();
        }
    }
}

/// What a caller keeps of a frame it queued.
pub(super) struct Ticket {
    pub(super) ctl: Arc<FrameCtl>,
    pub(super) ack: oneshot::Receiver<Result<()>>,
}

/// A caller on its way to the link's lock, from before queueing began.
pub(super) struct Direct<'a> {
    budget: &'a Budget,
}

impl Drop for Direct<'_> {
    /// The caller has the lock, or gave up waiting for it.
    fn drop(&mut self) {
        let mut held = self
            .budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        held.direct = held.direct.saturating_sub(1);
        // Nobody waits for this until a frame has been queued.
        if held.direct == 0 && held.queueing {
            self.budget.direct_done.notify_waiters();
        }
    }
}

/// How a caller that writes for itself gets the link.
pub(super) enum Turn<'a> {
    /// By waiting on the link's lock itself. Dropped once the caller holds it.
    Direct(Direct<'a>),
    /// The link, now.
    Now(SenderGuard),
    /// The link, when the writer has worked through what was queued first.
    Queued(oneshot::Receiver<SenderGuard>),
    /// The writer has gone, with the connection.
    Stopped,
}

/// The queue's sending side, with what it has let in.
pub(super) struct Outbox {
    limits: WireOptions,
    budget: Arc<Budget>,
    queue: mpsc::UnboundedSender<Entry>,
    #[cfg(test)]
    taken: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    waited_for_direct: std::sync::atomic::AtomicUsize,
}

impl Outbox {
    pub(super) fn new(limits: WireOptions) -> (Self, mpsc::UnboundedReceiver<Entry>) {
        let (queue, frames) = mpsc::unbounded_channel();
        (
            Self {
                limits,
                budget: Arc::new(Budget::default()),
                queue,
                #[cfg(test)]
                taken: std::sync::atomic::AtomicUsize::new(0),
                #[cfg(test)]
                waited_for_direct: std::sync::atomic::AtomicUsize::new(0),
            },
            frames,
        )
    }

    /// The options this queue was made with.
    pub(super) fn wire(&self) -> &WireOptions {
        &self.limits
    }

    /// Admits one frame and queues it, as one step.
    ///
    /// The count and the push happen under one lock, so two callers cannot be admitted in one
    /// order and queued in the other.
    pub(super) fn enqueue(
        &self,
        frame: String,
        deadline: Instant,
        call: Option<Registration>,
    ) -> std::result::Result<Ticket, Refusal> {
        self.queue_frame(frame, deadline, call, true)
    }

    /// Queues a frame this side sends of its own accord, beside the queue's bounds: it takes
    /// none of the room the bounds leave the host's frames, and is not refused for want of it.
    ///
    /// For frames that are bounded where they are made (a parse error, a cancel for a request
    /// given up), so that neither the peer's noise nor a caller letting go of a request can
    /// bring the queue to the bound that ends a connection. A frame larger than any frame may
    /// be is still refused.
    pub(super) fn enqueue_aside(
        &self,
        frame: String,
        deadline: Instant,
    ) -> std::result::Result<Ticket, Refusal> {
        self.queue_frame(frame, deadline, None, false)
    }

    fn queue_frame(
        &self,
        frame: String,
        deadline: Instant,
        call: Option<Registration>,
        counted: bool,
    ) -> std::result::Result<Ticket, Refusal> {
        let bytes = frame.len();
        let mut held = self
            .budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Before the bounds: a frame that comes too late says nothing about the peer.
        if held.sealed {
            return Err(Refusal::Sealed);
        }
        if counted {
            admission(&self.limits, held.frames, held.bytes, bytes)?;
        } else if bytes > self.limits.max_outbound_frame_bytes {
            return Err(Refusal::FrameTooLarge {
                limit: self.limits.max_outbound_frame_bytes,
                received: bytes,
            });
        }
        // From the first queued frame on, every write takes its turn here.
        held.queueing = true;
        let ctl = Arc::new(FrameCtl {
            phase: AtomicU8::new(QUEUED),
            bytes,
        });
        let (ack, acknowledged) = oneshot::channel();
        if counted {
            held.frames += 1;
            held.bytes = held.bytes.saturating_add(bytes);
        } else {
            held.aside += 1;
        }
        let queued = Outgoing {
            frame,
            ctl: Arc::clone(&ctl),
            budget: Arc::clone(&self.budget),
            ack: Some(ack),
            deadline,
            call,
            counted,
        };
        if let Err(refused) = self.queue.send(Entry::Frame(queued)) {
            // The frame comes back in the error and gives its share back as it is dropped, which
            // takes this lock.
            drop(held);
            drop(refused);
            return Err(Refusal::Stopped);
        }
        Ok(Ticket {
            ctl,
            ack: acknowledged,
        })
    }

    /// How a caller that writes its own frame gets the link.
    ///
    /// Before any frame has been queued: by the link's lock itself, as such callers always
    /// have, taken at once when it is free and waited on otherwise. A caller that waits is
    /// counted until it holds the lock, so the first queued frame can let everyone who called
    /// before it go first.
    ///
    /// Once frames are queued: at once when nothing is queued, nobody is ahead and the link is
    /// free, since the caller is then first in line by any ordering; otherwise by a turn queued
    /// behind what is there. The check and the queueing happen under the lock frames are
    /// admitted under, so a turn and a frame cannot each be told it came first.
    pub(super) fn turn(&self, sender: &Arc<Mutex<Box<dyn LinkSender>>>) -> Turn<'_> {
        let mut held = self
            .budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if !held.queueing {
            // Nobody on the way to it, and free: the lock hands itself to a waiter before it is
            // ever free again, so nobody is waiting on it either.
            if held.direct == 0
                && let Ok(sender) = Arc::clone(sender).try_lock_owned()
            {
                return Turn::Now(sender);
            }
            held.direct += 1;
            return Turn::Direct(Direct {
                budget: &self.budget,
            });
        }
        if held.frames == 0
            && held.aside == 0
            && held.turns == 0
            && let Ok(sender) = Arc::clone(sender).try_lock_owned()
        {
            return Turn::Now(sender);
        }
        let (grant, granted) = oneshot::channel();
        if self.queue.send(Entry::Turn(grant)).is_err() {
            return Turn::Stopped;
        }
        held.turns += 1;
        Turn::Queued(granted)
    }

    fn turn_reached(&self) {
        let mut held = self
            .budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        held.turns = held.turns.saturating_sub(1);
    }

    /// Whether a caller that writes for itself may send one frame of `bytes`.
    ///
    /// Against the per-frame bound alone. The queue's bounds are on what the queue holds, and
    /// such a caller's frame is never in it.
    pub(super) fn frame_fits(&self, bytes: usize) -> std::result::Result<(), Refusal> {
        if bytes > self.limits.max_outbound_frame_bytes {
            return Err(Refusal::FrameTooLarge {
                limit: self.limits.max_outbound_frame_bytes,
                received: bytes,
            });
        }
        Ok(())
    }

    /// Waits until every caller that went for the link directly, before queueing began, has it
    /// or has given up. None can be added once queueing has begun, so this ends.
    async fn direct_callers_served(&self) {
        loop {
            let done = self.budget.direct_done.notified();
            if self
                .budget
                .held
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .direct
                == 0
            {
                return;
            }
            #[cfg(test)]
            self.waited_for_direct.fetch_add(1, Ordering::AcqRel);
            done.await;
        }
    }

    /// Takes no more frames, from this call on. What is already queued stays queued.
    pub(super) fn seal(&self) {
        self.budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sealed = true;
    }

    /// Whether the queue has stopped taking frames.
    pub(super) fn is_sealed(&self) -> bool {
        self.budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sealed
    }

    /// Queues a barrier behind every frame queued so far. `None` when there is no queue to wait
    /// behind: nothing was ever queued, or the writer has gone.
    pub(super) fn barrier(&self) -> Option<oneshot::Receiver<()>> {
        if !self
            .budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .queueing
        {
            return None;
        }
        let (reached, waiting) = oneshot::channel();
        self.queue.send(Entry::Barrier(reached)).ok()?;
        Some(waiting)
    }

    /// How many entries the outbox task has taken off the queue, for a test that has to know
    /// the task has reached one.
    #[cfg(test)]
    pub(super) fn taken(&self) -> usize {
        self.taken.load(Ordering::Acquire)
    }

    /// How many times the outbox task has stopped to let a direct caller go first.
    #[cfg(test)]
    pub(super) fn waited_for_direct(&self) -> usize {
        self.waited_for_direct.load(Ordering::Acquire)
    }

    /// Callers on their way to the link's lock directly, and whether queueing has begun.
    #[cfg(test)]
    pub(super) fn direct(&self) -> (usize, bool) {
        let held = self
            .budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (held.direct, held.queueing)
    }

    #[cfg(test)]
    pub(super) fn turns(&self) -> usize {
        self.budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .turns
    }

    /// How many frames are queued beside the bounds.
    #[cfg(test)]
    pub(super) fn aside(&self) -> usize {
        self.budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .aside
    }

    #[cfg(test)]
    pub(super) fn held(&self) -> (usize, usize) {
        let held = self
            .budget
            .held
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (held.frames, held.bytes)
    }
}

/// Works through the queue in order: writes each frame, and hands the link to each caller that
/// writes for itself, waiting for it to give the link back before going on.
pub(super) async fn run(state: Arc<ClientState>, mut entries: mpsc::UnboundedReceiver<Entry>) {
    while let Some(entry) = entries.recv().await {
        #[cfg(test)]
        state.outbox.taken.fetch_add(1, Ordering::AcqRel);
        // Callers that went for the link before the first frame was queued called first.
        state.outbox.direct_callers_served().await;
        match entry {
            Entry::Barrier(reached) => {
                let _ = reached.send(());
            }
            Entry::Turn(mut grant) => {
                // Waiting for the lock is waiting for whoever writes now to finish. The turn stays
                // counted until this holds the link, so a caller arriving meanwhile queues behind
                // it and cannot take the link from under it. A caller that stopped waiting is
                // not waited for: its turn is over the moment it lets go.
                let sender = tokio::select! {
                    biased;
                    () = grant.closed() => None,
                    sender = Arc::clone(&state.sender).lock_owned() => Some(sender),
                };
                state.outbox.turn_reached();
                if let Some(sender) = sender {
                    let _ = grant.send(sender);
                }
            }
            Entry::Frame(outgoing) => frame(&state, outgoing).await,
        }
    }
}

/// Sees one queued frame through: enters its call among those awaiting an answer, writes it, and
/// tells whoever waits.
async fn frame(state: &ClientState, mut outgoing: Outgoing) {
    let mut call = None;
    if let Some(registration) = outgoing.call.take() {
        // A request its caller already gave up is neither entered nor written.
        if !outgoing.ctl.queued() {
            return;
        }
        match state.register(registration).await {
            Ok(id) => call = Some(id),
            Err(refused) => {
                let ack = outgoing.ack.take();
                drop(outgoing);
                if let Some(ack) = ack {
                    let _ = ack.send(Err(refused));
                }
                return;
            }
        }
    }
    let outcome = write(state, &mut outgoing).await;
    let ack = outgoing.ack.take();
    // Its share of the budget goes back before its caller hears: a caller that queues the next
    // frame on that news must find the room.
    drop(outgoing);
    if let Some(id) = call
        && !matches!(outcome, Some(Ok(())))
    {
        state.fail_unwritten(&id).await;
    }
    if let (Some(ack), Some(outcome)) = (ack, outcome) {
        let _ = ack.send(outcome);
    }
}

/// Writes one frame. `None` when its caller withdrew it first, so there is nobody to tell.
async fn write(state: &ClientState, outgoing: &mut Outgoing) -> Option<Result<()>> {
    let mut sender = state.sender.lock().await;
    // Read holding the sender: the write that held it before may have been abandoned mid-frame
    // or failed, and the pump has not necessarily ended the connection yet. `closed` is
    // deliberately not the test: a closing connection still writes what was queued before it.
    if state.link_poisoned.load(Ordering::Acquire) {
        return outgoing.ctl.withdraw().then(|| Err(state.unusable_link()));
    }
    // A frame that waited out its whole deadline in the queue has put nothing on the wire, so it
    // fails alone and the link stays good.
    if Instant::now() >= outgoing.deadline {
        return outgoing.ctl.withdraw().then(|| Err(state.write_timeout()));
    }
    if !outgoing.ctl.begin() {
        return None;
    }
    #[cfg(test)]
    state.run_after_write_began();
    // Armed only once this write holds the sender and has begun. Dropped before the sender is
    // released, so the next write sees the link it leaves behind.
    let mut midsend = MidSend::arm(state);
    let frame = std::mem::take(&mut outgoing.frame);
    let sent = tokio::select! {
        biased;
        sent = sender.send(frame) => sent,
        // The frame may be half on the wire: the guard above reports the link unusable.
        () = tokio::time::sleep_until(outgoing.deadline) => {
            return Some(Err(state.write_timeout()));
        }
    };
    midsend.finish();
    if let Err(error) = &sent
        && matches!(error.cause(), Error::Link { .. })
    {
        // A completed transport failure can leave the read half open. Poison before releasing
        // the sender so a queued frame cannot race the pump's termination. Local admission and
        // validation refusals leave a healthy transport usable.
        state.link_poisoned.store(true, Ordering::Release);
        state.signal_write_failure(error);
    }
    Some(sent)
}

/// The write of one frame queued with [`Client::submit_request`](super::Client::submit_request)
/// or [`Client::submit_notification`](super::Client::submit_notification).
///
/// Awaiting it answers "did the frame reach the link": `Ok` once the link took all of it, the
/// link's own error when the write failed, [`Error::Timeout`] when it did not finish within
/// [`ClientOptions::request_timeout`](super::ClientOptions::request_timeout) of being queued. The
/// frame is written whether or not this is awaited or kept; dropping it gives up the answer, not
/// the write.
///
/// [`started`](Self::started) separates a failure that left the peer untouched from one that may
/// not have.
#[must_use = "the frame is written either way; this is how its caller learns whether it was"]
///
/// ```no_run
/// # async fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
/// let written = client.submit_notification("session/cancel", serde_json::json!({}))?;
/// if let Err(error) = written.await {
///     // Nothing was written unless the write had begun.
///     eprintln!("{error}");
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Written {
    ctl: Arc<FrameCtl>,
    ack: oneshot::Receiver<Result<()>>,
    peer: String,
}

impl Written {
    pub(super) fn new(ticket: Ticket, peer: String) -> Self {
        Self {
            ctl: ticket.ctl,
            ack: ticket.ack,
            peer,
        }
    }

    /// Whether the write of this frame has begun, so that some byte of it may be out.
    ///
    /// `false` is a promise: the peer has seen nothing of the frame so far, and when the write
    /// has already failed, never will. `true` marks possible delivery, not success.
    ///
    /// ```no_run
    /// # fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
    /// let written = client.submit_notification("ping", serde_json::json!({}))?;
    /// // Queued a moment ago: the writer has not necessarily taken it yet.
    /// let _possibly_delivered = written.started();
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn started(&self) -> bool {
        self.ctl.began()
    }

    /// The size of the frame as it was queued, in encoded bytes.
    ///
    /// ```no_run
    /// # fn example(client: &mango_external_agents::jsonrpc::Client) -> mango_external_agents::Result<()> {
    /// let written = client.submit_notification("ping", serde_json::json!({}))?;
    /// assert!(written.frame_bytes() > 0);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn frame_bytes(&self) -> usize {
        self.ctl.bytes()
    }
}

impl Future for Written {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.ack).poll(cx).map(|acknowledged| {
            acknowledged.unwrap_or_else(|_| {
                Err(Error::Link {
                    peer: self.peer.clone(),
                    message: String::from(
                        "a connection that ended, or a caller that withdrew the frame, before the JSON-RPC frame was written",
                    ),
                })
            })
        })
    }
}
